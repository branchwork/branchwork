//! Leaf module for the local `git` and `gh` shell-outs that both the
//! server (standalone path) and the runner (SaaS dispatch handlers) need.
//!
//! Self-contained — no `crate::` dependencies other than two wire types
//! (`MergeOutcome`, `GhRun`) which themselves live in a leaf module
//! (`saas/runner_protocol.rs`). The runner pulls this file in via
//! `#[path = "../git_helpers.rs"]` and exposes `crate::saas::runner_protocol`
//! through a small re-export wrapper, so the same `use` statement resolves
//! identically in both compilation units.
//!
//! Functions are synchronous: shell out, parse output, return. Callers wrap
//! them in `tokio::task::spawn_blocking` (server CI poller, runner handlers)
//! and add a `tokio::time::timeout` for a wall-clock cap when needed.
//!
//! When this file changes, also touch `agents/git_ops.rs` (server-side
//! dispatchers re-export from here) and `bin/branchwork_runner.rs` (runner-
//! side handlers call directly into here).

#![allow(dead_code)] // Both binaries include this module but each uses a different subset.

use std::path::Path;
use std::process::Command;

use crate::saas::runner_protocol::{GhRun, MergeOutcome};

// ── Branch resolution ───────────────────────────────────────────────────────

/// Resolve the canonical default branch for the repo at `cwd`.
/// Tries `origin/HEAD` first, then falls back to local `master` / `main`.
/// Returns `None` if nothing resolves. Local-only — never fetches.
pub fn git_default_branch(cwd: &Path) -> Option<String> {
    // Step 1: origin/HEAD via symbolic-ref (set by `git clone` and
    // `git remote set-head --auto`). Exits 128, not 1, when absent —
    // gate on status.success() rather than matching exit codes.
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(cwd)
        .output();
    if let Ok(o) = out
        && o.status.success()
    {
        let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if let Some(name) = raw.strip_prefix("origin/")
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }

    // Step 2: probe local master, then main. --quiet suppresses the
    // "Needed a single revision" stderr that rev-parse writes on miss.
    // Note: a freshly `git init -b master`d repo with no commits
    // returns failure here — the symbolic HEAD exists but no ref does.
    for name in ["master", "main"] {
        let ok = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(name.to_string());
        }
    }

    None
}

/// List local branches in the repo at `cwd` (no remotes).
/// Sorted alphabetically. Empty `Vec` if `git` fails.
pub fn git_list_branches(cwd: &Path) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut branches: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    branches.sort();
    branches
}

/// Resolve the current branch name of the repo at `cwd` via
/// `git rev-parse --abbrev-ref HEAD`. Returns `None` for a missing repo,
/// a detached HEAD, or any other failure. Used by the runner-side
/// `MergeAgentBranch` handler to recover the agent's task branch from
/// the cwd it was spawned in (the high-level wire variant doesn't carry
/// `task_branch` — the runner's authoritative answer is "whatever HEAD
/// currently points at").
pub fn git_current_branch(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Capture `git rev-parse HEAD`. Private helper — the merge sequence needs
/// it to populate `MergeOutcome::Ok { merged_sha }`.
fn git_head_sha(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
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

// ── Merge / push ────────────────────────────────────────────────────────────

/// Run the five-step merge sequence locally:
///
///   1. `git rev-list --count <target>..<task_branch>` — empty-branch guard.
///   2. `git checkout <target>`.
///   3. `git merge <task_branch> --no-edit` (abort on conflict).
///   4. `git branch -d <task_branch>` (best-effort cleanup).
///   5. `git rev-parse HEAD` to capture `merged_sha`.
///
/// Returns a [`MergeOutcome`] mirroring the wire protocol so the same enum
/// flows from both the standalone path and the runner reply into the server's
/// HTTP layer.
pub fn merge_branch_local(cwd: &Path, target: &str, task_branch: &str) -> MergeOutcome {
    // 1. Empty-branch guard. If `rev-list` itself fails (deleted ref, detached
    //    HEAD, etc) we fall through permissively — the merge below will
    //    return its own clearer error.
    let revlist = Command::new("git")
        .args(["rev-list", "--count", &format!("{target}..{task_branch}")])
        .current_dir(cwd)
        .output();
    if let Ok(output) = &revlist
        && output.status.success()
    {
        let count: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        if count == 0 {
            return MergeOutcome::EmptyBranch;
        }
    }

    // 2. Checkout target.
    let checkout = Command::new("git")
        .args(["checkout", target])
        .current_dir(cwd)
        .output();
    match checkout {
        Ok(output) if !output.status.success() => {
            return MergeOutcome::CheckoutFailed {
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            };
        }
        Err(e) => {
            return MergeOutcome::Other {
                stderr: format!("Failed to run git: {e}"),
            };
        }
        _ => {}
    }

    // 3. Merge.
    let merge = Command::new("git")
        .args(["merge", task_branch, "--no-edit"])
        .current_dir(cwd)
        .output();
    match merge {
        Ok(output) if output.status.success() => {
            // 4. Best-effort branch cleanup.
            Command::new("git")
                .args(["branch", "-d", task_branch])
                .current_dir(cwd)
                .output()
                .ok();
            // 5. Capture merged SHA.
            let merged_sha = git_head_sha(cwd).unwrap_or_default();
            MergeOutcome::Ok { merged_sha }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Abort the failed merge so the working tree is clean.
            Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(cwd)
                .output()
                .ok();
            MergeOutcome::Conflict { stderr }
        }
        Err(e) => MergeOutcome::Other {
            stderr: format!("Failed to run git merge: {e}"),
        },
    }
}

/// `git push origin <branch>` in `cwd`. `Err(stderr)` carries the captured
/// error so the caller can log it.
pub fn push_branch_local(cwd: &Path, branch: &str) -> Result<(), String> {
    let push = Command::new("git")
        .args(["push", "origin", branch])
        .current_dir(cwd)
        .output();
    match push {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).to_string()),
        Err(e) => Err(format!("failed to run git push: {e}")),
    }
}

