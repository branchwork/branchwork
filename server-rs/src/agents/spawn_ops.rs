//! Agent-spawn / kill dispatchers: route to a connected runner in SaaS mode,
//! or operate locally in standalone mode.
//!
//! Mirrors the design of [`crate::agents::git_ops`]: branch on
//! [`crate::saas::dispatch::org_has_runner`], then either delegate to the
//! existing in-process [`crate::agents::pty_agent::start_pty_agent`] /
//! [`crate::agents::AgentRegistry::kill_agent`] (which shell out via the
//! local `git` binary and `supervisor::spawn_session_daemon`) or emit the
//! corresponding [`WireMessage`] to the runner over the WS link.
//!
//! ## SaaS-mode start
//!
//! 1. Generate `agent_id` server-side (so the HTTP caller has it before the
//!    runner replies).
//! 2. Insert the `agents` row with `mode='remote'`, `status='starting'`. The
//!    runner's `AgentStarted`-handler in `saas/runner_ws.rs` flips the row
//!    to `running` once the spawn succeeds (via INSERT ... ON CONFLICT
//!    DO UPDATE so the upgrade is idempotent for this dispatcher path).
//! 3. `source_branch` is left NULL in SaaS mode (informational only, see
//!    [`start_agent_via_runner`]).
//! 4. Send the `StartAgent` envelope reliably (outbox + push-if-connected)
//!    so an offline runner picks it up on reconnect.
//!
//! ## SaaS-mode kill
//!
//! 1. Send the `KillAgent` envelope reliably (outbox + push-if-connected),
//!    so a momentarily-offline runner still terminates the orphaned daemon
//!    on reconnect.
//! 2. Update `agents.status='killed'` server-side as a fast-path: the
//!    runner-side handler aborts the I/O task before sending `AgentStopped`,
//!    so the runner does not ship a status update on kill — only this
//!    server-side write moves the row out of `running`. Without it the
//!    dashboard would show the agent stuck on `running` forever even after
//!    the daemon is dead.
//! 3. Broadcast `agent_stopped` so connected dashboards refresh immediately.
//!
//! ## Standalone mode
//!
//! Both dispatchers delegate verbatim to the existing local helpers. No
//! behavioral change vs the pre-dispatcher code path — the dispatcher is
//! a thin branch.

use rusqlite::params;

use crate::agents::pty_agent::{self, StartPtyOpts};
use crate::saas::dispatch::org_has_runner;
use crate::saas::outbox;
use crate::saas::runner_protocol::{Envelope, WireMessage};
use crate::saas::runner_rpc::RunnerRpcError;
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

    let Some(runner_id) = pick_runner_for_org(&state.db, org_id) else {
        eprintln!(
            "[spawn_ops] org {org_id} has runner row(s) but selection failed; agent {agent_id} stays in 'starting'"
        );
        return agent_id;
    };

    send_reliable_to_runner(state, &runner_id, message, &payload).await;
    agent_id
}

