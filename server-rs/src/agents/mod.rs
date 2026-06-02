pub mod check_agent;
pub mod driver;
pub mod git_ops;
pub mod learnings_context;
pub mod phase_check;
pub mod prompt;
pub mod pty_agent;
pub mod session_protocol;
pub mod session_settings;
pub mod spawn_ops;
pub mod supervisor;
pub mod terminal_ws;
pub mod worktree;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;

use crate::config::Effort;
use crate::db::{Db, completed_task_numbers};
use crate::plan_parser::{self, ParsedPlan, PlanPhase, PlanTask};
use crate::ws::broadcast_event;
use driver::DriverRegistry;
use session_protocol::Message as SessionMessage;

pub type AgentId = String;

/// Cross-platform "is this PID still alive" check.
///
/// On Unix we use `kill(pid, 0)` — it sends no signal but succeeds if the
/// kernel would let us signal the process. On Windows we open a handle with
/// `PROCESS_QUERY_LIMITED_INFORMATION` (the cheapest access right that lets
/// us probe existence) and call `GetExitCodeProcess`; if the kernel reports
/// `STILL_ACTIVE` the process is running. Both calls share the small
/// PID-reuse hazard inherent to PID-based liveness checks across all OSes
/// — for our supervisor lifecycle we accept it. Anything outside the
/// `1..=u32::MAX` window short-circuits to `false`.
pub fn process_alive(pid: i64) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        if pid <= 0 || pid > u32::MAX as i64 {
            return false;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let got = GetExitCodeProcess(h, &mut exit_code);
            CloseHandle(h);
            got != 0 && exit_code == STILL_ACTIVE as u32
        }
    }
}

/// Cross-platform "politely ask this PID to stop" (SIGTERM on Unix,
/// `taskkill` on Windows). Best-effort — failures are swallowed.
pub fn process_terminate(pid: i64) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status();
    }
}

/// Get the current HEAD commit SHA in the given directory.
/// Returns None if the directory is not a git repo or git is unavailable.
/// Ensure the given directory is a git repository. If it isn't, run
/// `git init`, stage everything, and create an initial commit so that
/// branch isolation and diffs work for this project.
///
/// Returns true if the directory was already a repo or was successfully
/// initialized. Safe to call repeatedly.
pub fn ensure_git_initialized(cwd: &std::path::Path) -> bool {
    // Already a repo?
    let in_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    if matches!(&in_repo, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
    {
        return true;
    }

    // git init
    let init = std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(cwd)
        .output();
    if !matches!(&init, Ok(o) if o.status.success()) {
        eprintln!("[Branchwork] git init failed in {}", cwd.display());
        return false;
    }

    // Set a minimal identity if none exists globally — git commit will refuse otherwise
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "branchwork@localhost"])
        .current_dir(cwd)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "Branchwork"])
        .current_dir(cwd)
        .output();

    // Stage and make an empty initial commit — gives us a HEAD so branches can be created
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .output();
    let commit = std::process::Command::new("git")
        .args([
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit (Branchwork)",
        ])
        .current_dir(cwd)
        .output();
    match commit {
        Ok(o) if o.status.success() => {
            println!("[Branchwork] Initialized git repo in {}", cwd.display());
            true
        }
        _ => {
            eprintln!("[Branchwork] initial commit failed in {}", cwd.display());
            false
        }
    }
}

pub fn git_head_sha(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the current branch name in the given directory.
pub fn git_current_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name == "HEAD" {
            // Detached HEAD — not on a branch
            None
        } else {
            Some(name)
        }
    } else {
        None
    }
}

// `git_default_branch` lives in `crate::git_helpers` (a leaf module shared
// with the runner binary). Re-export it here so the existing call sites in
// `pty_agent.rs` and `api/agents.rs` keep compiling without churn —
// `git_list_branches` is reached only through the dispatcher in
// `agents/git_ops.rs` so it's not re-exported.
pub use crate::git_helpers::git_default_branch;

/// Create or checkout a git branch **in-place**. Returns true if successful.
/// For "start" mode: creates the branch (or checks it out if it already exists).
/// For "continue" mode: checks out the existing branch.
///
/// # Deprecated
///
/// In-place checkout mutates the single shared project working tree, which is
/// the root cause of the parallel-agent contention this plan fixes. It is
/// retained **only** for the legacy `BRANCHWORK_USE_WORKTREES=0` code path and
/// is superseded by [`setup_agent_worktree`], which gives each agent its own
/// isolated worktree. Deprecation window: one minor version — slated for
/// removal in `0.6.0`. Do not add new callers.
#[deprecated(
    since = "0.5.100",
    note = "use `setup_agent_worktree` for the worktree-per-agent start path; \
            `git_checkout_branch` is retained only for the legacy \
            BRANCHWORK_USE_WORKTREES=0 path and is slated for removal in 0.6.0"
)]
pub fn git_checkout_branch(cwd: &std::path::Path, branch: &str, is_continue: bool) -> bool {
    if is_continue {
        // Try to checkout the existing branch
        let status = std::process::Command::new("git")
            .args(["checkout", branch])
            .current_dir(cwd)
            .output();
        match status {
            Ok(output) if output.status.success() => {
                println!("[Branchwork] Checked out existing branch: {branch}");
                return true;
            }
            _ => {
                // Branch doesn't exist yet — fall through to create it
                println!("[Branchwork] Branch {branch} not found for continue, creating it");
            }
        }
    }

    // Try to create the branch
    let status = std::process::Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(cwd)
        .output();
    match status {
        Ok(output) if output.status.success() => {
            println!("[Branchwork] Created and checked out branch: {branch}");
            true
        }
        _ => {
            // Branch already exists — just check it out
            let fallback = std::process::Command::new("git")
                .args(["checkout", branch])
                .current_dir(cwd)
                .output();
            match fallback {
                Ok(output) if output.status.success() => {
                    println!("[Branchwork] Checked out existing branch: {branch}");
                    true
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[Branchwork] Failed to checkout branch {branch}: {stderr}");
                    false
                }
                Err(e) => {
                    eprintln!("[Branchwork] Failed to run git checkout: {e}");
                    false
                }
            }
        }
    }
}

/// Path segment substituted for a freestanding agent that carries no plan
/// name, so its worktree still lands at a deterministic location instead of an
/// empty path component.
///
/// `pub(crate)` so the SaaS worktree dispatcher
/// ([`crate::agents::git_ops::setup_worktree`]) resolves the same placeholder
/// before shipping the resolved segment over the wire — the runner has no
/// access to this constant.
pub(crate) const NO_PLAN_SEGMENT: &str = "_no-plan";
/// Path segment substituted for an agent with no task id — see
/// [`NO_PLAN_SEGMENT`].
pub(crate) const NO_TASK_SEGMENT: &str = "_no-task";

/// Set up the per-agent git worktree for a spawning agent and return its
/// absolute on-disk path. This is the worktree-per-agent replacement for the
/// in-place [`git_checkout_branch`] start path (used when
/// `BRANCHWORK_USE_WORKTREES` is not `0`).
///
/// Thin adapter over [`worktree::setup_worktree`]: the only thing it adds is
/// substituting deterministic placeholders for the rare freestanding agent
/// that has no plan ([`NO_PLAN_SEGMENT`]) or no task ([`NO_TASK_SEGMENT`]), so
/// the resolved worktree path component is never empty. Branch / continue
/// semantics and `$BRANCHWORK_WORKTREE_BASE` resolution are entirely owned by
/// `worktree::setup_worktree`.
#[allow(clippy::too_many_arguments)] // 8-arg signature mirrors `worktree::setup_worktree`
pub fn setup_agent_worktree(
    project_dir: &Path,
    project_slug: &str,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    agent_id: &str,
    branch: &str,
    base_branch: Option<&str>,
    is_continue: bool,
) -> Result<PathBuf, worktree::WorktreeError> {
    worktree::setup_worktree(
        project_dir,
        project_slug,
        plan_name.unwrap_or(NO_PLAN_SEGMENT),
        task_id.unwrap_or(NO_TASK_SEGMENT),
        agent_id,
        branch,
        base_branch,
        is_continue,
    )
}

/// Inner [`setup_agent_worktree`] against an explicit worktree `base`,
/// delegating to [`worktree::setup_worktree_in`]. The public
/// [`setup_agent_worktree`] resolves `base` from the environment first; tests
/// call this directly to pin a tempdir without mutating process env — the same
/// seam `worktree::setup_worktree_in` exposes, never via
/// `BRANCHWORK_WORKTREE_BASE` (a process-wide env write would race a parallel
/// test binary).
#[allow(clippy::too_many_arguments)] // 8-arg signature mirrors `worktree::setup_worktree_in`
pub(crate) fn setup_agent_worktree_in(
    base: &Path,
    project_dir: &Path,
    project_slug: &str,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    agent_id: &str,
    branch: &str,
    base_branch: Option<&str>,
    is_continue: bool,
) -> Result<PathBuf, worktree::WorktreeError> {
    worktree::setup_worktree_in(
        base,
        project_dir,
        project_slug,
        plan_name.unwrap_or(NO_PLAN_SEGMENT),
        task_id.unwrap_or(NO_TASK_SEGMENT),
        agent_id,
        branch,
        base_branch,
        is_continue,
    )
}

/// `merge_status` value marking a worktree that has been handed off to the
/// Phase 3 at-merge conflict resolver. The kill/fail cleanup
/// ([`cleanup_worktree_for_terminated_agent`]) skips any agent row carrying it
/// so the resolver keeps the conflicted tree to work in. **Phase 3.2 wires the
/// hand-off contract** (it sets this marker when a merge conflict hands the
/// worktree off); until then no row ever carries it, so the guard is a
/// forward-looking no-op today.
pub(crate) const MERGE_STATUS_HELD_FOR_RESOLVER: &str = "held_for_resolver";

/// Best-effort removal of a per-agent worktree, given its `cwd`. The
/// path-based core shared by every "this worktree has done its job" route:
/// the merge-success path ([`crate::api::agents::merge_agent_branch_inner`],
/// which every merge route converges on — manual dashboard merge, the
/// auto-mode loop, and the cadence batch drain) and the kill / fail paths
/// (via [`cleanup_worktree_for_terminated_agent`]).
///
/// No-op (returns without touching anything) in every case where there is no
/// linked worktree to remove:
/// - `BRANCHWORK_USE_WORKTREES=0` — the legacy in-place start path never
///   created a worktree; the agent's cwd *is* the shared project tree and must
///   never be removed.
/// - `agent_cwd` is the **main** worktree (its `.git` is a directory, not a
///   `gitdir:` pointer file) — a row spawned in-place, or a freestanding agent
///   that ran in the project root. Removing the project root would be
///   catastrophic, so the `.git`-is-a-file check is the load-bearing safety belt.
/// - `agent_cwd` does not exist / is not a git worktree on this host — e.g.
///   SaaS, where the worktree lives on the runner and removal is dispatched
///   there (worktree-per-agent Phase 2.7), so the server-side path is absent.
///
/// On a genuine linked worktree it shells out via [`worktree::remove_worktree`],
/// which escalates force as needed and logs the outcome (`Removed` /
/// `ForceRemoved` / `ManuallyRemoved`) at info level. A failure is logged but
/// never propagated — a leaked worktree is reaped by the startup orphan sweep.
/// The task branch ref is left intact (`git worktree remove` drops only the
/// working tree, not the branch), so a merged or recoverable branch survives.
///
/// **Resolver hand-off:** callers that might be removing a conflicted worktree
/// destined for the Phase 3 resolver must gate on
/// [`MERGE_STATUS_HELD_FOR_RESOLVER`] before calling this — see
/// [`cleanup_worktree_for_terminated_agent`]. The merge-success caller never
/// reaches this on conflict (it returns early in `merge_agent_branch_inner`),
/// so a conflicted worktree stays in place there too.
pub(crate) fn remove_agent_worktree(agent_cwd: &Path) {
    // Honour the legacy escape hatch explicitly (task contract). Even with the
    // env var unset (worktrees on by default) the `.git`-is-a-file check below
    // is the real safety belt; this is the cheap, explicit early-out.
    if !pty_agent::worktrees_enabled() {
        return;
    }

    // A linked worktree's `.git` is a *file* (`gitdir: …`); the main worktree's
    // `.git` is a *directory*. Only ever remove a linked worktree. A missing
    // `.git` also lands here (path absent in SaaS, or not a repo) → skip.
    if !agent_cwd.join(".git").is_file() {
        return;
    }

    // Find the main worktree to run `git worktree remove` from — git refuses to
    // remove the worktree you're standing in, so we cannot run it from
    // `agent_cwd` itself.
    let Some(main_root) = main_worktree_root(agent_cwd) else {
        eprintln!(
            "[worktree] could not resolve the main worktree for {} — skipping \
             cleanup (startup orphan sweep will reap it)",
            agent_cwd.display()
        );
        return;
    };

    if let Err(e) = worktree::remove_worktree(&main_root, agent_cwd) {
        eprintln!(
            "[worktree] cleanup of {} failed: {e} — leaving for the \
             startup orphan sweep",
            agent_cwd.display()
        );
    }
}

/// Remove the per-agent worktree for an agent that was **killed** or
/// **failed**, looked up by `agent_id`. Reads the agent row's `cwd` and
/// delegates to [`remove_agent_worktree`] (which carries the
/// `.git`-is-a-file safety belt, main-worktree resolution, and force
/// escalation). Best-effort: every failure is logged inside the core and
/// never propagated to the caller.
///
/// Call sites (every terminal transition that won't be followed by a merge):
/// - [`AgentRegistry::kill_agent`] — explicit user kill mid-task.
/// - `pty_agent::start_pty_agent` failure branches — the agent failed to
///   start (supervisor spawn error / socket connect error), so its freshly
///   created worktree would otherwise leak.
/// - `pty_agent::on_agent_exit` failed branches — the agent ran then crashed
///   (supervisor SIGKILL'd / non-zero CLI exit); no merge is coming.
///
/// **Resolver exception:** skips removal when the row's `merge_status` is
/// [`MERGE_STATUS_HELD_FOR_RESOLVER`], i.e. the worktree has been handed to
/// the Phase 3 at-merge conflict resolver, which needs the conflicted tree to
/// keep working in. Phase 3.2 sets that marker; until then no row carries it,
/// so the guard is a forward-looking no-op.
///
/// Not wired into the boot-reconcile (`mark_orphaned` / `mark_supervisor_died`)
/// or heartbeat (`mark_supervisor_unreachable`) paths: a heartbeat-unreachable
/// supervisor may still have a live CLI child writing into the tree, and
/// boot-time orphan reaping is owned by the dedicated startup orphan sweep
/// ([`worktree::list_orphans`]) which canonicalises paths against the live-cwd
/// set rather than removing per row.
pub(crate) fn cleanup_worktree_for_terminated_agent(db: &Db, agent_id: &str) {
    let row: Option<(Option<String>, Option<String>)> = {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT cwd, merge_status FROM agents WHERE id = ?1",
            rusqlite::params![agent_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .ok()
    };
    let Some((Some(cwd), merge_status)) = row else {
        return;
    };
    if merge_status.as_deref() == Some(MERGE_STATUS_HELD_FOR_RESOLVER) {
        // Handed to the Phase 3 at-merge resolver — leave the worktree in place.
        return;
    }
    remove_agent_worktree(Path::new(&cwd));
}

// ── At-merge conflict resolver (Phase 3.2) ──────────────────────────────────

/// Prompt template for the at-merge conflict resolver. Placeholders
/// (`{{original_task_description}}`, `{{target_branch}}`,
/// `{{conflicted_paths_list}}`, `{{task_id}}`, `{{our_diff}}`,
/// `{{their_diff}}`) are substituted by [`build_resolver_prompt`].
const RESOLVE_CONFLICT_TEMPLATE: &str = include_str!("../prompts/resolve_conflict.md");

/// The structured conflict context captured by `merge_branch_local` (Task
/// 3.1) and handed to the at-merge resolver: the unmerged paths plus both
/// sides' diffs against the merge base. Mirrors the fields of
/// [`crate::saas::runner_protocol::MergeOutcome::Conflict`]; the call site in
/// [`crate::api::agents::merge_agent_branch_inner`] builds one from that wire
/// variant.
#[derive(Debug, Clone)]
pub struct ConflictPayload {
    /// Files git left in an unmerged state during the failed merge.
    pub conflicted_paths: Vec<String>,
    /// `git diff <merge-base> <task_branch>` — the task branch's changes.
    pub our_diff: String,
    /// `git diff <merge-base> <target>` — the target branch's changes.
    pub their_diff: String,
}

/// Why [`spawn_conflict_resolver`] declined to spawn. The merge call site
/// reacts to each: `CapHit` falls through to the permanent-pause path
/// (Task 3.3); `NoParentAgent` is logged and the conflict surfaces unchanged.
#[derive(Debug)]
pub enum SpawnError {
    /// The task has already used its `plan_auto_mode.max_fix_attempts`
    /// budget (shared with the CI fix-on-red loop) — no resolver is spawned.
    CapHit,
    /// No original task agent row was found for `(plan_name, task_id)`, so
    /// there is no parent to thread the resolver under.
    NoParentAgent,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::CapHit => write!(f, "fix-attempt cap reached"),
            SpawnError::NoParentAgent => write!(f, "no parent task agent found"),
        }
    }
}

/// What the merge-conflict arm did with a conflict, surfaced on
/// [`crate::api::agents::MergeOutcome::resolver`] so the auto-mode loop
/// (`run_merge_step`) can decide whether to pause (Task 3.3). `None` on
/// the outcome means no resolver path applied at all — manual merge,
/// SaaS, worktrees disabled, non-auto-mode plan, or no plan/task — in
/// which case the loop keeps its pre-3.3 `merge_conflict` pause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverDisposition {
    /// A resolver agent was spawned in the conflicted worktree. The
    /// auto-mode loop must **not** pause — leave the plan running so the
    /// resolver's completion (`on_resolver_agent_completed`) can re-attempt
    /// the merge.
    Spawned {
        /// The spawned resolver agent id — carried so the loop can audit
        /// / thread the attempt.
        resolver_agent_id: AgentId,
        /// 1-based attempt number (shared `task_fix_attempts` budget).
        attempt: u32,
    },
    /// The shared `task_fix_attempts` cap is spent; no resolver was spawned.
    /// The auto-mode loop pauses the plan with reason
    /// `merge_conflict_unresolved`, carrying the conflicted paths so the
    /// dashboard can surface them. The conflicted worktree is left on disk
    /// (markers preserved) for human inspection.
    CapHit {
        /// The attempt number that would have been spawned (= count + 1) —
        /// for the pause audit/broadcast.
        attempt: u32,
        /// Files git left unmerged — surfaced in the pause broadcast.
        conflicted_paths: Vec<String>,
    },
}