// ── gh CLI ──────────────────────────────────────────────────────────────────

/// `gh run list --commit <sha> -L 1 --json databaseId,status,conclusion,url`
/// in `cwd`. Returns the most recent workflow run, or `None` when no
/// workflow has fired yet, `gh` is unavailable, or the call failed.
pub fn gh_run_list_local(cwd: &Path, sha: &str) -> Option<GhRun> {
    let out = Command::new("gh")
        .args([
            "run",
            "list",
            "--commit",
            sha,
            "-L",
            "1",
            "--json",
            "databaseId,status,conclusion,url",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let runs: Vec<GhRun> = serde_json::from_slice(&out.stdout).ok()?;
    runs.into_iter().next()
}

/// One workflow run as parsed from `gh run list --json
/// databaseId,workflowName,status,conclusion,createdAt`. Used by the
/// auto-mode CI gate (multi-run aggregation) on both the runner and the
/// standalone server. Fields beyond `databaseId` are best-effort:
/// defaulted on absent JSON keys so a stub `gh` (or a future schema
/// change) doesn't turn the whole aggregate into `None`.
#[derive(Debug, serde::Deserialize, Clone)]
pub struct GhRunDetail {
    #[serde(rename = "databaseId")]
    pub database_id: i64,
    #[serde(rename = "workflowName", default)]
    pub workflow_name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    /// ISO-8601 timestamp from `gh`. Used for sorting before passing into
    /// `ci::aggregate::compute` so `failing_run_id` resolves to the
    /// chronologically-earliest failure (the root cause).
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<String>,
}

/// `gh run list --commit <sha> --json
/// databaseId,workflowName,status,conclusion,createdAt --limit 50` in
/// `cwd`. Returns the full set of workflow runs for the SHA so callers
/// can apply the auto-mode aggregation rule. Returns `None` only when
/// the call itself failed (gh not installed, no auth, etc); an empty
/// result set comes back as `Some(vec![])` so callers can distinguish
/// "still polling" from "tooling broken."
pub fn gh_run_list_full_local(cwd: &Path, sha: &str) -> Option<Vec<GhRunDetail>> {
    let out = Command::new("gh")
        .args([
            "run",
            "list",
            "--commit",
            sha,
            "--json",
            "databaseId,workflowName,status,conclusion,createdAt",
            "--limit",
            "50",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// `gh run view <run_id> --log-failed` in `cwd`. The `--log-failed` output
/// can be hundreds of KB; keep the **tail** (failures accumulate at the end)
/// trimmed to ~8 KB and decode lossily so stray non-UTF-8 bytes don't drop
/// the buffer. Returns `None` when the run has no failure log (still
/// pending, gh unavailable, no auth, etc).
pub fn gh_failure_log_local(cwd: &Path, run_id: &str) -> Option<String> {
    const CAP_BYTES: usize = 8 * 1024;
    let out = Command::new("gh")
        .args(["run", "view", run_id, "--log-failed"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = out.stdout;
    let start = raw.len().saturating_sub(CAP_BYTES);
    Some(String::from_utf8_lossy(&raw[start..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git_init_with_commit(dir: &Path, initial_branch: &str) {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
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

    #[test]
    fn git_default_branch_master_via_local_probe() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        assert_eq!(git_default_branch(dir.path()), Some("master".to_string()));
    }

    #[test]
    fn git_default_branch_main_via_local_probe() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "main");
        assert_eq!(git_default_branch(dir.path()), Some("main".to_string()));
    }

    #[test]
    fn git_default_branch_none_when_no_commits() {
        let dir = TempDir::new().unwrap();
        let ok = Command::new("git")
            .args(["init", "-b", "master"])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok);
        // No commit yet — `master` is the symbolic HEAD but no ref exists,
        // so rev-parse --verify --quiet fails on both probes.
        assert_eq!(git_default_branch(dir.path()), None);
    }

    #[test]
    fn git_default_branch_uses_origin_head_when_set() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        // Seed a fake remote-tracking ref and point origin/HEAD at a
        // non-trunk branch. No clone or fetch needed.
        let head_sha = git_head_sha(dir.path()).unwrap();
        let refs_dir = dir.path().join(".git/refs/remotes/origin");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(refs_dir.join("trunk"), format!("{head_sha}\n")).unwrap();
        let ok = Command::new("git")
            .args([
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "failed to set origin/HEAD symref");
        assert_eq!(git_default_branch(dir.path()), Some("trunk".to_string()));
    }

    #[test]
    fn git_list_branches_single_master() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        assert_eq!(git_list_branches(dir.path()), vec!["master".to_string()]);
    }

    #[test]
    fn git_list_branches_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        run(&["branch", "feature/x"]);
        run(&["branch", "bw/1.1"]);
        assert_eq!(
            git_list_branches(dir.path()),
            vec![
                "bw/1.1".to_string(),
                "feature/x".to_string(),
                "master".to_string(),
            ]
        );
    }

    #[test]
    fn git_list_branches_empty_when_not_a_git_repo() {
        let dir = TempDir::new().unwrap();
        assert_eq!(git_list_branches(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn merge_branch_empty_returns_empty_branch() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        // Create a branch that points at the same commit — no commits ahead.
        Command::new("git")
            .args(["branch", "feature/empty"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let outcome = merge_branch_local(dir.path(), "master", "feature/empty");
        assert_eq!(outcome, MergeOutcome::EmptyBranch);
    }

    #[test]
    fn merge_branch_happy_path_returns_merged_sha() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        // Create feature branch with one commit ahead.
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
        };
        run(&["checkout", "-b", "feature/x"]);
        std::fs::write(dir.path().join("foo.txt"), "hi").unwrap();
        run(&["add", "foo.txt"]);
        run(&["commit", "-m", "add foo"]);
        run(&["checkout", "master"]);

        let outcome = merge_branch_local(dir.path(), "master", "feature/x");
        match outcome {
            MergeOutcome::Ok { merged_sha } => {
                assert!(!merged_sha.is_empty());
                assert_eq!(merged_sha.len(), 40, "expected full SHA");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        // Branch should be cleaned up.
        let branches = git_list_branches(dir.path());
        assert!(!branches.contains(&"feature/x".to_string()));
    }

    #[test]
    fn merge_branch_conflict_aborts_cleanly() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
        };
        // Set up two divergent commits touching the same file.
        std::fs::write(dir.path().join("conflict.txt"), "base\n").unwrap();
        run(&["add", "conflict.txt"]);
        run(&["commit", "-m", "base"]);

        run(&["checkout", "-b", "feature/conflict"]);
        std::fs::write(dir.path().join("conflict.txt"), "branch side\n").unwrap();
        run(&["add", "conflict.txt"]);
        run(&["commit", "-m", "branch change"]);

        run(&["checkout", "master"]);
        std::fs::write(dir.path().join("conflict.txt"), "master side\n").unwrap();
        run(&["add", "conflict.txt"]);
        run(&["commit", "-m", "master change"]);

        let outcome = merge_branch_local(dir.path(), "master", "feature/conflict");
        assert!(matches!(outcome, MergeOutcome::Conflict { .. }));
        // No leftover MERGE_HEAD.
        assert!(!dir.path().join(".git/MERGE_HEAD").exists());
    }
}
