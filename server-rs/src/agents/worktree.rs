//! Git worktree management for Branchwork agents.
//!
//! Two distinct consumers live here:
//!
//! 1. **Per-agent worktree manager** — [`setup_worktree`] /
//!    [`remove_worktree`] / [`list_orphans`], the durable isolation
//!    primitive from ADR 0002. Each agent gets its own *named-branch*
//!    worktree under `$BRANCHWORK_WORKTREE_BASE` (default
//!    `~/.branchwork/worktrees`) so concurrent agents on different task
//!    branches never share a `HEAD`. Call sites are wired in Phase 2 of
//!    the worktree-per-agent-isolation plan; the functions exist (and are
//!    tested) now, carrying `#[allow(dead_code)]` until then.
//!
//! 2. **Pre-merge gate temp worktree** — [`TempWorktree`], a short-lived
//!    *detached* checkout at a branch tip used by the pre-merge gate so
//!    its check commands don't fight the agent's own working copy. RAII:
//!    the worktree is removed on `Drop`. The lower-level
//!    [`add_worktree_at`] / [`remove_worktree_at`] pair exists for tests
//!    that need to pin a custom location.
//!
//! All shell-outs use `std::process::Command` to keep parity with
//! [`crate::agents::phase_check`] (the other worktree consumer); no async
//! machinery is required — the operations are short and callers already
//! run inside a tokio task.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Owned handle to a freshly-created `git worktree`. Drops the worktree
/// via `git worktree remove --force` (with a `std::fs::remove_dir_all`
/// fallback) when this struct goes out of scope, so the cleanup contract
/// is intrinsic — callers can't accidentally leak temp state by
/// returning early.
///
/// The brief calls for `git worktree add /tmp/bw-gate-<agent_id> <branch>`
/// → run checks → `git worktree remove --force /tmp/bw-gate-<agent_id>`
/// in a defer/drop guard. This struct is that guard.
///
/// `#[allow(dead_code)]`: used by the pre-merge gate in the server binary, but
/// this file is also `#[path]`-included by `branchwork-runner` (which only
/// needs the per-agent manager below), where `TempWorktree` is dead.
#[derive(Debug)]
#[allow(dead_code)]
pub struct TempWorktree {
    /// Project root the worktree was added FROM. We need this for the
    /// `git worktree remove` cleanup, which has to run in the original
    /// repo's `.git` context.
    project_dir: PathBuf,
    /// On-disk path of the worktree itself (e.g. `/tmp/bw-gate-<id>`).
    /// `Option` so [`Drop`] can take ownership and avoid double-cleanup.
    worktree_path: Option<PathBuf>,
}

#[allow(dead_code)] // server-only (pre-merge gate); dead in the runner #[path] include
impl TempWorktree {
    /// Create a fresh `git worktree` at `/tmp/bw-gate-<agent_id>` checked
    /// out at `branch`'s tip in detached-HEAD mode. Returns the wrapper
    /// on success; on failure the directory is not left behind.
    ///
    /// Detached HEAD avoids contention with the agent's still-checked-
    /// out copy of the same branch — `git worktree add` refuses to share
    /// a branch with another worktree by default.
    pub fn create(project_dir: &Path, agent_id: &str, branch: &str) -> Result<Self, String> {
        let path = PathBuf::from(format!("/tmp/bw-gate-{agent_id}"));
        add_worktree_at(project_dir, &path, branch)?;
        Ok(Self {
            project_dir: project_dir.to_path_buf(),
            worktree_path: Some(path),
        })
    }

    /// Path of the worktree on disk. Used by the gate runner to set the
    /// `cwd` of each check command.
    pub fn path(&self) -> &Path {
        self.worktree_path
            .as_deref()
            .expect("worktree path set until Drop")
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        if let Some(path) = self.worktree_path.take() {
            remove_worktree_at(&self.project_dir, &path);
        }
    }
}