/// Substitute the resolver prompt template's placeholders. Pure (no I/O) so
/// the prompt shape is unit-testable without spawning anything.
pub fn build_resolver_prompt(
    original_task_description: &str,
    target_branch: &str,
    task_id: &str,
    conflict: &ConflictPayload,
) -> String {
    let conflicted_paths_list = if conflict.conflicted_paths.is_empty() {
        "(none reported)".to_string()
    } else {
        conflict
            .conflicted_paths
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    RESOLVE_CONFLICT_TEMPLATE
        .replace("{{original_task_description}}", original_task_description)
        .replace("{{target_branch}}", target_branch)
        .replace("{{conflicted_paths_list}}", &conflicted_paths_list)
        .replace("{{task_id}}", task_id)
        .replace("{{our_diff}}", &conflict.our_diff)
        .replace("{{their_diff}}", &conflict.their_diff)
}

/// Load the original task's description from its plan YAML so the resolver
/// prompt can frame the conflict in terms of the task's intent. Returns
/// `None` (caller substitutes a placeholder) when `app_state` isn't wired
/// (unit-test registries) or the plan / task can't be read.
fn load_task_description(
    registry: &AgentRegistry,
    plan_name: &str,
    task_id: &str,
) -> Option<String> {
    let state = registry.app_state.get()?;
    let plan_path = state.plans_dir.join(format!("{plan_name}.yaml"));
    let plan = plan_parser::parse_plan_file(&plan_path).ok()?;
    plan.phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .find(|t| t.number == task_id)
        .map(|t| t.description.clone())
}

/// Re-create the merge conflict on disk in the agent's worktree.
///
/// Task 3.1's `merge_branch_local` runs `git merge --abort` after capturing
/// the structured payload, so no conflict markers survive on disk. The
/// agent's worktree is still on the task branch with its committed work, so
/// merging the target branch back in reproduces the same conflict (markers +
/// unmerged `UU` index entries) the resolver's prompt instructs it to fix.
///
/// Best-effort: a clean (non-conflicting) merge just leaves a tidy tree the
/// resolver finds already resolved, and any git error is logged and ignored —
/// the prompt still carries both diffs, so the resolver has its context
/// either way.
fn reconstruct_merge_conflict(worktree_path: &Path, target_branch: &str) {
    let out = std::process::Command::new("git")
        .args(["merge", target_branch, "--no-edit"])
        .current_dir(worktree_path)
        .output();
    match out {
        Ok(o) if o.status.success() => eprintln!(
            "[resolver] reconstruct: `git merge {target_branch}` in {} merged cleanly \
             (no conflict to reproduce)",
            worktree_path.display()
        ),
        Ok(_) => { /* expected: non-zero exit means the conflict is now on disk */ }
        Err(e) => eprintln!(
            "[resolver] reconstruct: failed to run `git merge {target_branch}` in {}: {e}",
            worktree_path.display()
        ),
    }
}

/// `git rev-parse HEAD` in `worktree_path`. `None` when the command fails
/// (not a repo, detached/unborn HEAD).
fn worktree_head_sha(worktree_path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(worktree_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// The paths git currently considers unmerged (conflicted) in
/// `worktree_path`, via `git diff --name-only --diff-filter=U` — exactly the
/// files a human will find carrying `<<<<<<<` markers if they inspect the
/// tree (the same probe the rebase-conflict path uses). Returned in git's
/// order (sorted by path). Any git error yields an empty `Vec` so a transient
/// failure never masquerades as a conflict; the new-commit check in
/// [`resolver_resolution_succeeded`] is the independent second gate.
fn worktree_unmerged_paths(worktree_path: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(worktree_path)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `worktree_path` still carries unmerged (conflicted) index
/// entries — i.e. the resolver left a `UU`-style conflict unresolved. Thin
/// wrapper over [`worktree_unmerged_paths`]: a non-empty result means the
/// resolution is incomplete.
fn worktree_has_unmerged_paths(worktree_path: &Path) -> bool {
    !worktree_unmerged_paths(worktree_path).is_empty()
}

/// Decide whether an at-merge resolver agent actually resolved its
/// conflict — the gate Task 3.3 runs before re-attempting the merge.
///
/// Success requires **all three** to hold:
/// - `exited_clean` — the resolver process exited 0 (its agent row is
///   `completed`, not `failed`).
/// - no unmerged paths in the worktree ([`worktree_has_unmerged_paths`]) —
///   the resolver left no `UU` conflict behind.
/// - `git rev-parse HEAD` differs from `base_commit` — the resolver
///   committed a new resolution on top of the task branch (`base_commit`
///   is the resolver agent's `base_commit` column, captured at spawn time
///   *before* the resolution while the worktree was still on the
///   pre-resolution task-branch tip).
///
/// Any of: non-zero exit, lingering conflict, missing `base_commit`, or
/// HEAD unchanged ⇒ `false` (the failure path, which recurses into Task 3.2
/// until the cap).
pub fn resolver_resolution_succeeded(
    worktree_path: &Path,
    base_commit: Option<&str>,
    exited_clean: bool,
) -> bool {
    if !exited_clean {
        return false;
    }
    if worktree_has_unmerged_paths(worktree_path) {
        return false;
    }
    let Some(base) = base_commit else {
        return false;
    };
    match worktree_head_sha(worktree_path) {
        Some(head) => head != base,
        None => false,
    }
}

/// Spawn an at-merge conflict resolver agent for a task whose branch failed
/// to merge cleanly.
///
/// Looks up the shared `task_fix_attempts` budget for the task and returns
/// [`SpawnError::CapHit`] without spawning once `plan_auto_mode.max_fix_attempts`
/// is exhausted. Otherwise it records the attempt, composes the prompt from
/// [`RESOLVE_CONFLICT_TEMPLATE`] (task intent + conflicted paths + both
/// diffs), reconstructs the conflict on disk in `worktree_path` (Task 3.1
/// aborted the real merge), and spawns a PTY agent **in that same worktree**
/// (`branch = None` so `start_pty_agent` reuses the conflicted tree rather
/// than adding a fresh worktree — the target branch is already checked out in
/// the main worktree, so a fresh `git worktree add <target>` would fail). The
/// spawned row is badged `mode = 'resolver'` and threaded under the original
/// task agent via `parent_agent_id` so the dashboard can render it distinctly.
///
/// Standalone path only (`start_pty_agent`); the SaaS dispatch caller gates
/// this out — see [`crate::api::agents::merge_agent_branch_inner`].
#[allow(clippy::too_many_arguments)]
pub async fn spawn_conflict_resolver(
    registry: &AgentRegistry,
    plan_name: &str,
    task_id: &str,
    target_branch: &str,
    conflict: &ConflictPayload,
    attempt: u32,
    worktree_path: &Path,
) -> Result<AgentId, SpawnError> {
    // 1. Cap check — a conflict resolver counts against the same
    //    `task_fix_attempts` / `plan_auto_mode.max_fix_attempts` budget as a
    //    CI fix agent. `count >= max` means the budget is spent; the caller
    //    turns `CapHit` into a permanent pause (Task 3.3). `attempt` is the
    //    1-based number of this resolver (the caller passes `count + 1`).
    let max = crate::db::plan_max_fix_attempts(&registry.db, plan_name);
    let count = crate::db::task_fix_attempt_count(&registry.db, plan_name, task_id);
    if count >= max {
        return Err(SpawnError::CapHit);
    }

    // 2. Find the original task agent — the parent the dashboard threads the
    //    resolver under. Most-recent row for this exact (plan, task); fix /
    //    resolver rows carry suffixed task ids (`-fix-N` / `-resolve-N`) so an
    //    exact `task_id` match never picks one of them up. Capture its org
    //    for the spawn.
    let (parent_agent_id, org_id): (String, String) = {
        let conn = registry.db.lock().unwrap();
        conn.query_row(
            "SELECT id, org_id FROM agents \
             WHERE plan_name = ?1 AND task_id = ?2 \
             ORDER BY started_at DESC LIMIT 1",
            rusqlite::params![plan_name, task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| SpawnError::NoParentAgent)?
    };

    // 3. Record the attempt BEFORE the spawn (agent_id NULL until the spawn
    //    returns) so a kill mid-spawn still increments the cap count. PK
    //    (plan, task, attempt) makes this idempotent on retry.
    {
        let conn = registry.db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_fix_attempts \
                (plan_name, task_number, attempt, started_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, task_number, attempt) DO NOTHING",
            rusqlite::params![plan_name, task_id, attempt as i64],
        )
        .ok();
    }

    // 4. Compose the prompt. Task description comes from the plan YAML (the
    //    authoritative source of task intent); fall back to a placeholder when
    //    it isn't reachable (no app_state / missing plan).
    let original_task_description = load_task_description(registry, plan_name, task_id)
        .unwrap_or_else(|| "(original task description unavailable)".to_string());
    let prompt =
        build_resolver_prompt(&original_task_description, target_branch, task_id, conflict);

    // 5. Re-create the conflict on disk so the prompt's instructions match
    //    what the resolver sees in its working tree.
    reconstruct_merge_conflict(worktree_path, target_branch);

    // 6. Spawn in the agent's existing worktree (see the fn doc for the
    //    `branch = None` rationale). The `-resolve-<n>` task-id marker keeps
    //    the resolver distinguishable from the original task agent on the
    //    completion path.
    let resolver_task_id = format!("{task_id}-resolve-{attempt}");
    let effort = registry
        .app_state
        .get()
        .map(|s| *s.effort.lock().unwrap())
        .unwrap_or(Effort::High);
    let resolver_id = pty_agent::start_pty_agent(
        registry,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: worktree_path,
            plan_name: Some(plan_name),
            task_id: Some(&resolver_task_id),
            effort,
            branch: None,
            is_continue: true,
            max_budget_usd: None,
            driver: None,
            user_id: None,
            org_id: Some(&org_id),
            runner_id: None,
        },
    )
    .await;

    // 7. Badge the row as a resolver threaded under its parent. Mark it
    //    `held_for_resolver` too: the resolver shares the conflicted
    //    worktree with the original agent, so if the resolver's own
    //    supervisor crashes (`cleanup_worktree_for_terminated_agent` keys on
    //    the resolver's own `merge_status`, not the parent's), the marker
    //    keeps that path from yanking the shared tree out from under the
    //    pending resolution. The merge-success cleanup
    //    (`remove_agent_worktree`, keyed on `cwd`) still removes it once the
    //    re-merge lands.
    {
        let conn = registry.db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET mode = 'resolver', parent_agent_id = ?2, merge_status = ?3 \
             WHERE id = ?1",
            rusqlite::params![resolver_id, parent_agent_id, MERGE_STATUS_HELD_FOR_RESOLVER],
        )
        .ok();
    }

    // 8. Backfill the attempt row's agent_id (was NULL at step 3).
    {
        let conn = registry.db.lock().unwrap();
        conn.execute(
            "UPDATE task_fix_attempts SET agent_id = ?1 \
             WHERE plan_name = ?2 AND task_number = ?3 AND attempt = ?4",
            rusqlite::params![resolver_id, plan_name, task_id, attempt as i64],
        )
        .ok();
    }

    Ok(resolver_id)
}

/// Resolve the main worktree's root directory by scanning
/// `git worktree list --porcelain` for the entry whose `.git` is a real
/// directory (the main worktree), as opposed to the `gitdir:` pointer files
/// linked worktrees carry. Ordering of the porcelain output is *not* reliable
/// across git versions (git 2.43 lists the *current* worktree first, not the
/// main one), so the main worktree is identified structurally rather than
/// positionally.
///
/// Returns `None` when `from` is not a git worktree or the command fails.
///
/// `pub(crate)` so the server-boot orphan sweep (`api::orphan_worktrees`)
/// can derive each project's stable root from a (possibly orphaned) agent
/// worktree cwd, the same way merge / kill cleanup does.
pub(crate) fn main_worktree_root(from: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(from)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            let p = PathBuf::from(rest);
            if p.join(".git").is_dir() {
                return Some(p);
            }
        }
    }
    None
}

#[derive(Clone)]
pub struct AgentRegistry {
    pub agents: Arc<Mutex<HashMap<AgentId, ManagedAgent>>>,
    pub db: Db,
    pub broadcast_tx: tokio::sync::broadcast::Sender<String>,
    /// Optional Slack/webhook URL. When set, agent-completion and phase-advance
    /// events fan out a POST here in addition to the in-process WS broadcast.
    /// Wrapped in `RwLock` so `PUT /api/settings` can update it live without
    /// a server restart; readers (notify call sites) take a brief read lock.
    pub webhook_url: Arc<RwLock<Option<String>>>,
    /// Directory where per-agent supervisor sockets (and their `.log` / `.pid`
    /// siblings) live. Created on startup.
    pub sockets_dir: PathBuf,
    /// Absolute path to the running server binary. Used to respawn the
    /// `session` subcommand as a detached daemon.
    pub server_exe: PathBuf,
    /// TCP port the dashboard's HTTP listener (and MCP endpoint) is bound
    /// to. Used by drivers that auto-register the Branchwork MCP server
    /// with the spawned CLI.
    pub port: u16,
    /// Available agent drivers, keyed by name. Built once at startup with
    /// [`DriverRegistry::with_defaults`]; clones share the underlying Arc.
    pub drivers: DriverRegistry,
    /// Whether to spawn agents with `--dangerously-skip-permissions` (or the
    /// driver's equivalent). Toggled live from the dashboard via
    /// `PUT /api/settings`.
    pub skip_permissions: Arc<AtomicBool>,
    /// Lazily-set reference to the global [`crate::state::AppState`].
    /// `main.rs` populates it after `AppState::new()` returns; remains
    /// unset in test fixtures that build only an `AgentRegistry`. The
    /// auto-mode completion hook reads this to dispatch the merge with
    /// the shared runner registry + broadcast/db handles.
    ///
    /// Holds `Arc<OnceLock<AppState>>` rather than `Weak`: the registry
    /// and `AppState` both live for the process lifetime, and a `Weak`
    /// would trade graceful liveness for a one-time leak that never
    /// matters. Tests that don't set this skip the auto-mode hook
    /// silently.
    pub app_state: Arc<OnceLock<crate::state::AppState>>,
    /// Test-only override for the worktree base directory. Production leaves
    /// this `None`, and [`setup_agent_worktree`] resolves
    /// `$BRANCHWORK_WORKTREE_BASE` (default `~/.branchwork/worktrees`) itself.
    /// Tests set it to a tempdir so [`pty_agent::start_pty_agent`]'s worktree
    /// path can be exercised without mutating process env — the same race-free
    /// seam [`worktree::setup_worktree_in`] exposes for the primitives.
    pub worktree_base_override: Option<PathBuf>,
}

