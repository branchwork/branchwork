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

/// Discard a task branch: `git checkout <target> && git branch -D
/// <task_branch>`. Mirrors the two-step sequence in
/// `api/agents.rs::discard_agent_branch` so the runner-side and
/// standalone paths produce byte-identical error strings (Task 5.7
/// SaaS discard parity). `Err(stderr)` carries the captured stderr —
/// the caller maps it to a `BranchDiscarded { ok=false, error }` reply
/// or an HTTP 500.
pub fn discard_branch_local(cwd: &Path, target: &str, task_branch: &str) -> Result<(), String> {
    let checkout = Command::new("git")
        .args(["checkout", target])
        .current_dir(cwd)
        .output();
    match checkout {
        Ok(out) if !out.status.success() => {
            return Err(format!(
                "Failed to checkout {target}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Err(e) => return Err(format!("failed to run git checkout: {e}")),
        _ => {}
    }

    let delete = Command::new("git")
        .args(["branch", "-D", task_branch])
        .current_dir(cwd)
        .output();
    match delete {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "Failed to delete branch: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) => Err(format!("failed to run git branch -D: {e}")),
    }
}

/// One non-fast-forward retry cycle: the SHA we rebased onto (origin's
/// winner of the race) and our new local HEAD after the rebase. The
/// caller (server-side `trigger_after_merge`) writes one
/// `audit::actions::AUTO_PUSH_REBASE_RETRY` row per entry and
/// broadcasts `auto_push_rebased` so the dashboard can render a small
/// "rebased on origin (n)" pill.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebaseRetry {
    /// 1-indexed retry number: 1 is the first rebase-then-retry after the
    /// initial push failed. A push that succeeds on attempt 2 produces
    /// `attempt: 1`; a push that succeeds on attempt 3 produces both
    /// `attempt: 1` and `attempt: 2`.
    pub attempt: usize,
    /// `git rev-parse HEAD` AFTER `git pull --rebase --autostash` — the
    /// local SHA we will retry-push. Diagnostic only.
    pub last_rebase_sha: String,
    /// `git rev-parse refs/remotes/origin/<branch>` AFTER the rebase — the
    /// origin HEAD that beat us in the race (i.e. the SHA we rebased
    /// onto). Used to surface the racing commit in the audit trail.
    pub prior_remote_sha: String,
}

/// Outcome of a successful `push_branch_local` call. `retries` is empty
/// when the very first push attempt succeeded; otherwise it carries one
/// `RebaseRetry` entry per rebase-then-retry cycle the helper performed.
/// Returned as part of `Result::Ok` so a successful push always reports
/// its retry history, even if the dashboard / audit layer ignores it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct PushReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retries: Vec<RebaseRetry>,
}

/// `git push origin <branch>` in `cwd`. On non-fast-forward rejection (a
/// sibling agent / parallel CI run pushed to the same branch first), runs
/// `git pull --rebase --autostash origin <branch>` and retries. Caps at
/// [`MAX_PUSH_ATTEMPTS`] total attempts; non-non-FF failures (auth, hooks,
/// permission denied) return immediately without retry. `Err(stderr)`
/// carries the captured error so the caller can log it.
///
/// On success, the returned `PushReport` describes every rebase-then-retry
/// cycle the helper performed (empty `retries` for a clean first-attempt
/// push). The caller is responsible for emitting `audit_log` rows +
/// `auto_push_rebased` broadcasts from those entries — `git_helpers` is
/// a leaf module and intentionally does not depend on the audit / WS
/// crates.
///
/// Caller assumption: HEAD is on `branch`. All current callers either ran
/// `git checkout <branch>` first (`merge_branch_local`) or are operating
/// on the branch they just merged. Pulling --rebase on a different branch
/// would silently rebase the wrong ref.
pub fn push_branch_local(cwd: &Path, branch: &str) -> Result<PushReport, String> {
    let mut report = PushReport::default();
    let mut last_err = String::new();
    for attempt in 1..=MAX_PUSH_ATTEMPTS {
        match try_push_once(cwd, branch) {
            Ok(()) => {
                if attempt > 1 {
                    eprintln!(
                        "[push-retry] push of {branch} succeeded on attempt {attempt} after rebase"
                    );
                }
                return Ok(report);
            }
            Err(stderr) => {
                last_err = stderr.clone();
                if !is_non_fast_forward_error(&stderr) {
                    // Not a non-FF rejection — auth failure, hook decline,
                    // permission denied, etc. Rebase will not fix any of
                    // these; return immediately so the caller sees the
                    // original error.
                    return Err(stderr);
                }
                if attempt >= MAX_PUSH_ATTEMPTS {
                    break;
                }
                eprintln!(
                    "[push-retry] push of {branch} rejected as non-fast-forward (attempt {attempt}); rebasing against origin/{branch}"
                );
                match rebase_against_origin(cwd, branch) {
                    Ok((last_rebase_sha, prior_remote_sha)) => {
                        // 1-indexed retry counter (the retry that happens
                        // AFTER `attempt` failed). A push that succeeds on
                        // attempt=2 reports a single retry with attempt=1.
                        report.retries.push(RebaseRetry {
                            attempt,
                            last_rebase_sha,
                            prior_remote_sha,
                        });
                    }
                    Err(rebase_err) => {
                        // Best-effort abort so we leave a clean tree behind.
                        // `--autostash` keeps the stash on rebase conflict;
                        // `git rebase --abort` will not unstash either, but
                        // leaves the rebase in a non-conflicted state so the
                        // operator can inspect.
                        let _ = Command::new("git")
                            .args(["rebase", "--abort"])
                            .current_dir(cwd)
                            .output();
                        return Err(format!(
                            "push rejected as non-fast-forward; rebase against origin/{branch} failed: {rebase_err}"
                        ));
                    }
                }
            }
        }
    }
    Err(last_err)
}

/// Cap on the number of `git push` attempts inside [`push_branch_local`].
/// Initial push counts as attempt 1, so the loop performs at most
/// `MAX_PUSH_ATTEMPTS - 1 = 2` rebase-and-retry cycles.
const MAX_PUSH_ATTEMPTS: usize = 3;

/// Single shot of `git push origin <branch>`. Returns the captured stderr on
/// failure so the caller can pattern-match for non-FF markers.
fn try_push_once(cwd: &Path, branch: &str) -> Result<(), String> {
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

/// Does the captured `git push` stderr look like a non-fast-forward
/// rejection that a `git pull --rebase` could plausibly resolve?
///
/// Case-insensitive substring match on the three markers git emits:
/// - `rejected`        — the `[rejected]` ref-update marker
/// - `non-fast-forward` — the explicit refusal classifier
/// - `fetch first`     — used when origin has unfetched commits
///
/// `rejected` alone also matches pre-receive-hook / branch-protection
/// rejections that a rebase will not fix; in that case the retry loop
/// will burn up to two extra attempts before bubbling the same error.
/// That's an intentional trade-off — the spec explicitly lists `rejected`
/// as one of the markers, and the loud-eventually behaviour is still
/// correct.
fn is_non_fast_forward_error(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("non-fast-forward") || s.contains("fetch first") || s.contains("rejected")
}

/// `git pull --rebase --autostash origin <branch>`. `--autostash` is the
/// defence-in-depth safeguard against a sibling agent leaving worktree
/// modifications during the merge step — the dirty-tree check upstream
/// should have caught that, but if it didn't, we don't want a clean push
/// to fail just because of stray writes.
///
/// On success, returns `(last_rebase_sha, prior_remote_sha)`:
/// - `last_rebase_sha` = `git rev-parse HEAD` after the rebase (the new
///   local SHA we will retry-push).
/// - `prior_remote_sha` = `git rev-parse refs/remotes/origin/<branch>`
///   after the rebase (the origin HEAD that won the race — the SHA we
///   rebased onto). `git pull --rebase` updates this remote-tracking ref
///   as part of the fetch phase, so it captures the winner regardless of
///   whether the local `origin/<branch>` was stale before the call.
///
/// If either rev-parse fails (corrupt repo, missing ref) we fall back to
/// the empty string in the corresponding slot rather than failing the
/// retry — the push itself already succeeded; the diagnostic SHAs are
/// nice-to-have, not load-bearing.
fn rebase_against_origin(cwd: &Path, branch: &str) -> Result<(String, String), String> {
    let pull = Command::new("git")
        .args(["pull", "--rebase", "--autostash", "origin", branch])
        .current_dir(cwd)
        .output();
    match pull {
        Ok(out) if out.status.success() => {
            let last_rebase_sha = git_head_sha(cwd).unwrap_or_default();
            let prior_remote_sha =
                git_rev_parse(cwd, &format!("refs/remotes/origin/{branch}")).unwrap_or_default();
            Ok((last_rebase_sha, prior_remote_sha))
        }
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).to_string()),
        Err(e) => Err(format!("failed to run git pull --rebase: {e}")),
    }
}

