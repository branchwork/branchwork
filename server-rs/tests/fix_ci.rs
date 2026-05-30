//! End-to-end test for POST /api/actions/fix-ci.
//!
//! Acceptance for dashboard-stability T3.3: the fix-CI endpoint must
//! record `source_branch` as the repo's trunk (master/main), NOT the
//! freshly-created `branchwork/fix/...` recovery branch.
//!
//! Why this test exists — bug shipped in b77d9c0 ("Dashboard polish:
//! task lock, agent reconcile, heartbeat, Fix CI") and fixed in 8f468f5
//! + 2478c20:
//!
//!   - `start_pty_agent` originally derived `source_branch` from
//!     `git_current_branch(cwd)`. The fix-CI handler had already done
//!     `git checkout -b <fix_branch>` by the time `start_pty_agent`
//!     ran, so the captured value was the fix branch itself.
//!   - The merge guard ran `git rev-list --count <src>..<fix>` and got
//!     0, which 409'd every legitimate fix commit (X..X = 0).
//!   - Two layers of fix landed: (1) the fix-CI handler captures the
//!     original branch BEFORE the checkout and re-`UPDATE`s the agents
//!     row after `start_pty_agent` returns; (2) `start_pty_agent`
//!     itself was changed to use `git_default_branch` instead of
//!     `git_current_branch`, so the captured value is now the canonical
//!     trunk regardless of caller pre-state.
//!
//! The assertion `agents.source_branch == "master"` catches the bug
//! shape directly: revert either fix in isolation and the test still
//! passes (the other layer fills in), but reverting both makes the
//! captured value drift to the fix branch and the assertion fails —
//! which is the "revert the fix → test fails" half of the acceptance.
//!
//! The harness's default path (no GitHub remote) seeds the failing
//! `ci_runs` row directly: a real `gh repo create` + push + wait-for-CI
//! cycle takes minutes and depends on external services, but the bug
//! lives entirely on the standalone server's local-fs path, so the
//! seeded variant catches the regression deterministically. An
//! optional gated companion test (`BRANCHWORK_TEST_GH_REPOS=1`) walks
//! the full GitHub Actions failure path for the cases where the
//! operator wants the wider acceptance signal.

mod support;

use serde_json::json;
use support::TestDashboard;

/// Plan YAML matching the Branchwork YamlPlan schema. Same shape as
/// `recovery.rs::minimal_plan` — kept inline so this file remains
/// self-contained when read in isolation.
fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