/// Run `git worktree add --detach <path> <branch>` from `project_dir`.
///
/// `git worktree add` requires the destination path NOT to exist; the
/// caller is expected to use a fresh `/tmp/bw-gate-<agent_id>`-shaped
/// path so a previous gate's leak (if cleanup somehow failed) doesn't
/// collide.
#[allow(dead_code)] // server-only (TempWorktree); dead in the runner #[path] include
fn add_worktree_at(project_dir: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| "non-utf8 worktree path".to_string())?;
    let out = Command::new("git")
        .args(["worktree", "add", "--detach", path_str, branch])
        .current_dir(project_dir)
        .output()
        .map_err(|e| format!("git worktree add spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Best-effort cleanup: `git worktree remove --force` first; fall back
/// to `remove_dir_all` so we never leak the directory on disk. Both
/// failures are logged but don't propagate — the gate runner's verdict
/// has already shipped by the time Drop runs and any reader has moved
/// on.
#[allow(dead_code)] // server-only (TempWorktree Drop); dead in the runner #[path] include
fn remove_worktree_at(project_dir: &Path, path: &Path) {
    let path_str = match path.to_str() {
        Some(s) => s,
        None => {
            eprintln!(
                "[worktree] non-utf8 path during cleanup: {}",
                path.display()
            );
            return;
        }
    };
    let out = Command::new("git")
        .args(["worktree", "remove", "--force", path_str])
        .current_dir(project_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => return,
        Ok(o) => {
            eprintln!(
                "[worktree] git worktree remove failed ({}): {}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[worktree] git worktree remove spawn failed: {e}");
        }
    }
    if path.exists()
        && let Err(e) = std::fs::remove_dir_all(path)
    {
        eprintln!(
            "[worktree] fallback remove_dir_all failed for {}: {e}",
            path.display()
        );
    }
}

// ── Per-agent worktree manager ───────────────────────────────────────
//
// The API below is the durable worktree primitive for the
// worktree-per-agent-isolation plan: `setup_worktree` on agent spawn,
// `remove_worktree` on completion / discard, and `list_orphans` for the
// startup sweep that reaps worktrees no live agent claims (ADR 0002 §
// "Worktree leak on crash").
//
// These create the agent's *named-branch* worktree under
// `$BRANCHWORK_WORKTREE_BASE`, distinct from `TempWorktree` above (the
// pre-merge gate's detached, throwaway checkout). Phase 2 wires the call
// sites; until then everything here carries `#[allow(dead_code)]`.

/// Error returned by the per-agent worktree manager.
#[derive(Debug)]
#[allow(dead_code)] // call sites land in Phase 2
pub enum WorktreeError {
    /// Could not create the parent directory of the worktree path.
    ParentDirCreate(io::Error),
    /// A `git` invocation exited non-zero (or failed to spawn). Also used
    /// for the `rm -rf` fallback failure in [`remove_worktree`].
    GitCommandFailed { cmd: String, stderr: String },
    /// The resolved worktree base path was unusable — non-UTF-8, or the
    /// home directory could not be determined for the default base.
    BasePathInvalid(String),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeError::ParentDirCreate(e) => {
                write!(f, "failed to create worktree parent directory: {e}")
            }
            WorktreeError::GitCommandFailed { cmd, stderr } => {
                write!(f, "`{cmd}` failed: {stderr}")
            }
            WorktreeError::BasePathInvalid(msg) => {
                write!(f, "worktree base path invalid: {msg}")
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

/// Outcome of a best-effort [`remove_worktree`] call, in increasing order
/// of how much force it took to clean up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // call sites land in Phase 2
pub enum RemoveOutcome {
    /// `git worktree remove <path>` succeeded.
    Removed,
    /// `git worktree remove --force <path>` succeeded.
    ForceRemoved,
    /// Fell back to `remove_dir_all` + `git worktree prune`.
    ManuallyRemoved,
}

/// A worktree found under `<base>/<project-slug>/` that no live agent
/// currently claims — a candidate for the startup orphan sweep.
#[derive(Debug, Clone)]
#[allow(dead_code)] // call sites land in Phase 2
pub struct OrphanWorktree {
    /// Absolute on-disk path of the orphaned worktree.
    pub path: PathBuf,
    /// Branch the worktree was checked out on, if any (`None` for a
    /// detached HEAD).
    pub branch: Option<String>,
    /// Filesystem mtime of the worktree directory, if it could be read.
    pub last_modified: Option<SystemTime>,
}

/// Create (or, on continue, re-attach) the per-agent git worktree and
/// return its absolute on-disk path.
///
/// The path is
/// `<base>/<project-slug>/<plan>/<task_number>-<agent-id-short>/`, where
/// `base` is `$BRANCHWORK_WORKTREE_BASE` (default `~/.branchwork/worktrees`)
/// and `agent-id-short` is the first 8 chars of `agent_id`. The parent
/// directory is created if missing.
///
/// Branch handling:
/// - `is_continue` + branch already exists → `git worktree add <path> <branch>`
/// - `is_continue` + branch missing → create it from `base_branch` (or `HEAD`)
/// - `!is_continue` → `git worktree add <path> -b <branch> <base_branch|HEAD>`
#[allow(dead_code, clippy::too_many_arguments)] // call sites land in Phase 2
pub fn setup_worktree(
    project_dir: &Path,
    project_slug: &str,
    plan: &str,
    task_number: &str,
    agent_id: &str,
    branch: &str,
    base_branch: Option<&str>,
    is_continue: bool,
) -> Result<PathBuf, WorktreeError> {
    let base = worktree_base()?;
    setup_worktree_in(
        &base,
        project_dir,
        project_slug,
        plan,
        task_number,
        agent_id,
        branch,
        base_branch,
        is_continue,
    )
}

/// Inner worktree setup against an explicit `base` directory. The public
/// [`setup_worktree`] resolves `base` from the environment first; tests
/// call this directly to pin a tempdir without mutating process env.
///
/// `pub(crate)` so the `tests/worktree_isolation.rs` integration test
/// (which `#[path]`-includes this module) can pin a tempdir base the same
/// way the unit tests below do — never via `BRANCHWORK_WORKTREE_BASE`,
/// which would be a process-wide env data race in a parallel test binary.
#[allow(dead_code, clippy::too_many_arguments)] // call sites land in Phase 2
pub(crate) fn setup_worktree_in(
    base: &Path,
    project_dir: &Path,
    project_slug: &str,
    plan: &str,
    task_number: &str,
    agent_id: &str,
    branch: &str,
    base_branch: Option<&str>,
    is_continue: bool,
) -> Result<PathBuf, WorktreeError> {
    let agent_id_short = &agent_id[..8.min(agent_id.len())];
    let path = base
        .join(project_slug)
        .join(plan)
        .join(format!("{task_number}-{agent_id_short}"));

    // `git worktree add` will not create missing intermediate dirs, so
    // ensure the parent exists first.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(WorktreeError::ParentDirCreate)?;
    }

    // Same-filesystem advisory (ADR 0002 failure mode #3). Best-effort,
    // once per process; never fails the spawn.
    warn_if_cross_filesystem(base, project_dir);

    let path_str = path.to_str().ok_or_else(|| {
        WorktreeError::BasePathInvalid(format!("non-utf8 worktree path: {}", path.display()))
    })?;

    if is_continue && branch_exists(project_dir, branch) {
        // Re-attach a worktree to the agent's existing branch.
        git_checked(project_dir, &["worktree", "add", path_str, branch])?;
    } else {
        // Fresh start, or a continue whose branch was never created:
        // branch off `base_branch` (or HEAD).
        let base_ref = base_branch.unwrap_or("HEAD");
        git_checked(
            project_dir,
            &["worktree", "add", path_str, "-b", branch, base_ref],
        )?;
    }

    Ok(path)
}

/// Best-effort removal of a worktree, escalating force as needed:
/// 1. `git worktree remove <path>` → [`RemoveOutcome::Removed`]
/// 2. `git worktree remove --force <path>` → [`RemoveOutcome::ForceRemoved`]
/// 3. `remove_dir_all` + `git worktree prune` → [`RemoveOutcome::ManuallyRemoved`]
///
/// Returns `Err` only if even the `remove_dir_all` fallback fails. Always
/// logs `[worktree] removed <path> outcome=<variant>` on success.
#[allow(dead_code)] // call sites land in Phase 2
pub fn remove_worktree(project_dir: &Path, path: &Path) -> Result<RemoveOutcome, WorktreeError> {
    let path_str = path.to_str().ok_or_else(|| {
        WorktreeError::BasePathInvalid(format!("non-utf8 worktree path: {}", path.display()))
    })?;

    if git_succeeds(project_dir, &["worktree", "remove", path_str]) {
        log_removal(path, RemoveOutcome::Removed);
        return Ok(RemoveOutcome::Removed);
    }

    if git_succeeds(project_dir, &["worktree", "remove", "--force", path_str]) {
        log_removal(path, RemoveOutcome::ForceRemoved);
        return Ok(RemoveOutcome::ForceRemoved);
    }

    // Manual fallback: rm -rf the directory, then prune the now-dangling
    // worktree reference from `.git/worktrees/`.
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| WorktreeError::GitCommandFailed {
            cmd: format!("rm -rf {}", path.display()),
            stderr: e.to_string(),
        })?;
    }
    let _ = git_succeeds(project_dir, &["worktree", "prune"]);
    log_removal(path, RemoveOutcome::ManuallyRemoved);
    Ok(RemoveOutcome::ManuallyRemoved)
}

/// List worktrees under `<base>/<project-slug>/` that are NOT in
/// `live_cwds` — the orphans a startup sweep should reap. Resolves the
/// base from the environment; returns an empty vec when it can't (or when
/// `git worktree list` fails).
#[allow(dead_code)] // call sites land in Phase 2
pub fn list_orphans(
    project_dir: &Path,
    project_slug: &str,
    live_cwds: &HashSet<PathBuf>,
) -> Vec<OrphanWorktree> {
    let base = match worktree_base() {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    list_orphans_in(&base, project_dir, project_slug, live_cwds)
}

/// Inner orphan listing against an explicit `base`. Tests call this to
/// pin a tempdir; the public [`list_orphans`] resolves `base` from env. The
/// runner binary calls it with its startup-resolved `state.worktree_base` so
/// the listing base exactly matches the sandbox base.
///
/// `pub(crate)` so the runner binary (which `#[path]`-includes this module)
/// can reach it the same way it reaches [`setup_worktree_in`].
#[allow(dead_code)] // call sites land in Phase 2
pub(crate) fn list_orphans_in(
    base: &Path,
    project_dir: &Path,
    project_slug: &str,
    live_cwds: &HashSet<PathBuf>,
) -> Vec<OrphanWorktree> {
    let scope = base.join(project_slug);

    let out = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_dir)
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out);

    // Normalise both sides of the membership test so a symlinked base or
    // tempdir (e.g. macOS `/tmp` → `/private/tmp`) doesn't read as orphaned.
    let live: HashSet<PathBuf> = live_cwds.iter().map(|p| canonical_or_self(p)).collect();
    let scope_canon = canonical_or_self(&scope);

    let mut orphans = Vec::new();
    for entry in parse_worktree_porcelain(&text) {
        let entry_canon = canonical_or_self(&entry.path);
        if !entry_canon.starts_with(&scope_canon) {
            continue;
        }
        if live.contains(&entry_canon) {
            continue;
        }
        let last_modified = std::fs::metadata(&entry.path)
            .and_then(|m| m.modified())
            .ok();
        orphans.push(OrphanWorktree {
            path: entry.path,
            branch: entry.branch,
            last_modified,
        });
    }
    orphans
}

