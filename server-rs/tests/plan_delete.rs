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
    // UPSERT so the seed is robust against concurrent inserts from the
    // grandfather merge_cadence migration (cadence plan T1.2): the
    // migration walks `plans_dir/*.yaml` and INSERTs a plan_auto_mode
    // row for every plan it sees, racing with this helper. The plain
    // INSERT would trip the PK constraint when the migration wins the
    // race. ON CONFLICT DO UPDATE keeps the test's `enabled=1` intent
    // regardless of order.
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1) \
         ON CONFLICT(plan_name) DO UPDATE SET enabled = excluded.enabled",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1) \
         ON CONFLICT(plan_name) DO UPDATE SET enabled = excluded.enabled",
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
    // Path components, not substring search — Windows uses `\archive\`.
    let archive_pathbuf = std::path::Path::new(archive_path);
    assert_eq!(
        archive_pathbuf
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str()),
        Some("archive"),
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
    // 0.3 wires `plan_curate::snapshot_plan` so soft delete now
    // surfaces the new `plan_snapshots` row id rather than the
    // pre-0.3 null placeholder.
    let snapshot_id = parsed["snapshot_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("soft delete must record a snapshot id: {parsed}"));
    assert!(snapshot_id > 0);
    let snap_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM plan_snapshots WHERE id = ?1 AND plan_name = ?2 AND kind = 'delete'",
            params![snapshot_id, plan],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(snap_count, 1, "snapshot row must exist after soft delete");
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
fn delete_dry_run_reports_would_delete_row_counts_per_cascade_table() {
    // The PlanBoard modal renders a "12 task statuses, 8 CI runs, …"
    // preview from the `wouldDelete` map. Seed the base singleton row
    // (one per cascade table) plus an extra row in two of the
    // multi-row tables so the per-table counts are not all 1 — that
    // way a regression that swaps `wouldDelete["table"]` to a static
    // 1 (or to the cascade-table list length) is visible.
    let d = TestDashboard::new();
    let plan = "preview-with-counts";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // Add a second task_status row (different task_number → no PK clash).
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status) \
             VALUES (?1, '1.2', 'completed')",
            params![plan],
        )
        .unwrap();
        // Add a second ci_runs row (no PK on (plan,task) → simple insert).
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) \
             VALUES (?1, '1.2', 'success')",
            params![plan],
        )
        .unwrap();
    }

    let (s, body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["dryRun"], true);

    let would_delete = body["wouldDelete"]
        .as_object()
        .unwrap_or_else(|| panic!("wouldDelete must be an object: {body}"));
    let n = |k: &str| -> i64 {
        would_delete
            .get(k)
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| panic!("{k} missing or non-integer in {body}"))
    };

    // Tables we explicitly grew to 2 rows.
    assert_eq!(n("task_status"), 2, "task_status: {body}");
    assert_eq!(n("ci_runs"), 2, "ci_runs: {body}");
    // Singleton-shaped seedings stay at 1.
    assert_eq!(n("plan_auto_mode"), 1, "plan_auto_mode: {body}");
    assert_eq!(n("plan_auto_advance"), 1, "plan_auto_advance: {body}");
    assert_eq!(n("task_fix_attempts"), 1, "task_fix_attempts: {body}");
    assert_eq!(n("plan_project"), 1, "plan_project: {body}");
    assert_eq!(n("plan_verdicts"), 1, "plan_verdicts: {body}");
    assert_eq!(n("plan_budget"), 1, "plan_budget: {body}");
    assert_eq!(n("task_learnings"), 1, "task_learnings: {body}");
    assert_eq!(n("plan_org"), 1, "plan_org: {body}");

    // Every cascade-table key must be present, even when the count is 0
    // (so the modal can render zero counts as "0 task statuses" rather
    // than skipping the row).
    for table in PLAN_CASCADE_TABLES_FOR_TEST {
        assert!(
            would_delete.contains_key(*table),
            "wouldDelete must include {table}: {body}"
        );
    }

    // No mutation: file still on disk, every cascade row still in the DB.
    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    assert!(yaml.exists(), "dry-run must not move the file");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(count_in(&conn, "task_status", plan), 2);
    assert_eq!(count_in(&conn, "ci_runs", plan), 2);
    assert_eq!(audit_plan_delete_count(&conn, plan), 0);
}