fn head_sha(cwd: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .expect("spawn git rev-parse HEAD");
    assert!(
        out.status.success(),
        "git rev-parse HEAD: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn rev_parse(cwd: &std::path::Path, refname: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", refname])
        .current_dir(cwd)
        .output()
        .expect("spawn git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse {refname}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Insert a `failure` ci_runs row pinned to `commit_sha`. The fix-CI
/// handler reads this row to learn the branch-off SHA + cached failure
/// log, so a real `commit_sha` (resolvable in the scratch repo) is
/// load-bearing — a bogus value would 500 on `git checkout -b`.
fn seed_failing_ci_run(d: &TestDashboard, plan: &str, task: &str, commit_sha: &str) -> i64 {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha, failure_log) \
         VALUES (?1, ?2, 'failure', ?3, 'cargo fmt -- --check FAILED on src/lib.rs')",
        rusqlite::params![plan, task, commit_sha],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Read the agents row inserted by the fix-CI handler. Returns the
/// columns the test asserts on. The agents row is written before the
/// PTY supervisor spawns, so even when `claude` isn't on PATH (the
/// default test env) the row exists and carries the recorded
/// `source_branch`.
fn agent_row(d: &TestDashboard, agent_id: &str) -> (Option<String>, Option<String>) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.query_row(
        "SELECT branch, source_branch FROM agents WHERE id = ?1",
        rusqlite::params![agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("agents row for fix-ci agent")
}

/// The agent's working directory as recorded by `start_pty_agent`. With
/// worktree-per-agent isolation on (the default), this is the agent's own
/// linked worktree under `$BRANCHWORK_WORKTREE_BASE`, NOT the shared project
/// tree — the recovery agent commits its fix here, so the test must too.
fn agent_cwd(d: &TestDashboard, agent_id: &str) -> std::path::PathBuf {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    let cwd: String = conn
        .query_row(
            "SELECT cwd FROM agents WHERE id = ?1",
            rusqlite::params![agent_id],
            |r| r.get(0),
        )
        .expect("agents row for fix-ci agent");
    std::path::PathBuf::from(cwd)
}

#[test]
fn fix_ci_records_trunk_as_source_branch_not_fix_branch() {
    // The bug-catching assertion. Before the b77d9c0 → 8f468f5 fix and
    // the 2478c20 follow-up, `agents.source_branch` for a fix-CI agent
    // was the fix branch itself (because `start_pty_agent` read
    // `git_current_branch(cwd)` AFTER the handler had already checked
    // out the fix branch). With both fixes in place, the column should
    // reflect the canonical trunk — `master` in the scratch repo.
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-fix-src", &minimal_plan("plan-fix-src", &d.project));

    let sha = head_sha(&d.project);
    let run_id = seed_failing_ci_run(&d, &plan, "1.1", &sha);

    let (status, body) = d.post(
        "/api/actions/fix-ci",
        json!({
            "planName": plan,
            "taskNumber": "1.1",
            "ciRunId": run_id,
        }),
    );
    assert_eq!(status, 200, "fix-ci POST failed: {body:?}");
    let agent_id = body["agentId"]
        .as_str()
        .expect("agentId in fix-ci response")
        .to_string();
    let fix_branch = body["branch"]
        .as_str()
        .expect("branch in fix-ci response")
        .to_string();
    assert_eq!(
        fix_branch,
        format!("branchwork/fix/{plan}/1.1/{run_id}"),
        "fix branch name should match the documented format"
    );

    let (db_branch, db_source_branch) = agent_row(&d, &agent_id);
    assert_eq!(
        db_branch.as_deref(),
        Some(fix_branch.as_str()),
        "agents.branch should be the fix branch"
    );
    // The bug catch: source_branch must be the trunk (master), NOT the
    // fix branch. If you revert both the start_pty_agent change
    // (git_default_branch ↔ git_current_branch) AND the fix-CI handler
    // UPDATE, the value drifts to the fix branch and this assertion
    // fails — which is exactly the b77d9c0 regression shape.
    assert_eq!(
        db_source_branch.as_deref(),
        Some("master"),
        "agents.source_branch must be the canonical trunk, got {db_source_branch:?} \
         (the b77d9c0 bug would record the fix branch `{fix_branch}` here)"
    );
    assert_ne!(
        db_source_branch.as_deref(),
        Some(fix_branch.as_str()),
        "agents.source_branch must not be the fix branch — that's the b77d9c0 regression"
    );
}

#[test]
fn fix_ci_branch_merges_after_a_real_fix_commit() {
    // The other half of the acceptance: "merge succeeds." The fix-CI
    // handler creates the recovery branch at the failing SHA (= same
    // commit as master in this fixture, since the test seeds the
    // failure on the initial commit). To exercise the merge path
    // realistically we add one commit on the fix branch — the agent
    // would commit its fix here in production — and then drive the
    // merge endpoint. Pre-fix behaviour with `source_branch =
    // fix_branch`: `git rev-list --count <fix>..<fix>` = 0 → 409. With
    // the fix in place: `git rev-list --count <master>..<fix>` = 1 →
    // merge proceeds.
    //
    // Note: the merge resolver re-resolves the canonical default at
    // merge time (api/agents.rs::resolve_merge_target), so even with
    // the buggy `source_branch` recorded the merge guard's `target`
    // parameter still comes from the freshly-resolved default. The
    // bug-shape failure under today's code path is the captured-but-
    // wrong source_branch column itself; this test pins the success
    // path so a future refactor that re-couples the merge guard to
    // `agents.source_branch` would also surface the regression here.
    let d = TestDashboard::new();
    let plan = d.create_plan(
        "plan-fix-merge",
        &minimal_plan("plan-fix-merge", &d.project),
    );

    let sha = head_sha(&d.project);
    let run_id = seed_failing_ci_run(&d, &plan, "1.1", &sha);

    let (status, body) = d.post(
        "/api/actions/fix-ci",
        json!({
            "planName": plan,
            "taskNumber": "1.1",
            "ciRunId": run_id,
        }),
    );
    assert_eq!(status, 200, "fix-ci POST failed: {body:?}");
    let agent_id = body["agentId"].as_str().unwrap().to_string();
    let fix_branch = body["branch"].as_str().unwrap().to_string();

    // Reproduce the end-state of a *completed* recovery agent: a live
    // per-agent worktree checked out on the fix branch, carrying the fix
    // commit. With worktree isolation on (the default) the agent runs in its
    // OWN linked worktree, not the shared project tree — committing in
    // `d.project` would advance master and leave the fix branch empty.
    //
    // We can't run a real `claude` agent here, and the spawn's transient
    // worktree is environment-dependent: when `claude` is missing (CI) the
    // supervisor fails to start and `cleanup_worktree_for_terminated_agent`
    // tears the worktree down, leaving `agents.cwd` pointing at a removed
    // directory; when `claude` is present (a dev box) it lingers. Normalise
    // to a known state — drop whatever the spawn left, then re-attach a
    // worktree to the surviving fix-branch ref at the agent's recorded cwd,
    // exactly as a completed agent would have left it. The fix-CI handler's
    // `git branch <fix_branch>` ref survives worktree teardown, so it's always
    // there to attach.
    let worktree = agent_cwd(&d, &agent_id);
    let worktree_str = worktree.to_str().unwrap();
    let _ = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", worktree_str])
        .current_dir(&d.project)
        .output();
    let _ = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&d.project)
        .output();
    let add_wt = std::process::Command::new("git")
        .args(["worktree", "add", worktree_str, &fix_branch])
        .current_dir(&d.project)
        .output()
        .unwrap();
    assert!(
        add_wt.status.success(),
        "re-attach worktree to fix branch: {}",
        String::from_utf8_lossy(&add_wt.stderr)
    );

    std::fs::write(worktree.join("fix.txt"), "the fix").unwrap();
    let add = std::process::Command::new("git")
        .args(["add", "fix.txt"])
        .current_dir(&worktree)
        .status()
        .unwrap();
    assert!(add.success());
    let commit = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "fix the broken thing"])
        .current_dir(&worktree)
        .status()
        .unwrap();
    assert!(commit.success());
    let fix_sha = head_sha(&worktree);
    assert_ne!(fix_sha, sha, "fix commit should advance the branch");
    // Branch refs are shared across all worktrees of a repo, so the main
    // project tree sees the fix branch advance even though the commit was
    // made in the agent's linked worktree.
    assert_eq!(
        rev_parse(&d.project, &fix_branch),
        fix_sha,
        "fix branch should now point at the fix commit"
    );

    // Flip the agent to `completed` so the merge endpoint dispatches.
    {
        let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
        conn.execute(
            "UPDATE agents SET status = 'completed' WHERE id = ?1",
            rusqlite::params![agent_id],
        )
        .unwrap();
    }

    let (s, body) = d.post(&format!("/api/agents/{agent_id}/merge"), json!({}));
    assert_eq!(s, 200, "merge after fix commit failed: {body:?}");
    assert_eq!(body["into"], "master", "merge target must be the trunk");

    // Master should now contain the fix commit, and the fix branch
    // should be gone (the merge step deletes it on success).
    assert_eq!(
        rev_parse(&d.project, "master"),
        fix_sha,
        "master should be at the fix commit after merge"
    );
    assert!(
        !d.local_branches().contains(&fix_branch),
        "fix branch should have been deleted after merge: {:?}",
        d.local_branches()
    );
}
