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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rand::Rng;
use rusqlite::params;

use crate::agents::pty_agent::StartPtyOpts;
use crate::agents::spawn_ops::start_agent_dispatch;
use crate::audit;
use crate::db;
use crate::saas::dispatch::{
    CiStatusError, fetch_failure_log_dispatch, get_ci_run_status_dispatch,
    has_github_actions_dispatch, merge_agent_branch_dispatch,
};
use crate::saas::runner_protocol::CiAggregate;
use crate::state::AppState;
use crate::ws::broadcast_event;

/// Audit-log action constants for auto-mode transitions.
pub mod actions {
    /// A task agent completed and the loop merged its branch.
    pub const AUTO_MODE_MERGED: &str = "auto_mode.merged";
    /// The loop aborted itself for a plan and recorded a pause reason.
    pub const AUTO_MODE_PAUSED: &str = "auto_mode.paused";
    /// CI came back green (or wasn't configured) — loop advanced.
    pub const AUTO_MODE_CI_PASSED: &str = "auto_mode.ci_passed";
    /// CI came back red — loop paused or spawned a fix agent.
    pub const AUTO_MODE_CI_FAILED: &str = "auto_mode.ci_failed";
    /// A fix agent was spawned for a Red CI outcome.
    pub const AUTO_MODE_FIX_SPAWNED: &str = "auto_mode.fix_spawned";
}

/// Phase labels broadcast on the `auto_mode_state` event so the UI pill can
/// reflect the current step. The set is closed: any new transition needs a
/// new constant + matching frontend label.
mod state_labels {
    pub const MERGING: &str = "merging";
    pub const AWAITING_CI: &str = "awaiting_ci";
    pub const ADVANCING: &str = "advancing";
    pub const PAUSED: &str = "paused";
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

    // Fix agents (`task_id` carries the `-fix-<n>` suffix that
    // [`spawn_fix_agent`] stamps on) flow through `on_fix_agent_completed`:
    // their fix branch is merged into the canonical default and CI is
    // re-polled on the new SHA. On Green the original task is marked
    // completed in `task_status` and `try_auto_advance` fires for the
    // original task id; on Red the loop spawns the next fix attempt.
    let is_fix_agent = task_id.contains("-fix-");
    tokio::spawn(async move {
        if is_fix_agent {
            on_fix_agent_completed(&state, &org_id, &agent_id, &plan_name, &task_id).await;
        } else {
            run_state_machine(&state, &org_id, &agent_id, &plan_name, &task_id).await;
        }
    });
}

/// Outcome of [`run_merge_step`] — what the orchestrator should do next.
/// Pulled out so the orchestrator can chain into the CI gate without the
/// merge step having to know about CI at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MergeStepOutcome {
    /// Merge succeeded; broadcast + audit already happened. SHA carried so
    /// the orchestrator can hand it to [`wait_for_ci`].
    Merged(String),
    /// Merge failed (conflict or other error). The plan was already paused
    /// (broadcast + audit'd by the merge step itself); the orchestrator
    /// just adds an `auto_mode_state(paused)` pill update on top.
    Paused,
}

/// Body of the merge step: dispatch the merge and map its outcome to the
/// existing `auto_mode_merged` / `auto_mode_paused` events + audit rows.
/// Returns a [`MergeStepOutcome`] so the orchestrator can chain into the
/// CI gate without re-reading state. Pulled out as a free function so
/// unit tests can drive just the merge half synchronously without
/// triggering the CI poll.
async fn run_merge_step(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
) -> MergeStepOutcome {
    let outcome = merge_agent_branch_dispatch(state, org_id, agent_id, None).await;

    if let Some(sha) = outcome.merged_sha {
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
        return MergeStepOutcome::Merged(sha);
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
    MergeStepOutcome::Paused
}

/// Broadcast an `auto_mode_state` event with the current loop phase. This
/// is the UI-pill feed: every transition (`merging` → `awaiting_ci` →
/// `advancing|paused`) emits exactly one of these so the dashboard can
/// keep its per-plan status pill live without reading the DB.
fn broadcast_state(
    state: &AppState,
    plan_name: &str,
    task_id: &str,
    label: &str,
    sha: Option<&str>,
    reason: Option<&str>,
) {
    let mut payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "state": label,
    });
    if let Some(sha) = sha {
        payload["sha"] = serde_json::json!(sha);
    }
    if let Some(reason) = reason {
        payload["reason"] = serde_json::json!(reason);
    }
    broadcast_event(&state.broadcast_tx, "auto_mode_state", payload);
}

/// State-machine driver: wraps the merge step in a CI poll + advance
/// chain. Mirrors the brief code:
///
/// ```text
/// match merge_outcome {
///     Merged(sha) => match wait_for_ci(...).await {
///         Green | NotConfigured => try_auto_advance(...),
///         Red { ci_run_id }    => pause(ci_failed: <ci_run_id>),
///         Stalled              => pause(ci_stalled),
///     },
///     Conflict | Failed => already paused in run_merge_step,
/// }
/// ```
///
/// Each transition broadcasts an `auto_mode_state` event so the UI pill
/// stays live; the merge-side `auto_mode_merged` / `auto_mode_paused`
/// events from [`run_merge_step`] still fire (existing dashboard
/// listeners depend on them).
async fn run_state_machine(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
) {
    broadcast_state(state, plan_name, task_id, state_labels::MERGING, None, None);

    let merged_sha = match run_merge_step(state, org_id, agent_id, plan_name, task_id).await {
        MergeStepOutcome::Merged(sha) => sha,
        MergeStepOutcome::Paused => {
            // run_merge_step has already paused + audit-logged; emit only
            // the pill update so the UI flips out of `merging`.
            broadcast_state(state, plan_name, task_id, state_labels::PAUSED, None, None);
            return;
        }
    };

    broadcast_state(
        state,
        plan_name,
        task_id,
        state_labels::AWAITING_CI,
        Some(&merged_sha),
        None,
    );

    let ci_outcome = wait_for_ci(state, org_id, plan_name, task_id, agent_id, &merged_sha).await;

    match ci_outcome {
        CiOutcome::Green | CiOutcome::NotConfigured => {
            on_ci_passed(state, org_id, plan_name, task_id, &merged_sha, &ci_outcome).await;
        }
        CiOutcome::Red { failing_run_id } => {
            on_ci_failed(
                state,
                org_id,
                plan_name,
                task_id,
                &merged_sha,
                failing_run_id.as_deref(),
            )
            .await;
        }
        CiOutcome::Stalled => {
            on_ci_stalled(state, org_id, plan_name, task_id, &merged_sha).await;
        }
    }
}

