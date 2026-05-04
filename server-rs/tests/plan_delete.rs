//! E2E tests for `DELETE /api/plans/:name` (plan-deletion 0.1).
//!
//! Pins the cascade contract: the YAML moves to `archive/` (or is
//! removed on `?hard=true`), every `plan_name`-keyed table is wiped
//! for the plan, and the `agents` table is left intact so historic
//! activity stays visible. Safety gates (running agents, open fix
//! attempts) return 409 instead of mutating anything.

mod support;

use rusqlite::params;
use support::TestDashboard;

fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Insert one row in every cascade table so the test can prove the
/// transaction wiped the lot. `agent_suffix` lets a single test seed
/// the cascade twice for the same plan name (delete preserves the
/// agents row, so a second seed would otherwise collide on the PK).
/// Returns the agent id of the newly inserted preserved row.
fn seed_cascade_rows_with_suffix(d: &TestDashboard, plan: &str, agent_suffix: &str) -> String {
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, '1.1', 'completed')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status) VALUES (?1, '1.1', 'success')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
        params![plan],
    )
    .unwrap();
    // outcome is set to 'success' so this row doesn't trip the
    // auto_mode_in_flight gate (that gate fires only on outcome IS NULL).
    conn.execute(
        "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, outcome) \
         VALUES (?1, '1.1', 1, 'success')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_project (plan_name, project) VALUES (?1, 'some/project')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_verdicts (plan_name, verdict) VALUES (?1, 'ok')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_budget (plan_name, max_budget_usd) VALUES (?1, 5.0)",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, '1.1', 'noted')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO plan_org (plan_name, org_id) VALUES (?1, 'default-org')",
        params![plan],
    )
    .unwrap();

    // A completed agent — must survive delete.
    let agent_id = format!("agent-{plan}-{agent_suffix}");
    conn.execute(
        "INSERT INTO agents (id, cwd, status, plan_name, task_id, branch) \
         VALUES (?1, ?2, 'completed', ?3, '1.1', 'branchwork/x/1.1')",
        params![agent_id, d.project.to_string_lossy(), plan],
    )
    .unwrap();

    agent_id
}

fn seed_cascade_rows(d: &TestDashboard, plan: &str) -> String {
    seed_cascade_rows_with_suffix(d, plan, "0")
}

fn count_in(conn: &rusqlite::Connection, table: &str, plan: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE plan_name = ?1");
    conn.query_row(&sql, params![plan], |r| r.get(0))
        .unwrap_or(-1)
}

