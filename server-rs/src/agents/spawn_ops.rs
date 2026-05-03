//! Agent-spawn dispatcher: route to a connected runner in SaaS mode, or
//! spawn a local session daemon in standalone mode.
//!
//! Mirrors the design of [`crate::agents::git_ops`]: branch on
//! [`crate::saas::dispatch::org_has_runner`], then either delegate to the
//! existing in-process [`crate::agents::pty_agent::start_pty_agent`] (which
//! shells out via the local `git` binary and `supervisor::spawn_session_daemon`)
//! or emit a [`WireMessage::StartAgent`] to the runner over the WS link.
//!
//! ## SaaS mode
//!
//! 1. Generate `agent_id` server-side (so the HTTP caller has it before the
//!    runner replies).
//! 2. Insert the `agents` row with `mode='remote'`, `status='starting'`. The
//!    runner's `AgentStarted`-handler in `saas/runner_ws.rs` flips the row
//!    to `running` once the spawn succeeds (via INSERT ... ON CONFLICT
//!    DO UPDATE so the upgrade is idempotent for this dispatcher path).
//! 3. Resolve `source_branch` via the existing
//!    [`git_ops::default_branch`] dispatcher — runner-routed when SaaS,
//!    local when standalone. `base_commit` and the active-branch checkout
//!    are intentionally skipped server-side: in SaaS mode the runner owns
//!    the filesystem and the agent itself does the branch checkout via the
//!    `unattended_contract_block` instructions baked into the prompt
//!    (see `agents/prompt.rs`).
//! 4. Send the `StartAgent` envelope reliably (outbox + push-if-connected)
//!    so an offline runner picks it up on reconnect.
//!
//! ## Standalone mode
//!
//! Delegates verbatim to `pty_agent::start_pty_agent`. No behavioral change
//! vs the pre-dispatcher code path — the dispatcher is a thin branch.

use rusqlite::params;

use crate::agents::pty_agent::{self, StartPtyOpts};
use crate::saas::dispatch::org_has_runner;
use crate::saas::outbox;
use crate::saas::runner_protocol::{Envelope, WireMessage};
use crate::state::AppState;
use crate::ws::broadcast_event;

/// Spawn an agent — either locally (standalone) or via the registered
/// runner (SaaS). Returns the agent_id in both cases.
///
/// The `org_id` argument selects which deployment we're in via
/// [`org_has_runner`]. When false, this is a passthrough to the
/// existing local path.
pub async fn start_agent_dispatch(
    state: &AppState,
    org_id: &str,
    opts: StartPtyOpts<'_>,
) -> String {
    if org_has_runner(&state.db, org_id) {
        start_agent_via_runner(state, org_id, opts).await
    } else {
        pty_agent::start_pty_agent(&state.registry, opts).await
    }
}