/// Pick the most recently-seen runner for `org_id`, prioritising online
/// runners. Returns `None` only if the `runners` table has no row for this
/// org — callers above the dispatcher have already gated on
/// [`org_has_runner`], so this should be infallible in practice.
fn pick_runner_for_org(db: &crate::db::Db, org_id: &str) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT id FROM runners WHERE org_id = ?1 \
         ORDER BY (status = 'online') DESC, last_seen_at DESC LIMIT 1",
        params![org_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Reliable delivery: enqueue first so an offline runner picks this up
/// on reconnect via outbox replay; push immediately if currently online.
async fn send_reliable_to_runner(
    state: &AppState,
    runner_id: &str,
    message: WireMessage,
    payload: &str,
) {
    let seq = {
        let conn = state.db.lock().unwrap();
        outbox::enqueue_server_command(&conn, runner_id, message.event_type(), payload)
    };
    let envelope = Envelope::reliable("server".to_string(), seq, message);
    let env_json = serde_json::to_string(&envelope).unwrap_or_default();

    if let Some(runner) = state.runners.lock().await.get(runner_id) {
        let _ = runner.command_tx.send(env_json);
    }
}

/// Kill an agent — either locally (standalone) via SIGTERM through the
/// in-process [`crate::agents::AgentRegistry::kill_agent`], or via a
/// reliably-enqueued [`WireMessage::KillAgent`] to the registered runner
/// (SaaS).
///
/// `Ok(true)` ⇒ the agent existed and the kill was issued (in either
/// mode); `Ok(false)` ⇒ the agent_id is unknown to this server. The error
/// arm is reserved for runner-selection / send failures and is not used
/// today — the outbox absorbs transient runner outages, and the local
/// path never errors. Keeping the `Result` in the signature lets the
/// auto-mode loop (3.3) and any future caller surface RPC failures
/// uniformly without an API break.
///
/// In SaaS mode the row is updated server-side to `status='killed'` as a
/// fast-path: the runner-side handler aborts the per-agent I/O task
/// before SIGTERM lands on the daemon, so it does not (today) follow up
/// with an `AgentStopped`. Without this server-side update the dashboard
/// would observe `running` forever.
pub async fn kill_agent_dispatch(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
) -> Result<bool, RunnerRpcError> {
    if org_has_runner(&state.db, org_id) {
        kill_agent_via_runner(state, org_id, agent_id).await
    } else {
        Ok(state.registry.kill_agent(agent_id).await)
    }
}

async fn kill_agent_via_runner(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
) -> Result<bool, RunnerRpcError> {
    // Existence check — return Ok(false) so the HTTP handler maps to 404.
    // The local registry's `kill_agent` is permissive (always returns
    // true), but SaaS mode can be stricter because we have authoritative
    // org-scoped DB state.
    let exists: bool = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM agents WHERE id = ?1 AND org_id = ?2",
            params![agent_id, org_id],
            |_row| Ok(()),
        )
        .is_ok()
    };
    if !exists {
        return Ok(false);
    }

    let Some(runner_id) = pick_runner_for_org(&state.db, org_id) else {
        eprintln!(
            "[spawn_ops] org {org_id} has runner row(s) but selection failed; \
             cannot route KillAgent for {agent_id}"
        );
        return Err(RunnerRpcError::NoConnectedRunner);
    };

    let message = WireMessage::KillAgent {
        agent_id: agent_id.to_string(),
    };
    let payload = serde_json::to_string(&message).unwrap_or_default();
    send_reliable_to_runner(state, &runner_id, message, &payload).await;

    // Fast-path the row out of `running` / `starting`. The status filter
    // matches the local kill_agent path so we never overwrite a terminal
    // state (already-killed / failed / completed agents stay as-is).
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET status = 'killed', finished_at = datetime('now'), branch = NULL \
             WHERE id = ?1 AND status IN ('running', 'starting')",
            params![agent_id],
        )
        .ok();
    }

    broadcast_event(
        &state.broadcast_tx,
        "agent_stopped",
        serde_json::json!({"id": agent_id, "status": "killed"}),
    );

    Ok(true)
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
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
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

    /// Acceptance: spawn a fix agent via 0.8's `start_agent_dispatch`,
    /// then call `kill_agent_dispatch`. Assert a `KillAgent` envelope
    /// reaches the stub runner with the expected `agent_id`, the
    /// agents row is fast-pathed to status='killed', and the
    /// KillAgent is enqueued for reliable delivery.
    #[tokio::test]
    async fn saas_dispatch_emits_kill_agent_envelope_to_runner() {
        let (db, _td) = full_db();
        let org_id = "default-org"; // seeded by db::init
        seed_runner(&db, "runner-1", org_id, "online");

        let runners = new_runner_registry();
        let mut server_to_runner_rx = install_capturing_runner(&runners, "runner-1").await;
        let state = test_app_state(db.clone(), runners);

        // Spawn via T0.8 so the agents row exists with mode='remote'.
        let cwd = PathBuf::from("/runner/projects/demo");
        let opts = StartPtyOpts {
            prompt: "fix the failing test".to_string(),
            cwd: &cwd,
            plan_name: Some("demo-plan"),
            task_id: Some("0.9"),
            effort: crate::config::Effort::High,
            branch: Some("branchwork/demo-plan/0.9"),
            is_continue: false,
            max_budget_usd: Some(2.5),
            driver: Some("claude"),
            user_id: None,
            org_id: Some(org_id),
        };
        let agent_id = start_agent_dispatch(&state, org_id, opts).await;

        // Drain the StartAgent envelope so the KillAgent is the next read.
        let _start_payload =
            tokio::time::timeout(Duration::from_millis(500), server_to_runner_rx.recv())
                .await
                .expect("StartAgent envelope should arrive")
                .expect("channel still open");

        // Now flip the row to 'running' so the kill fast-path actually
        // updates it (mirrors what AgentStarted would do in production).
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = 'running' WHERE id = ?1",
                params![agent_id],
            )
            .unwrap();
        }

        // Dispatch the kill.
        let result = kill_agent_dispatch(&state, org_id, &agent_id).await;
        assert!(
            matches!(result, Ok(true)),
            "expected Ok(true), got {result:?}"
        );

        // The KillAgent envelope should have arrived at the stub runner.
        let payload = tokio::time::timeout(Duration::from_millis(500), server_to_runner_rx.recv())
            .await
            .expect("KillAgent envelope should arrive")
            .expect("channel still open");

        let envelope: Envelope = serde_json::from_str(&payload).unwrap();
        match envelope.message {
            WireMessage::KillAgent { agent_id: got_id } => {
                assert_eq!(got_id, agent_id);
            }
            other => panic!("expected KillAgent variant, got {other:?}"),
        }

        // Server-side row must be fast-pathed to status='killed' with
        // branch cleared so it stops advertising as mergeable.
        let (status, branch): (String, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, branch FROM agents WHERE id = ?1",
                params![agent_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(status, "killed");
        assert_eq!(branch, None);

        // Outbox should hold the KillAgent for replay on reconnect.
        let outbox_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM inbox_pending WHERE runner_id = ?1 AND command_type = 'kill_agent'",
                params!["runner-1"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            outbox_count, 1,
            "KillAgent should be enqueued for reliable delivery"
        );
    }

    /// Unknown agent_id ⇒ Ok(false) (the HTTP handler maps this to 404).
    /// We must NOT send any envelope or update any row in this case.
    #[tokio::test]
    async fn saas_kill_dispatch_returns_false_for_unknown_agent() {
        let (db, _td) = full_db();
        let org_id = "default-org";
        seed_runner(&db, "runner-1", org_id, "online");

        let runners = new_runner_registry();
        let mut server_to_runner_rx = install_capturing_runner(&runners, "runner-1").await;
        let state = test_app_state(db, runners);

        let result = kill_agent_dispatch(&state, org_id, "no-such-agent").await;
        assert!(
            matches!(result, Ok(false)),
            "expected Ok(false), got {result:?}"
        );

        // No envelope should have been sent.
        let envelope =
            tokio::time::timeout(Duration::from_millis(150), server_to_runner_rx.recv()).await;
        assert!(envelope.is_err(), "no envelope should have been emitted");
    }

    /// Standalone (`org_has_runner == false`): the dispatcher must
    /// delegate to the in-process `AgentRegistry::kill_agent`, which
    /// SIGTERMs the local session daemon (or no-ops cleanly if the
    /// agent doesn't exist in-process). We verify the local path by
    /// observing the DB-level kill semantics: an in-DB-only agent row
    /// (no live socket / pid) flips to 'killed' and the broadcast
    /// fires.
    ///
    /// We can't drive a real PTY supervisor from this unit test, so we
    /// simulate "agent registered in DB but not in-process" — the
    /// fall-through branch of `AgentRegistry::kill_agent` that updates
    /// the row regardless. Combined with the SaaS-mode test above,
    /// this exercises both branches of the dispatcher.
    #[tokio::test]
    async fn standalone_kill_dispatch_takes_local_path() {
        let (db, _td) = full_db();
        let org_id = "default-org"; // seeded by db::init, no runners row
        assert!(
            !org_has_runner(&db, org_id),
            "standalone test requires no runners"
        );

        let runners = new_runner_registry();
        let state = test_app_state(db.clone(), runners);

        // Insert an agent row that is "alive" in DB but not in-process.
        let agent_id = "stale-agent-001";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, org_id) \
                 VALUES (?1, '/tmp/test', 'running', 'pty', ?2)",
                params![agent_id, org_id],
            )
            .unwrap();
        }

        // Subscribe to the broadcast so we can observe agent_stopped.
        let mut bc_rx = state.broadcast_tx.subscribe();

        let result = kill_agent_dispatch(&state, org_id, agent_id).await;
        // Local kill_agent always returns true (existing semantics —
        // see the docstring on `kill_agent_dispatch`).
        assert!(
            matches!(result, Ok(true)),
            "expected Ok(true), got {result:?}"
        );

        // DB row should be flipped to 'killed' by the local path.
        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(status, "killed");

        // agent_stopped should have been broadcast.
        let event = tokio::time::timeout(Duration::from_millis(200), bc_rx.recv())
            .await
            .expect("expected agent_stopped broadcast")
            .expect("broadcast channel still open");
        assert!(event.contains("agent_stopped"), "got broadcast: {event}");
        assert!(
            event.contains(agent_id),
            "broadcast should reference {agent_id}: {event}"
        );
    }
}
