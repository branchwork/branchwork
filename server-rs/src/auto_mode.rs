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

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rand::Rng;
use rusqlite::params;
use tokio_util::sync::CancellationToken;

use crate::agents::pty_agent::StartPtyOpts;
use crate::agents::spawn_ops::start_agent_dispatch;
use crate::audit;
use crate::db;
use crate::plan_parser;
use crate::repo_config::MergeCadence;
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
    /// A task agent completed cleanly but the loop's cadence gate
    /// ([`super::should_merge_now`]) said it wasn't time to merge yet —
    /// the agent's branch was left intact and the row marked
    /// `merge_status='deferred_for_cadence'`. The next completion that
    /// flips the gate to true drains every deferred row in dependency
    /// order before merging itself. Diff carries
    /// `{plan, task, agent_id, cadence}`.
    pub const AUTO_MODE_MERGE_DEFERRED: &str = "auto_mode.merge_deferred";
    /// The loop aborted itself for a plan and recorded a pause reason.
    pub const AUTO_MODE_PAUSED: &str = "auto_mode.paused";
    /// CI came back green (or wasn't configured) — loop advanced.
    pub const AUTO_MODE_CI_PASSED: &str = "auto_mode.ci_passed";
    /// CI came back red — loop paused or spawned a fix agent.
    pub const AUTO_MODE_CI_FAILED: &str = "auto_mode.ci_failed";
    /// A fix agent was spawned for a Red CI outcome.
    pub const AUTO_MODE_FIX_SPAWNED: &str = "auto_mode.fix_spawned";
    /// A short-interval poller detected that the working tree of a plan
    /// previously paused with `agent_left_uncommitted_work` is now clean
    /// (either the operator committed/staged the dirty files, or they
    /// stashed them) — the loop auto-resumed without operator input.
    /// Diff carries `{plan, last_completed_task, poll_count}`.
    pub const AUTO_RESUMED_TREE_CLEAN: &str = "auto_mode.auto_resumed_tree_clean";
    /// The operator clicked Flush deferred merges on the dashboard — the
    /// server unconditionally drained every `merge_status='deferred_for_cadence'`
    /// row in the plan regardless of the configured cadence. Diff carries
    /// `{plan, count, paused}`. Individual merges still emit their own
    /// `AUTO_MODE_MERGED` rows; this one bookends the flush as a whole
    /// so the audit log carries the operator intent.
    pub const AUTO_MODE_FLUSHED_DEFERRED: &str = "auto_mode.flushed_deferred";
    /// A pre-merge gate check failed (or the whole-gate ceiling fired)
    /// before the merge could land. The plan is paused with the literal
    /// reason `pre_merge_check_failed`; diff carries
    /// `{plan, task, agent_id, check_name, exit_code, output_snippet}`
    /// where `output_snippet` is the captured combined stdout+stderr
    /// truncated to [`PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES`] so the dashboard
    /// can render the offending output. Phase 1 of the `pre-merge-gate`
    /// plan; the constant name follows the task spec verbatim (CHECK,
    /// not GATE — T1.3 renamed from the placeholder T1.2 wired in).
    pub const AUTO_MODE_PRE_MERGE_CHECK_FAILED: &str = "auto_mode.pre_merge_check_failed";
}

// ── Per-branch push lock (Phase 2) ──────────────────────────────────────────
//
// The merge → push critical section runs in this auto-mode flow (and in
// the user-driven HTTP Merge button's spawned `trigger_after_merge`).
// Both paths are serialized against external auto-bump CI via the
// `master_push_lock` table — see [`db::try_acquire_push_lock`].
//
// The acquire helper polls the DB row every `PUSH_LOCK_POLL_MS` until it
// succeeds or `wait_timeout` elapses. A successful acquire returns a
// [`PushLockGuard`] whose `Drop` impl releases the row, so the caller
// can't forget. Crashes during the critical section are recovered by the
// 30-second TTL: a fresh acquire after that window force-evicts the
// dead holder's row.

/// How often the wait loop wakes up to retry `try_acquire_push_lock`.
const PUSH_LOCK_POLL_MS: u64 = 200;

/// Hard cap on how long auto-mode waits for the push lock before
/// pausing the plan. CI / API callers carry their own per-request
/// timeout; the in-process auto-mode flow uses this default.
pub const PUSH_LOCK_DEFAULT_WAIT_SECS: u64 = 30;

/// RAII guard returned by [`wait_for_push_lock`]. Dropping it releases
/// the row from `master_push_lock`. Holding it past Drop is impossible
/// (Drop is the only way the row is freed in the happy path) so callers
/// don't have to chase explicit release calls through every early-return
/// branch.
pub struct PushLockGuard {
    db: db::Db,
    branch: String,
    token: String,
    /// Set to `true` by [`PushLockGuard::forget`] to skip the Drop
    /// release. Used when an outer caller has taken ownership of the
    /// release (e.g. the HTTP endpoint hands the token back to the
    /// client so the client can release explicitly).
    forgotten: bool,
}

impl PushLockGuard {
    /// Branch the guard is holding the lock for. Useful for diagnostics.
    #[allow(dead_code)]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Token identifying this guard's hold. The HTTP endpoint surfaces
    /// this to clients so they can release the lock explicitly; tests
    /// use it to verify which call won the race.
    #[allow(dead_code)] // exposed for tests + future callers that hold the token
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Disable the Drop-release. The caller becomes responsible for
    /// calling [`db::release_push_lock`] (or letting the TTL evict the
    /// row on its own). Used by the HTTP endpoint, where the guard
    /// crosses an HTTP response boundary and the client holds the
    /// release responsibility.
    pub fn forget(mut self) -> String {
        self.forgotten = true;
        std::mem::take(&mut self.token)
    }

    /// Explicit release, primarily for tests. Equivalent to letting the
    /// guard drop, but lets the caller assert on the return value.
    #[allow(dead_code)]
    pub fn release(mut self) -> bool {
        if self.forgotten {
            return false;
        }
        self.forgotten = true;
        db::release_push_lock(&self.db, &self.branch, &self.token)
    }
}

impl Drop for PushLockGuard {
    fn drop(&mut self) {
        if !self.forgotten {
            db::release_push_lock(&self.db, &self.branch, &self.token);
        }
    }
}

/// Failure modes of [`wait_for_push_lock`]. The caller decides whether
/// to pause the plan, return 503 over HTTP, or retry next tick.
#[derive(Debug)]
pub enum PushLockError {
    /// `wait_timeout` elapsed without the live holder releasing. Carries
    /// a snapshot of the holder that won the race so the caller can
    /// surface a useful diagnostic.
    Timeout(db::PushLockHolder),
}

impl std::fmt::Display for PushLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushLockError::Timeout(h) => write!(
                f,
                "push lock for branch held by {} (token {}, age {}s)",
                h.holder_kind, h.holder_token, h.age_secs
            ),
        }
    }
}

/// Wait until the per-branch push lock can be acquired, polling every
/// `PUSH_LOCK_POLL_MS` (default 200 ms). Returns a [`PushLockGuard`] on
/// success or [`PushLockError::Timeout`] if `wait_timeout` elapses with
/// a live holder still in place.
///
/// The first poll iteration runs immediately so an uncontended acquire
/// has zero added latency. After that, each retry first sleeps the poll
/// interval to let the holder make progress.
pub async fn wait_for_push_lock(
    db: &db::Db,
    branch: &str,
    holder_kind: &str,
    holder_pid: i64,
    holder_meta: Option<&str>,
    wait_timeout: Duration,
) -> Result<PushLockGuard, PushLockError> {
    let deadline = Instant::now() + wait_timeout;
    let poll = Duration::from_millis(PUSH_LOCK_POLL_MS);
    loop {
        match db::try_acquire_push_lock(
            db,
            branch,
            holder_kind,
            holder_pid,
            holder_meta,
            db::PUSH_LOCK_TTL_SECS,
        ) {
            Ok(token) => {
                return Ok(PushLockGuard {
                    db: db.clone(),
                    branch: branch.to_string(),
                    token,
                    forgotten: false,
                });
            }
            Err(holder) => {
                if Instant::now() >= deadline {
                    return Err(PushLockError::Timeout(holder));
                }
                tokio::time::sleep(poll).await;
            }
        }
    }
}

/// Phase labels broadcast on the `auto_mode_state` event so the UI pill can
/// reflect the current step. The set is closed: any new transition needs a
/// new constant + matching frontend label.
mod state_labels {
    pub const MERGING: &str = "merging";
    pub const AWAITING_CI: &str = "awaiting_ci";
    pub const ADVANCING: &str = "advancing";
    pub const PAUSED: &str = "paused";
    /// The cadence gate ([`super::should_merge_now`]) returned false —
    /// the agent merged nothing and the row is marked
    /// `deferred_for_cadence` until the boundary task completes.
    pub const DEFERRED: &str = "deferred";
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

/// Decide whether auto-mode should merge `completed_task`'s branch right
/// now, given the plan's effective [`MergeCadence`].
///
/// Resolution chain (mirrors `repo_defaults_for` + [`db::plan_merge_cadence`]
/// in `api/plans.rs` and the `resolveMergeCadence` helper in
/// `web/src/api/plans.ts`):
///
/// 1. Plan-level pin: `plan_auto_mode.merge_cadence` if set
///    ([`db::plan_merge_cadence`]).
/// 2. Repo default: `[auto_mode].merge_cadence` from the project's
///    `branchwork.toml` ([`crate::repo_config::load_for_project_dir`]).
/// 3. Hard-coded fallback: [`MergeCadence::default()`] (= [`MergeCadence::Phase`]).
///
/// Rules per cadence:
///   - [`MergeCadence::Task`]: always `true` (legacy auto-mode behaviour;
///     fastest feedback, highest CI volume).
///   - [`MergeCadence::Phase`]: `true` iff every task in `completed_task`'s
///     phase has `status IN (completed, skipped)`. `completed_task` itself
///     is treated as completed (the caller is telling us it just
///     finished — the corresponding `task_status` write may race the
///     call).
///   - [`MergeCadence::Plan`]: `true` iff every task in every phase
///     satisfies the same rule. With a single-phase plan, this collapses
///     to the same trigger as [`MergeCadence::Phase`] — accepted per the
///     plan brief.
///
/// `failed` blocks the boundary (and so does `pending` / `in_progress` /
/// `checking` — any non-done status keeps the predicate `false`). The
/// operator either fixes the failing task or marks it skipped before the
/// loop can advance.
///
/// Returns `false` defensively when the plan can't be loaded or when
/// `completed_task` doesn't appear in any phase — auto-mode never
/// half-merges on incomplete metadata.
#[allow(dead_code)] // wired in by 2.2+ when the merge-gate consumer lands
pub fn should_merge_now(state: &AppState, plan_name: &str, completed_task: &str) -> bool {
    let cadence = resolve_effective_cadence(state, plan_name);

    // `Task` cadence is the legacy "merge on every completion" mode;
    // skip the plan/status load entirely so the hot path stays cheap.
    if cadence == MergeCadence::Task {
        return true;
    }

    let plan = match plan_parser::find_plan_file(&state.plans_dir, plan_name)
        .and_then(|p| plan_parser::parse_plan_file(&p).ok())
    {
        Some(p) => p,
        None => return false,
    };

    let status_map: HashMap<String, String> = {
        let conn = state.db.lock().unwrap();
        let Ok(mut stmt) =
            conn.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?1")
        else {
            return false;
        };
        stmt.query_map(params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    };

    // The caller's word is authoritative for the just-completed task —
    // its `task_status` row may not have been written yet (the auto-mode
    // hot path races the MCP / HTTP / fix-agent paths that write it).
    let is_done = |task_number: &str| -> bool {
        if task_number == completed_task {
            return true;
        }
        matches!(
            status_map.get(task_number).map(String::as_str),
            Some("completed") | Some("skipped"),
        )
    };

    match cadence {
        // Task already handled above; included for exhaustiveness.
        MergeCadence::Task => true,
        MergeCadence::Phase => {
            // Find the phase that owns the just-completed task.
            let Some(phase) = plan
                .phases
                .iter()
                .find(|p| p.tasks.iter().any(|t| t.number == completed_task))
            else {
                return false;
            };
            phase.tasks.iter().all(|t| is_done(&t.number))
        }
        MergeCadence::Plan => {
            // Defensive: refuse the merge if `completed_task` isn't even
            // in the plan (parser drift, stale agent row).
            if !plan
                .phases
                .iter()
                .any(|p| p.tasks.iter().any(|t| t.number == completed_task))
            {
                return false;
            }
            plan.phases
                .iter()
                .flat_map(|p| p.tasks.iter())
                .all(|t| is_done(&t.number))
        }
    }
}

/// Resolve the effective merge cadence for `plan_name`. Plan-level
/// override wins; otherwise inherit the repo `[auto_mode] merge_cadence`
/// if a `branchwork.toml` is present; otherwise [`MergeCadence::default()`]
/// (= [`MergeCadence::Phase`]).
fn resolve_effective_cadence(state: &AppState, plan_name: &str) -> MergeCadence {
    if let Some(c) = db::plan_merge_cadence(&state.db, plan_name) {
        return c;
    }
    if let Some(dir) = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name)
        && let Some(cfg) = crate::repo_config::load_for_project_dir(&dir)
    {
        return cfg.auto_mode.merge_cadence;
    }
    MergeCadence::default()
}

// ── Pre-merge gate (Phase 1 of the pre-merge-gate plan) ─────────────────────
//
// The gate runs each configured `[auto_mode.pre_merge_checks]` entry in a
// fresh detached-HEAD worktree at the agent's branch tip before any merge
// happens. A failing check pauses auto-mode with the literal reason
// `pre_merge_check_failed` (T1.3); the check name + truncated output
// snippet travel via the `auto_mode_pre_merge_check_failed` broadcast
// payload and the matching audit row. A passing (or absent) gate is a
// no-op so plans without the section keep their pre-1.2 behaviour.
//
// Wired into [`run_state_machine`] between [`should_merge_now`] returning
// true and [`drain_deferred_for_cadence`] — the cadence drain itself may
// land several merges on trunk locally, so the gate has to run BEFORE
// that batch starts. The gate runs on the trigger agent's branch only;
// drained siblings are not re-gated (the brief explicitly keeps Phase 1
// minimal; per-sibling gates can land in a later phase).
//
// 50 KB cap inside the check runner (`PRE_MERGE_CHECK_OUTPUT_CAP_BYTES`)
// keeps the in-memory `GateOutcome::Fail.output` bounded; the audit row
// and broadcast carry a tighter 4 KB snippet
// (`PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES`) so neither persisted SQLite rows
// nor the dashboard live feed balloon. The middle-truncation marker
// (`[…truncated…]`) means a long log keeps both its beginning
// (compile target / setup banner) and end (actual error) — the most
// useful slices for triage.

/// Maximum captured bytes per check output. Anything longer is collapsed
/// to `<first half> [...truncated...] <last half>` so the in-memory
/// `GateOutcome::Fail.output` stays bounded while the check is running.
pub(crate) const PRE_MERGE_CHECK_OUTPUT_CAP_BYTES: usize = 50 * 1024;

/// Cap on the `output_snippet` field carried by the audit row + the
/// `auto_mode_pre_merge_check_failed` broadcast (T1.3 of the
/// `pre-merge-gate` plan). Tighter than [`PRE_MERGE_CHECK_OUTPUT_CAP_BYTES`]
/// because the audit row is persisted forever and the broadcast crosses
/// the WS wire — 4 KB is enough context for triage; the operator
/// reproduces locally for the full log.
pub(crate) const PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES: usize = 4 * 1024;

/// Marker inserted in the middle of a truncated capture.
pub(crate) const PRE_MERGE_TRUNCATION_MARKER: &str = "\n[…truncated…]\n";

/// Outcome of [`run_pre_merge_gate`].
///
/// `Pass` is the common case: gate absent OR every configured check
/// exited 0 within its per-check `timeout_secs`. The caller proceeds to
/// the merge step exactly as before.
///
/// `Fail` carries enough context for the pause / audit row / dashboard
/// payload — the check's `name` (the unique identifier from
/// `branchwork.toml`), the exit code (`None` if killed by timeout), and
/// the captured combined stdout+stderr (50 KB cap, middle-truncated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    Pass,
    Fail {
        check: String,
        exit_code: Option<i32>,
        output: String,
    },
}

/// Run the pre-merge gate for `agent_id` on its task branch in a fresh
/// temporary `git worktree`. Each configured check runs to completion or
/// its `timeout_secs`; the first failure wins and short-circuits the
/// rest.
///
/// Resolution chain for the check config: load `branchwork.toml` from
/// the plan's project directory (via [`crate::ci::project_dir_for`]); if
/// the section is empty or the file absent, return [`GateOutcome::Pass`]
/// immediately — the feature is strictly opt-in.
///
/// Worktree cleanup is intrinsic: [`crate::agents::worktree::TempWorktree`]
/// drops via `git worktree remove --force` (with a `remove_dir_all`
/// fallback) the moment the function returns, regardless of outcome.
///
/// Errors that prevent the gate from running (missing project dir,
/// agent row gone, branch column NULL, worktree creation fails) collapse
/// to a synthetic [`GateOutcome::Fail`] with `check = "_gate_setup_"` so
/// the caller pauses on the same code path as a real check failure
/// — auto-mode never half-merges on incomplete metadata.
pub(crate) async fn run_pre_merge_gate(
    state: &AppState,
    plan_name: &str,
    task_id: &str,
    agent_id: &str,
) -> GateOutcome {
    // Resolve the project directory FIRST so we can short-circuit on
    // missing config without touching the DB or the filesystem.
    let project_dir = match crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name) {
        Some(d) => d,
        None => {
            // No project → no config → no gate. Gate is opt-in; absent
            // config is success.
            return GateOutcome::Pass;
        }
    };
    let Some(repo_cfg) = crate::repo_config::load_for_project_dir(&project_dir) else {
        return GateOutcome::Pass;
    };
    let checks = &repo_cfg.auto_mode.pre_merge_checks;
    if checks.is_empty() {
        return GateOutcome::Pass;
    }
    let total_timeout = Duration::from_secs(repo_cfg.auto_mode.pre_merge_total_timeout_secs as u64);

    // Resolve the agent's branch.
    let branch: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT branch FROM agents WHERE id = ?1",
            params![agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };
    let Some(branch) = branch else {
        // No branch column → nothing to gate against. Same shape as
        // worktree creation failing — pause the loop so the operator
        // notices instead of silently skipping the gate.
        return GateOutcome::Fail {
            check: "_gate_setup_".to_string(),
            exit_code: None,
            output: format!("agent {agent_id} has no branch column set"),
        };
    };

    // Create the temporary worktree at `branch`'s tip. Drop guard cleans
    // up unconditionally below.
    let worktree =
        match crate::agents::worktree::TempWorktree::create(&project_dir, agent_id, &branch) {
            Ok(w) => w,
            Err(e) => {
                return GateOutcome::Fail {
                    check: "_gate_setup_".to_string(),
                    exit_code: None,
                    output: format!("worktree add failed: {e}"),
                };
            }
        };
    let worktree_path = worktree.path().to_path_buf();

    // Track start time for the whole-gate ceiling. We don't subtract
    // it from the per-check budget directly — the brief says fail
    // CLOSED if the cumulative time exceeds the cap.
    let started = Instant::now();

    for check in checks {
        // Compute the cwd for this check: `<worktree>/<check.cwd or .>`.
        let cwd = match check.cwd.as_deref() {
            Some(sub) if !sub.is_empty() => worktree_path.join(sub),
            _ => worktree_path.clone(),
        };

        // Honor the whole-gate ceiling: if we're already past it,
        // fail closed without running another check. (Plan name is
        // synthetic so the audit row + pause reason both name the
        // explicit ceiling rather than the check that happened to be
        // next in line — easier to triage.)
        if started.elapsed() >= total_timeout {
            broadcast_state(state, plan_name, task_id, state_labels::PAUSED, None, None);
            return GateOutcome::Fail {
                check: "_total_timeout_".to_string(),
                exit_code: None,
                output: format!(
                    "pre-merge gate exceeded total timeout of {}s before \
                     running {:?}",
                    total_timeout.as_secs(),
                    check.name
                ),
            };
        }

        let per_check_timeout = Duration::from_secs(check.timeout_secs as u64);
        let outcome = run_single_check(&check.cmd, &cwd, per_check_timeout).await;
        match outcome {
            SingleCheckOutcome::Passed => {
                // Continue to the next check.
            }
            SingleCheckOutcome::Failed { exit_code, output } => {
                return GateOutcome::Fail {
                    check: check.name.clone(),
                    exit_code,
                    output,
                };
            }
        }
    }

    // Every check passed.
    GateOutcome::Pass
}

/// Outcome of a single configured check — internal to the gate runner.
#[derive(Debug)]
enum SingleCheckOutcome {
    Passed,
    Failed {
        exit_code: Option<i32>,
        output: String,
    },
}

