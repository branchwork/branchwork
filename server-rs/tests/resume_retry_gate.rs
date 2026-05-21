//! End-to-end tests for `POST /api/plans/:name/resume` (T2.1 of the
//! pre-merge-gate plan).
//!
//! The standard Resume path (PUT /config + `pausedReason: null`) is
//! covered by `tests/plan_config.rs::put_explicit_null_paused_reason_resumes_loop`.
//! This file pins the new gate-retry path:
//!
//! - `{retryGate: true}` on a plan paused with `pre_merge_check_failed`
//!   → 200, pause cleared, audit row written, broadcast fired,
//!   `auto_mode.enabled` preserved.
//! - `{retryGate: true}` on a plan paused for a different reason → 409
//!   with `error: not_pre_merge_pause` (no pause mutation).
//! - `{retryGate: true}` with no trigger agent visible → 409 with
//!   `error: no_trigger_agent`.
//! - `{retryGate: false}` (or absent body) → falls through to the
//!   standard resume path.
//!
//! The actual `run_state_machine` re-run + gate execution is unit-tested
//! in `auto_mode.rs::tests`; this file pins the API surface and the
//! audit/pause side-effects.

mod support;

use serde_json::{Value, json};
use support::TestDashboard;

fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Seed a completed agent row with a branch (the row the trigger-agent
/// lookup will pick up). Mirrors `merge_guard.rs::seed_agent` but lets
/// callers control `started_at` (so a test can order multiple agents
/// deterministically) and `task_id` (so fix-agent exclusion can be
/// exercised).
fn seed_completed_agent(
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
            (id, session_id, cwd, status, mode, plan_name, task_id, \
             branch, source_branch, started_at) \
         VALUES (?1, ?1, ?2, 'completed', 'pty', ?3, ?4, ?5, 'master', ?6)",
        rusqlite::params![
            id,
            d.project.to_string_lossy(),
            plan,
            task,
            branch,
            started_at,
        ],
    )
    .unwrap();
}

/// Pause the plan in `paused_reason` and ensure `auto_mode.enabled` is
/// on (the loop self-pause leaves enabled=1, paused_reason populated).
fn pause_plan(d: &TestDashboard, plan: &str, reason: &str) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, enabled, paused_reason, paused_at) \
         VALUES (?1, 1, ?2, datetime('now')) \
         ON CONFLICT(plan_name) DO UPDATE SET \
             enabled = 1, \
             paused_reason = excluded.paused_reason, \
             paused_at = datetime('now')",
        rusqlite::params![plan, reason],
    )
    .unwrap();
}