/// Mirror of `api::plans::PLAN_CASCADE_TABLES` so tests can iterate
/// the canonical cascade list without a `pub(crate)` re-export. Drift
/// between the two is fine *visually* (the production const is the
/// source of truth) but `delete_dry_run_reports_would_delete_row_counts_per_cascade_table`
/// will panic if the `wouldDelete` map ever drops a key listed here.
const PLAN_CASCADE_TABLES_FOR_TEST: &[&str] = &[
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
];

#[test]
fn delete_dry_run_reports_blocked_by_running_agent_without_409() {
    // The modal disables Delete proactively when the dry-run preview
    // says the plan is blocked. That contract requires the dry-run
    // path to never 409 — gate state is rolled into `blockedBy`
    // instead. Real DELETE still 409s; that is covered separately.
    let d = TestDashboard::new();
    let plan = "blocked-preview-agent";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO agents (id, cwd, status, plan_name, task_id) \
         VALUES ('agent-running-preview', ?1, 'running', ?2, '1.1')",
        params![d.project.to_string_lossy(), plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200, "dry-run must NOT 409 on a blocked plan: {body}");
    assert_eq!(body["dryRun"], true);

    let blocked = body["blockedBy"]
        .as_object()
        .unwrap_or_else(|| panic!("blockedBy must be populated: {body}"));
    let agents = blocked["runningAgents"]
        .as_array()
        .unwrap_or_else(|| panic!("runningAgents must be an array: {body}"));
    assert_eq!(
        agents.len(),
        1,
        "runningAgents should list the one agent: {body}"
    );
    assert_eq!(agents[0], "agent-running-preview");
    assert_eq!(blocked["autoModeInFlight"], false);
}

#[test]
fn delete_dry_run_reports_blocked_by_auto_mode_in_flight() {
    let d = TestDashboard::new();
    let plan = "blocked-preview-fix";
    d.create_plan(plan, &minimal_plan(plan, &d.project));

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, outcome) \
         VALUES (?1, '1.1', 1, NULL)",
        params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200, "dry-run must NOT 409: {body}");
    assert_eq!(body["dryRun"], true);

    let blocked = body["blockedBy"]
        .as_object()
        .unwrap_or_else(|| panic!("blockedBy must be populated: {body}"));
    assert_eq!(blocked["autoModeInFlight"], true);
    assert_eq!(
        blocked["runningAgents"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(usize::MAX),
        0,
        "runningAgents must be an empty array, not absent: {body}"
    );
}

#[test]
fn delete_dry_run_blocked_by_is_null_when_plan_is_not_blocked() {
    let d = TestDashboard::new();
    let plan = "preview-not-blocked";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let (s, body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["dryRun"], true);
    assert!(
        body["blockedBy"].is_null(),
        "blockedBy must be null when plan is not blocked: {body}"
    );
}

#[test]
fn delete_dry_run_does_not_create_snapshot_or_audit_or_broadcast() {
    // Tightens the no-side-effects contract of the dry-run path.
    // Specifically: no plan_snapshots row, no audit_logs row, no FS
    // change. WS broadcast is harder to assert here without wiring a
    // listener, but the absence of `broadcast_event` calls in the
    // dry-run branch is verified by code inspection — this test pins
    // the observable side effects.
    let d = TestDashboard::new();
    let plan = "preview-no-side-effects";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    let archive_dir = d.plans_dir.join("archive");
    let archive_existed_before = archive_dir.exists();

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let snap_count_before: i64 = {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM plan_snapshots WHERE plan_name = ?1",
            params![plan],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    };

    let (s, _body) = d.delete(&format!("/api/plans/{plan}?dry_run=true"));
    assert_eq!(s, 200);

    // No archive directory created, no archived YAML, original yaml
    // still on disk.
    assert!(yaml.exists(), "dry-run must not move the file");
    assert_eq!(
        archive_dir.exists(),
        archive_existed_before,
        "dry-run must not create the archive directory"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let snap_count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM plan_snapshots WHERE plan_name = ?1",
            params![plan],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    assert_eq!(
        snap_count_before, snap_count_after,
        "dry-run must not write a plan_snapshots row"
    );
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
