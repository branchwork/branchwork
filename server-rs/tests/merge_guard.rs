//! End-to-end tests for the merge guard and related branch handling.
//!
//! These codify the bugs I shipped in dashboard-polish so they can't
//! regress:
//!   - Empty branch merge must 409, not silently no-op.
//!   - An agent whose source_branch equals the task branch (stale
//!     checkout) must not make the merge guard hit a self-compare.
//!   - rev-list errors on stale refs must degrade permissively, not 500.

mod support;

use serde_json::json;
use support::TestDashboard;

fn seed_agent(
    d: &TestDashboard,
    id: &str,
    plan: &str,
    task: &str,
    branch: Option<&str>,
    source_branch: Option<&str>,
) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, \
             branch, source_branch) \
         VALUES (?1, ?1, ?2, 'completed', 'pty', ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            d.project.to_string_lossy(),
            plan,
            task,
            branch,
            source_branch
        ],
    )
    .unwrap();
}

/// Plan YAML with an absolute `project` path — Windows' `dirs::home_dir()`
/// ignores env overrides, so we can't rely on `$HOME/<name>`. Absolute
/// paths work on both platforms.
fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

#[test]
fn empty_branch_merge_returns_409_not_500() {
    let d = TestDashboard::new();
    d.create_plan("mp-a", &minimal_plan("mp-a", &d.project));

    // A branch with no commits ahead of master — the classic "agent
    // exited without committing" failure mode. Before the guard this
    // silently no-opped. Now it should 409.
    let empty = "branchwork/mp-a/1.1";
    d.create_task_branch(empty, /* with_commit */ false);
    seed_agent(
        &d,
        "agent-empty",
        "mp-a",
        "1.1",
        Some(empty),
        Some("master"),
    );

    let (s, body) = d.post("/api/agents/agent-empty/merge", json!({}));
    assert_eq!(s, 409, "expected 409, got {s}: {body}");
    let msg = body["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("no commits") || msg.contains("not committed"),
        "error message should mention missing commits: {msg}"
    );
    // Branch should still exist — we didn't delete it.
    assert!(d.local_branches().contains(&empty.to_string()));
}

#[test]
fn merge_with_real_commits_succeeds() {
    let d = TestDashboard::new();
    d.create_plan("mp-b", &minimal_plan("mp-b", &d.project));

    let br = "branchwork/mp-b/1.1";
    d.create_task_branch(br, /* with_commit */ true);
    seed_agent(&d, "agent-real", "mp-b", "1.1", Some(br), Some("master"));

    let (s, body) = d.post("/api/agents/agent-real/merge", json!({}));
    assert_eq!(s, 200, "expected 200, got {s}: {body}");
    assert_eq!(body["ok"], true);
    // The branch gets deleted after successful merge.
    assert!(!d.local_branches().contains(&br.to_string()));
}

#[test]
fn self_referencing_source_branch_does_not_cause_500() {
    // Regression: b77d9c0 shipped Fix CI with source_branch recorded AS
    // the task branch (because start_pty_agent captured git_current_branch
    // on a freshly-checked-out task branch). The merge guard then
    // compared X..X = 0 commits and 409'd every legitimate merge; when
    // git couldn't resolve the range it 500'd with "ambiguous argument".
    // The fix (3fea12d) stores NULL when source == task_branch and
    // degrades permissively when rev-list errors.
    let d = TestDashboard::new();
    d.create_plan("mp-c", &minimal_plan("mp-c", &d.project));

    let br = "branchwork/mp-c/1.1";
    d.create_task_branch(br, /* with_commit */ true);
    // Store source_branch as the same thing as branch — the bug shape.
    seed_agent(&d, "agent-self", "mp-c", "1.1", Some(br), Some(br));

    let (s, body) = d.post("/api/agents/agent-self/merge", json!({}));
    // Accept either: 200 (guard falls back to main/master and merge
    // succeeds) or 409 with a legible error. Absolutely no 500.
    assert!(
        s == 200 || s == 409,
        "expected 200 or 409 (not 500), got {s}: {body}"
    );
    // If merge succeeded, the branch is gone. If 409, the branch remains.
    if s == 200 {
        assert!(!d.local_branches().contains(&br.to_string()));
    }
}

#[test]
fn merge_with_nonexistent_source_branch_does_not_500() {
    // If source_branch points at a deleted ref (out-of-band cleanup),
    // the rev-list call errors. Before the fix the guard returned 500.
    // Now it logs and falls through to `git merge` which has its own
    // clearer error handling.
    let d = TestDashboard::new();
    d.create_plan("mp-d", &minimal_plan("mp-d", &d.project));

    let br = "branchwork/mp-d/1.1";
    d.create_task_branch(br, /* with_commit */ true);
    seed_agent(
        &d,
        "agent-ghost",
        "mp-d",
        "1.1",
        Some(br),
        Some("refs/no/such/branch"),
    );

    let (s, _body) = d.post("/api/agents/agent-ghost/merge", json!({}));
    // No 500 — accept 200/409/500-if-the-actual-merge-fails-loudly (not
    // from the guard). Specifically test: not a 500 from the "Failed to
    // inspect branch commits" path.
    assert!(
        s != 500,
        "guard must not 500 on unresolvable source_branch (got {s})"
    );
}