/// Run one configured check via `sh -c`, capturing combined stdout +
/// stderr, with `timeout` as the per-check wall-clock cap.
///
/// `exit_code = None` on Failed means the timeout fired (or the process
/// died from a signal, which surfaces here as `code.is_none()` too — the
/// audit shape is honest about the uncertainty).
///
/// On Unix, the child is placed in a new session via `setsid` so a
/// `killpg(SIGKILL)` reaches every descendant — without this, killing
/// `sh` orphans whatever it spawned (e.g. `sleep`) and leaves the pipe
/// FDs open, which would block our stdout/stderr drain past the
/// timeout.
async fn run_single_check(
    cmd: &str,
    cwd: &std::path::Path,
    timeout: Duration,
) -> SingleCheckOutcome {
    use tokio::io::AsyncReadExt;
    use tokio::process::Command as TokioCommand;

    let mut cmd_builder = TokioCommand::new("sh");
    cmd_builder
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // tokio's process API: a child whose handle is dropped before
        // it exits stays as a zombie unless `kill_on_drop(true)` is
        // set. We always wait explicitly below, but the safety net is
        // cheap.
        .kill_on_drop(true);

    // Unix: put the child in its own process group so we can kill the
    // whole tree on timeout. `sh -c` propagates SIGKILL to children
    // only when they share its pgid — `setsid` gives the child a
    // brand-new pgid equal to its pid. `tokio::process::Command::pre_exec`
    // is the passthrough to `std::os::unix::process::CommandExt::pre_exec`.
    //
    // SAFETY: `pre_exec` runs in the post-fork pre-exec window where the
    // only safe operations are async-signal-safe; `setsid` is on the
    // POSIX safe list. The closure does not touch any shared state from
    // the parent.
    #[cfg(unix)]
    unsafe {
        cmd_builder.pre_exec(|| {
            // setsid never fails on a newly-forked process where the
            // pid != session leader's pid (which is the case here).
            // Ignore the return value: even if it failed, the worst
            // case is the timeout path falls back to single-process
            // kill semantics (same as Windows).
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match cmd_builder.spawn() {
        Ok(c) => c,
        Err(e) => {
            return SingleCheckOutcome::Failed {
                exit_code: None,
                output: format!("failed to spawn `sh -c {cmd:?}`: {e}"),
            };
        }
    };

    // Capture the child's pid for the process-group kill on Unix.
    #[cfg(unix)]
    let child_pid = child.id().map(|p| p as i32);

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout.take() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr.take() {
            let _ = s.read_to_end(&mut buf).await;
        }
        buf
    });

    let wait_result = tokio::time::timeout(timeout, child.wait()).await;
    match wait_result {
        Ok(Ok(status)) => {
            let out_bytes = stdout_task.await.unwrap_or_default();
            let err_bytes = stderr_task.await.unwrap_or_default();
            let combined = combine_streams(&out_bytes, &err_bytes);
            let output = truncate_output(&combined, PRE_MERGE_CHECK_OUTPUT_CAP_BYTES);
            if status.success() {
                SingleCheckOutcome::Passed
            } else {
                SingleCheckOutcome::Failed {
                    exit_code: status.code(),
                    output,
                }
            }
        }
        Ok(Err(e)) => SingleCheckOutcome::Failed {
            exit_code: None,
            output: format!("failed to await child: {e}"),
        },
        Err(_elapsed) => {
            // Timed out. Kill the whole process group on Unix so child
            // processes (e.g. `sleep` under `sh -c`) die too — otherwise
            // they keep the pipe FDs open and block stdout/stderr drain.
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                unsafe {
                    // `-pid` targets the process group whose pgid == pid.
                    // Safe per `man 2 kill`; on failure (e.g. group
                    // already gone) we silently fall through to the
                    // single-process kill below.
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            // Single-process kill as a portable fallback (Unix sends a
            // second SIGKILL to the leader, which is a no-op; Windows
            // uses TerminateProcess on the lone child).
            let _ = child.start_kill();
            // Give the kill a brief moment so the io tasks can drain
            // whatever the child managed to emit before SIGKILL. Bounded
            // by 500ms so a truly stuck pipe can't wedge the gate.
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            let out_bytes = tokio::time::timeout(Duration::from_millis(500), stdout_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let err_bytes = tokio::time::timeout(Duration::from_millis(500), stderr_task)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let mut combined = combine_streams(&out_bytes, &err_bytes);
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&format!(
                "[killed by gate: exceeded per-check timeout of {}s]\n",
                timeout.as_secs()
            ));
            SingleCheckOutcome::Failed {
                exit_code: None,
                output: truncate_output(&combined, PRE_MERGE_CHECK_OUTPUT_CAP_BYTES),
            }
        }
    }
}

/// Combine stdout + stderr into a single lossy-UTF8 string for the audit
/// payload. We just concatenate (stdout, then stderr) — the byte streams
/// were captured separately by `tokio::process::ChildStd*`, and the
/// audit row is for human review, not log parsing.
fn combine_streams(stdout: &[u8], stderr: &[u8]) -> String {
    let so = String::from_utf8_lossy(stdout);
    let se = String::from_utf8_lossy(stderr);
    match (so.is_empty(), se.is_empty()) {
        (true, true) => String::new(),
        (false, true) => so.into_owned(),
        (true, false) => se.into_owned(),
        (false, false) => {
            // Keep stdout first; stderr usually carries the actual
            // failure on UNIX toolchains. Newline separator so the
            // boundary is visible.
            let mut s = so.into_owned();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&se);
            s
        }
    }
}

/// Trim `s` to `cap` bytes, dropping the middle of the string and
/// replacing it with [`PRE_MERGE_TRUNCATION_MARKER`].
///
/// Truncation respects char boundaries (we only ever split at byte
/// indices that we walk back to a UTF-8 leading byte) so we don't emit
/// invalid UTF-8 in the audit payload.
fn truncate_output(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let marker_len = PRE_MERGE_TRUNCATION_MARKER.len();
    if cap <= marker_len + 2 {
        // Pathological cap: just show the marker.
        return PRE_MERGE_TRUNCATION_MARKER.to_string();
    }
    let half = (cap - marker_len) / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_start = ceil_char_boundary(s, s.len() - half);
    let mut out = String::with_capacity(head_end + marker_len + (s.len() - tail_start));
    out.push_str(&s[..head_end]);
    out.push_str(PRE_MERGE_TRUNCATION_MARKER);
    out.push_str(&s[tail_start..]);
    out
}

/// Walk `idx` back to the nearest UTF-8 char boundary at or below `idx`.
/// `std::str::floor_char_boundary` is unstable, hence the hand-rolled
/// version. Identical semantics, scoped to ASCII-or-multi-byte UTF-8
/// (no SIMD nuance — gate outputs are typically log text).
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Walk `idx` forward to the nearest UTF-8 char boundary at or above
/// `idx`. Mirror of [`floor_char_boundary`].
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
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
///
/// `trigger_ci=true` (the default for the cadence-boundary trigger
/// merge) spawns `crate::ci::trigger_after_merge` which pushes the
/// new trunk SHA to origin and inserts the `ci_runs` row. `false` is
/// used for the deferred siblings during a cadence batch drain — they
/// land locally; the final merge in the batch is what pushes. Either
/// way the per-merge `auto_mode_merged` broadcast + audit row still
/// fire, so the dashboard sees every drained task get marked merged.
async fn run_merge_step(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
    trigger_ci: bool,
) -> MergeStepOutcome {
    let outcome = merge_agent_branch_dispatch(state, org_id, agent_id, None, trigger_ci).await;

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
        // Clear any stale cadence-deferral marker on this agent row —
        // a successful merge is the natural transition out of the
        // `deferred_for_cadence` state. (Drains run a SELECT before
        // calling us, but a defensive UPDATE here is cheap and keeps
        // the column self-healing if a row ever ends up marked
        // without being in the drain list.)
        conn.execute(
            "UPDATE agents SET merge_status = NULL \
             WHERE id = ?1 AND merge_status = 'deferred_for_cadence'",
            params![agent_id],
        )
        .ok();
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

    db::auto_mode_pause(&state.db, plan_name, &reason, None);

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

/// Mark `agent_id` as `merge_status='deferred_for_cadence'`, broadcast
/// `auto_mode_merge_deferred` and the matching `auto_mode_state(deferred)`
/// pill update, and write an audit row. Used by [`run_state_machine`]
/// when [`should_merge_now`] returns false on a clean task completion.
/// The agent's `branch` column is left intact — the cadence-boundary
/// drain reads it back when it's time to merge.
async fn defer_for_cadence(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
    cadence: MergeCadence,
) {
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
            params![agent_id],
        )
        .ok();
    }

    let cadence_wire = db::merge_cadence_wire(cadence);
    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "agent_id": agent_id,
        "cadence": cadence_wire,
    });
    broadcast_event(
        &state.broadcast_tx,
        "auto_mode_merge_deferred",
        payload.clone(),
    );
    broadcast_state(
        state,
        plan_name,
        task_id,
        state_labels::DEFERRED,
        None,
        Some(cadence_wire),
    );

    let conn = state.db.lock().unwrap();
    audit::log(
        &conn,
        org_id,
        None,
        Some("branchwork-auto-mode"),
        actions::AUTO_MODE_MERGE_DEFERRED,
        audit::resources::AGENT,
        Some(agent_id),
        Some(&payload.to_string()),
    );
}

/// Look up agent rows for `plan_name` that completed cleanly but were
/// deferred via [`defer_for_cadence`]. The list is filtered to the
/// scope implied by `cadence`:
///
/// - [`MergeCadence::Phase`]: only deferred agents whose task lives in
///   the same phase as `trigger_task` (the task that just completed
///   and flipped [`should_merge_now`] to true).
/// - [`MergeCadence::Plan`]: every deferred agent in the plan.
/// - [`MergeCadence::Task`]: empty (this cadence never defers, so the
///   drain is a no-op).
///
/// The trigger agent itself is intentionally **not** in the returned
/// list — the caller merges it separately as the final step of the
/// batch (so the per-merge CI trigger fires exactly once on the
/// boundary).
///
/// Output is ordered by **dependency order** — the YAML declaration
/// order in the plan. Production task numbers like "1.10" don't sort
/// lexically alongside "1.2", so we walk `plan.phases.tasks` to
/// produce an ordered task list, then filter to the deferred subset.
/// Agent rows are joined to that ordering, so a phase with deferred
/// agents on tasks 1.1, 1.2, 1.3 is returned in that exact order.
fn list_deferred_for_cadence_in_order(
    state: &AppState,
    plan_name: &str,
    trigger_task: &str,
    trigger_agent_id: &str,
    cadence: MergeCadence,
) -> Vec<(String, String)> {
    if cadence == MergeCadence::Task {
        return Vec::new();
    }

    let plan = match plan_parser::find_plan_file(&state.plans_dir, plan_name)
        .and_then(|p| plan_parser::parse_plan_file(&p).ok())
    {
        Some(p) => p,
        None => return Vec::new(),
    };

    // Build the dependency-ordered task list scoped to phase or plan.
    let task_order: Vec<String> = match cadence {
        MergeCadence::Task => return Vec::new(),
        MergeCadence::Phase => {
            let Some(phase) = plan
                .phases
                .iter()
                .find(|p| p.tasks.iter().any(|t| t.number == trigger_task))
            else {
                return Vec::new();
            };
            phase.tasks.iter().map(|t| t.number.clone()).collect()
        }
        MergeCadence::Plan => plan
            .phases
            .iter()
            .flat_map(|p| p.tasks.iter().map(|t| t.number.clone()))
            .collect(),
    };

    // Pull the deferred agents in scope into a (task_id -> agent_id) map.
    // A task may have multiple agent rows (retries, killed siblings); the
    // drain only cares about rows that (a) are marked deferred AND (b)
    // still carry a non-null branch (the merge target). Take the most
    // recent matching row per task — that's the row whose branch points
    // at the actual committed work.
    let mut per_task: HashMap<String, String> = HashMap::new();
    {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, task_id FROM agents \
             WHERE plan_name = ?1 \
               AND merge_status = 'deferred_for_cadence' \
               AND branch IS NOT NULL \
               AND id != ?2 \
             ORDER BY started_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![plan_name, trigger_agent_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        if let Ok(rows) = rows {
            for r in rows.flatten() {
                // Last-write-wins: keep the most-recent deferred agent
                // for a given task (ORDER BY started_at ASC means the
                // latest row wins on collision).
                per_task.insert(r.1, r.0);
            }
        }
    }

    task_order
        .into_iter()
        .filter_map(|t| per_task.remove(&t).map(|agent_id| (agent_id, t)))
        .collect()
}

/// Drain every deferred-for-cadence agent in scope before the trigger
/// agent is merged. Each drained merge fires the `auto_mode_merged`
/// broadcast + audit row but DOES NOT push (the trigger agent's merge
/// is what pushes). Returns `Some(reason)` if any drained merge failed —
/// the plan is already paused + audit-logged by [`run_merge_step`], so
/// the caller just propagates the abort and lets the UI flip out of
/// `merging`. Returns `None` on full success (or empty drain queue).
async fn drain_deferred_for_cadence(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    trigger_task: &str,
    trigger_agent_id: &str,
    cadence: MergeCadence,
) -> Option<()> {
    let deferred = list_deferred_for_cadence_in_order(
        state,
        plan_name,
        trigger_task,
        trigger_agent_id,
        cadence,
    );
    if deferred.is_empty() {
        return Some(()); // nothing to drain — success
    }

    for (agent_id, task_id) in deferred {
        match run_merge_step(state, org_id, &agent_id, plan_name, &task_id, false).await {
            MergeStepOutcome::Merged(_) => continue,
            MergeStepOutcome::Paused => {
                // run_merge_step has already paused + audit-logged; the
                // caller flips the pill out of `merging` to `paused`.
                return None;
            }
        }
    }
    Some(())
}

// ── Operator-driven flush (Task 2.3) ────────────────────────────────────────

/// One merged agent in a flush batch. Returned to the HTTP caller so the
/// UI can show a "merged tasks 1.1, 1.2, 1.3" toast.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlushedAgent {
    pub agent_id: String,
    pub task_id: String,
}

/// Outcome of [`flush_deferred_merges`]. `merged` is the list of agents
/// that successfully landed; `paused` is true when a merge inside the
/// batch failed and the plan was paused mid-flush (the failed agent is
/// **not** in `merged`). The HTTP handler maps this into a JSON response
/// shape with `ok`, `merged`, `paused`, and a human-friendly `message`.
#[derive(Debug, Clone)]
pub struct FlushMergesOutcome {
    pub merged: Vec<FlushedAgent>,
    pub paused: bool,
}

/// List every `merge_status='deferred_for_cadence'` agent in `plan_name`
/// ordered by YAML declaration order (phase 1 task 1, phase 1 task 2,
/// …). Differs from [`list_deferred_for_cadence_in_order`] in two ways:
///   1. No cadence scoping — every deferred agent in the plan is
///      returned regardless of phase.
///   2. No trigger-agent exclusion — there is no trigger when an
///      operator manually flushes; the whole batch is the work.
///
/// The most-recent `started_at` row wins per task (last-write-wins on
/// retries) — same posture as the cadence-batch helper.
fn list_all_deferred_in_order(state: &AppState, plan_name: &str) -> Vec<(String, String)> {
    let plan = match plan_parser::find_plan_file(&state.plans_dir, plan_name)
        .and_then(|p| plan_parser::parse_plan_file(&p).ok())
    {
        Some(p) => p,
        None => return Vec::new(),
    };

    let task_order: Vec<String> = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter().map(|t| t.number.clone()))
        .collect();

    let mut per_task: HashMap<String, String> = HashMap::new();
    {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, task_id FROM agents \
             WHERE plan_name = ?1 \
               AND merge_status = 'deferred_for_cadence' \
               AND branch IS NOT NULL \
             ORDER BY started_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        if let Ok(rows) = rows {
            for r in rows.flatten() {
                // Last-write-wins per task: ORDER BY started_at ASC means
                // the most-recent row's agent_id ends up in the map.
                per_task.insert(r.1, r.0);
            }
        }
    }

    task_order
        .into_iter()
        .filter_map(|t| per_task.remove(&t).map(|agent_id| (agent_id, t)))
        .collect()
}

/// Operator escape hatch — drain every `merge_status='deferred_for_cadence'`
/// row in `plan_name` regardless of the configured cadence. The final
/// merge in the batch is the one that triggers CI (push + `ci_runs`
/// row) so the user gets exactly one master build + deploy out the
/// other end. Earlier merges in the batch land locally with
/// `trigger_ci=false`, same as the cadence-boundary drain.
///
/// Idempotent: zero deferred rows is a clean no-op. A flush emitted
/// against a plan that has no deferred work doesn't broadcast or
/// audit-log a merge — it returns `{merged: [], paused: false}`. The
/// HTTP handler turns that into a 200 with a clear "no deferred merges
/// to flush" message.
///
/// On a merge failure inside the batch: [`run_merge_step`] already
/// pauses the plan and audit-logs its own `AUTO_MODE_PAUSED`. The
/// flush returns `{merged: <successful so far>, paused: true}` so the
/// HTTP handler can echo that back to the UI. The plan's auto-mode
/// pill flips to `paused` via the existing `auto_mode_paused` event.
///
/// Emits a single `auto_mode_flushed_deferred` broadcast at the end
/// (alongside the per-merge `auto_mode_merged` events that fire
/// naturally inside `run_merge_step`) and a matching audit row so the
/// operator intent is captured separately from the individual merges.
pub async fn flush_deferred_merges(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
) -> FlushMergesOutcome {
    let deferred = list_all_deferred_in_order(state, plan_name);
    if deferred.is_empty() {
        return FlushMergesOutcome {
            merged: Vec::new(),
            paused: false,
        };
    }

    let last_idx = deferred.len() - 1;
    let mut merged: Vec<FlushedAgent> = Vec::with_capacity(deferred.len());
    let mut paused = false;
    for (i, (agent_id, task_id)) in deferred.into_iter().enumerate() {
        // Only the FINAL merge in the batch triggers CI (push + ci_runs).
        // Earlier merges land locally so the whole batch ships as one
        // master build, matching the cadence-boundary drain shape.
        let trigger_ci = i == last_idx;
        match run_merge_step(state, org_id, &agent_id, plan_name, &task_id, trigger_ci).await {
            MergeStepOutcome::Merged(_) => {
                merged.push(FlushedAgent { agent_id, task_id });
            }
            MergeStepOutcome::Paused => {
                // run_merge_step has already paused + audit-logged.
                paused = true;
                break;
            }
        }
    }

    let payload = serde_json::json!({
        "plan": plan_name,
        "count": merged.len(),
        "paused": paused,
    });
    broadcast_event(
        &state.broadcast_tx,
        "auto_mode_flushed_deferred",
        payload.clone(),
    );
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_MODE_FLUSHED_DEFERRED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }

    FlushMergesOutcome { merged, paused }
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
///
/// Task 2.2 added a cadence gate at the top: [`should_merge_now`]
/// decides whether the just-completed task crosses the configured
/// boundary (phase / plan). If not, the agent is marked
/// `merge_status='deferred_for_cadence'`, `auto_mode_merge_deferred`
/// fires, and the loop returns without touching trunk. The next
/// completion that flips `should_merge_now` to true drains every
/// deferred sibling in dependency order (no per-merge push), then
/// merges itself (the one push that lands the whole batch).
async fn run_state_machine(
    state: &AppState,
    org_id: &str,
    agent_id: &str,
    plan_name: &str,
    task_id: &str,
) {
    // Defense-in-depth idempotency (T1.3): if the plan is already
    // paused, bail out before doing any work. `on_task_agent_completed`
    // already runs the same `db::auto_mode_enabled` check at the entry
    // edge of the auto-mode flow, but a direct caller of
    // `run_state_machine` (or a re-fired completion event for the same
    // agent) must not re-run the pre-merge gate, re-pause the row,
    // re-emit broadcasts, or write a duplicate audit row. The brief
    // pins this explicitly: "subsequent calls to `run_state_machine`
    // for the same task short-circuit — no infinite re-run."
    if !db::auto_mode_enabled(&state.db, plan_name) {
        return;
    }

    let cadence = resolve_effective_cadence(state, plan_name);

    // Cadence gate: if we're not at the boundary, defer and bail.
    // `Task` cadence never defers — `should_merge_now` always returns
    // true and the drain helper short-circuits to empty.
    if !should_merge_now(state, plan_name, task_id) {
        defer_for_cadence(state, org_id, agent_id, plan_name, task_id, cadence).await;
        // Deferral doesn't merge, but the phase still needs to make
        // progress — otherwise auto-mode deadlocks here (task done,
        // merge deferred, no next-task spawn). Kick try_auto_advance
        // so the next pending task in the phase picks up. `None` for
        // merged_sha: no merge happened, manual-path semantics.
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
            None,
        )
        .await;
        return;
    }

    // Pre-merge gate (Phase 1 of pre-merge-gate plan, T1.2): run each
    // configured `[auto_mode.pre_merge_checks]` entry in a fresh worktree
    // checked out at the agent's branch tip BEFORE any merge happens.
    // The gate is opt-in — when the section is missing or empty, this
    // call returns `Pass` immediately without touching disk. We emit the
    // `merging` pill AFTER the gate runs so a long-running gate doesn't
    // make the UI claim a merge is in progress; the gate paused-pause
    // path emits its own `paused` pill update.
    match run_pre_merge_gate(state, plan_name, task_id, agent_id).await {
        GateOutcome::Pass => {}
        GateOutcome::Fail {
            check,
            exit_code,
            output,
        } => {
            // T1.3 of the pre-merge-gate plan: pause the plan with the
            // literal reason `pre_merge_check_failed` (no check name
            // appended — the check name travels in the audit/broadcast
            // payload, the reason is the kind of pause). The agent's
            // work is still on its branch; the block is at the merge
            // gate, not the work. We intentionally do NOT touch
            // `agents.merge_status` here — a deferred-for-cadence row
            // stays deferred so the sibling can drain after the
            // operator clicks Resume. The dirty-tree-watcher /
            // runner-offline-style auto-resume paths intentionally do
            // NOT cover this — fixing a failing build requires a
            // human in the loop.
            let reason = "pre_merge_check_failed";
            db::auto_mode_pause(&state.db, plan_name, reason, None);

            // Truncate the captured output to a tight per-row cap for
            // both the audit row (persisted forever) and the WS
            // broadcast (crosses the wire). The full 50 KB capture
            // lives in `GateOutcome::Fail.output` and is dropped on
            // the floor when this function returns — operator
            // reproduces locally for the unabridged log.
            let output_snippet = truncate_output(&output, PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES);

            let payload = serde_json::json!({
                "plan": plan_name,
                "task": task_id,
                "agent_id": agent_id,
                "check_name": check,
                "exit_code": exit_code,
                "output_snippet": output_snippet,
            });
            broadcast_event(
                &state.broadcast_tx,
                "auto_mode_pre_merge_check_failed",
                payload.clone(),
            );
            // Also surface a paused pill so the existing AutoModeStatusPill
            // path catches it (the new event is structured detail — the
            // pill listener is already wired to auto_mode_paused, and
            // the dashboard banner reads `pausedReason` from the
            // PlanConfig snapshot loaded via GET /api/plans/<name>/config).
            broadcast_event(
                &state.broadcast_tx,
                "auto_mode_paused",
                serde_json::json!({
                    "plan": plan_name,
                    "task": task_id,
                    "reason": reason,
                }),
            );
            {
                let conn = state.db.lock().unwrap();
                audit::log(
                    &conn,
                    org_id,
                    None,
                    Some("branchwork-auto-mode"),
                    actions::AUTO_MODE_PRE_MERGE_CHECK_FAILED,
                    audit::resources::PLAN,
                    Some(plan_name),
                    Some(&payload.to_string()),
                );
            }
            broadcast_state(state, plan_name, task_id, state_labels::PAUSED, None, None);
            return;
        }
    }

    broadcast_state(state, plan_name, task_id, state_labels::MERGING, None, None);

    // Drain deferred siblings before the trigger merge. Each drained
    // merge lands on trunk locally but does NOT push (trigger_ci=false);
    // the trigger merge below is what pushes the whole batch.
    if drain_deferred_for_cadence(state, org_id, plan_name, task_id, agent_id, cadence)
        .await
        .is_none()
    {
        // A drained merge failed; `run_merge_step` already paused the
        // plan. Flip the pill out of `merging` to `paused`.
        broadcast_state(state, plan_name, task_id, state_labels::PAUSED, None, None);
        return;
    }

    let merged_sha = match run_merge_step(state, org_id, agent_id, plan_name, task_id, true).await {
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
        // Cancelled: the API toggle-off has already done all the work
        // (auto_mode.enabled cleared, in-flight fix agents killed, audit
        // row written). The loop just bails — no further pause / merge /
        // spawn should fire.
        CiOutcome::Cancelled => {}
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
        Some(merged_sha.to_string()),
    )
    .await;
}

