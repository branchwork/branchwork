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

/// Seed `parallel = 1` on both auto-mode/auto-advance rows for `plan` by
/// going around the API — the unified PUT now refuses `parallel = true`
/// (Task 3.5.3) until worktrees ship, so any test that needs to start
/// with the column flipped on must drive the DB directly. Mirrors what a
/// historical row would look like once the worktree plan eventually
/// flips `WORKTREES_SHIPPED` to true and an opted-in project enabled the
/// toggle.
fn seed_parallel_via_sql(db_path: &std::path::Path, plan: &str) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, parallel) VALUES (?1, 1) \
         ON CONFLICT(plan_name) DO UPDATE SET parallel = 1",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_auto_advance (plan_name, parallel, updated_at) \
         VALUES (?1, 1, datetime('now')) \
         ON CONFLICT(plan_name) DO UPDATE SET parallel = 1",
        params![plan],
    )
    .unwrap();
}

#[test]
fn put_parallel_disable_round_trips_via_unified_config() {
    // Enable side is gated until worktrees ship (3.5.3); we still need
    // proof that the disable path keeps both tables in lockstep so a
    // stuck `parallel = 1` row can be flipped off via the API.
    let d = TestDashboard::new();
    d.create_plan("cfg-par-rt", &minimal_plan("cfg-par-rt", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_parallel_via_sql(&db_path, "cfg-par-rt");

    let (s, body) = d.get("/api/plans/cfg-par-rt/config");
    assert_eq!(s, 200);
    assert_eq!(
        body["parallel"], true,
        "seeded value must be visible: {body}"
    );

    // PUT parallel=false flips both tables back. No worktree gate fires
    // on disable — turning the toggle off must always succeed.
    let (s, body) = d.put("/api/plans/cfg-par-rt/config", json!({"parallel": false}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], false);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let am: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_mode WHERE plan_name = ?1",
            params!["cfg-par-rt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(am, 0, "plan_auto_mode.parallel must be cleared");
    let aa: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params!["cfg-par-rt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(aa, 0, "plan_auto_advance.parallel must be cleared");
}

#[test]
fn put_partial_preserves_parallel() {
    // PUT-ing other fields without `parallel` must not flip `parallel`
    // back to its default. Seeded via SQL because the API path that
    // would set it is gated until worktrees ship (3.5.3).
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-par-partial",
        &minimal_plan("cfg-par-partial", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_parallel_via_sql(&db_path, "cfg-par-partial");

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

// ── 3.5.3 worktree gate tests ───────────────────────────────────────────

#[test]
fn put_parallel_true_returns_412_when_worktrees_not_shipped() {
    // Acceptance: PUT with `parallel=true` returns 412 with the documented
    // body when the project has NOT opted in. (Worktrees have shipped, so the
    // per-project `worktree_isolation_opt_in` flag is the gate that's unmet
    // here — this plan never set it.)
    let d = TestDashboard::new();
    d.create_plan("cfg-par-gate", &minimal_plan("cfg-par-gate", &d.project));

    let (s, body) = d.put("/api/plans/cfg-par-gate/config", json!({"parallel": true}));
    assert_eq!(s, 412, "body: {body}");
    assert_eq!(body["error"], "worktrees_required", "body: {body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("worktree")),
        "message must mention worktrees: {body}",
    );

    // The DB columns must NOT have flipped — refused requests have no
    // side-effect on plan state.
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let am: Option<i64> = conn
        .query_row(
            "SELECT parallel FROM plan_auto_mode WHERE plan_name = ?1",
            params!["cfg-par-gate"],
            |row| row.get(0),
        )
        .ok();
    assert!(
        am.is_none() || am == Some(0),
        "plan_auto_mode.parallel must not be set on refusal: {am:?}",
    );
    let aa: Option<i64> = conn
        .query_row(
            "SELECT parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params!["cfg-par-gate"],
            |row| row.get(0),
        )
        .ok();
    assert!(
        aa.is_none() || aa == Some(0),
        "plan_auto_advance.parallel must not be set on refusal: {aa:?}",
    );

    // Subsequent GET still reports parallel=false; the refusal didn't
    // accidentally persist anything.
    let (s, body) = d.get("/api/plans/cfg-par-gate/config");
    assert_eq!(s, 200);
    assert_eq!(body["parallel"], false);
}

#[test]
fn put_parallel_true_succeeds_with_opt_in_now_that_worktrees_shipped() {
    // Both gates must agree before parallel=true is allowed. Worktrees have
    // shipped (`WORKTREES_SHIPPED = true`), so an opted-in project is the
    // one remaining gate — once it's set, the enable side of the toggle
    // succeeds and the flag persists across both tables. (The not-opted-in
    // case is still refused; see `put_parallel_true_returns_412_*`.)
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-par-optedin",
        &minimal_plan("cfg-par-optedin", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO plan_project (plan_name, project, worktree_isolation_opt_in) \
         VALUES (?1, ?2, 1) \
         ON CONFLICT(plan_name) DO UPDATE SET worktree_isolation_opt_in = 1",
        params!["cfg-par-optedin", d.project.to_string_lossy()],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.put(
        "/api/plans/cfg-par-optedin/config",
        json!({"parallel": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], true, "body: {body}");

    // Persisted on both tables (kept in lockstep by the unified PUT).
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let am: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_mode WHERE plan_name = ?1",
            params!["cfg-par-optedin"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(am, 1, "plan_auto_mode.parallel must be set");
    let aa: i64 = conn
        .query_row(
            "SELECT parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params!["cfg-par-optedin"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(aa, 1, "plan_auto_advance.parallel must be set");

    // GET round-trips the enabled flag.
    let (s, body) = d.get("/api/plans/cfg-par-optedin/config");
    assert_eq!(s, 200);
    assert_eq!(body["parallel"], true);
}

#[test]
fn put_parallel_true_audits_refused_attempt() {
    // The refusal must be visible in the audit log so a sysadmin can see
    // the trail of attempts. action = config.parallel_refused, diff
    // carries `{requested: true, reason: "worktrees_not_ready"}`.
    let d = TestDashboard::new();
    d.create_plan("cfg-par-audit", &minimal_plan("cfg-par-audit", &d.project));

    let (s, _) = d.put("/api/plans/cfg-par-audit/config", json!({"parallel": true}));
    assert_eq!(s, 412);

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT diff FROM audit_logs \
             WHERE action = 'config.parallel_refused' \
               AND resource_type = 'plan' \
               AND resource_id = ?1",
        )
        .unwrap();
    let diffs: Vec<String> = stmt
        .query_map(params!["cfg-par-audit"], |row| {
            row.get::<_, Option<String>>(0)
                .map(|o| o.unwrap_or_default())
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(
        diffs.len(),
        1,
        "expected one refused-attempt row, got: {diffs:?}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&diffs[0]).unwrap();
    assert_eq!(parsed["requested"], true, "diff: {parsed}");
    assert_eq!(parsed["reason"], "worktrees_not_ready", "diff: {parsed}");
}

#[test]
fn put_parallel_false_is_never_gated() {
    // Disabling the toggle must always succeed regardless of worktree
    // state. The gate only blocks turning it on; turning it off must
    // remain reachable so an operator can clear a stuck value.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-par-disable",
        &minimal_plan("cfg-par-disable", &d.project),
    );

    // No seeding necessary — even on a clean state, disable is a no-op
    // success path. (Seeding-then-disabling is covered by the round-trip
    // test above.)
    let (s, body) = d.put(
        "/api/plans/cfg-par-disable/config",
        json!({"parallel": false}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["parallel"], false);
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

// ── runner affinity (T11.4) ──────────────────────────────────────────────────

fn seed_test_runner(db_path: &std::path::Path, runner_id: &str, status: &str) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
         VALUES (?1, ?2, 'default-org', ?3, datetime('now'))",
        params![runner_id, runner_id, status],
    )
    .unwrap();
}

#[test]
fn get_config_returns_null_runner_id_by_default() {
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-rid-default",
        &minimal_plan("cfg-rid-default", &d.project),
    );

    let (s, body) = d.get("/api/plans/cfg-rid-default/config");
    assert_eq!(s, 200, "body: {body}");
    assert!(body["runnerId"].is_null(), "got: {body}");
}

#[test]
fn put_runner_id_round_trips_via_unified_config() {
    let d = TestDashboard::new();
    d.create_plan("cfg-rid-rt", &minimal_plan("cfg-rid-rt", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    let (s, body) = d.put(
        "/api/plans/cfg-rid-rt/config",
        json!({"runnerId": "runner-x"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerId"], "runner-x");

    // Re-read via GET to confirm persistence.
    let (s, body) = d.get("/api/plans/cfg-rid-rt/config");
    assert_eq!(s, 200);
    assert_eq!(body["runnerId"], "runner-x");
}

#[test]
fn put_runner_id_explicit_null_clears_the_pin() {
    let d = TestDashboard::new();
    d.create_plan("cfg-rid-clear", &minimal_plan("cfg-rid-clear", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    // First, set the pin.
    let (s, _) = d.put(
        "/api/plans/cfg-rid-clear/config",
        json!({"runnerId": "runner-x"}),
    );
    assert_eq!(s, 200);

    // Then explicit null clears it (back to "any online runner").
    let (s, body) = d.put("/api/plans/cfg-rid-clear/config", json!({"runnerId": null}));
    assert_eq!(s, 200, "body: {body}");
    assert!(body["runnerId"].is_null(), "got: {body}");

    // The clear must DELETE the row, not just NULL the column.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM plan_runner_affinity WHERE plan_name = ?1",
            params!["cfg-rid-clear"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn put_partial_preserves_unspecified_runner_id() {
    // Setting auto_mode without touching runner_id leaves the existing
    // pin intact (mirrors the auto_mode/max_fix_attempts contract).
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-rid-partial",
        &minimal_plan("cfg-rid-partial", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    let (s, _) = d.put(
        "/api/plans/cfg-rid-partial/config",
        json!({"runnerId": "runner-x"}),
    );
    assert_eq!(s, 200);

    // PUT autoMode only — runnerId must NOT clear.
    let (s, body) = d.put(
        "/api/plans/cfg-rid-partial/config",
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerId"], "runner-x", "pin clobbered: {body}");
}

#[test]
fn put_runner_id_re_pin_clears_runner_offline_pause() {
    // If the loop self-paused with runner_offline because the previous
    // pin went offline, repinning to a now-online runner should clear
    // the pause proactively (no manual Resume click).
    let d = TestDashboard::new();
    d.create_plan("cfg-rid-repin", &minimal_plan("cfg-rid-repin", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-old", "offline");
    seed_test_runner(&db_path, "runner-new", "online");

    // Pin to the offline one + simulate a runner_offline pause.
    let (s, _) = d.put(
        "/api/plans/cfg-rid-repin/config",
        json!({"autoMode": true, "runnerId": "runner-old"}),
    );
    assert_eq!(s, 200);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = 'runner_offline', paused_at = datetime('now') \
         WHERE plan_name = ?1",
        params!["cfg-rid-repin"],
    )
    .unwrap();
    drop(conn);

    // Repin to an online runner.
    let (s, body) = d.put(
        "/api/plans/cfg-rid-repin/config",
        json!({"runnerId": "runner-new"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerId"], "runner-new");
    assert!(body["pausedReason"].is_null(), "pause should clear: {body}");
}

// ── T11.5: per-plan runner failover policy ──────────────────────────────

#[test]
fn get_config_returns_pause_failover_by_default() {
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-rf-default",
        &minimal_plan("cfg-rf-default", &d.project),
    );

    let (s, body) = d.get("/api/plans/cfg-rf-default/config");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerFailover"], "pause", "got: {body}");
}

#[test]
fn put_runner_failover_round_trips_when_pinned() {
    let d = TestDashboard::new();
    d.create_plan("cfg-rf-rt", &minimal_plan("cfg-rf-rt", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    // Pin first, then opt into sibling failover.
    let (s, _) = d.put(
        "/api/plans/cfg-rf-rt/config",
        json!({"runnerId": "runner-x"}),
    );
    assert_eq!(s, 200);
    let (s, body) = d.put(
        "/api/plans/cfg-rf-rt/config",
        json!({"runnerFailover": "sibling"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerFailover"], "sibling");

    // Re-read confirms persistence.
    let (s, body) = d.get("/api/plans/cfg-rf-rt/config");
    assert_eq!(s, 200);
    assert_eq!(body["runnerFailover"], "sibling");
}

#[test]
fn put_runner_failover_atomic_with_pin_in_one_request() {
    // Setting both runnerId and runnerFailover in a single PUT applies
    // both: the pin write goes first (UPSERTs the row with default
    // 'pause'), then the failover write flips it to 'sibling'.
    let d = TestDashboard::new();
    d.create_plan("cfg-rf-atomic", &minimal_plan("cfg-rf-atomic", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    let (s, body) = d.put(
        "/api/plans/cfg-rf-atomic/config",
        json!({"runnerId": "runner-x", "runnerFailover": "sibling"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerId"], "runner-x");
    assert_eq!(body["runnerFailover"], "sibling");
}

#[test]
fn put_runner_failover_without_pin_returns_409() {
    let d = TestDashboard::new();
    d.create_plan("cfg-rf-no-pin", &minimal_plan("cfg-rf-no-pin", &d.project));

    let (s, body) = d.put(
        "/api/plans/cfg-rf-no-pin/config",
        json!({"runnerFailover": "sibling"}),
    );
    assert_eq!(s, 409, "body: {body}");
    assert_eq!(body["error"], "no_runner_pin");

    // Default 'pause' was preserved (no row exists).
    let (s, body) = d.get("/api/plans/cfg-rf-no-pin/config");
    assert_eq!(s, 200);
    assert_eq!(body["runnerFailover"], "pause");
}

#[test]
fn put_invalid_runner_failover_returns_400() {
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-rf-invalid",
        &minimal_plan("cfg-rf-invalid", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");
    let (s, _) = d.put(
        "/api/plans/cfg-rf-invalid/config",
        json!({"runnerId": "runner-x"}),
    );
    assert_eq!(s, 200);

    let (s, body) = d.put(
        "/api/plans/cfg-rf-invalid/config",
        json!({"runnerFailover": "yolo"}),
    );
    assert_eq!(s, 400, "body: {body}");
    assert_eq!(body["error"], "invalid_runner_failover");
}

#[test]
fn put_partial_preserves_unspecified_runner_failover() {
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-rf-partial",
        &minimal_plan("cfg-rf-partial", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-x", "online");

    let (s, _) = d.put(
        "/api/plans/cfg-rf-partial/config",
        json!({"runnerId": "runner-x", "runnerFailover": "sibling"}),
    );
    assert_eq!(s, 200);

    // PUT autoMode only — runnerFailover must NOT revert.
    let (s, body) = d.put(
        "/api/plans/cfg-rf-partial/config",
        json!({"autoMode": true}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(
        body["runnerFailover"], "sibling",
        "policy clobbered: {body}"
    );
}

#[test]
fn re_pinning_preserves_runner_failover_at_api_layer() {
    // The DB-layer test pins this invariant; this test verifies the
    // API plumbing keeps it intact (the unified PUT writes runner_id
    // before runner_failover).
    let d = TestDashboard::new();
    d.create_plan("cfg-rf-repin", &minimal_plan("cfg-rf-repin", &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    seed_test_runner(&db_path, "runner-a", "online");
    seed_test_runner(&db_path, "runner-b", "online");

    let (s, _) = d.put(
        "/api/plans/cfg-rf-repin/config",
        json!({"runnerId": "runner-a", "runnerFailover": "sibling"}),
    );
    assert_eq!(s, 200);

    // Re-pin to runner-b WITHOUT touching runnerFailover.
    let (s, body) = d.put(
        "/api/plans/cfg-rf-repin/config",
        json!({"runnerId": "runner-b"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["runnerId"], "runner-b");
    assert_eq!(body["runnerFailover"], "sibling");
}

// ── 3.1 dirty-tree paused_files acceptance ──────────────────────────────

#[test]
fn get_config_surfaces_paused_files_after_dirty_tree_pause() {
    // Acceptance: after a dirty-tree pause, GET /api/plans/{name}/config
    // returns a `pausedFiles: ["server.log", ...]` array alongside the
    // reason. We seed the row directly via SQL because the actual pause
    // path requires a live agent + hook fire which the integration
    // harness does not exercise; the wire is the same — the loop calls
    // `db::auto_mode_pause` with the trimmed file list and the column
    // round-trips through `read_plan_config`.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-paused-files",
        &minimal_plan("cfg-paused-files", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let files_json = serde_json::json!(["server.log", "runner.log", "web-dev.log"]).to_string();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, enabled, paused_reason, paused_at, paused_files) \
         VALUES (?1, 1, ?2, datetime('now'), ?3) \
         ON CONFLICT(plan_name) DO UPDATE SET \
           paused_reason = excluded.paused_reason, \
           paused_at = excluded.paused_at, \
           paused_files = excluded.paused_files",
        params![
            "cfg-paused-files",
            "agent_left_uncommitted_work",
            files_json
        ],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.get("/api/plans/cfg-paused-files/config");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(
        body["pausedReason"], "agent_left_uncommitted_work",
        "body: {body}"
    );
    let files = body["pausedFiles"]
        .as_array()
        .unwrap_or_else(|| panic!("pausedFiles must be an array, got: {body}"));
    let files: Vec<&str> = files.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(files, vec!["server.log", "runner.log", "web-dev.log"]);
}

#[test]
fn get_config_omits_paused_files_when_no_pause() {
    // Default state — no pause, no files. The field is omitted from the
    // serialised JSON entirely (skip_serializing_if = Option::is_none) so
    // the wire shape stays unchanged for plans that never dirty-tree-paused.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-no-pause-files",
        &minimal_plan("cfg-no-pause-files", &d.project),
    );

    let (s, body) = d.get("/api/plans/cfg-no-pause-files/config");
    assert_eq!(s, 200, "body: {body}");
    assert!(
        body.get("pausedFiles").is_none(),
        "pausedFiles must be omitted when no pause has files: {body}"
    );
}

#[test]
fn resume_via_explicit_null_paused_reason_clears_paused_files() {
    // Seed a dirty-tree pause with a file list, then PUT pausedReason: null
    // (the Resume button). Both pausedReason AND pausedFiles must clear.
    let d = TestDashboard::new();
    d.create_plan(
        "cfg-resume-files",
        &minimal_plan("cfg-resume-files", &d.project),
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let files_json = serde_json::json!(["server.log"]).to_string();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, enabled, paused_reason, paused_at, paused_files) \
         VALUES (?1, 1, ?2, datetime('now'), ?3) \
         ON CONFLICT(plan_name) DO UPDATE SET \
           enabled = 1, \
           paused_reason = excluded.paused_reason, \
           paused_at = excluded.paused_at, \
           paused_files = excluded.paused_files",
        params![
            "cfg-resume-files",
            "agent_left_uncommitted_work",
            files_json
        ],
    )
    .unwrap();
    drop(conn);

    // Confirm seed shows pausedFiles before resume.
    let (s, body) = d.get("/api/plans/cfg-resume-files/config");
    assert_eq!(s, 200);
    assert_eq!(body["pausedFiles"].as_array().unwrap().len(), 1);

    // Resume: pausedReason null clears the pause AND its file list.
    let (s, body) = d.put(
        "/api/plans/cfg-resume-files/config",
        json!({"pausedReason": null}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert!(body["pausedReason"].is_null(), "got: {body}");
    assert!(
        body.get("pausedFiles").is_none(),
        "pausedFiles must clear on resume: {body}"
    );

    // Direct DB check confirms the column is NULL, not an empty array.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let stored: Option<String> = conn
        .query_row(
            "SELECT paused_files FROM plan_auto_mode WHERE plan_name = ?1",
            params!["cfg-resume-files"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, None,
        "auto_mode_resume must NULL the column, not write an empty array"
    );
}
