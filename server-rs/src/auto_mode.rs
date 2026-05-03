//! Auto-mode loop entry points.
//!
//! Auto-mode chains task completion → merge → CI check → fix-on-red so a
//! plan can run end-to-end without a human clicking Merge. The loop is
//! built up across this plan's phases:
//!   - Phase 1: merge on completion (this module — entry point only).
//!   - Phase 2: gate the next-task spawn on CI.
//!   - Phase 3: fix-on-red with bounded retries.
//!
//! Both completion call sites (standalone `pty_agent::on_agent_exit` and
//! SaaS `runner_ws::AgentStopped`) call [`on_task_agent_completed`] so the
//! merge-and-pause behaviour is identical regardless of where the agent
//! ran. The function is a no-op when the plan is not opted into auto-mode
//! or has self-paused — checking that gate is cheap and keeps the call
//! sites unconditional.

use rusqlite::params;

use crate::audit;
use crate::db;
use crate::saas::dispatch::merge_agent_branch_dispatch;
use crate::state::AppState;
use crate::ws::broadcast_event;

/// Audit-log action constants for auto-mode transitions. Phase 2/3 will
/// add `auto_mode_ci_passed` / `auto_mode_ci_failed` etc; for now only
/// the merge-side outcomes are wired.
pub mod actions {
    /// A task agent completed and the loop merged its branch.
    pub const AUTO_MODE_MERGED: &str = "auto_mode.merged";
    /// The loop aborted itself for a plan and recorded a pause reason.
    pub const AUTO_MODE_PAUSED: &str = "auto_mode.paused";
}

/// Called from the agent-completion path (standalone and SaaS) once a task
/// agent has cleanly stopped. If auto-mode is enabled for the plan, this
/// kicks off the merge and either:
///   - broadcasts `auto_mode_merged` on success (Phase 2 will continue
///     into the CI gate from this branch — for Phase 1 the loop stops
///     here), or
///   - records a pause via [`db::auto_mode_pause`] and broadcasts
///     `auto_mode_paused` on conflict / error.
///
/// Spawns a tokio task internally so callers (which run inside the
/// completion hot-path) don't await the merge.
///
/// `state` carries the shared `db` / `runners` / `broadcast_tx`; the
/// underlying [`merge_agent_branch_dispatch`] picks runner vs local based
/// on `org_has_runner`, so this module stays mode-agnostic.
pub async fn on_task_agent_completed(
    state: &AppState,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
) {
    if !db::auto_mode_enabled(&state.db, plan_name) {
        return;
    }

    // Look up `org_id` for the audit log. The merge dispatcher reads its
    // own org_id off the agent row, so we don't need to pass it through
    // — but the audit log is org-scoped and we want `auto_mode_merged` /
    // `auto_mode_paused` rows to belong to the same org as the agent.
    let org_id: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT org_id FROM agents WHERE id = ?1",
            params![agent_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "default-org".to_string())
    };

    let state = state.clone();
    let agent_id = agent_id.to_string();
    let plan_name = plan_name.to_string();
    let task_id = task_id.to_string();
    tokio::spawn(async move {
        run_merge_step(&state, &org_id, &agent_id, &plan_name, &task_id).await;
    });
}

/// Body of the spawned task: dispatch the merge and map its outcome to a
/// broadcast + audit-log entry. Pulled out as a free function so unit
/// tests can drive it synchronously without touching the spawn surface.
async fn run_merge_step(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
) {
    let outcome = merge_agent_branch_dispatch(state, org_id, agent_id, None).await;

    if let Some(sha) = outcome.merged_sha {
        // Successful auto-merge. Phase 2 will continue from here into the
        // CI gate; for Phase 1 we just announce + audit-log and stop.
        let payload = serde_json::json!({
            "plan": plan_name,
            "task": task_id,
            "sha": sha,
            "target": outcome.target_branch,
        });
        broadcast_event(&state.broadcast_tx, "auto_mode_merged", payload.clone());
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_MERGED,
            audit::resources::AGENT,
            Some(agent_id),
            Some(&payload.to_string()),
        );
        return;
    }

    // Failure path: pause auto-mode for this plan. `had_conflict` and the
    // generic error case both block the loop until a human resumes — the
    // distinction shows up in the recorded reason so the dashboard can
    // explain *why* the plan paused.
    let reason = if outcome.had_conflict {
        "merge_conflict".to_string()
    } else {
        let msg = outcome
            .error
            .as_deref()
            .unwrap_or("merge dispatch returned no merged_sha and no error");
        format!("merge_failed: {msg}")
    };

    db::auto_mode_pause(&state.db, plan_name, &reason);

    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "reason": reason,
        "target": outcome.target_branch,
    });
    broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
    let conn = state.db.lock().unwrap();
    audit::log(
        &conn,
        org_id,
        None,
        Some("branchwork-auto-mode"),
        actions::AUTO_MODE_PAUSED,
        audit::resources::PLAN,
        Some(plan_name),
        Some(&payload.to_string()),
    );
}

