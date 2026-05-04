//! E2E tests for `POST /api/snapshots/:id/restore` (plan-deletion 0.4).
//!
//! Pins the round-trip contract: a soft-delete snapshots the plan;
//! restore replays the cascade rows, writes the YAML back, drops the
//! archive copy, and marks the snapshot's `restored_at`. Repeat
//! restores return 410 (Gone) — the snapshot row stays around as the
//! audit substrate but cannot be re-applied. Slug-collision is the
//! one user-recoverable refusal: the user renames the colliding plan
//! and retries.

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

/// Same shape as `tests/plan_delete.rs::seed_cascade_rows` — keep in
/// sync if a new cascade table lands.
fn seed_cascade_rows(d: &TestDashboard, plan: &str) {
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();

    conn.execute(
        "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, '1.1', 'completed')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, run_url) \
         VALUES (?1, '1.1', 'success', 'https://example/runs/1')",
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
    conn.execute(
        "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, outcome) \
         VALUES (?1, '1.1', 1, 'success')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_project (plan_name, project) VALUES (?1, 'restore-proj')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_verdicts (plan_name, verdict) VALUES (?1, 'ok')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO plan_budget (plan_name, max_budget_usd) VALUES (?1, 7.5)",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_learnings (plan_name, task_number, learning) \
         VALUES (?1, '1.1', 'first lesson')",
        params![plan],
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO plan_org (plan_name, org_id) VALUES (?1, 'default-org')",
        params![plan],
    )
    .unwrap();
}

fn count_in(conn: &rusqlite::Connection, table: &str, plan: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE plan_name = ?1");
    conn.query_row(&sql, params![plan], |r| r.get(0))
        .unwrap_or(-1)
}

fn snapshot_id_from_delete(body: &serde_json::Value) -> i64 {
    body["snapshotId"]
        .as_i64()
        .unwrap_or_else(|| panic!("snapshotId missing/wrong type in body: {body}"))
}

#[test]
fn restore_unknown_snapshot_returns_404() {
    let d = TestDashboard::new();
    let (s, body) = d.post("/api/snapshots/424242/restore", serde_json::json!({}));
    assert_eq!(s, 404, "body: {body}");
    assert_eq!(body["error"], "snapshot_not_found");
}

#[test]
fn soft_delete_then_restore_replays_cascade_and_writes_yaml() {
    let d = TestDashboard::new();
    let plan = "restore-me";
    let yaml_body = minimal_plan(plan, &d.project);
    d.create_plan(plan, &yaml_body);
    seed_cascade_rows(&d, plan);

    let yaml = d.plans_dir.join(format!("{plan}.yaml"));
    assert!(yaml.exists(), "precondition: yaml must exist");

    // Soft delete first; pull the snapshot id out of the response.
    let (s, del_body) = d.delete(&format!("/api/plans/{plan}"));
    assert_eq!(s, 200, "delete body: {del_body}");
    let snap_id = snapshot_id_from_delete(&del_body);
    let archive_path = del_body["archivePath"]
        .as_str()
        .expect("archivePath set on soft delete")
        .to_string();
    assert!(
        std::path::Path::new(&archive_path).exists(),
        "archive must exist after soft delete: {archive_path}"
    );
    assert!(!yaml.exists(), "yaml must be gone after soft delete");

    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(
            count_in(&conn, "task_status", plan),
            0,
            "delete must wipe task_status"
        );
        assert_eq!(
            count_in(&conn, "task_learnings", plan),
            0,
            "delete must wipe task_learnings"
        );
    }

    // Restore.
    let (s, body) = d.post(
        &format!("/api/snapshots/{snap_id}/restore"),
        serde_json::json!({}),
    );
    assert_eq!(s, 200, "restore body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["plan"], plan);
    assert_eq!(body["snapshotId"], snap_id);
    assert!(
        body["restoredAt"].as_str().is_some(),
        "restoredAt must be a non-empty string: {body}"
    );

    // YAML is back at the original location with the original body.
    assert!(yaml.exists(), "yaml must be written by restore");
    assert_eq!(
        std::fs::read_to_string(&yaml).unwrap(),
        yaml_body,
        "yaml body must round-trip exactly"
    );
    // The archive copy is gone (snapshot was the source of truth).
    assert!(
        !std::path::Path::new(&archive_path).exists(),
        "archive copy must be cleaned up after restore: {archive_path}"
    );

    // Cascade rows are back.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
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
        let n = count_in(&conn, table, plan);
        assert!(n >= 1, "restore must re-populate {table} (got {n} rows)");
    }
    // The replayedRows breakdown carries inserted/skipped per table.
    let replayed = body["replayedRows"]
        .as_object()
        .expect("replayedRows object");
    assert!(
        replayed.contains_key("task_status"),
        "replayedRows must include task_status: {replayed:?}"
    );
    assert_eq!(
        replayed["task_status"]["inserted"]
            .as_i64()
            .expect("inserted count"),
        1,
        "task_status replayed inserted=1"
    );
    assert_eq!(
        replayed["task_status"]["skipped"]
            .as_i64()
            .expect("skipped count"),
        0,
        "task_status replayed skipped=0 on a clean restore"
    );

    // Snapshot row marked restored_at.
    let restored_at: Option<String> = conn
        .query_row(
            "SELECT restored_at FROM plan_snapshots WHERE id = ?1",
            params![snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        restored_at.is_some(),
        "restored_at must be set on the snapshot row"
    );

    // Audit log got a plan.restore row.
    let (action, resource_id): (String, Option<String>) = conn
        .query_row(
            "SELECT action, resource_id FROM audit_logs \
             WHERE action = 'plan.restore' AND resource_id = ?1",
            params![plan],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("plan.restore audit row must exist");
    assert_eq!(action, "plan.restore");
    assert_eq!(resource_id.as_deref(), Some(plan));
}

#[test]
fn restore_twice_returns_410_gone() {
    let d = TestDashboard::new();
    let plan = "twice-restored";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let (_, del) = d.delete(&format!("/api/plans/{plan}"));
    let snap_id = snapshot_id_from_delete(&del);

    let (s1, _) = d.post(
        &format!("/api/snapshots/{snap_id}/restore"),
        serde_json::json!({}),
    );
    assert_eq!(s1, 200, "first restore must succeed");

    let (s2, body2) = d.post(
        &format!("/api/snapshots/{snap_id}/restore"),
        serde_json::json!({}),
    );
    assert_eq!(s2, 410, "second restore must be 410 Gone, body: {body2}");
    assert_eq!(body2["error"], "snapshot_already_restored");
    assert!(
        body2["restored_at"].as_str().is_some(),
        "410 body must surface restored_at: {body2}"
    );
}

#[test]
fn restore_with_slug_collision_returns_409() {
    let d = TestDashboard::new();
    let plan = "collide-me";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let (_, del) = d.delete(&format!("/api/plans/{plan}"));
    let snap_id = snapshot_id_from_delete(&del);

    // User reuses the slug for an unrelated plan with a different
    // title. The new YAML body is on disk; restore must refuse.
    let new_yaml = minimal_plan("collide-me-NEW-TITLE", &d.project);
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), &new_yaml).unwrap();

    let (s, body) = d.post(
        &format!("/api/snapshots/{snap_id}/restore"),
        serde_json::json!({}),
    );
    assert_eq!(s, 409, "slug collision must return 409, body: {body}");
    assert_eq!(body["error"], "slug_collision");
    let current = body["current"].as_str().unwrap_or("");
    assert!(
        current.contains("collide-me-NEW-TITLE"),
        "current summary must surface the colliding plan title: {current}"
    );

    // The snapshot row is NOT marked restored — the user can rename
    // and retry.
    let conn =
        rusqlite::Connection::open(d.dir.path().join(".claude").join("branchwork.db")).unwrap();
    let restored_at: Option<String> = conn
        .query_row(
            "SELECT restored_at FROM plan_snapshots WHERE id = ?1",
            params![snap_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        restored_at.is_none(),
        "409 must leave restored_at NULL so the user can retry"
    );
}

#[test]
fn hard_delete_leaves_no_snapshot_to_restore() {
    // Hard delete bypasses the snapshot path entirely. There's no row
    // in plan_snapshots to point a restore at, so any id (including
    // ids that were valid before another plan was hard-deleted)
    // returns 404.
    let d = TestDashboard::new();
    let plan = "hard-delete-no-undo";
    d.create_plan(plan, &minimal_plan(plan, &d.project));
    seed_cascade_rows(&d, plan);

    let (s, body) = d.delete(&format!("/api/plans/{plan}?hard=true"));
    assert_eq!(s, 200, "hard delete must succeed: {body}");
    assert!(
        body["snapshotId"].is_null(),
        "hard delete writes no snapshot"
    );

    // No snapshots at all in the DB.
    let conn =
        rusqlite::Connection::open(d.dir.path().join(".claude").join("branchwork.db")).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM plan_snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "hard delete must leave plan_snapshots empty");

    // Trying to restore any id returns 404 (the user has no recovery
    // affordance for hard deletes — that's the contract).
    let (s, body) = d.post("/api/snapshots/1/restore", serde_json::json!({}));
    assert_eq!(s, 404, "no snapshot to restore: {body}");
}