fn audit_plan_delete_count(conn: &rusqlite::Connection, plan: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM audit_logs WHERE action = 'plan.delete' AND resource_id = ?1",
        params![plan],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn delete_unknown_plan_returns_404() {
    let d = TestDashboard::new();
    let (s, body) = d.delete("/api/plans/does-not-exist");
    assert_eq!(s, 404, "body: {body}");
    assert_eq!(body["error"], "plan_not_found");
}

#[test]
fn delete_with_running_agent_returns_409_and_preserves_state() {
    let d = TestDashboard::new();
    let plan = "blocked-by-agent";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, '1.1', 'in_progress')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (id, cwd, status, plan_name, task_id) \
         VALUES ('agent-running', ?1, 'running', ?2, '1.1')",
        params![d.project.to_string_lossy(), plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.delete(&format!("/api/plans/{plan}"));
    assert_eq!(s, 409, "body: {body}");
    assert_eq!(body["error"], "plan_has_running_agents");
    assert_eq!(body["agents"][0], "agent-running", "body: {body}");

    // YAML still on disk; cascade rows untouched.
    assert!(d.plans_dir.join(format!("{plan}.yaml")).exists());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(count_in(&conn, "task_status", plan), 1);
}

#[test]
fn delete_with_open_fix_attempt_returns_409_auto_mode_in_flight() {
    let d = TestDashboard::new();
    let plan = "blocked-by-fix";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // outcome IS NULL — fix attempt is still in flight.
    conn.execute(
        "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, outcome) \
         VALUES (?1, '1.1', 1, NULL)",
        params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.delete(&format!("/api/plans/{plan}"));
    assert_eq!(s, 409, "body: {body}");
    assert_eq!(body["error"], "auto_mode_in_flight");

    // YAML and the open fix attempt are both still there.
    assert!(d.plans_dir.join(format!("{plan}.yaml")).exists());
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(count_in(&conn, "task_fix_attempts", plan), 1);
}

#[test]
fn delete_soft_archives_yaml_and_cascades_db_rows() {
    let d = TestDashboard::new();
    let plan = "soft-delete-me";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    let agent_id = seed_cascade_rows(&d, plan);

    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    assert!(yaml.exists(), "precondition: yaml must exist");

    let (s, body) = d.delete(&format!("/api/plans/{plan}"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["hard"], false);
    let archive_path = body["archivePath"].as_str().expect("archivePath set");
    assert!(
        archive_path.contains("/archive/"),
        "archive_path must live under archive/: {archive_path}"
    );
    assert!(
        archive_path.contains(plan),
        "archive_path must mention the plan name: {archive_path}"
    );
    assert!(
        archive_path.ends_with(".yaml"),
        "archive_path must keep the .yaml suffix: {archive_path}"
    );
    // Cascade counts must be reported in the response (one row per table).
    let cascaded = body["cascadedRows"]
        .as_object()
        .expect("cascadedRows object");
    for table in [
        "task_status",
        "ci_runs",
        "plan_auto_mode",
        "plan_auto_advance",
        "task_fix_attempts",
        "plan_project",
        "plan_verdicts",
        "plan_budget",
        "task_learnings",
        "plan_org",
    ] {
        assert!(
            cascaded.get(table).is_some(),
            "cascadedRows must contain {table}: {cascaded:?}"
        );
    }

    // (a) Original YAML moved out, (b) archive file present.
    assert!(!yaml.exists(), "original yaml must be gone");
    assert!(
        std::path::Path::new(archive_path).exists(),
        "archive yaml must exist on disk: {archive_path}"
    );

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    // (b) every cascade table has zero rows for the plan.
    for table in [
        "task_status",
        "ci_runs",
        "plan_auto_mode",
        "plan_auto_advance",
        "task_fix_attempts",
        "plan_project",
        "plan_verdicts",
        "plan_budget",
        "task_learnings",
        "plan_org",
    ] {
        assert_eq!(
            count_in(&conn, table, plan),
            0,
            "{table} must be empty for {plan}"
        );
    }

    // (c) agents row preserved with status / plan_name / branch intact.
    let (status, branch): (String, Option<String>) = conn
        .query_row(
            "SELECT status, branch FROM agents WHERE id = ?1",
            params![agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("agent row must still exist");
    assert_eq!(status, "completed");
    assert_eq!(branch.as_deref(), Some("branchwork/x/1.1"));

    // (d) audit_log has a plan.delete row.
    assert_eq!(audit_plan_delete_count(&conn, plan), 1);

    // The audit diff carries the cascade telemetry the UI needs to render
    // an Undo affordance later.
    let diff: String = conn
        .query_row(
            "SELECT diff FROM audit_logs WHERE action = 'plan.delete' AND resource_id = ?1",
            params![plan],
            |r| r.get(0),
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&diff).unwrap();
    assert_eq!(parsed["hard"], false);
    assert!(parsed["snapshot_id"].is_null(), "snapshot stub: {parsed}");
    assert_eq!(parsed["archive_path"].as_str(), Some(archive_path));
    assert!(
        parsed["cascaded_rows"]["task_status"].as_i64().unwrap() >= 1,
        "diff must record cascade row counts: {parsed}"
    );
}

#[test]
fn delete_hard_removes_yaml_with_no_archive_or_snapshot() {
    let d = TestDashboard::new();
    let plan = "hard-delete-me";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    assert!(yaml.exists());

    let (s, body) = d.delete(&format!("/api/plans/{plan}?hard=true"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["hard"], true);
    assert!(
        body["archivePath"].is_null(),
        "hard delete must not archive: {body}"
    );
    assert!(
        body["snapshotId"].is_null(),
        "hard delete must skip snapshot: {body}"
    );

    assert!(!yaml.exists(), "yaml must be removed on hard delete");
    let archive_dir = d.plans_dir.join("archive");
    if archive_dir.exists() {
        // Subdir may have been created by an earlier soft delete in the
        // same run; just assert no archive file for *this* plan.
        let entries: Vec<_> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{plan}."))
            })
            .collect();
        assert!(
            entries.is_empty(),
            "hard delete must not write an archive file: {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(count_in(&conn, "task_status", plan), 0);
    assert_eq!(count_in(&conn, "task_fix_attempts", plan), 0);
}

#[test]
fn delete_dry_run_passes_gates_without_touching_state() {
    let d = TestDashboard::new();
    let plan = "preview-only";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    assert!(yaml.exists());

    let (s, body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["dryRun"], true);
    assert_eq!(
        body["filePath"].as_str().unwrap_or(""),
        yaml.to_str().unwrap()
    );
    assert!(
        body["cascadeTables"].is_array(),
        "dry-run body should expose the cascade table list: {body}"
    );

    // Nothing changed on disk or in the DB.
    assert!(yaml.exists(), "dry-run must not move the file");
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(count_in(&conn, "task_status", plan), 1);
    assert_eq!(audit_plan_delete_count(&conn, plan), 0);
}

#[test]
fn delete_soft_then_hard_replays_cascade_for_a_recreated_plan() {
    // The same plan name can be re-created (e.g. by re-saving the YAML)
    // and re-deleted. Verifies the handler isn't sensitive to leftover
    // archive entries from a prior soft delete.
    let d = TestDashboard::new();
    let plan = "redelete";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let (s, _) = d.delete(&format!("/api/plans/{plan}"));
    assert_eq!(s, 200);

    // Recreate (UI flow: user pastes the plan back, or 0.4 restore)
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows_with_suffix(&d, plan, "1");

    let (s, body) = d.delete(&format!("/api/plans/{plan}?hard=true"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["hard"], true);
    assert!(!d.plans_dir.join(format!("{plan}.yaml")).exists());
}
