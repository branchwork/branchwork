//! Integration acceptance for non-negotiable #1 of the
//! `worktree-per-agent-isolation` plan (Task 1.3): two agents working on
//! different task branches each get their own git worktree and never
//! collide on a shared `HEAD`.
//!
//! Flow: build a bare repo + working clone (one commit on `master`),
//! create a worktree for two distinct `(task, agent_id)` pairs under the
//! same plan, commit a disjoint file in each, assert each branch carries
//! only its own file, merge both into `master` cleanly, then
//! `remove_worktree` both and confirm `git worktree list` is back to the
//! repo alone.
//!
//! `branchwork-server` has no library target, so — exactly as
//! `tests/pty_resize.rs` reaches the supervisor — we `#[path]`-include the
//! worktree module into this test crate. We drive the `setup_worktree_in`
//! seam (which takes an explicit base) rather than the public
//! `setup_worktree` so the base is pinned to a tempdir without ever
//! mutating `BRANCHWORK_WORKTREE_BASE`. That matters because the `#[path]`
//! include also pulls the module's own `#[cfg(test)] mod tests` into this
//! binary (integration crates compile with `cfg(test)` set), and one of
//! those (`worktree_base_is_always_absolute`) reads that env var — so a
//! `set_var` here would be a process-wide data race against it. Using the
//! `_in` seam is the exact pattern T1.1 built for tests ("never set_var").
//!
//! `tests/support`'s `run` git helper is module-private (cf.
//! `tests/flush_merges.rs`), so we use a small local `git` helper and
//! build the bare repo + clone inline.

#[allow(dead_code)]
#[path = "../src/agents/worktree.rs"]
mod worktree;

use std::path::{Path, PathBuf};
use std::process::Command;

use worktree::{remove_worktree, setup_worktree_in};

/// Capture trimmed stdout of a git command in `cwd`, panicking with stderr
/// on a non-zero exit.
fn git_out(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run a git command in `cwd` for its side effect (output discarded).
fn git(cwd: &Path, args: &[&str]) {
    git_out(cwd, args);
}

/// Write `file=content` and commit it inside `worktree`.
fn commit_file(worktree: &Path, file: &str, content: &str, msg: &str) {
    std::fs::write(worktree.join(file), content).unwrap();
    git(worktree, &["add", file]);
    git(worktree, &["commit", "-q", "-m", msg]);
}

/// Paths git reports for `repo` via `git worktree list --porcelain`.
fn worktree_paths(repo: &Path) -> Vec<PathBuf> {
    git_out(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

#[test]
fn two_parallel_worktrees_do_not_cross_contaminate() {
    let tmp = tempfile::TempDir::new().unwrap();

    // ── 1. Bare repo + working clone, one commit on master ───────────
    let origin = tmp.path().join("origin.git");
    git(
        tmp.path(),
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "master",
            origin.to_str().unwrap(),
        ],
    );

    let work = tmp.path().join("work");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    git(&work, &["config", "user.email", "test@branchwork.local"]);
    git(&work, &["config", "user.name", "Branchwork Test"]);
    std::fs::write(work.join("README.md"), "init\n").unwrap();
    git(&work, &["add", "README.md"]);
    git(&work, &["commit", "-q", "-m", "initial"]);
    git(&work, &["push", "-q", "origin", "master"]);

    // Worktree base lives in its own tempdir so the test never touches the
    // real `~/.branchwork/worktrees`.
    let base = tmp.path().join("wt-base");

    // ── 2. Two worktrees: same plan, different (task, agent_id) ──────
    let branch_a = "branchwork/wt-iso/1.1";
    let branch_b = "branchwork/wt-iso/1.2";
    let path_a = setup_worktree_in(
        &base,
        &work,
        "iso-proj",
        "wt-iso",
        "1.1",
        "aaaaaaaa0000",
        branch_a,
        None,
        false,
    )
    .expect("setup worktree A");
    let path_b = setup_worktree_in(
        &base,
        &work,
        "iso-proj",
        "wt-iso",
        "1.2",
        "bbbbbbbb1111",
        branch_b,
        None,
        false,
    )
    .expect("setup worktree B");

    assert_ne!(
        path_a, path_b,
        "the two agents must get distinct worktree paths"
    );
    assert!(path_a.exists(), "worktree A should exist on disk");
    assert!(path_b.exists(), "worktree B should exist on disk");

    // ── 3. Commit disjoint work in each worktree ─────────────────────
    commit_file(&path_a, "feature_a.txt", "A", "feat a");
    commit_file(&path_b, "feature_b.txt", "B", "feat b");

    // ── 4. Each branch HEAD carries only its own feature file ────────
    // Working-tree isolation: neither worktree ever sees the other's file.
    assert!(path_a.join("feature_a.txt").exists(), "A's worktree has A");
    assert!(
        !path_a.join("feature_b.txt").exists(),
        "A's worktree must not contain B's file"
    );
    assert!(path_b.join("feature_b.txt").exists(), "B's worktree has B");
    assert!(
        !path_b.join("feature_a.txt").exists(),
        "B's worktree must not contain A's file"
    );

    // Committed-tree isolation: the branch HEAD trees are disjoint.
    let tree_a = git_out(&work, &["ls-tree", "-r", "--name-only", branch_a]);
    assert!(
        tree_a.lines().any(|l| l == "feature_a.txt"),
        "branch A HEAD has feature_a.txt: {tree_a:?}"
    );
    assert!(
        !tree_a.lines().any(|l| l == "feature_b.txt"),
        "branch A HEAD must not have feature_b.txt: {tree_a:?}"
    );
    let tree_b = git_out(&work, &["ls-tree", "-r", "--name-only", branch_b]);
    assert!(
        tree_b.lines().any(|l| l == "feature_b.txt"),
        "branch B HEAD has feature_b.txt: {tree_b:?}"
    );
    assert!(
        !tree_b.lines().any(|l| l == "feature_a.txt"),
        "branch B HEAD must not have feature_a.txt: {tree_b:?}"
    );

    // ── 5. Merge both branches into master from the project root ─────
    // `work` is on master; the task branches are checked out in A/B, but
    // merging *into* master only reads their commits, so this is allowed.
    // The two branches touch disjoint files, so both merges are clean.
    git(
        &work,
        &["merge", "--no-ff", "-m", "merge task 1.1", branch_a],
    );
    git(
        &work,
        &["merge", "--no-ff", "-m", "merge task 1.2", branch_b],
    );
    let master_tree = git_out(&work, &["ls-tree", "-r", "--name-only", "master"]);
    assert!(
        master_tree.lines().any(|l| l == "feature_a.txt"),
        "master HEAD has feature_a.txt after merge: {master_tree:?}"
    );
    assert!(
        master_tree.lines().any(|l| l == "feature_b.txt"),
        "master HEAD has feature_b.txt after merge: {master_tree:?}"
    );

    // ── 6. Remove both worktrees; only the original repo remains ─────
    remove_worktree(&work, &path_a).expect("remove worktree A");
    remove_worktree(&work, &path_b).expect("remove worktree B");
    assert!(!path_a.exists(), "worktree A dir gone after remove");
    assert!(!path_b.exists(), "worktree B dir gone after remove");

    let listed = worktree_paths(&work);
    assert_eq!(
        listed.len(),
        1,
        "only the original repo worktree should remain: {listed:?}"
    );
    assert_eq!(
        std::fs::canonicalize(&listed[0]).unwrap(),
        std::fs::canonicalize(&work).unwrap(),
        "the remaining worktree must be the project root: {listed:?}"
    );
}
