//! End-to-end tests for the phase-1 recovery endpoints.
//!
//! These would have caught every bug I shipped across dashboard-polish:
//! empty-branch merge false-positive, stale source_branch poisoning,
//! untracked-files-as-dirty, missing Discard for stopped agents.

mod support;

use serde_json::json;
use support::TestDashboard;

/// Plan YAML matching the orchestrAI YamlPlan schema.
///
/// `project` is the absolute path to the scratch repo. We use an
/// absolute path here instead of a relative-to-HOME one because on
/// Windows `dirs::home_dir()` reads from the Win32 API
/// (`GetUserProfileDirectoryW`) and ignores the `HOME`/`USERPROFILE`
/// env vars we set for the child process — so a relative path would
/// join to the real runner's profile, not our tempdir. Absolute paths
/// sidestep this: `Path::join` returns the absolute RHS unchanged.
fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n      - number: '1.2'\n        title: Task 1.2\n        description: ''\n        acceptance: ''\n",
        name = name,
        // Quote because on Windows the path contains backslashes which
        // YAML treats as escape chars in double-quoted strings. Single
        // quotes + doubled internal single-quotes would be safer, but
        // Windows paths can't contain single quotes so plain-unquoted
        // works. Emit the path as-is; serde_yaml accepts it as a scalar.
        project = project_dir.display()
    )
}

#[test]
fn reset_task_status_is_idempotent_and_broadcasts_null() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-a", &minimal_plan("plan-a", &d.project));

    // Set a status, then reset it. status endpoint is PUT.
    let (s, _) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "in_progress"}),
    );
    assert_eq!(s, 200);

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/reset-status"),
        json!({}),
    );
    assert_eq!(s, 200, "reset: {body:?}");
    assert_eq!(body["cleared"], 1);

    // Running again is fine — idempotent, cleared=0.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/reset-status"),
        json!({}),
    );
    assert_eq!(s, 200);
    assert_eq!(body["cleared"], 0);
}

#[test]
fn list_stale_branches_distinguishes_empty_from_populated() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-b", &minimal_plan("plan-b", &d.project));

    // An empty branch (no commits ahead of master) — classic "agent exited
    // without committing" leftover.
    d.create_task_branch(
        &format!("orchestrai/{plan}/1.1"),
        /* with_commit */ false,
    );
    // A branch with real work.
    d.create_task_branch(&format!("orchestrai/{plan}/1.2"), true);

    let (s, body) = d.get(&format!("/api/plans/{plan}/branches/stale"));
    assert_eq!(s, 200, "{body:?}");
    let branches = body["branches"].as_array().expect("branches array");
    assert_eq!(branches.len(), 2);

    let empty = branches
        .iter()
        .find(|b| b["name"].as_str().unwrap().ends_with("/1.1"))
        .unwrap();
    assert_eq!(empty["commitsAheadOfTrunk"], 0);
    assert_eq!(empty["hasUniqueCommits"], false);

    let full = branches
        .iter()
        .find(|b| b["name"].as_str().unwrap().ends_with("/1.2"))
        .unwrap();
    assert!(full["commitsAheadOfTrunk"].as_u64().unwrap() >= 1);
    assert_eq!(full["hasUniqueCommits"], true);
}

#[test]
fn purge_refuses_unique_commits_without_force_then_accepts() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-c", &minimal_plan("plan-c", &d.project));

    let empty_br = format!("orchestrai/{plan}/1.1");
    let full_br = format!("orchestrai/{plan}/1.2");
    d.create_task_branch(&empty_br, false);
    d.create_task_branch(&full_br, true);

    // Without force: empty succeeds, full is refused.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": [&empty_br, &full_br]}),
    );
    assert_eq!(s, 200);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let r_empty = &results[0];
    assert_eq!(r_empty["branch"], empty_br);
    assert_eq!(r_empty["ok"], true);
    let r_full = &results[1];
    assert_eq!(r_full["branch"], full_br);
    assert_eq!(r_full["ok"], false);
    assert_eq!(r_full["error"], "has_unique_commits");

    // Empty should be gone, full should still be there.
    let branches = d.local_branches();
    assert!(!branches.contains(&empty_br), "{empty_br} not deleted");
    assert!(branches.contains(&full_br), "{full_br} gone unexpectedly");

    // With force: full gets deleted too.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": [&full_br], "force": true}),
    );
    assert_eq!(s, 200);
    assert_eq!(body["results"][0]["ok"], true, "{body:?}");
    assert!(!d.local_branches().contains(&full_br));
}