/// Green-or-NotConfigured branch: broadcast advancing, audit ci_passed,
/// then call `try_auto_advance` which spawns the next phase's tasks if
/// the current phase is fully done.
async fn on_ci_passed(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    ci_outcome: &CiOutcome,
) {
    broadcast_state(
        state,
        plan_name,
        task_id,
        state_labels::ADVANCING,
        Some(merged_sha),
        None,
    );

    let outcome_label = match ci_outcome {
        CiOutcome::Green => "green",
        CiOutcome::NotConfigured => "not_configured",
        _ => "unknown",
    };
    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "sha": merged_sha,
        "outcome": outcome_label,
    });
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_CI_PASSED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }

    let registry = state.registry.clone();
    let plans_dir = state.plans_dir.clone();
    let plan_name_owned = plan_name.to_string();
    let task_id_owned = task_id.to_string();
    let effort = *state.effort.lock().unwrap();
    let port = state.config_port();
    crate::agents::try_auto_advance(
        registry,
        plans_dir,
        plan_name_owned,
        task_id_owned,
        effort,
        port,
    )
    .await;
}

/// Red branch: pause with `ci_failed: <ci_run_id>`, broadcast
/// `auto_mode_paused`, broadcast `auto_mode_state(paused)`, audit
/// `AUTO_MODE_CI_FAILED`. Phase 3 will replace this with a bounded fix
/// loop; the brief is explicit that Phase 2 stops at "pause + record".
async fn on_ci_failed(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    failing_run_id: Option<&str>,
) {
    let id_str = failing_run_id.unwrap_or("unknown");
    let reason = format!("ci_failed: {id_str}");
    db::auto_mode_pause(&state.db, plan_name, &reason);

    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "sha": merged_sha,
        "reason": reason,
        "ci_run_id": failing_run_id,
    });
    broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
    broadcast_state(
        state,
        plan_name,
        task_id,
        state_labels::PAUSED,
        Some(merged_sha),
        Some(&reason),
    );
    let conn = state.db.lock().unwrap();
    audit::log(
        &conn,
        org_id,
        None,
        Some("branchwork-auto-mode"),
        actions::AUTO_MODE_CI_FAILED,
        audit::resources::PLAN,
        Some(plan_name),
        Some(&payload.to_string()),
    );
}

/// Stalled branch: pause with `ci_stalled`, broadcast `auto_mode_paused`,
/// broadcast `auto_mode_state(paused)`, audit `AUTO_MODE_PAUSED`. The
/// distinction from `ci_failed` is that no specific run id caused the
/// pause — CI just never reached a terminal verdict in time.
async fn on_ci_stalled(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
) {
    let reason = "ci_stalled".to_string();
    db::auto_mode_pause(&state.db, plan_name, &reason);

    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "sha": merged_sha,
        "reason": reason,
    });
    broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
    broadcast_state(
        state,
        plan_name,
        task_id,
        state_labels::PAUSED,
        Some(merged_sha),
        Some(&reason),
    );
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

// ── Phase 3: fix-on-red ─────────────────────────────────────────────────────

/// Spawn a fix agent to recover from a Red CI outcome.
///
/// Looks up the original task agent's cwd, builds the fix branch name
/// `branchwork/<plan>/<task>-fix-<attempt>`, fetches the failing-job log
/// via [`fetch_failure_log_dispatch`] (passing the explicit
/// `failing_run_id` rather than `None` so the loop never depends on the
/// runner-side cache lookup as the primary path), and dispatches the
/// spawn through [`start_agent_dispatch`] so SaaS mode emits a
/// `StartAgent` envelope to the runner and standalone mode delegates to
/// `start_pty_agent`.
///
/// A `task_fix_attempts` row is inserted **before** the spawn so the
/// count survives an in-flight kill — that count feeds the cap check in
/// T3.3. The agent_id is backfilled onto the same row once the dispatch
/// returns.
///
/// Returns `Some(agent_id)` on a successful spawn dispatch; `None` if
/// the original task agent could not be found in the `agents` table
/// (the fix loop has nowhere to point the new agent's cwd, so it bails).
///
/// The retry cap, the wiring from `on_ci_failed`, and the fix-merge
/// codepath all land in T3.2 / T3.3 — this function is the spawn
/// primitive they build on.
#[allow(dead_code)] // wired into on_ci_failed in T3.3 once the cap check lands
pub async fn spawn_fix_agent(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    failing_run_id: &str,
    attempt: u32,
) -> Option<String> {
    // 1. Look up the original task agent's cwd. We filter out fix-agent
    //    rows so a re-entrant spawn (attempt N > 1) doesn't accidentally
    //    pick up a previous fix agent's cwd.
    let cwd: PathBuf = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT cwd FROM agents \
             WHERE plan_name = ?1 AND task_id = ?2 AND task_id NOT LIKE '%-fix-%' \
             ORDER BY started_at DESC LIMIT 1",
            params![plan_name, task_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .map(PathBuf::from)?
    };

    // 2. Branch + fix-task-id naming. The completion handler in
    //    `on_task_agent_completed` keys off the `-fix-` substring to
    //    route fix agents through the T3.2 merge codepath instead of the
    //    standard advance state machine.
    let fix_branch = format!("branchwork/{plan_name}/{task_id}-fix-{attempt}");
    let fix_task_id = format!("{task_id}-fix-{attempt}");

    // 3. Fetch the failing-job log. Pass the explicit run id rather than
    //    None so the loop is never at the mercy of the runner-side cache
    //    lookup; assert the dispatcher echoes that id back so a future
    //    refactor can't quietly swap it for a different run.
    let (log, run_id_used) =
        fetch_failure_log_dispatch(state, org_id, plan_name, Some(failing_run_id)).await;
    debug_assert_eq!(
        run_id_used.as_deref(),
        Some(failing_run_id),
        "fetch_failure_log_dispatch should echo the explicit run id"
    );

    let prompt = build_fix_prompt(
        plan_name,
        task_id,
        &fix_branch,
        failing_run_id,
        log.as_deref(),
    );

    // 4. Record the attempt BEFORE the spawn. agent_id stays NULL until
    //    the dispatcher returns; the count is what enforces the cap in
    //    T3.3, so a kill mid-spawn must still leave the count incremented.
    //    PK = (plan_name, task_number, attempt) makes this idempotent on
    //    retry — duplicate triples are ignored, not overwritten.
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_fix_attempts \
                (plan_name, task_number, attempt, started_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, task_number, attempt) DO NOTHING",
            params![plan_name, task_id, attempt as i64],
        )
        .ok();
    }

    // 5. Mode-aware spawn. `is_continue=false` because the fix branch is
    //    fresh — there is no prior session to resume. driver/effort/budget
    //    inherit defaults so the fix agent looks identical to a task agent
    //    on the wire (the fix-marker lives only in `task_id`).
    let opts = StartPtyOpts {
        prompt,
        cwd: &cwd,
        plan_name: Some(plan_name),
        task_id: Some(&fix_task_id),
        effort: *state.effort.lock().unwrap(),
        branch: Some(&fix_branch),
        is_continue: false,
        max_budget_usd: None,
        driver: None,
        user_id: None,
        org_id: Some(org_id),
    };
    let agent_id = start_agent_dispatch(state, org_id, opts).await;

    // 6. Backfill agent_id onto the just-recorded row so the T3.2
    //    completion handler can join from agent_id back to its attempt.
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE task_fix_attempts SET agent_id = ?1 \
             WHERE plan_name = ?2 AND task_number = ?3 AND attempt = ?4",
            params![agent_id, plan_name, task_id, attempt as i64],
        )
        .ok();
    }

    Some(agent_id)
}

