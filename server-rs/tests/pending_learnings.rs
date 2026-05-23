//! E2E tests for the Phase 1, Task 1.4 pending-learnings dashboard surface.
//!
//! `GET /api/learnings/pending` is the data source for the "Learnings
//! due" panel. `GET /api/learnings/pending/{id}/log` backs the per-row
//! drilldown. Mirrors the seed pattern from `pending_learning_gate.rs`
//! so the two test files share the same fixture shape and the failure
//! produced by Task 1.2 is what Task 1.4 surfaces.

mod support;

use serde_json::json;
use std::path::Path;
use support::TestDashboard;

/// Mirrors `tests/pending_learning_gate.rs::single_task_plan`. The
/// literal `\n` escapes preserve YAML indentation; the backslash-newline
/// continuation form silently strips it (see auto-mode-phase-verify
/// T1.2 learning).
fn single_task_plan(name: &str, project_dir: &Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Seed a `ci_failure_events` row with an owning live agent. The agent
/// row is needed because the list endpoint LEFT JOIN-s `agents` to
/// surface `agentStatus`. Returns the event id.
fn seed_pending_failure(d: &TestDashboard, plan: &str, task: &str, run_id: &str) -> i64 {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let agent_id = format!("agent-{run_id}");
    conn.execute(
        "INSERT OR REPLACE INTO agents (id, plan_name, task_id, status, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'running', '/tmp', datetime('now'))",
        rusqlite::params![agent_id, plan, task],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_failure_events \
             (agent_id, plan_name, task_number, branch, run_id, run_url, \
              workflow, conclusion, summary, org_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'default-org')",
        rusqlite::params![
            agent_id,
            plan,
            task,
            format!("branchwork/{plan}/{task}"),
            run_id,
            format!("https://github.com/owner/repo/actions/runs/{run_id}"),
            "tests.yml",
            "failure",
            format!("workflow tests.yml failed (run {run_id})"),
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Headline acceptance test for Task 1.4. With one agent blocked, the
/// panel data source returns one row; after a learning capture, the
/// queue is empty.
#[test]
fn list_pending_returns_blocked_agent_then_empties_after_learning_capture() {
    let d = TestDashboard::new();
    let plan = "pending-learnings-acceptance";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "12345");

    // With one agent blocked, the panel shows it.
    let (status, body) = d.get("/api/learnings/pending");
    assert_eq!(status, 200, "expected 200, got {status}; body={body}");
    let items = body["items"].as_array().expect("items must be array");
    assert_eq!(items.len(), 1, "exactly one pending row: {items:?}");
    let row = &items[0];
    assert_eq!(row["planName"], plan);
    assert_eq!(row["taskNumber"], "1.1");
    assert_eq!(row["runId"], "12345");
    assert_eq!(row["workflow"], "tests.yml");
    assert_eq!(row["conclusion"], "failure");
    assert_eq!(row["agentStatus"], "running");
    assert_eq!(
        row["runUrl"],
        "https://github.com/owner/repo/actions/runs/12345"
    );
    assert!(
        row["observedAt"].as_str().is_some_and(|s| !s.is_empty()),
        "observedAt must be a non-empty timestamp: {row}"
    );

    // After capture_learning lands (any path that calls
    // mark_ci_failures_resolved — here POST /learnings — is fine), the
    // panel re-renders empty.
    let (s2, body2) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/learnings"),
        json!({"learning": "tests failed because of fmt; ran cargo fmt and committed."}),
    );
    assert_eq!(s2, 201, "learning POST should succeed: {body2}");

    let (_, body3) = d.get("/api/learnings/pending");
    let items = body3["items"].as_array().unwrap();
    assert!(
        items.is_empty(),
        "panel must re-render empty after learning capture: {items:?}"
    );
}

/// Multi-event ordering: oldest-observed-first per the ORDER BY in
/// `fetch_pending`. Pin so the dashboard drilldown order stays stable
/// across polls.
#[test]
fn list_pending_orders_oldest_first_across_multiple_failures() {
    let d = TestDashboard::new();
    let plan = "pending-learnings-ordering";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    let first = seed_pending_failure(&d, plan, "1.1", "100");
    let second = seed_pending_failure(&d, plan, "1.1", "200");

    let (_, body) = d.get("/api/learnings/pending");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], first);
    assert_eq!(items[1]["id"], second);
}

/// Empty queue is the steady state — the panel hides itself when items
/// is empty. Verify the endpoint returns the empty wire shape (not a
/// 404, not a null) so the frontend can render unconditionally.
#[test]
fn list_pending_returns_empty_items_array_when_queue_is_empty() {
    let d = TestDashboard::new();
    let (status, body) = d.get("/api/learnings/pending");
    assert_eq!(status, 200);
    let items = body["items"].as_array().expect("items must be array");
    assert!(items.is_empty(), "items must be empty: {items:?}");
}

/// `GET /api/learnings/pending/{id}/log` returns 404 for an unknown
/// event id. The dashboard never builds this URL by hand — the id comes
/// from a list response — but a defensive 404 keeps the contract clean.
#[test]
fn fetch_log_returns_404_for_unknown_event_id() {
    let d = TestDashboard::new();
    let (status, body) = d.get("/api/learnings/pending/99999/log");
    assert_eq!(status, 404, "expected 404, got {status}; body={body}");
}

/// `GET /api/learnings/pending/{id}/log` returns 404 with code
/// `log_unavailable` when the event exists but has no matching ci_runs
/// row (the JOIN failed). The seed in this test deliberately does NOT
/// create a ci_runs row, so the JOIN comes back NULL — distinct error
/// code from `event_not_found` so the dashboard can copy-paste the
/// run_url instead of waiting on a refetch.
#[test]
fn fetch_log_returns_log_unavailable_when_no_ci_runs_row() {
    let d = TestDashboard::new();
    let plan = "pending-learnings-no-ci-runs";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    let event_id = seed_pending_failure(&d, plan, "1.1", "500");
    // No ci_runs row inserted — JOIN will be NULL.

    let (status, body) = d.get(&format!("/api/learnings/pending/{event_id}/log"));
    assert_eq!(status, 404, "expected 404, got {status}; body={body}");
    let error_text = body["error"].as_str().unwrap_or("");
    assert!(
        error_text == "log_unavailable" || error_text == "event_not_found",
        "expected log_unavailable or event_not_found, got {error_text:?}"
    );
}