/// `git rev-parse <ref>` — resolve a refname to a 40-char SHA. Returns
/// `None` if the ref is missing or git fails. Private helper used by
/// `rebase_against_origin` to capture `origin/<branch>` after the
/// fetch+rebase.
fn git_rev_parse(cwd: &Path, refname: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
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

    // ── push_branch_local: non-FF rebase + retry ────────────────────────────
    //
    // Phase 1 / Task 1.1 of the auto-push-rebase-on-non-fast-forward plan.
    // The classic race: a sibling agent / auto-bump job pushes to origin
    // between our merge and our push. The bare-helper level must absorb
    // that race so the auto-mode loop doesn't pause on every parallel push.

    /// Configure a freshly-cloned working tree so `git commit` works without
    /// inheriting the operator's `user.email` / `user.name`.
    fn configure_identity(cwd: &Path, email: &str, name: &str) {
        for args in [
            vec!["config", "user.email", email],
            vec!["config", "user.name", name],
        ] {
            let ok = Command::new("git")
                .args(args.as_slice())
                .current_dir(cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git config {args:?} failed");
        }
    }

    /// Add a file with `body` and commit with `msg` in `cwd`.
    fn commit_file(cwd: &Path, name: &str, body: &str, msg: &str) {
        std::fs::write(cwd.join(name), body).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", cwd.display());
        };
        run(&["add", name]);
        run(&["commit", "-m", msg]);
    }

    /// `git log --format=%s <branch>` parsed into a `Vec<String>` of subjects.
    /// Most-recent-first, matching `git log` default order.
    fn log_subjects(cwd: &Path, refspec: &str) -> Vec<String> {
        let out = Command::new("git")
            .args(["log", "--format=%s", refspec])
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git log {refspec} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn push_branch_local_rebases_on_non_fast_forward() {
        // Acceptance scenario verbatim from the task brief:
        //   - spawn a bare origin
        //   - push commit A from local-a
        //   - in parallel clone local-b push commit B (origin now ahead)
        //   - call push_branch_local from local-a on a new commit C
        //   - assert C lands cleanly and origin sees A -> B -> C-after-rebase
        let tmp = TempDir::new().unwrap();
        let origin = tmp.path().join("origin.git");
        let local_a = tmp.path().join("local-a");
        let local_b = tmp.path().join("local-b");

        // Bare origin, default branch master.
        let ok = Command::new("git")
            .args(["init", "--bare", "-b", "master"])
            .arg(&origin)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init --bare failed");

        // Clone to local-a, commit A, push.
        let ok = Command::new("git")
            .args([
                "clone",
                origin.to_string_lossy().as_ref(),
                local_a.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-a failed");
        configure_identity(&local_a, "a@t", "agent-a");
        commit_file(&local_a, "a.txt", "A\n", "A");
        let first = push_branch_local(&local_a, "master").expect("first push (A) should succeed");
        assert!(
            first.retries.is_empty(),
            "first push (clean A) should have no retries: {first:?}"
        );

        // Clone to local-b (parallel agent), commit B, push.
        let ok = Command::new("git")
            .args([
                "clone",
                origin.to_string_lossy().as_ref(),
                local_b.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "clone to local-b failed");
        configure_identity(&local_b, "b@t", "agent-b");
        commit_file(&local_b, "b.txt", "B\n", "B");
        let second = push_branch_local(&local_b, "master")
            .expect("second push (B from parallel clone) should succeed");
        assert!(
            second.retries.is_empty(),
            "second push (clean B) should have no retries: {second:?}"
        );
        // Origin now at A -> B; local-a still at A.

        // local-a makes commit C on top of A and pushes — should detect
        // non-FF, rebase onto origin/master (= B), and push cleanly.
        commit_file(&local_a, "c.txt", "C\n", "C");
        let result = push_branch_local(&local_a, "master");
        let report = result
            .as_ref()
            .expect("third push (C, with rebase) should succeed");

        // Exactly one rebase retry recorded: attempt 1 failed, we rebased
        // onto B, attempt 2 succeeded. SHAs are diagnostic but must be
        // present.
        assert_eq!(
            report.retries.len(),
            1,
            "expected one retry entry, got {report:?}"
        );
        let r = &report.retries[0];
        assert_eq!(r.attempt, 1, "first retry should be attempt=1");
        assert_eq!(
            r.last_rebase_sha.len(),
            40,
            "last_rebase_sha must be a full SHA"
        );
        assert_eq!(
            r.prior_remote_sha.len(),
            40,
            "prior_remote_sha must be a full SHA"
        );
        // prior_remote_sha must be the SHA of commit B (the winner of the
        // race — local-b's HEAD at the time of its push).
        let b_sha = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&local_b)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(
            r.prior_remote_sha, b_sha,
            "prior_remote_sha must be commit B (origin HEAD that beat us)"
        );

        // Origin should now see A -> B -> C-after-rebase in linear history.
        // git log default ordering is newest-first.
        let subjects = log_subjects(&origin, "master");
        assert_eq!(
            subjects,
            vec!["C".to_string(), "B".to_string(), "A".to_string()],
            "expected linear history A -> B -> C, got {subjects:?}"
        );

        // All three files should be present in the final tree.
        let ls = Command::new("git")
            .args(["ls-tree", "--name-only", "-r", "master"])
            .current_dir(&origin)
            .output()
            .unwrap();
        let files: std::collections::HashSet<String> = String::from_utf8_lossy(&ls.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert!(
                files.contains(name),
                "expected {name} in origin tree, got {files:?}"
            );
        }
    }

    #[test]
    fn push_branch_local_does_not_retry_on_non_non_ff_error() {
        // Auth failures, hook declines, permission denied — none of these
        // are fixable by a rebase, so the retry loop must NOT swallow N
        // attempts trying. Simulate by pushing to a non-existent remote
        // URL: the failure isn't tagged as non-FF, so we should see
        // exactly one push attempt and immediate failure.
        let tmp = TempDir::new().unwrap();
        git_init_with_commit(tmp.path(), "master");
        // Set origin to a path that does not exist — git will fail with
        // "fatal: '<path>' does not appear to be a git repository" or
        // "could not read from remote repository". Neither contains any
        // of the non-FF markers (rejected / non-fast-forward / fetch
        // first), so the loop must bail out after attempt 1.
        let bogus = tmp.path().join("nope.git");
        let ok = Command::new("git")
            .args(["remote", "add", "origin"])
            .arg(&bogus)
            .current_dir(tmp.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git remote add failed");

        let result = push_branch_local(tmp.path(), "master");
        assert!(result.is_err(), "push to bogus remote should fail");
        let err = result.unwrap_err();
        // Sanity check: the error should NOT include our retry banner
        // (which would only show up if the loop tried to rebase).
        assert!(
            !err.to_lowercase().contains("rebase against origin"),
            "did not expect rebase-failure formatting on a non-non-FF error: {err}"
        );
    }

    #[test]
    fn is_non_fast_forward_error_recognises_canonical_markers() {
        // Pin the three markers the brief enumerates (rejected,
        // non-fast-forward, fetch first). A reword in any of git's
        // localized strings would break the retry signal — anchor here
        // first.
        let canonical_non_ff = "To /tmp/origin.git\n ! [rejected]        master -> master (non-fast-forward)\nerror: failed to push some refs to '/tmp/origin.git'\n";
        let fetch_first = "To /tmp/origin.git\n ! [rejected]        master -> master (fetch first)\nerror: failed to push some refs to '/tmp/origin.git'\n";
        let auth = "fatal: Authentication failed for 'https://example.com/repo.git/'\n";
        let no_remote = "fatal: '/tmp/nope.git' does not appear to be a git repository\nfatal: Could not read from remote repository.\n";

        assert!(is_non_fast_forward_error(canonical_non_ff));
        assert!(is_non_fast_forward_error(fetch_first));
        assert!(!is_non_fast_forward_error(auth));
        assert!(!is_non_fast_forward_error(no_remote));
        assert!(!is_non_fast_forward_error(""));
    }
}