/// Compose the fix-agent prompt. Task-specific block first, then the
/// unattended-execution contract block from T0.7 appended verbatim so
/// the fix agent inherits the same commit-don't-push-don't-ask rules
/// every other auto-mode-spawned agent gets. Do NOT instruct the agent
/// to push or merge here — Branchwork's loop owns both, and the contract
/// block already forbids it.
#[allow(dead_code)] // exercised via spawn_fix_agent and a unit test
fn build_fix_prompt(
    plan_name: &str,
    task_id: &str,
    fix_branch: &str,
    failing_run_id: &str,
    log: Option<&str>,
) -> String {
    let log_block = match log {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => "(failure log unavailable — runner could not resolve it; \
              re-run `gh run view <id> --log-failed` manually if you need it)"
            .to_string(),
    };
    let contract = crate::agents::prompt::unattended_contract_block(fix_branch);
    format!(
        "CI failed on the merge of task {task_id} (plan {plan_name}) after the \
         auto-mode loop merged it into the canonical default branch.\n\
         \n\
         Root-cause CI run id: {failing_run_id}.\n\
         Other downstream workflows (e.g. deploy) may show as `skipped` because \
         of this failure — fix the root cause and the rest will re-run \
         automatically.\n\
         \n\
         Failing job log (truncated to ~8 KB tail):\n\
         {log_block}\n\
         \n\
         Goal: fix the regression on this branch ({fix_branch}). When CI passes \
         for the merged commit, the loop continues with the next task.\n\
         \n\
         {contract}",
    )
}

/// Called when a fix agent completes cleanly (its task_id carries the
/// `-fix-<n>` marker that [`spawn_fix_agent`] stamps on). Merges the fix
/// branch into the canonical default (NOT the original task branch — the
/// fix lands straight on trunk), re-polls CI on the resulting SHA, and
/// chains:
///
///   - Green / NotConfigured → close the `task_fix_attempts` row with
///     `outcome="green"`, mark the original task `completed` in
///     `task_status`, and call `try_auto_advance` for the original task
///     so the next phase / next-task spawn fires.
///   - Red → close the row with `outcome="red"`, audit
///     `AUTO_MODE_CI_FAILED`, and loop into the next fix attempt
///     (`spawn_fix_agent` with `attempt+1`). The retry cap lands in T3.3.
///   - Stalled → close with `outcome="stalled"`, pause `ci_stalled`.
///   - Conflict / merge failure → close with `outcome="merge_failed"`,
///     pause with reason `"fix_merge_failed: <detail>"`.
///
/// The original task id is recovered from the `task_fix_attempts` row
/// keyed by `(plan_name, agent_id)` — `spawn_fix_agent` stores the
/// original task id as `task_number` and the fix agent id as `agent_id`,
/// so the mapping is implicit but durable. This avoids parsing the
/// `-fix-<n>` suffix off the fix task id (the format could in principle
/// change without breaking the loop).
async fn on_fix_agent_completed(
    state: &AppState,
    org_id: &str,
    fix_agent_id: &str,
    plan_name: &str,
    fix_task_id: &str,
) {
    let (original_task, attempt) =
        match db::fix_attempt_for_agent(&state.db, plan_name, fix_agent_id) {
            Some(t) => t,
            None => {
                eprintln!(
                    "[auto_mode] fix-agent {fix_agent_id} ({plan_name}/{fix_task_id}) \
                     has no task_fix_attempts row — skipping fix-merge"
                );
                return;
            }
        };

    broadcast_state(
        state,
        plan_name,
        fix_task_id,
        state_labels::MERGING,
        None,
        None,
    );

    // Merge fix branch → canonical default. `into = None` resolves
    // canonical default at merge time (master / main). The fix branch
    // does NOT land back on the original task branch — fixes go straight
    // to trunk so try_auto_advance can immediately move to the next task
    // once CI is green.
    let outcome = merge_agent_branch_dispatch(state, org_id, fix_agent_id, None).await;

    let merged_sha = match outcome.merged_sha.clone() {
        Some(sha) => {
            let payload = serde_json::json!({
                "plan": plan_name,
                "task": fix_task_id,
                "original_task": original_task,
                "attempt": attempt,
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
                Some(fix_agent_id),
                Some(&payload.to_string()),
            );
            sha
        }
        None => {
            // Conflict or merge dispatch error. Close the attempt row
            // with `merge_failed` and pause with the brief's literal
            // reason prefix `fix_merge_failed`.
            db::close_fix_attempt(
                &state.db,
                plan_name,
                &original_task,
                attempt,
                "merge_failed",
            );

            let detail = if outcome.had_conflict {
                "merge_conflict".to_string()
            } else {
                outcome
                    .error
                    .as_deref()
                    .unwrap_or("merge dispatch returned no merged_sha")
                    .to_string()
            };
            let reason = format!("fix_merge_failed: {detail}");
            db::auto_mode_pause(&state.db, plan_name, &reason);

            let payload = serde_json::json!({
                "plan": plan_name,
                "task": fix_task_id,
                "original_task": original_task,
                "attempt": attempt,
                "reason": reason,
                "target": outcome.target_branch,
            });
            broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
            broadcast_state(
                state,
                plan_name,
                fix_task_id,
                state_labels::PAUSED,
                None,
                Some(&reason),
            );
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
            return;
        }
    };

    broadcast_state(
        state,
        plan_name,
        fix_task_id,
        state_labels::AWAITING_CI,
        Some(&merged_sha),
        None,
    );

    let ci_outcome = wait_for_ci(
        state,
        org_id,
        plan_name,
        fix_task_id,
        fix_agent_id,
        &merged_sha,
    )
    .await;

    match ci_outcome {
        CiOutcome::Green | CiOutcome::NotConfigured => {
            db::close_fix_attempt(&state.db, plan_name, &original_task, attempt, "green");
            on_fix_ci_passed(
                state,
                org_id,
                plan_name,
                &original_task,
                fix_task_id,
                &merged_sha,
                &ci_outcome,
            )
            .await;
        }
        CiOutcome::Red { failing_run_id } => {
            db::close_fix_attempt(&state.db, plan_name, &original_task, attempt, "red");
            on_fix_ci_failed(
                state,
                org_id,
                plan_name,
                &original_task,
                fix_task_id,
                &merged_sha,
                attempt,
                failing_run_id.as_deref(),
            )
            .await;
        }
        CiOutcome::Stalled => {
            db::close_fix_attempt(&state.db, plan_name, &original_task, attempt, "stalled");
            on_ci_stalled(state, org_id, plan_name, fix_task_id, &merged_sha).await;
        }
    }
}

/// Green / NotConfigured branch on a fix-agent CI: mark the original
/// task `completed` in `task_status` (source `auto`), audit
/// `AUTO_MODE_CI_PASSED`, then call `try_auto_advance` for the **original**
/// task id so phase progression proceeds from the right anchor.
async fn on_fix_ci_passed(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    original_task: &str,
    fix_task_id: &str,
    merged_sha: &str,
    ci_outcome: &CiOutcome,
) {
    broadcast_state(
        state,
        plan_name,
        fix_task_id,
        state_labels::ADVANCING,
        Some(merged_sha),
        None,
    );

    // Mark the original task completed. The fix branch has been merged
    // into trunk, so the project has the work that was originally
    // attempted on the task branch — `task_status[original_task]` should
    // reflect that. source='auto' so a future user manual-edit can still
    // override; a future auto-status sync also can since auto rows are
    // overwriteable (T2.3 of the navbar plan).
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, source, updated_at) \
             VALUES (?1, ?2, 'completed', 'auto', datetime('now')) \
             ON CONFLICT(plan_name, task_number) \
             DO UPDATE SET status = excluded.status, \
                           source = 'auto', \
                           updated_at = excluded.updated_at",
            params![plan_name, original_task],
        )
        .ok();
    }
    broadcast_event(
        &state.broadcast_tx,
        "task_status_changed",
        serde_json::json!({
            "plan_name": plan_name,
            "task_number": original_task,
            "status": "completed",
            "reason": "auto_mode: fix agent landed CI green",
        }),
    );

    let outcome_label = match ci_outcome {
        CiOutcome::Green => "green",
        CiOutcome::NotConfigured => "not_configured",
        _ => "unknown",
    };
    let payload = serde_json::json!({
        "plan": plan_name,
        "task": original_task,
        "fix_task": fix_task_id,
        "sha": merged_sha,
        "outcome": outcome_label,
    });
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_CI_PASSED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }

    let registry = state.registry.clone();
    let plans_dir = state.plans_dir.clone();
    let plan_name_owned = plan_name.to_string();
    let original_task_owned = original_task.to_string();
    let effort = *state.effort.lock().unwrap();
    let port = state.config_port();
    crate::agents::try_auto_advance(
        registry,
        plans_dir,
        plan_name_owned,
        original_task_owned,
        effort,
        port,
    )
    .await;
}