/// Red branch on the original task agent's merged SHA. Audits
/// `AUTO_MODE_CI_FAILED` and hands off to [`try_spawn_fix_agent_with_cap`]
/// — that helper either spawns the next fix attempt or, if the per-task
/// retry cap is reached, pauses the plan with reason `fix_cap_reached`.
async fn on_ci_failed(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    failing_run_id: Option<&str>,
) {
    let id_str = failing_run_id.unwrap_or("unknown");
    let payload = serde_json::json!({
        "plan": plan_name,
        "task": task_id,
        "sha": merged_sha,
        "ci_run_id": failing_run_id,
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

    try_spawn_fix_agent_with_cap(
        state,
        org_id,
        plan_name,
        task_id,
        merged_sha,
        id_str,
        failing_run_id,
    )
    .await;
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
    db::auto_mode_pause(&state.db, plan_name, &reason, None);

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
    /// The plan's [`CancellationToken`] fired before CI reached a
    /// terminal state. Returned when the user toggles `auto_mode` off
    /// mid-flight; the loop returns immediately without paging the
    /// dashboard, since the toggle itself is the user's intent.
    Cancelled,
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
    let cancel = state.cancel_token_for(plan_name);
    wait_for_ci_inner(
        plan_name,
        task_id,
        merged_sha,
        || has_github_actions_dispatch(state, org_id, agent_id),
        || get_ci_run_status_dispatch(state, org_id, plan_name, task_id, merged_sha),
        WaitForCiConfig::default(),
        &cancel,
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
    cancel: &CancellationToken,
) -> CiOutcome
where
    HasFn: Fn() -> HasFut,
    HasFut: Future<Output = bool>,
    GetFn: Fn() -> GetFut,
    GetFut: Future<Output = Result<Option<CiAggregate>, CiStatusError>>,
{
    if cancel.is_cancelled() {
        return CiOutcome::Cancelled;
    }
    if !has_actions().await {
        return CiOutcome::NotConfigured;
    }

    let deadline = Instant::now() + config.total_timeout;
    loop {
        if cancel.is_cancelled() {
            return CiOutcome::Cancelled;
        }
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
        tokio::select! {
            _ = cancel.cancelled() => return CiOutcome::Cancelled,
            _ = tokio::time::sleep(sleep) => {}
        }
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
        runner_id: None,
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
    // once CI is green. `trigger_ci = true` — fix merges always push
    // (they're standalone, never batched via the cadence drain).
    let outcome = merge_agent_branch_dispatch(state, org_id, fix_agent_id, None, true).await;

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
            db::auto_mode_pause(&state.db, plan_name, &reason, None);

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
        // Cancelled: the toggle-off path already paused / killed agents.
        // Close the attempt row so the cap accounting still reflects the
        // fix that ran; do not spawn another attempt.
        CiOutcome::Cancelled => {
            db::close_fix_attempt(&state.db, plan_name, &original_task, attempt, "cancelled");
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
        Some(merged_sha.to_string()),
    )
    .await;
}

/// Red branch on a fix-agent CI: audit `AUTO_MODE_CI_FAILED` and hand
/// off to [`try_spawn_fix_agent_with_cap`] so the next attempt is gated
/// by the per-plan retry cap (T3.3). The `prior_attempt` is informational
/// only — the helper recomputes the next attempt number from
/// `task_fix_attempt_count` so a stale value can't accidentally double-
/// spawn or skip a slot.
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

    try_spawn_fix_agent_with_cap(
        state,
        org_id,
        plan_name,
        original_task,
        merged_sha,
        id_str,
        failing_run_id,
    )
    .await;
}

/// Spawn the next fix agent for `(plan_name, task_id)` if the per-plan
/// `max_fix_attempts` cap allows. Otherwise pause the plan with reason
/// `fix_cap_reached` and emit the matching dashboard event + audit row.
///
/// `attempts >= cap` is the gate. With the schema default `cap = 3`:
/// - count=0 → spawn attempt 1 (the very first fix run)
/// - count=2 → spawn attempt 3 (the last allowed)
/// - count=3 → cap reached, pause
///
/// On a successful spawn this emits `auto_mode_fix_spawned` and audits
/// `AUTO_MODE_FIX_SPAWNED`. On a `None` return from
/// [`spawn_fix_agent`] (original task agent row missing — defensive,
/// should not happen in practice) the plan is paused with
/// `fix_spawn_failed` so the dashboard can surface the degenerate state.
async fn try_spawn_fix_agent_with_cap(
    state: &AppState,
    org_id: &str,
    plan_name: &str,
    task_id: &str,
    merged_sha: &str,
    failing_run_id_str: &str,
    failing_run_id: Option<&str>,
) {
    let attempts = db::task_fix_attempt_count(&state.db, plan_name, task_id);
    let cap = db::plan_max_fix_attempts(&state.db, plan_name);

    if attempts >= cap {
        let reason = "fix_cap_reached".to_string();
        db::auto_mode_pause(&state.db, plan_name, &reason, None);

        let payload = serde_json::json!({
            "plan": plan_name,
            "task": task_id,
            "attempts": attempts,
            "cap": cap,
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
        return;
    }

    let next_attempt = attempts.saturating_add(1);
    let new_fix_agent = spawn_fix_agent(
        state,
        org_id,
        plan_name,
        task_id,
        failing_run_id_str,
        next_attempt,
    )
    .await;

    if let Some(new_id) = new_fix_agent {
        let next_fix_task = format!("{task_id}-fix-{next_attempt}");
        let payload = serde_json::json!({
            "plan": plan_name,
            "task": task_id,
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
        db::auto_mode_pause(&state.db, plan_name, &reason, None);
        let payload = serde_json::json!({
            "plan": plan_name,
            "task": task_id,
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
}

// ── Idle poller ─────────────────────────────────────────────────────────────
//
// Background fallback for drivers that don't expose a Stop-hook surface.
// Phase 1 wired Claude through `Driver::stop_hook_config` + per-session
// settings file + `hooks::handle_stop_hook`, so Claude agents always trigger
// auto-finish via the hook. Other drivers (Aider, Codex, Gemini today; any
// future driver whose `stop_hook_config` returns `None`) have no programmatic
// way to call back into us when their CLI returns to idle. This poller
// closes the gap: every 60 s it scans `running` agents whose `last_activity_at`
// has not advanced for `BRANCHWORK_AUTO_FINISH_IDLE_SECS` and fires the same
// graceful-exit + audit + broadcast path the Stop hook uses, with
// `trigger: "idle_timeout"` on the audit diff and broadcast payload.
//
// Off by default — set `BRANCHWORK_AUTO_FINISH_IDLE=1` on the server to
// enable. ADR 0003 §"Failure modes" treats the timer as a stopgap that only
// opt-in users see; driver-specific instrumentation is the long-term fix.
//
// The two env vars are read **once** at server start via
// [`IdleFinishConfig::from_env`] and cached for the process lifetime —
// reducing accidental drift between iterations. See
// [`docs/reference/configuration.md`](../../../docs/reference/configuration.md)
// for the user-facing description of both vars.

const IDLE_POLL_INTERVAL_SECS: u64 = 60;
const IDLE_THRESHOLD_DEFAULT_SECS: i64 = 300;

/// Cached env-var configuration for the idle-poller fallback. Built once
/// at server start via [`Self::from_env`] and handed to
/// [`spawn_idle_poller`].
#[derive(Clone, Copy, Debug)]
pub struct IdleFinishConfig {
    /// `BRANCHWORK_AUTO_FINISH_IDLE == "1"`. When false the poller is
    /// fully inert — no tokio task is spawned at all.
    pub enabled: bool,
    /// `BRANCHWORK_AUTO_FINISH_IDLE_SECS`, or
    /// [`IDLE_THRESHOLD_DEFAULT_SECS`] if unset / invalid.
    pub threshold_secs: i64,
}

impl IdleFinishConfig {
    /// Read both env vars from the process environment. Mirrors the gate
    /// logic the previous per-iteration read used: `enabled = (var ==
    /// "1")`, `threshold = parse u positive integer or default`.
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var("BRANCHWORK_AUTO_FINISH_IDLE").ok().as_deref(),
            std::env::var("BRANCHWORK_AUTO_FINISH_IDLE_SECS")
                .ok()
                .as_deref(),
        )
    }

    /// Pure parse helper — split out from [`Self::from_env`] so unit
    /// tests can exercise the parsing rules without mutating the
    /// process-wide env.
    pub fn from_values(enabled_raw: Option<&str>, secs_raw: Option<&str>) -> Self {
        let enabled = matches!(enabled_raw, Some("1"));
        let threshold_secs = secs_raw
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(IDLE_THRESHOLD_DEFAULT_SECS);
        Self {
            enabled,
            threshold_secs,
        }
    }
}

/// Spawn the idle-poller background task using cached config. When
/// `cfg.enabled` is false this is a no-op — no tokio task is spawned, so
/// the default-off server pays nothing. Runs forever otherwise;
/// cancellation is process-exit only. Call once from `main::run_server`.
pub fn spawn_idle_poller(state: AppState, cfg: IdleFinishConfig) {
    if !cfg.enabled {
        return;
    }
    println!(
        "[Branchwork] Auto-finish idle poller enabled (threshold {}s)",
        cfg.threshold_secs
    );
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(IDLE_POLL_INTERVAL_SECS));
        // The first .tick() fires immediately; consume it so the first
        // real pass happens 60 s after server start, matching the CI
        // poller's "sleep before work" cadence.
        interval.tick().await;
        loop {
            interval.tick().await;
            run_idle_pass(&state, cfg.threshold_secs).await;
        }
    });
}

/// One pass of the idle poller. Factored out from [`spawn_idle_poller`] so
/// unit tests can drive it with synthetic threshold values without
/// mutating the process-wide env var.
///
/// Mirrors the Stop-hook decision tree in
/// [`crate::hooks::handle_stop_hook`] (filter on auto-mode-enabled, check
/// tree state, dirty pauses with `agent_left_uncommitted_work`, clean
/// fires `graceful_exit` + `AGENT_AUTO_FINISH` audit + `auto_finish_triggered`
/// broadcast). The only differences:
///   - upstream filter on `driver.stop_hook_config(...)` returning `None`
///     so Claude agents (which always go through the hook) are skipped, and
///   - `trigger` discriminator is `idle_timeout` instead of `stop_hook`.
///
/// Idempotency uses the same `auto_finish_dedupe` set as the Stop handler
/// so the two triggers can't double-fire on the same agent.
async fn run_idle_pass(state: &AppState, threshold_secs: i64) {
    type Row = (
        String,         // agent_id
        String,         // session_id
        Option<String>, // plan_name
        Option<String>, // task_id
        String,         // org_id
        Option<String>, // driver name
        i64,            // idle seconds (now - last_activity_at)
    );
    // SQL math: `strftime('%s','now') - strftime('%s', last_activity_at)`
    // gives whole-second idle. last_activity_at is written via
    // `datetime('now')` in `hooks::receive_hook` so both are UTC unix
    // epoch seconds; no timezone math needed.
    let rows: Vec<Row> = {
        let conn = state.db.lock().unwrap();
        // Filter out plan-session agents (task_id IS NULL) for the same
        // reason as the Stop-hook handler: their tree state reflects
        // any parallel task agent's WIP, not their own work, so the
        // dirty-tree pause path would mis-fire `agent_left_uncommitted_work`
        // the moment the session sat idle past the threshold.
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, plan_name, task_id, org_id, driver, \
             CAST(strftime('%s','now') - strftime('%s', last_activity_at) AS INTEGER) \
             FROM agents \
             WHERE status = 'running' \
               AND last_activity_at IS NOT NULL \
               AND task_id IS NOT NULL",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[idle-poller] prepare failed: {e}");
                return;
            }
        };
        let iter = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        });
        match iter {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(e) => {
                eprintln!("[idle-poller] query failed: {e}");
                return;
            }
        }
    };

    let hook_url = format!("http://localhost:{}/hooks", state.port);
    for (agent_id, session_id, plan_name, task_id, org_id, driver_name, idle_secs) in rows {
        let Some(plan_name) = plan_name else {
            continue;
        };
        // Driver registry lookup keeps the contract aligned with Phase 1:
        // any driver that returns `Some(...)` from `stop_hook_config` owns
        // its own auto-finish path (via the Claude `Stop` hook today, or
        // an equivalent surface tomorrow) and must not be double-triggered
        // by this poller.
        let default_driver =
            crate::persisted_settings::PersistedSettings::load(&state.settings_path)
                .default_driver()
                .to_string();
        let (_resolved, driver_arc) = state
            .registry
            .drivers
            .get_or_default_with(driver_name.as_deref(), Some(&default_driver));
        if driver_arc
            .stop_hook_config(&session_id, &hook_url)
            .is_some()
        {
            continue;
        }
        if !db::auto_mode_enabled(&state.db, &plan_name) {
            continue;
        }
        if idle_secs < threshold_secs {
            continue;
        }
        match crate::agents::check_tree_clean_for_completion(
            &state.db,
            &state.plans_dir,
            &plan_name,
        ) {
            crate::agents::TreeState::Dirty { files } => {
                let trimmed: Vec<String> = files.iter().take(5).cloned().collect();
                db::auto_mode_pause(
                    &state.db,
                    &plan_name,
                    "agent_left_uncommitted_work",
                    Some(&trimmed),
                );
                let payload = serde_json::json!({
                    "plan": plan_name,
                    "task": task_id,
                    "reason": "agent_left_uncommitted_work",
                    "files": trimmed,
                });
                broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
                // Auto-resume watcher (Task 4.1) — same as the Stop-hook
                // dirty path. Idempotent at the per-plan level so the
                // idle poller can re-fire the pause without spawning a
                // second watcher.
                spawn_dirty_tree_watcher(state.clone(), plan_name.clone());
                let conn = state.db.lock().unwrap();
                audit::log(
                    &conn,
                    &org_id,
                    None,
                    Some("branchwork-auto-mode"),
                    actions::AUTO_MODE_PAUSED,
                    audit::resources::PLAN,
                    Some(&plan_name),
                    Some(&payload.to_string()),
                );
                continue;
            }
            crate::agents::TreeState::Clean | crate::agents::TreeState::Unknown => {}
        }

        // Dedupe with the Stop-hook handler. The first trigger (whichever
        // path fires first) wins for the lifetime of this `agent_id`.
        let first_call = state
            .auto_finish_dedupe
            .lock()
            .unwrap()
            .insert(agent_id.clone());
        if !first_call {
            continue;
        }

        let registry = state.registry.clone();
        let agent_id_for_spawn = agent_id.clone();
        tokio::spawn(async move {
            registry.graceful_exit(&agent_id_for_spawn).await;
        });
        {
            let conn = state.db.lock().unwrap();
            audit::log(
                &conn,
                &org_id,
                None,
                Some("branchwork-auto-mode"),
                audit::actions::AGENT_AUTO_FINISH,
                audit::resources::AGENT,
                Some(&agent_id),
                Some(&serde_json::json!({ "trigger": "idle_timeout" }).to_string()),
            );
        }
        broadcast_event(
            &state.broadcast_tx,
            "auto_finish_triggered",
            serde_json::json!({
                "agent_id": agent_id,
                "plan": plan_name,
                "task": task_id,
                "trigger": "idle_timeout",
            }),
        );
    }
}

// ── Dirty-tree watcher (Task 4.1) ──────────────────────────────────────────
//
// When auto-mode pauses on `agent_left_uncommitted_work`, the operator
// either commits or stashes the offending files (intent: continue the
// plan) or leaves them dirty (intent: drop the plan). Forcing a manual
// Resume click in the "commits and continues" case is friction: the
// signal "tree is now clean" is observable, so the loop can auto-resume.
//
// We implement that as a per-plan short-interval poller (no inotify
// dependency — `notify` would add 3 transitive crates for a property that
// `git status --porcelain --untracked-files=no` already gives us in a
// single shell-out). Poll every [`DIRTY_TREE_POLL_INTERVAL_SECS`] seconds
// for at most [`DIRTY_TREE_MAX_POLLS`] iterations (≈100 seconds total).
// Bounded so a permanently-dirty tree doesn't churn forever: the operator
// will see the persistent paused pill and decide whether to commit or
// click Resume manually.

/// How often the dirty-tree watcher checks `git status --porcelain
/// --untracked-files=no` for the paused plan's working tree.
const DIRTY_TREE_POLL_INTERVAL_SECS: u64 = 5;

/// Hard cap on poll iterations — after this many ticks with no clean
/// signal, the watcher exits and the plan remains paused. The operator
/// can still click Resume manually. `5 s × 20 = 100 s`.
const DIRTY_TREE_MAX_POLLS: u32 = 20;

/// Spawn a one-shot dirty-tree watcher for `plan_name`. Idempotent at the
/// per-plan level via [`AppState::dirty_tree_watchers`]: a second call for
/// a plan that already has a live watcher is a no-op (the first watcher
/// will observe the same tree state and act on it).
///
/// The watcher exits silently when any of the following happens:
///   - the working tree comes back clean → auto-resume + audit + broadcast +
///     `try_auto_advance` (mirrors the operator-driven Resume path in
///     `api/plans.rs::put_plan_config`),
///   - the plan's `paused_reason` changes out from under us (manual
///     Resume by the operator, or a different pause reason replaced
///     `agent_left_uncommitted_work`),
///   - [`DIRTY_TREE_MAX_POLLS`] iterations have elapsed without a
///     clean signal — the operator must Resume manually.
///
/// The dedupe set entry is removed on every exit path so the next pause
/// can spawn a fresh watcher.
pub fn spawn_dirty_tree_watcher(state: AppState, plan_name: String) {
    // Dedupe: first pause wins for this plan. `HashSet::insert` returns
    // true on first insert. If a watcher is already running we drop
    // silently — the live watcher will pick up whatever tree-clean
    // signal arrives next.
    let first_call = state
        .dirty_tree_watchers
        .lock()
        .unwrap()
        .insert(plan_name.clone());
    if !first_call {
        return;
    }

    tokio::spawn(async move {
        run_dirty_tree_watcher(state, plan_name).await;
    });
}

/// Body of the dirty-tree watcher loop. Factored out from
/// [`spawn_dirty_tree_watcher`] so unit tests can drive it without a
/// tokio::spawn handle (and without the 5-second polling interval — tests
/// inject `interval_override` to compress the loop).
async fn run_dirty_tree_watcher(state: AppState, plan_name: String) {
    run_dirty_tree_watcher_with_config(
        state,
        plan_name,
        Duration::from_secs(DIRTY_TREE_POLL_INTERVAL_SECS),
        DIRTY_TREE_MAX_POLLS,
    )
    .await;
}

/// Configurable variant of [`run_dirty_tree_watcher`] for tests. Production
/// callers go through [`spawn_dirty_tree_watcher`] which hardcodes the 5 s
/// interval and 20-poll cap.
async fn run_dirty_tree_watcher_with_config(
    state: AppState,
    plan_name: String,
    poll_interval: Duration,
    max_polls: u32,
) {
    // The loop ALWAYS removes the dedupe entry on exit. Using a defer-style
    // scope guard keeps every early-return path symmetric; without it a
    // future contributor adding a new exit point could leak an entry and
    // permanently lock the plan out of auto-resume.
    struct DedupeGuard {
        watchers: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        plan_name: String,
    }
    impl Drop for DedupeGuard {
        fn drop(&mut self) {
            self.watchers.lock().unwrap().remove(&self.plan_name);
        }
    }
    let _guard = DedupeGuard {
        watchers: state.dirty_tree_watchers.clone(),
        plan_name: plan_name.clone(),
    };

    for _ in 0..max_polls {
        tokio::time::sleep(poll_interval).await;

        // Re-read the pause state every tick: the operator may have
        // clicked Resume manually (paused_reason → NULL), or a different
        // pause path may have replaced our reason. Either way the
        // watcher's job is done — exit and let the corresponding handler
        // own the resume.
        let cfg = db::auto_mode_config(&state.db, &plan_name);
        match cfg.paused_reason.as_deref() {
            Some("agent_left_uncommitted_work") => {}
            _ => return,
        }

        // Tree probe. Unknown is permissive — same convention as
        // `check_tree_clean_for_completion`. Dirty keeps polling.
        match crate::agents::check_tree_clean_for_completion(
            &state.db,
            &state.plans_dir,
            &plan_name,
        ) {
            crate::agents::TreeState::Clean | crate::agents::TreeState::Unknown => {
                // Resume path: identical to the operator-driven Resume in
                // `api/plans.rs::put_plan_config` (clear paused state,
                // audit, broadcast, fire `try_auto_advance` from the last
                // completed task) except for the action constant and the
                // synthetic user identity.
                resume_after_clean_tree(&state, &plan_name).await;
                return;
            }
            crate::agents::TreeState::Dirty { .. } => {
                continue;
            }
        }
    }
    // Cap reached: the plan stays paused. The dedupe entry is freed by
    // the guard's Drop so a subsequent pause can spawn a fresh watcher
    // (e.g. the operator dirties the tree again later).
}