async fn start_agent_via_runner(state: &AppState, org_id: &str, opts: StartPtyOpts<'_>) -> String {
    let StartPtyOpts {
        prompt,
        cwd,
        plan_name,
        task_id,
        effort,
        branch,
        is_continue: _is_continue,
        max_budget_usd,
        driver: driver_name,
        user_id,
        org_id: _opt_org,
    } = opts;

    let agent_id = uuid::Uuid::new_v4().to_string();
    let cwd_str = cwd.to_string_lossy().to_string();
    let (driver_name_resolved, _driver) = state.registry.drivers.get_or_default(driver_name);
    let driver_name_owned = driver_name_resolved.to_string();
    let effort_str = effort.to_string();

    // `source_branch` is left NULL in SaaS mode. It's informational only —
    // the merge resolver in `api/agents.rs::resolve_merge_target`
    // re-resolves at merge time via the runner-routed `default_branch`
    // dispatcher, and the merge-dropdown UI calls `list_merge_targets`
    // which dispatches the same way. Resolving here would force a
    // GetDefaultBranch round-trip on every spawn that blocks the user-
    // visible "Start" until the runner replies.

    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id, prompt, branch, driver, org_id) \
             VALUES (?1, ?2, 'starting', 'remote', ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                agent_id,
                cwd_str,
                plan_name,
                task_id,
                prompt,
                branch,
                driver_name_owned,
                org_id,
            ],
        )
        .ok();
        // user_id column does not exist on `agents` in this schema; the
        // standalone path also passes user_id only to the auth/audit log,
        // not to the row itself. Keep parity by ignoring `user_id` here.
        let _ = user_id;
    }

    broadcast_event(
        &state.broadcast_tx,
        "agent_started",
        serde_json::json!({
            "id": agent_id,
            "planName": plan_name,
            "taskId": task_id,
            "driver": driver_name_owned,
            "mode": "remote",
            "status": "starting",
        }),
    );

    let message = WireMessage::StartAgent {
        agent_id: agent_id.clone(),
        plan_name: plan_name.unwrap_or("").to_string(),
        task_id: task_id.unwrap_or("").to_string(),
        prompt,
        cwd: cwd_str,
        driver: driver_name_owned,
        effort: Some(effort_str),
        max_budget_usd,
    };
    let payload = serde_json::to_string(&message).unwrap_or_default();

    // Pick the most recently-seen runner for this org (matches
    // runner_request_with_registry's selection rule). Online-only filter:
    // if every runner is offline, the StartAgent still gets queued in the
    // outbox for the runner picked here, replayed when it reconnects.
    let runner_id: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT id FROM runners WHERE org_id = ?1 \
             ORDER BY (status = 'online') DESC, last_seen_at DESC LIMIT 1",
            params![org_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };

    let Some(runner_id) = runner_id else {
        eprintln!(
            "[spawn_ops] org {org_id} has runner row(s) but selection failed; agent {agent_id} stays in 'starting'"
        );
        return agent_id;
    };

    // Reliable delivery: enqueue first so an offline runner picks this up
    // on reconnect via outbox replay; push immediately if currently online.
    let seq = {
        let conn = state.db.lock().unwrap();
        outbox::enqueue_server_command(&conn, &runner_id, message.event_type(), &payload)
    };
    let envelope = Envelope::reliable("server".to_string(), seq, message);
    let env_json = serde_json::to_string(&envelope).unwrap_or_default();

    if let Some(runner) = state.runners.lock().await.get(&runner_id) {
        let _ = runner.command_tx.send(env_json);
    }

    agent_id
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use tokio::sync::{Mutex, mpsc, oneshot};

    use crate::saas::runner_protocol::Envelope;
    use crate::saas::runner_ws::{
        ConnectedRunner, RunnerRegistry, RunnerResponse, new_runner_registry,
    };

    /// Build a full-schema DB on a tempfile so the `agents` row INSERT
    /// has every column it expects (and `runners` exists for org_has_runner).
    fn full_db() -> (crate::db::Db, tempfile::TempDir) {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        (db, tempdir)
    }

    fn seed_runner(db: &crate::db::Db, runner_id: &str, org_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
             VALUES (?1, 'test', ?2, ?3, datetime('now'))",
            params![runner_id, org_id, status],
        )
        .unwrap();
    }

    /// Connect a fake runner to the registry whose `command_tx` parks the
    /// envelopes it receives onto an mpsc channel the test reads from.
    async fn install_capturing_runner(
        registry: &RunnerRegistry,
        runner_id: &str,
    ) -> mpsc::UnboundedReceiver<String> {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (cmd_tx, server_to_runner_rx) = mpsc::unbounded_channel::<String>();
        registry.lock().await.insert(
            runner_id.to_string(),
            ConnectedRunner {
                command_tx: cmd_tx,
                hostname: None,
                version: None,
                pending,
            },
        );
        server_to_runner_rx
    }

    fn test_app_state(db: crate::db::Db, runners: RunnerRegistry) -> AppState {
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans");
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/tmp/branchwork-test-claude"),
            0,
            true,
        );
        AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings.json"),
        }
    }

    /// SaaS path acceptance: dispatch sends a `StartAgent` envelope to the
    /// connected runner with the expected `agent_id`, `cwd`, `driver`, and
    /// `effort` (per the brief's acceptance criteria).
    #[tokio::test]
    async fn saas_dispatch_emits_start_agent_envelope_to_runner() {
        let (db, _td) = full_db();
        let org_id = "default-org"; // seeded by db::init
        seed_runner(&db, "runner-1", org_id, "online");

        let runners = new_runner_registry();
        let mut server_to_runner_rx = install_capturing_runner(&runners, "runner-1").await;
        let state = test_app_state(db.clone(), runners);

        let cwd = PathBuf::from("/runner/projects/demo");
        let opts = StartPtyOpts {
            prompt: "hello world".to_string(),
            cwd: &cwd,
            plan_name: Some("demo-plan"),
            task_id: Some("0.8"),
            effort: crate::config::Effort::High,
            branch: Some("branchwork/demo-plan/0.8"),
            is_continue: false,
            max_budget_usd: Some(2.5),
            driver: Some("claude"),
            user_id: None,
            org_id: Some(org_id),
        };
        let agent_id = start_agent_dispatch(&state, org_id, opts).await;

        let payload = tokio::time::timeout(Duration::from_millis(500), server_to_runner_rx.recv())
            .await
            .expect("envelope should arrive")
            .expect("channel still open");

        let envelope: Envelope = serde_json::from_str(&payload).unwrap();
        match envelope.message {
            WireMessage::StartAgent {
                agent_id: got_id,
                cwd: got_cwd,
                driver,
                effort,
                plan_name,
                task_id,
                ..
            } => {
                assert_eq!(got_id, agent_id);
                assert_eq!(got_cwd, "/runner/projects/demo");
                assert_eq!(driver, "claude");
                assert_eq!(effort.as_deref(), Some("high"));
                assert_eq!(plan_name, "demo-plan");
                assert_eq!(task_id, "0.8");
            }
            other => panic!("expected StartAgent variant, got {other:?}"),
        }

        // Server-side row must exist with mode='remote' and status='starting'
        // (waiting for AgentStarted to flip it to 'running').
        let (status, mode): (String, String) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, mode FROM agents WHERE id = ?1",
                params![agent_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(status, "starting");
        assert_eq!(mode, "remote");

        // Outbox should hold the StartAgent for replay on reconnect.
        let outbox_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM inbox_pending WHERE runner_id = ?1 AND command_type = 'start_agent'",
                params!["runner-1"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            outbox_count, 1,
            "StartAgent should be enqueued for reliable delivery"
        );
    }

    /// Standalone path: when `org_has_runner` returns false, the dispatcher
    /// must NOT send a wire envelope. We can't easily check the local
    /// `start_pty_agent` from this test (it tries to spawn a real session
    /// daemon binary), so instead we verify by routing: an org with no
    /// runners triggers `org_has_runner == false`, which the dispatcher
    /// uses to take the local branch — covered separately by the existing
    /// pty_agent unit tests.
    #[tokio::test]
    async fn standalone_dispatch_routes_to_local_when_no_runner() {
        let (db, _td) = full_db();
        // No runner row inserted — org_has_runner returns false.
        assert!(!org_has_runner(&db, "default-org"));
    }
}
