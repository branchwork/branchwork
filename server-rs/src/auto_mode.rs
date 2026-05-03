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

use std::future::Future;
use std::time::{Duration, Instant};

use rand::Rng;
use rusqlite::params;

use crate::audit;
use crate::db;
use crate::saas::dispatch::{
    CiStatusError, get_ci_run_status_dispatch, has_github_actions_dispatch,
    merge_agent_branch_dispatch,
};
use crate::saas::runner_protocol::CiAggregate;
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

// ── Phase 2: CI poll loop ───────────────────────────────────────────────────

/// Outcome of [`wait_for_ci`] — what the loop should do next for a merged
/// SHA. The loop body in Phase 2.x consumes this to decide between
/// advancing to the next task (Green / NotConfigured), spawning a fix
/// agent (Red), or pausing the plan (Stalled).
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiOutcome {
    /// CI ran every workflow for the SHA and they all passed (or were
    /// intentionally skipped — the upstream-poison rule in
    /// `ci::aggregate` already collapses benign skips into `success`).
    Green,
    /// CI ran and at least one workflow failed / was cancelled / timed
    /// out. `failing_run_id` is the root-cause run id (the aggregator
    /// guarantees it's set for these conclusions); the loop hands it to
    /// the fix-prompt builder so the agent loads the right log.
    Red { failing_run_id: Option<String> },
    /// No terminal verdict before the total timeout (~20 min). Loop pauses
    /// the plan with reason `"ci_stalled"` so a human can investigate.
    Stalled,
    /// Project has no GitHub Actions configured. Treated as green by the
    /// loop — there is no CI to gate on.
    NotConfigured,
}

/// Poll-loop tuning. Hard-coded for now per the task brief; a plan-level
/// override is a later iteration. Pulled out as a struct so unit tests can
/// shorten the timeouts without exercising real wall-clock behaviour.
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
#[derive(Debug, Clone, Copy)]
struct WaitForCiConfig {
    /// Base interval between polls (jittered ± `jitter_window`).
    poll_interval: Duration,
    /// Symmetric jitter window applied around `poll_interval` per tick.
    jitter_window: Duration,
    /// Hard cap on the total wait. After this elapses the loop returns
    /// [`CiOutcome::Stalled`] regardless of the in-flight aggregate.
    total_timeout: Duration,
}

impl Default for WaitForCiConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(15),
            jitter_window: Duration::from_secs(2),
            total_timeout: Duration::from_secs(20 * 60),
        }
    }
}

/// Poll CI status for `merged_sha` until it lands a terminal verdict, the
/// total timeout (20 min) elapses, or it turns out the project has no
/// GitHub Actions configured.
///
/// Mode-aware via [`crate::saas::dispatch`]: the standalone path resolves
/// CI state from the local `gh` shell-out, the SaaS path round-trips
/// through the runner. Callers stay mode-agnostic.
///
/// `agent_id` is only used by [`has_github_actions_dispatch`] to look up
/// the agent's cwd; the actual CI poll is keyed by `(plan_name, task_id,
/// merged_sha)`.
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
pub async fn wait_for_ci(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    agent_id: &str,
    merged_sha: &str,
) -> CiOutcome {
    wait_for_ci_inner(
        plan_name,
        task_id,
        merged_sha,
        || has_github_actions_dispatch(state, org_id, agent_id),
        || get_ci_run_status_dispatch(state, org_id, plan_name, task_id, merged_sha),
        WaitForCiConfig::default(),
    )
    .await
}

/// Body of [`wait_for_ci`] with the dispatch closures injected. Lets unit
/// tests stub all four outcomes without setting up a runner registry, a
/// `gh` binary, or a real `ci_runs` row. Each closure may be invoked many
/// times across the lifetime of the call.
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
async fn wait_for_ci_inner<HasFn, GetFn, HasFut, GetFut>(
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    has_actions: HasFn,
    get_status: GetFn,
    config: WaitForCiConfig,
) -> CiOutcome
where
    HasFn: Fn() -> HasFut,
    HasFut: Future<Output = bool>,
    GetFn: Fn() -> GetFut,
    GetFut: Future<Output = Result<Option<CiAggregate>, CiStatusError>>,
{
    if !has_actions().await {
        return CiOutcome::NotConfigured;
    }

    let deadline = Instant::now() + config.total_timeout;
    loop {
        match get_status().await {
            Ok(Some(agg)) if agg.status == "completed" => {
                return classify_aggregate(plan_name, task_id, merged_sha, &agg);
            }
            Ok(Some(_)) => {
                // Aggregate exists but at least one workflow is still
                // queued/in_progress — keep polling.
            }
            Ok(None) => {
                // No workflow runs for this SHA yet (or `gh` returned
                // nothing). The brief is explicit: keep polling.
            }
            Err(e) => {
                // Transport failure (RPC) or schema drift (InvalidResponse).
                // The brief is explicit: retry on the next tick without
                // surfacing the error to the caller.
                eprintln!(
                    "[auto_mode] CI status fetch failed for {plan_name}/{task_id}@{merged_sha}: {e} — retrying"
                );
            }
        }

        if Instant::now() >= deadline {
            return CiOutcome::Stalled;
        }

        let sleep = jittered_interval(config.poll_interval, config.jitter_window);
        tokio::time::sleep(sleep).await;
    }
}