// ── helpers ──────────────────────────────────────────────────────────

/// Resolve the worktree base directory: `$BRANCHWORK_WORKTREE_BASE` if set
/// and non-empty, else `~/.branchwork/worktrees`. Always returns an
/// absolute path so callers get a stable, cwd-independent location.
///
/// `pub` so the runner binary (which `#[path]`-includes this module) can
/// resolve the same base at startup and widen its `validated_cwd` sandbox
/// to accept per-agent worktree paths in addition to `--cwd` (Phase 2.7 /
/// ADR 0002 §SaaS: the runner owns the filesystem).
#[allow(dead_code)] // call sites land in Phase 2
pub fn worktree_base() -> Result<PathBuf, WorktreeError> {
    let base = match std::env::var_os("BRANCHWORK_WORKTREE_BASE") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .ok_or_else(|| {
                WorktreeError::BasePathInvalid("could not determine home directory".to_string())
            })?
            .join(".branchwork")
            .join("worktrees"),
    };
    if base.is_absolute() {
        Ok(base)
    } else {
        // A relative override is resolved against the process cwd so the
        // returned path is absolute and `git worktree add` lands
        // predictably regardless of the git invocation's `current_dir`.
        std::env::current_dir()
            .map(|cwd| cwd.join(&base))
            .map_err(|e| WorktreeError::BasePathInvalid(format!("cwd unavailable: {e}")))
    }
}

