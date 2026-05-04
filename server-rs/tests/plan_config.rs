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
fn put_explicit_null_paused_reason_resumes_loop() {
    let d = TestDashboard::new();
    d.create_plan("cfg-resume", &minimal_plan("cfg-resume", &d.project));

    // Opt-in + simulate the loop self-pausing (the loop helpers wire later in
    // the plan but the column is the source of truth either way).
    let (s, _) = d.put("/api/plans/cfg-resume/config", json!({"autoMode": true}));
    assert_eq!(s, 200);

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = ?1, paused_at = datetime('now') \
         WHERE plan_name = ?2",
        params!["merge_conflict", "cfg-resume"],
    )
    .unwrap();
    drop(conn);

    // PUT with explicit null clears the pause and the response reflects it.
    let (s, body) = d.put(
        "/api/plans/cfg-resume/config",
        json!({"pausedReason": null}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert!(body["pausedReason"].is_null(), "got: {body}");
    // autoMode is left intact — only the pause flag is cleared.
    assert_eq!(body["autoMode"], true);
}

#[test]
fn put_paused_reason_with_non_null_value_is_ignored() {
    let d = TestDashboard::new();
    d.create_plan("cfg-pr-ignore", &minimal_plan("cfg-pr-ignore", &d.project));

    let (s, _) = d.put("/api/plans/cfg-pr-ignore/config", json!({"autoMode": true}));
    assert_eq!(s, 200);

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = ?1, paused_at = datetime('now') \
         WHERE plan_name = ?2",
        params!["merge_conflict", "cfg-pr-ignore"],
    )
    .unwrap();
    drop(conn);

    // Sending a non-null value is silently ignored — the loop is the only
    // writer of paused reasons.
    let (s, body) = d.put(
        "/api/plans/cfg-pr-ignore/config",
        json!({"pausedReason": "user_set_reason"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["pausedReason"], "merge_conflict", "got: {body}");
}

#[test]
fn parallel_defaults_to_false_in_get_response() {
    // Acceptance: default is false for plans with no auto-mode/auto-advance
    // rows yet AND for plans where rows exist but the column was never set.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-par-default",
        &minimal_plan("cfg-par-default", &d.project),
    );

    let (s, body) = d.get("/api/plans/cfg-par-default/config");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], false, "default-off without rows: {body}");

    // Toggle some other field so the rows exist with their default parallel=0.
    let (s, body) = d.put(
        "/api/plans/cfg-par-default/config",
        json!({"autoMode": true, "autoAdvance": true}),
    );
    assert_eq!(s, 200);
    assert_eq!(body["parallel"], false, "row exists, parallel=0: {body}");
}

#[test]
fn put_parallel_round_trips_via_unified_config() {
    let d = TestDashboard::new();
    d.create_plan("cfg-par-rt", &minimal_plan("cfg-par-rt", &d.project));

    // PUT parallel=true, observe it in the response and on a fresh GET.
    let (s, body) = d.put("/api/plans/cfg-par-rt/config", json!({"parallel": true}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], true);

    let (s, body) = d.get("/api/plans/cfg-par-rt/config");
    assert_eq!(s, 200);
    assert_eq!(body["parallel"], true);

    // Both tables carry the value (kept in lockstep so each spawn helper
    // sees the same answer).
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let am: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_mode WHERE plan_name = ?1",
            params!["cfg-par-rt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(am, 1, "plan_auto_mode.parallel must be set");
    let aa: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params!["cfg-par-rt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(aa, 1, "plan_auto_advance.parallel must be set");
    drop(conn);

    // PUT parallel=false flips both tables back.
    let (s, body) = d.put("/api/plans/cfg-par-rt/config", json!({"parallel": false}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], false);
}

#[test]
fn put_partial_preserves_parallel() {
    // PUT-ing other fields without `parallel` must not flip `parallel`
    // back to its default.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-par-partial",
        &minimal_plan("cfg-par-partial", &d.project),
    );

    let (s, _) = d.put(
        "/api/plans/cfg-par-partial/config",
        json!({"parallel": true}),
    );
    assert_eq!(s, 200);

    // PUT autoMode without parallel — parallel must remain true.
    let (s, body) = d.put(
        "/api/plans/cfg-par-partial/config",
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], true, "parallel clobbered: {body}");

    // PUT maxFixAttempts without parallel — parallel must remain true.
    let (s, body) = d.put(
        "/api/plans/cfg-par-partial/config",
        json!({"maxFixAttempts": 7}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], true, "parallel clobbered: {body}");
    assert_eq!(body["maxFixAttempts"], 7);
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