/// Map a `CiAggregate` with `status=="completed"` to the loop outcome.
/// The aggregator (in `ci::aggregate::compute`) is the single place the
/// upstream-poison rule lives — the loop just consumes its verdict and
/// **must not** re-interpret raw per-run skips. Defensive: any conclusion
/// outside the documented set degrades to Stalled so the plan pauses
/// rather than silently advancing on an unknown verdict.
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
fn classify_aggregate(
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    agg: &CiAggregate,
) -> CiOutcome {
    match agg.conclusion.as_deref() {
        Some("success") => CiOutcome::Green,
        Some("failure") | Some("cancelled") | Some("timed_out") => CiOutcome::Red {
            failing_run_id: agg.failing_run_id.clone(),
        },
        other => {
            eprintln!(
                "[auto_mode] unexpected CI conclusion {other:?} for {plan_name}/{task_id}@{merged_sha} — treating as Stalled"
            );
            CiOutcome::Stalled
        }
    }
}

/// Add ±`jitter_window` to `interval` for the next sleep tick. Matches the
/// brief: "15 s, jittered ± 2 s". Clamped to a minimum of 1 ms so a
/// degenerate config can't busy-spin.
#[allow(dead_code)] // wired into the auto-mode loop in Phase 2.x of this plan
fn jittered_interval(interval: Duration, jitter_window: Duration) -> Duration {
    let interval_ms = interval.as_millis() as i64;
    let window_ms = jitter_window.as_millis() as i64;
    let offset_ms = if window_ms == 0 {
        0
    } else {
        rand::rng().random_range(-window_ms..=window_ms)
    };
    Duration::from_millis((interval_ms + offset_ms).max(1) as u64)
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
    /// auto-mode merge + CI-poll paths actually use. The production fn is
    /// private; duplicating just-what-we-need here keeps the test
    /// self-contained.
    fn req_id_for(msg: &WireMessage) -> Option<&str> {
        match msg {
            WireMessage::GetDefaultBranch { req_id, .. }
            | WireMessage::ListBranches { req_id, .. }
            | WireMessage::MergeBranch { req_id, .. }
            | WireMessage::PushBranch { req_id, .. }
            | WireMessage::HasGithubActions { req_id, .. }
            | WireMessage::GetCiRunStatus { req_id, .. } => Some(req_id),
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

    // ── wait_for_ci: closure-stubbed unit tests ─────────────────────────────

    use crate::saas::runner_protocol::{CiAggregate, CiRunSummary};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tight config for unit tests so the loop ticks fast and the Stalled
    /// branch fires within ~100 ms instead of 20 minutes.
    fn fast_config() -> WaitForCiConfig {
        WaitForCiConfig {
            poll_interval: Duration::from_millis(5),
            jitter_window: Duration::from_millis(2),
            total_timeout: Duration::from_millis(80),
        }
    }

    fn aggregate_success() -> CiAggregate {
        CiAggregate {
            status: "completed".to_string(),
            conclusion: Some("success".to_string()),
            runs: vec![CiRunSummary {
                run_id: "1".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            }],
            failing_run_id: None,
        }
    }

    fn aggregate_failure(failing: &str) -> CiAggregate {
        CiAggregate {
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            runs: vec![CiRunSummary {
                run_id: failing.to_string(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            }],
            failing_run_id: Some(failing.to_string()),
        }
    }

    fn aggregate_in_progress() -> CiAggregate {
        CiAggregate {
            status: "in_progress".to_string(),
            conclusion: None,
            runs: vec![CiRunSummary {
                run_id: "1".into(),
                workflow_name: "tests".into(),
                status: "in_progress".into(),
                conclusion: None,
                skipped_due_to_upstream: false,
            }],
            failing_run_id: None,
        }
    }

    /// The Reglyze fixture: tests=failure, lint=success, deploy=skipped.
    /// `mark_upstream_skips` (in `ci::aggregate`) flips `deploy.skipped_due_to_upstream`,
    /// `compute` then picks `failing_run_id="100"` (tests, not deploy).
    fn aggregate_reglyze_three_runs() -> CiAggregate {
        let mut runs = vec![
            CiRunSummary {
                run_id: "100".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "101".into(),
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "102".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
            },
        ];
        crate::ci::aggregate::mark_upstream_skips(&mut runs);
        crate::ci::aggregate::compute(&runs)
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_not_configured_when_has_actions_false() {
        let get_calls = Arc::new(AtomicUsize::new(0));
        let get_calls_inner = get_calls.clone();

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { false },
            move || {
                let count = get_calls_inner.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                }
            },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::NotConfigured);
        assert_eq!(
            get_calls.load(Ordering::SeqCst),
            0,
            "get_status must not be called when has_actions returns false"
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_green_on_success_aggregate() {
        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            || async { Ok(Some(aggregate_success())) },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::Green);
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_red_with_failing_run_id_on_failure_aggregate() {
        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            || async { Ok(Some(aggregate_failure("42"))) },
            fast_config(),
        )
        .await;

        assert_eq!(
            outcome,
            CiOutcome::Red {
                failing_run_id: Some("42".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_red_for_cancelled_conclusion() {
        let mut agg = aggregate_failure("99");
        agg.conclusion = Some("cancelled".to_string());

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            move || {
                let agg = agg.clone();
                async move { Ok(Some(agg)) }
            },
            fast_config(),
        )
        .await;

        assert_eq!(
            outcome,
            CiOutcome::Red {
                failing_run_id: Some("99".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_red_for_timed_out_conclusion() {
        let mut agg = aggregate_failure("77");
        agg.conclusion = Some("timed_out".to_string());

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            move || {
                let agg = agg.clone();
                async move { Ok(Some(agg)) }
            },
            fast_config(),
        )
        .await;

        assert_eq!(
            outcome,
            CiOutcome::Red {
                failing_run_id: Some("77".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_returns_stalled_after_timeout() {
        // get_status always returns Ok(None) (no runs yet) — the loop must
        // keep polling until total_timeout, then surface Stalled.
        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            || async { Ok(None) },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::Stalled);
    }

    #[tokio::test]
    async fn wait_for_ci_inner_keeps_polling_on_in_progress_then_returns_terminal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            move || {
                let count = calls_inner.clone();
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(if n == 0 {
                        aggregate_in_progress()
                    } else {
                        aggregate_success()
                    }))
                }
            },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::Green);
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "loop must have polled at least twice (in_progress then completed)"
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_keeps_polling_on_rpc_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_inner = calls.clone();

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            move || {
                let count = calls_inner.clone();
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Err(CiStatusError::InvalidResponse)
                    } else {
                        Ok(Some(aggregate_success()))
                    }
                }
            },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::Green);
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "loop must have retried after the RPC error"
        );
    }

    #[tokio::test]
    async fn wait_for_ci_inner_unknown_conclusion_treats_as_stalled() {
        let mut agg = aggregate_success();
        agg.conclusion = Some("action_required".to_string());

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-1",
            || async { true },
            move || {
                let agg = agg.clone();
                async move { Ok(Some(agg)) }
            },
            fast_config(),
        )
        .await;

        assert_eq!(outcome, CiOutcome::Stalled);
    }

    /// Headline regression test from the brief: stub the dispatch to return
    /// the three-runs aggregate from 0.4's regression test (tests=failure,
    /// lint=success, deploy=skipped-due-to-upstream). The loop must emit
    /// `CiOutcome::Red { failing_run_id: Some("100") }` — NOT Green and
    /// NOT `failing_run_id: Some("102")` (the skipped deploy).
    #[tokio::test]
    async fn wait_for_ci_inner_reglyze_three_runs_returns_red_with_tests_id_not_deploy_id() {
        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-reglyze",
            || async { true },
            || async { Ok(Some(aggregate_reglyze_three_runs())) },
            fast_config(),
        )
        .await;

        assert_eq!(
            outcome,
            CiOutcome::Red {
                failing_run_id: Some("100".to_string()),
            },
            "loop must surface the root-cause failing run id (tests=100), \
             not the upstream-skipped deploy=102 — this is the Reglyze bug"
        );
    }

    // ── wait_for_ci: integration tests ──────────────────────────────────────

    /// Standalone branch: project has no `.github/workflows/` directory —
    /// `has_github_actions_dispatch` returns false, the loop short-circuits
    /// to NotConfigured without calling `get_ci_run_status_dispatch`.
    /// Exercises the full real dispatch path, no closure injection.
    #[tokio::test]
    async fn wait_for_ci_standalone_no_workflows_returns_not_configured() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, _rx) = test_app_state(db, new_runner_registry(), plans_dir);

        let outcome = wait_for_ci(&state, "default-org", "p", "1.1", "agent-1", "sha-1").await;

        assert_eq!(outcome, CiOutcome::NotConfigured);
    }

    /// Standalone branch: `.github/workflows/ci.yml` is present (so
    /// `has_github_actions_dispatch` returns true) AND a real `ci_runs`
    /// row exists for the merged SHA (the kind `ci::trigger_after_merge`
    /// would have written). The dispatcher's `gh run list` shell-out
    /// returns nothing in the test environment (no `gh` auth), so the
    /// loop polls until `total_timeout` elapses and surfaces `Stalled`.
    /// Uses a tight config to bound the wall-clock cost.
    #[tokio::test]
    async fn wait_for_ci_standalone_workflows_present_eventually_stalls() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(cwd.join(".github").join("workflows")).unwrap();
        std::fs::write(
            cwd.join(".github").join("workflows").join("ci.yml"),
            "name: ci\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
        )
        .unwrap();
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");

        // Real ci_runs row, as ci::trigger_after_merge would have written.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO ci_runs \
                   (plan_name, task_number, agent_id, provider, commit_sha, branch, status, org_id) \
                 VALUES ('p', '1.1', 'agent-1', 'github', 'sha-merged', 'branchwork/p/1.1', 'pending', 'default-org')",
                [],
            )
            .unwrap();
        }

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, _rx) = test_app_state(db, new_runner_registry(), plans_dir);

        let outcome = wait_for_ci_inner(
            "p",
            "1.1",
            "sha-merged",
            || has_github_actions_dispatch(&state, "default-org", "agent-1"),
            || get_ci_run_status_dispatch(&state, "default-org", "p", "1.1", "sha-merged"),
            // Tight timeout so this test stays under a second; the real
            // 20-min cap would be ridiculous in CI.
            WaitForCiConfig {
                poll_interval: Duration::from_millis(10),
                jitter_window: Duration::from_millis(2),
                total_timeout: Duration::from_millis(150),
            },
        )
        .await;

        assert_eq!(outcome, CiOutcome::Stalled);
    }

    /// SaaS branch: registered runner replies to both `HasGithubActions`
    /// (with `present=true`) and `GetCiRunStatus` (with a canned
    /// success-conclusion `CiAggregate`). The loop must surface `Green`.
    #[tokio::test]
    async fn wait_for_ci_saas_runner_returns_green_aggregate_drives_green_outcome() {
        let (db, _dir) = fresh_db();
        seed_runner_row(&db, "runner-1", "default-org");
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_success(),
            ))),
            _ => None,
        })
        .await;

        let (state, _rx) =
            test_app_state(db, runners, PathBuf::from("/tmp/auto-mode-saas-wait-plans"));

        let outcome = wait_for_ci(&state, "default-org", "p", "1.1", "agent-1", "sha-merged").await;

        assert_eq!(outcome, CiOutcome::Green);
    }

    /// SaaS branch: runner replies with the Reglyze failure aggregate. The
    /// loop must surface `Red { failing_run_id: Some("100") }` — the
    /// root-cause `tests` run id, not the upstream-skipped `deploy`.
    /// Pairs with the closure-stubbed Reglyze test above to prove the
    /// regression is caught both via direct injection and via the live
    /// dispatch round-trip.
    #[tokio::test]
    async fn wait_for_ci_saas_runner_returns_failure_aggregate_drives_red_with_failing_run_id() {
        let (db, _dir) = fresh_db();
        seed_runner_row(&db, "runner-1", "default-org");
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_reglyze_three_runs(),
            ))),
            _ => None,
        })
        .await;

        let (state, _rx) =
            test_app_state(db, runners, PathBuf::from("/tmp/auto-mode-saas-wait-plans"));

        let outcome = wait_for_ci(&state, "default-org", "p", "1.1", "agent-1", "sha-merged").await;

        assert_eq!(
            outcome,
            CiOutcome::Red {
                failing_run_id: Some("100".to_string()),
            },
            "SaaS round-trip must surface tests run id (100), not the \
             upstream-skipped deploy run id (102)"
        );
    }
}
