//! E2E tests for the Phase 1, Task 1.2 pending-learning gate.
//!
//! Acceptance criterion (from the plan brief):
//!
//! > With the pending flag set, calling `mcp__branchwork__update_task_status`
//! > on the agent's task returns 409 with `code=pending_learning` and a
//! > payload pointing at the unresolved CI failure.
//!
//! Branchwork's MCP `update_task_status` and HTTP `PUT
//! /api/plans/:name/tasks/:num/status` share the same gate logic (both call
//! `db::pending_ci_failures` then map matches to 409 / McpError). This file
//! pins the HTTP surface end-to-end (MCP path is covered by unit tests in
//! `mcp/tools/status.rs`); the gate response shape, idempotence on retry,
//! and the resolve-on-learning side effect are all exercised against a real
//! spawned server.

mod support;

use serde_json::json;
use std::path::Path;
use support::TestDashboard;

/// Mirrors `tests/dirty_tree.rs::single_task_plan`. NB: literal `\n`
/// escapes preserve YAML indentation; the backslash-newline continuation
/// form silently strips it. (Replayed gotcha — see auto-mode-phase-verify
/// T1.2 learning.)
fn single_task_plan(name: &str, project_dir: &Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Seed a `ci_failure_events` row directly into the server's SQLite DB so
/// the test does not need a live runner / GitHub Actions wiring. Mirrors
/// the production T1.1 INSERT shape exactly (`resolved_at` defaults to
/// NULL → pending). Returns the row id.
fn seed_pending_failure(d: &TestDashboard, plan: &str, task: &str, run_id: &str) -> i64 {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_failure_events \
             (agent_id, plan_name, task_number, branch, run_id, \
              run_url, workflow, conclusion, summary, org_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'default-org')",
        rusqlite::params![
            format!("agent-{}", run_id),
            plan,
            task,
            format!("branchwork/{}/{}", plan, task),
            run_id,
            format!("https://github.com/owner/repo/actions/runs/{}", run_id),
            "tests.yml",
            "failure",
            format!("workflow tests.yml failed (run {})", run_id),
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Headline acceptance test: with a pending row in place, calling PUT
/// status=completed returns 409 with the structured pending_learning
/// payload pointing at the unresolved row.
#[test]
fn http_set_status_completed_returns_409_pending_learning_with_failure_payload() {
    let d = TestDashboard::new();
    let plan = "pending-learning-acceptance";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "12345");

    let (status, body) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );

    assert_eq!(status, 409, "expected 409, got {status}; body={body}");
    assert_eq!(body["error"], "pending_learning", "{body}");
    assert_eq!(body["code"], "pending_learning", "{body}");
    let message = body["message"].as_str().unwrap_or("");
    assert!(
        message.contains("pending-learning-acceptance/tasks/1.1/learnings"),
        "message must point at the learnings endpoint: {message:?}"
    );
    let failures = body["failures"].as_array().expect("failures must be array");
    assert_eq!(failures.len(), 1, "exactly one pending row: {failures:?}");
    let f = &failures[0];
    assert_eq!(f["runId"], "12345");
    assert_eq!(f["workflow"], "tests.yml");
    assert_eq!(f["conclusion"], "failure");
    assert_eq!(
        f["runUrl"],
        "https://github.com/owner/repo/actions/runs/12345"
    );
    assert!(
        f["observedAt"].as_str().is_some_and(|s| !s.is_empty()),
        "observedAt must be a non-empty timestamp: {f}"
    );

    // task_status must NOT have been written — the gate fires before
    // the INSERT. /statuses returns an empty array (no row exists yet).
    let (_, statuses) = d.get(&format!("/api/plans/{plan}/statuses"));
    let rows = statuses.as_array().unwrap();
    assert!(
        rows.iter().all(|r| r["task_number"] != "1.1"),
        "no task_status row should exist post-409: {statuses}"
    );
}

/// Non-completed statuses are allowed through — the gate only fires for
/// `completed`. Mirrors the dirty-tree gate scope (see T5.2 learning of
/// `dirty-tree-check-stop-pausing-on-write-noise`).
#[test]
fn http_set_status_non_completed_bypasses_pending_learning_gate() {
    let d = TestDashboard::new();
    let plan = "pending-learning-non-completed";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "200");

    for status_value in ["in_progress", "failed", "skipped", "pending", "checking"] {
        let (s, body) = d.put(
            &format!("/api/plans/{plan}/tasks/1.1/status"),
            json!({"status": status_value}),
        );
        assert_eq!(
            s, 200,
            "status={status_value} should bypass the gate; got {s} body={body}"
        );
    }
}

