//! E2E tests for T4.2 — HTTP dispatch refuses to send agents to a
//! red-drift runner.
//!
//! Pins the on-wire contract that the dashboard banners (T4.1) + Start
//! button gating depend on:
//!   - status code: **409 Conflict**
//!   - body field `error` / `code`: literal **`runner_version_blocked`**
//!   - body field `upgradeUrl`: literal **`"/runners"`** (the Upgrade
//!     runner button — T4.1 — lives there)
//!   - body fields `runnerId` / `runnerVersion` / `serverVersion` echo
//!     the dispatcher's resolution so the UI can render the offending
//!     runner without a follow-up query
//!
//! Also pins the operator escape hatch from T1.3: flipping the
//! `runners.version_mismatch_override` column lifts the gate.

mod support;

use rusqlite::params;
use support::TestDashboard;

/// Mirror of merge_guard.rs::minimal_plan — absolute project path so the
/// resolver finds the harness scratch repo regardless of how
/// `dirs::home_dir()` resolves on the host.
fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Acceptance: POST /api/actions/start-task against a red-drift runner
/// returns 409 with the version-blocked body shape — the body the
/// dashboard toast renders into an "Upgrade runner" link.
#[test]
fn start_task_dispatch_to_red_runner_returns_409_runner_version_blocked() {
    let d = TestDashboard::new();
    let db_path = d.dir.path().join(".claude/branchwork.db");

    // Seed a v0.3.0 runner (pre-1.0 minor diff ⇒ Severity::Red against
    // any 0.x.y server build).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at, version) \
             VALUES (?1, 'old', 'default-org', 'online', datetime('now'), ?2)",
            params!["runner-red", "0.3.0"],
        )
        .unwrap();
    }

    let plan_name = d.create_plan("drift-test", &minimal_plan("drift-test", &d.project));

    let (status, body) = d.post(
        "/api/actions/start-task",
        serde_json::json!({
            "planName": plan_name,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );

    assert_eq!(status, 409, "expected 409 Conflict, body: {body}");
    assert_eq!(
        body["error"].as_str(),
        Some("runner_version_blocked"),
        "body: {body}"
    );
    assert_eq!(
        body["code"].as_str(),
        Some("runner_version_blocked"),
        "body: {body}"
    );
    assert_eq!(
        body["runnerId"].as_str(),
        Some("runner-red"),
        "body: {body}"
    );
    assert_eq!(
        body["runnerVersion"].as_str(),
        Some("0.3.0"),
        "body: {body}"
    );
    assert!(
        body["serverVersion"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "serverVersion must be populated, body: {body}"
    );
    assert_eq!(
        body["upgradeUrl"].as_str(),
        Some("/runners"),
        "body: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|s| s.contains("upgrade")),
        "message must mention upgrade, body: {body}"
    );
}

/// Side-effect contract: a refused dispatch must NOT insert an agent
/// row or pause the plan. The Start button is a user-facing click — we
/// want a clean 409, not a phantom failed agent.
#[test]
fn refused_dispatch_inserts_no_agent_row_and_does_not_pause_plan() {
    let d = TestDashboard::new();
    let db_path = d.dir.path().join(".claude/branchwork.db");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at, version) \
             VALUES (?1, 'old', 'default-org', 'online', datetime('now'), ?2)",
            params!["runner-red", "0.3.0"],
        )
        .unwrap();
    }

    let plan_name = d.create_plan(
        "drift-side-effects",
        &minimal_plan("drift-side-effects", &d.project),
    );

    let (status, _body) = d.post(
        "/api/actions/start-task",
        serde_json::json!({
            "planName": plan_name,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(status, 409);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let agent_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE plan_name = ?1",
            params![plan_name],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        agent_count, 0,
        "refused dispatch must not insert agent rows"
    );

    let paused: Option<String> = conn
        .query_row(
            "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
            params![plan_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    assert!(
        paused.is_none(),
        "refused dispatch must not pause the plan: paused_reason={paused:?}"
    );
}

/// Operator override (T1.3): flipping `version_mismatch_override = 1`
/// lifts the gate. The 409 must NOT fire — the dispatcher proceeds
/// (and either succeeds or fails for downstream reasons, neither of
/// which is the version block).
#[test]
fn override_lifts_the_409_gate() {
    let d = TestDashboard::new();
    let db_path = d.dir.path().join(".claude/branchwork.db");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at, version, version_mismatch_override) \
             VALUES (?1, 'old', 'default-org', 'online', datetime('now'), ?2, 1)",
            params!["runner-red", "0.3.0"],
        )
        .unwrap();
    }

    let plan_name = d.create_plan(
        "drift-override",
        &minimal_plan("drift-override", &d.project),
    );

    let (status, body) = d.post(
        "/api/actions/start-task",
        serde_json::json!({
            "planName": plan_name,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_ne!(status, 409, "override must lift the 409 — but got: {body}");
}

/// Standalone mode (no runners): the 409 gate must never fire because
/// there's no runner to compare to. Sanity test ensuring the helper's
/// `org_has_runner` short-circuit holds at the HTTP layer.
#[test]
fn standalone_mode_never_returns_409_runner_version_blocked() {
    let d = TestDashboard::new();
    // No runners row inserted — org_has_runner == false.

    let plan_name = d.create_plan(
        "drift-standalone",
        &minimal_plan("drift-standalone", &d.project),
    );

    let (status, body) = d.post(
        "/api/actions/start-task",
        serde_json::json!({
            "planName": plan_name,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_ne!(
        status, 409,
        "standalone must not fire the version-block gate, body: {body}"
    );
}
