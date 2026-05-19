//! End-to-end coverage for `POST /api/plans/:name/flush-merges` (Task 2.3
//! of the ci-cadence-build-vs-test-configurable plan).
//!
//! The flush endpoint is the operator escape hatch for the cadence
//! batch — drains every `merge_status='deferred_for_cadence'` agent
//! in the plan, regardless of the configured cadence, and triggers CI
//! on the final merge. These tests cover:
//!   - 3 deferred agents → all 3 merge, ci_runs row inserted, agent
//!     `merge_status` flips back to NULL.
//!   - Re-running with zero deferred → clean no-op response.

mod support;

use serde_json::json;
use std::path::Path;
use support::TestDashboard;

fn minimal_3task_plan(name: &str, project_dir: &Path) -> String {
    format!(
        "title: {name}\n\
         context: ''\n\
         project: {project}\n\
         phases:\n  \
           - number: 1\n    \
             title: Phase 1\n    \
             description: ''\n    \
             tasks:\n      \
               - number: '1.1'\n        \
                 title: Task 1.1\n        \
                 description: ''\n        \
                 acceptance: ''\n      \
               - number: '1.2'\n        \
                 title: Task 1.2\n        \
                 description: ''\n        \
                 acceptance: ''\n      \
               - number: '1.3'\n        \
                 title: Task 1.3\n        \
                 description: ''\n        \
                 acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Same shape as the auto_mode_completion seed helper but pre-stamps
/// `merge_status = 'deferred_for_cadence'` so the flush endpoint sees
/// rows that match its filter.
fn seed_deferred_agent(
    d: &TestDashboard,
    id: &str,
    plan: &str,
    task: &str,
    branch: &str,
    started_at: &str,
) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, branch, \
             source_branch, org_id, merge_status, started_at) \
         VALUES (?1, ?1, ?2, 'completed', 'pty', ?3, ?4, ?5, 'master', \
                 'default-org', 'deferred_for_cadence', ?6)",
        rusqlite::params![
            id,
            d.project.to_string_lossy(),
            plan,
            task,
            branch,
            started_at
        ],
    )
    .unwrap();
}