/// Red branch on a fix-agent CI: audit `AUTO_MODE_CI_FAILED` and loop
/// into the next fix attempt (`attempt + 1`). T3.3 will gate this on the
/// per-plan retry cap; for T3.2 each Red simply spawns another fix agent.
///
/// If [`spawn_fix_agent`] returns `None` (the original task agent row
/// has gone — defensive only, this should never happen in practice), the
/// loop pauses with `fix_spawn_failed` so the dashboard can surface the
/// degenerate state instead of silently dropping the run.
#[allow(clippy::too_many_arguments)] // step in the loop pipeline, not API
async fn on_fix_ci_failed(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    original_task: &str,
    fix_task_id: &str,
    merged_sha: &str,
    prior_attempt: u32,
    failing_run_id: Option<&str>,
) {
    let id_str = failing_run_id.unwrap_or("unknown");
    let payload = serde_json::json!({
        "plan": plan_name,
        "task": original_task,
        "fix_task": fix_task_id,
        "sha": merged_sha,
        "ci_run_id": failing_run_id,
        "prior_attempt": prior_attempt,
    });
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_CI_FAILED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }

    let next_attempt = prior_attempt.saturating_add(1);
    let new_fix_agent = spawn_fix_agent(
        state,
        org_id,
        plan_name,
        original_task,
        id_str,
        next_attempt,
    )
    .await;

    if let Some(new_id) = new_fix_agent {
        let next_fix_task = format!("{original_task}-fix-{next_attempt}");
        let payload = serde_json::json!({
            "plan": plan_name,
            "task": original_task,
            "fix_task": next_fix_task,
            "fix_agent_id": new_id,
            "attempt": next_attempt,
            "ci_run_id": failing_run_id,
        });
        broadcast_event(
            &state.broadcast_tx,
            "auto_mode_fix_spawned",
            payload.clone(),
        );
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_FIX_SPAWNED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    } else {
        let reason = "fix_spawn_failed: original task agent row missing".to_string();
        db::auto_mode_pause(&state.db, plan_name, &reason);
        let payload = serde_json::json!({
            "plan": plan_name,
            "task": original_task,
            "reason": reason,
        });
        broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
        broadcast_state(
            state,
            plan_name,
            fix_task_id,
            state_labels::PAUSED,
            Some(merged_sha),
            Some(&reason),
        );
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
            | WireMessage::GetCiRunStatus { req_id, .. }
            | WireMessage::CiFailureLog { req_id, .. } => Some(req_id),
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

    // ── Phase 2: full state-machine E2E tests ───────────────────────────────
    //
    // These drive `run_state_machine` end-to-end: completion → merge → CI →
    // (advance | pause). The merge + CI dispatches are stubbed via the echo
    // runner so we can drive both Green and Red outcomes without standing
    // up gh / GitHub Actions in the test environment.
    //
    // `try_auto_advance` is awaited for real; it calls
    // `pty_agent::start_pty_agent`, which inserts the agents row BEFORE the
    // session daemon spawn (and the spawn fails fast on the fake binary
    // path, leaving the row at status='failed'). That insert is exactly
    // the signal the brief asks the acceptance test to assert on:
    //
    //   > completion → auto-merge → stub CI green → next task spawns
    //   > automatically (assert via DB row count of `agents` for that plan).

    /// Write a 2-phase plan YAML to disk: phase 0 with task 0.1, phase 1
    /// with task 1.1. `project` is set to a per-test unique fake path so
    /// the eventual `start_pty_agent` work_dir lives at `~/<fake>` and
    /// `git_checkout_branch` fails silently instead of touching the real
    /// repo this test runs from.
    fn write_two_phase_plan(plans_dir: &std::path::Path, name: &str, fake_project: &str) {
        std::fs::create_dir_all(plans_dir).unwrap();
        let yaml = format!(
            "title: Phase-2 E2E plan\n\
             project: {fake_project}\n\
             phases:\n  \
               - number: 0\n    \
                 title: Phase 0\n    \
                 tasks:\n      \
                   - number: \"0.1\"\n        \
                     title: First task\n  \
               - number: 1\n    \
                 title: Phase 1\n    \
                 tasks:\n      \
                   - number: \"1.1\"\n        \
                     title: Second task\n"
        );
        std::fs::write(plans_dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    fn count_agents_for_plan(db: &Db, plan: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM agents WHERE plan_name = ?1",
            params![plan],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// Drain `auto_mode_state` event labels in arrival order.
    fn drain_state_labels(rx: &mut broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_state")
                && let Some(label) = v.pointer("/data/state").and_then(|s| s.as_str())
            {
                out.push(label.to_string());
            }
        }
        out
    }

    /// Mark task `0.1` of plan `p` as completed in `task_status` so
    /// `try_auto_advance` sees phase 0 as fully done and moves on to phase 1.
    fn mark_task_status_completed(db: &Db, plan: &str, task: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
             VALUES (?1, ?2, 'completed', datetime('now'))",
            params![plan, task],
        )
        .unwrap();
    }

    /// Headline acceptance test from the brief:
    /// completion → auto-merge → stub CI green → next task spawns
    /// automatically (assert via DB row count of `agents` for that plan).
    #[tokio::test]
    async fn green_ci_advances_to_next_phase_task() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        // Per-process unique fake project under $HOME so the resolved
        // work_dir doesn't clash with any other test (or real repo).
        let fake_project = format!("branchwork-test-{}-green-ci", uuid::Uuid::new_v4().simple());
        write_two_phase_plan(&plans_dir, "p", &fake_project);

        mark_task_status_completed(&db, "p", "0.1");

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
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
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_success(),
            ))),
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(db.clone(), runners, plans_dir);
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        enable_auto_mode(&db, "p");

        run_state_machine(&state, org_id, "agent-1", "p", "0.1").await;

        // Acceptance: the next-task agent row exists. `start_pty_agent`
        // inserts before the daemon spawn, so even though the spawn fails
        // on the fake binary path the row sticks (with status='failed').
        assert_eq!(
            count_agents_for_plan(&db, "p"),
            2,
            "expected 2 agents (original 0.1 + auto-spawned 1.1)"
        );

        // Plan stays unpaused on green.
        assert!(
            paused_reason(&db, "p").is_none(),
            "plan should not be paused on green CI"
        );

        // The state pill saw merging → awaiting_ci → advancing.
        let labels = drain_state_labels(&mut rx);
        assert!(
            labels.contains(&"merging".to_string()),
            "expected `merging` in labels: {labels:?}"
        );
        assert!(
            labels.contains(&"awaiting_ci".to_string()),
            "expected `awaiting_ci` in labels: {labels:?}"
        );
        assert!(
            labels.contains(&"advancing".to_string()),
            "expected `advancing` in labels: {labels:?}"
        );
        assert!(
            !labels.contains(&"paused".to_string()),
            "no `paused` expected on green CI, got: {labels:?}"
        );

        // Audit log: AUTO_MODE_MERGED on the agent + AUTO_MODE_CI_PASSED on
        // the plan. AUTO_MODE_CI_FAILED must NOT be present.
        let agent_actions = audit_actions_for(&db, "agent-1");
        assert!(
            agent_actions.iter().any(|a| a == actions::AUTO_MODE_MERGED),
            "expected AUTO_MODE_MERGED in agent actions: {agent_actions:?}"
        );

        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_CI_PASSED),
            "expected AUTO_MODE_CI_PASSED in plan actions: {plan_actions:?}"
        );
        assert!(
            !plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_CI_FAILED),
            "AUTO_MODE_CI_FAILED must not be present on green: {plan_actions:?}"
        );
    }

    /// Companion acceptance test from the brief:
    /// stub CI red → no next task spawned, plan paused with the expected
    /// reason (`ci_failed: <run_id>`).
    #[tokio::test]
    async fn red_ci_pauses_plan_and_does_not_advance() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        let fake_project = format!("branchwork-test-{}-red-ci", uuid::Uuid::new_v4().simple());
        write_two_phase_plan(&plans_dir, "p", &fake_project);

        mark_task_status_completed(&db, "p", "0.1");

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
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
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_failure("42"),
            ))),
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(db.clone(), runners, plans_dir);
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        enable_auto_mode(&db, "p");

        run_state_machine(&state, org_id, "agent-1", "p", "0.1").await;

        // Acceptance: no new agent — only the original.
        assert_eq!(
            count_agents_for_plan(&db, "p"),
            1,
            "no next task should spawn on red CI"
        );

        // Plan paused with `ci_failed: <run_id>` (the failing run id from
        // the failure aggregate).
        let reason = paused_reason(&db, "p").expect("plan should be paused");
        assert_eq!(reason, "ci_failed: 42");

        // State pill saw merging → awaiting_ci → paused. No `advancing`.
        let labels = drain_state_labels(&mut rx);
        assert!(
            labels.contains(&"merging".to_string()),
            "expected `merging` in labels: {labels:?}"
        );
        assert!(
            labels.contains(&"awaiting_ci".to_string()),
            "expected `awaiting_ci` in labels: {labels:?}"
        );
        assert!(
            labels.contains(&"paused".to_string()),
            "expected `paused` in labels: {labels:?}"
        );
        assert!(
            !labels.contains(&"advancing".to_string()),
            "no `advancing` expected on red CI, got: {labels:?}"
        );

        // Audit: AUTO_MODE_MERGED then AUTO_MODE_CI_FAILED on the plan;
        // AUTO_MODE_CI_PASSED must not be present.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_CI_FAILED),
            "expected AUTO_MODE_CI_FAILED in plan actions: {plan_actions:?}"
        );
        assert!(
            !plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_CI_PASSED),
            "AUTO_MODE_CI_PASSED must not be present on red: {plan_actions:?}"
        );
    }

    /// Stalled-CI variant: aggregator never reaches a terminal verdict
    /// before the timeout. The loop pauses with `ci_stalled` and does not
    /// advance. Uses tight WaitForCiConfig via a closure-injected wrapper
    /// to keep wall-clock under a second.
    #[tokio::test]
    async fn stalled_ci_pauses_with_ci_stalled_reason() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        let fake_project = format!("branchwork-test-{}-stalled", uuid::Uuid::new_v4().simple());
        write_two_phase_plan(&plans_dir, "p", &fake_project);

        mark_task_status_completed(&db, "p", "0.1");

        // Echo runner replies for the merge half; the CI half goes through
        // a closure-injected `wait_for_ci_inner` with fast_config and
        // `Ok(None)` for get_status — the loop polls until total_timeout
        // elapses, returning Stalled.
        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
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

        let (state, _rx) = test_app_state(db.clone(), runners, plans_dir);
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        enable_auto_mode(&db, "p");

        // Run the merge step + manually drive a stalled CI outcome via
        // the closure-injected inner. We can't use `run_state_machine`
        // directly here because it calls `wait_for_ci` with the default
        // 20-min timeout — and stubbing the dispatch to return Ok(None)
        // forever via the runner is more invasive than just calling the
        // already-tested `on_ci_stalled` branch directly.
        let merge_outcome = run_merge_step(&state, org_id, "agent-1", "p", "0.1").await;
        let merged_sha = match merge_outcome {
            MergeStepOutcome::Merged(sha) => sha,
            MergeStepOutcome::Paused => panic!("merge should succeed in stub"),
        };
        on_ci_stalled(&state, org_id, "p", "0.1", &merged_sha).await;

        // No advance.
        assert_eq!(
            count_agents_for_plan(&db, "p"),
            1,
            "no next task should spawn on stalled CI"
        );

        // Pause reason is `ci_stalled` (literal — no run id to attach).
        assert_eq!(paused_reason(&db, "p").as_deref(), Some("ci_stalled"));

        // Audit: AUTO_MODE_PAUSED on the plan (Stalled audits as PAUSED,
        // not CI_FAILED — different from the Red branch).
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED),
            "expected AUTO_MODE_PAUSED in plan actions: {plan_actions:?}"
        );
    }

    // ── Phase 3.1: spawn_fix_agent ──────────────────────────────────────────
    //
    // Two tests, one per mode:
    //   - Standalone: stub the Red CI outcome by passing a known
    //     `failing_run_id`, call `spawn_fix_agent`, and assert a fresh
    //     agent row appears with branch `branchwork/<plan>/<task>-fix-1`,
    //     a `task_fix_attempts` row was recorded for the same triple, and
    //     the prompt embeds the unattended-execution contract block from
    //     T0.7. The standalone path goes through `start_pty_agent`, which
    //     inserts the row before failing fast on the fake server-exe path
    //     — same inserts-then-fails pattern the merge-state-machine tests
    //     above rely on.
    //   - SaaS: stub the runner so `CiFailureLog` returns a known log
    //     substring, then assert the dispatcher emits a `StartAgent`
    //     envelope to the runner with the fix branch + task_id, the
    //     `task_fix_attempts` row, and the prompt carries both the
    //     failure-log substring AND the literal contract-block text.

    /// The text we expect the prompt's contract-block section to include.
    /// Pulled from `unattended_contract_block` so the test fails loudly if
    /// T0.7's wording drifts without the fix-prompt builder picking up the
    /// new block.
    const CONTRACT_NEEDLE: &str = "Unattended-execution contract";

    /// The fix-prompt template's task-specific header — proves the prompt
    /// was built by `build_fix_prompt` and not by some unrelated path.
    const PROMPT_TASK_HEADER: &str = "CI failed on the merge of task";

    fn task_fix_attempt_row(
        db: &Db,
        plan: &str,
        task: &str,
        attempt: u32,
    ) -> Option<(Option<String>, Option<String>)> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT agent_id, started_at FROM task_fix_attempts \
             WHERE plan_name = ?1 AND task_number = ?2 AND attempt = ?3",
            params![plan, task, attempt as i64],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .ok()
    }

    /// Standalone-mode acceptance: spawn_fix_agent inserts a fix agent
    /// row with the expected branch + task_id, records a
    /// `task_fix_attempts` row, and writes a prompt that embeds both
    /// the fix-prompt header and the unattended-execution contract.
    /// `start_pty_agent` fails fast on the fake server-exe path; the
    /// row stuck in 'failed' is exactly the signal the assertion needs.
    #[tokio::test]
    async fn standalone_spawn_fix_agent_inserts_row_and_records_attempt() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Original task agent row — gives spawn_fix_agent a cwd to point
        // the fix branch at. status='completed' so the lookup picks it up.
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");

        let agent_id = spawn_fix_agent(&state, "default-org", "p", "1.1", "555", 1)
            .await
            .expect("spawn_fix_agent should return a fresh agent_id");

        // ── Fix agent row exists with the expected branch + task_id ────
        let (branch, task_id, prompt): (Option<String>, Option<String>, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT branch, task_id, prompt FROM agents WHERE id = ?1",
                params![agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(branch.as_deref(), Some("branchwork/p/1.1-fix-1"));
        assert_eq!(task_id.as_deref(), Some("1.1-fix-1"));

        // ── Prompt embeds the fix-prompt header AND the contract block ─
        let prompt = prompt.expect("agent row should carry the fix prompt");
        assert!(
            prompt.contains(PROMPT_TASK_HEADER),
            "prompt should carry the fix-prompt task header: {prompt}"
        );
        assert!(
            prompt.contains(CONTRACT_NEEDLE),
            "prompt should embed the unattended-execution contract block: {prompt}"
        );
        assert!(
            prompt.contains("branchwork/p/1.1-fix-1"),
            "prompt should reference the fix branch (so the contract block \
             names the right branch to commit to): {prompt}"
        );
        assert!(
            prompt.contains("555"),
            "prompt should reference the failing run id: {prompt}"
        );

        // ── task_fix_attempts row is recorded with the agent_id backfill
        let row = task_fix_attempt_row(&db, "p", "1.1", 1)
            .expect("task_fix_attempts row should exist for attempt 1");
        assert_eq!(
            row.0.as_deref(),
            Some(agent_id.as_str()),
            "agent_id should be backfilled onto the attempt row"
        );
        assert!(row.1.is_some(), "started_at should be set");
    }

    /// Re-entrant spawn (attempt N>1) must not clobber an earlier
    /// attempt's row. The (plan, task, attempt) PK + ON CONFLICT DO
    /// NOTHING is what enforces that — the test asserts both rows
    /// coexist and the original attempt-1 agent_id is preserved.
    #[tokio::test]
    async fn standalone_spawn_fix_agent_idempotent_on_conflict() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");

        let attempt_1_agent = spawn_fix_agent(&state, "default-org", "p", "1.1", "100", 1)
            .await
            .expect("attempt 1 should succeed");
        let attempt_2_agent = spawn_fix_agent(&state, "default-org", "p", "1.1", "100", 2)
            .await
            .expect("attempt 2 should succeed");
        assert_ne!(
            attempt_1_agent, attempt_2_agent,
            "each attempt should yield a distinct agent_id"
        );

        // Both attempt rows exist and carry their respective agent_ids.
        let row_1 = task_fix_attempt_row(&db, "p", "1.1", 1).expect("attempt 1 row must persist");
        let row_2 = task_fix_attempt_row(&db, "p", "1.1", 2).expect("attempt 2 row must persist");
        assert_eq!(row_1.0.as_deref(), Some(attempt_1_agent.as_str()));
        assert_eq!(row_2.0.as_deref(), Some(attempt_2_agent.as_str()));

        // db::task_fix_attempt_count returns the cap-feeding value.
        assert_eq!(
            crate::db::task_fix_attempt_count(&db, "p", "1.1"),
            2,
            "attempt count should match the number of distinct attempts"
        );
    }

    /// SaaS-mode acceptance: spawn_fix_agent → `start_agent_dispatch` →
    /// SaaS branch emits a `StartAgent` envelope to the registered
    /// runner. The stub runner replies to `CiFailureLog` with a known
    /// log substring; the assertion proves the log lands inside the
    /// `StartAgent.prompt` alongside the contract block.
    #[tokio::test]
    async fn saas_spawn_fix_agent_emits_start_agent_envelope_with_log_and_contract() {
        let (db, _dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let runner_log = "RUNNER-MOCK-FAILURE-LOG: assertion failed at line 42";
        let runner_log_owned = runner_log.to_string();
        let runners = new_runner_registry();
        let mut outgoing = install_echo_runner(&runners, "runner-1", move |msg| match msg {
            WireMessage::CiFailureLog { run_id, .. } => Some(RunnerResponse::CiFailureLogFetched {
                log: Some(runner_log_owned.clone()),
                run_id_used: run_id.clone(),
            }),
            _ => None,
        })
        .await;

        let (state, _rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-saas-fix-plans"),
        );
        seed_agent(
            &db,
            "agent-1",
            Path::new("/runner/projects/demo"),
            "p",
            "1.1",
            "branchwork/p/1.1",
        );

        let agent_id = spawn_fix_agent(&state, org_id, "p", "1.1", "555", 1)
            .await
            .expect("spawn_fix_agent should return a fresh agent_id");

        // Drain runner-bound payloads, find the StartAgent envelope.
        let mut start_agent_payload: Option<String> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), outgoing.recv()).await
            {
                Ok(Some(payload)) => {
                    if payload.contains("\"type\":\"start_agent\"") {
                        start_agent_payload = Some(payload);
                        break;
                    }
                }
                _ => break,
            }
        }
        let payload = start_agent_payload.expect("expected a start_agent envelope on the wire");

        // ── StartAgent envelope shape: agent_id + branch + task_id ────
        let envelope: crate::saas::runner_protocol::Envelope =
            serde_json::from_str(&payload).expect("envelope must parse");
        match envelope.message {
            WireMessage::StartAgent {
                agent_id: got_id,
                plan_name,
                task_id,
                prompt,
                cwd,
                ..
            } => {
                assert_eq!(
                    got_id, agent_id,
                    "envelope agent_id should match dispatch return"
                );
                assert_eq!(plan_name, "p");
                assert_eq!(task_id, "1.1-fix-1");
                assert_eq!(cwd, "/runner/projects/demo");
                assert!(
                    prompt.contains(runner_log),
                    "prompt should embed the runner-supplied failure log: {prompt}"
                );
                assert!(
                    prompt.contains(CONTRACT_NEEDLE),
                    "prompt should embed the unattended-execution contract: {prompt}"
                );
                assert!(
                    prompt.contains("branchwork/p/1.1-fix-1"),
                    "prompt should reference the fix branch: {prompt}"
                );
            }
            other => panic!("expected StartAgent variant, got {other:?}"),
        }

        // ── task_fix_attempts row recorded for (plan, task, 1) ────────
        let row = task_fix_attempt_row(&db, "p", "1.1", 1)
            .expect("task_fix_attempts row should exist for attempt 1");
        assert_eq!(
            row.0.as_deref(),
            Some(agent_id.as_str()),
            "agent_id should be backfilled onto the attempt row"
        );
    }

    /// `on_task_agent_completed` for a fix agent that has no
    /// `task_fix_attempts` row recorded short-circuits silently. The
    /// fix-completion handler can't recover the original task id from a
    /// missing row, so it must bail rather than guess. Defensive: this
    /// state should never happen in practice because [`spawn_fix_agent`]
    /// inserts the row BEFORE the dispatch; a row-less fix agent
    /// indicates the row was deleted out of band or the test set up the
    /// agent directly without going through spawn_fix_agent.
    #[tokio::test]
    async fn fix_agent_completion_without_attempt_row_is_silent_no_op() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        git_create_task_branch(&cwd, "branchwork/p/1.1-fix-1", true);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(
            &db,
            "fix-agent-1",
            &cwd,
            "p",
            "1.1-fix-1",
            "branchwork/p/1.1-fix-1",
        );
        enable_auto_mode(&db, "p");

        on_task_agent_completed(&state, "fix-agent-1", "p", "1.1-fix-1").await;
        // Allow any spawned task a moment to no-op.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = drain_event_types(&mut rx);
        assert!(
            !events.iter().any(|e| e.starts_with("auto_mode_")),
            "no auto_mode_* events expected when the fix-attempt row is missing: {events:?}"
        );
        assert!(
            paused_reason(&db, "p").is_none(),
            "plan should stay unpaused — missing-row short-circuit is silent"
        );
    }

    // ── Phase 3.2: on_fix_agent_completed ───────────────────────────────────
    //
    // The headline acceptance test from the brief:
    //
    //   completion → red CI → fix agent → fix agent completes → merge →
    //   green CI → next task spawns. Assert the task_fix_attempts row is
    //   updated with outcome="green" and the original task is marked
    //   complete in task_status.
    //
    // Three tests cover the fix-completion branches:
    //   - Green: original task marked completed, attempt closed=green,
    //     advance fires (next-phase agent row exists).
    //   - Red:   attempt closed=red, AUTO_MODE_FIX_SPAWNED audit, a
    //     fresh fix agent (attempt+1) row exists, and a second
    //     task_fix_attempts row was recorded for the next attempt.
    //   - Conflict: attempt closed=merge_failed, plan paused with reason
    //     starting `fix_merge_failed`.
    //
    // All three drive `on_fix_agent_completed` directly with seeded DB
    // state and an echo runner that stubs the merge + CI dispatches —
    // mirrors the Phase 2 state-machine tests above.

    fn fix_attempt_outcome(db: &Db, plan: &str, task: &str, attempt: u32) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT outcome FROM task_fix_attempts \
             WHERE plan_name = ?1 AND task_number = ?2 AND attempt = ?3",
            params![plan, task, attempt as i64],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn task_status_value(db: &Db, plan: &str, task: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM task_status \
             WHERE plan_name = ?1 AND task_number = ?2",
            params![plan, task],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    fn count_agents_for_plan_task(db: &Db, plan: &str, task: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM agents WHERE plan_name = ?1 AND task_id = ?2",
            params![plan, task],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// Headline acceptance test: completion → red CI → fix agent → fix
    /// agent completes → merge → green CI → next-task agent spawns. The
    /// task_fix_attempts row carries `outcome="green"` and `task_status`
    /// for the original task is `completed`.
    #[tokio::test]
    async fn fix_agent_green_marks_original_task_completed_and_advances() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        let fake_project = format!(
            "branchwork-test-{}-fix-green",
            uuid::Uuid::new_v4().simple()
        );
        write_two_phase_plan(&plans_dir, "p", &fake_project);

        // Stub runner: merge succeeds with a fresh sha; CI is green.
        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Ok {
                    merged_sha: "fixsha".into(),
                }))
            }
            WireMessage::PushBranch { .. } => Some(RunnerResponse::PushResult {
                ok: true,
                stderr: None,
            }),
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_success(),
            ))),
            _ => None,
        })
        .await;

        let (state, _rx) = test_app_state(db.clone(), runners, plans_dir);

        // Seed: original task agent (carries the cwd that spawn_fix_agent
        // would otherwise look up, but for this test we drive
        // on_fix_agent_completed directly so the cwd lookup happens via
        // the fix agent's own agents row).
        seed_agent(
            &db,
            "agent-original",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        // Seed: fix agent (already completed). The fix branch is what
        // gets merged into the canonical default in this test.
        seed_agent(
            &db,
            "fix-agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1-fix-1",
            "branchwork/p/0.1-fix-1",
        );
        // Seed: task_fix_attempts row (attempt 1, agent_id = fix agent),
        // simulating that spawn_fix_agent ran. task_number is the
        // ORIGINAL task id (0.1), not the fix task id.
        crate::db::record_fix_attempt(&db, "p", "0.1", 1, "fix-agent-1");
        enable_auto_mode(&db, "p");

        on_fix_agent_completed(&state, org_id, "fix-agent-1", "p", "0.1-fix-1").await;

        // Acceptance #1: outcome = "green" on the attempt row.
        assert_eq!(
            fix_attempt_outcome(&db, "p", "0.1", 1).as_deref(),
            Some("green"),
            "task_fix_attempts row should be closed with outcome=green"
        );

        // Acceptance #2: original task is marked completed in task_status.
        assert_eq!(
            task_status_value(&db, "p", "0.1").as_deref(),
            Some("completed"),
            "original task_status[0.1] should be 'completed' after fix → green CI"
        );

        // Acceptance #3: next-phase task (1.1) spawned via try_auto_advance.
        // start_pty_agent inserts the agents row before the daemon spawn,
        // so even though the spawn fails on the fake binary path the row
        // sticks (status='failed') — that's the signal the brief asks for.
        assert_eq!(
            count_agents_for_plan_task(&db, "p", "1.1"),
            1,
            "expected the next-phase task (1.1) to have spawned an agent row"
        );

        // Plan stays unpaused on green.
        assert!(
            paused_reason(&db, "p").is_none(),
            "plan should not be paused on green fix CI"
        );
    }

    /// Companion: fix-agent CI comes back Red → outcome=red on the
    /// closed attempt row, a fresh attempt-2 fix agent is spawned (the
    /// loop into T3.3), and AUTO_MODE_FIX_SPAWNED is audited.
    #[tokio::test]
    async fn fix_agent_red_loops_into_next_attempt() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Ok {
                    merged_sha: "fixsha".into(),
                }))
            }
            WireMessage::PushBranch { .. } => Some(RunnerResponse::PushResult {
                ok: true,
                stderr: None,
            }),
            WireMessage::HasGithubActions { .. } => {
                Some(RunnerResponse::GithubActionsDetected(true))
            }
            WireMessage::GetCiRunStatus { .. } => Some(RunnerResponse::CiRunStatusResolved(Some(
                aggregate_failure("999"),
            ))),
            // Failure-log fetch fires from spawn_fix_agent for the
            // next attempt — return the canned reply.
            WireMessage::CiFailureLog { run_id, .. } => Some(RunnerResponse::CiFailureLogFetched {
                log: Some("the failing log".into()),
                run_id_used: run_id.clone(),
            }),
            _ => None,
        })
        .await;

        let (state, _rx) = test_app_state(db.clone(), runners, plans_dir);

        // Seed original + fix-1 agent rows; original carries the cwd
        // that spawn_fix_agent (fired from on_fix_ci_failed) reuses.
        seed_agent(
            &db,
            "agent-original",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        seed_agent(
            &db,
            "fix-agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1-fix-1",
            "branchwork/p/0.1-fix-1",
        );
        crate::db::record_fix_attempt(&db, "p", "0.1", 1, "fix-agent-1");
        enable_auto_mode(&db, "p");

        on_fix_agent_completed(&state, org_id, "fix-agent-1", "p", "0.1-fix-1").await;

        // outcome=red on attempt-1.
        assert_eq!(
            fix_attempt_outcome(&db, "p", "0.1", 1).as_deref(),
            Some("red"),
            "task_fix_attempts row should be closed with outcome=red"
        );

        // attempt-2 row was inserted by spawn_fix_agent inside the loop.
        assert_eq!(
            crate::db::task_fix_attempt_count(&db, "p", "0.1"),
            2,
            "expected a second fix-attempt row recorded by the loop"
        );

        // Original task NOT marked completed — Red doesn't advance.
        assert!(
            task_status_value(&db, "p", "0.1").is_none(),
            "task_status[0.1] should not be set on red CI"
        );

        // AUTO_MODE_CI_FAILED + AUTO_MODE_FIX_SPAWNED on the plan.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_CI_FAILED),
            "expected AUTO_MODE_CI_FAILED in plan actions: {plan_actions:?}"
        );
        assert!(
            plan_actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_FIX_SPAWNED),
            "expected AUTO_MODE_FIX_SPAWNED in plan actions: {plan_actions:?}"
        );
    }

    /// Conflict / merge failure on the fix branch → outcome=merge_failed,
    /// plan paused with reason starting `fix_merge_failed`. Mirrors the
    /// task-merge `merge_failed:` pattern but uses the brief's specific
    /// `fix_merge_failed` prefix to distinguish the two paths in the UI.
    #[tokio::test]
    async fn fix_agent_merge_conflict_pauses_with_fix_merge_failed_reason() {
        let (db, _dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Conflict {
                    stderr: "CONFLICT (content): Merge conflict in foo.txt".into(),
                }))
            }
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-fix-conflict-plans"),
        );
        seed_agent(
            &db,
            "fix-agent-1",
            Path::new("/runner/cwd"),
            "p",
            "0.1-fix-1",
            "branchwork/p/0.1-fix-1",
        );
        crate::db::record_fix_attempt(&db, "p", "0.1", 1, "fix-agent-1");
        enable_auto_mode(&db, "p");

        on_fix_agent_completed(&state, org_id, "fix-agent-1", "p", "0.1-fix-1").await;

        // outcome=merge_failed on the attempt row.
        assert_eq!(
            fix_attempt_outcome(&db, "p", "0.1", 1).as_deref(),
            Some("merge_failed"),
            "task_fix_attempts row should be closed with outcome=merge_failed"
        );

        // Plan paused with `fix_merge_failed` prefix.
        let reason = paused_reason(&db, "p").expect("plan should be paused");
        assert!(
            reason.starts_with("fix_merge_failed"),
            "expected fix_merge_failed prefix, got: {reason}"
        );

        // auto_mode_paused broadcast.
        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_mode_paused".to_string()),
            "expected auto_mode_paused in {events:?}"
        );

        // Original task NOT marked completed.
        assert!(
            task_status_value(&db, "p", "0.1").is_none(),
            "task_status[0.1] should not be set on merge conflict"
        );
    }

    /// `build_fix_prompt` falls back to a placeholder when the failure-log
    /// fetch returns None (gh unavailable / runner cache miss / no run).
    /// Asserts the placeholder is descriptive enough to be useful and the
    /// contract block still lands.
    #[test]
    fn build_fix_prompt_falls_back_when_log_is_none() {
        let prompt = build_fix_prompt("p", "1.1", "branchwork/p/1.1-fix-1", "555", None);
        assert!(prompt.contains("failure log unavailable"));
        assert!(prompt.contains(CONTRACT_NEEDLE));
        assert!(prompt.contains("branchwork/p/1.1-fix-1"));
        assert!(prompt.contains("555"));
    }
}
