//! End-to-end tests for `GET /api/health/state` (Task 2.3).
//!
//! Covers: ok-state report shape, wedged agent.status detection,
//! wedged task_status detection, stuck-checking detection, orphan
//! sweep telemetry, and CI dismissed counts.

mod support;

use serde_json::Value;
use support::TestDashboard;

fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

#[test]
fn health_state_reports_ok_for_a_clean_install() {
    let d = TestDashboard::new();
    let (s, body) = d.get("/api/health/state");
    assert_eq!(s, 200);
    assert_eq!(body["status"], "ok", "{body:?}");
    let warnings = body["warnings"].as_array().expect("warnings array");
    // The boot-sweep telemetry warning is the only non-data-driven
    // warning that could fire; on a fresh install the sweep ran during
    // server startup so `lastSweepAt` is set.
    assert!(
        warnings.is_empty(),
        "expected no warnings, got {warnings:?}",
    );
    assert!(
        body["categories"]["branches"]["lastSweepAt"].is_string(),
        "{body:?}",
    );
}

#[test]
fn health_state_flags_agent_with_unknown_status() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-a", &minimal_plan("plan-a", &d.project));

    // Seed an agents row with a nonsensical status — this is the
    // canonical "wedged state" the acceptance criterion calls out.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO agents (id, plan_name, task_id, cwd, status, started_at) \
         VALUES ('wedged-1', ?1, '1.1', '/tmp/scratch', 'haunted', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.get("/api/health/state");
    assert_eq!(s, 200);
    assert_eq!(body["status"], "warnings", "{body:?}");

    let warnings = body["warnings"].as_array().expect("warnings array");
    let agent_warning = warnings
        .iter()
        .find(|w| w["category"] == "agents" && w["kind"] == "unknown_status")
        .unwrap_or_else(|| panic!("expected agents.unknown_status warning, got {warnings:?}"));
    assert_eq!(agent_warning["count"], 1);
    let plans = agent_warning["affectedPlans"]
        .as_array()
        .expect("affectedPlans");
    assert!(plans.iter().any(|p| p == &Value::String(plan.clone())));
    assert_eq!(agent_warning["recovery"], "reset_task_status");

    // The byStatus map should include the bogus value too — the UI
    // can render the offending status verbatim.
    assert_eq!(body["categories"]["agents"]["byStatus"]["haunted"], 1);
    assert_eq!(body["categories"]["agents"]["unknownStatusCount"], 1);
}

#[test]
fn health_state_flags_task_status_with_unknown_value() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-b", &minimal_plan("plan-b", &d.project));

    // task_status doesn't have a CHECK constraint, so we can land a
    // bogus value via direct SQL exactly the way an operator with a
    // sqlite3 prompt could.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
         VALUES (?1, '1.1', 'wedged', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.get("/api/health/state");
    assert_eq!(s, 200);
    let warnings = body["warnings"].as_array().expect("warnings array");
    let task_warning = warnings
        .iter()
        .find(|w| w["category"] == "taskStatus" && w["kind"] == "unknown_status")
        .unwrap_or_else(|| panic!("expected taskStatus.unknown_status, got {warnings:?}"));
    assert_eq!(task_warning["count"], 1);
    let plans = task_warning["affectedPlans"]
        .as_array()
        .expect("affectedPlans");
    assert!(plans.iter().any(|p| p == &Value::String(plan.clone())));
}

#[test]
fn health_state_flags_stuck_checking_without_live_agent() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-c", &minimal_plan("plan-c", &d.project));

    // Seed a task_status row stuck in 'checking' with no live agent
    // pointed at it. This is the scenario `reconcile_task_statuses`
    // is supposed to clean up at boot, but the health endpoint also
    // surfaces it for runtime drift.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
         VALUES (?1, '1.1', 'checking', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.get("/api/health/state");
    assert_eq!(s, 200);

    let warnings = body["warnings"].as_array().expect("warnings array");
    let stuck = warnings
        .iter()
        .find(|w| w["category"] == "taskStatus" && w["kind"] == "stuck_checking")
        .unwrap_or_else(|| panic!("expected taskStatus.stuck_checking, got {warnings:?}"));
    assert_eq!(stuck["count"], 1);
    assert_eq!(stuck["recovery"], "reset_task_status");
    let plans = stuck["affectedPlans"]
        .as_array()
        .expect("affectedPlans array");
    assert!(plans.iter().any(|p| p == &Value::String(plan.clone())));
    assert_eq!(body["categories"]["taskStatus"]["stuckCheckingCount"], 1);
}

#[test]
fn health_state_does_not_flag_checking_with_live_agent() {
    // Defensive: the stuck-checking detector should NOT fire when a
    // running agent is attached — the task is mid-check, not wedged.
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-live", &minimal_plan("plan-live", &d.project));

    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
         VALUES (?1, '1.1', 'checking', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (id, plan_name, task_id, cwd, status, started_at) \
         VALUES ('live-checker', ?1, '1.1', '/tmp/scratch', 'running', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (_s, body) = d.get("/api/health/state");
    let warnings = body["warnings"].as_array().expect("warnings array");
    assert!(
        !warnings
            .iter()
            .any(|w| w["category"] == "taskStatus" && w["kind"] == "stuck_checking"),
        "stuck-checking warning fired despite a live agent: {warnings:?}",
    );
}

#[test]
fn health_state_includes_orphan_sweep_telemetry() {
    let d = TestDashboard::new();
    let (_s, body) = d.get("/api/health/state");
    // Boot sweep ran during startup, so timestamp + cleared count
    // should both be populated. Cleared is 0 in a fresh install (no
    // dangling branches) and that is the correct steady state.
    assert!(body["categories"]["branches"]["lastSweepAt"].is_string());
    assert!(body["categories"]["branches"]["secondsSinceLastSweep"].is_i64());
    assert_eq!(body["categories"]["branches"]["lastSweepClearedCount"], 0);
}

#[test]
fn health_state_counts_dismissed_ci_runs() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-ci", &minimal_plan("plan-ci", &d.project));

    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, conclusion, dismissed_at) \
         VALUES (?1, '1.1', 'completed', 'failure', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, conclusion) \
         VALUES (?1, '1.1', 'completed', 'success')",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (_s, body) = d.get("/api/health/state");
    assert_eq!(body["categories"]["ciRuns"]["totalCount"], 2);
    assert_eq!(body["categories"]["ciRuns"]["dismissedCount"], 1);
}