/// Run `git <args>` in the project dir, panicking on non-zero exit
/// (mirrors `support::run`, which is module-private).
fn git(project: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(project)
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create a task branch that touches a UNIQUE file so multiple
/// task-branches off master can all merge back without 3-way merge
/// conflicts on a shared file (e.g. `work.txt`). Mirror of
/// `TestDashboard::create_task_branch`, kept local because the helper
/// upstream writes the same file from every branch.
fn create_distinct_task_branch(d: &TestDashboard, branch: &str, file: &str) {
    git(&d.project, &["checkout", "-q", "-b", branch]);
    std::fs::write(d.project.join(file), format!("content for {branch}")).unwrap();
    git(&d.project, &["add", file]);
    git(
        &d.project,
        &["commit", "-q", "-m", &format!("work on {branch}")],
    );
    git(&d.project, &["checkout", "-q", "master"]);
}

#[test]
fn flush_merges_drains_three_deferred_agents_and_triggers_ci_once() {
    let d = TestDashboard::new();
    d.create_plan(
        "flush-three",
        &minimal_3task_plan("flush-three", &d.project),
    );

    // T2.2 (cadence-deferral) only triggers CI on the cadence boundary;
    // for the flush variant, we want the FINAL merge to trigger CI so the
    // operator gets exactly one master build + deploy. The `should_record_ci_run`
    // gate inside `trigger_after_merge` needs:
    //   1. `.github/workflows/*.yml` present on the merged target
    //      (committed BEFORE the task branches so they inherit it).
    //   2. An `origin` remote that accepts pushes — a bare repo inside
    //      the tempdir does that without network.
    //   3. The merge target equals the canonical default branch (master).
    d.setup_github_actions();
    d.setup_origin_remote();

    // Three task branches, each touching a UNIQUE file so the second and
    // third merges land cleanly without conflicting on a shared `work.txt`.
    create_distinct_task_branch(&d, "branchwork/flush-three/1.1", "f1.txt");
    create_distinct_task_branch(&d, "branchwork/flush-three/1.2", "f2.txt");
    create_distinct_task_branch(&d, "branchwork/flush-three/1.3", "f3.txt");

    // Seed agents in dependency order (1.1 first, 1.3 last). started_at
    // is the tie-breaker inside `list_all_deferred_in_order` when several
    // agent rows reference the same task — they don't collide here, but
    // pinning the order keeps the test resilient to that helper changing.
    seed_deferred_agent(
        &d,
        "agent-1.1",
        "flush-three",
        "1.1",
        "branchwork/flush-three/1.1",
        "2026-05-19 10:00:00",
    );
    seed_deferred_agent(
        &d,
        "agent-1.2",
        "flush-three",
        "1.2",
        "branchwork/flush-three/1.2",
        "2026-05-19 10:01:00",
    );
    seed_deferred_agent(
        &d,
        "agent-1.3",
        "flush-three",
        "1.3",
        "branchwork/flush-three/1.3",
        "2026-05-19 10:02:00",
    );

    let (status, body) = d.post("/api/plans/flush-three/flush-merges", json!({}));
    assert_eq!(status, 200, "expected 200, got {status}: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["count"], 3, "expected 3 merges, got {body}");
    assert_eq!(body["paused"], false);
    assert_eq!(body["ciTriggered"], true);
    let merged = body["merged"].as_array().expect("merged array");
    assert_eq!(merged.len(), 3);
    // Plan-declaration order (1.1, 1.2, 1.3) per `list_all_deferred_in_order`.
    assert_eq!(merged[0]["taskId"], "1.1");
    assert_eq!(merged[1]["taskId"], "1.2");
    assert_eq!(merged[2]["taskId"], "1.3");

    // Every drained agent's `merge_status` should flip back to NULL —
    // `run_merge_step` clears the column inside its success branch so a
    // future cadence-boundary drain doesn't re-pick the same row.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for id in ["agent-1.1", "agent-1.2", "agent-1.3"] {
        let ms: Option<String> = conn
            .query_row(
                "SELECT merge_status FROM agents WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ms, None,
            "agent {id} should have merge_status cleared after flush, got {ms:?}",
        );
    }

    // `trigger_after_merge` runs in `tokio::spawn` after the response,
    // so poll the DB for the row. 5s is far more than enough on every
    // platform we test on. The flush helper only fires CI on the FINAL
    // merge in the batch, so exactly one row should land.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut row_count: i64 = 0;
    while std::time::Instant::now() < deadline {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        row_count = conn
            .query_row("SELECT COUNT(*) FROM ci_runs", [], |r| r.get(0))
            .unwrap_or(0);
        if row_count >= 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        row_count, 1,
        "expected exactly one ci_runs row (only the final merge triggers CI)",
    );

    // The ci_runs row points at the LAST task (1.3) — that's the merge
    // that ran with `trigger_ci=true` inside `flush_deferred_merges`.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let task_number: String = conn
        .query_row(
            "SELECT task_number FROM ci_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        task_number, "1.3",
        "CI must be triggered on the FINAL merge in the batch, not the first",
    );
}

#[test]
fn flush_merges_with_zero_deferred_is_a_clean_noop() {
    let d = TestDashboard::new();
    d.create_plan(
        "flush-empty",
        &minimal_3task_plan("flush-empty", &d.project),
    );

    // No deferred agents seeded; not even a completed agent row exists.
    let (status, body) = d.post("/api/plans/flush-empty/flush-merges", json!({}));
    assert_eq!(status, 200, "expected 200, got {status}: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["count"], 0);
    assert_eq!(body["paused"], false);
    assert_eq!(body["ciTriggered"], false);
    assert!(body["merged"].as_array().unwrap().is_empty());
    let msg = body["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("No deferred merges"),
        "expected 'No deferred merges' in message, got: {msg}"
    );

    // No ci_runs row should have been inserted on a no-op flush.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ci_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_count, 0, "no-op flush should not insert ci_runs");
}

#[test]
fn flush_merges_is_idempotent_after_a_successful_flush() {
    // Acceptance criterion: "Idempotent — running it again with zero
    // deferred tasks is a no-op + clear response." This test runs the
    // first flush against a single deferred agent, then runs flush
    // again immediately and asserts the second call sees zero deferred
    // (the first flush cleared `merge_status`) and returns the no-op
    // shape.
    let d = TestDashboard::new();
    d.create_plan(
        "flush-twice",
        &minimal_3task_plan("flush-twice", &d.project),
    );
    d.setup_github_actions();
    d.setup_origin_remote();

    create_distinct_task_branch(&d, "branchwork/flush-twice/1.1", "alpha.txt");
    seed_deferred_agent(
        &d,
        "agent-once",
        "flush-twice",
        "1.1",
        "branchwork/flush-twice/1.1",
        "2026-05-19 11:00:00",
    );

    let (s1, b1) = d.post("/api/plans/flush-twice/flush-merges", json!({}));
    assert_eq!(s1, 200, "first flush should succeed: {b1}");
    assert_eq!(b1["count"], 1);
    assert_eq!(b1["paused"], false);

    // Second flush — agent's `merge_status` is NULL now, so the list
    // is empty and the call collapses to the no-op response shape.
    let (s2, b2) = d.post("/api/plans/flush-twice/flush-merges", json!({}));
    assert_eq!(s2, 200, "second flush should succeed: {b2}");
    assert_eq!(b2["count"], 0);
    assert_eq!(b2["paused"], false);
    assert_eq!(b2["ciTriggered"], false);
    assert!(b2["merged"].as_array().unwrap().is_empty());
    assert!(
        b2["message"].as_str().unwrap_or("").contains("No deferred"),
        "expected no-deferred message, got: {b2}"
    );
}