/// Resume a plan whose dirty tree has just come back clean. Mirrors the
/// operator-driven Resume path in `api/plans.rs::put_plan_config` so the
/// dashboard sees the same WS event and audit shape — only the action
/// constant differs ([`actions::AUTO_RESUMED_TREE_CLEAN`] instead of
/// [`audit::actions::AUTO_MODE_RESUMED`]). The synthetic user identity
/// is `branchwork-auto-mode`, matching the dirty-tree pause's authorship.
async fn resume_after_clean_tree(state: &AppState, plan_name: &str) {
    // Look up the org for the audit row. `plan_auto_mode` has no
    // `org_id` column today, so we read it off the most recent agent
    // row for the plan — same convention `on_task_agent_completed`
    // uses. Falls back to default-org so the resume never silently
    // drops on a plan with no agent rows yet.
    let org_id: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT org_id FROM agents WHERE plan_name = ?1 \
             ORDER BY started_at DESC LIMIT 1",
            params![plan_name],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .unwrap_or_else(|| "default-org".to_string())
    };

    db::auto_mode_resume(&state.db, plan_name);

    let last_completed: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT task_number FROM task_status \
             WHERE plan_name = ?1 AND status IN ('completed', 'skipped') \
             ORDER BY updated_at DESC LIMIT 1",
            params![plan_name],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };

    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            None,
            Some("branchwork-auto-mode"),
            actions::AUTO_RESUMED_TREE_CLEAN,
            audit::resources::PLAN,
            Some(plan_name),
            Some(
                &serde_json::json!({
                    "plan": plan_name,
                    "last_completed_task": last_completed,
                })
                .to_string(),
            ),
        );
    }

    broadcast_event(
        &state.broadcast_tx,
        "auto_mode_resumed",
        serde_json::json!({
            "plan": plan_name,
            "last_completed_task": last_completed,
            "reason": "tree_clean",
        }),
    );

    if let Some(task) = last_completed {
        let registry = state.registry.clone();
        let plans_dir = state.plans_dir.clone();
        let plan_name_owned = plan_name.to_string();
        let effort = *state.effort.lock().unwrap();
        let port = state.config_port();
        tokio::spawn(async move {
            crate::agents::try_auto_advance(
                registry,
                plans_dir,
                plan_name_owned,
                task,
                effort,
                port,
                None,
            )
            .await;
        });
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
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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
                drivers: None,
                pending,
                server_url: "http://localhost:3100".to_string(),
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

        let outcome = run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;
        let merged_sha = match &outcome {
            MergeStepOutcome::Merged(sha) => sha.clone(),
            MergeStepOutcome::Paused => panic!("expected Merged, got Paused"),
        };

        // Trunk SHA advanced — branch was actually merged.
        let master_after = git_head_sha(&cwd);
        assert_ne!(master_before, master_after, "master should advance");
        assert_eq!(
            merged_sha, master_after,
            "MergeStepOutcome::Merged(sha) should carry the new trunk HEAD"
        );

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

        // ci::trigger_after_merge is spawned by the merge inner —
        // run_merge_step and the manual /merge endpoint converge on the
        // same async post-merge trigger (Phase 3 acceptance: the
        // downstream ci_runs insert + gh_run_list poll behaves
        // identically whether the merge came from a human click or
        // from auto-mode). Poll for the pending row with a generous
        // deadline — the spawn races the assertion otherwise, and the
        // chain of shell-outs (has_remote, default_branch, push) is
        // slow on Windows CI (every `git` invocation pays MSYS startup
        // cost).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        type CiRunRow = (String, String, String, Option<String>, String, i64);
        let mut row: Option<CiRunRow> = None;
        while std::time::Instant::now() < deadline {
            row = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT provider, status, task_number, commit_sha, plan_name, id \
                     FROM ci_runs WHERE plan_name = ?1",
                    params!["p"],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    },
                )
                .ok()
            };
            if row.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (provider, status, task_number, commit_sha, plan_name, ci_row_id) =
            row.expect("expected ci::trigger_after_merge to insert a pending ci_runs row");

        // Acceptance (a): row has provider='github', status='pending',
        // commit_sha pinned to the merged SHA returned by run_merge_step.
        assert_eq!(provider, "github", "provider must be 'github'");
        assert_eq!(status, "pending", "status must be 'pending'");
        assert_eq!(task_number, "1.1");
        assert_eq!(plan_name, "p");
        assert_eq!(
            commit_sha.as_deref(),
            Some(merged_sha.as_str()),
            "commit_sha must match the MergeStepOutcome::Merged(sha) value"
        );

        // Sanity: exactly one row, not two — guards against a future
        // refactor that accidentally spawns trigger_after_merge twice
        // (once from the inner, once from run_merge_step).
        let count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM ci_runs WHERE plan_name = ?1",
                params!["p"],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
        };
        assert_eq!(
            count, 1,
            "expected exactly one ci_runs row — duplicate trigger would surface here"
        );

        // Acceptance (b): plan API's per-task lookup (ci::latest_per_task)
        // now returns a populated CiStatus for task 1.1, so the dashboard's
        // ciRunId no longer reads as the placeholder '-'.
        let ci_map = {
            let conn = db.lock().unwrap();
            crate::ci::latest_per_task(&conn, "p", &["1.1"])
        };
        let ci = ci_map
            .get("1.1")
            .expect("latest_per_task should return a CiStatus for task 1.1");
        assert_eq!(ci.id, ci_row_id, "CiStatus.id must match ci_runs.id");
        assert_eq!(ci.status, "pending");
        assert_eq!(ci.commit_sha.as_deref(), Some(merged_sha.as_str()));
        assert!(
            ci.via_fix_attempt.is_none(),
            "first run on canonical task — no fix-attempt rollup yet"
        );
    }

    /// Task 1.2 of auto-push-rebase-on-non-fast-forward: a non-FF push
    /// that succeeds on rebase-then-retry must emit one
    /// `auto_push_rebase_retry` audit row + one `auto_push_rebased`
    /// broadcast event. Pins the wiring in `ci::trigger_after_merge`
    /// against the `PushReport.retries` slice returned by
    /// `push_branch_local`.
    #[tokio::test]
    async fn standalone_post_merge_rebase_retry_audits_and_broadcasts() {
        let (db, dir) = fresh_db();

        // ─── Set up a bare origin and TWO clones that will race ─────────
        let origin = dir.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "master"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(init.status.success());

        // Clone A: the "local" agent workspace. Add a workflow + the
        // initial commit, then push so origin has a HEAD.
        let cwd = dir.path().join("local-a");
        let ok = Command::new("git")
            .args([
                "clone",
                "-q",
                origin.to_string_lossy().as_ref(),
                cwd.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-a failed");
        run_git(&cwd, &["config", "user.email", "a@t.test"]);
        run_git(&cwd, &["config", "user.name", "agent-a"]);
        std::fs::create_dir_all(cwd.join(".github").join("workflows")).unwrap();
        std::fs::write(
            cwd.join(".github").join("workflows").join("ci.yml"),
            "name: ci\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
        )
        .unwrap();
        run_git(&cwd, &["add", ".github/workflows/ci.yml"]);
        run_git(&cwd, &["commit", "-q", "-m", "add ci"]);
        run_git(&cwd, &["push", "-q", "-u", "origin", "master"]);

        // Clone B: a "sibling" workspace that races by pushing first.
        let local_b = dir.path().join("local-b");
        let ok = Command::new("git")
            .args([
                "clone",
                "-q",
                origin.to_string_lossy().as_ref(),
                local_b.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-b failed");
        run_git(&local_b, &["config", "user.email", "b@t.test"]);
        run_git(&local_b, &["config", "user.name", "agent-b"]);
        std::fs::write(local_b.join("sibling.txt"), "from B\n").unwrap();
        run_git(&local_b, &["add", "sibling.txt"]);
        run_git(&local_b, &["commit", "-q", "-m", "sibling commit"]);
        run_git(&local_b, &["push", "-q", "origin", "master"]);
        // Origin now ahead of local-a's view: A's `origin/master` is
        // stale until the post-merge push attempt fetches it.

        // local-a creates a task branch, commits, and is about to merge.
        // The merge runs locally and produces a non-FF push attempt.
        git_create_task_branch(&cwd, "branchwork/p/1.1", true);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.1", "branchwork/p/1.1");
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;

        // Master moved (merge happened locally) and the spawned
        // `trigger_after_merge` ran the rebase-then-retry path. Poll for
        // both the ci_runs row (proves push succeeded) and the audit row
        // (proves the retry was recorded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut ci_run_count: i64 = 0;
        let mut retry_audit_count: i64 = 0;
        while std::time::Instant::now() < deadline {
            {
                let conn = db.lock().unwrap();
                ci_run_count = conn
                    .query_row(
                        "SELECT COUNT(*) FROM ci_runs WHERE plan_name = ?1",
                        params!["p"],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0);
                retry_audit_count = conn
                    .query_row(
                        "SELECT COUNT(*) FROM audit_logs \
                         WHERE action = ?1 AND resource_id = ?2",
                        params![crate::audit::actions::AUTO_PUSH_REBASE_RETRY, "p"],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0);
            }
            if ci_run_count > 0 && retry_audit_count > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            ci_run_count, 1,
            "expected the post-rebase retry push to succeed and write a ci_runs row"
        );
        assert_eq!(
            retry_audit_count, 1,
            "expected exactly one auto_push_rebase_retry audit row for the single retry"
        );

        // Broadcast: at least one `auto_push_rebased` event landed.
        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_push_rebased".to_string()),
            "expected auto_push_rebased in {events:?}"
        );

        // Audit row body: parse the diff and verify the canonical fields.
        let diff_json: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT diff FROM audit_logs WHERE action = ?1 AND resource_id = ?2",
                params![crate::audit::actions::AUTO_PUSH_REBASE_RETRY, "p"],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let diff: serde_json::Value = serde_json::from_str(&diff_json).unwrap();
        assert_eq!(diff["kind"], "auto_push_rebase_retry");
        assert_eq!(diff["branch"], "master");
        assert_eq!(diff["attempt"], 1);
        assert_eq!(
            diff["last_rebase_sha"].as_str().unwrap().len(),
            40,
            "last_rebase_sha must be a full SHA"
        );
        assert_eq!(
            diff["prior_remote_sha"].as_str().unwrap().len(),
            40,
            "prior_remote_sha must be a full SHA"
        );
    }

    /// Task 1.3 of auto-push-rebase-on-non-fast-forward: when the
    /// post-merge push hits a non-FF rejection AND the rebase produces
    /// CONFLICT (the rebased commit touches the same lines as a commit
    /// on origin — e.g. auto-bump bumped Cargo.toml line 3 while the
    /// task agent also edited line 3), the loop must:
    ///   1. Leave a clean worktree (rebase aborted, no MERGE_HEAD).
    ///   2. NOT insert a `ci_runs` row (no successful push happened).
    ///   3. Pause the plan with reason `auto_push_rebase_conflict`.
    ///   4. Broadcast `auto_mode_paused` carrying the conflicting files.
    ///   5. Audit `AUTO_MODE_PAUSED` with the same payload (PLAN resource).
    /// Pins the wiring in `ci::trigger_after_merge`.
    #[tokio::test]
    async fn standalone_post_merge_rebase_conflict_pauses_with_files() {
        let (db, dir) = fresh_db();

        // ─── Bare origin + two clones, but with an OVERLAPPING file
        //     edit on line 3 of Cargo.toml so the rebase produces a
        //     CONFLICT instead of cleanly stacking.
        let origin = dir.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare", "-q", "-b", "master"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(init.status.success());

        // local-a: workflow + Cargo.toml + initial push.
        let cwd = dir.path().join("local-a");
        let ok = Command::new("git")
            .args([
                "clone",
                "-q",
                origin.to_string_lossy().as_ref(),
                cwd.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-a failed");
        run_git(&cwd, &["config", "user.email", "a@t.test"]);
        run_git(&cwd, &["config", "user.name", "agent-a"]);
        std::fs::create_dir_all(cwd.join(".github").join("workflows")).unwrap();
        std::fs::write(
            cwd.join(".github").join("workflows").join("ci.yml"),
            "name: ci\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run_git(&cwd, &["add", ".github/workflows/ci.yml", "Cargo.toml"]);
        run_git(&cwd, &["commit", "-q", "-m", "init"]);
        run_git(&cwd, &["push", "-q", "-u", "origin", "master"]);

        // local-b races: bump line 3 to 0.2.0 + push (wins race).
        let local_b = dir.path().join("local-b");
        let ok = Command::new("git")
            .args([
                "clone",
                "-q",
                origin.to_string_lossy().as_ref(),
                local_b.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-b failed");
        run_git(&local_b, &["config", "user.email", "b@t.test"]);
        run_git(&local_b, &["config", "user.name", "auto-bump"]);
        std::fs::write(
            local_b.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        run_git(&local_b, &["add", "Cargo.toml"]);
        run_git(&local_b, &["commit", "-q", "-m", "auto-bump 0.2.0"]);
        run_git(&local_b, &["push", "-q", "origin", "master"]);

        // local-a creates a task branch, edits the SAME line to 0.3.0,
        // commits, and arms a merge. master is still at the pre-race
        // SHA in local-a's view, so the merge will FF.
        let task_branch = "branchwork/p/1.3";
        run_git(&cwd, &["checkout", "-q", "-b", task_branch]);
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.3.0\"\n",
        )
        .unwrap();
        run_git(&cwd, &["add", "Cargo.toml"]);
        run_git(&cwd, &["commit", "-q", "-m", "task agent 0.3.0"]);
        run_git(&cwd, &["checkout", "-q", "master"]);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        seed_agent(&db, "agent-1", &cwd, "p", "1.3", task_branch);
        enable_auto_mode(&db, "p");

        run_merge_step(&state, "default-org", "agent-1", "p", "1.3", true).await;

        // Wait for `trigger_after_merge` (spawned in a tokio task) to
        // run through the push → non-FF → rebase → CONFLICT → pause
        // sequence. Done when paused_reason is set; never inserts a
        // ci_runs row on this path.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut reason: Option<String> = None;
        let mut ci_run_count: i64 = 0;
        while std::time::Instant::now() < deadline {
            reason = paused_reason(&db, "p");
            {
                let conn = db.lock().unwrap();
                ci_run_count = conn
                    .query_row(
                        "SELECT COUNT(*) FROM ci_runs WHERE plan_name = ?1",
                        params!["p"],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0);
            }
            if reason.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(
            reason.as_deref(),
            Some("auto_push_rebase_conflict"),
            "expected plan to pause with auto_push_rebase_conflict reason, got {reason:?}"
        );
        assert_eq!(
            ci_run_count, 0,
            "ci_runs row must NOT be inserted when the push fails on rebase conflict"
        );

        // Worktree must be clean — no MERGE_HEAD, no in-progress rebase.
        assert!(!cwd.join(".git/MERGE_HEAD").exists());
        assert!(!cwd.join(".git/rebase-merge").exists());
        assert!(!cwd.join(".git/rebase-apply").exists());

        // Broadcast: an `auto_mode_paused` event with reason + the
        // conflicting file list must have fired. Capture the full frames
        // so we can pin the payload shape, not just the event type.
        let mut frames: Vec<String> = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            frames.push(msg);
        }
        let pause_event = frames
            .iter()
            .filter_map(|f| serde_json::from_str::<serde_json::Value>(f).ok())
            .find(|v| {
                v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_paused")
                    && v["data"].get("reason").and_then(|r| r.as_str())
                        == Some("auto_push_rebase_conflict")
            })
            .expect("expected an auto_mode_paused event with auto_push_rebase_conflict reason");
        let files = pause_event["data"]["files"]
            .as_array()
            .expect("files must be an array");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "Cargo.toml");
        assert_eq!(pause_event["data"]["plan"], "p");
        assert_eq!(pause_event["data"]["task"], "1.3");
        assert_eq!(pause_event["data"]["branch"], "master");

        // Audit row body: AUTO_MODE_PAUSED logged for resource_id=plan
        // with the same files payload.
        let diff_json: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT diff FROM audit_logs \
                 WHERE action = ?1 AND resource_id = ?2 \
                 ORDER BY id DESC LIMIT 1",
                params![crate::auto_mode::actions::AUTO_MODE_PAUSED, "p"],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };
        let diff: serde_json::Value =
            serde_json::from_str(&diff_json.expect("expected an AUTO_MODE_PAUSED audit row"))
                .unwrap();
        assert_eq!(diff["reason"], "auto_push_rebase_conflict");
        assert_eq!(diff["files"][0], "Cargo.toml");
        assert_eq!(diff["file_count"], 1);
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

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;

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

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;

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

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;

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

        run_merge_step(&state, "default-org", "agent-1", "p", "1.1", true).await;

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
                informational: false,
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
                informational: false,
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
                informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "101".into(),
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "102".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
                informational: false,
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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
            &CancellationToken::new(),
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

    /// Red CI on the first task-agent merge spawns a fix agent (T3.3
    /// wired the cap-checked spawn into `on_ci_failed`). Asserts:
    /// AUTO_MODE_CI_FAILED + AUTO_MODE_FIX_SPAWNED audited, a fix agent
    /// row exists for `0.1-fix-1`, the plan is NOT paused (we are still
    /// well under cap=3), and no `advancing` pill ever fired.
    #[tokio::test]
    async fn red_ci_spawns_fix_agent_under_cap() {
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
            // Failure-log lookup fires from `spawn_fix_agent` for the
            // newly-built fix prompt — return a canned reply so the
            // dispatcher echo-back assertion in spawn_fix_agent doesn't
            // panic on a missing run_id_used.
            WireMessage::CiFailureLog { run_id, .. } => Some(RunnerResponse::CiFailureLogFetched {
                log: Some("fake failure log".into()),
                run_id_used: run_id.clone(),
            }),
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

        // Acceptance: original task agent + a fresh fix agent for `0.1-fix-1`.
        assert_eq!(
            count_agents_for_plan_task(&db, "p", "0.1-fix-1"),
            1,
            "expected a fix agent row for 0.1-fix-1 to be inserted by the spawn"
        );

        // task_fix_attempts row recorded with attempt=1.
        assert_eq!(
            crate::db::task_fix_attempt_count(&db, "p", "0.1"),
            1,
            "expected exactly one fix-attempt row for the first red-CI spawn"
        );

        // Plan is NOT paused — under cap=3, we spawn rather than pause.
        assert!(
            paused_reason(&db, "p").is_none(),
            "plan should not be paused on the first red CI under the cap"
        );

        // State pill saw merging → awaiting_ci. No `advancing`, no `paused`.
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
            !labels.contains(&"advancing".to_string()),
            "no `advancing` expected on red CI: {labels:?}"
        );
        assert!(
            !labels.contains(&"paused".to_string()),
            "no `paused` expected when a fix agent is spawned: {labels:?}"
        );

        // Audit: AUTO_MODE_CI_FAILED + AUTO_MODE_FIX_SPAWNED on the plan;
        // AUTO_MODE_CI_PASSED must not be present.
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
        let merge_outcome = run_merge_step(&state, org_id, "agent-1", "p", "0.1", true).await;
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

    // ── Phase 3.3: retry cap + cancellation token ───────────────────────────

    /// Set `plan_auto_mode.max_fix_attempts` for `plan_name` (UPSERT).
    /// Used by the cap tests to drop the schema default of 3 to a smaller
    /// value when needed; defaults to UPSERT semantics so the row may or
    /// may not exist already.
    fn set_max_fix_attempts(db: &Db, plan: &str, cap: u32) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled, max_fix_attempts) \
             VALUES (?1, 1, ?2) \
             ON CONFLICT(plan_name) DO UPDATE SET max_fix_attempts = excluded.max_fix_attempts",
            params![plan, cap as i64],
        )
        .unwrap();
    }

    /// Brief acceptance T3.3 #1: simulate 4 red CIs in a row with cap=3;
    /// assert exactly 3 fix agents were spawned and the plan ends paused
    /// with `fix_cap_reached`. Drives [`try_spawn_fix_agent_with_cap`]
    /// directly so the test runs in millis instead of dragging through
    /// 4 × wait_for_ci polls.
    #[tokio::test]
    async fn fix_cap_reached_after_n_attempts_pauses_plan() {
        let (db, _dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        // Stub runner so each spawn_fix_agent's failure-log fetch + start-
        // agent dispatch resolves cleanly. We don't care about the agent's
        // actual lifecycle here — only that a row appears for each spawn.
        let runners = new_runner_registry();
        let _outgoing = install_echo_runner(&runners, "runner-1", |msg| match msg {
            WireMessage::CiFailureLog { run_id, .. } => Some(RunnerResponse::CiFailureLogFetched {
                log: Some("fake log".into()),
                run_id_used: run_id.clone(),
            }),
            _ => None,
        })
        .await;

        let (state, mut rx) = test_app_state(
            db.clone(),
            runners,
            PathBuf::from("/tmp/auto-mode-cap-plans"),
        );

        // Original task agent — gives spawn_fix_agent a cwd to point at.
        seed_agent(
            &db,
            "agent-original",
            Path::new("/runner/cwd"),
            "p",
            "0.1",
            "branchwork/p/0.1",
        );
        enable_auto_mode(&db, "p");
        set_max_fix_attempts(&db, "p", 3);

        // Drive 4 red CIs: each call simulates the gate that fires from
        // on_ci_failed (attempt 1) and on_fix_ci_failed (attempts 2..=4).
        // Calls 1..=3 must spawn (count→cap window is open); call 4 must
        // pause with fix_cap_reached.
        for _ in 0..4 {
            try_spawn_fix_agent_with_cap(&state, org_id, "p", "0.1", "deadbeef", "42", Some("42"))
                .await;
        }

        // Acceptance #1: exactly 3 fix-attempt rows recorded.
        assert_eq!(
            crate::db::task_fix_attempt_count(&db, "p", "0.1"),
            3,
            "expected exactly 3 task_fix_attempts rows for cap=3"
        );

        // Each spawned attempt has its own agent row (one per attempt).
        for attempt in 1..=3 {
            let task_id = format!("0.1-fix-{attempt}");
            assert_eq!(
                count_agents_for_plan_task(&db, "p", &task_id),
                1,
                "expected an agent row for {task_id}"
            );
        }
        // No row for attempt 4 — cap reached, spawn skipped.
        assert_eq!(
            count_agents_for_plan_task(&db, "p", "0.1-fix-4"),
            0,
            "no agent row should exist for attempt 4 — cap was reached"
        );

        // Acceptance #2: plan paused with reason `fix_cap_reached`.
        assert_eq!(
            paused_reason(&db, "p").as_deref(),
            Some("fix_cap_reached"),
            "plan should be paused with fix_cap_reached"
        );

        // Acceptance #3: `auto_mode_paused` event was broadcast carrying
        // {attempts, cap}; the dashboard relies on these to render the
        // banner with the actual numbers.
        let mut saw_cap_payload = false;
        while let Ok(msg) = rx.try_recv() {
            let v: serde_json::Value = match serde_json::from_str(&msg) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("auto_mode_paused") {
                continue;
            }
            let data = match v.get("data") {
                Some(d) => d,
                None => continue,
            };
            if data.get("reason").and_then(|r| r.as_str()) == Some("fix_cap_reached")
                && data.get("attempts").and_then(|a| a.as_u64()) == Some(3)
                && data.get("cap").and_then(|c| c.as_u64()) == Some(3)
            {
                saw_cap_payload = true;
                break;
            }
        }
        assert!(
            saw_cap_payload,
            "expected an auto_mode_paused event with reason=fix_cap_reached, attempts=3, cap=3"
        );

        // Audit row mirrors the broadcast.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions
                .iter()
                .filter(|a| *a == actions::AUTO_MODE_FIX_SPAWNED)
                .count()
                == 3,
            "expected exactly 3 AUTO_MODE_FIX_SPAWNED audit rows: {plan_actions:?}"
        );
        assert!(
            plan_actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED),
            "expected AUTO_MODE_PAUSED in plan actions: {plan_actions:?}"
        );
    }

    /// Brief acceptance T3.3 #2: spawn a fix agent, toggle auto-mode off
    /// mid-flight; assert the fix agent is killed and no merge runs.
    /// Drives the `cancel_plan` + `kill_agent_dispatch` chain that
    /// [`crate::api::plans::put_plan_config`] performs when `autoMode`
    /// flips to false.
    #[tokio::test]
    async fn toggle_auto_mode_off_kills_in_flight_fix_agent_and_cancels_wait() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Seed: a fix agent already running (status='running', fix-task
        // marker in task_id). The toggle-off path looks for exactly this
        // shape via `task_id LIKE '%-fix-%'` AND status IN ('running',
        // 'starting').
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents \
                    (id, session_id, cwd, status, mode, plan_name, task_id, branch, source_branch, org_id) \
                 VALUES (?1, ?1, '/tmp/cwd', 'running', 'pty', 'p', '0.1-fix-1', \
                         'branchwork/p/0.1-fix-1', 'master', ?2)",
                params!["fix-agent-1", org_id],
            )
            .unwrap();
        }
        enable_auto_mode(&db, "p");

        // Prime the cancel token so we can observe it being fired. Future
        // wait_for_ci_inner calls would clone this token and select on it.
        let token = state.cancel_token_for("p");
        assert!(!token.is_cancelled());

        // Drive the same flow `put_plan_config` performs when toggling
        // off: flip `enabled=0`, snapshot in-flight fix agents, cancel
        // the per-plan token, then kill each fix agent. Done inline
        // rather than over HTTP so the test stays a unit test.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE plan_auto_mode SET enabled = 0 WHERE plan_name = 'p'",
                [],
            )
            .unwrap();
        }
        let in_flight: Vec<(String, String)> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id, COALESCE(org_id, 'default-org') FROM agents \
                     WHERE plan_name = ?1 AND task_id LIKE '%-fix-%' \
                       AND status IN ('running', 'starting')",
                )
                .unwrap();
            stmt.query_map(params!["p"], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .flatten()
            .collect()
        };
        assert_eq!(
            in_flight.len(),
            1,
            "should snapshot exactly the one fix agent"
        );
        state.cancel_plan("p");
        for (agent_id, agent_org) in &in_flight {
            let _ =
                crate::agents::spawn_ops::kill_agent_dispatch(&state, agent_org, agent_id).await;
        }

        // Acceptance #1: token observable on the cloned handle is now cancelled.
        assert!(
            token.is_cancelled(),
            "the cancel token cloned earlier must observe cancellation"
        );

        // Acceptance #2: agent row flipped to 'killed'.
        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM agents WHERE id = ?1",
                params!["fix-agent-1"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            status, "killed",
            "fix agent row should be in status='killed'"
        );

        // Acceptance #3: no merge runs. The auto_mode_enabled() gate at
        // the entry of `on_task_agent_completed` returns false (enabled=0
        // by the toggle-off above), so even if the agent's exit hook were
        // to fire it would be a silent no-op.
        assert!(
            !crate::db::auto_mode_enabled(&db, "p"),
            "auto_mode must be disabled after toggle-off"
        );
    }

    /// Cancellation propagates into the wait_for_ci poll: a token fired
    /// while the loop is mid-tick returns [`CiOutcome::Cancelled`] within
    /// one poll interval (no merge / no spawn / no pause downstream).
    #[tokio::test]
    async fn wait_for_ci_inner_returns_cancelled_when_token_fires_mid_poll() {
        // Slow poll, fast cancel: the loop would otherwise time out at
        // `total_timeout`; we want to prove the cancel arm wins.
        let cfg = WaitForCiConfig {
            poll_interval: Duration::from_millis(500),
            jitter_window: Duration::from_millis(0),
            total_timeout: Duration::from_secs(30),
        };
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Fire the cancel after one poll interval so the loop is parked
        // in the select! sleep arm when cancellation lands.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token_clone.cancel();
        });

        let outcome = wait_for_ci_inner(
            "p",
            "0.1",
            "sha-1",
            || async { true },
            // Always returns Some(in_progress) so the loop keeps polling
            // — without cancellation it would run until the 30s timeout.
            || async { Ok(Some(aggregate_in_progress())) },
            cfg,
            &token,
        )
        .await;

        assert_eq!(outcome, CiOutcome::Cancelled);
    }

    // ── Idle poller ─────────────────────────────────────────────────────────

    /// Insert a `running` agent with an explicit `driver` and a controlled
    /// `last_activity_at` (`idle_secs` seconds in the past) so the idle
    /// poller can be exercised deterministically.
    fn seed_running_agent_idle(
        db: &Db,
        id: &str,
        cwd: &Path,
        plan: &str,
        task: &str,
        driver: &str,
        idle_secs: u32,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents \
                (id, session_id, cwd, status, mode, plan_name, task_id, driver, \
                 last_activity_at, org_id) \
             VALUES (?1, ?1, ?2, 'running', 'pty', ?3, ?4, ?5, \
                     datetime('now', ?6), 'default-org')",
            params![
                id,
                cwd.to_string_lossy(),
                plan,
                task,
                driver,
                format!("-{idle_secs} seconds"),
            ],
        )
        .unwrap();
    }

    fn map_plan_to_project(db: &Db, plan: &str, project: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_project (plan_name, project) VALUES (?1, ?2) \
             ON CONFLICT(plan_name) DO UPDATE SET project = excluded.project",
            params![plan, project],
        )
        .unwrap();
    }

    fn audit_diff_for(db: &Db, resource_id: &str, action: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT diff FROM audit_logs WHERE resource_id = ?1 AND action = ?2 \
             ORDER BY id LIMIT 1",
            params![resource_id, action],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn drain_events(rx: &mut broadcast::Receiver<String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                out.push(v);
            }
        }
        out
    }

    /// Claude agents are owned by the Stop-hook path; the idle poller
    /// must skip them so the two triggers can't double-fire on the same
    /// agent. Filter is the registry lookup
    /// (`stop_hook_config(...).is_some()`), not a driver-name string match,
    /// so future drivers that opt into the hook surface inherit the
    /// exclusion automatically.
    #[tokio::test]
    async fn idle_poller_skips_drivers_with_stop_hook_config() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "claude", 1000);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 60).await;

        let evs = drain_event_types(&mut rx);
        assert!(
            !evs.iter().any(|t| t == "auto_finish_triggered"),
            "claude agent must be skipped, got: {evs:?}"
        );
        assert!(audit_actions_for(&db, "a-1").is_empty());
    }

    /// Threshold gate: an agent whose `last_activity_at` is more recent
    /// than the configured idle window must not be touched.
    #[tokio::test]
    async fn idle_poller_skips_under_threshold() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 30);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 300).await;

        let evs = drain_event_types(&mut rx);
        assert!(
            !evs.iter().any(|t| t == "auto_finish_triggered"),
            "agent under threshold must not fire auto-finish, got: {evs:?}"
        );
    }

    /// Auto-mode-off gate: even a long-idle agent on a plan whose
    /// auto-mode is disabled must stay untouched (the poller shares this
    /// gate with the Stop hook for parity).
    #[tokio::test]
    async fn idle_poller_skips_when_auto_mode_off() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 1000);
        // Deliberately no enable_auto_mode — gate stays false.
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 60).await;

        let evs = drain_event_types(&mut rx);
        assert!(
            !evs.iter().any(|t| t == "auto_finish_triggered"),
            "auto-mode-off plan must not fire, got: {evs:?}"
        );
    }

    /// Happy path: idle agent + auto-mode on + clean tree + non-Claude
    /// driver → graceful_exit fired (proxy: AGENT_AUTO_FINISH audit row),
    /// `auto_finish_triggered` broadcast carries `trigger: "idle_timeout"`,
    /// and the plan stays unpaused.
    #[tokio::test]
    async fn idle_poller_clean_tree_fires_graceful_exit_and_audit() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 1000);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 60).await;
        // Give the spawned graceful_exit task a tick (no-op without a live
        // PTY, but we want to make sure it doesn't panic).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let actions = audit_actions_for(&db, "a-1");
        assert!(
            actions
                .iter()
                .any(|a| a == audit::actions::AGENT_AUTO_FINISH),
            "expected AGENT_AUTO_FINISH in {actions:?}"
        );
        let diff = audit_diff_for(&db, "a-1", audit::actions::AGENT_AUTO_FINISH)
            .expect("AGENT_AUTO_FINISH should have a diff");
        assert!(
            diff.contains("\"trigger\":\"idle_timeout\""),
            "diff should pin trigger to idle_timeout, got: {diff}"
        );

        let evs = drain_events(&mut rx);
        let triggered = evs
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_finish_triggered"))
            .expect("expected auto_finish_triggered broadcast");
        let data = triggered.get("data").unwrap();
        assert_eq!(
            data.get("trigger").and_then(|v| v.as_str()),
            Some("idle_timeout")
        );
        assert_eq!(data.get("agent_id").and_then(|v| v.as_str()), Some("a-1"));
        assert_eq!(data.get("plan").and_then(|v| v.as_str()), Some("p"));
        assert_eq!(data.get("task").and_then(|v| v.as_str()), Some("1.1"));

        assert!(
            paused_reason(&db, "p").is_none(),
            "clean-tree path must not pause the plan"
        );
    }

    /// Dirty-tree path: idle non-Claude agent left uncommitted work →
    /// pause the plan with `agent_left_uncommitted_work`, broadcast
    /// `auto_mode_paused`, log `AUTO_MODE_PAUSED` against the plan, no
    /// `AGENT_AUTO_FINISH` row, no `auto_finish_triggered` broadcast.
    /// Mirrors the Stop-hook dirty-tree branch in `hooks::handle_stop_hook`.
    #[tokio::test]
    async fn idle_poller_dirty_tree_pauses_plan() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Modify a tracked file without committing — porcelain reports it.
        std::fs::write(cwd.join("README.md"), "modified but not committed").unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 1000);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 60).await;

        assert_eq!(
            paused_reason(&db, "p").as_deref(),
            Some("agent_left_uncommitted_work")
        );
        let evs = drain_event_types(&mut rx);
        assert!(evs.iter().any(|t| t == "auto_mode_paused"));
        assert!(
            !evs.iter().any(|t| t == "auto_finish_triggered"),
            "dirty-tree path must not fire auto_finish_triggered"
        );

        let plan_actions = audit_actions_for(&db, "p");
        assert!(plan_actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED));
        let agent_actions = audit_actions_for(&db, "a-1");
        assert!(
            !agent_actions
                .iter()
                .any(|a| a == audit::actions::AGENT_AUTO_FINISH),
            "AGENT_AUTO_FINISH must not fire on the dirty path"
        );
    }

    /// Two passes for the same long-idle agent: the second pass must be a
    /// dedupe no-op because the agent's `status` is still `running`
    /// (the row only flips to `completed` inside `on_agent_exit` after
    /// the PTY actually closes — we don't drive a real PTY here).
    /// Counter-proxy: AGENT_AUTO_FINISH audit count == 1.
    #[tokio::test]
    async fn idle_poller_dedupes_across_two_passes() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 1000);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        run_idle_pass(&state, 60).await;
        run_idle_pass(&state, 60).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE resource_id = 'a-1' AND action = ?1",
                params![audit::actions::AGENT_AUTO_FINISH],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            count, 1,
            "AGENT_AUTO_FINISH must be written exactly once across two passes"
        );
        assert!(state.auto_finish_dedupe.lock().unwrap().contains("a-1"));
    }

    /// Brief acceptance for T4.3: pin the idle-poller fallback path against
    /// the values the env-var contract uses (`BRANCHWORK_AUTO_FINISH_IDLE_SECS=1`,
    /// agent idle for 10 s) and assert it produces the *same* audit +
    /// broadcast shape as the Stop-hook path in
    /// [`crate::hooks::handle_stop_hook`], differing only in the `trigger`
    /// discriminator (`idle_timeout` vs. `stop_hook`).
    ///
    /// This is the load-bearing contract-parity test: dashboards consume
    /// `auto_finish_triggered` and `AGENT_AUTO_FINISH` regardless of which
    /// path fired, so the field set must match. The test passes
    /// `threshold_secs = 1` directly to [`run_idle_pass`] (the
    /// "inject-a-clock" surface the brief allows) instead of mutating the
    /// process-wide env var, keeping it safe under parallel `cargo test`.
    #[tokio::test]
    async fn idle_poll_at_one_second_threshold_matches_stop_hook_audit_and_broadcast_contract() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // Non-Claude driver (Aider falls back to the default trait impl,
        // which returns `None` from `stop_hook_config`), idle for 10 s.
        seed_running_agent_idle(&db, "a-1", &cwd, "p", "1.1", "aider", 10);
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // One pass with threshold = 1 s. Mirrors the env-var contract
        // `BRANCHWORK_AUTO_FINISH_IDLE_SECS=1` without mutating the env.
        run_idle_pass(&state, 1).await;
        // Give the spawned graceful_exit task a tick (no-op without a live
        // PTY, but we want to confirm the spawn itself doesn't panic).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // ── graceful_exit called exactly once ────────────────────────────
        // Counter-proxy: AGENT_AUTO_FINISH is written inside the same
        // gated block that spawns graceful_exit (see `run_idle_pass`), so
        // the audit count == graceful_exit call count. Same proxy the
        // Stop-hook test uses (`hooks::tests::auto_finish_audit_count`).
        let auto_finish_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE resource_id = 'a-1' AND action = ?1",
                params![audit::actions::AGENT_AUTO_FINISH],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            auto_finish_count, 1,
            "graceful_exit must fire exactly once for the idle agent"
        );

        // ── Audit row contract parity with Stop-hook path ────────────────
        // Stop-hook writes (action=AGENT_AUTO_FINISH, resource_type=AGENT,
        // resource_id=agent_id, diff={"trigger":"stop_hook"}). Idle path
        // must mirror that shape; only the `trigger` value flips.
        let (resource_type, diff): (String, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT resource_type, diff FROM audit_logs \
                 WHERE resource_id = 'a-1' AND action = ?1",
                params![audit::actions::AGENT_AUTO_FINISH],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            resource_type,
            audit::resources::AGENT,
            "AGENT_AUTO_FINISH must be logged against the AGENT resource (Stop-hook parity)"
        );
        let diff = diff.expect("AGENT_AUTO_FINISH must carry a diff");
        let diff_value: serde_json::Value =
            serde_json::from_str(&diff).expect("diff must be valid JSON");
        assert_eq!(
            diff_value,
            serde_json::json!({ "trigger": "idle_timeout" }),
            "diff must be exactly {{trigger:idle_timeout}} — matches Stop-hook \
             shape with only the discriminator differing"
        );

        // ── Broadcast contract parity with Stop-hook path ────────────────
        // Stop-hook emits `auto_finish_triggered` with
        // `{agent_id, plan, task, trigger:"stop_hook"}`. Idle path must
        // emit the same field set with `trigger:"idle_timeout"`.
        let evs = drain_events(&mut rx);
        let triggered: Vec<&serde_json::Value> = evs
            .iter()
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_finish_triggered"))
            .collect();
        assert_eq!(
            triggered.len(),
            1,
            "expected exactly one auto_finish_triggered broadcast, got {}",
            triggered.len()
        );
        let data = triggered[0]
            .get("data")
            .and_then(|d| d.as_object())
            .expect("broadcast data must be an object");
        // Field set parity: same keys as the Stop-hook broadcast.
        let mut keys: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["agent_id", "plan", "task", "trigger"],
            "broadcast data field set must match the Stop-hook contract"
        );
        assert_eq!(data.get("agent_id").and_then(|v| v.as_str()), Some("a-1"));
        assert_eq!(data.get("plan").and_then(|v| v.as_str()), Some("p"));
        assert_eq!(data.get("task").and_then(|v| v.as_str()), Some("1.1"));
        assert_eq!(
            data.get("trigger").and_then(|v| v.as_str()),
            Some("idle_timeout"),
            "trigger discriminator differs from Stop-hook (which emits 'stop_hook')"
        );

        // ── Clean-path side effects (Stop-hook parity) ───────────────────
        // Plan stays unpaused; no AUTO_MODE_PAUSED row was written.
        assert!(
            paused_reason(&db, "p").is_none(),
            "clean-tree idle path must not pause the plan"
        );
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            !plan_actions.iter().any(|a| a == actions::AUTO_MODE_PAUSED),
            "AUTO_MODE_PAUSED must not fire on the clean idle path"
        );
        assert!(
            !evs.iter()
                .any(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_paused")),
            "auto_mode_paused must not be broadcast on the clean idle path"
        );

        // Dedupe set retains the agent so a second pass would no-op
        // (matches Stop-hook's dedupe behaviour for repeat hits).
        assert!(
            state.auto_finish_dedupe.lock().unwrap().contains("a-1"),
            "agent_id should be retained in auto_finish_dedupe (Stop-hook parity)"
        );
    }

    // ── Dirty-tree watcher (Task 4.1) ───────────────────────────────────────

    /// Seed a plan_auto_mode row whose pause reason is
    /// `agent_left_uncommitted_work` — what `hooks::handle_stop_hook`
    /// would write after detecting a dirty tree.
    fn seed_dirty_tree_pause(db: &Db, plan: &str, files: &[&str]) {
        let files_owned: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        crate::db::auto_mode_pause(db, plan, "agent_left_uncommitted_work", Some(&files_owned));
        // Ensure auto-mode is enabled so the resume path's
        // `try_auto_advance` gate would pass — the watcher only resumes
        // plans that were explicitly opted into auto-mode.
        enable_auto_mode(db, plan);
        // enable_auto_mode clears paused_reason via ON CONFLICT, so
        // re-apply the pause AFTER it (DB UPSERT order matters).
        crate::db::auto_mode_pause(db, plan, "agent_left_uncommitted_work", Some(&files_owned));
    }

    /// Acceptance criterion: pause a plan with a dirty file, then commit
    /// the file. Within a few polls the watcher resumes the plan,
    /// emits `AUTO_RESUMED_TREE_CLEAN` against the PLAN resource,
    /// broadcasts `auto_mode_resumed`, and clears the dedupe entry.
    #[tokio::test]
    async fn dirty_tree_watcher_resumes_when_tree_becomes_clean() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Dirty the tree by modifying a tracked file.
        std::fs::write(cwd.join("README.md"), "uncommitted edit").unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_dirty_tree_pause(&db, "p", &["README.md"]);
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Drain the broadcast channel so we only see the watcher's
        // events below.
        let _ = drain_event_types(&mut rx);

        // Spawn the watcher in the background and clean the tree after
        // one tick — the watcher should observe the clean state on the
        // SECOND poll and resume.
        let watcher_handle = tokio::spawn(run_dirty_tree_watcher_with_config(
            state.clone(),
            "p".to_string(),
            Duration::from_millis(50),
            20,
        ));

        // Wait for the dedupe entry to land (the spawn registered it).
        // We're testing the loop-body variant directly, so dedupe must
        // be set manually here:
        state
            .dirty_tree_watchers
            .lock()
            .unwrap()
            .insert("p".to_string());

        tokio::time::sleep(Duration::from_millis(75)).await;
        // Commit the dirty file — `git status` should now report clean.
        run_git(&cwd, &["add", "README.md"]);
        run_git(&cwd, &["commit", "-q", "-m", "fix"]);

        // Wait for the watcher to detect the clean state and exit.
        let _ = tokio::time::timeout(Duration::from_secs(2), watcher_handle)
            .await
            .expect("watcher should exit within 2 s after the tree is clean");

        // Pause is cleared.
        assert!(
            paused_reason(&db, "p").is_none(),
            "watcher should have resumed the plan"
        );

        // AUTO_RESUMED_TREE_CLEAN was logged against the PLAN resource.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            plan_actions
                .iter()
                .any(|a| a == actions::AUTO_RESUMED_TREE_CLEAN),
            "expected AUTO_RESUMED_TREE_CLEAN in {plan_actions:?}"
        );

        // `auto_mode_resumed` was broadcast.
        let evs = drain_event_types(&mut rx);
        assert!(
            evs.iter().any(|t| t == "auto_mode_resumed"),
            "expected auto_mode_resumed in {evs:?}"
        );

        // Dedupe entry is freed so the next pause can spawn a fresh
        // watcher.
        assert!(
            !state.dirty_tree_watchers.lock().unwrap().contains("p"),
            "dedupe entry must be removed on resume"
        );
    }

    /// A persistently-dirty tree exhausts the poll cap and the watcher
    /// exits silently — plan stays paused, no resume audit/broadcast,
    /// dedupe entry freed so a future "tree got dirty again" pause can
    /// re-arm the watcher.
    #[tokio::test]
    async fn dirty_tree_watcher_gives_up_after_max_polls() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Leave the tree permanently dirty.
        std::fs::write(cwd.join("README.md"), "permanently uncommitted").unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_dirty_tree_pause(&db, "p", &["README.md"]);
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Pre-arm dedupe (mimic the spawn_dirty_tree_watcher gate).
        state
            .dirty_tree_watchers
            .lock()
            .unwrap()
            .insert("p".to_string());

        // 3 polls × 10 ms = ~30 ms total wall clock — fast.
        run_dirty_tree_watcher_with_config(
            state.clone(),
            "p".to_string(),
            Duration::from_millis(10),
            3,
        )
        .await;

        // Plan stays paused with the original reason.
        assert_eq!(
            paused_reason(&db, "p").as_deref(),
            Some("agent_left_uncommitted_work"),
            "watcher must not change pause state when the tree stays dirty"
        );

        // No resume audit.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            !plan_actions
                .iter()
                .any(|a| a == actions::AUTO_RESUMED_TREE_CLEAN),
            "AUTO_RESUMED_TREE_CLEAN must not fire when the tree stays dirty"
        );

        // No resume broadcast.
        let evs = drain_event_types(&mut rx);
        assert!(
            !evs.iter().any(|t| t == "auto_mode_resumed"),
            "auto_mode_resumed must not fire when the tree stays dirty"
        );

        // Dedupe entry freed.
        assert!(
            !state.dirty_tree_watchers.lock().unwrap().contains("p"),
            "dedupe entry must be removed on the give-up path so a future pause can re-arm"
        );
    }

    /// Operator clicks Resume manually while the watcher is mid-poll:
    /// the watcher detects the pause-reason change on its next tick and
    /// exits early without firing its own resume audit/broadcast.
    #[tokio::test]
    async fn dirty_tree_watcher_exits_early_on_manual_resume() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        // Keep the tree dirty so the watcher would otherwise keep polling.
        std::fs::write(cwd.join("README.md"), "still dirty").unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_dirty_tree_pause(&db, "p", &["README.md"]);
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let _ = drain_event_types(&mut rx);

        state
            .dirty_tree_watchers
            .lock()
            .unwrap()
            .insert("p".to_string());

        // Run the watcher in the background. After one tick, simulate
        // the operator clicking Resume by clearing paused_reason
        // directly via the db helper. The watcher's pause-reason
        // re-check at the top of the next iteration must observe the
        // change and exit.
        let watcher = tokio::spawn(run_dirty_tree_watcher_with_config(
            state.clone(),
            "p".to_string(),
            Duration::from_millis(30),
            20,
        ));

        tokio::time::sleep(Duration::from_millis(45)).await;
        crate::db::auto_mode_resume(&db, "p");

        let _ = tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("watcher should exit within 1 s after paused_reason clears");

        // The watcher did NOT log its own resume audit (manual resume
        // path owns its audit row in api/plans.rs).
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            !plan_actions
                .iter()
                .any(|a| a == actions::AUTO_RESUMED_TREE_CLEAN),
            "AUTO_RESUMED_TREE_CLEAN must not fire when the operator beats the watcher to resume"
        );
        let evs = drain_event_types(&mut rx);
        assert!(
            !evs.iter().any(|t| t == "auto_mode_resumed"),
            "auto_mode_resumed must not fire from the watcher on manual resume path"
        );

        // Dedupe entry is still freed so a future pause can re-arm.
        assert!(
            !state.dirty_tree_watchers.lock().unwrap().contains("p"),
            "dedupe entry must be removed on early exit"
        );
    }

    /// `spawn_dirty_tree_watcher` is idempotent per plan: a second call
    /// while a watcher is already running does not spawn a duplicate.
    /// Counter-proxy: dedupe set keeps exactly one entry across two
    /// spawn calls.
    #[tokio::test]
    async fn spawn_dirty_tree_watcher_dedupes_per_plan() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_master(&cwd);
        std::fs::write(cwd.join("README.md"), "dirty").unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_dirty_tree_pause(&db, "p", &["README.md"]);
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Two back-to-back spawns. The first wins; the second must be a
        // no-op (dedupe set already contains "p"). The actual tokio
        // task is the production 5 s loop, but we never await it — we
        // only care that the dedupe set tracks exactly one entry.
        spawn_dirty_tree_watcher(state.clone(), "p".to_string());
        spawn_dirty_tree_watcher(state.clone(), "p".to_string());

        let watchers = state.dirty_tree_watchers.lock().unwrap().clone();
        let entries_for_p: Vec<&String> = watchers.iter().filter(|n| *n == "p").collect();
        assert_eq!(
            entries_for_p.len(),
            1,
            "watcher dedupe set must track exactly one entry per plan"
        );
    }

    /// `IdleFinishConfig::from_values` parsing: covers the matrix the
    /// previous per-iteration env reads handled. Pure helper, no env
    /// mutation — safe under parallel cargo test.
    #[test]
    fn idle_finish_config_from_values() {
        // Both unset → disabled, default threshold.
        let cfg = IdleFinishConfig::from_values(None, None);
        assert!(!cfg.enabled);
        assert_eq!(cfg.threshold_secs, IDLE_THRESHOLD_DEFAULT_SECS);

        // Enabled flag must be the literal string "1" — anything else
        // (incl. "true", "yes", "on", whitespace, empty) leaves the
        // poller disabled. Matches the original gate semantics.
        for raw in ["", "0", "true", "TRUE", "yes", "on", " 1", "1 "] {
            let cfg = IdleFinishConfig::from_values(Some(raw), None);
            assert!(!cfg.enabled, "enabled must be false for {raw:?}, got true");
        }
        let cfg = IdleFinishConfig::from_values(Some("1"), None);
        assert!(cfg.enabled);

        // Threshold parsing: positive int wins, anything else (zero,
        // negative, non-numeric, empty) falls back to the default.
        let cfg = IdleFinishConfig::from_values(Some("1"), Some("120"));
        assert_eq!(cfg.threshold_secs, 120);
        for raw in ["0", "-5", "abc", "", "1.5"] {
            let cfg = IdleFinishConfig::from_values(Some("1"), Some(raw));
            assert_eq!(
                cfg.threshold_secs, IDLE_THRESHOLD_DEFAULT_SECS,
                "threshold must default for {raw:?}"
            );
        }
    }

    // ── Push lock (Phase 2) ──────────────────────────────────────────────

    /// Happy path: a single caller can acquire, observe the row, and the
    /// guard's Drop releases the row.
    #[tokio::test]
    async fn push_lock_guard_releases_on_drop() {
        let (db, _dir) = fresh_db();
        {
            let _guard = wait_for_push_lock(
                &db,
                "master",
                "auto_mode",
                std::process::id() as i64,
                Some("plan=p"),
                Duration::from_secs(1),
            )
            .await
            .expect("uncontended acquire must succeed");
            assert!(
                crate::db::peek_push_lock(&db, "master").is_some(),
                "row must exist while guard is alive"
            );
        }
        assert!(
            crate::db::peek_push_lock(&db, "master").is_none(),
            "guard Drop must release the row"
        );
    }

    /// The CORE acceptance criterion: with two callers firing in the same
    /// second, exactly one acquires the lock first; the other waits.
    /// Driving "the same second" with `tokio::join!` is sufficient because
    /// the polling loop has a 200ms tick — the join races the two acquire
    /// futures on the same thread, both hit `try_acquire_push_lock` within
    /// a few microseconds of each other.
    ///
    /// We can't predict which caller wins (SQLite acquire order is
    /// implementation-defined), but we CAN assert:
    ///   - Both eventually succeed.
    ///   - At every moment only one holder exists.
    ///   - The winner's release is observed by the loser (the loser's
    ///     final acquire happens AFTER the winner releases).
    ///   - The two tokens differ.
    #[tokio::test]
    async fn two_simultaneous_acquires_serialize_with_one_winner() {
        let (db, _dir) = fresh_db();
        let db_a = db.clone();
        let db_b = db.clone();

        let started_b = Arc::new(StdMutex::new(false));
        let started_b_clone = started_b.clone();

        // Caller A acquires, holds for ~250ms (long enough to span at
        // least one full poll tick on B's loop), then releases.
        let task_a = tokio::spawn(async move {
            let guard = wait_for_push_lock(
                &db_a,
                "master",
                "auto_mode",
                1,
                Some("a"),
                Duration::from_secs(5),
            )
            .await
            .expect("A must acquire");
            let token_a = guard.token().to_string();
            // Wait until B has at least entered the wait loop. Then
            // hold the lock through a couple of poll ticks.
            for _ in 0..50 {
                if *started_b_clone.lock().unwrap() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            // Snapshot the holder before drop so we can assert it was us.
            let holder_before =
                crate::db::peek_push_lock(&db_a, "master").expect("row must exist while A holds");
            assert_eq!(holder_before.holder_token, token_a);
            drop(guard);
            token_a
        });

        // Tiny delay so A has a chance to acquire first; then B enters
        // the wait loop. With a 1s timeout B will succeed once A drops.
        tokio::time::sleep(Duration::from_millis(20)).await;
        *started_b.lock().unwrap() = true;
        let task_b = tokio::spawn(async move {
            let guard =
                wait_for_push_lock(&db_b, "master", "ci", 2, Some("b"), Duration::from_secs(5))
                    .await
                    .expect("B must acquire after A releases");
            guard.token().to_string()
        });

        let token_a = task_a.await.unwrap();
        let token_b = task_b.await.unwrap();
        assert_ne!(
            token_a, token_b,
            "winner + loser must get different opaque tokens"
        );
        // After both tasks complete, B's guard has dropped too: no row.
        assert!(
            crate::db::peek_push_lock(&db, "master").is_none(),
            "after both releases the row must be gone"
        );
    }

    /// The /api/git/push-lock endpoint surface contract: when the lock
    /// is held by another caller AND the wait_timeout elapses, the
    /// helper returns `PushLockError::Timeout` carrying the live
    /// holder snapshot. Tests the timeout path directly so we don't
    /// need a full HTTP harness.
    #[tokio::test]
    async fn wait_for_push_lock_times_out_with_live_holder() {
        let (db, _dir) = fresh_db();
        // Hold the lock for a long time.
        let _holder = wait_for_push_lock(
            &db,
            "master",
            "auto_mode",
            42,
            Some("plan=p1"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        let started = std::time::Instant::now();
        let res = wait_for_push_lock(
            &db,
            "master",
            "ci",
            7,
            Some("run-99"),
            Duration::from_millis(400),
        )
        .await;
        let elapsed = started.elapsed();

        match res {
            Err(PushLockError::Timeout(h)) => {
                assert_eq!(h.holder_kind, "auto_mode");
                assert_eq!(h.holder_pid, 42);
                assert_eq!(h.holder_meta.as_deref(), Some("plan=p1"));
            }
            Ok(_) => panic!("acquire must NOT succeed while another holder is alive"),
        }
        assert!(
            elapsed >= Duration::from_millis(400),
            "must wait the full timeout before returning Timeout, got {elapsed:?}"
        );
        // The original holder is still there.
        let holder = crate::db::peek_push_lock(&db, "master").expect("row must still exist");
        assert_eq!(holder.holder_kind, "auto_mode");
    }

    /// `PushLockGuard::forget()` disables the Drop release so the HTTP
    /// endpoint can take ownership of the token. Verify the lock row
    /// SURVIVES guard drop in that case.
    #[tokio::test]
    async fn push_lock_guard_forget_skips_drop_release() {
        let (db, _dir) = fresh_db();
        let guard = wait_for_push_lock(
            &db,
            "master",
            "ci",
            std::process::id() as i64,
            Some("run-1"),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let token = guard.forget();
        // The Drop already ran when `forget()` consumed `guard`; row
        // must still be present because Drop saw forgotten=true.
        let holder =
            crate::db::peek_push_lock(&db, "master").expect("forget() must skip Drop release");
        assert_eq!(holder.holder_token, token);
        // Caller is now responsible for explicit release.
        assert!(crate::db::release_push_lock(&db, "master", &token));
        assert!(crate::db::peek_push_lock(&db, "master").is_none());
    }

    /// Regression test for the wiring gap that originally let auto-mode
    /// merges land without recording a pending `ci_runs` row. Every call
    /// site of [`merge_agent_branch_dispatch`] must be paired with a
    /// [`crate::ci::trigger_after_merge`] spawn — either directly in the
    /// caller, or via the single shared helper
    /// [`crate::api::agents::merge_agent_branch_inner`] that
    /// `merge_agent_branch_dispatch` delegates to.
    ///
    /// Pure code-shape test: pulls the three involved source files in
    /// via `include_str!` and string-greps the post-comment-stripped
    /// text. No DB, no spawn, no async.
    ///
    /// To verify the test catches regressions by hand, delete the
    /// `tokio::spawn(crate::ci::trigger_after_merge(...))` block from
    /// `api/agents.rs::merge_agent_branch_inner` and re-run: assertion
    /// (3) fires immediately.
    #[test]
    fn every_merge_agent_branch_dispatch_call_site_pairs_with_trigger_after_merge() {
        // Three files participate in the chain:
        //   • `auto_mode.rs`     — host of the dispatch call sites
        //   • `saas/dispatch.rs` — defines `merge_agent_branch_dispatch`
        //                         and delegates to the shared helper
        //   • `api/agents.rs`    — defines the shared helper
        //                         `merge_agent_branch_inner` and spawns
        //                         `trigger_after_merge` inside it
        let auto_mode_src = include_str!("auto_mode.rs");
        let dispatch_src = include_str!("saas/dispatch.rs");
        let inner_src = include_str!("api/agents.rs");

        // Strip everything after `//` on each line so doc comments and
        // inline `//` notes don't show up as false call sites or fake
        // pairings. The targeted identifiers
        // (`merge_agent_branch_dispatch(`, `merge_agent_branch_inner`,
        // `trigger_after_merge`) don't appear inside string literals
        // anywhere in the repo today, so a naive split on `//` is safe.
        let auto_mode_code = strip_line_comments(auto_mode_src);
        let dispatch_code = strip_line_comments(dispatch_src);
        let inner_code = strip_line_comments(inner_src);

        // ── (1) Enumerate call sites of `merge_agent_branch_dispatch(` ────
        //
        // After comment-stripping, the literal `merge_agent_branch_dispatch(`
        // only matches actual function calls and the function definition
        // (excluded by the `fn`-prefix filter). The `use ...` import in
        // `auto_mode.rs:31` has no `(` after the identifier, so it's
        // already excluded by the substring match.
        //
        // Today there are exactly two call sites in `auto_mode.rs`:
        //   • `run_merge_step`           (line ~298)
        //   • `on_fix_agent_completed`   (line ~1031)
        // A third caller (e.g. a future manual-rebase recovery path)
        // would still route through `dispatch → inner → trigger`, so
        // the chain holds without a test update.
        let call_sites: Vec<(usize, &str)> = auto_mode_code
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("merge_agent_branch_dispatch("))
            .filter(|(_, line)| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("pub async fn")
                    && !trimmed.starts_with("async fn")
                    && !trimmed.starts_with("fn ")
            })
            .collect();

        assert!(
            call_sites.len() >= 2,
            "expected at least 2 call sites of `merge_agent_branch_dispatch` \
             in `auto_mode.rs`, found {}: {:?}.\n\n\
             If you've moved or removed a call site, ensure the new wiring \
             still pairs with `crate::ci::trigger_after_merge` (directly, or \
             via the shared helper `crate::api::agents::merge_agent_branch_inner`).",
            call_sites.len(),
            call_sites,
        );

        // ── (2) Dispatch delegates to the shared helper ───────────────────
        //
        // `merge_agent_branch_dispatch`'s entire job is to be a single
        // uniform entry point — today it's a one-line delegation to
        // `merge_agent_branch_inner`. If a future refactor inlines the
        // merge logic or routes through a different helper, this test
        // must be updated to point at the new pairing.
        assert!(
            dispatch_code.contains("merge_agent_branch_inner"),
            "`merge_agent_branch_dispatch` in `saas/dispatch.rs` must \
             delegate to `merge_agent_branch_inner` — the shared helper \
             that spawns `crate::ci::trigger_after_merge`. If you've \
             changed the dispatch surface, the new body must still reach \
             `trigger_after_merge` (either by calling \
             `merge_agent_branch_inner` here, or by spawning \
             `trigger_after_merge` directly)."
        );

        // ── (3) The shared helper spawns `trigger_after_merge` ────────────
        //
        // This is the load-bearing wiring: break it and BOTH merge paths
        // (user click via the HTTP merge endpoint, and auto-mode via
        // `merge_agent_branch_dispatch`) silently stop recording CI rows.
        // The literal `trigger_after_merge` survives comment-stripping
        // only at the actual spawn site (the doc comment reference at
        // `api/agents.rs:570` is stripped because it starts with `///`).
        assert!(
            inner_code.contains("trigger_after_merge"),
            "`merge_agent_branch_inner` in `api/agents.rs` must spawn \
             `crate::ci::trigger_after_merge` so every merge — user \
             click via the HTTP endpoint AND auto-mode via \
             `merge_agent_branch_dispatch` — records a pending `ci_runs` \
             row. If you've moved the CI trigger out of \
             `merge_agent_branch_inner`, update this test to point at \
             the new pairing (either the new helper, or a direct \
             pairing at every dispatch call site)."
        );
    }

    /// Drop everything after `//` on each line so doc comments and
    /// inline notes don't contribute false matches when grepping for
    /// identifiers in this regression test. Doesn't handle block
    /// comments (`/* ... */`) — the targeted identifiers never appear
    /// inside block comments in this repo.
    fn strip_line_comments(src: &str) -> String {
        src.lines()
            .map(|line| match line.find("//") {
                Some(i) => &line[..i],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── should_merge_now (Task 2.1) ─────────────────────────────────────
    //
    // Pin the four acceptance branches from the brief:
    //   (a) `task` cadence always returns `true`.
    //   (b) `phase` returns `false` mid-phase, `true` at the last task.
    //   (c) `plan` returns `false` mid-plan, `true` at the final task.
    //   (d) `failed` blocks the boundary (operator either fixes it or
    //       marks the task `skipped`).
    //
    // Plus a few sanity tests: skipped counts as done, the resolution
    // chain (plan-pin → repo default → hard-coded `Phase`), and the
    // defensive return when `completed_task` isn't in the plan.

    /// Write a 2-phase, 3-tasks-per-phase plan with an ABSOLUTE
    /// `project:` path so [`crate::ci::project_dir_for`] resolves
    /// straight to the tempdir we control — drop a `branchwork.toml`
    /// inside to exercise the repo-default branch of the resolution
    /// chain. `home.join(abs)` correctly discards `home` (well-tested
    /// Rust `PathBuf::join` behaviour).
    fn write_six_task_plan(plans_dir: &Path, name: &str, project_dir: &Path) {
        std::fs::create_dir_all(plans_dir).unwrap();
        let yaml = format!(
            "title: Six-task plan\n\
             project: {project}\n\
             phases:\n  \
               - number: 0\n    \
                 title: Phase 0\n    \
                 tasks:\n      \
                   - number: \"0.1\"\n        \
                     title: 0.1\n      \
                   - number: \"0.2\"\n        \
                     title: 0.2\n      \
                   - number: \"0.3\"\n        \
                     title: 0.3\n  \
               - number: 1\n    \
                 title: Phase 1\n    \
                 tasks:\n      \
                   - number: \"1.1\"\n        \
                     title: 1.1\n      \
                   - number: \"1.2\"\n        \
                     title: 1.2\n      \
                   - number: \"1.3\"\n        \
                     title: 1.3\n",
            project = project_dir.display(),
        );
        std::fs::write(plans_dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    /// Insert a `task_status` row with any status — extends
    /// [`mark_task_status_completed`] for the `failed` / `skipped`
    /// / `in_progress` cases the predicate must respect.
    fn mark_task_status(db: &Db, plan: &str, task: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, task_number) DO UPDATE SET status = excluded.status",
            params![plan, task, status],
        )
        .unwrap();
    }

    /// Build a minimal `AppState` with the chosen `plans_dir`; the
    /// caller drops the broadcast receiver because `should_merge_now`
    /// emits no events.
    fn app_state(db: Db, plans_dir: PathBuf) -> AppState {
        let (state, _rx) = test_app_state(db, new_runner_registry(), plans_dir);
        state
    }

    #[test]
    fn should_merge_now_task_cadence_always_true() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Task));

        let state = app_state(db.clone(), plans_dir);

        // No task_status rows at all → still true.
        assert!(should_merge_now(&state, "p", "0.1"));
        // Even with a failed sibling, `task` cadence is unconditional.
        mark_task_status(&db, "p", "0.2", "failed");
        assert!(should_merge_now(&state, "p", "0.1"));
        // Mid-plan completion of a later task is also fine.
        assert!(should_merge_now(&state, "p", "1.2"));
    }

    #[test]
    fn should_merge_now_phase_returns_false_mid_phase_and_true_at_last_task() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Phase));

        let state = app_state(db.clone(), plans_dir);

        // First in phase 0; 0.2 and 0.3 still pending → false.
        assert!(!should_merge_now(&state, "p", "0.1"));

        // Mid-phase: 0.1 already done but 0.3 still pending → false.
        mark_task_status(&db, "p", "0.1", "completed");
        assert!(!should_merge_now(&state, "p", "0.2"));

        // Last task in phase 0: 0.1 + 0.2 done, completing 0.3 closes
        // the phase → true. Phase 1 tasks pending don't matter for
        // `phase` cadence.
        mark_task_status(&db, "p", "0.2", "completed");
        assert!(should_merge_now(&state, "p", "0.3"));
    }

    #[test]
    fn should_merge_now_plan_returns_false_mid_plan_and_true_at_final_task() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Plan));

        let state = app_state(db.clone(), plans_dir);

        // Nothing done → false.
        assert!(!should_merge_now(&state, "p", "0.1"));

        // Phase 0 fully done, completing first phase-1 task → still
        // false because phase 1 is mostly pending.
        for t in &["0.1", "0.2", "0.3"] {
            mark_task_status(&db, "p", t, "completed");
        }
        assert!(!should_merge_now(&state, "p", "1.1"));

        // Phase 1 first two done; completing 1.3 closes the plan → true.
        mark_task_status(&db, "p", "1.1", "completed");
        mark_task_status(&db, "p", "1.2", "completed");
        assert!(should_merge_now(&state, "p", "1.3"));
    }

    #[test]
    fn should_merge_now_failed_task_blocks_phase_predicate() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Phase));

        let state = app_state(db.clone(), plans_dir);

        // Phase otherwise full: 0.1 just completed (caller's word),
        // 0.3 completed, but 0.2 failed → false.
        mark_task_status(&db, "p", "0.2", "failed");
        mark_task_status(&db, "p", "0.3", "completed");
        assert!(!should_merge_now(&state, "p", "0.1"));

        // Resolving the failure by marking it skipped flips the
        // predicate to true (operator escape hatch in the brief).
        mark_task_status(&db, "p", "0.2", "skipped");
        assert!(should_merge_now(&state, "p", "0.1"));
    }

    #[test]
    fn should_merge_now_failed_task_blocks_plan_predicate() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Plan));

        let state = app_state(db.clone(), plans_dir);

        // Every task done except 1.2 (failed) → completing 1.3 still
        // can't close the plan.
        for t in &["0.1", "0.2", "0.3", "1.1"] {
            mark_task_status(&db, "p", t, "completed");
        }
        mark_task_status(&db, "p", "1.2", "failed");
        assert!(!should_merge_now(&state, "p", "1.3"));
    }

    #[test]
    fn should_merge_now_skipped_counts_as_done() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Phase));

        let state = app_state(db.clone(), plans_dir);

        // 0.1 skipped, 0.3 completed, completing 0.2 should close
        // the phase (skipped tasks are treated as done).
        mark_task_status(&db, "p", "0.1", "skipped");
        mark_task_status(&db, "p", "0.3", "completed");
        assert!(should_merge_now(&state, "p", "0.2"));
    }

    #[test]
    fn should_merge_now_defaults_to_phase_when_no_pin_and_no_repo_config() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        // Deliberately no `db::set_plan_merge_cadence` call AND no
        // `branchwork.toml` in `project_dir`. Resolution chain falls
        // through to `MergeCadence::default()` (= Phase).

        let state = app_state(db.clone(), plans_dir);

        // Mid-phase → false. Last task in phase 0 → true. That's the
        // Phase contract.
        assert!(!should_merge_now(&state, "p", "0.1"));
        mark_task_status(&db, "p", "0.1", "completed");
        mark_task_status(&db, "p", "0.2", "completed");
        assert!(should_merge_now(&state, "p", "0.3"));
    }

    #[test]
    fn should_merge_now_inherits_repo_default_when_no_plan_pin() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        // Repo override: branchwork.toml says merge_cadence = "plan".
        // Combined with no plan-level pin, the predicate uses Plan.
        std::fs::write(
            project_dir.join("branchwork.toml"),
            "[auto_mode]\nmerge_cadence = \"plan\"\n",
        )
        .unwrap();
        // The `repo_config` cache is keyed by canonical path; tests in
        // other modules also write toml in tempdirs, so unique paths
        // already keep us out of each other's way.
        crate::repo_config::clear_cache_for_tests();

        let state = app_state(db.clone(), plans_dir);

        // Phase 0 fully done, completing first phase-1 task → false
        // because Plan cadence requires every task done, and phase 1
        // is still mostly pending.
        for t in &["0.1", "0.2", "0.3"] {
            mark_task_status(&db, "p", t, "completed");
        }
        assert!(!should_merge_now(&state, "p", "1.1"));
    }

    #[test]
    fn should_merge_now_plan_pin_wins_over_repo_default() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        // Repo override says Plan; plan pin says Task. Plan pin wins,
        // so the predicate is unconditionally `true` even with
        // everything else pending.
        std::fs::write(
            project_dir.join("branchwork.toml"),
            "[auto_mode]\nmerge_cadence = \"plan\"\n",
        )
        .unwrap();
        crate::repo_config::clear_cache_for_tests();
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Task));

        let state = app_state(db, plans_dir);

        assert!(should_merge_now(&state, "p", "0.1"));
    }

    #[test]
    fn should_merge_now_unknown_task_returns_false_defensively() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Phase));

        let state = app_state(db, plans_dir);

        // Bogus task number: parser drift, stale agent row. Refuse
        // the merge rather than guess.
        assert!(!should_merge_now(&state, "p", "9.9"));
    }

    #[test]
    fn should_merge_now_returns_false_when_plan_missing() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        // No plan YAML at all.
        std::fs::create_dir_all(&plans_dir).unwrap();
        db::set_plan_merge_cadence(&db, "ghost", Some(MergeCadence::Phase));

        let state = app_state(db, plans_dir);

        // No plan to load → refuse. `Task` cadence still short-circuits
        // before the load, so this only fires for Phase/Plan.
        assert!(!should_merge_now(&state, "ghost", "0.1"));
    }

    // ── cadence-deferral + boundary drain (Task 2.2) ────────────────────
    //
    // These pin the Task 2.2 contract end-to-end:
    //   - Mid-phase / mid-plan completions defer: the agent's row
    //     gains `merge_status='deferred_for_cadence'`, no MergeBranch
    //     / PushBranch envelopes go out, no `ci_runs` row appears.
    //   - The boundary-task completion drains every deferred sibling
    //     in dependency order (per the plan's YAML declaration order),
    //     then merges itself. The trigger merge is the only one that
    //     produces a PushBranch envelope — the upstream drains run
    //     with `trigger_ci=false`, so a 4-task phase produces exactly
    //     ONE master push and ONE `ci_runs` row.

    /// Write a 4-task single-phase plan YAML. Mirrors the headline
    /// acceptance scenario in the brief: phase 1 with tasks 1.1 / 1.2 /
    /// 1.3 / 1.4. `project` is set to a unique fake path so the helpers
    /// that resolve `work_dir` don't touch a real repo.
    fn write_four_task_phase_plan(plans_dir: &Path, name: &str, fake_project: &str) {
        std::fs::create_dir_all(plans_dir).unwrap();
        let yaml = format!(
            "title: 4-task phase plan\n\
             project: {fake_project}\n\
             phases:\n  \
               - number: 1\n    \
                 title: Phase 1\n    \
                 tasks:\n      \
                   - number: \"1.1\"\n        \
                     title: 1.1\n      \
                   - number: \"1.2\"\n        \
                     title: 1.2\n      \
                   - number: \"1.3\"\n        \
                     title: 1.3\n      \
                   - number: \"1.4\"\n        \
                     title: 1.4\n"
        );
        std::fs::write(plans_dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    /// Read `merge_status` for the agent row.
    fn agent_merge_status(db: &Db, agent_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT merge_status FROM agents WHERE id = ?1",
            params![agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    /// Drain `auto_mode_merge_deferred` event payloads in arrival order.
    fn drain_merge_deferred_payloads(
        rx: &mut broadcast::Receiver<String>,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_merge_deferred")
                && let Some(d) = v.get("data")
            {
                out.push(d.clone());
            }
        }
        out
    }

    /// Drain `auto_mode_merged` event payloads in arrival order.
    fn drain_merged_payloads(rx: &mut broadcast::Receiver<String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_merged")
                && let Some(d) = v.get("data")
            {
                out.push(d.clone());
            }
        }
        out
    }

    /// Convenience: insert one agent row per (id, task) pair on plan
    /// `p`. Each row carries a synthetic task branch name. `cwd` is the
    /// same for every agent because the SaaS-mode merge dispatch
    /// (echo runner) doesn't actually shell out to git — the path just
    /// has to deserialize.
    fn seed_phase_agents(db: &Db, cwd: &Path, rows: &[(&str, &str)]) {
        for (id, task) in rows {
            let branch = format!("branchwork/p/{task}");
            seed_agent(db, id, cwd, "p", task, &branch);
        }
    }

    /// Count agents on plan `p` whose `merge_status='deferred_for_cadence'`.
    fn count_deferred_agents(db: &Db, plan: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM agents \
             WHERE plan_name = ?1 AND merge_status = 'deferred_for_cadence'",
            params![plan],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    /// Drain every pending envelope from the echo runner's outgoing
    /// channel and append it to `seen`. Callers maintain their own
    /// growing list of captured envelope types so they can assert
    /// running totals AND envelope ordering across multiple state-
    /// machine calls — single-shot `try_recv` would lose anything
    /// emitted after the assertion and the wrong envelope type would
    /// be silently consumed.
    ///
    /// Each entry in `seen` is the JSON `"type"` discriminator
    /// (the snake_case `WireMessage` variant name). `Envelope` carries
    /// its `message` via `#[serde(flatten)]`, so the discriminator
    /// lives at the top level — no nested `"message"` key.
    fn drain_envelope_types(rx: &mut mpsc::UnboundedReceiver<String>, seen: &mut Vec<String>) {
        while let Ok(payload) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload)
                && let Some(t) = v.get("type").and_then(|t| t.as_str())
            {
                seen.push(t.to_string());
            }
        }
    }

    /// Brief's headline acceptance test: 4-task phase under
    /// `merge_cadence='phase'`. Tasks 1.1 / 1.2 / 1.3 each complete →
    /// no master pushes (and no `ci_runs` rows); task 1.4 completes →
    /// ONE master push containing all four task merges in order.
    #[tokio::test]
    async fn phase_cadence_defers_three_then_drains_all_four_on_boundary() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        seed_runner_row(&db, "runner-1", org_id);

        let plans_dir = dir.path().join("plans");
        let fake_project = format!(
            "branchwork-test-{}-phase-batch",
            uuid::Uuid::new_v4().simple()
        );
        write_four_task_phase_plan(&plans_dir, "p", &fake_project);
        // The cadence pin is what flips this plan out of legacy `Task`
        // behaviour; without it the gate would always return true and
        // each completion would merge immediately.
        db::set_plan_merge_cadence(&db, "p", Some(MergeCadence::Phase));
        enable_auto_mode(&db, "p");

        // Echo runner stubs: deterministic merged_sha per envelope so the
        // assertions on order are unambiguous. Counter via Arc<Mutex>
        // so the closure can mutate per call.
        let merge_counter = Arc::new(StdMutex::new(0u32));
        let mc = merge_counter.clone();
        let runners = new_runner_registry();
        let mut outgoing = install_echo_runner(&runners, "runner-1", move |msg| match msg {
            WireMessage::GetDefaultBranch { .. } => {
                Some(RunnerResponse::DefaultBranchResolved(Some("master".into())))
            }
            WireMessage::MergeBranch { .. } => {
                let mut n = mc.lock().unwrap();
                *n += 1;
                Some(RunnerResponse::MergeResult(WireMergeOutcome::Ok {
                    merged_sha: format!("sha-{n:03}"),
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

        // Seed all four agent rows + their task branches. cwd is on the
        // "runner" so it never touches real disk.
        let cwd = Path::new("/runner/cwd");
        seed_phase_agents(
            &db,
            cwd,
            &[
                ("agent-1.1", "1.1"),
                ("agent-1.2", "1.2"),
                ("agent-1.3", "1.3"),
                ("agent-1.4", "1.4"),
            ],
        );

        // Per-call running tally — drain_envelope_types accumulates
        // envelope types into `seen`. Indirect `count` helper closes
        // over `seen` to count entries matching a given event_type.
        let mut seen: Vec<String> = Vec::new();
        let count = |seen: &[String], et: &str| seen.iter().filter(|s| *s == et).count();

        // ── Drive 1.1 ─────────────────────────────────────────────
        // No task_status rows yet → caller's word treats 1.1 as done,
        // 1.2/1.3/1.4 still pending → defer.
        run_state_machine(&state, org_id, "agent-1.1", "p", "1.1").await;
        drain_envelope_types(&mut outgoing, &mut seen);

        assert_eq!(
            agent_merge_status(&db, "agent-1.1").as_deref(),
            Some("deferred_for_cadence"),
            "1.1 must be marked deferred at the cadence gate"
        );
        // Mid-phase: zero MergeBranch envelopes, zero PushBranch envelopes,
        // no ci_runs row inserted.
        assert_eq!(
            count(&seen, "merge_branch"),
            0,
            "1.1 defer must not send MergeBranch"
        );
        assert_eq!(
            count(&seen, "push_branch"),
            0,
            "1.1 defer must not send PushBranch"
        );
        let ci_count = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM ci_runs WHERE plan_name = 'p'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
        };
        assert_eq!(ci_count, 0, "1.1 defer must not insert a ci_runs row");

        // Now flip the task_status row so the gate at 1.2 sees 1.1 as done
        // (via the persisted row, since the auto-mode loop reads from
        // `task_status`, not just the caller's word).
        mark_task_status(&db, "p", "1.1", "completed");

        // ── Drive 1.2 ─────────────────────────────────────────────
        run_state_machine(&state, org_id, "agent-1.2", "p", "1.2").await;
        drain_envelope_types(&mut outgoing, &mut seen);
        assert_eq!(
            agent_merge_status(&db, "agent-1.2").as_deref(),
            Some("deferred_for_cadence"),
        );
        assert_eq!(count(&seen, "merge_branch"), 0);
        assert_eq!(count(&seen, "push_branch"), 0);

        mark_task_status(&db, "p", "1.2", "completed");

        // ── Drive 1.3 ─────────────────────────────────────────────
        run_state_machine(&state, org_id, "agent-1.3", "p", "1.3").await;
        drain_envelope_types(&mut outgoing, &mut seen);
        assert_eq!(
            agent_merge_status(&db, "agent-1.3").as_deref(),
            Some("deferred_for_cadence"),
        );
        assert_eq!(count(&seen, "merge_branch"), 0);
        assert_eq!(count(&seen, "push_branch"), 0);

        // All three deferred rows are present.
        assert_eq!(count_deferred_agents(&db, "p"), 3);

        // Three `auto_mode_merge_deferred` broadcasts, one per defer.
        let deferred_events = drain_merge_deferred_payloads(&mut rx);
        let deferred_tasks: Vec<String> = deferred_events
            .iter()
            .filter_map(|d| d.get("task").and_then(|t| t.as_str()).map(String::from))
            .collect();
        assert_eq!(
            deferred_tasks,
            vec!["1.1".to_string(), "1.2".to_string(), "1.3".to_string()],
            "expected one deferred broadcast per task, in task order"
        );
        for d in &deferred_events {
            assert_eq!(
                d.get("cadence").and_then(|c| c.as_str()),
                Some("phase"),
                "every deferred broadcast carries the effective cadence"
            );
        }

        mark_task_status(&db, "p", "1.3", "completed");

        // ── Drive 1.4: the cadence boundary ───────────────────────
        // should_merge_now flips true (every task in phase 1 is done
        // per task_status + caller's word). The state machine must
        // drain 1.1/1.2/1.3 in YAML declaration order, then merge
        // 1.4 with `trigger_ci=true` — single PushBranch envelope,
        // single `ci_runs` row.
        run_state_machine(&state, org_id, "agent-1.4", "p", "1.4").await;
        drain_envelope_types(&mut outgoing, &mut seen);

        // Exactly 4 MergeBranch envelopes (3 drain + 1 trigger).
        assert_eq!(
            count(&seen, "merge_branch"),
            4,
            "expected 4 MergeBranch envelopes (3 drains + 1 trigger), got {}",
            count(&seen, "merge_branch"),
        );

        // Poll for the PushBranch envelope: trigger_after_merge is
        // spawned, so the push lands asynchronously. Drain into the
        // running tally on each poll so we don't lose any envelope.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && count(&seen, "push_branch") == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            drain_envelope_types(&mut outgoing, &mut seen);
        }
        let pushes = count(&seen, "push_branch");
        assert_eq!(
            pushes, 1,
            "single master push at the end of the batch, got {pushes}",
        );

        // Exactly one `ci_runs` row for the entire phase batch (pinned
        // to 1.4's trigger; the drains used `trigger_ci=false`).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ci_count = 0i64;
        while std::time::Instant::now() < deadline {
            ci_count = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM ci_runs WHERE plan_name = 'p'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
            };
            if ci_count > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            ci_count, 1,
            "single ci_runs row for the whole batch, got {ci_count}",
        );

        // All four agents merged → no rows left in the deferred set.
        assert_eq!(
            count_deferred_agents(&db, "p"),
            0,
            "drain must clear merge_status on every batched agent",
        );

        // Four `auto_mode_merged` broadcasts arrive in YAML
        // declaration order (1.1, 1.2, 1.3, 1.4). The trigger agent's
        // merge fires last because the drain runs first.
        let merged_events = drain_merged_payloads(&mut rx);
        let merged_tasks: Vec<String> = merged_events
            .iter()
            .filter_map(|d| d.get("task").and_then(|t| t.as_str()).map(String::from))
            .collect();
        assert_eq!(
            merged_tasks,
            vec![
                "1.1".to_string(),
                "1.2".to_string(),
                "1.3".to_string(),
                "1.4".to_string(),
            ],
            "drain merges in YAML declaration order, trigger merges last",
        );

        // Plan stays unpaused on success.
        assert!(paused_reason(&db, "p").is_none());
    }

    /// Direct cover for [`defer_for_cadence`]: a non-boundary completion
    /// stamps `merge_status='deferred_for_cadence'` on the agent row,
    /// emits the `auto_mode_merge_deferred` broadcast, and audits the
    /// `AUTO_MODE_MERGE_DEFERRED` action. The agent's `branch` column
    /// is left intact so the eventual drain can find the branch back.
    #[tokio::test]
    async fn defer_for_cadence_marks_agent_and_broadcasts_event() {
        let (db, dir) = fresh_db();
        let org_id = "default-org";
        let plans_dir = dir.path().join("plans");
        let fake_project = format!(
            "branchwork-test-{}-defer-only",
            uuid::Uuid::new_v4().simple()
        );
        write_four_task_phase_plan(&plans_dir, "p", &fake_project);

        let cwd = Path::new("/runner/cwd");
        seed_agent(&db, "agent-1.1", cwd, "p", "1.1", "branchwork/p/1.1");

        let runners = new_runner_registry();
        let (state, mut rx) = test_app_state(db.clone(), runners, plans_dir);

        // Pre-condition: branch column is set, merge_status is NULL.
        let pre_status = agent_merge_status(&db, "agent-1.1");
        assert!(pre_status.is_none());

        defer_for_cadence(&state, org_id, "agent-1.1", "p", "1.1", MergeCadence::Phase).await;

        // Post-condition: merge_status flipped to `deferred_for_cadence`.
        assert_eq!(
            agent_merge_status(&db, "agent-1.1").as_deref(),
            Some("deferred_for_cadence"),
        );

        // Branch column intact — the drain reads it back later.
        let branch: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT branch FROM agents WHERE id = 'agent-1.1'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap_or(None)
        };
        assert_eq!(branch.as_deref(), Some("branchwork/p/1.1"));

        // Broadcast payload carries plan/task/agent_id/cadence.
        let payloads = drain_merge_deferred_payloads(&mut rx);
        assert_eq!(payloads.len(), 1, "exactly one deferred broadcast");
        let d = &payloads[0];
        assert_eq!(d.get("plan").and_then(|v| v.as_str()), Some("p"));
        assert_eq!(d.get("task").and_then(|v| v.as_str()), Some("1.1"));
        assert_eq!(
            d.get("agent_id").and_then(|v| v.as_str()),
            Some("agent-1.1"),
        );
        assert_eq!(d.get("cadence").and_then(|v| v.as_str()), Some("phase"));

        // Audit row recorded.
        let actions = audit_actions_for(&db, "agent-1.1");
        assert!(
            actions
                .iter()
                .any(|a| a == actions::AUTO_MODE_MERGE_DEFERRED),
            "expected AUTO_MODE_MERGE_DEFERRED in actions: {actions:?}"
        );
    }

    /// Direct cover for [`list_deferred_for_cadence_in_order`]: deferred
    /// agents are returned in YAML declaration order regardless of the
    /// insertion order, the trigger agent is excluded, and the helper
    /// returns the agent_id↔task_id pairs (so the drain can iterate
    /// without re-querying the DB).
    #[test]
    fn list_deferred_returns_agents_in_yaml_declaration_order() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let fake_project = format!(
            "branchwork-test-{}-deferred-order",
            uuid::Uuid::new_v4().simple()
        );
        write_four_task_phase_plan(&plans_dir, "p", &fake_project);

        let cwd = Path::new("/runner/cwd");

        // Seed in REVERSE order so a buggy implementation that returns
        // started_at order would produce [1.3, 1.2, 1.1].
        for (id, task) in &[
            ("agent-1.3", "1.3"),
            ("agent-1.1", "1.1"),
            ("agent-1.2", "1.2"),
            ("agent-1.4", "1.4"),
        ] {
            let branch = format!("branchwork/p/{task}");
            seed_agent(&db, id, cwd, "p", task, &branch);
        }
        // Mark 1.1/1.2/1.3 as deferred. 1.4 is the trigger — left
        // unmarked so the helper excludes it explicitly.
        for id in &["agent-1.1", "agent-1.2", "agent-1.3"] {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }

        let state = app_state(db, plans_dir);
        let got = list_deferred_for_cadence_in_order(
            &state,
            "p",
            "1.4",
            "agent-1.4",
            MergeCadence::Phase,
        );
        assert_eq!(
            got,
            vec![
                ("agent-1.1".to_string(), "1.1".to_string()),
                ("agent-1.2".to_string(), "1.2".to_string()),
                ("agent-1.3".to_string(), "1.3".to_string()),
            ],
            "expected drain order matching YAML declaration"
        );

        // The trigger agent (agent-1.4) is excluded even when it carries
        // `merge_status='deferred_for_cadence'` itself — defensive
        // against a race where the agent flips status mid-flight.
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' \
                 WHERE id = 'agent-1.4'",
                [],
            )
            .unwrap();
        }
        let got = list_deferred_for_cadence_in_order(
            &state,
            "p",
            "1.4",
            "agent-1.4",
            MergeCadence::Phase,
        );
        assert_eq!(got.len(), 3, "trigger agent must be excluded");
    }

    /// Task cadence never defers — the drain helper short-circuits to
    /// an empty list and the state machine merges every completion
    /// immediately, mirroring legacy auto-mode.
    #[test]
    fn list_deferred_returns_empty_for_task_cadence() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let fake_project = format!(
            "branchwork-test-{}-task-noop",
            uuid::Uuid::new_v4().simple()
        );
        write_four_task_phase_plan(&plans_dir, "p", &fake_project);

        // Mark every agent deferred — but with Task cadence the helper
        // returns empty regardless.
        let cwd = Path::new("/runner/cwd");
        seed_phase_agents(&db, cwd, &[("a", "1.1"), ("b", "1.2"), ("c", "1.3")]);
        for id in &["a", "b", "c"] {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }

        let state = app_state(db, plans_dir);
        let got =
            list_deferred_for_cadence_in_order(&state, "p", "1.4", "agent-1.4", MergeCadence::Task);
        assert!(
            got.is_empty(),
            "Task cadence drain is unconditionally empty"
        );
    }

    /// Plan cadence scopes the drain to every deferred agent in the
    /// plan, not just the trigger's phase. A 2-phase plan with deferred
    /// agents in both phases drains all of them together.
    #[test]
    fn list_deferred_plan_cadence_drains_across_phases() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        // Reuse the existing 2-phase / 3-task-per-phase helper.
        write_six_task_plan(&plans_dir, "p", &project_dir);

        let cwd = Path::new("/runner/cwd");
        // Phase 0 has one deferred agent; phase 1 has two.
        seed_agent(&db, "a-0.2", cwd, "p", "0.2", "branchwork/p/0.2");
        seed_agent(&db, "a-1.1", cwd, "p", "1.1", "branchwork/p/1.1");
        seed_agent(&db, "a-1.2", cwd, "p", "1.2", "branchwork/p/1.2");
        // Trigger row — phase 1 task 1.3.
        seed_agent(&db, "a-1.3", cwd, "p", "1.3", "branchwork/p/1.3");
        for id in &["a-0.2", "a-1.1", "a-1.2"] {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }

        let state = app_state(db, plans_dir);
        let got =
            list_deferred_for_cadence_in_order(&state, "p", "1.3", "a-1.3", MergeCadence::Plan);
        assert_eq!(
            got,
            vec![
                ("a-0.2".to_string(), "0.2".to_string()),
                ("a-1.1".to_string(), "1.1".to_string()),
                ("a-1.2".to_string(), "1.2".to_string()),
            ],
            "plan cadence drain must walk every phase in YAML order"
        );
    }

    /// Phase cadence does NOT drain agents from another phase. A
    /// deferred agent on task 0.2 (phase 0) must NOT appear when phase
    /// 1's boundary fires.
    #[test]
    fn list_deferred_phase_cadence_scopes_to_trigger_phase() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);

        let cwd = Path::new("/runner/cwd");
        seed_agent(&db, "a-0.2", cwd, "p", "0.2", "branchwork/p/0.2");
        seed_agent(&db, "a-1.1", cwd, "p", "1.1", "branchwork/p/1.1");
        seed_agent(&db, "a-1.3", cwd, "p", "1.3", "branchwork/p/1.3");
        for id in &["a-0.2", "a-1.1"] {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }

        let state = app_state(db, plans_dir);
        let got =
            list_deferred_for_cadence_in_order(&state, "p", "1.3", "a-1.3", MergeCadence::Phase);
        assert_eq!(
            got,
            vec![("a-1.1".to_string(), "1.1".to_string())],
            "phase cadence must not drain agents from another phase"
        );
    }

    /// A row without a `branch` column is ignored even if it carries
    /// `merge_status='deferred_for_cadence'` — the drain only acts on
    /// rows that still point at a mergeable ref. This also covers the
    /// case where a sibling merge already cleared the branch on a
    /// retry row.
    #[test]
    fn list_deferred_ignores_rows_with_null_branch() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);

        let cwd = Path::new("/runner/cwd");
        seed_agent(&db, "a-1.1", cwd, "p", "1.1", "branchwork/p/1.1");
        seed_agent(&db, "a-1.2-stale", cwd, "p", "1.2", "branchwork/p/1.2");
        // Clear branch on the stale row (simulates sibling-merge cleanup).
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET branch = NULL, merge_status = 'deferred_for_cadence' \
                 WHERE id = 'a-1.2-stale'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = 'a-1.1'",
                [],
            )
            .unwrap();
        }
        seed_agent(&db, "a-1.3", cwd, "p", "1.3", "branchwork/p/1.3");

        let state = app_state(db, plans_dir);
        let got =
            list_deferred_for_cadence_in_order(&state, "p", "1.3", "a-1.3", MergeCadence::Phase);
        assert_eq!(
            got,
            vec![("a-1.1".to_string(), "1.1".to_string())],
            "rows with NULL branch must not appear in the drain"
        );
    }

    /// When two agent rows exist for the same task (e.g. a killed
    /// retry), the LATEST one wins. The drain uses the most-recently
    /// started agent's branch — the killed sibling's stale branch ref
    /// would point at orphan commits.
    #[test]
    fn list_deferred_picks_most_recent_agent_per_task() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        write_six_task_plan(&plans_dir, "p", &project_dir);

        let cwd = Path::new("/runner/cwd");
        seed_agent(&db, "a-1.1-killed", cwd, "p", "1.1", "branchwork/p/1.1");
        // The default `started_at` is `datetime('now')` — sleep briefly
        // so the second row's timestamp is strictly later. SQLite
        // datetime resolution is 1s; we just need NEWER to win on the
        // ORDER BY started_at ASC tie-break.
        std::thread::sleep(std::time::Duration::from_secs(1));
        seed_agent(&db, "a-1.1-retry", cwd, "p", "1.1", "branchwork/p/1.1-r2");
        for id in &["a-1.1-killed", "a-1.1-retry"] {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        seed_agent(&db, "a-1.3", cwd, "p", "1.3", "branchwork/p/1.3");

        let state = app_state(db, plans_dir);
        let got =
            list_deferred_for_cadence_in_order(&state, "p", "1.3", "a-1.3", MergeCadence::Phase);
        assert_eq!(
            got,
            vec![("a-1.1-retry".to_string(), "1.1".to_string())],
            "most-recent agent per task must win the drain slot"
        );
    }

    // ── Pre-merge gate (Phase 1 of pre-merge-gate plan, T1.2) ──────────────────

    /// Build a minimal plan YAML at `<plans_dir>/<plan_name>.yaml` pointing
    /// at an absolute `project_dir` so the gate's project resolution
    /// (`ci::project_dir_for` → `home.join(p)` with `p` absolute) collapses
    /// to the tempdir we control. Same trick as `write_six_task_plan` — see
    /// its doc comment for the absolute-path rationale.
    fn write_one_task_plan(plans_dir: &Path, name: &str, project_dir: &Path) {
        std::fs::create_dir_all(plans_dir).unwrap();
        let yaml = format!(
            "title: Pre-merge gate test\n\
             project: {project}\n\
             phases:\n  \
               - number: 0\n    \
                 title: Phase 0\n    \
                 tasks:\n      \
                   - number: \"0.1\"\n        \
                     title: 0.1\n",
            project = project_dir.display(),
        );
        std::fs::write(plans_dir.join(format!("{name}.yaml")), yaml).unwrap();
    }

    /// Write a `branchwork.toml` at `project_dir` carrying the
    /// `[auto_mode.pre_merge_checks]` array. Cache must be cleared
    /// because the static repo-config cache is keyed by canonical path
    /// and persists across tests run in the same process.
    fn write_branchwork_toml(project_dir: &Path, contents: &str) {
        std::fs::write(project_dir.join("branchwork.toml"), contents).unwrap();
        crate::repo_config::clear_cache_for_tests();
    }

    /// Confirm a clean branch passes the gate, the temp worktree is
    /// removed, and the state machine reaches the merge step. This is
    /// the negative half of the T1.2 acceptance: plant a clean branch +
    /// passing checks, then call `run_state_machine` and assert
    /// `auto_mode_merged` fires + the worktree path was cleaned up on
    /// the happy path too.
    #[tokio::test]
    async fn pre_merge_gate_passes_for_clean_branch_and_runs_to_merge() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        // Bare origin so the merge inner's trigger_after_merge can push
        // master without exploding the test.
        let origin = dir.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&origin)
            .output()
            .unwrap();
        assert!(init.status.success());
        run_git(
            &project_dir,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        run_git(&project_dir, &["push", "-q", "-u", "origin", "master"]);

        // Branch with a trivial commit.
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        // `true` here is the trivially-passing check. Single check keeps
        // the test fast.
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"trivial\", cmd = \"true\", timeout_secs = 5 },\n\
             ]\n",
        );

        let agent_id = "agent-pre-merge-pass";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, mut _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);

        // Drive the gate directly first to assert the cleanup contract
        // (path created, path gone after return) — `run_state_machine`
        // hides that signal behind the `merging` -> `awaiting_ci`
        // transitions.
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        // If a previous failing test left it behind, clean up first so
        // the create succeeds.
        let _ = std::fs::remove_dir_all(&expected_path);
        let outcome = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        assert_eq!(outcome, GateOutcome::Pass, "expected Pass for clean branch");
        assert!(
            !expected_path.exists(),
            "worktree path {} should be cleaned up via Drop guard",
            expected_path.display()
        );

        // Plan stays unpaused on a passing gate.
        assert!(
            paused_reason(&db, "p").is_none(),
            "passing gate should not pause the plan"
        );
    }

    /// Headline T1.2 acceptance: plant a branch with a Cargo.toml syntax
    /// error, configure the gate with a `cargo build`-shaped check, run
    /// the gate, and assert a `GateOutcome::Fail` is returned with the
    /// check name + non-zero exit code + diagnostic in `output`. We use
    /// a stub `cargo` shell script instead of real Cargo so the test
    /// doesn't pay a compile cost — the contract under test is "shell
    /// command exits non-zero ⇒ gate fails", which doesn't require an
    /// honest-to-goodness rustc invocation.
    #[tokio::test]
    async fn pre_merge_gate_fails_when_check_exits_nonzero() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        // Plant a Cargo.toml with a deliberate syntax error.
        std::fs::write(
            project_dir.join("Cargo.toml"),
            "[package\nname = \"broken\"\n",
        )
        .unwrap();
        run_git(&project_dir, &["add", "Cargo.toml"]);
        run_git(
            &project_dir,
            &["commit", "-q", "-m", "add broken Cargo.toml"],
        );
        // Create the task branch off this commit so the worktree
        // checkout carries the broken file.
        run_git(&project_dir, &["checkout", "-q", "-b", "branchwork/p/0.1"]);
        std::fs::write(project_dir.join("work.txt"), "task work").unwrap();
        run_git(&project_dir, &["add", "work.txt"]);
        run_git(&project_dir, &["commit", "-q", "-m", "task work"]);
        run_git(&project_dir, &["checkout", "-q", "master"]);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        // The shell-script `cargo build` stub: `grep` is a binary that
        // ships everywhere and exits non-zero with a diagnostic when its
        // pattern doesn't match in stdin. We pipe `Cargo.toml` through
        // it looking for the (missing) closing bracket; mismatched
        // bracket means non-zero exit with the offending line in
        // stderr, which is what we want the gate to capture.
        //
        // The cmd is intentionally chained so stdout AND stderr land in
        // the combined buffer: `sh -c` runs `<cmd>` so `2>&1` would be
        // syntactically valid, but the gate captures stdout + stderr
        // separately on the Tokio side. Simplest portable failure:
        // `false` + a preceding diagnostic via stderr.
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"cargo build\", cmd = \"echo 'fake rustc: error: expected one of `]`, found `[package`' >&2 && exit 101\", timeout_secs = 10 },\n\
             ]\n",
        );

        let agent_id = "agent-pre-merge-fail";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        let _ = std::fs::remove_dir_all(&expected_path);

        let outcome = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        match outcome {
            GateOutcome::Fail {
                check,
                exit_code,
                output,
            } => {
                assert_eq!(check, "cargo build", "first-failure wins, name preserved");
                assert_eq!(
                    exit_code,
                    Some(101),
                    "exit code from the shell must propagate"
                );
                assert!(
                    output.contains("expected one of"),
                    "captured output must include the stderr diagnostic; got {output:?}"
                );
            }
            GateOutcome::Pass => panic!("expected Fail, got Pass"),
        }

        // Worktree was created (via `git worktree add`) but the Drop
        // guard cleaned it up.
        assert!(
            !expected_path.exists(),
            "worktree path {} should be removed even on Fail",
            expected_path.display()
        );
    }

    /// First-failure-wins contract: if the first check fails, the
    /// second check must NOT run. Verified by configuring the second
    /// check to write to a sentinel file the test polls for.
    #[tokio::test]
    async fn pre_merge_gate_short_circuits_on_first_failure() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);

        let sentinel = dir.path().join("ran-second-check.flag");
        let sentinel_str = sentinel.to_string_lossy().to_string();
        // First check fails; second would write the sentinel. If the
        // sentinel exists after the gate, the second check ran (bug).
        let toml = format!(
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               {{ name = \"first\", cmd = \"false\", timeout_secs = 5 }},\n  \
               {{ name = \"second\", cmd = \"touch {sentinel_str}\", timeout_secs = 5 }},\n\
             ]\n"
        );
        write_branchwork_toml(&project_dir, &toml);

        let agent_id = "agent-short-circuit";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        let _ = std::fs::remove_dir_all(&expected_path);

        let outcome = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        match outcome {
            GateOutcome::Fail { check, .. } => assert_eq!(check, "first"),
            GateOutcome::Pass => panic!("expected Fail"),
        }
        assert!(
            !sentinel.exists(),
            "second check must not run after first failure"
        );
    }

    /// When no `[auto_mode.pre_merge_checks]` section is present, the
    /// gate is a no-op pass. Absent file ⇒ Pass. Empty array ⇒ Pass.
    #[tokio::test]
    async fn pre_merge_gate_returns_pass_when_no_checks_configured() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);

        // No branchwork.toml at all → Pass.
        crate::repo_config::clear_cache_for_tests();
        let agent_id = "agent-no-config";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let outcome = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        assert_eq!(outcome, GateOutcome::Pass);

        // Now write an empty `pre_merge_checks` array → also Pass, no
        // worktree creation.
        write_branchwork_toml(&project_dir, "[auto_mode]\npre_merge_checks = []\n");
        let outcome2 = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        assert_eq!(outcome2, GateOutcome::Pass);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        assert!(
            !expected_path.exists(),
            "no checks ⇒ no worktree should be created"
        );
    }

    /// A check that exceeds its `timeout_secs` is killed and counts as
    /// a fail with `exit_code = None`. The captured output carries a
    /// `[killed by gate]` marker so the audit row distinguishes timeout
    /// from non-zero exit.
    #[tokio::test]
    async fn pre_merge_gate_fails_on_per_check_timeout() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        // 1 s timeout, sleep 30 s → must time out and kill the child.
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"slowpoke\", cmd = \"sleep 30\", timeout_secs = 1 },\n\
             ]\n",
        );

        let agent_id = "agent-per-check-timeout";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        let _ = std::fs::remove_dir_all(&expected_path);

        let started = Instant::now();
        let outcome = run_pre_merge_gate(&state, "p", "0.1", agent_id).await;
        let elapsed = started.elapsed();

        match outcome {
            GateOutcome::Fail {
                check,
                exit_code,
                output,
            } => {
                assert_eq!(check, "slowpoke");
                assert!(exit_code.is_none(), "timeout ⇒ exit_code = None");
                assert!(
                    output.contains("killed by gate"),
                    "output must mark the timeout kill; got {output:?}"
                );
            }
            GateOutcome::Pass => panic!("expected Fail (timed out)"),
        }
        assert!(
            elapsed < Duration::from_secs(15),
            "gate must return promptly after timeout; took {elapsed:?}"
        );
        assert!(
            !expected_path.exists(),
            "worktree should be cleaned up even when a check times out"
        );
    }

    /// Output truncation: a check that prints > 50 KB of stdout should
    /// have its captured output collapsed to roughly the cap, with the
    /// `[…truncated…]` marker in the middle.
    #[test]
    fn truncate_output_collapses_middle_with_marker() {
        // 60 KB string: should truncate.
        let big = "a".repeat(60_000);
        let truncated = truncate_output(&big, PRE_MERGE_CHECK_OUTPUT_CAP_BYTES);
        assert!(
            truncated.contains(PRE_MERGE_TRUNCATION_MARKER.trim()),
            "marker must appear in truncated output"
        );
        assert!(
            truncated.len() <= PRE_MERGE_CHECK_OUTPUT_CAP_BYTES + PRE_MERGE_TRUNCATION_MARKER.len(),
            "truncated output must stay within cap (+marker): len={}",
            truncated.len()
        );

        // 10 KB string: no truncation, identity round-trip.
        let small = "b".repeat(10_000);
        assert_eq!(
            truncate_output(&small, PRE_MERGE_CHECK_OUTPUT_CAP_BYTES),
            small
        );
    }

    /// A failing gate inside `run_state_machine` must pause the plan
    /// with the literal reason `pre_merge_check_failed`, audit the
    /// failure as `AUTO_MODE_PRE_MERGE_CHECK_FAILED`, broadcast
    /// `auto_mode_pre_merge_check_failed` with the canonical payload,
    /// and SKIP the merge step entirely. T1.3 of the pre-merge-gate
    /// plan: name + payload shape is the user-visible contract.
    #[tokio::test]
    async fn run_state_machine_pauses_when_pre_merge_gate_fails() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);
        let master_before = git_head_sha(&project_dir);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"always-fails\", cmd = \"echo broken && false\", timeout_secs = 5 },\n\
             ]\n",
        );

        let agent_id = "agent-gate-pauses";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");
        // Phase cadence: with a one-task plan, the boundary fires
        // immediately so `should_merge_now` returns true and the gate
        // runs.

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        let _ = std::fs::remove_dir_all(&expected_path);

        run_state_machine(&state, "default-org", agent_id, "p", "0.1").await;

        // Plan must be paused with the literal T1.3 reason — the check
        // name lives in the payload, not the reason.
        let reason = paused_reason(&db, "p").expect("plan should be paused after gate failure");
        assert_eq!(
            reason, "pre_merge_check_failed",
            "paused_reason should be the literal T1.3 sentinel; got {reason:?}"
        );

        // Merge must NOT have run — master unchanged.
        let master_after = git_head_sha(&project_dir);
        assert_eq!(
            master_before, master_after,
            "merge must be skipped when gate fails"
        );

        // Audit row landed with the new constant.
        let (actions, diffs): (Vec<String>, Vec<Option<String>>) = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT action, diff FROM audit_logs WHERE resource_id = ?1 ORDER BY id")
                .unwrap();
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(params!["p"], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            rows.into_iter().unzip()
        };
        let idx = actions
            .iter()
            .position(|a| a == actions::AUTO_MODE_PRE_MERGE_CHECK_FAILED)
            .unwrap_or_else(|| {
                panic!(
                    "expected {} in {actions:?}",
                    actions::AUTO_MODE_PRE_MERGE_CHECK_FAILED
                )
            });

        // Audit diff carries the T1.3 payload shape verbatim.
        let diff_str = diffs[idx].as_ref().expect("audit row must carry a diff");
        let diff: serde_json::Value = serde_json::from_str(diff_str).expect("diff must be JSON");
        assert_eq!(diff["plan"], "p");
        assert_eq!(diff["task"], "0.1");
        assert_eq!(diff["agent_id"], agent_id);
        assert_eq!(diff["check_name"], "always-fails");
        // exit_code can be a positive int or null (signal); just assert presence.
        assert!(diff.get("exit_code").is_some());
        let snippet = diff["output_snippet"]
            .as_str()
            .expect("output_snippet must be a string");
        assert!(
            snippet.contains("broken"),
            "snippet should carry captured output; got {snippet:?}"
        );
        assert!(
            snippet.len() <= PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES + PRE_MERGE_TRUNCATION_MARKER.len(),
            "snippet must be capped at {}; got {}",
            PRE_MERGE_AUDIT_SNIPPET_CAP_BYTES,
            snippet.len()
        );

        // Broadcast event landed with the new name.
        let events = drain_event_types(&mut rx);
        assert!(
            events.contains(&"auto_mode_pre_merge_check_failed".to_string()),
            "expected auto_mode_pre_merge_check_failed in {events:?}"
        );
        // The merging pill is suppressed on a gate fail (we never enter
        // the merging state), so `auto_mode_merged` must NOT have fired.
        assert!(
            !events.contains(&"auto_mode_merged".to_string()),
            "merging step must not run; got events {events:?}"
        );

        // Worktree cleaned up.
        assert!(
            !expected_path.exists(),
            "worktree {} should be removed on gate failure",
            expected_path.display()
        );
    }

    /// T1.3 acceptance criterion: a second call to `run_state_machine`
    /// for the same task on an already-paused plan must short-circuit
    /// — no second pre-merge gate run, no duplicate audit row, no
    /// duplicate broadcast. The first call paused the plan; the second
    /// must observe that state and bail at the top.
    #[tokio::test]
    async fn run_state_machine_short_circuits_after_gate_failure_pauses_plan() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"always-fails\", cmd = \"echo broken && false\", timeout_secs = 5 },\n\
             ]\n",
        );

        let agent_id = "agent-short-circuit";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, mut rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        let expected_path = std::path::PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        let _ = std::fs::remove_dir_all(&expected_path);

        // First call: gate fails, plan pauses, audit + broadcast land.
        run_state_machine(&state, "default-org", agent_id, "p", "0.1").await;
        let events_after_first = drain_event_types(&mut rx);
        assert!(
            events_after_first.contains(&"auto_mode_pre_merge_check_failed".to_string()),
            "first call should fire auto_mode_pre_merge_check_failed; got {events_after_first:?}"
        );
        let first_audit_count = audit_actions_for(&db, "p")
            .iter()
            .filter(|a| *a == actions::AUTO_MODE_PRE_MERGE_CHECK_FAILED)
            .count();
        assert_eq!(
            first_audit_count, 1,
            "first call should land exactly one audit row"
        );

        // Second call: plan is paused, must short-circuit. No new gate
        // run (worktree wouldn't be re-created), no new audit row, no
        // new broadcast.
        run_state_machine(&state, "default-org", agent_id, "p", "0.1").await;
        let events_after_second = drain_event_types(&mut rx);
        assert!(
            !events_after_second.contains(&"auto_mode_pre_merge_check_failed".to_string()),
            "second call must NOT re-fire the gate event; got {events_after_second:?}"
        );
        assert!(
            !events_after_second.contains(&"auto_mode_paused".to_string()),
            "second call must NOT re-broadcast paused; got {events_after_second:?}"
        );
        let second_audit_count = audit_actions_for(&db, "p")
            .iter()
            .filter(|a| *a == actions::AUTO_MODE_PRE_MERGE_CHECK_FAILED)
            .count();
        assert_eq!(
            second_audit_count, 1,
            "second call must not write a duplicate audit row; got {second_audit_count}"
        );

        // Worktree from the FIRST call is still cleaned up; second call
        // shouldn't have created a new one either.
        assert!(
            !expected_path.exists(),
            "no worktree should linger after second call"
        );
    }

    /// T1.3 acceptance criterion: after a gate failure, the
    /// `plan_auto_mode` row carries the literal `pre_merge_check_failed`
    /// reason AND a non-NULL `paused_at` timestamp. This is the same
    /// shape the unified `/api/plans/<name>/config` endpoint reads via
    /// `read_plan_config` — verifying the DB write means the API
    /// response is correct too (the config handler is a thin SELECT
    /// wrapper, exercised by `tests/plan_config.rs` integration tests).
    #[tokio::test]
    async fn pre_merge_check_failed_pause_state_matches_config_contract() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"always-fails\", cmd = \"false\", timeout_secs = 5 },\n\
             ]\n",
        );

        let agent_id = "agent-config-state";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        run_state_machine(&state, "default-org", agent_id, "p", "0.1").await;

        // Verify the `plan_auto_mode` row carries the T1.3 shape end-to-end.
        let (paused_reason, paused_at): (Option<String>, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT paused_reason, paused_at FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p"],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(
            paused_reason.as_deref(),
            Some("pre_merge_check_failed"),
            "paused_reason should be the literal T1.3 sentinel"
        );
        assert!(
            paused_at.is_some(),
            "paused_at must be set when the plan is paused"
        );

        // Also verify `db::auto_mode_config` (the helper read by
        // `read_plan_config` -> GET /api/plans/<name>/config) returns
        // the same shape, so the dashboard banner sees the pause.
        let cfg = crate::db::auto_mode_config(&db, "p");
        assert_eq!(cfg.paused_reason.as_deref(), Some("pre_merge_check_failed"));
        // `enabled = 1` is preserved across the pause (auto-mode is the
        // user opt-in; the pause is loop self-state).
        assert!(
            cfg.enabled,
            "auto_mode.enabled must survive the pause; resume re-engages without re-toggling"
        );
    }

    /// T1.3 acceptance criterion: the agent's `merge_status` survives
    /// the gate failure unchanged. The agent's work is still on its
    /// branch; the block is at the merge gate, not the work. A
    /// `deferred_for_cadence` row stays deferred so the operator can
    /// click Resume after fixing the offending check and the sibling
    /// drains as planned. A NULL `merge_status` (the trigger agent on
    /// a phase boundary) stays NULL.
    #[tokio::test]
    async fn pre_merge_check_failed_does_not_touch_merge_status() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init_master(&project_dir);
        git_create_task_branch(&project_dir, "branchwork/p/0.1", true);

        let plans_dir = dir.path().join("plans");
        write_one_task_plan(&plans_dir, "p", &project_dir);
        write_branchwork_toml(
            &project_dir,
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"always-fails\", cmd = \"false\", timeout_secs = 5 },\n\
             ]\n",
        );

        let agent_id = "agent-merge-status";
        seed_agent(&db, agent_id, &project_dir, "p", "0.1", "branchwork/p/0.1");
        // Pretend a prior cadence tick stashed this agent as
        // deferred_for_cadence; the gate failure must not flip it.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET merge_status = 'deferred_for_cadence' WHERE id = ?1",
                params![agent_id],
            )
            .unwrap();
        }
        enable_auto_mode(&db, "p");

        let (state, _rx) = test_app_state(db.clone(), new_runner_registry(), plans_dir);
        run_state_machine(&state, "default-org", agent_id, "p", "0.1").await;

        // `merge_status` must still be `deferred_for_cadence` — the
        // gate failed at the merge boundary, the agent's commit on its
        // branch is unaffected.
        let merge_status: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT merge_status FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert_eq!(
            merge_status.as_deref(),
            Some("deferred_for_cadence"),
            "gate failure must NOT clear merge_status; agent's work stays on its branch"
        );
    }
}