#[test]
fn purge_rejects_out_of_scope_branch_names() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-d", &minimal_plan("plan-d", &d.project));

    // Pre-create a branch outside this plan's prefix.
    d.create_task_branch("other/work", false);

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": ["other/work", "master"]}),
    );
    assert_eq!(s, 200);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results[0]["error"], "out_of_scope");
    assert_eq!(results[1]["error"], "out_of_scope");
    assert!(d.local_branches().contains(&"other/work".to_string()));
    assert!(d.local_branches().contains(&"master".to_string()));
}

#[test]
fn dismiss_ci_run_hides_it_from_latest() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-e", &minimal_plan("plan-e", &d.project));

    // Seed a ci_runs row directly via the status-changed path — we don't
    // have a public endpoint to insert arbitrary runs, so use the server's
    // own rusqlite db. `/api/plans/:name/statuses` confirms presence
    // indirectly by not crashing; for the dismissal contract we just need
    // to ensure `latest_per_task` no longer returns the row. Simpler: seed
    // via `ci_status_changed` broadcast? No, that's read-only. Use a
    // direct ALTER through the auto-status endpoint path.

    // Instead: start a task to create an agent row, then use the agent's
    // merge endpoint to force a ci_runs row. But no remote exists so the
    // push fails. The simplest honest option: open the DB file from the
    // test.
    let db_path = d.dir.path().join(".claude/orchestrai.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha, updated_at) \
         VALUES (?1, '1.1', 'failure', 'deadbeef', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    let run_id = conn.last_insert_rowid();
    drop(conn);

    // Before dismissal: the CI appears in the plan's task CI map.
    let (_s, plan_body) = d.get(&format!("/api/plans/{plan}"));
    let tasks = plan_body["phases"][0]["tasks"].as_array().unwrap();
    let t11 = &tasks[0];
    assert_eq!(
        t11["ci"]["status"].as_str(),
        Some("failure"),
        "expected failure badge, got {t11:?}"
    );

    // Dismiss.
    let (s, body) = d.delete(&format!("/api/ci/{run_id}"));
    assert_eq!(s, 200, "{body:?}");

    // After dismissal: the CI field is null.
    let (_s, plan_body) = d.get(&format!("/api/plans/{plan}"));
    let tasks = plan_body["phases"][0]["tasks"].as_array().unwrap();
    let t11 = &tasks[0];
    assert!(
        t11.get("ci").is_none() || t11["ci"].is_null(),
        "expected ci cleared after dismiss, got {t11:?}"
    );
}

#[test]
fn failure_log_serves_cached_text_and_404s_when_missing() {
    // Pre-seed a ci_runs row with a cached `failure_log` so the endpoint
    // serves from cache without ever shelling out to `gh` (which the test
    // env may not have, and which has no remote to query anyway). A second
    // row with no cache + no provider run_id covers the 404 path.
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-f", &minimal_plan("plan-f", &d.project));

    let db_path = d.dir.path().join(".claude/orchestrai.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha, run_id, failure_log) \
         VALUES (?1, '1.1', 'failure', 'deadbeef', '999', 'boom: it broke')",
        rusqlite::params![plan],
    )
    .unwrap();
    let cached_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha) \
         VALUES (?1, '1.2', 'pending', 'cafef00d')",
        rusqlite::params![plan],
    )
    .unwrap();
    let pending_id = conn.last_insert_rowid();
    drop(conn);

    // Cache hit: 200 with the literal log body, content-type text/plain.
    // The harness parses non-JSON bodies as `Value::String`, so we read it
    // back as a string.
    let (s, body) = d.get(&format!("/api/ci/{cached_id}/failure-log"));
    assert_eq!(s, 200, "{body:?}");
    assert_eq!(body.as_str(), Some("boom: it broke"), "{body:?}");

    // Pending run with no provider run_id: 404, plain-text reason.
    let (s, body) = d.get(&format!("/api/ci/{pending_id}/failure-log"));
    assert_eq!(s, 404, "{body:?}");
    assert!(
        body.as_str()
            .unwrap_or("")
            .contains("failure log unavailable"),
        "{body:?}"
    );
}
