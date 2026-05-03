//! E2E tests for `GET/PUT /api/plans/:name/config` (Task 0.6).
//!
//! Covers:
//! - GET defaults (no rows yet): autoAdvance=false, autoMode=false,
//!   maxFixAttempts=3, pausedReason=null.
//! - PUT autoMode + maxFixAttempts and read back.
//! - Partial PUT preserves the unspecified column (no clobber to default).
//! - PUT autoAdvance via the unified endpoint matches the dedicated
//!   `/auto-advance` route (existing wire shape unchanged).
//! - GET surfaces pausedReason once the loop self-pauses (simulated by
//!   writing the row directly via SQLite, since the loop landings come in
//!   later phases).

mod support;

use rusqlite::params;
use serde_json::json;
use support::TestDashboard;

fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

#[test]
fn get_config_defaults_when_no_rows_exist() {
    let d = TestDashboard::new();
    d.create_plan("cfg-defaults", &minimal_plan("cfg-defaults", &d.project));

    let (status, body) = d.get("/api/plans/cfg-defaults/config");
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(body["autoAdvance"], false);
    assert_eq!(body["autoMode"], false);
    assert_eq!(body["maxFixAttempts"], 3);
    assert!(body["pausedReason"].is_null(), "got: {body}");
}

#[test]
fn put_auto_mode_and_max_fix_attempts_round_trips() {
    let d = TestDashboard::new();
    d.create_plan("cfg-rt", &minimal_plan("cfg-rt", &d.project));

    let (s, body) = d.put(
        "/api/plans/cfg-rt/config",
        json!({"autoMode": true, "maxFixAttempts": 7}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoMode"], true);
    assert_eq!(body["maxFixAttempts"], 7);

    // Re-read via GET to confirm persistence.
    let (s, body) = d.get("/api/plans/cfg-rt/config");
    assert_eq!(s, 200);
    assert_eq!(body["autoMode"], true);
    assert_eq!(body["maxFixAttempts"], 7);
    assert_eq!(body["autoAdvance"], false, "auto_advance must stay default");
    assert!(body["pausedReason"].is_null());
}

#[test]
fn put_partial_preserves_unspecified_columns() {
    let d = TestDashboard::new();
    d.create_plan("cfg-partial", &minimal_plan("cfg-partial", &d.project));

    // Set both fields first.
    let (s, _) = d.put(
        "/api/plans/cfg-partial/config",
        json!({"autoMode": true, "maxFixAttempts": 5}),
    );
    assert_eq!(s, 200);

    // PUT only maxFixAttempts; autoMode must NOT flip back to false.
    let (s, body) = d.put(
        "/api/plans/cfg-partial/config",
        json!({"maxFixAttempts": 9}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoMode"], true, "auto_mode clobbered: {body}");
    assert_eq!(body["maxFixAttempts"], 9);

    // PUT only autoMode=false; max stays at 9.
    let (s, body) = d.put("/api/plans/cfg-partial/config", json!({"autoMode": false}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoMode"], false);
    assert_eq!(body["maxFixAttempts"], 9, "max clobbered: {body}");
}

#[test]
fn put_auto_advance_via_config_matches_dedicated_endpoint() {
    let d = TestDashboard::new();
    d.create_plan("cfg-aa", &minimal_plan("cfg-aa", &d.project));

    // Existing `/auto-advance` endpoint still works (acceptance criterion).
    let (s, body) = d.put("/api/plans/cfg-aa/auto-advance", json!({"enabled": true}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoAdvance"], true);

    // The unified GET sees the same value.
    let (s, body) = d.get("/api/plans/cfg-aa/config");
    assert_eq!(s, 200);
    assert_eq!(body["autoAdvance"], true);

    // Flipping it back via the unified PUT also works.
    let (s, body) = d.put("/api/plans/cfg-aa/config", json!({"autoAdvance": false}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoAdvance"], false);
}

#[test]
fn get_surfaces_paused_reason_when_set() {
    let d = TestDashboard::new();
    d.create_plan("cfg-paused", &minimal_plan("cfg-paused", &d.project));

    // Opt-in first.
    let (s, _) = d.put("/api/plans/cfg-paused/config", json!({"autoMode": true}));
    assert_eq!(s, 200);

    // Simulate the loop self-pausing by writing directly to SQLite — the
    // loop helpers land in later phases. The row already exists from the
    // PUT above so this is an UPDATE.
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = ?1, paused_at = datetime('now') \
         WHERE plan_name = ?2",
        params!["merge_conflict", "cfg-paused"],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.get("/api/plans/cfg-paused/config");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["pausedReason"], "merge_conflict");
    // Enabled flag is independent of paused state — the UI uses both to
    // distinguish "user opted out" from "loop self-paused".
    assert_eq!(body["autoMode"], true);
}