#[cfg(test)]
mod tests {
    //! Integration-style tests for the auto-mode merge-on-completion hook.
    //!
    //! These exercise the full helper end-to-end (DB → merge dispatch → WS
    //! broadcast → audit row) using a real git repo in a tempdir for the
    //! standalone path and the `dispatch.rs::tests`-style echo runner for
    //! the SaaS path. The standalone hook in `pty_agent::on_agent_exit`
    //! and the SaaS hook in `runner_ws::AgentStopped` both call the same
    //! [`run_merge_step`] (via [`on_task_agent_completed`]), so covering
    //! the helper directly is equivalent to covering both call sites.

    use super::*;

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use rusqlite::params;
    use tempfile::TempDir;
    use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

    use crate::config::Effort;
    use crate::db::Db;
    use crate::saas::runner_protocol::{Envelope, MergeOutcome as WireMergeOutcome, WireMessage};
    use crate::saas::runner_ws::{
        ConnectedRunner, RunnerRegistry, RunnerResponse, new_runner_registry,
    };

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Initialize the full DB schema in a tempdir. Mirrors what production
    /// `crate::db::init` does — gets `agents` / `plan_auto_mode` /
    /// `audit_logs` / `ci_runs` / `runners` / etc. without any of the
    /// migration-table-less duplicate-column noise.
    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("branchwork.db");
        (crate::db::init(&path), dir)
    }

    /// Build a minimal `AppState` wired with real DB + broadcast + runner
    /// registry. `plans_dir` is unused on the merge-only path but the
    /// type wants something non-empty.
    fn test_app_state(
        db: Db,
        runners: RunnerRegistry,
        plans_dir: PathBuf,
    ) -> (AppState, broadcast::Receiver<String>) {
        let (broadcast_tx, rx) = broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/nonexistent/branchwork-server"),
            0,
            true,
        );
        let state = AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings.json"),
        };
        (state, rx)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        if !out.status.success() {
            panic!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    fn git_head_sha(cwd: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Initialise a git repo at `cwd` with master + an initial commit.
    fn git_init_master(cwd: &Path) {
        std::fs::create_dir_all(cwd).unwrap();
        run_git(cwd, &["init", "-q", "-b", "master"]);
        run_git(cwd, &["config", "user.email", "t@t.test"]);
        run_git(cwd, &["config", "user.name", "Test"]);
        std::fs::write(cwd.join("README.md"), "init").unwrap();
        run_git(cwd, &["add", "README.md"]);
        run_git(cwd, &["commit", "-q", "-m", "initial"]);
    }

    /// Create a branch off master with `with_commit` controlling whether
    /// it has a commit ahead. Always returns to master.
    fn git_create_task_branch(cwd: &Path, branch: &str, with_commit: bool) {
        run_git(cwd, &["checkout", "-q", "-b", branch]);
        if with_commit {
            std::fs::write(cwd.join("work.txt"), "work").unwrap();
            run_git(cwd, &["add", "work.txt"]);
            run_git(cwd, &["commit", "-q", "-m", "task work"]);
        }
        run_git(cwd, &["checkout", "-q", "master"]);
    }

    fn seed_agent(db: &Db, id: &str, cwd: &Path, plan: &str, task: &str, branch: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents \
                (id, session_id, cwd, status, mode, plan_name, task_id, branch, source_branch, org_id) \
             VALUES (?1, ?1, ?2, 'completed', 'pty', ?3, ?4, ?5, 'master', 'default-org')",
            params![id, cwd.to_string_lossy(), plan, task, branch],
        )
        .unwrap();
    }

    fn enable_auto_mode(db: &Db, plan: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1) \
             ON CONFLICT(plan_name) DO UPDATE SET enabled = 1, paused_reason = NULL",
            params![plan],
        )
        .unwrap();
    }

    fn paused_reason(db: &Db, plan: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
            params![plan],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn audit_actions_for(db: &Db, resource_id: &str) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT action FROM audit_logs WHERE resource_id = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(params![resource_id], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    }

    /// Drain the broadcast channel and parse each frame's `type` field.
    /// The WS broadcast is fire-and-forget; we just collect what's in the
    /// queue right now, not what arrives later.
    fn drain_event_types(rx: &mut broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && let Some(t) = v.get("type").and_then(|t| t.as_str())
            {
                out.push(t.to_string());
            }
        }
        out
    }

    /// Install a stub runner whose `command_tx` pipes outgoing envelopes
    /// into `respond`, which decides what `RunnerResponse` to deliver on
    /// the matching `pending` oneshot. Returns a receiver of the raw
    /// outgoing payloads so tests can assert on the exact wire shape.
    async fn install_echo_runner<F>(
        registry: &RunnerRegistry,
        runner_id: &str,
        respond: F,
    ) -> mpsc::UnboundedReceiver<String>
    where
        F: Fn(&WireMessage) -> Option<RunnerResponse> + Send + Sync + 'static,
    {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        let (echo_tx, echo_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(payload) = cmd_rx.recv().await {
                let _ = echo_tx.send(payload.clone());
                let envelope: Envelope = match serde_json::from_str(&payload) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let req_id = match req_id_for(&envelope.message) {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                if let Some(reply) = respond(&envelope.message)
                    && let Some(tx) = pending_clone.lock().await.remove(&req_id)
                {
                    let _ = tx.send(reply);
                }
            }
        });

        registry.lock().await.insert(
            runner_id.to_string(),
            ConnectedRunner {
                command_tx: cmd_tx,
                hostname: None,
                version: None,
                pending,
            },
        );
        echo_rx
    }

    /// Test-local copy of `runner_rpc::req_id_for` for the variants the
    /// auto-mode merge path actually uses. The production fn is private;
    /// duplicating just-what-we-need here keeps the test self-contained.
    fn req_id_for(msg: &WireMessage) -> Option<&str> {
        match msg {
            WireMessage::GetDefaultBranch { req_id, .. }
            | WireMessage::ListBranches { req_id, .. }
            | WireMessage::MergeBranch { req_id, .. }
            | WireMessage::PushBranch { req_id, .. } => Some(req_id),
            _ => None,
        }
    }

    fn seed_runner_row(db: &Db, runner_id: &str, org_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
             VALUES (?1, 'test', ?2, 'online', datetime('now'))",
            params![runner_id, org_id],
        )
        .unwrap();
    }

    // ── Standalone path ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn standalone_clean_completion_merges_and_broadcasts_auto_mode_merged() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Add a stub workflow + origin remote so trigger_after_merge has
        // something to push against; the brief requires asserting the
        // post-merge CI pipeline fires for canonical-default merges.
        std::fs::create_dir_all(cwd.join(".github").join("workflows")).unwrap();
        std::fs::write(cwd.join(".github").join("workflows").join("ci.yml"), "name: ci\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n").unwrap();
        run_git(&cwd, &["add", ".github/workflows/ci.yml"]);
        run_git(&cwd, &["commit", "-q", "-m", "add ci workflow"]);
        let origin = dir.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(init.status.success());
        run_git(
            &cwd,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        // Push master to origin so it has a HEAD when the trigger pushes.
        run_git(&cwd, &["push", "-q", "-u", "origin", "master"]);

        git_create_task_branch(&cwd, "branchwork/p/1.1", true);
        let master_before = git_head_sha(&cwd);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1").await;

        // Trunk SHA advanced — branch was actually merged.
        let master_after = git_head_sha(&cwd);
        assert_ne!(master_before, master_after, "master should advance");

        // Broadcast event "auto_mode_merged" (alongside the inner
        // "agent_branch_merged" that merge_agent_branch_inner emits).
        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_mode_merged".to_string()),
            "expected auto_mode_merged in {events:?}"
        );

        // Plan stays unpaused on success.
        assert!(paused_reason(&db, "p").is_none());

        // Audit log carries the auto_mode.merged action.
        let actions = audit_actions_for(&db, "agent-1");
        assert!(
            actions.iter().any(|a| a == actions::AUTO_MODE_MERGED),
            "expected {} in {actions:?}",
            actions::AUTO_MODE_MERGED
        );

        // ci::trigger_after_merge is spawned by the merge inner; for a
        // canonical-default merge it pushes + writes a pending ci_runs
        // row. Poll for the row with a short deadline — the spawn races
        // the assertion otherwise.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut ci_run_count: i64 = 0;
        while std::time::Instant::now() < deadline {
            ci_run_count = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM ci_runs WHERE plan_name = ?1",
                    params!["p"],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
            };
            if ci_run_count > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            ci_run_count, 1,
            "expected ci::trigger_after_merge to insert a pending ci_runs row"
        );
    }

    #[tokio::test]
    async fn standalone_no_commit_pauses_with_merge_failed_reason() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Branch with NO commit ahead of master — the unattended-contract
        // violation. The merge dispatcher returns an `EmptyBranch` outcome
        // and the auto-mode helper records it as `merge_failed: ...`.
        git_create_task_branch(&cwd, "branchwork/p/1.1", false);
        let master_before = git_head_sha(&cwd);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1").await;

        // Master untouched.
        assert_eq!(git_head_sha(&cwd), master_before, "master should not move");

        // Pause reason recorded; starts with `merge_failed:` because the
        // wire outcome is EmptyBranch (mapped through the inner merge fn
        // to a "task branch has no commits" error string).
        let reason = paused_reason(&db, "p").expect("plan should be paused");
        assert!(
            reason.starts_with("merge_failed:"),
            "expected merge_failed prefix, got: {reason}"
        );

        // Broadcast event "auto_mode_paused".
        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_mode_paused".to_string()),
            "expected auto_mode_paused in {events:?}"
        );

        // Audit log carries the auto_mode.paused action.
        let actions = audit_actions_for(&db, "p");
        assert!(
            actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED),
            "expected {} in {actions:?}",
            actions::AUTO_MODE_PAUSED
        );
    }

    #[tokio::test]
    async fn auto_mode_disabled_is_a_silent_no_op() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        git_create_task_branch(&cwd, "branchwork/p/1.1", true);
        let master_before = git_head_sha(&cwd);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");
        // No `enable_auto_mode` — gate stays false.

        on_task_agent_completed(&state, "agent-1", "p", "1.1").await;
        // Allow the spawned task (if it had one) a moment to no-op.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Master unchanged.
        assert_eq!(git_head_sha(&cwd), master_before);

        // No auto-mode events.
        let events = drain_event_types(&mut rx);
        assert!(
            !events.iter().any(|e| e.starts_with("auto_mode_")),
            "no auto_mode_* events expected, got: {events:?}"
        );

        // No audit rows.
        assert!(audit_actions_for(&db, "agent-1").is_empty());
        assert!(audit_actions_for(&db, "p").is_empty());
    }

    // ── SaaS path ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn saas_clean_completion_dispatches_merge_and_broadcasts() {
        let (db, _dir) = fresh_db();
        seed_runner_row(&db, "runner-1", "default-org");

        let runners = new_runner_registry();
        // Stub runner replies: GetDefaultBranch -> Some("master"); the
        // merge inner does NOT call ListBranches because there's no
        // explicit `into`; MergeBranch -> Ok with a fixed sha.
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Ok {
                    merged_sha: "deadbeef".into(),
                }))
            }
            // PushBranch may fire from the spawned trigger_after_merge
            // (org_has_runner === true skips the local has_github_actions
            // check). The runner-side push is best-effort here and the
            // auto-mode hook itself doesn't await it.
            WireMessage::PushBranch { .. } => Some(RunnerResponse::PushResult {
                ok: true,
                stderr: None,
            }),
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-saas-plans"),
        );
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1").await;

        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_mode_merged".to_string()),
            "expected auto_mode_merged in {events:?}"
        );
        assert!(paused_reason(&db, "p").is_none());

        let actions = audit_actions_for(&db, "agent-1");
        assert!(
            actions.iter().any(|a| a == actions::AUTO_MODE_MERGED),
            "expected {} in {actions:?}",
            actions::AUTO_MODE_MERGED
        );
    }

    #[tokio::test]
    async fn saas_empty_branch_outcome_pauses_plan() {
        let (db, _dir) = fresh_db();
        seed_runner_row(&db, "runner-1", "default-org");

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::EmptyBranch))
            }
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-saas-plans"),
        );
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1").await;

        let reason = paused_reason(&db, "p").expect("plan should be paused");
        assert!(
            reason.starts_with("merge_failed:"),
            "expected merge_failed prefix, got: {reason}"
        );

        let events = drain_event_types(&mut rx);
        assert!(events.contains(&"auto_mode_paused".to_string()));

        let actions = audit_actions_for(&db, "p");
        assert!(actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED));
    }

    /// Wire-shape pin: the SaaS path emits a `MergeBranch` envelope to the
    /// runner (via the inner merge fn's git_ops dispatch). Acceptance from
    /// the brief: "assert the server emits `MergeBranch` to the runner".
    #[tokio::test]
    async fn saas_path_emits_merge_branch_envelope_to_runner() {
        let (db, _dir) = fresh_db();
        seed_runner_row(&db, "runner-1", "default-org");

        let runners = new_runner_registry();
        let mut outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Ok {
                    merged_sha: "deadbeef".into(),
                }))
            }
            WireMessage::PushBranch { .. } => Some(RunnerResponse::PushResult {
                ok: true,
                stderr: None,
            }),
            _ => None,
        })
        .await;

        let (state, _rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-saas-plans"),
        );
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1").await;

        // Drain everything the runner saw and look for MergeBranch.
        let mut saw_merge = false;
        while let Ok(payload) = outgoing.try_recv() {
            if payload.contains("\"type\":\"merge_branch\"") {
                saw_merge = true;
                // The MergeBranch envelope must carry the task branch.
                assert!(
                    payload.contains("branchwork/p/1.1"),
                    "merge_branch envelope missing task branch: {payload}"
                );
            }
        }
        assert!(saw_merge, "expected a merge_branch envelope on the wire");
    }
}