/// In-process state for a live agent whose PTY runs inside a detached
/// session daemon. Created during [`pty_agent::start_pty_agent`] and
/// dropped on kill or PTY exit.
///
/// The socket path and daemon PID live in the DB (`agents.supervisor_socket`
/// / `agents.pid`), not here — reconnect is always DB-driven so there's no
/// benefit to duplicating them in memory.
pub struct ManagedAgent {
    /// Channel into the writer task that frames messages (`Input`, `Resize`,
    /// `Kill`) and sends them over the socket. Dropping this closes the
    /// writer side and terminates the daemon connection.
    pub command_tx: tokio::sync::mpsc::UnboundedSender<SessionMessage>,
    /// Broadcast of PTY output bytes received from the supervisor. Each
    /// attached terminal WebSocket subscribes for its own receiver.
    pub output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

impl AgentRegistry {
    pub fn new(
        db: Db,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        webhook_url: Option<String>,
        sockets_dir: PathBuf,
        server_exe: PathBuf,
        port: u16,
        skip_permissions: bool,
    ) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            db,
            broadcast_tx,
            webhook_url: Arc::new(RwLock::new(webhook_url)),
            sockets_dir,
            server_exe,
            port,
            drivers: DriverRegistry::with_defaults(),
            skip_permissions: Arc::new(AtomicBool::new(skip_permissions)),
            app_state: Arc::new(OnceLock::new()),
            worktree_base_override: None,
        }
    }

    /// Resolve the per-agent socket path `<sockets_dir>/<id>.sock`.
    pub fn socket_for(&self, agent_id: &str) -> PathBuf {
        self.sockets_dir.join(format!("{agent_id}.sock"))
    }

    /// Resolve the per-agent MCP config path
    /// `<sockets_dir>/<id>.mcp.json`. The file is written by
    /// [`pty_agent::start_pty_agent`] when the driver declares an MCP
    /// config via [`AgentDriver::mcp_config_json`].
    pub fn mcp_config_for(&self, agent_id: &str) -> PathBuf {
        self.sockets_dir.join(format!("{agent_id}.mcp.json"))
    }

    /// Clean up dead agents and reattach alive ones (from previous server runs).
    ///
    /// Every row in `running|starting` is either reattached or marked dead:
    /// - **PTY mode**: if we have a `supervisor_socket`, the daemon PID is
    ///   alive, and we can reconnect, the agent is live again. PID-dead rows
    ///   become `killed`/`supervisor_died` (the supervisor specifically went
    ///   away — usually `kill -9` or OOM); other failures (no socket
    ///   recorded, connect refused) become `failed`/`orphaned`.
    /// - **Remote mode** (SaaS runner-managed agents): the runner owns the
    ///   PID; the server can't probe it locally. Marked
    ///   `killed`/`supervisor_died` and a best-effort `KillAgent` is fanned
    ///   out to every online runner so any in-memory state on a survivor
    ///   matches.
    /// - **Stream-JSON mode** (check agents): the server held the child's
    ///   stdin/stdout; those handles are gone after a crash and there is no
    ///   reattach path. Always orphaned.
    ///
    /// Broadcasting `agent_stopped` for each row is what lets the frontend
    /// unlock task cards — without it, a server crash mid-run leaves
    /// `taskLocked` true forever because the agent row never leaves the
    /// `running`/`starting` set.
    pub async fn cleanup_and_reattach(&self) {
        let rows: Vec<(String, Option<i64>, Option<String>, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db
                .prepare(
                    "SELECT id, pid, supervisor_socket, mode FROM agents \
                     WHERE status IN ('running', 'starting')",
                )
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .flatten()
            .collect()
        };

        // T5.15: leave `mode='remote'` rows alone. Pre-T5.15 we marked
        // them all as `killed/supervisor_died` and fanned out KillAgent
        // to every runner — that was harmless when KillAgent's pid
        // targeting was broken (T5.13 fixed it), but post-T5.13 it
        // actively SIGTERM'd every live daemon on every prod redeploy,
        // and the live SaaS agents the user was watching all died.
        //
        // Liveness for remote agents is the runner's call, not ours:
        // - The runner sends `RunnerHello { active_agents }` on every
        //   (re)connect (T5.14). For each id in that list whose row is
        //   in `killed/failed`, the hello handler flips it back to
        //   `running` — so a row that survives this cleanup pass and
        //   gets confirmed alive by the runner stays running.
        // - For an agent that's truly dead (runner host rebooted, etc.)
        //   the runner will simply not list it on Hello and the row
        //   stays at `running` until the user manually kills it OR the
        //   runner reports `AgentStopped` for it on a later WS frame.
        //   That's a degradation vs the pre-T5.15 "mark everything
        //   dead" posture but the user-visible UX is far better than
        //   the alternative — every prod redeploy was nuking live work.
        //
        // No `remote_dead` collection, no KillAgent fan-out: cleanup is
        // pty-mode + stream-json only.

        for (id, pid, socket, mode) in rows {
            match mode.as_str() {
                "remote" => {
                    // Defer to runner's hello — see comment above.
                    continue;
                }
                "pty" => {}
                _ => {
                    // Stream-JSON agents have no supervisor: the server itself
                    // held their stdin/stdout. After a restart those handles
                    // are gone — even if the PID happens to still be alive
                    // (or has been reused) we cannot talk to it. Orphan
                    // every row unconditionally.
                    self.mark_orphaned(&id, "stream-json agent: IO handles lost on server restart");
                    continue;
                }
            }

            let socket_path = match socket.as_deref() {
                Some(s) => PathBuf::from(s),
                None => {
                    self.mark_orphaned(&id, "no supervisor socket recorded");
                    continue;
                }
            };

            let alive = pid.map(process_alive).unwrap_or(false);
            if !alive {
                self.mark_supervisor_died(
                    &id,
                    &format!("supervisor PID {pid:?} not alive on boot"),
                );
                continue;
            }

            if pty_agent::reattach_agent(self, &id, &socket_path, pid.unwrap_or(0) as u32).await {
                println!(
                    "[Branchwork] Reattached agent {} → socket {}",
                    &id[..8.min(id.len())],
                    socket_path.display()
                );
            } else {
                self.mark_orphaned(
                    &id,
                    &format!("supervisor socket unreachable: {}", socket_path.display()),
                );
            }
        }

        // T5.15: no KillAgent fan-out for remote agents — the
        // pre-T5.15 fanout (combined with T5.13's pid fix) was nuking
        // every live SaaS daemon on every prod redeploy. See the
        // comment in the loop above for the reconciliation strategy.

        // Second pass: normalize task_status rows. A task stuck in `checking`
        // whose check-agent is no longer running is the most common wedged
        // state — the server died mid-check, the status row is orphaned, the
        // task card is effectively frozen. Revert to whatever the previous
        // status was (fall back to deleting the row if we can't infer).
        self.reconcile_task_statuses().await;

        // Third pass: clear `agents.branch` rows whose branch no longer
        // exists in the project's git. Out-of-band `git branch -D` leaves
        // the row pointing at a ghost; the merge banner keeps offering an
        // action that can't succeed.
        self.reconcile_orphaned_branches().await;
    }

    /// Revert `task_status` rows stuck in `checking` when their check-agent
    /// is no longer alive. Called by [`cleanup_and_reattach`] on boot.
    ///
    /// When the most recent check-agent for a stuck task already wrote a
    /// verdict to `agent_output` (server died after the verdict reached the
    /// stream but before `start_check_agent`'s post-exit thread persisted
    /// it), the verdict is recovered and applied: `completed` → completed,
    /// `failed` → failed, anything else (`in_progress`, `pending`, …) →
    /// pending. When no verdict was ever produced the row is dropped, which
    /// reverts the task to its prior status (or implicit `pending` when no
    /// row remains).
    async fn reconcile_task_statuses(&self) {
        let stuck: Vec<(String, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = match db
                .prepare("SELECT plan_name, task_number FROM task_status WHERE status = 'checking'")
            {
                Ok(s) => s,
                Err(_) => return,
            };
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect())
                .unwrap_or_default()
        };

        for (plan_name, task_number) in stuck {
            // Is there any still-running check agent for this task?
            let live: Option<String> = {
                let db = self.db.lock().unwrap();
                db.query_row(
                    "SELECT id FROM agents \
                     WHERE plan_name = ?1 AND task_id = ?2 \
                           AND status IN ('running', 'starting') \
                     LIMIT 1",
                    rusqlite::params![plan_name, task_number],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            };
            if live.is_some() {
                continue; // agent still alive — leave the status alone
            }

            // Try to recover a verdict from the most recent check-agent's
            // output. Both the agent-id lookup and the verdict scan run
            // under a single lock acquisition — purely synchronous SQLite
            // work, no awaits between them.
            let recovered: Option<(String, String)> = {
                let db = self.db.lock().unwrap();
                let agent_id: Option<String> = db
                    .query_row(
                        "SELECT id FROM agents \
                         WHERE plan_name = ?1 AND task_id = ?2 AND mode = 'stream-json' \
                         ORDER BY started_at DESC LIMIT 1",
                        rusqlite::params![plan_name, task_number],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                agent_id.and_then(|id| check_agent::extract_check_agent_verdict(&db, &id))
            };

            match recovered {
                Some((verdict_status, verdict_reason)) => {
                    let mapped = match verdict_status.as_str() {
                        "completed" => "completed",
                        "failed" => "failed",
                        _ => "pending",
                    };
                    {
                        let db = self.db.lock().unwrap();
                        db.execute(
                            "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                             VALUES (?1, ?2, ?3, 'manual', datetime('now'))
                             ON CONFLICT(plan_name, task_number)
                             DO UPDATE SET status = excluded.status,
                                           source = 'manual',
                                           updated_at = datetime('now')",
                            rusqlite::params![plan_name, task_number, mapped],
                        )
                        .ok();
                    }
                    broadcast_event(
                        &self.broadcast_tx,
                        "task_status_changed",
                        serde_json::json!({
                            "plan_name": plan_name,
                            "task_number": task_number,
                            "status": mapped,
                            "reason": format!(
                                "boot_sweep: recovered verdict ({verdict_status}): {verdict_reason}"
                            ),
                        }),
                    );
                    println!(
                        "[Branchwork] Recovered verdict for stuck 'checking' on \
                         {plan_name}/{task_number}: verdict={verdict_status} → status={mapped}"
                    );
                }
                None => {
                    {
                        let db = self.db.lock().unwrap();
                        db.execute(
                            "DELETE FROM task_status \
                             WHERE plan_name = ?1 AND task_number = ?2 AND status = 'checking'",
                            rusqlite::params![plan_name, task_number],
                        )
                        .ok();
                    }
                    broadcast_event(
                        &self.broadcast_tx,
                        "task_status_changed",
                        serde_json::json!({
                            "plan_name": plan_name,
                            "task_number": task_number,
                            "status": serde_json::Value::Null,
                            "reason": "boot_sweep: checking status reverted, no verdict recoverable",
                        }),
                    );
                    println!(
                        "[Branchwork] Reverted stuck 'checking' on {plan_name}/{task_number} — \
                         no verdict recoverable"
                    );
                }
            }
        }
    }

    /// Clear `agents.branch` for rows whose branch no longer resolves in
    /// the project's git. Called by [`cleanup_and_reattach`] on boot, so
    /// the merge banner stops offering an action that can't succeed when
    /// a user manually `git branch -D`'d the task branch out of band.
    ///
    /// Three filters keep the sweep safe:
    /// * `mode != 'remote'` — SaaS agents live on a runner's filesystem;
    ///   a local `git show-ref` would always fail and incorrectly clear a
    ///   branch that's perfectly valid runner-side. Boot-sweep visibility
    ///   into runner branches is out of scope here (the merge dispatch
    ///   already round-trips `ListBranches` at click time).
    /// * `status NOT IN ('running','starting')` — defensive guard for
    ///   non-boot callers. Pass 1 of `cleanup_and_reattach` has already
    ///   moved every running/starting row to a terminal status by the
    ///   time this runs, but the guard makes the function safe to call
    ///   from outside boot too. Mirrors the live-agent guard
    ///   `purge_stale_branches` got in Task 1.2.
    /// * `cwd` must resolve to a directory locally — if the user moved
    ///   or renamed the project we can't tell whether the branch was
    ///   deleted; leave the row alone so reattaching the project to the
    ///   right path restores the merge banner.
    async fn reconcile_orphaned_branches(&self) {
        let rows: Vec<(String, String, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = match db.prepare(
                "SELECT id, branch, cwd FROM agents \
                 WHERE branch IS NOT NULL AND cwd IS NOT NULL AND cwd != '' \
                   AND mode != 'remote' \
                   AND status NOT IN ('running', 'starting')",
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
        };

        let mut cleared = 0u32;
        for (agent_id, branch, cwd) in rows {
            if !std::path::Path::new(&cwd).is_dir() {
                continue;
            }
            let exists = std::process::Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .current_dir(&cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if exists {
                continue;
            }
            {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET branch = NULL WHERE id = ?1",
                    rusqlite::params![agent_id],
                )
                .ok();
            }
            cleared += 1;
            broadcast_event(
                &self.broadcast_tx,
                "agent_branch_cleared",
                serde_json::json!({
                    "agent_id": agent_id,
                    "branch": branch,
                    "reason": "boot_sweep: branch not present in project git",
                }),
            );
            println!(
                "[Branchwork] Cleared orphaned branch {branch} on agent {} — \
                 not in project git",
                &agent_id[..8.min(agent_id.len())]
            );
        }

        // Record sweep telemetry so /api/health/state can report
        // "time since last orphan sweep" + "branches cleared in last sweep".
        // Written unconditionally (cleared=0 is the steady-state, and the
        // health endpoint needs the timestamp to know the sweep ran at all).
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "INSERT INTO settings (key, value) VALUES ('last_orphan_sweep_at', datetime('now')) \
                 ON CONFLICT(key) DO UPDATE SET value = datetime('now')",
                rusqlite::params![],
            )
            .ok();
            db.execute(
                "INSERT INTO settings (key, value) VALUES ('last_orphan_sweep_cleared', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![cleared.to_string()],
            )
            .ok();
        }
    }

    /// Mark an agent `failed` + stop_reason='supervisor_unreachable' when the
    /// heartbeat loop in [`pty_agent::spawn_heartbeat_task`] sees three
    /// consecutive Pings go unanswered. Evicts the live registry entry so
    /// the UI can't pretend the agent is still there, updates the DB, and
    /// broadcasts a stop event so task cards unlock in real-time.
    pub async fn mark_supervisor_unreachable(&self, agent_id: &str) {
        // Drop the live session. Any still-open reader/writer on the same
        // ManagedAgent will wind down on their own once they hit the closed
        // channel or connection.
        self.agents.lock().await.remove(agent_id);
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', \
                     stop_reason = 'supervisor_unreachable', \
                     finished_at = datetime('now') \
                 WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": agent_id,
                "status": "failed",
                "stop_reason": "supervisor_unreachable",
                "reason": "supervisor did not respond to 3 consecutive pings (~45s)",
            }),
        );
        println!(
            "[Branchwork] Agent {} marked unreachable — heartbeat timeout",
            &agent_id[..8.min(agent_id.len())]
        );
    }

    /// Mark an agent `killed` + stop_reason='supervisor_died' and broadcast
    /// the change. Used by cleanup_and_reattach (PID dead branch) and the
    /// 30 s supervisor-liveness poller in `main` for the named-wart case
    /// where a session daemon was killed out of band (`kill -9`, OOM
    /// killer) while the row was running. The classification is more
    /// specific than `orphaned`: the row's history isn't lost, the
    /// supervisor specifically went away.
    pub(crate) fn mark_supervisor_died(&self, agent_id: &str, detail: &str) {
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'killed', \
                     stop_reason = 'supervisor_died', \
                     finished_at = datetime('now') \
                 WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": agent_id,
                "status": "killed",
                "stop_reason": "supervisor_died",
                "reason": detail,
            }),
        );
        println!(
            "[Branchwork] Agent {} marked killed/supervisor_died — {detail}",
            &agent_id[..8.min(agent_id.len())]
        );
    }

    /// Mark an agent `failed` + stop_reason='orphaned' and broadcast the
    /// change so any connected browser unlocks the task card without
    /// waiting for a manual refresh. Used by cleanup_and_reattach for
    /// pre-history orphan classes (no socket recorded, stream-json IO
    /// handles lost). PID-dead rows go through
    /// [`Self::mark_supervisor_died`] instead.
    fn mark_orphaned(&self, agent_id: &str, detail: &str) {
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', \
                     stop_reason = 'orphaned', \
                     finished_at = datetime('now') \
                 WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": agent_id,
                "status": "failed",
                "stop_reason": "orphaned",
                "reason": detail,
            }),
        );
        println!(
            "[Branchwork] Agent {} marked orphaned — {detail}",
            &agent_id[..8.min(agent_id.len())]
        );
    }

    /// Ask the agent's CLI to exit cleanly by sending its driver-specific
    /// exit sequence (e.g. `/exit\r` for Claude Code) through the PTY.
    /// Returns true if the sequence was sent; false if the agent is not
    /// live in-process or the driver doesn't support clean exit.
    pub async fn graceful_exit(&self, agent_id: &str) -> bool {
        // Look up the driver for this agent so we know what to send.
        let driver_name: Option<String> = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT driver FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        let driver_name = driver_name.unwrap_or_else(|| driver::DEFAULT_DRIVER.to_string());
        let exit_bytes: Vec<u8> = match self.drivers.get(&driver_name) {
            Some(d) => match d.graceful_exit_sequence() {
                Some(b) => b.to_vec(),
                None => return false,
            },
            None => return false,
        };

        let agents = self.agents.lock().await;
        if let Some(agent) = agents.get(agent_id) {
            let _ = agent.command_tx.send(SessionMessage::Input(exit_bytes));
            return true;
        }
        drop(agents);

        // T5.17: SaaS fallback. Pre-T5.17 this branch returned `false`
        // and the caller (Finish button via `api/agents.rs::finish_agent`,
        // auto-mode via `auto_mode.rs::run_idle_finish_loop`) gave up.
        // For SaaS agents the in-process registry never has them — they
        // live on the runner — so every Finish click and every auto-
        // mode tick was a silent no-op and the dashboard sat at "ready
        // to merge" while the daemon kept its claude alive forever.
        //
        // Mirror the `finish_agent` HTTP path's SaaS branch here: look
        // up the agent's mode + org_id, find an online runner for that
        // org, and ship the driver's exit bytes as
        // `WireMessage::AgentInput`. The runner's existing handler
        // forwards the bytes to the daemon's PTY; claude exits cleanly
        // on its own and `AgentStopped` flows back through the normal
        // channels.
        let Some(state) = self.app_state.get() else {
            return false;
        };
        let row: Option<(String, String)> = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT mode, org_id FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok()
        };
        let Some((mode, org_id)) = row else {
            return false;
        };
        if mode != "remote" {
            return false;
        }
        let runner_id: Option<String> = {
            let conn = self.db.lock().unwrap();
            conn.query_row(
                "SELECT id FROM runners WHERE org_id = ?1 \
                 ORDER BY (status = 'online') DESC, last_seen_at DESC LIMIT 1",
                rusqlite::params![org_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };
        let Some(runner_id) = runner_id else {
            return false;
        };

        use base64::Engine;
        let envelope = crate::saas::runner_protocol::Envelope::best_effort(
            "server".to_string(),
            crate::saas::runner_protocol::WireMessage::AgentInput {
                agent_id: agent_id.to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&exit_bytes),
            },
        );
        let Ok(json) = serde_json::to_string(&envelope) else {
            return false;
        };
        let runners = state.runners.lock().await;
        let Some(runner) = runners.get(&runner_id) else {
            return false;
        };
        runner.command_tx.send(json).is_ok()
    }

    pub async fn kill_agent(&self, agent_id: &str) -> bool {
        // Live agent: send Kill over the control channel. The supervisor
        // propagates the kill to the PTY's child, closes the socket, and
        // the reader task we spawned in start_pty_agent will update the
        // DB + broadcast `agent_stopped` when it sees EOF.
        let maybe_agent = {
            let mut agents = self.agents.lock().await;
            agents.remove(agent_id)
        };
        if let Some(agent) = maybe_agent {
            let _ = agent.command_tx.send(SessionMessage::Kill);
            // Ensure the DB is flipped promptly even if the reader task is
            // slow to observe EOF. Also clear the branch so a killed agent
            // doesn't keep advertising itself as mergeable in the UI.
            {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET status = 'killed', finished_at = datetime('now'), branch = NULL WHERE id = ? AND status IN ('running', 'starting')",
                    rusqlite::params![agent_id],
                )
                .ok();
            }
            broadcast_event(
                &self.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": agent_id, "status": "killed"}),
            );
            // Fall through: also try the PID-based fallback below in case
            // the daemon's socket is wedged.
            let pid = {
                let db = self.db.lock().unwrap();
                db.query_row(
                    "SELECT pid FROM agents WHERE id = ?",
                    rusqlite::params![agent_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
            };
            if let Some(p) = pid {
                process_terminate(p);
            }
            // Worktree-per-agent cleanup (Task 2.4): the agent was killed
            // mid-task, so its isolated worktree is junk — remove it (unless
            // it's held for the Phase 3 resolver). Best-effort + self-gating;
            // a no-op under BRANCHWORK_USE_WORKTREES=0 or for in-place / SaaS
            // rows. The reader task's later `on_agent_exit` sees the row is
            // already `killed`, so it won't try to remove it again.
            cleanup_worktree_for_terminated_agent(&self.db, agent_id);
            return true;
        }

        // Not in the in-process registry — still try to make sure the
        // daemon (if any) is dead, and mark the row killed.
        let (socket, pid) = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT supervisor_socket, pid FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
            )
            .unwrap_or((None, None))
        };
        if let Some(p) = pid {
            process_terminate(p);
        }
        if let Some(s) = socket.as_deref() {
            // Clean up stale socket / pidfile / log siblings so a future
            // restart isn't confused by them. Best-effort only.
            let socket = Path::new(s);
            let _ = std::fs::remove_file(socket);
            let _ = std::fs::remove_file(supervisor::pidfile_path(socket));
        }
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'killed', finished_at = datetime('now'), branch = NULL WHERE id = ?",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({"id": agent_id, "status": "killed"}),
        );
        // Same worktree cleanup as the live-agent branch above. Scoped the DB
        // write so the lock is released before this re-locks internally.
        cleanup_worktree_for_terminated_agent(&self.db, agent_id);
        true
    }

    /// Cancel every in-flight at-merge conflict resolver agent for
    /// `plan_name` — the hook the `PUT /api/plans/:name/config` handler runs
    /// when the operator toggles `auto_mode` **off** (Task 3.4).
    ///
    /// For each running/starting row badged `mode = 'resolver'`:
    /// - kills it via [`Self::kill_agent`]. The resolver row carries
    ///   `merge_status = 'held_for_resolver'`, so the kill's worktree-cleanup
    ///   ([`cleanup_worktree_for_terminated_agent`]) **skips removal** — the
    ///   conflicted tree is left on disk for the human to inspect, exactly as
    ///   the brief requires.
    /// - reads the still-unmerged paths out of the preserved worktree, and
    /// - broadcasts a `resolver_cancelled` event carrying the **original**
    ///   task id (so the dashboard can surface the conflict under the original
    ///   task card, not the throwaway `-resolve-N` row) and those paths.
    ///
    /// Re-enabling auto-mode does **not** auto-resume the resolver: the killed
    /// row stays `killed`, nothing watches it, and the conflict only re-enters
    /// the resolver path if a merge is re-attempted. The human must intervene.
    ///
    /// Standalone-only in practice — resolvers are never spawned in SaaS mode
    /// ([`crate::api::agents::merge_agent_branch_inner`] gates the spawn on
    /// `!org_has_runner`), so the snapshot query simply returns empty there.
    pub async fn cancel_in_flight_resolvers(&self, plan_name: &str) {
        // 1. Snapshot the resolvers BEFORE killing. Collect everything we
        //    need (cwd to read conflicts, the parent for the original task id)
        //    inside the lock, then drop it: `kill_agent` re-locks `db`
        //    internally, so holding the guard across the await would deadlock.
        struct ResolverSnapshot {
            id: String,
            cwd: Option<String>,
            resolver_task_id: String,
            parent_agent_id: Option<String>,
            parent_task_id: Option<String>,
        }
        let resolvers: Vec<ResolverSnapshot> = {
            let conn = self.db.lock().unwrap();
            let Ok(mut stmt) = conn.prepare(
                "SELECT r.id, r.cwd, r.task_id, r.parent_agent_id, p.task_id \
                 FROM agents r \
                 LEFT JOIN agents p ON p.id = r.parent_agent_id \
                 WHERE r.plan_name = ?1 \
                   AND r.mode = 'resolver' \
                   AND r.status IN ('running', 'starting')",
            ) else {
                return;
            };
            let Ok(rows) = stmt.query_map(rusqlite::params![plan_name], |row| {
                Ok(ResolverSnapshot {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    resolver_task_id: row.get(2)?,
                    parent_agent_id: row.get(3)?,
                    parent_task_id: row.get(4)?,
                })
            }) else {
                return;
            };
            rows.flatten().collect()
        };

        // 2. Kill each resolver (worktree preserved), then surface the
        //    leftover conflict so the human can pick it up.
        for r in resolvers {
            self.kill_agent(&r.id).await;

            // The conflicts a human will find in the preserved worktree. Empty
            // when the tree is gone or the resolver had already resolved it.
            let conflicted_paths = r
                .cwd
                .as_deref()
                .map(|c| worktree_unmerged_paths(Path::new(c)))
                .unwrap_or_default();

            // Surface under the ORIGINAL task, not the `-resolve-N` row:
            // prefer the parent agent's task id, falling back to stripping the
            // `-resolve-<n>` suffix off the resolver's own task id.
            let original_task = r.parent_task_id.clone().unwrap_or_else(|| {
                r.resolver_task_id
                    .rsplit_once("-resolve-")
                    .map(|(orig, _)| orig.to_string())
                    .unwrap_or_else(|| r.resolver_task_id.clone())
            });

            broadcast_event(
                &self.broadcast_tx,
                "resolver_cancelled",
                serde_json::json!({
                    "plan": plan_name,
                    "task": original_task,
                    "agent_id": r.id,
                    "parent_agent_id": r.parent_agent_id,
                    "conflicted_paths": conflicted_paths,
                }),
            );
        }
    }
}

// ── Task prompt + auto-advance ───────────────────────────────────────────────

/// Resolve the project name for a plan, preferring the DB override row (set via
/// the project endpoint) and falling back to the value parsed from the plan.
fn plan_project(db: &Db, plan_name: &str, parsed_project: Option<&str>) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT project FROM plan_project WHERE plan_name = ?1",
        rusqlite::params![plan_name],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .or_else(|| parsed_project.map(|s| s.to_string()))
}

/// Outcome of [`check_tree_clean_for_completion`]: the working tree is
/// either clean (good — we can mark `completed`), or dirty (reject the
/// status change with a helpful error), or we couldn't tell (missing
/// project dir, no git, git errored — treat as clean; the merge-time
/// empty-branch guard is the backstop).
#[derive(Debug)]
pub enum TreeState {
    Clean,
    Dirty { files: Vec<String> },
    Unknown,
}

/// Refuse to mark a task `completed` if the project's working tree has
/// uncommitted changes. Closes the hole where an agent modifies files,
/// calls `update_task_status(completed)`, and exits — leaving the changes
/// to be silently stepped on by the next agent.
///
/// The project's `branchwork.toml` may opt specific paths out of the
/// pause via `[auto_mode.dirty_tree] ignore = [...]` (gitignore-style
/// globs). Those paths still show up in `git status` but the check
/// pretends they're clean — intended for known-operational files
/// (build logs, scratch notes, generated config) so noise doesn't
/// pause plans. Agent code paths must NOT be ignored: the merge-time
/// empty-branch guard is the backstop but the dirty-tree check is the
/// primary signal for "agent left work uncommitted".
///
/// Returns `Dirty` with the first few changed paths for a useful error
/// message, `Clean` when it's safe to proceed (either nothing dirty
/// or everything dirty matched the ignore list), and `Unknown` when
/// we can't introspect (no project dir, not a git repo, etc) — the
/// caller should treat that as permissive because the merge-time
/// empty-branch guard still catches the case where nothing was ever
/// committed.
pub fn check_tree_clean_for_completion(
    db: &Db,
    plans_dir: &std::path::Path,
    plan_name: &str,
) -> TreeState {
    let Some(cwd) = crate::ci::project_dir_for(plans_dir, db, plan_name) else {
        return TreeState::Unknown;
    };
    // `git status --porcelain` is the canonical "is this tree clean?"
    // probe. `--untracked-files=no` is the key bit: the failure mode we're
    // catching is "agent modified tracked files but didn't commit", not
    // "editor left a swap file lying around". Untracked files are noise
    // agents can't reason about — gitignore or stash is the user's call,
    // not something to block task completion on.
    let out = match std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&cwd)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return TreeState::Unknown,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Porcelain lines are "XY path" with a 3-char prefix; slice past it.
    // Empty lines (trailing newline etc) are dropped.
    let all_paths: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.get(3..).unwrap_or(l))
        .collect();
    if all_paths.is_empty() {
        return TreeState::Clean;
    }
    // Per-project allowlist: known-operational files (e.g. `*.log`,
    // `.bob/**`) shouldn't trip the pause. The list lives in
    // `<project>/branchwork.toml`. Falls back to "no filter" when the
    // file is absent or malformed — the warn-and-skip path inside
    // `repo_config` keeps us from making noisy plans noisier.
    let repo_cfg = crate::repo_config::load_for_project_dir(&cwd);
    let dirty_tree_cfg = repo_cfg
        .as_ref()
        .map(|c| &c.auto_mode.dirty_tree)
        .cloned()
        .unwrap_or_default();
    let dirty_paths: Vec<&str> = all_paths
        .into_iter()
        .filter(|path| !dirty_tree_cfg.path_matches_ignore(path))
        .collect();
    if dirty_paths.is_empty() {
        return TreeState::Clean;
    }
    let files: Vec<String> = dirty_paths
        .iter()
        .take(10)
        .map(|p| (*p).to_string())
        .collect();
    TreeState::Dirty { files }
}

/// Build a prompt fragment listing completed tasks from sibling plans (and the
/// current plan's earlier tasks) in the same project, so an agent picking up a
/// task inherits what its predecessors established.
///
/// `current_task` is excluded from the listing (it's the task being worked on).
/// Returns `None` when there's nothing useful to share.
pub fn build_cross_plan_context(
    db: &Db,
    plans_dir: &std::path::Path,
    current_plan: &ParsedPlan,
    current_task_number: &str,
) -> Option<String> {
    let project = plan_project(db, &current_plan.name, current_plan.project.as_deref())?;
    let home = dirs::home_dir().unwrap_or_default();

    // Every plan mapped to this project (DB override + parsed value).
    let summaries = plan_parser::list_plans(plans_dir);
    let project_overrides: HashMap<String, String> = {
        let conn = db.lock().unwrap();
        conn.prepare("SELECT plan_name, project FROM plan_project")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()
            })
            .unwrap_or_default()
    };

    let mut entries: Vec<String> = Vec::new();

    for summary in summaries {
        let plan_project_name = project_overrides
            .get(&summary.name)
            .cloned()
            .or(summary.project.clone());
        if plan_project_name.as_deref() != Some(project.as_str()) {
            continue;
        }

        let plan = if summary.name == current_plan.name {
            current_plan.clone()
        } else {
            match plan_parser::find_plan_file(plans_dir, &summary.name)
                .and_then(|p| plan_parser::parse_plan_file(&p).ok())
            {
                Some(p) => p,
                None => continue,
            }
        };

        // Completed/skipped tasks for this plan.
        let status_map: HashMap<String, String> = {
            let conn = db.lock().unwrap();
            conn.prepare(
                "SELECT task_number, status FROM task_status \
                 WHERE plan_name = ?1 AND status IN ('completed', 'skipped')",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![summary.name], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()
            })
            .unwrap_or_default()
        };

        for phase in &plan.phases {
            for task in &phase.tasks {
                // Never include the task the agent is about to work on.
                if plan.name == current_plan.name && task.number == current_task_number {
                    continue;
                }
                if !status_map.contains_key(&task.number) {
                    continue;
                }

                let learnings = {
                    let conn = db.lock().unwrap();
                    crate::db::task_learnings(&conn, &plan.name, &task.number)
                };

                let source = if plan.name == current_plan.name {
                    format!("Task {}", task.number)
                } else {
                    format!("Task {} ({})", task.number, plan.title)
                };

                let mut line = format!("- {source}: {}", task.title);
                if !task.file_paths.is_empty() {
                    line.push_str(" — files: ");
                    line.push_str(&task.file_paths.join(", "));
                }
                entries.push(line);

                for l in learnings {
                    entries.push(format!("    • {l}"));
                }
            }
        }
    }

    // Reference home to silence the unused warning in case we later expand this
    // with project-directory–relative grounding; keep the import useful.
    let _ = home;

    if entries.is_empty() {
        None
    } else {
        Some(format!(
            "Related work already completed in this project:\n{}",
            entries.join("\n")
        ))
    }
}