/// Capturing a learning resolves the gate so the next
/// `update_task_status(completed)` succeeds. The brief calls this out
/// explicitly: the existing learning-write path is the natural hand-off
/// point for clearing pending_learning.
#[test]
fn capturing_a_learning_resolves_the_gate_and_completed_succeeds() {
    let d = TestDashboard::new();
    let plan = "pending-learning-resolve-via-learning";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "300");

    // First attempt: 409.
    let (s1, _) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s1, 409);

    // Capture a learning via the existing endpoint.
    let (s2, body2) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/learnings"),
        json!({"learning": "CI tripped because of fmt; ran cargo fmt and committed."}),
    );
    assert_eq!(s2, 201, "learning POST should succeed: {body2}");
    assert_eq!(
        body2["resolvedCiFailures"], 1,
        "learning POST should have resolved the pending row"
    );

    // Retry: 200 OK.
    let (s3, body3) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s3, 200, "post-learning retry should succeed: {body3}");
    assert_eq!(body3["status"], "completed");
}

/// Subsequent learnings on a task with no pending rows are a no-op: the
/// resolve helper is idempotent. Doubles as a regression check that the
/// resolve hook does not spuriously bump rows the operator never had.
#[test]
fn second_learning_after_resolve_does_not_falsely_resolve_anything() {
    let d = TestDashboard::new();
    let plan = "pending-learning-idempotent-resolve";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "400");

    let (_, body1) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/learnings"),
        json!({"learning": "first learning"}),
    );
    assert_eq!(body1["resolvedCiFailures"], 1);

    let (_, body2) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/learnings"),
        json!({"learning": "second learning"}),
    );
    assert_eq!(
        body2["resolvedCiFailures"], 0,
        "no pending rows left after first learning; second resolve must be a no-op"
    );
}

/// Cross-task isolation: a pending failure on task X must NOT block
/// completion of task Y in the same plan. Pins the partial-index lookup
/// is keyed on (plan_name, task_number), not plan alone.
#[test]
fn pending_failure_on_one_task_does_not_block_a_sibling_task() {
    // Two-task plan: 1.1 has a pending failure, 1.2 does not.
    let d = TestDashboard::new();
    let plan = "pending-learning-cross-task";
    let yaml = format!(
        "title: {plan}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n      - number: '1.2'\n        title: Task 1.2\n        description: ''\n        acceptance: ''\n",
        plan = plan,
        project = d.project.display()
    );
    d.create_plan(plan, &yaml);
    seed_pending_failure(&d, plan, "1.1", "500");

    // 1.2: no pending row → completed succeeds.
    let (s, body) = d.put(
        &format!("/api/plans/{plan}/tasks/1.2/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s, 200, "sibling task must complete: {body}");

    // 1.1 still blocked.
    let (s, body) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s, 409, "1.1 with pending row should still block: {body}");
}

/// Multiple pending rows on the same task are all surfaced in `failures`
/// (so the dashboard can render them as a list) and a single learning
/// resolves all of them.
#[test]
fn multiple_pending_failures_surface_as_array_and_resolve_in_one_shot() {
    let d = TestDashboard::new();
    let plan = "pending-learning-multi";
    d.create_plan(plan, &single_task_plan(plan, &d.project));
    seed_pending_failure(&d, plan, "1.1", "601");
    seed_pending_failure(&d, plan, "1.1", "602");
    seed_pending_failure(&d, plan, "1.1", "603");

    let (s, body) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s, 409);
    let failures = body["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 3, "all 3 pending rows surfaced");

    // One learning clears all of them.
    let (_, learning_body) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/learnings"),
        json!({"learning": "all three runs failed on the same fmt issue"}),
    );
    assert_eq!(learning_body["resolvedCiFailures"], 3);

    let (s2, _) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s2, 200, "retry succeeds after one learning resolves all");
}

/// A pre-resolved row (e.g. from a previous learning) must NOT trip the
/// gate. Pins the `resolved_at IS NULL` filter is in fact applied.
#[test]
fn pre_resolved_failure_does_not_trip_the_gate() {
    let d = TestDashboard::new();
    let plan = "pending-learning-pre-resolved";
    d.create_plan(plan, &single_task_plan(plan, &d.project));

    // Seed a row and immediately resolve it via direct SQL.
    seed_pending_failure(&d, plan, "1.1", "700");
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE ci_failure_events SET resolved_at = datetime('now') \
         WHERE plan_name = ?1 AND task_number = ?2",
        rusqlite::params![plan, "1.1"],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(s, 200, "pre-resolved row must not block completion: {body}");
}