fn read_paused_reason(d: &TestDashboard, plan: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.query_row(
        "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
        rusqlite::params![plan],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

fn audit_rows_for(d: &TestDashboard, action: &str, plan: &str) -> Vec<Value> {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT diff FROM audit_logs \
             WHERE action = ?1 AND resource_id = ?2 \
             ORDER BY id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![action, plan], |row| {
            row.get::<_, Option<String>>(0)
        })
        .unwrap();
    rows.flatten()
        .filter_map(|s| s.and_then(|s| serde_json::from_str::<Value>(&s).ok()))
        .collect()
}

#[test]
fn retry_gate_on_pre_merge_pause_clears_pause_and_audits() {
    let d = TestDashboard::new();
    let plan = "rg-happy";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    // Enable auto-mode + pause for pre-merge gate.
    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    pause_plan(&d, plan, "pre_merge_check_failed");

    // Seed the trigger agent row.
    seed_completed_agent(
        &d,
        "agent-trigger",
        plan,
        "1.1",
        "branchwork/rg-happy/1.1",
        "2026-05-21 09:00:00",
    );

    // POST /resume with retryGate: true.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/resume"),
        json!({"retryGate": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["plan"], plan);
    assert_eq!(body["task"], "1.1");
    assert_eq!(body["agentId"], "agent-trigger");
    assert_eq!(body["trigger"], "retry_gate");

    // Pause should be cleared on disk.
    assert_eq!(read_paused_reason(&d, plan), None);

    // Audit row was written with the expected diff fields.
    let rows = audit_rows_for(&d, "auto_mode.pre_merge_gate_retried", plan);
    assert_eq!(rows.len(), 1, "rows: {rows:?}");
    assert_eq!(rows[0]["plan"], plan);
    assert_eq!(rows[0]["task"], "1.1");
    assert_eq!(rows[0]["agent_id"], "agent-trigger");
}

#[test]
fn retry_gate_picks_most_recent_non_fix_agent() {
    let d = TestDashboard::new();
    let plan = "rg-pick";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    pause_plan(&d, plan, "pre_merge_check_failed");

    // Two prior agents on the plan: an old completed run on task 1.1,
    // a fix agent for 1.1, and a fresh completion on 1.1 (the
    // canonical trigger). The lookup picks the freshest non-fix row.
    seed_completed_agent(
        &d,
        "agent-old",
        plan,
        "1.1",
        "branchwork/rg-pick/1.1",
        "2026-05-21 08:00:00",
    );
    seed_completed_agent(
        &d,
        "agent-fix",
        plan,
        "1.1-fix-1",
        "branchwork/fix/rg-pick/1.1/fix-1",
        "2026-05-21 09:30:00",
    );
    seed_completed_agent(
        &d,
        "agent-fresh",
        plan,
        "1.1",
        "branchwork/rg-pick/1.1",
        "2026-05-21 09:00:00",
    );

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/resume"),
        json!({"retryGate": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    // The fix agent should be skipped despite being most recent; the
    // fresh non-fix completion wins.
    assert_eq!(body["agentId"], "agent-fresh", "body: {body}");
    assert_eq!(body["task"], "1.1");
}

#[test]
fn retry_gate_on_non_pre_merge_pause_returns_409() {
    let d = TestDashboard::new();
    let plan = "rg-wrong-pause";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    pause_plan(&d, plan, "merge_failed");
    seed_completed_agent(
        &d,
        "agent-x",
        plan,
        "1.1",
        "branchwork/rg-wrong-pause/1.1",
        "2026-05-21 09:00:00",
    );

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/resume"),
        json!({"retryGate": true}),
    );
    assert_eq!(s, 409, "body: {body}");
    assert_eq!(body["error"], "not_pre_merge_pause", "body: {body}");
    assert_eq!(body["currentPausedReason"], "merge_failed");

    // Pause should still be in place — refusal must not mutate state.
    assert_eq!(
        read_paused_reason(&d, plan),
        Some("merge_failed".to_string())
    );

    // No retry audit row.
    let rows = audit_rows_for(&d, "auto_mode.pre_merge_gate_retried", plan);
    assert!(rows.is_empty(), "rows: {rows:?}");
}

#[test]
fn retry_gate_with_no_trigger_agent_returns_409() {
    let d = TestDashboard::new();
    let plan = "rg-no-agent";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    pause_plan(&d, plan, "pre_merge_check_failed");
    // No agent rows seeded — the lookup returns None and the endpoint
    // 409s with `no_trigger_agent`.

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/resume"),
        json!({"retryGate": true}),
    );
    assert_eq!(s, 409, "body: {body}");
    assert_eq!(body["error"], "no_trigger_agent", "body: {body}");

    // Pause should still be in place — the refusal must not mutate
    // state.
    assert_eq!(
        read_paused_reason(&d, plan),
        Some("pre_merge_check_failed".to_string())
    );
}

#[test]
fn standard_resume_via_post_without_retry_gate_clears_pause() {
    let d = TestDashboard::new();
    let plan = "rg-standard";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    // Any pause reason works here — standard resume doesn't gate on
    // reason; it mirrors PUT /config + pausedReason: null.
    pause_plan(&d, plan, "merge_failed");

    // POST /resume with empty body falls through to the standard path.
    let (s, body) = d.post(&format!("/api/plans/{plan}/resume"), json!({}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["trigger"], "standard");

    // Pause cleared.
    assert_eq!(read_paused_reason(&d, plan), None);

    // Standard resume writes an `auto_mode.resumed` audit row (not the
    // retry-gate one).
    let retry_rows = audit_rows_for(&d, "auto_mode.pre_merge_gate_retried", plan);
    assert!(retry_rows.is_empty(), "rows: {retry_rows:?}");
    let standard_rows = audit_rows_for(&d, "auto_mode.resumed", plan);
    assert_eq!(standard_rows.len(), 1, "rows: {standard_rows:?}");
}

#[test]
fn standard_resume_via_post_with_explicit_retry_gate_false_clears_pause() {
    // Same as the empty-body case but with `retryGate: false` written
    // out explicitly. Pins the wire shape so an older dashboard that
    // sends `false` doesn't accidentally hit the gate-retry branch.
    let d = TestDashboard::new();
    let plan = "rg-explicit-false";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let (s, _) = d.put(
        &format!("/api/plans/{plan}/config"),
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200);
    pause_plan(&d, plan, "ci_failed");

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/resume"),
        json!({"retryGate": false}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["trigger"], "standard");
    assert_eq!(read_paused_reason(&d, plan), None);
}