pub fn build_task_prompt(
    plan: &ParsedPlan,
    phase: &PlanPhase,
    task: &PlanTask,
    is_continue: bool,
    port: u16,
    cross_plan_context: Option<&str>,
    mcp_available: bool,
) -> String {
    let files_section = if !task.file_paths.is_empty() {
        format!(
            "\nFiles involved:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        String::new()
    };

    let acceptance_section = if !task.acceptance.is_empty() {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    } else {
        String::new()
    };

    let context_section = cross_plan_context
        .map(|c| format!("\n{c}\n"))
        .unwrap_or_default();

    // Drivers that auto-register the Branchwork MCP server (currently just
    // Claude) get a terser "call the MCP tool" instruction; others fall back
    // to a curl against the HTTP API, which is always live as a backstop.
    let status_step = if mcp_available {
        format!(
            "4. Mark the task status by calling the `update_task_status` MCP tool \
             (from the `branchwork` server) with {{\"plan\": \"{plan_name}\", \
             \"task\": \"{task_num}\", \"status\": \"completed\"}}",
            plan_name = plan.name,
            task_num = task.number,
        )
    } else {
        format!(
            "4. Mark the task status by running: curl -s -X PUT \
             http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/status \
             -H \"Content-Type: application/json\" -d '{{\"status\":\"completed\"}}'",
            port = port,
            plan_name = plan.name,
            task_num = task.number,
        )
    };

    let task_branch = format!("branchwork/{}/{}", plan.name, task.number);
    let contract = prompt::unattended_contract_block(&task_branch);

    format!(
        "{intro}\n\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Description:\n{description}\n\
         {files}{acceptance}\n\
         {context}\n\
         {instruction}\n\n\
         {contract}\n\n\
         IMPORTANT: When you think you are done, do NOT stop. Instead:\n\
         1. Summarize what you did\n\
         2. Record one short learning other tasks in this project should know (file paths established, key decisions, gotchas) by running: curl -s -X POST http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/learnings -H \"Content-Type: application/json\" -d '{{\"learning\":\"...\"}}'\n\
         3. Commit your changes with `git add -A && git commit -m '<short summary>'`. You MUST do this before marking the task completed — merges only carry committed work, and uncommitted changes are silently dropped when the next agent checks out the task branch.\n\
         {status_step}\n\
         5. Ask the user if they need anything else\n\
         6. Only stop when the user explicitly says they are done",
        intro = if is_continue {
            "You are continuing work on a partially implemented task. Some parts may already exist — check the current state of the code before making changes."
        } else {
            "You are working on the following task from a plan."
        },
        plan_title = plan.title,
        phase_num = phase.number,
        phase_title = phase.title,
        task_num = task.number,
        task_title = task.title,
        description = task.description,
        files = files_section,
        acceptance = acceptance_section,
        context = context_section,
        instruction = if is_continue {
            "First, read the relevant files to understand what has already been done. Then complete the remaining work."
        } else {
            "Please implement this task. When done, summarize what you changed."
        },
        plan_name = plan.name,
        contract = contract,
    )
}

/// Build the prompt for a *plan-level* agent: the plan is loaded as
/// context, but no specific task is selected and no completion contract
/// applies. The agent boots, says hello, and waits for user instructions
/// instead of pushing toward a commit + status update like a task agent
/// would.
///
/// `mcp_available` mirrors `build_task_prompt`: drivers that auto-register
/// the Branchwork MCP server get told to use the MCP tools; everyone else
/// gets the HTTP-API fallback.
pub fn build_plan_session_prompt(plan: &ParsedPlan, port: u16, mcp_available: bool) -> String {
    let phase_lines = if plan.phases.is_empty() {
        "  (no phases)".to_string()
    } else {
        plan.phases
            .iter()
            .map(|p| {
                let total = p.tasks.len();
                let done = p
                    .tasks
                    .iter()
                    .filter(|t| t.status.as_deref() == Some("completed"))
                    .count();
                format!(
                    "  Phase {n}: {title} ({done}/{total} done)",
                    n = p.number,
                    title = p.title,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let context_section = if plan.context.trim().is_empty() {
        String::new()
    } else {
        format!("\nPlan context:\n{}\n", plan.context.trim())
    };

    let tools_section = if mcp_available {
        "You can read and edit this plan via the `branchwork` MCP server (already \
         registered). Useful tools include `list_tasks`, `get_task`, \
         `update_task_status`, and `record_task_learning`."
            .to_string()
    } else {
        format!(
            "You can read and edit this plan via the Branchwork HTTP API at \
             http://localhost:{port}/api/plans/{plan_name}/...",
            plan_name = plan.name,
        )
    };

    format!(
        "You are starting an interactive session for the plan below. There is no \
         specific task assigned and no completion contract — do NOT commit, do NOT \
         mark anything completed, and do NOT exit on your own. Wait for the user's \
         instructions before doing any work.\n\n\
         Plan: {plan_title} ({plan_name})\n\
         Phases:\n{phase_lines}\n\
         {context_section}\n\
         {tools_section}\n\n\
         Greet the user briefly, summarise the plan in one or two sentences so they \
         know you've loaded it, and then ask what they'd like to do.",
        plan_title = plan.title,
        plan_name = plan.name,
    )
}

/// Whether auto-advance is enabled for `plan_name` (opt-in, default off).
pub fn auto_advance_enabled(db: &Db, plan_name: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled FROM plan_auto_advance WHERE plan_name = ?1",
        rusqlite::params![plan_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

/// Compute the remaining budget for a plan, or `Err((spent, max))` if the
/// budget is already exhausted. Mirrors `plan_remaining_budget` in the API
/// module so we don't have to thread it through here.
fn remaining_budget(db: &Db, plan_name: &str) -> Result<Option<f64>, (f64, f64)> {
    let conn = db.lock().unwrap();
    let max: Option<f64> = conn
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?1",
            rusqlite::params![plan_name],
            |row| row.get::<_, f64>(0),
        )
        .ok();
    match max {
        Some(m) => {
            let spent: f64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
                     WHERE plan_name = ?1 AND cost_usd IS NOT NULL",
                    rusqlite::params![plan_name],
                    |row| row.get::<_, f64>(0),
                )
                .unwrap_or(0.0);
            if spent >= m {
                Err((spent, m))
            } else {
                Ok(Some((m - spent).max(0.0)))
            }
        }
        None => Ok(None),
    }
}

/// Atomically claim a task for spawning: flip its row to `in_progress` only if
/// it's currently `pending` or `failed`. Returns true if we won the claim.
/// Atomically claim a task slot for the given `(plan, task)` and gate the
/// claim on the plan being idle (no other task agent is `starting` /
/// `running`).
///
/// **Why the idle gate:** without it, two concurrent `try_auto_advance`
/// calls — one per CI-passed completion when several phase-N tasks
/// finish close together — each pick a *different* ready task in phase
/// N+1 and each win their own claim race, spawning two agents
/// simultaneously. The intra-call `break` in `spawn_ready_tasks` only
/// gates within one advance call; this `NOT EXISTS` predicate gates
/// across them.
///
/// Check agents (`mode = 'stream-json'`) are excluded — they're
/// read-only verification runs that don't touch the worktree, so they
/// shouldn't block auto-advance. Fix agents (`mode = 'pty'`,
/// `task_id` matches `<task>-fix-<n>`) **are** counted, which gives us
/// the desired "no new task spawns while a fix is in flight" behaviour
/// for free — they appear in `agents` with the same `plan_name` and a
/// `running`-class status.
///
/// The check and the claim live in one SQL statement so SQLite's
/// per-write serialisation gives us race-freedom without a tokio mutex.
fn claim_task(db: &Db, plan_name: &str, task_number: &str) -> bool {
    let conn = db.lock().unwrap();
    let updated = conn
        .execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at)
             SELECT ?1, ?2, 'in_progress', datetime('now')
             WHERE NOT EXISTS (
                 SELECT 1 FROM agents
                 WHERE plan_name = ?1
                   AND status IN ('starting', 'running')
                   AND mode != 'stream-json'
             )
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at
             WHERE task_status.status IN ('pending', 'failed')
               AND NOT EXISTS (
                   SELECT 1 FROM agents
                   WHERE plan_name = ?1
                     AND status IN ('starting', 'running')
                     AND mode != 'stream-json'
               )",
            rusqlite::params![plan_name, task_number],
        )
        .unwrap_or(0);
    updated > 0
}

/// If the just-completed task finished the last open task in its phase, and
/// auto-advance is enabled for the plan, spawn agents for ready tasks of the
/// next phase that has any. No-op otherwise.
///
/// `merged_sha` is the merge commit produced by `merge_agent_branch_dispatch`
/// when this completion came through the auto-mode merge pipeline. It's
/// threaded through so the `phase_completed` broadcast (Task 2.1) can carry
/// the phase-end SHA the Check agent (Task 2.2) verifies against. Manual
/// completion paths (HTTP `PUT /tasks/:id/status`, MCP `update_task_status`,
/// auto-mode resume) pass `None` — the event still fires so the UI can
/// reflect phase progress, but `phase_sha` is `null`.
///
/// Runs in the background (caller `tokio::spawn`s it) so it can't block the
/// HTTP response that triggered it.
pub async fn try_auto_advance(
    registry: AgentRegistry,
    plans_dir: PathBuf,
    plan_name: String,
    completed_task_number: String,
    effort: Effort,
    port: u16,
    merged_sha: Option<String>,
) {
    // Either auto-advance (phase-level opt-in) or auto-mode (the
    // merge-CI-advance loop's master gate, which already excludes paused
    // plans via paused_reason IS NULL) is enough to advance. The
    // auto-mode loop relies on this branch to spawn the next phase's
    // tasks after a green CI without requiring a separate auto_advance
    // toggle.
    if !auto_advance_enabled(&registry.db, &plan_name)
        && !crate::db::auto_mode_enabled(&registry.db, &plan_name)
    {
        return;
    }

    let plan_path = match plan_parser::find_plan_file(&plans_dir, &plan_name) {
        Some(p) => p,
        None => return,
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Find the phase that owns the just-completed task, and its index in
    // `plan.phases` (T5.19 uses the index to limit the intra-phase scan
    // to phases AT-OR-BEFORE the current one — see the scan loop below).
    let current_phase_idx = match plan
        .phases
        .iter()
        .position(|p| p.tasks.iter().any(|t| t.number == completed_task_number))
    {
        Some(i) => i,
        None => return,
    };
    let current_phase = &plan.phases[current_phase_idx];

    // Snapshot all task statuses for this plan so we can decide locally.
    let status_map: HashMap<String, String> = {
        let conn = registry.db.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?1")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map(rusqlite::params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    };

    // Done-set, work_dir, and budget are needed for both the intra-phase
    // and next-phase paths, so compute them up front. Budget check returns
    // early if exhausted — same as pre-3.1 behaviour, just hoisted.
    let done_set = {
        let conn = registry.db.lock().unwrap();
        completed_task_numbers(&conn, &plan_name)
    };

    let max_budget_usd = match remaining_budget(&registry.db, &plan_name) {
        Ok(b) => b,
        Err(_) => return,
    };

    let work_dir: PathBuf = {
        let home = dirs::home_dir().unwrap();
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    };

    // Intra-phase scan: are any tasks in the current phase — OR any
    // earlier phase — newly ready (pending/failed with all deps
    // satisfied)? If so, spawn them and broadcast `task_advanced`.
    //
    // T5.19: pre-T5.19 the scan was limited to `current_phase.tasks`,
    // so tasks the user added to an EARLIER phase after auto-mode had
    // already advanced past it were never picked up. User saw it
    // directly: plan in phase 3 → user adds a task to phase 2 →
    // ignored. Walking phases `[0..=current_phase_idx]` catches the
    // newly-pending earlier-phase tasks without spawning anything
    // ahead of the current phase boundary (future phases are still
    // gated by `phase_done` + `spawn_next_phase_tasks` below).
    let mut total_spawned: Vec<String> = Vec::new();
    for phase in &plan.phases[..=current_phase_idx] {
        let ready: Vec<&PlanTask> = phase
            .tasks
            .iter()
            .filter(|t| {
                let status = status_map
                    .get(&t.number)
                    .map(String::as_str)
                    .unwrap_or("pending");
                (status == "pending" || status == "failed")
                    && t.dependencies.iter().all(|d| done_set.contains(d))
            })
            .collect();

        if ready.is_empty() {
            continue;
        }

        let spawned = spawn_ready_tasks(
            &registry,
            &plans_dir,
            &plan,
            phase,
            &ready,
            effort,
            port,
            max_budget_usd,
            &work_dir,
        )
        .await;
        total_spawned.extend(spawned);
    }

    if !total_spawned.is_empty() {
        // `task_advanced` only fires if we actually spawned at least one
        // task. If every candidate lost the `claim_task` race (concurrent
        // auto-advance trigger), stay quiet — the other trigger already
        // broadcast.
        broadcast_event(
            &registry.broadcast_tx,
            "task_advanced",
            serde_json::json!({
                "plan": plan_name,
                "from_task": completed_task_number,
                "to_tasks": total_spawned,
            }),
        );
        return;
    }

    // No ready tasks in the current phase. Either the phase is fully done
    // (every task completed/skipped) — fall through to the next-phase
    // scan — or some tasks are still in_progress (stuck dep chain),
    // in which case we wait for them to finish.
    let phase_done = current_phase.tasks.iter().all(|t| {
        matches!(
            status_map.get(&t.number).map(String::as_str),
            Some("completed") | Some("skipped")
        )
    });
    if !phase_done {
        return;
    }

    // Phase boundary just crossed: every task in `current_phase` is
    // completed/skipped. Broadcast `phase_completed` once, BEFORE the
    // next-phase scan, so it fires regardless of whether a next phase
    // exists or has ready tasks (last phase of the plan still emits).
    // `phase_sha` carries the merge commit when the trigger came from
    // the auto-mode merge pipeline (`on_ci_passed` /
    // `on_fix_ci_passed`); for manual completion paths it's `null`.
    // The event powers Task 2.2's phase-end Check agent — the SHA is
    // what the verify suite runs against.
    broadcast_event(
        &registry.broadcast_tx,
        "phase_completed",
        serde_json::json!({
            "plan_name": plan_name,
            "phase_number": current_phase.number,
            "phase_sha": merged_sha,
        }),
    );

    // Phase 2.2 gate: when a `phase_verification` command is configured at
    // the phase / plan / repo `branchwork.toml` layer AND the trigger came
    // from the auto-mode merge pipeline (i.e. `merged_sha.is_some()` — a
    // real commit exists to verify against), defer the next-phase advance
    // to the `phase_check` listener that subscribed to `phase_completed`
    // above. The Check agent runs the verification command on a fresh
    // worktree at `merged_sha`; on success it calls
    // [`advance_after_phase_verify`] (the same scan-and-spawn block we
    // would run here); on failure it pauses auto-mode for the plan.
    //
    // Manual completion paths (PUT /tasks/:id/status, MCP, resume) pass
    // `merged_sha=None` — there's no commit to verify against, so we
    // bypass the gate and fall through to the immediate next-phase
    // advance, preserving pre-2.2 behaviour for manual flows.
    if merged_sha.is_some() && phase_verification_configured(&plan, current_phase) {
        return;
    }

    spawn_next_phase_tasks(
        &registry,
        &plans_dir,
        &plan,
        current_phase,
        &status_map,
        &done_set,
        max_budget_usd,
        &work_dir,
        effort,
        port,
    )
    .await;
}

/// Resolve `phase_verification` for the just-completed phase across plan,
/// per-phase, and repo `branchwork.toml` layers. Used by both
/// [`try_auto_advance`] (to gate the next-phase advance) and
/// [`crate::agents::phase_check`] (to decide whether to spawn a Check
/// agent at all).
fn phase_verification_configured(plan: &ParsedPlan, phase: &PlanPhase) -> bool {
    let project_dir = plan
        .project
        .as_ref()
        .and_then(|p| dirs::home_dir().map(|h| h.join(p)));
    let repo_config = project_dir
        .as_deref()
        .and_then(crate::repo_config::load_for_project_dir);
    crate::ci::resolution::resolve_phase_verification(plan, phase, repo_config.as_ref()).is_some()
}

/// Re-entry point for the phase_check listener after the verify Check
/// agent reports a passing verdict. Re-derives the same plan / status /
/// budget / work_dir state that [`try_auto_advance`] computed, then runs
/// the identical next-phase scan-and-spawn block. Splitting this out
/// (rather than re-calling `try_auto_advance` with a "skip-verify" flag)
/// keeps `phase_completed` from being re-broadcast on the verify pass.
pub async fn advance_after_phase_verify(
    registry: AgentRegistry,
    plans_dir: PathBuf,
    plan_name: String,
    current_phase_number: u32,
    effort: Effort,
    port: u16,
) {
    let plan_path = match plan_parser::find_plan_file(&plans_dir, &plan_name) {
        Some(p) => p,
        None => return,
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => return,
    };
    let current_phase = match plan
        .phases
        .iter()
        .find(|p| p.number == current_phase_number)
    {
        Some(p) => p,
        None => return,
    };

    let status_map: HashMap<String, String> = {
        let conn = registry.db.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?1")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map(rusqlite::params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    };
    let done_set = {
        let conn = registry.db.lock().unwrap();
        completed_task_numbers(&conn, &plan_name)
    };
    let max_budget_usd = match remaining_budget(&registry.db, &plan_name) {
        Ok(b) => b,
        Err(_) => return,
    };
    let work_dir: PathBuf = {
        let home = dirs::home_dir().unwrap();
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    };

    spawn_next_phase_tasks(
        &registry,
        &plans_dir,
        &plan,
        current_phase,
        &status_map,
        &done_set,
        max_budget_usd,
        &work_dir,
        effort,
        port,
    )
    .await;
}

/// Inner helper: scan for the next phase with ready tasks, spawn them,
/// broadcast `phase_advanced`, optionally fan out the webhook. Extracted
/// from [`try_auto_advance`] so [`advance_after_phase_verify`] can call
/// the same block without re-broadcasting `phase_completed`.
#[allow(clippy::too_many_arguments)]
async fn spawn_next_phase_tasks(
    registry: &AgentRegistry,
    plans_dir: &Path,
    plan: &ParsedPlan,
    current_phase: &PlanPhase,
    status_map: &HashMap<String, String>,
    done_set: &std::collections::HashSet<String>,
    max_budget_usd: Option<f64>,
    work_dir: &Path,
    effort: Effort,
    port: u16,
) {
    let plan_name = &plan.name;
    let next_phase = plan
        .phases
        .iter()
        .filter(|p| p.number > current_phase.number)
        .find(|p| {
            p.tasks.iter().any(|t| {
                let status = status_map
                    .get(&t.number)
                    .map(String::as_str)
                    .unwrap_or("pending");
                (status == "pending" || status == "failed")
                    && t.dependencies.iter().all(|d| done_set.contains(d))
            })
        });
    let next_phase = match next_phase {
        Some(p) => p,
        None => return,
    };

    let ready_tasks: Vec<&PlanTask> = next_phase
        .tasks
        .iter()
        .filter(|t| {
            let status = status_map
                .get(&t.number)
                .map(String::as_str)
                .unwrap_or("pending");
            (status == "pending" || status == "failed")
                && t.dependencies.iter().all(|d| done_set.contains(d))
        })
        .collect();

    println!(
        "[auto-advance] {plan_name}: phase {} done -> spawning {} task(s) in phase {}",
        current_phase.number,
        ready_tasks.len(),
        next_phase.number
    );

    spawn_ready_tasks(
        registry,
        plans_dir,
        plan,
        next_phase,
        &ready_tasks,
        effort,
        port,
        max_budget_usd,
        work_dir,
    )
    .await;

    broadcast_event(
        &registry.broadcast_tx,
        "phase_advanced",
        serde_json::json!({
            "plan_name": plan_name,
            "from_phase": current_phase.number,
            "to_phase": next_phase.number,
        }),
    );

    let webhook_snapshot = registry.webhook_url.read().unwrap().clone();
    if webhook_snapshot.is_some() {
        let msg = crate::notifications::phase_advance_message(
            plan_name,
            current_phase.number,
            next_phase.number,
            next_phase
                .tasks
                .iter()
                .filter(|t| {
                    let s = status_map
                        .get(&t.number)
                        .map(String::as_str)
                        .unwrap_or("pending");
                    s == "pending" || s == "failed"
                })
                .count(),
        );
        crate::notifications::notify(webhook_snapshot, msg);
    }
}

/// Spawn at most one auto-advance agent from `tasks`, a pre-filtered
/// list of ready candidates from `phase`. We iterate in order and break
/// after the first successful `claim_task` + `start_pty_agent` — this
/// is the unconditional sequential gate (Task 3.5.1): one agent per
/// `try_auto_advance` call, always. The next ready task in the chain
/// spawns when the current task's completion fires another
/// `try_auto_advance` (via `on_task_agent_completed` for auto-mode
/// plans / `task_status_changed` for auto-advance plans).
///
/// Returns the task numbers we actually claimed and spawned (length 0
/// or 1) — callers use this to gate any aggregate follow-up broadcast
/// (`task_advanced` for intra-phase advance stays quiet when every
/// candidate lost its claim race).
///
/// The `parallel: bool` per-task column (Phase 3.5.2) is what flips
/// this from sequential to fan-out, but only when both the column AND
/// the worktree cross-check (3.5.3) say yes. Today neither holds, so
/// this loop break is the load-bearing safety against parallel agents
/// stomping on a single working tree.
#[allow(clippy::too_many_arguments)]
async fn spawn_ready_tasks(
    registry: &AgentRegistry,
    plans_dir: &Path,
    plan: &ParsedPlan,
    phase: &PlanPhase,
    tasks: &[&PlanTask],
    effort: Effort,
    port: u16,
    max_budget_usd: Option<f64>,
    work_dir: &Path,
) -> Vec<String> {
    let mut spawned: Vec<String> = Vec::with_capacity(1);
    for task in tasks {
        if !claim_task(&registry.db, &plan.name, &task.number) {
            continue;
        }

        broadcast_event(
            &registry.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": plan.name,
                "task_number": task.number,
                "status": "in_progress",
            }),
        );

        let cross_ctx = build_cross_plan_context(&registry.db, plans_dir, plan, &task.number);
        // Auto-advance spawns use the default driver; check whether it
        // auto-registers MCP so the prompt picks MCP tool vs curl.
        let mcp_available = registry.drivers.injects_mcp(None);
        let prompt = build_task_prompt(
            plan,
            phase,
            task,
            false,
            port,
            cross_ctx.as_deref(),
            mcp_available,
        );
        let branch_name = format!("branchwork/{}/{}", plan.name, task.number);

        pty_agent::start_pty_agent(
            registry,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: work_dir,
                plan_name: Some(&plan.name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd,
                driver: None,
                user_id: None,
                org_id: None,
                runner_id: None,
            },
        )
        .await;

        spawned.push(task.number.clone());
        // Sequential gate: one agent per `try_auto_advance` call.
        // See doc comment above for the full rationale.
        break;
    }
    spawned
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        (crate::db::init(&path), dir)
    }

    #[test]
    fn process_alive_reports_current_pid_alive() {
        // The test process itself is unambiguously alive on every supported
        // OS. This is the load-bearing positive case for the Windows
        // OpenProcess + GetExitCodeProcess wiring (the Unix kill(0) path is
        // exercised by the same assertion).
        let me = std::process::id() as i64;
        assert!(process_alive(me), "current process should be alive");
    }

    #[test]
    fn process_alive_reports_sentinel_pid_dead() {
        // i32::MAX is above any plausible OS pid_max, so OpenProcess
        // (Windows) and kill(0) (Unix) both fail. Mirrors the high-PID
        // sentinel that cleanup_and_reattach uses to prove rows are
        // orphaned. (We deliberately don't assert PID 0 / negative PIDs
        // here — POSIX kill(2) treats them as process-group sentinels and
        // succeeds, so the result legitimately diverges from Windows.)
        assert!(
            !process_alive(i32::MAX as i64),
            "i32::MAX must report as dead"
        );
    }

    #[test]
    fn auto_advance_default_off() {
        let (db, _dir) = fresh_db();
        assert!(!auto_advance_enabled(&db, "p1"));
    }

    #[test]
    fn auto_advance_toggle() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(auto_advance_enabled(&db, "p1"));
        assert!(!auto_advance_enabled(&db, "p2"));

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE plan_auto_advance SET enabled = 0 WHERE plan_name = ?1",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(!auto_advance_enabled(&db, "p1"));
    }

    #[test]
    fn claim_task_only_first_caller_wins() {
        let (db, _dir) = fresh_db();
        // No row exists yet — first claim creates as in_progress.
        assert!(claim_task(&db, "p1", "1.1"));
        // Second claim sees in_progress, refuses.
        assert!(!claim_task(&db, "p1", "1.1"));
    }

    #[test]
    fn claim_task_reclaims_failed() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'failed')",
                params!["p1", "1.1"],
            )
            .unwrap();
        }
        // Failed tasks can be re-spawned.
        assert!(claim_task(&db, "p1", "1.1"));
        assert!(!claim_task(&db, "p1", "1.1"));
    }

    /// Regression: two CI-passed completions arriving close together in
    /// the auto-mode loop both reach `try_auto_advance` after their
    /// respective tasks completed. Without this gate, each call picks a
    /// *different* ready task in the next phase and both spawn — two
    /// concurrent agents on a single working tree. The DB-side
    /// `NOT EXISTS` predicate makes the second call lose the claim
    /// attempt without needing a tokio mutex around the whole advance.
    fn insert_running_agent(db: &Db, id: &str, plan_name: &str, task_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES (?1, '/tmp/proj', 'running', 'pty', ?2, ?3)",
            params![id, plan_name, task_id],
        )
        .unwrap();
    }

    #[test]
    fn claim_task_defers_when_sibling_agent_is_running_for_same_plan() {
        let (db, _dir) = fresh_db();
        insert_running_agent(&db, "agent-A", "p1", "1.1");

        // Sibling task in the same plan must NOT claim while agent-A
        // runs — the per-plan idle gate is what keeps auto-advance
        // strictly sequential across concurrent advance triggers.
        assert!(!claim_task(&db, "p1", "1.2"));

        // Different plan is unaffected — the gate is per-plan.
        assert!(claim_task(&db, "p2", "1.1"));

        // Once agent-A is no longer 'running'/'starting', sibling
        // claims succeed.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = 'completed' WHERE id = 'agent-A'",
                [],
            )
            .unwrap();
        }
        assert!(claim_task(&db, "p1", "1.2"));
    }

    #[test]
    fn claim_task_ignores_check_agents_in_idle_gate() {
        let (db, _dir) = fresh_db();
        // Check agents (mode='stream-json') are read-only verification
        // runs that don't touch the worktree, so they shouldn't gate
        // auto-advance — assert the predicate's `mode != 'stream-json'`
        // exclusion holds.
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES ('check-1', '/tmp/proj', 'running', 'stream-json', 'p1', '1.1')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(claim_task(&db, "p1", "1.2"));
    }

    #[test]
    fn claim_task_defers_for_starting_agents_too_not_just_running() {
        let (db, _dir) = fresh_db();
        // The window between agent-row insert and the spawn-side
        // status flip to 'running' is short but real; the gate must
        // see 'starting' as in-flight too, otherwise two advances can
        // both squeeze through during that window.
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES ('agent-A', '/tmp/proj', 'starting', 'pty', 'p1', '1.1')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(!claim_task(&db, "p1", "1.2"));
    }

    fn write_yaml_plan(dir: &std::path::Path, name: &str, project: &str, body: &str) {
        let path = dir.join(format!("{name}.yaml"));
        let yaml = format!("title: \"{name} title\"\nproject: {project}\nphases:\n{body}");
        std::fs::write(path, yaml).unwrap();
    }

    #[test]
    fn cross_plan_context_includes_sibling_completed_tasks() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // Sibling plan in the same project with a completed auth task.
        write_yaml_plan(
            &plans_dir,
            "auth-plan",
            "demo-proj",
            "  - number: 1\n\
             \x20   title: \"Auth\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Build auth middleware\"\n\
             \x20       file_paths: [\"src/middleware/auth.ts\"]\n",
        );

        // Current plan in the same project. Task 2.1 is what we'll be working on.
        write_yaml_plan(
            &plans_dir,
            "checkout-plan",
            "demo-proj",
            "  - number: 2\n\
             \x20   title: \"Checkout\"\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: \"Add checkout endpoint\"\n",
        );

        // Mark sibling task complete and store a learning on it.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'completed')",
                params!["auth-plan", "1.1"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
                params!["auth-plan", "1.1", "Uses JWT with refresh tokens"],
            )
            .unwrap();
        }

        let current = plan_parser::parse_plan_file(&plans_dir.join("checkout-plan.yaml")).unwrap();
        let ctx = build_cross_plan_context(&db, &plans_dir, &current, "2.1")
            .expect("expected sibling context");

        assert!(ctx.contains("Build auth middleware"), "got: {ctx}");
        assert!(ctx.contains("src/middleware/auth.ts"), "got: {ctx}");
        assert!(ctx.contains("auth-plan title"), "got: {ctx}");
        assert!(ctx.contains("Uses JWT with refresh tokens"), "got: {ctx}");
    }

    #[test]
    fn cross_plan_context_excludes_other_projects_and_self() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        write_yaml_plan(
            &plans_dir,
            "unrelated",
            "other-proj",
            "  - number: 1\n\
             \x20   title: \"Unrelated\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Should not appear\"\n",
        );

        write_yaml_plan(
            &plans_dir,
            "current",
            "demo-proj",
            "  - number: 1\n\
             \x20   title: \"Cur\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Already done in this plan\"\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: \"The task being worked on\"\n",
        );

        // Both unrelated/1.1 and current/1.1 are completed; only current/1.1 should
        // appear (same project) and current/1.2 (the task being worked on) must
        // never appear.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('unrelated', '1.1', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('current', '1.1', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('current', '1.2', 'completed')",
                [],
            )
            .unwrap();
        }

        let current = plan_parser::parse_plan_file(&plans_dir.join("current.yaml")).unwrap();
        let ctx = build_cross_plan_context(&db, &plans_dir, &current, "1.2")
            .expect("expected own-plan context");

        assert!(ctx.contains("Already done in this plan"), "got: {ctx}");
        assert!(
            !ctx.contains("Should not appear"),
            "leaked unrelated project: {ctx}"
        );
        assert!(
            !ctx.contains("The task being worked on"),
            "leaked current task into its own context: {ctx}",
        );
    }

    #[test]
    fn cross_plan_context_none_when_no_project() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // No `project:` field — plan can't be cross-referenced.
        let path = plans_dir.join("orphan.yaml");
        std::fs::write(
            &path,
            "title: Orphan\nphases:\n  - number: 1\n    title: Solo\n    tasks:\n      - number: \"1.1\"\n        title: Lonely\n",
        )
        .unwrap();

        let current = plan_parser::parse_plan_file(&path).unwrap();
        assert!(build_cross_plan_context(&db, &plans_dir, &current, "1.1").is_none());
    }

    #[test]
    fn claim_task_skips_completed() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'completed')",
                params!["p1", "1.1"],
            )
            .unwrap();
        }
        // Completed tasks are never re-spawned.
        assert!(!claim_task(&db, "p1", "1.1"));
    }

    fn sample_plan_for_prompt() -> (ParsedPlan, PlanPhase, PlanTask) {
        let task = PlanTask {
            number: "2.6".to_string(),
            title: "Update agent prompts".to_string(),
            description: "Replace curl with MCP tools.".to_string(),
            file_paths: vec![],
            acceptance: String::new(),
            dependencies: vec![],
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        };
        let phase = PlanPhase {
            number: 2,
            title: "MCP Server".to_string(),
            description: String::new(),
            tasks: vec![task.clone()],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let plan = ParsedPlan {
            name: "portable-agents".to_string(),
            file_path: String::new(),
            title: "Portable agents".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![phase.clone()],
            verification: None,
            ci_blocking_workflows: None,
            phase_verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        };
        (plan, phase, task)
    }

    #[test]
    fn build_task_prompt_uses_mcp_tool_when_available() {
        let (plan, phase, task) = sample_plan_for_prompt();
        let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, true);
        // MCP branch: tool name appears, curl PUT does not.
        assert!(
            prompt.contains("update_task_status"),
            "expected MCP tool mention, got: {prompt}"
        );
        assert!(
            !prompt.contains("curl -s -X PUT"),
            "curl PUT should be omitted when MCP is available: {prompt}"
        );
        // Learnings curl (step 2) is still there regardless of MCP.
        assert!(
            prompt.contains("curl -s -X POST"),
            "learnings curl should remain: {prompt}"
        );
    }

    #[test]
    fn build_task_prompt_falls_back_to_curl_when_mcp_unavailable() {
        let (plan, phase, task) = sample_plan_for_prompt();
        let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, false);
        // Curl branch: PUT appears, MCP tool does not.
        assert!(
            prompt.contains("curl -s -X PUT"),
            "expected curl PUT fallback, got: {prompt}"
        );
        assert!(
            !prompt.contains("update_task_status"),
            "MCP tool should not be mentioned when unavailable: {prompt}"
        );
    }

    #[test]
    fn build_task_prompt_embeds_unattended_contract_block() {
        let (plan, phase, task) = sample_plan_for_prompt();
        // Both MCP and curl branches must carry the contract — auto-mode
        // can spawn either flavour and the no-push / no-ask rules apply
        // uniformly.
        for mcp in [true, false] {
            let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, mcp);
            assert!(
                prompt.contains("Unattended-execution contract"),
                "contract heading missing (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("branchwork/portable-agents/2.6"),
                "task branch must be interpolated literally (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("Do not run `git push`"),
                "no-push rule missing (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("Do not ask the user"),
                "no-ask rule missing (mcp={mcp}): {prompt}"
            );
        }
    }

    #[test]
    fn build_plan_session_prompt_loads_plan_context_without_completion_contract() {
        let (mut plan, _phase, _task) = sample_plan_for_prompt();
        plan.context = "Migrate every agent to MCP-based status updates.".into();

        let prompt = build_plan_session_prompt(&plan, 3100, true);

        // Plan identity surfaces so the agent knows what it's been spawned for.
        assert!(
            prompt.contains("Portable agents") && prompt.contains("portable-agents"),
            "plan title and slug must appear: {prompt}"
        );
        assert!(
            prompt.contains("Phase 2: MCP Server"),
            "phase summary missing: {prompt}"
        );
        assert!(
            prompt.contains("Migrate every agent to MCP-based status updates."),
            "plan context must be embedded: {prompt}"
        );

        // The "wait for instructions" framing is the whole point — no
        // completion contract, no commit mandate, no auto-exit.
        assert!(
            prompt.contains("Wait for the user's instructions"),
            "session prompt must instruct the agent to wait: {prompt}"
        );
        assert!(
            !prompt.contains("Unattended-execution contract"),
            "plan session must NOT carry the task contract: {prompt}"
        );
        assert!(
            !prompt.contains("git add -A && git commit"),
            "plan session must NOT mandate a commit: {prompt}"
        );
        assert!(
            !prompt.contains("Mark the task status"),
            "plan session must NOT push a status update: {prompt}"
        );
    }

    #[test]
    fn build_plan_session_prompt_falls_back_to_http_when_mcp_unavailable() {
        let (plan, _phase, _task) = sample_plan_for_prompt();
        let prompt = build_plan_session_prompt(&plan, 3100, false);
        assert!(
            prompt.contains("http://localhost:3100/api/plans/portable-agents/"),
            "expected HTTP fallback hint, got: {prompt}"
        );
        assert!(
            !prompt.contains("`branchwork` MCP server"),
            "MCP hint must be absent when unavailable: {prompt}"
        );
    }

    #[test]
    fn build_task_prompt_mandates_commit_before_status_update() {
        let (plan, phase, task) = sample_plan_for_prompt();
        for mcp in [true, false] {
            let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, mcp);
            assert!(
                prompt.contains("git add -A && git commit"),
                "expected commit mandate (mcp={mcp}): {prompt}"
            );
            let commit_idx = prompt.find("git add -A && git commit").unwrap();
            let status_idx = prompt
                .find("Mark the task status")
                .expect("status step present");
            assert!(
                commit_idx < status_idx,
                "commit step must precede status step (mcp={mcp})"
            );
        }
    }

    /// Build a registry wired to a fresh DB + an in-memory broadcast channel
    /// so cleanup_and_reattach has everything it needs to run without
    /// touching the filesystem or network.
    fn test_registry(db: Db) -> (AgentRegistry, tokio::sync::broadcast::Receiver<String>) {
        let (tx, rx) = tokio::sync::broadcast::channel::<String>(32);
        let mut registry = AgentRegistry::new(
            db,
            tx,
            None,
            PathBuf::from("/tmp/branchwork-test-sockets"),
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        // Auto-advance tests spawn against fake project dirs
        // (`branchwork-no-such-dir-*`), so `git worktree add` always fails —
        // but `setup_worktree` still `create_dir_all`s the base first. Pin the
        // base under the OS temp dir so that pollution never lands in the real
        // `~/.branchwork/worktrees`. Never set `$BRANCHWORK_WORKTREE_BASE`
        // here: a process-wide env write would race parallel test binaries.
        registry.worktree_base_override =
            Some(std::env::temp_dir().join("branchwork-mod-test-worktrees"));
        (registry, rx)
    }

    fn insert_agent(
        db: &Db,
        id: &str,
        mode: &str,
        status: &str,
        pid: Option<i64>,
        socket: Option<&str>,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, pid, supervisor_socket) \
             VALUES (?1, '/tmp', ?2, ?3, ?4, ?5)",
            params![id, status, mode, pid, socket],
        )
        .unwrap();
    }

    fn agent_status(db: &Db, id: &str) -> (String, Option<String>) {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status, stop_reason FROM agents WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
    }

    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    #[tokio::test]
    async fn cleanup_orphans_pty_row_without_supervisor_socket() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // PTY agent that never recorded a socket (pre-upgrade / crashed mid-spawn).
        insert_agent(&db, "agent-no-sock", "pty", "running", Some(1), None);
        registry.cleanup_and_reattach().await;

        let (status, reason) = agent_status(&db, "agent-no-sock");
        assert_eq!(status, "failed", "expected status=failed");
        assert_eq!(reason.as_deref(), Some("orphaned"));

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains("agent-no-sock")
                && e.contains("orphaned")),
            "expected agent_stopped/orphaned broadcast, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_orphans_stream_json_agent_regardless_of_pid() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Use PID 1 (init) so process_alive returns true on Unix — we want to
        // prove stream-json rows are orphaned even when the PID is alive,
        // because the server lost the stdin/stdout handles.
        insert_agent(&db, "check-alive", "stream-json", "running", Some(1), None);
        // And a starting row with a dead PID, for good measure.
        insert_agent(
            &db,
            "check-dead",
            "stream-json",
            "starting",
            Some(0x7fff_ffff),
            None,
        );

        registry.cleanup_and_reattach().await;

        for id in ["check-alive", "check-dead"] {
            let (status, reason) = agent_status(&db, id);
            assert_eq!(status, "failed", "{id}: expected failed");
            assert_eq!(
                reason.as_deref(),
                Some("orphaned"),
                "{id}: expected orphaned"
            );
        }

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("check-alive") && e.contains("orphaned")),
            "missing orphan broadcast for check-alive: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.contains("check-dead") && e.contains("orphaned")),
            "missing orphan broadcast for check-dead: {events:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_marks_pty_row_with_dead_pid_as_supervisor_died() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Socket present but PID very unlikely to be alive. i32::MAX is above
        // the typical pid_max; kill(0) will return ESRCH. The named-wart
        // (Task 11.6) classification: the supervisor specifically went
        // away (kill -9, OOM killer) — the row's history isn't lost, so
        // `killed`/`supervisor_died` is the right label, not the more
        // generic `failed`/`orphaned`.
        insert_agent(
            &db,
            "pty-dead",
            "pty",
            "running",
            Some(i32::MAX as i64),
            Some("/tmp/branchwork-test-sockets/does-not-exist.sock"),
        );

        registry.cleanup_and_reattach().await;

        let (status, reason) = agent_status(&db, "pty-dead");
        assert_eq!(status, "killed");
        assert_eq!(reason.as_deref(), Some("supervisor_died"));

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("pty-dead") && e.contains("supervisor_died")),
            "missing supervisor_died broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_leaves_remote_row_alone_for_runner_hello_to_reconcile() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // T5.15: remote rows are NOT touched by `cleanup_and_reattach`.
        // Liveness for SaaS agents is the runner's call; the server defers
        // to `RunnerHello.active_agents` to confirm or contradict each
        // surviving row. Pre-T5.15 we marked them all killed and fanned
        // out KillAgent, which post-T5.13 actively SIGTERM'd every live
        // daemon on every prod redeploy — see the comment in
        // `cleanup_and_reattach` for the full reasoning.
        insert_agent(&db, "remote-stale", "remote", "running", None, None);

        registry.cleanup_and_reattach().await;

        let (status, reason) = agent_status(&db, "remote-stale");
        assert_eq!(status, "running", "remote row must be left at 'running'");
        assert_eq!(reason, None, "stop_reason must NOT be set");

        let events = drain_events(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e.contains("remote-stale") && e.contains("supervisor_died")),
            "no supervisor_died broadcast for remote rows: {events:?}"
        );
    }

    #[tokio::test]
    async fn mark_supervisor_unreachable_flips_running_row_and_broadcasts() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        insert_agent(
            &db,
            "wedged",
            "pty",
            "running",
            Some(1),
            Some("/tmp/w.sock"),
        );
        registry.mark_supervisor_unreachable("wedged").await;

        let (status, reason) = agent_status(&db, "wedged");
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("supervisor_unreachable"));

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains("wedged")
                && e.contains("supervisor_unreachable")),
            "expected agent_stopped/supervisor_unreachable broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn mark_supervisor_unreachable_leaves_terminal_rows_alone() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Already-killed agent: the heartbeat shouldn't overwrite the row.
        insert_agent(&db, "already-killed", "pty", "killed", Some(1), None);
        registry.mark_supervisor_unreachable("already-killed").await;

        let (status, reason) = agent_status(&db, "already-killed");
        assert_eq!(status, "killed", "terminal status must not be overwritten");
        assert_eq!(reason.as_deref(), None);

        // The broadcast still fires (the method doesn't read back the DB to
        // decide), but what matters is that the row survives. Drain to keep
        // the channel tidy.
        let _ = drain_events(&mut rx);
    }

    #[tokio::test]
    async fn cleanup_leaves_non_running_rows_alone() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Rows already in a terminal state must not be rewritten.
        insert_agent(&db, "done", "pty", "completed", Some(1), None);
        insert_agent(&db, "killed", "pty", "killed", Some(1), None);

        registry.cleanup_and_reattach().await;

        assert_eq!(agent_status(&db, "done").0, "completed");
        assert_eq!(agent_status(&db, "killed").0, "killed");
        // reconcile passes run and may emit unrelated events; what matters is
        // that neither completed nor killed got touched.
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| (e.contains("\"id\":\"done\"")
                || e.contains("\"id\":\"killed\""))
                && e.contains("agent_stopped")),
            "terminal rows re-broadcast: {events:?}"
        );
    }

    // `git_default_branch` / `git_list_branches` tests live alongside the
    // implementations in `crate::git_helpers` — they moved out with the
    // helpers when the leaf module was carved out for the runner binary.

    // ── reconcile_task_statuses tests (Task 2.1) ──────────────────────
    //
    // Pins the boot-sweep behaviour for `task_status` rows stuck in
    // `checking`. When the server died mid-check, the row is wedged: the
    // task card stays locked and no agent is alive to resolve it. The
    // sweep either recovers a verdict from the check-agent's
    // `agent_output` rows (server died after the verdict was streamed
    // but before `start_check_agent`'s post-exit thread persisted it) or
    // drops the row so the task reverts to its prior state.

    /// Insert a check-agent `agents` row (mode=stream-json) for a given
    /// (plan, task). The row is `failed` because pass 1 of
    /// `cleanup_and_reattach` has already orphaned every stream-json row
    /// by the time the task_status sweep runs — this fixture skips pass 1
    /// and pre-stages the post-pass state directly.
    fn insert_check_agent_row(db: &Db, id: &str, plan_name: &str, task_number: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id) \
             VALUES (?1, '/tmp/proj', 'failed', 'stream-json', ?2, ?3)",
            params![id, plan_name, task_number],
        )
        .unwrap();
    }

    /// Append an `agent_output` row whose `content` is a stream-json
    /// `result` envelope wrapping `text` — the simplest of the two
    /// formats `extract_check_agent_verdict` accepts.
    fn append_agent_output(db: &Db, agent_id: &str, text: &str) {
        let envelope = serde_json::json!({"result": text}).to_string();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, 'result', ?2)",
            params![agent_id, envelope],
        )
        .unwrap();
    }

    fn task_status_of(db: &Db, plan_name: &str, task_number: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
            params![plan_name, task_number],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    #[tokio::test]
    async fn reconcile_recovers_completed_verdict_from_agent_output() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Stage the wedge: task at `checking`, check-agent dead with a
        // verdict already on disk in agent_output.
        set_task_status(&db, "p1", "1.1", "checking");
        insert_check_agent_row(&db, "agent-c1", "p1", "1.1");
        append_agent_output(
            &db,
            "agent-c1",
            r#"Looks good. {"status": "completed", "reason": "all tests pass"}"#,
        );

        registry.cleanup_and_reattach().await;

        assert_eq!(
            task_status_of(&db, "p1", "1.1").as_deref(),
            Some("completed"),
            "verdict=completed must promote task_status to completed"
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("task_status_changed")
                && e.contains("\"status\":\"completed\"")
                && e.contains("recovered verdict")),
            "expected task_status_changed/completed broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_recovers_in_progress_verdict_as_pending() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        set_task_status(&db, "p1", "1.2", "checking");
        insert_check_agent_row(&db, "agent-c2", "p1", "1.2");
        append_agent_output(
            &db,
            "agent-c2",
            r#"Still working. {"status": "in_progress", "reason": "tests not run"}"#,
        );

        registry.cleanup_and_reattach().await;

        assert_eq!(
            task_status_of(&db, "p1", "1.2").as_deref(),
            Some("pending"),
            "verdict=in_progress must collapse to task_status=pending"
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("task_status_changed")
                && e.contains("\"status\":\"pending\"")
                && e.contains("in_progress")),
            "expected task_status_changed/pending broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_recovers_failed_verdict_as_failed() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // The current check-agent vocabulary doesn't emit `failed`, but
        // the mapping is symmetric and forward-compatible: if a verdict
        // ever surfaces with status=failed we honour it rather than
        // silently downgrading to pending.
        set_task_status(&db, "p1", "1.3", "checking");
        insert_check_agent_row(&db, "agent-c3", "p1", "1.3");
        append_agent_output(
            &db,
            "agent-c3",
            r#"Broken. {"status": "failed", "reason": "tests red"}"#,
        );

        registry.cleanup_and_reattach().await;

        assert_eq!(
            task_status_of(&db, "p1", "1.3").as_deref(),
            Some("failed"),
            "verdict=failed must promote task_status to failed"
        );
        let _ = drain_events(&mut rx);
    }

    #[tokio::test]
    async fn reconcile_deletes_row_when_no_verdict_was_produced() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Check-agent died mid-stream — agent_output has assistant
        // chatter but no verdict JSON. Falling back to a clean delete is
        // safer than guessing.
        set_task_status(&db, "p1", "1.4", "checking");
        insert_check_agent_row(&db, "agent-c4", "p1", "1.4");
        append_agent_output(&db, "agent-c4", "let me investigate the test suite");

        registry.cleanup_and_reattach().await;

        assert!(
            task_status_of(&db, "p1", "1.4").is_none(),
            "no verdict must drop the row, leaving the task at implicit pending"
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("task_status_changed")
                && e.contains("\"status\":null")
                && e.contains("no verdict recoverable")),
            "expected task_status_changed/null broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_deletes_row_when_no_check_agent_row_exists() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // task_status is `checking` but no `agents` row was ever inserted
        // (e.g. the row was pruned out-of-band). The sweep must still
        // unstick the task — verdict lookup returns None ⇒ delete path.
        set_task_status(&db, "p1", "1.5", "checking");

        registry.cleanup_and_reattach().await;

        assert!(
            task_status_of(&db, "p1", "1.5").is_none(),
            "missing agent row must still drop the stuck task_status"
        );
        let _ = drain_events(&mut rx);
    }

    #[tokio::test]
    async fn reconcile_leaves_checking_alone_when_agent_still_alive() {
        let (db, _dir) = fresh_db();
        let (registry, _rx) = test_registry(db.clone());

        // The full `cleanup_and_reattach` flow always orphans stream-json
        // rows in pass 1 (IO handles are gone after a server restart), so
        // the only way a check-agent row can be alive when pass 2 runs is
        // if the sweep is invoked outside boot — exercised by calling
        // `reconcile_task_statuses` directly. Defensive contract: if any
        // agent for the task is still in the running/starting set, leave
        // task_status alone.
        set_task_status(&db, "p1", "1.6", "checking");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id) \
                 VALUES ('agent-live', '/tmp/proj', 'running', 'stream-json', 'p1', '1.6')",
                [],
            )
            .unwrap();
        }

        registry.reconcile_task_statuses().await;

        assert_eq!(
            task_status_of(&db, "p1", "1.6").as_deref(),
            Some("checking"),
            "live check-agent must keep task_status at checking"
        );
    }

    #[tokio::test]
    async fn reconcile_picks_most_recent_check_agent_when_multiple_exist() {
        let (db, _dir) = fresh_db();
        let (registry, _rx) = test_registry(db.clone());

        // Two stream-json rows for the same (plan, task): the earlier one
        // returned an in_progress verdict, the later one came back
        // completed. ORDER BY started_at DESC must pick the later run.
        set_task_status(&db, "p1", "1.7", "checking");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id, started_at) \
                 VALUES ('agent-old', '/tmp/proj', 'failed', 'stream-json', 'p1', '1.7', '2026-01-01 00:00:00')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id, started_at) \
                 VALUES ('agent-new', '/tmp/proj', 'failed', 'stream-json', 'p1', '1.7', '2026-05-01 00:00:00')",
                [],
            )
            .unwrap();
        }
        append_agent_output(
            &db,
            "agent-old",
            r#"{"status": "in_progress", "reason": "stale"}"#,
        );
        append_agent_output(
            &db,
            "agent-new",
            r#"{"status": "completed", "reason": "fresh"}"#,
        );

        registry.cleanup_and_reattach().await;

        assert_eq!(
            task_status_of(&db, "p1", "1.7").as_deref(),
            Some("completed"),
            "most recent check-agent's verdict must win"
        );
    }

    // ── reconcile_orphaned_branches tests (Task 2.2) ──────────────────
    //
    // Pins the boot-sweep contract for `agents.branch` rows whose ref no
    // longer resolves in the project's git: clear the column and
    // broadcast `agent_branch_cleared` so the merge banner doesn't keep
    // offering a doomed action. Three guards keep the sweep safe:
    // remote (SaaS) agents are skipped because the branch lives on the
    // runner's filesystem; live (`running`/`starting`) rows are skipped
    // defensively for non-boot callers; missing-cwd rows are skipped so
    // a moved project directory doesn't silently strip valid branches.

    /// Init a git repo at `dir` with a single empty commit on `branch`.
    /// Mirrors `git_helpers::tests::git_init_with_commit` — kept inline
    /// so this test module stays self-contained.
    fn git_init_with_commit(dir: &std::path::Path, initial_branch: &str) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        run(&["init", "-b", initial_branch]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
    }

    /// Create `branch` at the current HEAD without checking it out.
    fn git_create_branch(dir: &std::path::Path, branch: &str) {
        let ok = std::process::Command::new("git")
            .args(["branch", branch])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git branch {branch} failed in {}", dir.display());
    }

    /// Insert an `agents` row with explicit cwd / mode / status / branch.
    /// The shared `insert_agent` fixture hardcodes `cwd='/tmp'` which
    /// can't host a real git repo for the show-ref probe.
    fn insert_branched_agent(db: &Db, id: &str, cwd: &str, mode: &str, status: &str, branch: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, branch) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, cwd, status, mode, branch],
        )
        .unwrap();
    }

    fn agent_branch(db: &Db, id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT branch FROM agents WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    // ── setup_agent_worktree (Task 2.1) ──────────────────────────────

    #[test]
    fn setup_agent_worktree_substitutes_placeholders_for_freestanding_agent() {
        // Pin the on-disk path shape: a freestanding agent (no plan / no
        // task) must still land at a deterministic, non-empty path component
        // rather than `<base>/<slug>//-<id>`.
        assert_eq!(NO_PLAN_SEGMENT, "_no-plan");
        assert_eq!(NO_TASK_SEGMENT, "_no-task");

        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_with_commit(proj.path(), "master");

        let path = setup_agent_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            None, // no plan
            None, // no task
            "abcdef1234567890",
            "branchwork/freestanding/solo",
            None,
            false,
        )
        .expect("freestanding worktree should be created");

        assert_eq!(
            path,
            base.path()
                .join("proj")
                .join("_no-plan")
                .join("_no-task-abcdef12"),
            "None plan/task must resolve to the _no-plan/_no-task placeholders"
        );
        assert!(path.exists(), "worktree directory should exist");
    }

    #[test]
    fn setup_agent_worktree_passes_through_and_partially_substitutes() {
        // The adapter only fills `None` segments; provided plan/task pass
        // through to `worktree::setup_worktree_in` verbatim. One real repo,
        // three distinct (branch, agent) pairs so the worktrees don't collide.
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_with_commit(proj.path(), "master");

        // Both present → both pass through.
        let both = setup_agent_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            Some("my-plan"),
            Some("0.1"),
            "aaaaaaaa00",
            "branchwork/my-plan/0.1",
            None,
            false,
        )
        .expect("worktree should be created");
        assert_eq!(
            both,
            base.path()
                .join("proj")
                .join("my-plan")
                .join("0.1-aaaaaaaa")
        );

        // Plan present, task absent → only task substituted.
        let task_only = setup_agent_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            Some("my-plan"),
            None,
            "bbbbbbbb00",
            "branchwork/my-plan/no-task",
            None,
            false,
        )
        .expect("worktree should be created");
        assert_eq!(
            task_only,
            base.path()
                .join("proj")
                .join("my-plan")
                .join("_no-task-bbbbbbbb")
        );

        // Plan absent, task present → only plan substituted.
        let plan_only = setup_agent_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            None,
            Some("0.2"),
            "cccccccc00",
            "branchwork/freestanding/0.2",
            None,
            false,
        )
        .expect("worktree should be created");
        assert_eq!(
            plan_only,
            base.path()
                .join("proj")
                .join("_no-plan")
                .join("0.2-cccccccc")
        );
    }

    #[tokio::test]
    async fn reconcile_clears_branch_when_local_ref_missing() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        let proj = TempDir::new().unwrap();
        git_init_with_commit(proj.path(), "master");
        // Branch never created (or was `git branch -D`'d out of band).

        insert_branched_agent(
            &db,
            "agent-ghost",
            proj.path().to_str().unwrap(),
            "pty",
            "completed",
            "branchwork/p1/1.1",
        );

        registry.reconcile_orphaned_branches().await;

        assert!(
            agent_branch(&db, "agent-ghost").is_none(),
            "missing local ref must clear the branch column"
        );
        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_branch_cleared")
                && e.contains("agent-ghost")
                && e.contains("branchwork/p1/1.1")),
            "expected agent_branch_cleared broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_keeps_branch_when_local_ref_resolves() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        let proj = TempDir::new().unwrap();
        git_init_with_commit(proj.path(), "master");
        git_create_branch(proj.path(), "branchwork/p1/1.2");

        insert_branched_agent(
            &db,
            "agent-live-branch",
            proj.path().to_str().unwrap(),
            "pty",
            "completed",
            "branchwork/p1/1.2",
        );

        registry.reconcile_orphaned_branches().await;

        assert_eq!(
            agent_branch(&db, "agent-live-branch").as_deref(),
            Some("branchwork/p1/1.2"),
            "resolvable branch must survive the sweep"
        );
        let events = drain_events(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e.contains("agent_branch_cleared") && e.contains("agent-live-branch")),
            "no broadcast must fire when nothing changed: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_skips_remote_agents_branches() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // SaaS-mode agent: cwd points at a runner-side path that may not
        // exist on the server. Local `git show-ref` would always fail
        // and clear a branch that is perfectly valid on the runner. The
        // mode='remote' filter prevents that.
        insert_branched_agent(
            &db,
            "agent-remote",
            "/home/runneruser/project",
            "remote",
            "killed",
            "branchwork/p1/1.3",
        );

        registry.reconcile_orphaned_branches().await;

        assert_eq!(
            agent_branch(&db, "agent-remote").as_deref(),
            Some("branchwork/p1/1.3"),
            "remote-mode branches must not be touched by the local sweep"
        );
        let events = drain_events(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e.contains("agent_branch_cleared") && e.contains("agent-remote")),
            "remote-mode branches must not broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_skips_live_agents_defensively() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Live row + missing branch ref. Pass 1 of `cleanup_and_reattach`
        // would have terminated this row before pass 3 ever sees it, but
        // calling `reconcile_orphaned_branches` directly skips pass 1 —
        // and the guard keeps the function safe for non-boot callers.
        let proj = TempDir::new().unwrap();
        git_init_with_commit(proj.path(), "master");

        insert_branched_agent(
            &db,
            "agent-running",
            proj.path().to_str().unwrap(),
            "pty",
            "running",
            "branchwork/p1/1.4",
        );
        insert_branched_agent(
            &db,
            "agent-starting",
            proj.path().to_str().unwrap(),
            "pty",
            "starting",
            "branchwork/p1/1.5",
        );

        registry.reconcile_orphaned_branches().await;

        assert_eq!(
            agent_branch(&db, "agent-running").as_deref(),
            Some("branchwork/p1/1.4"),
            "running rows must not have their branch cleared"
        );
        assert_eq!(
            agent_branch(&db, "agent-starting").as_deref(),
            Some("branchwork/p1/1.5"),
            "starting rows must not have their branch cleared"
        );
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| e.contains("agent_branch_cleared")),
            "live rows must not broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_skips_when_cwd_is_missing() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // cwd doesn't resolve to a directory (project was moved/renamed
        // between runs). We can't tell whether the branch was deleted or
        // is just out of view — leaving the row alone means reattaching
        // the project to the right path restores the merge banner.
        insert_branched_agent(
            &db,
            "agent-moved",
            "/nonexistent/branchwork-test/missing-project",
            "pty",
            "completed",
            "branchwork/p1/1.6",
        );

        registry.reconcile_orphaned_branches().await;

        assert_eq!(
            agent_branch(&db, "agent-moved").as_deref(),
            Some("branchwork/p1/1.6"),
            "missing cwd must not flush the branch reference"
        );
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| e.contains("agent_branch_cleared")),
            "missing cwd must not broadcast: {events:?}"
        );
    }

    // ── spawn_conflict_resolver tests (Task 3.2) ──────────────────────

    /// Build a git repo on `master` plus a `branchwork/p/1.1` task branch
    /// that genuinely conflicts with master on `file.txt`, then attach a
    /// linked worktree at `wt` checked out on the task branch — the state the
    /// agent leaves behind: committed work on its task branch in its own
    /// worktree while the trunk moved on.
    fn setup_conflicting_worktree(repo: &std::path::Path, wt: &std::path::Path) {
        let run = |dir: &std::path::Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        std::fs::create_dir_all(repo).unwrap();
        run(repo, &["init", "-b", "master"]);
        run(repo, &["config", "user.email", "t@t"]);
        run(repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-m", "base"]);
        // Fork the task branch off the base, before the trunk moves on.
        run(repo, &["branch", "branchwork/p/1.1"]);
        // Trunk changes file.txt.
        std::fs::write(repo.join("file.txt"), "target side\n").unwrap();
        run(repo, &["commit", "-am", "target change"]);
        // Attach the agent's worktree on the task branch + commit a
        // conflicting change there.
        run(
            repo,
            &["worktree", "add", wt.to_str().unwrap(), "branchwork/p/1.1"],
        );
        std::fs::write(wt.join("file.txt"), "task side\n").unwrap();
        run(wt, &["commit", "-am", "task change"]);
    }

    #[test]
    fn build_resolver_prompt_substitutes_all_placeholders() {
        let conflict = ConflictPayload {
            conflicted_paths: vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()],
            our_diff: "OUR_DIFF_BODY_X".to_string(),
            their_diff: "THEIR_DIFF_BODY_Y".to_string(),
        };
        let prompt =
            build_resolver_prompt("Add a GET /widgets endpoint.", "master", "1.1", &conflict);

        // Acceptance: the prompt carries the original task description, both
        // diffs, the conflicted paths, the target, the commit task id, and the
        // literal conflict markers the resolver is told to look for.
        assert!(
            prompt.contains("Add a GET /widgets endpoint."),
            "missing task description: {prompt}"
        );
        assert!(prompt.contains("master"), "missing target branch: {prompt}");
        assert!(
            prompt.contains("- src/foo.rs"),
            "missing conflicted path foo"
        );
        assert!(
            prompt.contains("- src/bar.rs"),
            "missing conflicted path bar"
        );
        assert!(prompt.contains("OUR_DIFF_BODY_X"), "missing our_diff");
        assert!(prompt.contains("THEIR_DIFF_BODY_Y"), "missing their_diff");
        assert!(
            prompt.contains("Resolve merge conflicts for 1.1"),
            "missing commit-message task id"
        );
        assert!(
            prompt.contains("<<<<<<<") && prompt.contains("=======") && prompt.contains(">>>>>>>"),
            "missing literal conflict markers"
        );
        assert!(
            !prompt.contains("{{"),
            "unsubstituted placeholder left: {prompt}"
        );
    }

    #[tokio::test]
    async fn spawn_conflict_resolver_threads_resolver_under_parent() {
        let (db, _dbdir) = fresh_db();
        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: "p".to_string(),
        };
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);

        // Original task agent row — the parent the resolver threads under.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, plan_name, task_id, cwd, status, mode, branch, org_id) \
                 VALUES ('parent-1', 'p', '1.1', ?1, 'completed', 'pty', 'branchwork/p/1.1', 'default-org')",
                params![wt.to_str().unwrap()],
            )
            .unwrap();
        }

        let (registry, _rx) = test_registry(db.clone());
        let conflict = ConflictPayload {
            conflicted_paths: vec!["file.txt".to_string()],
            our_diff: "diff a/file.txt b/file.txt (task)".to_string(),
            their_diff: "diff a/file.txt b/file.txt (target)".to_string(),
        };

        let resolver_id =
            spawn_conflict_resolver(&registry, "p", "1.1", "master", &conflict, 1, &wt)
                .await
                .expect("resolver should spawn");

        // Resolver row: badged mode='resolver', threaded under the parent, on
        // a `-resolve-N` task id distinct from the original task.
        type Row = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let (mode, parent, plan, task, prompt): Row = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT mode, parent_agent_id, plan_name, task_id, prompt FROM agents WHERE id = ?1",
                params![resolver_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };
        assert_eq!(
            mode, "resolver",
            "resolver row must be badged mode='resolver'"
        );
        assert_eq!(
            parent.as_deref(),
            Some("parent-1"),
            "parent_agent_id must point at the original task agent"
        );
        assert_eq!(plan.as_deref(), Some("p"));
        assert_eq!(task.as_deref(), Some("1.1-resolve-1"));
        let prompt = prompt.expect("resolver prompt stored on the row");
        assert!(
            prompt.contains("file.txt"),
            "prompt names the conflicted path"
        );
        assert!(
            prompt.contains("(task)") && prompt.contains("(target)"),
            "prompt carries both diffs"
        );
        assert!(
            prompt.contains("<<<<<<<"),
            "prompt carries the conflict-marker instructions"
        );

        // task_fix_attempts row recorded + backfilled with the resolver id.
        let attempt_agent: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT agent_id FROM task_fix_attempts \
                 WHERE plan_name='p' AND task_number='1.1' AND attempt=1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            attempt_agent.as_deref(),
            Some(resolver_id.as_str()),
            "task_fix_attempts.agent_id must be backfilled with the resolver id"
        );
    }

    #[tokio::test]
    async fn spawn_conflict_resolver_returns_cap_hit_when_budget_spent() {
        let (db, _dbdir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            // Cap of 1 with one attempt already on record → count(1) >= max(1).
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled, max_fix_attempts) VALUES ('p', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, started_at) \
                 VALUES ('p', '1.1', 1, datetime('now'))",
                [],
            )
            .unwrap();
        }
        let (registry, _rx) = test_registry(db.clone());
        let conflict = ConflictPayload {
            conflicted_paths: vec!["file.txt".to_string()],
            our_diff: String::new(),
            their_diff: String::new(),
        };

        // The worktree path is never touched — the cap check returns first.
        let res = spawn_conflict_resolver(
            &registry,
            "p",
            "1.1",
            "master",
            &conflict,
            2,
            std::path::Path::new("/nonexistent-cap-hit"),
        )
        .await;

        assert!(
            matches!(res, Err(SpawnError::CapHit)),
            "expected CapHit, got {res:?}"
        );
        let resolver_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM agents WHERE mode = 'resolver'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(resolver_count, 0, "cap hit must not spawn a resolver");
    }

    // ── cancel_in_flight_resolvers (Task 3.4) ─────────────────────────────

    /// Insert a fully-specified agent row for the cancellation tests.
    #[allow(clippy::too_many_arguments)]
    fn insert_resolver_test_agent(
        db: &Db,
        id: &str,
        plan_name: &str,
        task_id: &str,
        cwd: &std::path::Path,
        status: &str,
        mode: &str,
        parent_agent_id: Option<&str>,
        merge_status: Option<&str>,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents \
                (id, plan_name, task_id, cwd, status, mode, parent_agent_id, merge_status, org_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'default-org')",
            params![
                id,
                plan_name,
                task_id,
                cwd.to_str().unwrap(),
                status,
                mode,
                parent_agent_id,
                merge_status
            ],
        )
        .unwrap();
    }

    /// Pull the first `resolver_cancelled` event's `data` object out of a
    /// drained broadcast stream (skipping the `agent_stopped` the kill emits).
    fn first_resolver_cancelled(events: &[String]) -> Option<serde_json::Value> {
        events.iter().find_map(|raw| {
            let v: serde_json::Value = serde_json::from_str(raw).ok()?;
            if v.get("type")?.as_str()? == "resolver_cancelled" {
                Some(v.get("data")?.clone())
            } else {
                None
            }
        })
    }

    /// Acceptance: toggling auto-mode off kills the running resolver, leaves
    /// its conflicted worktree on disk, and surfaces the conflicted paths
    /// under the original task card.
    #[tokio::test]
    async fn cancel_in_flight_resolvers_kills_resolver_preserves_worktree_and_surfaces_paths() {
        let (db, _dbdir) = fresh_db();
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);

        // Reconstruct the conflict on disk (as `reconstruct_merge_conflict`
        // does at resolver spawn) so the worktree carries a live `UU` entry.
        let _ = std::process::Command::new("git")
            .args(["merge", "master", "--no-edit"])
            .current_dir(&wt)
            .output();
        assert!(
            worktree_unmerged_paths(&wt).contains(&"file.txt".to_string()),
            "fixture must leave file.txt unmerged"
        );

        // Original task agent (the parent) + the resolver threaded under it.
        // Both hold the shared conflicted worktree (`held_for_resolver`).
        insert_resolver_test_agent(
            &db,
            "parent-1",
            "p",
            "1.1",
            &wt,
            "completed",
            "pty",
            None,
            Some(MERGE_STATUS_HELD_FOR_RESOLVER),
        );
        insert_resolver_test_agent(
            &db,
            "resolver-1",
            "p",
            "1.1-resolve-1",
            &wt,
            "running",
            "resolver",
            Some("parent-1"),
            Some(MERGE_STATUS_HELD_FOR_RESOLVER),
        );

        let (registry, mut rx) = test_registry(db.clone());
        registry.cancel_in_flight_resolvers("p").await;

        // Resolver killed; parent untouched.
        assert_eq!(agent_status(&db, "resolver-1").0, "killed");
        assert_eq!(agent_status(&db, "parent-1").0, "completed");
        // Worktree preserved on disk — `held_for_resolver` skips the cleanup
        // that would otherwise `git worktree remove` this linked tree.
        assert!(
            wt.is_dir(),
            "resolver worktree must be preserved for human inspection"
        );

        // `resolver_cancelled` surfaced the conflict under the ORIGINAL task.
        let events = drain_events(&mut rx);
        let data =
            first_resolver_cancelled(&events).expect("a resolver_cancelled event must broadcast");
        assert_eq!(data["plan"], "p");
        assert_eq!(
            data["task"], "1.1",
            "must surface under the original task id, not 1.1-resolve-1"
        );
        assert_eq!(data["agent_id"], "resolver-1");
        assert_eq!(data["parent_agent_id"], "parent-1");
        let paths: Vec<String> = serde_json::from_value(data["conflicted_paths"].clone()).unwrap();
        assert!(
            paths.contains(&"file.txt".to_string()),
            "conflicted paths must surface the unmerged file: {paths:?}"
        );

        // Re-enabling auto-mode never re-processes the now-killed resolver: a
        // second pass finds nothing running and fires no event.
        registry.cancel_in_flight_resolvers("p").await;
        assert!(
            first_resolver_cancelled(&drain_events(&mut rx)).is_none(),
            "a killed resolver must not be re-cancelled (no auto-resume)"
        );
    }

    /// Scoping: only running/starting `mode='resolver'` rows of *this* plan
    /// are cancelled — finished resolvers, other plans' resolvers, and the
    /// plan's normal task agents are all left alone.
    #[tokio::test]
    async fn cancel_in_flight_resolvers_scopes_to_running_resolvers_of_the_plan() {
        let (db, _dbdir) = fresh_db();
        let cwd = std::path::Path::new("/tmp");

        // Already-finished resolver for plan p — must not be re-killed.
        insert_resolver_test_agent(
            &db,
            "done-resolver",
            "p",
            "1.1-resolve-1",
            cwd,
            "completed",
            "resolver",
            None,
            Some(MERGE_STATUS_HELD_FOR_RESOLVER),
        );
        // Running resolver for a DIFFERENT plan — out of scope.
        insert_resolver_test_agent(
            &db,
            "other-plan-resolver",
            "q",
            "2.1-resolve-1",
            cwd,
            "running",
            "resolver",
            None,
            Some(MERGE_STATUS_HELD_FOR_RESOLVER),
        );
        // Running NON-resolver task agent for plan p — must not be killed by
        // the resolver-cancel path (the fix-agent path above owns those).
        insert_resolver_test_agent(
            &db,
            "task-agent",
            "p",
            "1.2",
            cwd,
            "running",
            "pty",
            None,
            None,
        );

        let (registry, mut rx) = test_registry(db.clone());
        registry.cancel_in_flight_resolvers("p").await;

        assert_eq!(agent_status(&db, "done-resolver").0, "completed");
        assert_eq!(agent_status(&db, "other-plan-resolver").0, "running");
        assert_eq!(agent_status(&db, "task-agent").0, "running");
        assert!(
            first_resolver_cancelled(&drain_events(&mut rx)).is_none(),
            "no in-flight resolver for plan p ⇒ no resolver_cancelled event"
        );
    }

    // ── resolver_resolution_succeeded gate (Task 3.3) ─────────────────────

    /// A resolver that committed a merge resolution on the task branch
    /// (exit 0, no unmerged paths, HEAD moved past `base_commit`) ⇒ resolved.
    #[test]
    fn resolver_resolution_succeeded_true_on_clean_resolution() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);

        let base = git_head_sha_in(&wt);
        // Resolve the conflict + commit a merge commit.
        let _ = std::process::Command::new("git")
            .args(["merge", "master", "--no-edit"])
            .current_dir(&wt)
            .output();
        std::fs::write(wt.join("file.txt"), "resolved\n").unwrap();
        run_git_in(&wt, &["add", "file.txt"]);
        run_git_in(&wt, &["commit", "-q", "-m", "resolve"]);

        assert!(
            resolver_resolution_succeeded(&wt, Some(&base), true),
            "clean exit + no unmerged + new commit ⇒ resolved"
        );
    }

    /// Non-zero exit ⇒ not resolved, regardless of tree state.
    #[test]
    fn resolver_resolution_succeeded_false_on_non_zero_exit() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);
        let base = git_head_sha_in(&wt);
        // Even with a real commit on top, a non-zero exit fails the gate.
        std::fs::write(wt.join("file.txt"), "more work\n").unwrap();
        run_git_in(&wt, &["commit", "-qam", "more"]);
        assert!(
            !resolver_resolution_succeeded(&wt, Some(&base), false),
            "non-zero exit must fail the resolution gate"
        );
    }

    /// Lingering unmerged (`UU`) paths ⇒ not resolved.
    #[test]
    fn resolver_resolution_succeeded_false_on_unmerged_paths() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);
        let base = git_head_sha_in(&wt);
        // Reproduce the conflict and leave it unresolved (no commit).
        let _ = std::process::Command::new("git")
            .args(["merge", "master", "--no-edit"])
            .current_dir(&wt)
            .output();
        assert!(
            !resolver_resolution_succeeded(&wt, Some(&base), true),
            "a worktree with UU entries must fail the gate"
        );
    }

    /// No new commit (HEAD == base_commit) ⇒ not resolved.
    #[test]
    fn resolver_resolution_succeeded_false_on_no_new_commit() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("wt");
        setup_conflicting_worktree(&repo, &wt);
        let base = git_head_sha_in(&wt);
        // Clean tree, but HEAD never moved past base — the resolver did
        // nothing useful (e.g. a no-op driver that just aborted).
        assert!(
            !resolver_resolution_succeeded(&wt, Some(&base), true),
            "HEAD unchanged ⇒ no resolution"
        );
        // Missing base_commit also fails (can't verify a new commit).
        assert!(
            !resolver_resolution_succeeded(&wt, None, true),
            "missing base_commit ⇒ can't confirm a resolution"
        );
    }

    /// Local helpers for the resolution-gate tests — small wrappers so the
    /// tests stay self-contained (the auto_mode tests have their own copies).
    fn run_git_in(cwd: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed in {}", cwd.display());
    }

    fn git_head_sha_in(cwd: &std::path::Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // ── try_auto_advance tests (Task 3.4) ─────────────────────────────
    //
    // Pin the post-3.1/3.2 contract: `try_auto_advance` scans the
    // current phase first (intra-phase advance) and only crosses a
    // phase boundary when the current phase is fully done.
    //
    // We let `start_pty_agent` run end-to-end through `test_registry`:
    // the supervisor spawn fails fast (server_exe is /nonexistent),
    // but the `agents` row is INSERTed before that point — same
    // insert-then-fail pattern the recovery / Phase 1 integration
    // tests rely on. Each spawn writes a per-session
    // `~/.claude/sessions/<session_id>.settings.json` (claude is the
    // test_registry default driver) which we clean up via
    // `SettingsCleanup`'s Drop guard so test runs don't accumulate
    // stale files in the developer's $HOME.

    /// Insert (or upsert) a `task_status` row to a fixed status.
    /// Bypasses `claim_task`'s `IN ('pending','failed')` gate so a
    /// test can pre-stage states the loop normally arrives at via
    /// spawn (in_progress, completed).
    fn set_task_status(db: &Db, plan_name: &str, task_number: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, task_number) DO UPDATE SET \
                status = excluded.status, updated_at = excluded.updated_at",
            params![plan_name, task_number, status],
        )
        .unwrap();
    }

    fn enable_auto_mode(db: &Db, plan_name: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled, paused_reason, paused_at) \
             VALUES (?1, 1, NULL, NULL)",
            params![plan_name],
        )
        .unwrap();
    }

    fn enable_auto_advance(db: &Db, plan_name: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
            params![plan_name],
        )
        .unwrap();
    }

    /// Distinct `task_id` values currently on `agents` rows for
    /// `plan_name`. Each successful spawn through `start_pty_agent`
    /// inserts one row, so this is the captured "invocation list" for
    /// `spawn_ready_tasks`.
    fn agent_task_ids(db: &Db, plan_name: &str) -> std::collections::HashSet<String> {
        let conn = db.lock().unwrap();
        conn.prepare(
            "SELECT task_id FROM agents \
             WHERE plan_name = ?1 AND task_id IS NOT NULL",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))?
                .collect::<Result<std::collections::HashSet<_>, _>>()
        })
        .unwrap_or_default()
    }

    /// `try_auto_advance` calls `start_pty_agent`, which writes a
    /// per-session `~/.claude/sessions/<session_id>.settings.json` for
    /// the claude driver. The supervisor spawn then fails (server_exe
    /// is /nonexistent), so `on_agent_exit` never fires to clean up.
    /// This Drop covers the leak so test runs don't accumulate stale
    /// settings files in the developer's $HOME.
    struct SettingsCleanup {
        db: Db,
        plan_name: String,
    }

    impl Drop for SettingsCleanup {
        fn drop(&mut self) {
            let Some(home) = dirs::home_dir() else {
                return;
            };
            let session_ids: Vec<String> = {
                let Ok(conn) = self.db.lock() else { return };
                conn.prepare(
                    "SELECT session_id FROM agents \
                     WHERE plan_name = ?1 AND session_id IS NOT NULL",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(params![&self.plan_name], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_default()
            };
            for sid in session_ids {
                let _ = session_settings::delete_for_agent_with_home(&home, &sid);
            }
        }
    }

    /// Write a YAML plan whose `project` field resolves (via
    /// `dirs::home_dir().join(...)` inside `try_auto_advance`) to a
    /// path that is NOT a real git working tree —
    /// `git_checkout_branch` inside `start_pty_agent` then fails
    /// harmlessly instead of polluting the test runner's repo.
    fn write_advance_plan(plans_dir: &Path, plan_name: &str, phases_yaml: &str) {
        let path = plans_dir.join(format!("{plan_name}.yaml"));
        let yaml = format!(
            "title: \"{plan_name} title\"\n\
             project: branchwork-no-such-dir-{plan_name}\n\
             phases:\n{phases_yaml}"
        );
        std::fs::write(path, yaml).unwrap();
    }

    fn has_task_advanced(events: &[String], plan_name: &str) -> bool {
        events.iter().any(|e| {
            e.contains("\"type\":\"task_advanced\"")
                && e.contains(&format!("\"plan\":\"{plan_name}\""))
        })
    }

    fn has_phase_advanced(events: &[String], plan_name: &str) -> bool {
        events.iter().any(|e| {
            e.contains("\"type\":\"phase_advanced\"")
                && e.contains(&format!("\"plan_name\":\"{plan_name}\""))
        })
    }

    fn phase_completed_events<'a>(events: &'a [String], plan_name: &str) -> Vec<&'a String> {
        events
            .iter()
            .filter(|e| {
                e.contains("\"type\":\"phase_completed\"")
                    && e.contains(&format!("\"plan_name\":\"{plan_name}\""))
            })
            .collect()
    }

    /// Acceptance-criteria spec: a 5-task linear chain (a→b→c→d→e)
    /// advances exactly one task per `try_auto_advance` invocation.
    /// Pre-stage `a=completed`; each tick must spawn exactly the next
    /// link, broadcast `task_advanced`, and never `phase_advanced`.
    #[tokio::test]
    async fn linear_intra_phase_chain_completes_one_task_per_completion() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "lin-chain";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"b\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"c\"]\n\
             \x20     - number: \"e\"\n\
             \x20       title: E\n\
             \x20       dependencies: [\"d\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // a was completed externally; the loop must spawn exactly b on
        // the next tick, then c, d, e — one new spawn per completion.
        set_task_status(&db, plan, "a", "completed");

        let chain = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")];
        let mut expected: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut prior_total = 0usize;

        for (from, to) in chain {
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
                None,
            )
            .await;

            expected.insert(to.to_string());
            let actual = agent_task_ids(&db, plan);
            assert_eq!(
                actual, expected,
                "after completing {from}, agents rows should be {expected:?}, got {actual:?}",
            );
            let total = actual.len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "exactly one new spawn at {from}->{to} (delta {delta})",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            assert!(
                has_task_advanced(&events, plan),
                "expected task_advanced after {from}->{to}, events: {events:?}",
            );
            assert!(
                !has_phase_advanced(&events, plan),
                "phase_advanced should not fire intra-phase ({from}->{to}): {events:?}",
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.contains(&format!("\"to_tasks\":[\"{to}\"]"))),
                "task_advanced payload should list {to:?}: {events:?}",
            );

            // Mark `to` completed so the next tick treats it as done.
            set_task_status(&db, plan, to, "completed");
        }
    }

    /// Eligibility filter: when `b` completes but `e` still has an
    /// in-progress blocker (`d`), the intra-phase scan must not spawn
    /// `e`. Once `d` completes too, the next tick spawns `e` and only
    /// `e`. (Fan-out where multiple siblings become ready at the same
    /// tick is covered by Phase 3.5 — kept out of this test.)
    #[tokio::test]
    async fn mixed_deps_within_phase_only_spawns_when_blockers_done() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "mixed-deps";
        // Deps: a -> b; c -> d; e depends on {b, d}.
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"c\"]\n\
             \x20     - number: \"e\"\n\
             \x20       title: E\n\
             \x20       dependencies: [\"b\", \"d\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Pre-stage so the first try_auto_advance call has nothing
        // ready (a/b/c done, d in_progress, e blocked on d). a/c must
        // be past pending so they aren't candidates either — net spawn
        // count = 0 on the b-completion tick.
        set_task_status(&db, plan, "a", "completed");
        set_task_status(&db, plan, "b", "completed");
        set_task_status(&db, plan, "c", "completed");
        set_task_status(&db, plan, "d", "in_progress");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "b".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        assert!(
            agent_task_ids(&db, plan).is_empty(),
            "no spawn expected — e is still blocked on d (in_progress)",
        );
        let events = drain_events(&mut rx);
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must not fire when nothing was spawned: {events:?}",
        );
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must not fire (phase still has in_progress + pending tasks): {events:?}",
        );

        // d completes → e becomes ready.
        set_task_status(&db, plan, "d", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "d".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["e".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "e should spawn once both b and d are done, got {actual:?}",
        );
        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced expected for e: {events:?}",
        );
        assert!(
            events.iter().any(|e| e.contains("\"to_tasks\":[\"e\"]")),
            "task_advanced payload should list e: {events:?}",
        );
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced should not fire intra-phase: {events:?}",
        );
    }

    /// Phase boundary still uses the next-phase path: with phase 1
    /// fully done, the tick spawns 2.1 and broadcasts
    /// `phase_advanced`. `task_advanced` must NOT fire on this path —
    /// the two events are mutually exclusive per ADR 0003 §3.
    #[tokio::test]
    async fn phase_boundary_still_fires_phase_advanced() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "phase-boundary";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: One-Two\n\
             \x20       dependencies: [\"1.1\"]\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20       dependencies: [\"2.1\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Phase 1 fully done: 1.1 + 1.2 completed.
        set_task_status(&db, plan, "1.1", "completed");
        set_task_status(&db, plan, "1.2", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.2".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        // 2.1 spawns (no deps); 2.2 stays blocked on 2.1.
        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["2.1".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "phase boundary should spawn 2.1 only (2.2 still blocked on 2.1), got {actual:?}",
        );

        let events = drain_events(&mut rx);
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced expected on phase boundary: {events:?}",
        );
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must not fire on the phase-boundary path: {events:?}",
        );
    }

    /// Two-opt-in independence (non-negotiable #10): with
    /// `plan_auto_advance.enabled=1` and no `plan_auto_mode` row,
    /// intra-phase advance must still fire. Pins that the gate at the
    /// top of `try_auto_advance` is OR, not AND, of the two opt-ins.
    #[tokio::test]
    async fn auto_advance_only_no_auto_mode_still_advances_intra_phase() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "advance-only";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n",
        );
        // Critical: enable plan_auto_advance only — no plan_auto_mode
        // row. The two opt-ins must remain independent.
        enable_auto_advance(&db, plan);
        assert!(
            auto_advance_enabled(&db, plan),
            "auto_advance should be on for this test",
        );
        assert!(
            !crate::db::auto_mode_enabled(&db, plan),
            "auto_mode must NOT be enabled for this test",
        );

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["b".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "auto_advance alone should spawn b after a completes, got {actual:?}",
        );
        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced expected with auto_advance-only opt-in: {events:?}",
        );
    }

    /// Task 3.5.1 — sequential gate: when several siblings become
    /// ready at the same tick (fan-out), `spawn_ready_tasks` must
    /// break after the first successful claim. The remaining ready
    /// tasks spawn on subsequent `try_auto_advance` ticks driven by
    /// the first task's completion. This is the load-bearing safety
    /// today (no `parallel:` column or worktree cross-check yet) so
    /// pin it explicitly: one agent per call, regardless of how many
    /// candidates the eligibility filter produced.
    #[tokio::test]
    async fn fan_out_only_spawns_one_task_per_tick() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "fan-out";
        // Three siblings (b, c, d) all depend on a — completing a
        // makes all three ready at the same tick.
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"a\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        // Exactly one of {b, c, d} spawned — the order is the YAML
        // declaration order so `b` is the deterministic pick, but we
        // assert on the count and the membership to keep the test
        // robust against future ordering tweaks in the eligibility
        // filter.
        let actual = agent_task_ids(&db, plan);
        assert_eq!(
            actual.len(),
            1,
            "exactly one task must spawn per tick (fan-out gate), got {actual:?}",
        );
        assert!(
            actual.iter().all(|t| ["b", "c", "d"].contains(&t.as_str())),
            "spawned task must be one of the ready siblings, got {actual:?}",
        );

        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced must fire for the one spawned task: {events:?}",
        );
        // `to_tasks` payload is a 1-element array — never multiple
        // siblings at once. Match exactly one of the singleton-array
        // shapes; reject any 2+ element variant.
        let task_advanced = events
            .iter()
            .find(|e| e.contains("\"type\":\"task_advanced\""))
            .expect("task_advanced event present");
        let one_of_singletons = ["[\"b\"]", "[\"c\"]", "[\"d\"]"]
            .iter()
            .any(|s| task_advanced.contains(s));
        assert!(
            one_of_singletons,
            "to_tasks must be a 1-element array, event was: {task_advanced}",
        );
    }

    // ── Task 3.5.5 — sequential gate under fan-out + ignored toggle ──
    //
    // Three regression tests pinning that the 3.5.1 break in
    // `spawn_ready_tasks` is the load-bearing safety today:
    //   1. The phase-boundary path also spawns one-per-tick when
    //      multiple dep-free tasks land in the next phase.
    //   2. Forcing `plan_auto_mode.parallel=1` via direct SQL
    //      (bypassing the API gate) does not relax the loop —
    //      the runtime ignores the column for now, and that is
    //      exactly what keeps a single working tree safe from
    //      racing agents until worktree isolation ships.
    //   3. A linear chain crossing a phase boundary calls
    //      `claim_task` exactly once per advance — proxy: agents
    //      row delta of 1 per `try_auto_advance`. Pins that the
    //      intra-phase and cross-phase paths share the gate.
    //
    // The PUT-side rejection test (412 + audit row) lives with
    // 3.5.3 in `tests/plan_config.rs` since it's HTTP-shaped:
    //   - put_parallel_true_returns_412_when_worktrees_not_shipped
    //   - put_parallel_true_audits_refused_attempt
    //
    // That HTTP audit row IS the post-mortem trail the brief
    // refers to. The SQL-bypass scenario in #2 below
    // intentionally produces no audit row of its own (no API
    // call ran); its job is to prove the sequential break holds
    // even when the audit trail is missing.

    /// Task 3.5.5 #1 — sequential gate also holds on the
    /// phase-boundary path. With phase 1 fully done and phase 2
    /// holding 5 dep-free tasks, the first
    /// `try_auto_advance` tick spawns only `2.1` (YAML
    /// declaration order); the other four stay `pending`. Each
    /// subsequent completion advances exactly one more task —
    /// five sequential spawns, never five in parallel.
    #[tokio::test]
    async fn phase_boundary_with_5_ready_tasks_spawns_only_one() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "phase-boundary-fanout";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20     - number: \"2.3\"\n\
             \x20       title: Two-Three\n\
             \x20     - number: \"2.4\"\n\
             \x20       title: Two-Four\n\
             \x20     - number: \"2.5\"\n\
             \x20       title: Two-Five\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Phase 1 fully done: only 1.1 in it, mark it
        // completed externally (simulating the user/agent that
        // closed it out).
        set_task_status(&db, plan, "1.1", "completed");

        // Tick 1 — phase boundary. Five dep-free tasks become
        // ready in phase 2; only 2.1 (YAML-first) spawns.
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["2.1".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "phase boundary fan-out: only 2.1 should spawn on tick 1, got {actual:?}",
        );

        // 2.2..2.5 must still be pending — no `task_status`
        // row at all is equivalent to pending in this schema.
        for tid in ["2.2", "2.3", "2.4", "2.5"] {
            let conn = db.lock().unwrap();
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
                    params![plan, tid],
                    |row| row.get(0),
                )
                .ok();
            assert!(
                status.is_none() || status.as_deref() == Some("pending"),
                "{tid} should still be pending after tick 1, got {status:?}",
            );
        }

        let events = drain_events(&mut rx);
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced expected on tick 1: {events:?}",
        );
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must NOT fire on the phase-boundary tick: {events:?}",
        );

        // Drive the remaining four spawns. Each is intra-phase
        // (we're already in phase 2 after tick 1), so every
        // tick must emit `task_advanced`, never
        // `phase_advanced`.
        let chain = [
            ("2.1", "2.2"),
            ("2.2", "2.3"),
            ("2.3", "2.4"),
            ("2.4", "2.5"),
        ];
        let mut prior_total = 1usize;
        for (from, to) in chain {
            set_task_status(&db, plan, from, "completed");
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
                None,
            )
            .await;
            let total = agent_task_ids(&db, plan).len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "exactly one new spawn at {from}->{to} (delta {delta})",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            assert!(
                has_task_advanced(&events, plan),
                "intra-phase {from}->{to}: task_advanced expected, events: {events:?}",
            );
            assert!(
                !has_phase_advanced(&events, plan),
                "intra-phase {from}->{to}: phase_advanced must NOT fire, events: {events:?}",
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.contains(&format!("\"to_tasks\":[\"{to}\"]"))),
                "task_advanced payload must list {to:?}: {events:?}",
            );
        }

        // End state: all 5 phase-2 tasks have been spawned —
        // but across 5 sequential ticks, never 5 in parallel.
        assert_eq!(
            agent_task_ids(&db, plan).len(),
            5,
            "all 5 phase-2 tasks must be spawned across 5 ticks",
        );
    }

    /// Task 3.5.5 #2 — `plan_auto_mode.parallel=1` set
    /// directly via SQL (bypassing the API PUT-time gate from
    /// 3.5.3) does NOT relax the sequential break in
    /// `spawn_ready_tasks`. The 3.5.1 break is unconditional —
    /// the loop does not consult the column at all today, so a
    /// hand-edited DB or a stuck row from a future migration
    /// glitch cannot accidentally fan out parallel agents onto
    /// a single working tree.
    ///
    /// The HTTP-shaped audit trail
    /// (`config.parallel_refused`) lives in
    /// `tests/plan_config.rs`. This test deliberately bypasses
    /// the PUT path so no audit row is written here — the
    /// behavioural property under test is "gate held without
    /// audit too".
    #[tokio::test]
    async fn parallel_true_db_with_worktrees_off_still_sequential() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "parallel-bypass";
        // Three siblings (b, c, d) all depend on a — completing
        // a makes all three ready at the same tick (fan-out
        // shape).
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"a\"]\n",
        );

        // Force `enabled=1, parallel=1` directly. The API PUT
        // path would refuse `parallel=true` with 412
        // (worktrees_required) and audit
        // `config.parallel_refused`; this test simulates a
        // post-rollback / hand-edit scenario where the column
        // is set but the runtime gate must still hold.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled, parallel, paused_reason) \
                 VALUES (?1, 1, 1, NULL) \
                 ON CONFLICT(plan_name) DO UPDATE SET \
                     enabled = excluded.enabled, \
                     parallel = excluded.parallel, \
                     paused_reason = excluded.paused_reason",
                params![plan],
            )
            .unwrap();
        }
        // Sanity: confirm the row really has parallel=1 so a
        // future migration that drops the column can't quietly
        // make this test pass for the wrong reason.
        {
            let conn = db.lock().unwrap();
            let (enabled, parallel): (i64, i64) = conn
                .query_row(
                    "SELECT enabled, parallel FROM plan_auto_mode WHERE plan_name = ?1",
                    params![plan],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(enabled, 1, "auto-mode must be enabled");
            assert_eq!(parallel, 1, "parallel column must be set in DB");
        }

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        // Despite `parallel=1` in the DB, the sequential break
        // holds: exactly one of {b, c, d} spawns, never
        // multiple. The runtime gate ignores the column until
        // worktree isolation ships — that's the whole point of
        // 3.5.3's WORKTREES_SHIPPED const + this regression.
        let actual = agent_task_ids(&db, plan);
        assert_eq!(
            actual.len(),
            1,
            "parallel=1 in DB must NOT relax the sequential gate, got {actual:?}",
        );
        assert!(
            actual.iter().all(|t| ["b", "c", "d"].contains(&t.as_str())),
            "spawned task must be one of the ready siblings, got {actual:?}",
        );

        let events = drain_events(&mut rx);
        let task_advanced = events
            .iter()
            .find(|e| e.contains("\"type\":\"task_advanced\""))
            .expect("task_advanced event present");
        let one_of_singletons = ["[\"b\"]", "[\"c\"]", "[\"d\"]"]
            .iter()
            .any(|s| task_advanced.contains(s));
        assert!(
            one_of_singletons,
            "to_tasks must be a 1-element array even with parallel=1, event was: {task_advanced}",
        );

        // Defensive: no `phase_advanced` here — we're still in
        // phase 1 after tick 1 because b/c/d haven't all
        // completed.
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must not fire when only one task spawned: {events:?}",
        );
    }

    /// Task 3.5.5 #3 — pin that the same sequential gate
    /// covers BOTH intra-phase and cross-phase advances. Drive
    /// a 5-task linear chain that straddles a phase boundary
    /// (phase 1: 1.1→1.2→1.3, phase 2: 2.1→2.2 with
    /// 2.1 depending on 1.3); each `try_auto_advance` call
    /// must result in exactly one new `claim_task` success.
    /// Counter proxy: agents-row delta is 1 per tick (each
    /// successful claim leads to a `start_pty_agent` insert),
    /// so a delta of 1 on every tick is the load-bearing
    /// assertion that 3.5.1's break path was taken on each
    /// path.
    ///
    /// Tick layout:
    ///   - Tick 1 (after 1.1): intra-phase — spawn 1.2 (`task_advanced`)
    ///   - Tick 2 (after 1.2): intra-phase — spawn 1.3 (`task_advanced`)
    ///   - Tick 3 (after 1.3): cross-phase — spawn 2.1 (`phase_advanced`)
    ///   - Tick 4 (after 2.1): intra-phase — spawn 2.2 (`task_advanced`)
    #[tokio::test]
    async fn linear_chain_completes_one_per_completion() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "lin-chain-cross-phase";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: One-Two\n\
             \x20       dependencies: [\"1.1\"]\n\
             \x20     - number: \"1.3\"\n\
             \x20       title: One-Three\n\
             \x20       dependencies: [\"1.2\"]\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20       dependencies: [\"1.3\"]\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20       dependencies: [\"2.1\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "1.1", "completed");

        // (from, to, is_phase_boundary). The "from"
        // completed-task arg is metadata used only to locate
        // the current phase; the actual spawn decision is
        // driven by the `done_set` snapshot.
        let chain = [
            ("1.1", "1.2", false),
            ("1.2", "1.3", false),
            ("1.3", "2.1", true),
            ("2.1", "2.2", false),
        ];
        let mut prior_total = 0usize;
        for (from, to, is_phase_boundary) in chain {
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
                None,
            )
            .await;

            // Counter for `claim_task`: agents-row delta is 1
            // per successful claim (each claim leads to a
            // `start_pty_agent` row insert before the spawn
            // fails on the test_registry's fake server_exe).
            // A delta of 1 on every tick — both intra-phase
            // and cross-phase — proves both paths ride the
            // 3.5.1 break.
            let total = agent_task_ids(&db, plan).len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "claim_task must succeed exactly once at {from}->{to} (delta {delta}, intra+cross share the gate)",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            if is_phase_boundary {
                assert!(
                    has_phase_advanced(&events, plan),
                    "phase boundary {from}->{to}: phase_advanced expected, events: {events:?}",
                );
                assert!(
                    !has_task_advanced(&events, plan),
                    "phase boundary {from}->{to}: task_advanced must NOT fire, events: {events:?}",
                );
            } else {
                assert!(
                    has_task_advanced(&events, plan),
                    "intra-phase {from}->{to}: task_advanced expected, events: {events:?}",
                );
                assert!(
                    !has_phase_advanced(&events, plan),
                    "intra-phase {from}->{to}: phase_advanced must NOT fire, events: {events:?}",
                );
            }

            // Mark the spawned task completed so the next tick
            // sees it as done in `status_map` (the loop's
            // claim flipped it to `in_progress`; a real agent
            // would close it out as `completed`).
            set_task_status(&db, plan, to, "completed");
        }

        // Five-task chain, 1.1 pre-staged + 4 spawned by the
        // loop = 4 successful claims total. Never more.
        assert_eq!(
            agent_task_ids(&db, plan).len(),
            4,
            "linear chain across phase boundary: 4 claims total",
        );
    }

    // ── Task 2.1 — phase-completed broadcast ──────────────────────────
    //
    // Acceptance: 3-task phase, merging tasks 1 and 2 emits no
    // `phase_completed`; merging task 3 emits exactly one. The event
    // payload carries `(plan_name, phase_number, phase_sha)` where
    // `phase_sha` is the merge commit threaded in by the auto-mode
    // pipeline (or `null` for manual completion paths). A second pin
    // proves the event still fires on the LAST phase of a plan, where
    // there is no `phase_advanced` follow-up.

    /// 3-task phase: task 1 + task 2 completion produce zero
    /// `phase_completed` events; the third produces exactly one with
    /// `phase_sha` carrying the merged SHA passed by the auto-mode
    /// caller.
    #[tokio::test]
    async fn phase_completed_fires_once_when_last_task_in_phase_merges() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "three-task-phase";
        // Phase 1 has three independent tasks; phase 2 exists so the
        // next-phase scan still has work to do (proves the
        // `phase_completed` broadcast happens BEFORE the next-phase
        // scan, regardless of whether a follow-up phase exists).
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: One-Two\n\
             \x20     - number: \"1.3\"\n\
             \x20       title: One-Three\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Tick 1: task 1.1 completes — phase still has 1.2 and 1.3
        // open. No `phase_completed` event.
        set_task_status(&db, plan, "1.1", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-1".to_string()),
        )
        .await;
        let events1 = drain_events(&mut rx);
        assert!(
            phase_completed_events(&events1, plan).is_empty(),
            "phase_completed must NOT fire after 1.1: {events1:?}",
        );

        // Tick 2: task 1.2 completes — phase still has 1.3 open. No
        // `phase_completed` event.
        set_task_status(&db, plan, "1.2", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.2".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-2".to_string()),
        )
        .await;
        let events2 = drain_events(&mut rx);
        assert!(
            phase_completed_events(&events2, plan).is_empty(),
            "phase_completed must NOT fire after 1.2: {events2:?}",
        );

        // Tick 3: task 1.3 completes — phase 1 is fully done. Exactly
        // one `phase_completed` event with `phase_number=1` and
        // `phase_sha="sha-3"`.
        set_task_status(&db, plan, "1.3", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.3".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-3".to_string()),
        )
        .await;
        let events3 = drain_events(&mut rx);
        let completed = phase_completed_events(&events3, plan);
        assert_eq!(
            completed.len(),
            1,
            "exactly one phase_completed expected after 1.3: {events3:?}",
        );
        let payload = completed[0];
        assert!(
            payload.contains("\"phase_number\":1"),
            "phase_completed must carry phase_number=1: {payload}",
        );
        assert!(
            payload.contains("\"phase_sha\":\"sha-3\""),
            "phase_completed must carry the merged SHA passed by the caller: {payload}",
        );
        // Also pin that the next-phase scan still ran (phase_advanced
        // fires on the same tick): proves `phase_completed` does not
        // short-circuit the existing `phase_advanced` broadcast.
        assert!(
            has_phase_advanced(&events3, plan),
            "phase_advanced should still fire alongside phase_completed: {events3:?}",
        );
    }

    /// Manual-completion path (no merged SHA threaded through):
    /// `phase_completed` still fires, with `phase_sha` serialised as
    /// JSON `null`. Pins that the SHA is optional, not required.
    #[tokio::test]
    async fn phase_completed_carries_null_sha_for_manual_completion() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "manual-phase-end";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"only\"\n\
             \x20       title: Only\n",
        );
        enable_auto_advance(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "only", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "only".to_string(),
            Effort::Medium,
            registry.port,
            None,
        )
        .await;

        let events = drain_events(&mut rx);
        let completed = phase_completed_events(&events, plan);
        assert_eq!(
            completed.len(),
            1,
            "phase_completed must fire on the last phase too: {events:?}",
        );
        let payload = completed[0];
        // `Option::None` serialises to JSON `null` via serde_json.
        assert!(
            payload.contains("\"phase_sha\":null"),
            "phase_sha must be JSON null when no merged SHA was passed: {payload}",
        );
        // No next phase exists, so no `phase_advanced` follow-up.
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must not fire on the last phase: {events:?}",
        );
    }

    // ── Task 2.2 — phase_verification gate ───────────────────────────
    //
    // The gate added in Phase 2.2 short-circuits the next-phase advance
    // inside `try_auto_advance` when (a) `merged_sha.is_some()` AND (b)
    // `resolve_phase_verification` returns Some for the just-completed
    // phase. The deferred next-phase scan is meant to run later, when
    // the `phase_check` listener calls `advance_after_phase_verify`
    // after the verify Check agent passes.
    //
    // These tests pin the gate behaviour without spawning the actual
    // Check agent (no `claude` binary on the test runner). They use
    // YAML plans that set `phase_verification` directly on the phase so
    // the resolution helper returns Some without needing a
    // `branchwork.toml` on disk.

    /// `phase_verification` set on the just-completed phase + merged_sha
    /// passed in: phase_completed STILL fires, but phase_advanced is
    /// suppressed (deferred to the listener).
    #[tokio::test]
    async fn phase_verification_gate_suppresses_phase_advanced_on_merged_path() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "verify-gated";
        // Phase 1 has phase_verification set, phase 2 exists as a
        // would-be advance target. With the gate active, completing
        // 1.1 with a merged_sha should fire phase_completed but NOT
        // phase_advanced — the listener will handle that later.
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   phase_verification: \"true\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "1.1", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-1".to_string()),
        )
        .await;

        let events = drain_events(&mut rx);
        let completed = phase_completed_events(&events, plan);
        assert_eq!(
            completed.len(),
            1,
            "phase_completed must still fire even when the gate defers next-phase: {events:?}",
        );
        // Gate effect: the next-phase advance is deferred to the
        // phase_check listener. No `phase_advanced` should fire on this
        // tick — the auto-advance flow returned early after broadcasting
        // phase_completed.
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must NOT fire when phase_verification is configured + merged_sha is some: {events:?}",
        );
    }

    /// `phase_verification` set on the just-completed phase + manual
    /// completion path (merged_sha = None): the gate is bypassed and the
    /// existing next-phase advance still fires. This preserves
    /// pre-2.2 manual-completion semantics — the gate only kicks in
    /// when there's a real merge SHA to verify against.
    #[tokio::test]
    async fn phase_verification_gate_bypassed_when_no_merged_sha() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "verify-manual";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   phase_verification: \"true\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "1.1", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            None, // ← manual completion, gate must be bypassed
        )
        .await;

        let events = drain_events(&mut rx);
        assert_eq!(
            phase_completed_events(&events, plan).len(),
            1,
            "phase_completed must fire: {events:?}",
        );
        // Manual path: gate is bypassed, phase_advanced still fires
        // (matches pre-2.2 behaviour for the manual flow).
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced MUST fire on the manual completion path even with verification configured: {events:?}",
        );
    }

    /// `advance_after_phase_verify` is the re-entry point the
    /// phase_check listener calls after a passing verify. It must NOT
    /// re-broadcast `phase_completed` (that already fired before the
    /// gate deferred us) but it MUST run the same next-phase
    /// scan-and-spawn block. The test uses a verification-gated plan,
    /// drives `try_auto_advance` to set the gate, drains the events,
    /// then directly invokes `advance_after_phase_verify` to simulate
    /// the listener's success path. Expected: phase_advanced fires
    /// once, phase_completed does NOT fire again.
    #[tokio::test]
    async fn advance_after_phase_verify_runs_next_phase_scan_without_rebroadcast() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "verify-resume";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   phase_verification: \"true\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Step 1: drive the gate. phase_completed fires, phase_advanced
        // does not. (Re-using the gate-suppression test's setup; we
        // assert here too so a regression in the gate doesn't
        // silently make the listener test pass for the wrong reason.)
        set_task_status(&db, plan, "1.1", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-1".to_string()),
        )
        .await;

        let events_gate = drain_events(&mut rx);
        assert_eq!(
            phase_completed_events(&events_gate, plan).len(),
            1,
            "phase_completed must fire on the gate tick: {events_gate:?}",
        );
        assert!(
            !has_phase_advanced(&events_gate, plan),
            "gate must defer phase_advanced: {events_gate:?}",
        );

        // Step 2: the listener simulates a passing verify by calling
        // advance_after_phase_verify. This must spawn 2.1 and fire
        // phase_advanced, but NOT re-fire phase_completed.
        advance_after_phase_verify(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            1,
            Effort::Medium,
            registry.port,
        )
        .await;

        let events_resume = drain_events(&mut rx);
        assert!(
            phase_completed_events(&events_resume, plan).is_empty(),
            "phase_completed must NOT re-fire on the resume path: {events_resume:?}",
        );
        assert!(
            has_phase_advanced(&events_resume, plan),
            "phase_advanced MUST fire on the resume path: {events_resume:?}",
        );
        assert!(
            agent_task_ids(&db, plan).contains("2.1"),
            "task 2.1 must be claimed/spawned by the resume path: {:?}",
            agent_task_ids(&db, plan),
        );
    }

    /// No `phase_verification` configured anywhere + merged_sha passed:
    /// the gate doesn't engage, phase_advanced fires alongside
    /// phase_completed (re-asserting the Task 2.1 behaviour the existing
    /// test pinned).
    #[tokio::test]
    async fn phase_verification_gate_does_not_engage_when_unconfigured() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "verify-none";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "1.1", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
            Some("sha-x".to_string()),
        )
        .await;

        let events = drain_events(&mut rx);
        assert_eq!(
            phase_completed_events(&events, plan).len(),
            1,
            "phase_completed must fire: {events:?}",
        );
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced MUST fire when no verification is configured: {events:?}",
        );
    }

    // ──────────────────────────────────────────────────────────────────
    // check_tree_clean_for_completion: per-project ignore allowlist
    // ──────────────────────────────────────────────────────────────────

    /// Init a git repo at `cwd`, commit `tracked_files` (each path is
    /// pre-populated with `"init\n"` and then committed). Returns the
    /// repo path. Mirrors `hooks::tests::git_init_with_clean_tree` but
    /// lets the caller seed nested tracked files for porcelain testing.
    fn git_init_with_tracked_files(cwd: &std::path::Path, tracked_files: &[&str]) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(cwd).unwrap();
        run(&["init", "-q", "-b", "master"]);
        run(&["config", "user.email", "t@t.test"]);
        run(&["config", "user.name", "Test"]);
        for path in tracked_files {
            let full = cwd.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, "init\n").unwrap();
            run(&["add", path]);
        }
        run(&["commit", "-q", "-m", "initial"]);
    }

    fn map_plan_to_project_path(db: &Db, plan: &str, project: &std::path::Path) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_project (plan_name, project) VALUES (?1, ?2) \
             ON CONFLICT(plan_name) DO UPDATE SET project = excluded.project",
            params![plan, project.to_string_lossy()],
        )
        .unwrap();
    }

    /// Acceptance criterion: `[auto_mode.dirty_tree] ignore = ["*.log"]`
    /// in `branchwork.toml` plus a dirty `server.log` returns `Clean`.
    #[test]
    fn dirty_log_alone_is_clean_when_ignored_by_branchwork_toml() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["server.log"]);
        // Make server.log dirty by writing modified content.
        std::fs::write(cwd.join("server.log"), "modified noise\n").unwrap();
        std::fs::write(
            cwd.join("branchwork.toml"),
            "[auto_mode.dirty_tree]\nignore = [\"*.log\"]\n",
        )
        .unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p1", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p1") {
            TreeState::Clean => {}
            other => panic!("expected Clean (server.log filtered), got {other:?}"),
        }
    }

    /// Acceptance criterion: a dirty agent-code path (`server-rs/src/foo.rs`)
    /// still returns `Dirty` even with the `*.log` allowlist active.
    #[test]
    fn dirty_agent_code_file_remains_dirty_with_log_ignore() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["server-rs/src/foo.rs"]);
        std::fs::write(
            cwd.join("server-rs/src/foo.rs"),
            "fn changed_but_not_committed() {}\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join("branchwork.toml"),
            "[auto_mode.dirty_tree]\nignore = [\"*.log\"]\n",
        )
        .unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p2", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p2") {
            TreeState::Dirty { files } => {
                assert!(
                    files.iter().any(|f| f.ends_with("foo.rs")),
                    "expected foo.rs in dirty files, got {files:?}"
                );
            }
            other => panic!("expected Dirty (foo.rs not ignored), got {other:?}"),
        }
    }

    /// Mixed dirty set: one operational + one code file, with only the
    /// operational glob in the allowlist. The code file alone is what
    /// flips the verdict to Dirty; `files` must contain ONLY the code
    /// file (not the filtered log), so dashboards don't show noise.
    #[test]
    fn mixed_dirty_set_filters_log_keeps_code_file() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["server.log", "server-rs/src/foo.rs"]);
        std::fs::write(cwd.join("server.log"), "modified\n").unwrap();
        std::fs::write(cwd.join("server-rs/src/foo.rs"), "modified\n").unwrap();
        std::fs::write(
            cwd.join("branchwork.toml"),
            "[auto_mode.dirty_tree]\nignore = [\"*.log\"]\n",
        )
        .unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p3", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p3") {
            TreeState::Dirty { files } => {
                assert!(
                    files.iter().any(|f| f.ends_with("foo.rs")),
                    "expected foo.rs in dirty files, got {files:?}"
                );
                assert!(
                    files.iter().all(|f| !f.ends_with("server.log")),
                    "server.log must be filtered out of dirty list, got {files:?}"
                );
            }
            other => panic!("expected Dirty (mixed set), got {other:?}"),
        }
    }

    /// `.bob/**` and `.mcp.json` patterns from the task description.
    #[test]
    fn ignores_bob_directory_and_mcp_json() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &[".bob/notes.txt", ".mcp.json"]);
        std::fs::write(cwd.join(".bob/notes.txt"), "scratch\n").unwrap();
        std::fs::write(cwd.join(".mcp.json"), "{}\n").unwrap();
        std::fs::write(
            cwd.join("branchwork.toml"),
            "[auto_mode.dirty_tree]\nignore = [\".bob/**\", \".mcp.json\"]\n",
        )
        .unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p4", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p4") {
            TreeState::Clean => {}
            other => panic!(
                "expected Clean (both files filtered by .bob/** and .mcp.json), got {other:?}"
            ),
        }
    }

    /// No `branchwork.toml` at all: behaviour unchanged from pre-task —
    /// any dirty tracked path produces `Dirty`.
    #[test]
    fn absent_branchwork_toml_falls_back_to_unfiltered_behaviour() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["server.log"]);
        std::fs::write(cwd.join("server.log"), "modified\n").unwrap();
        // no branchwork.toml

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p5", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p5") {
            TreeState::Dirty { files } => {
                assert!(
                    files.iter().any(|f| f.ends_with("server.log")),
                    "expected server.log in dirty files (no allowlist), got {files:?}"
                );
            }
            other => panic!("expected Dirty without branchwork.toml, got {other:?}"),
        }
    }

    /// `branchwork.toml` exists but is missing the `[auto_mode.dirty_tree]`
    /// section — fall back to "no filter" without panicking.
    #[test]
    fn branchwork_toml_without_dirty_tree_section_falls_back_to_unfiltered() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["server.log"]);
        std::fs::write(cwd.join("server.log"), "modified\n").unwrap();
        std::fs::write(
            cwd.join("branchwork.toml"),
            "[ci]\nblocking_workflows = [\"CI\"]\n",
        )
        .unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p6", &cwd);

        match check_tree_clean_for_completion(&db, &plans_dir, "p6") {
            TreeState::Dirty { files } => {
                assert!(files.iter().any(|f| f.ends_with("server.log")));
            }
            other => panic!("expected Dirty (no dirty_tree section), got {other:?}"),
        }
    }

    /// `git status --porcelain` already drops files flagged with
    /// `--skip-worktree` (same goes for `--assume-unchanged`), so the
    /// dirty-tree check inherits that masking for free. This test pins
    /// the property: a future refactor (e.g. switching to a libgit2
    /// call that walks the index differently) must keep dashboards
    /// quiet for operators who deliberately took a tracked file out of
    /// the working set (e.g. a local-only config override).
    ///
    /// Setup: track `local-config.json`, modify it on disk, observe
    /// `Dirty`, flag `--skip-worktree`, observe `Clean`, flip to
    /// `--no-skip-worktree`, observe `Dirty` again — all without
    /// committing or reverting the on-disk change.
    #[test]
    fn skip_worktree_flagged_file_does_not_trip_dirty_check() {
        crate::repo_config::clear_cache_for_tests();
        let (db, _holder) = fresh_db();
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("project");
        git_init_with_tracked_files(&cwd, &["local-config.json"]);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        map_plan_to_project_path(&db, "p_skip", &cwd);

        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        // Baseline: modify the tracked file on disk; with no flag set
        // porcelain emits a ` M local-config.json` line, so we land on
        // Dirty. Establishes that the test fixture is wired correctly
        // before we exercise the skip-worktree behaviour.
        std::fs::write(cwd.join("local-config.json"), "modified\n").unwrap();
        match check_tree_clean_for_completion(&db, &plans_dir, "p_skip") {
            TreeState::Dirty { files } => {
                assert!(
                    files.iter().any(|f| f.ends_with("local-config.json")),
                    "baseline: expected local-config.json in dirty files, got {files:?}"
                );
            }
            other => panic!("baseline: expected Dirty, got {other:?}"),
        }

        // Flag the file with --skip-worktree. The on-disk modification
        // is unchanged; only git's index entry gets the bit set. Now
        // porcelain omits the file and the check should flip to Clean.
        git(&["update-index", "--skip-worktree", "local-config.json"]);
        match check_tree_clean_for_completion(&db, &plans_dir, "p_skip") {
            TreeState::Clean => {}
            other => panic!("with --skip-worktree, expected Clean, got {other:?}"),
        }

        // Flip the flag off. The file is still modified on disk (we
        // never touched it on the filesystem during the flag toggles),
        // so porcelain emits it again and the verdict returns to Dirty.
        git(&["update-index", "--no-skip-worktree", "local-config.json"]);
        match check_tree_clean_for_completion(&db, &plans_dir, "p_skip") {
            TreeState::Dirty { files } => {
                assert!(
                    files.iter().any(|f| f.ends_with("local-config.json")),
                    "after --no-skip-worktree, expected local-config.json in dirty files, got {files:?}"
                );
            }
            other => panic!("after --no-skip-worktree, expected Dirty, got {other:?}"),
        }
    }
}