/// Does a *local* branch named `branch` exist in `project_dir`?
#[allow(dead_code)] // call sites land in Phase 2
fn branch_exists(project_dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(project_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a `git` subcommand in `project_dir`, mapping a non-zero exit (or a
/// spawn failure) to [`WorktreeError::GitCommandFailed`].
#[allow(dead_code)] // call sites land in Phase 2
fn git_checked(project_dir: &Path, args: &[&str]) -> Result<(), WorktreeError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(project_dir)
        .output()
        .map_err(|e| WorktreeError::GitCommandFailed {
            cmd: format!("git {}", args.join(" ")),
            stderr: format!("spawn failed: {e}"),
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::GitCommandFailed {
            cmd: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Run a `git` subcommand in `project_dir`; return whether it exited 0.
/// Used by the best-effort [`remove_worktree`] escalation.
#[allow(dead_code)] // call sites land in Phase 2
fn git_succeeds(project_dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(project_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[allow(dead_code)] // call sites land in Phase 2
fn log_removal(path: &Path, outcome: RemoveOutcome) {
    eprintln!("[worktree] removed {} outcome={outcome:?}", path.display());
}

/// One block from `git worktree list --porcelain`.
#[allow(dead_code)] // call sites land in Phase 2
struct PorcelainEntry {
    path: PathBuf,
    branch: Option<String>,
}

/// Parse `git worktree list --porcelain` output. Blocks are separated by
/// a blank line; each opens with `worktree <abs-path>` and may carry a
/// `branch refs/heads/<name>` line (absent for detached / bare entries).
#[allow(dead_code)] // call sites land in Phase 2
fn parse_worktree_porcelain(text: &str) -> Vec<PorcelainEntry> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    for line in text.lines() {
        if line.is_empty() {
            if let Some(p) = path.take() {
                entries.push(PorcelainEntry {
                    path: p,
                    branch: branch.take(),
                });
            }
            branch = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        }
        // `HEAD`, `detached`, `bare`, `locked`, `prunable` lines are ignored.
    }
    // Flush the final block when the output has no trailing blank line.
    if let Some(p) = path.take() {
        entries.push(PorcelainEntry {
            path: p,
            branch: branch.take(),
        });
    }
    entries
}

/// Canonicalize `path`, falling back to the path itself when it can't be
/// resolved (e.g. it no longer exists). Keeps `live_cwds` membership
/// robust against symlinked tempdirs.
#[allow(dead_code)] // call sites land in Phase 2
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Walk up from `path` to the nearest ancestor that exists on disk.
#[allow(dead_code)] // call sites land in Phase 2
fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur.exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Warn (once per process) when the worktree base lives on a different
/// filesystem than the project's `.git` — git worktree ops still work but
/// run slower without hardlinked object refs (ADR 0002 failure mode #3).
#[cfg(unix)]
#[allow(dead_code)] // call sites land in Phase 2
fn warn_if_cross_filesystem(base: &Path, project_dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    use std::sync::Once;
    static WARNED: Once = Once::new();

    let git_dir = project_dir.join(".git");
    let git_probe = if git_dir.exists() {
        git_dir
    } else {
        project_dir.to_path_buf()
    };

    let base_dev = nearest_existing_ancestor(base)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.dev());
    let git_dev = std::fs::metadata(&git_probe).ok().map(|m| m.dev());

    if let (Some(b), Some(g)) = (base_dev, git_dev)
        && b != g
    {
        WARNED.call_once(|| {
            eprintln!(
                "[worktree] base {} is on a different filesystem than {} — git \
                 worktree operations will be slower (no hardlinked object refs). \
                 Prefer a base on the same filesystem. See ADR 0002 failure mode #3.",
                base.display(),
                git_probe.display()
            );
        });
    }
}

#[cfg(not(unix))]
#[allow(dead_code)] // call sites land in Phase 2
fn warn_if_cross_filesystem(_base: &Path, _project_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::Command;
    use tempfile::TempDir;

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

    fn git_init_master(cwd: &Path) {
        run_git(cwd, &["init", "-q", "-b", "master"]);
        run_git(cwd, &["config", "user.email", "t@t.test"]);
        run_git(cwd, &["config", "user.name", "Test"]);
        std::fs::write(cwd.join("README.md"), "init").unwrap();
        run_git(cwd, &["add", "README.md"]);
        run_git(cwd, &["commit", "-q", "-m", "initial"]);
    }

    fn git_create_branch_with_commit(cwd: &Path, branch: &str) {
        run_git(cwd, &["checkout", "-q", "-b", branch]);
        std::fs::write(cwd.join("work.txt"), "work").unwrap();
        run_git(cwd, &["add", "work.txt"]);
        run_git(cwd, &["commit", "-q", "-m", "task work"]);
        run_git(cwd, &["checkout", "-q", "master"]);
    }

    #[test]
    fn create_then_drop_round_trips() {
        let dir = TempDir::new().unwrap();
        let project = dir.path();
        git_init_master(project);
        git_create_branch_with_commit(project, "feature/x");

        let path = {
            let wt =
                TempWorktree::create(project, "test-1", "feature/x").expect("worktree should add");
            let p = wt.path().to_path_buf();
            assert!(p.exists(), "worktree path should exist after create");
            assert!(
                p.join("work.txt").exists(),
                "worktree should carry feature/x content"
            );
            p
            // Drop fires here.
        };
        assert!(!path.exists(), "worktree path should be gone after Drop");
    }

    #[test]
    fn create_fails_when_branch_missing() {
        let dir = TempDir::new().unwrap();
        let project = dir.path();
        git_init_master(project);

        let err = TempWorktree::create(project, "test-2", "branch-that-does-not-exist")
            .expect_err("worktree should fail for unknown branch");
        assert!(err.contains("git worktree add"), "err={err}");
    }

    #[test]
    fn drop_is_idempotent_on_already_removed_path() {
        let dir = TempDir::new().unwrap();
        let project = dir.path();
        git_init_master(project);
        git_create_branch_with_commit(project, "feature/y");

        let wt = TempWorktree::create(project, "test-3", "feature/y").unwrap();
        let path = wt.path().to_path_buf();
        // Force-remove the directory before Drop runs.
        std::fs::remove_dir_all(&path).ok();
        // Drop should not panic; the inner `git worktree remove` will
        // log a non-fatal warning but the fallback `remove_dir_all` is
        // also a no-op once the directory is gone.
        drop(wt);
        assert!(!path.exists());
    }

    // ── Per-agent worktree manager ───────────────────────────────────

    #[test]
    fn setup_worktree_fresh_start_creates_branch_worktree() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "my-project",
            "my-plan",
            "0.1",
            "abcdef1234567890",
            "branchwork/my-plan/0.1",
            None,
            false,
        )
        .expect("fresh worktree should be created");

        // Path shape: <base>/<slug>/<plan>/<task>-<id8>.
        assert_eq!(
            path,
            base.path()
                .join("my-project")
                .join("my-plan")
                .join("0.1-abcdef12")
        );
        assert!(path.exists(), "worktree dir should exist");
        assert!(
            path.join("README.md").exists(),
            "worktree carries master content"
        );
        assert!(
            branch_exists(proj.path(), "branchwork/my-plan/0.1"),
            "branch should have been created"
        );
    }

    #[test]
    fn setup_worktree_yields_clean_working_tree() {
        // Case 1: a fresh worktree is a *functional* git working tree, not
        // just a directory — `git status` inside it reports clean.
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "cafef00d99",
            "branchwork/p/0.1",
            None,
            false,
        )
        .expect("fresh worktree should be created");

        assert!(path.exists(), "worktree directory should exist");
        assert_eq!(
            git_status_porcelain(&path),
            "",
            "git status inside the fresh worktree should be clean"
        );
    }

    #[test]
    fn setup_worktree_truncates_short_agent_id_safely() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        // `abc` is shorter than 8 chars — `8.min(len)` must not panic.
        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "abc",
            "branchwork/p/0.1",
            None,
            false,
        )
        .expect("short agent id should not panic");
        assert!(path.ends_with("0.1-abc"));
    }

    #[test]
    fn setup_worktree_continue_attaches_existing_branch() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());
        git_create_branch_with_commit(proj.path(), "branchwork/p/1.1");

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "1.1",
            "0123456789",
            "branchwork/p/1.1",
            None,
            true, // is_continue, branch already exists
        )
        .expect("continue should attach the existing branch");

        assert!(
            path.join("work.txt").exists(),
            "worktree should carry the branch's committed work"
        );
    }

    #[test]
    fn setup_worktree_continue_missing_branch_creates_it() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "2.3",
            "deadbeef00",
            "branchwork/p/2.3",
            None, // base_branch -> HEAD
            true, // is_continue, but branch doesn't exist yet
        )
        .expect("continue with missing branch should create it from HEAD");

        assert!(path.exists());
        assert!(
            branch_exists(proj.path(), "branchwork/p/2.3"),
            "branch should have been created from HEAD"
        );
    }

    #[test]
    fn setup_worktree_fails_loud_on_bad_base_ref() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let err = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "feedface00",
            "branchwork/p/0.1",
            Some("no-such-base-branch"),
            false,
        )
        .expect_err("unknown base branch should fail");
        assert!(
            matches!(err, WorktreeError::GitCommandFailed { .. }),
            "expected GitCommandFailed, got {err:?}"
        );
    }

    #[test]
    fn setup_worktree_path_collision_errors_clearly() {
        // Case 3 / non-negotiable #2: two agents must never share a
        // worktree. A second setup with the same (slug, plan, task,
        // agent_id) resolves to the same path + branch and MUST fail loudly
        // rather than silently co-tenanting the directory.
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "collide0000",
            "branchwork/p/0.1",
            None,
            false,
        )
        .expect("first setup should succeed");

        let err = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "collide0000",
            "branchwork/p/0.1",
            None,
            false,
        )
        .expect_err("second setup at the same path must fail");

        match err {
            WorktreeError::GitCommandFailed { stderr, .. } => {
                let lower = stderr.to_lowercase();
                assert!(
                    lower.contains("already checked out") || lower.contains("already exists"),
                    "stderr should name the collision, got: {stderr}"
                );
            }
            other => panic!("expected GitCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn remove_worktree_removes_clean() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.1",
            "aaaaaaaa11",
            "branchwork/p/0.1",
            None,
            false,
        )
        .unwrap();
        assert!(path.exists());

        let outcome = remove_worktree(proj.path(), &path).expect("remove should succeed");
        assert_eq!(outcome, RemoveOutcome::Removed);
        assert!(!path.exists(), "worktree dir gone after remove");
        assert!(
            !worktree_paths(proj.path()).iter().any(|p| p == &path),
            "git reference pruned after remove"
        );
    }

    #[test]
    fn remove_worktree_tolerates_dir_deleted_behind_gits_back() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.2",
            "bbbbbbbb22",
            "branchwork/p/0.2",
            None,
            false,
        )
        .unwrap();
        // Nuke the dir without telling git, then remove the reference.
        std::fs::remove_dir_all(&path).unwrap();

        let outcome = remove_worktree(proj.path(), &path).expect("remove tolerates missing dir");
        assert!(
            matches!(
                outcome,
                RemoveOutcome::Removed
                    | RemoveOutcome::ForceRemoved
                    | RemoveOutcome::ManuallyRemoved
            ),
            "any escalation tier is acceptable, got {outcome:?}"
        );
        assert!(
            !worktree_paths(proj.path()).iter().any(|p| p == &path),
            "dangling git reference pruned"
        );
    }

    #[test]
    fn remove_worktree_with_held_file_escalates_force() {
        // Case 5: a dirty worktree makes the plain `git worktree remove`
        // refuse, so removal escalates to `--force` (Unix) or the manual
        // `rm -rf` fallback (where, on Windows, an open handle blocks git).
        // Either way the path is gone afterwards.
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let path = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "0.3",
            "held00face0",
            "branchwork/p/0.3",
            None,
            false,
        )
        .unwrap();

        // An untracked file makes the worktree dirty so the tier-1
        // `git worktree remove` refuses on every platform. We also hold it
        // open: harmless on Unix (the untracked file does the forcing),
        // it's the escalation driver on Windows.
        let held = std::fs::File::create(path.join("held.txt")).unwrap();

        let outcome = remove_worktree(proj.path(), &path).expect("escalated remove should succeed");
        drop(held);

        assert!(
            matches!(
                outcome,
                RemoveOutcome::ForceRemoved | RemoveOutcome::ManuallyRemoved
            ),
            "dirty/held worktree should escalate past a plain remove, got {outcome:?}"
        );
        assert!(
            !path.exists(),
            "worktree directory should be gone after removal"
        );
    }

    #[test]
    fn list_orphans_finds_unclaimed_worktrees_under_scope() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        let live = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "1.1",
            "11111111aa",
            "branchwork/p/1.1",
            None,
            false,
        )
        .unwrap();
        let orphan = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj",
            "p",
            "1.2",
            "22222222bb",
            "branchwork/p/1.2",
            None,
            false,
        )
        .unwrap();

        let mut live_cwds = HashSet::new();
        live_cwds.insert(live.clone());

        let orphans = list_orphans_in(base.path(), proj.path(), "proj", &live_cwds);
        // Case 6: with one of the two worktrees claimed, exactly the other
        // one is reported.
        assert_eq!(
            orphans.len(),
            1,
            "exactly the one unclaimed worktree should be reported"
        );
        let orphan_paths: Vec<PathBuf> =
            orphans.iter().map(|o| canonical_or_self(&o.path)).collect();

        assert!(
            orphan_paths.contains(&canonical_or_self(&orphan)),
            "the unclaimed worktree should be reported"
        );
        assert!(
            !orphan_paths.contains(&canonical_or_self(&live)),
            "the live worktree should be excluded"
        );
        assert!(
            !orphan_paths.contains(&canonical_or_self(proj.path())),
            "the main project worktree is outside scope and excluded"
        );

        let reported = orphans
            .iter()
            .find(|o| canonical_or_self(&o.path) == canonical_or_self(&orphan))
            .expect("orphan present");
        assert_eq!(reported.branch.as_deref(), Some("branchwork/p/1.2"));
        assert!(
            reported.last_modified.is_some(),
            "mtime should be populated for an existing dir"
        );
    }

    #[test]
    fn list_orphans_scopes_to_the_requested_slug() {
        let proj = TempDir::new().unwrap();
        let base = TempDir::new().unwrap();
        git_init_master(proj.path());

        // A worktree under a *different* project slug must not be reported
        // when listing orphans for "proj-a".
        let other = setup_worktree_in(
            base.path(),
            proj.path(),
            "proj-b",
            "p",
            "1.1",
            "33333333cc",
            "branchwork/p/1.1",
            None,
            false,
        )
        .unwrap();

        let orphans = list_orphans_in(base.path(), proj.path(), "proj-a", &HashSet::new());
        assert!(
            !orphans
                .iter()
                .any(|o| canonical_or_self(&o.path) == canonical_or_self(&other)),
            "worktree under proj-b must not surface for proj-a"
        );
    }

    #[test]
    fn parse_porcelain_handles_branch_detached_and_trailing_block() {
        let text = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/master

worktree /base/proj/p/1.1-abcd
HEAD 2222222222222222222222222222222222222222
branch refs/heads/branchwork/p/1.1

worktree /base/proj/p/1.2-efgh
HEAD 3333333333333333333333333333333333333333
detached
";
        let entries = parse_worktree_porcelain(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, PathBuf::from("/repo/main"));
        assert_eq!(entries[0].branch.as_deref(), Some("master"));
        assert_eq!(entries[1].path, PathBuf::from("/base/proj/p/1.1-abcd"));
        assert_eq!(entries[1].branch.as_deref(), Some("branchwork/p/1.1"));
        assert_eq!(entries[2].path, PathBuf::from("/base/proj/p/1.2-efgh"));
        assert_eq!(entries[2].branch, None, "detached HEAD has no branch");
    }

    #[test]
    fn parse_porcelain_flushes_final_block_without_trailing_blank() {
        let text = "worktree /a\nHEAD 1\nbranch refs/heads/feature/x";
        let entries = parse_worktree_porcelain(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("/a"));
        assert_eq!(entries[0].branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn worktree_base_is_always_absolute() {
        assert!(
            worktree_base().unwrap().is_absolute(),
            "resolved base must be absolute"
        );
    }

    /// Test helper: `git status --porcelain` output for `cwd`, trimmed.
    fn git_status_porcelain(cwd: &Path) -> String {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Test helper: the worktree paths git knows about for `cwd`.
    fn worktree_paths(cwd: &Path) -> Vec<PathBuf> {
        let out = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(cwd)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        parse_worktree_porcelain(&text)
            .into_iter()
            .map(|e| e.path)
            .collect()
    }
}
