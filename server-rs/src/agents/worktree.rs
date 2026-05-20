//! Temporary git worktree for the pre-merge gate.
//!
//! The pre-merge gate (Phase 1 of the `pre-merge-gate` plan) runs each
//! configured check inside a per-agent `git worktree` so the agent's own
//! files don't fight the check's edits. The worktree is detached on the
//! task branch's tip; cleanup runs unconditionally (Drop guard).
//!
//! Production callers should prefer [`TempWorktree::create`], which takes
//! the agent_id and resolves a `/tmp/bw-gate-<agent_id>` path. The lower-
//! level [`add_worktree_at`] / [`remove_worktree_at`] pair exists for
//! tests that need to pin a custom location.
//!
//! All shell-outs use `std::process::Command` to keep parity with
//! [`crate::agents::phase_check`] (the existing worktree consumer); no
//! async machinery is required — the operations are short and the gate
//! caller already runs inside a tokio task.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Owned handle to a freshly-created `git worktree`. Drops the worktree
/// via `git worktree remove --force` (with a `std::fs::remove_dir_all`
/// fallback) when this struct goes out of scope, so the cleanup contract
/// is intrinsic — callers can't accidentally leak temp state by
/// returning early.
///
/// The brief calls for `git worktree add /tmp/bw-gate-<agent_id> <branch>`
/// → run checks → `git worktree remove --force /tmp/bw-gate-<agent_id>`
/// in a defer/drop guard. This struct is that guard.
#[derive(Debug)]
pub struct TempWorktree {
    /// Project root the worktree was added FROM. We need this for the
    /// `git worktree remove` cleanup, which has to run in the original
    /// repo's `.git` context.
    project_dir: PathBuf,
    /// On-disk path of the worktree itself (e.g. `/tmp/bw-gate-<id>`).
    /// `Option` so [`Drop`] can take ownership and avoid double-cleanup.
    worktree_path: Option<PathBuf>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
