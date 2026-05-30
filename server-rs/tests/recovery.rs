//! End-to-end tests for the phase-1 recovery endpoints.
//!
//! These would have caught every bug I shipped across dashboard-polish:
//! empty-branch merge false-positive, stale source_branch poisoning,
//! untracked-files-as-dirty, missing Discard for stopped agents.

mod support;

use serde_json::json;
use support::TestDashboard;

/// Plan YAML matching the Branchwork YamlPlan schema.
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

/// Reset must refuse while a live agent is still pointed at the task —
/// otherwise the agent's next status write would land on a row the user
/// thinks they cleared, and the dashboard would silently de-sync. The
/// caller has to kill or finish the agent first.
#[test]
fn reset_task_status_refuses_while_agent_is_running() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-running", &minimal_plan("plan-running", &d.project));

    // Seed a status so the row exists — the test fails closed if the guard
    // reaches DELETE despite the running agent.
    let (s, _) = d.put(
        &format!("/api/plans/{plan}/tasks/1.1/status"),
        json!({"status": "checking"}),
    );
    assert_eq!(s, 200);

    // Pin a live agent at this task. We seed agents.id directly because no
    // public endpoint inserts a `running` row without spawning a real PTY,
    // which the test env can't satisfy.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO agents (id, plan_name, task_id, cwd, status, started_at) \
         VALUES ('agent-running-1', ?1, '1.1', '/tmp/scratch', 'running', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/reset-status"),
        json!({}),
    );
    assert_eq!(s, 409, "{body:?}");
    assert_eq!(body["error"], "agent_running");
    assert_eq!(body["agent_id"], "agent-running-1");
    assert!(
        body["message"].as_str().unwrap_or("").contains("Kill"),
        "{body:?}"
    );

    // status row is unchanged — the row we seeded survives.
    let (_s, status_body) = d.get(&format!("/api/plans/{plan}/statuses"));
    let rows = status_body.as_array().expect("statuses array");
    let row = rows
        .iter()
        .find(|r| r["task_number"] == "1.1")
        .expect("1.1 row");
    assert_eq!(row["status"], "checking", "{status_body:?}");

    // Once the agent transitions out of running/starting, reset succeeds.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE agents SET status = 'completed' WHERE id = 'agent-running-1'",
        rusqlite::params![],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/tasks/1.1/reset-status"),
        json!({}),
    );
    assert_eq!(s, 200, "{body:?}");
    assert_eq!(body["cleared"], 1);
}

#[test]
fn list_stale_branches_distinguishes_empty_from_populated() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-b", &minimal_plan("plan-b", &d.project));

    // An empty branch (no commits ahead of master) — classic "agent exited
    // without committing" leftover.
    d.create_task_branch(
        &format!("branchwork/{plan}/1.1"),
        /* with_commit */ false,
    );
    // A branch with real work.
    d.create_task_branch(&format!("branchwork/{plan}/1.2"), true);

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

    let empty_br = format!("branchwork/{plan}/1.1");
    let full_br = format!("branchwork/{plan}/1.2");
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

/// Purge must refuse while a `running` / `starting` agent still owns
/// the branch — otherwise the next session write would land on a branch
/// the user thinks they cleared, and the in-flight commit history would
/// vanish out from under the agent. `force=true` does NOT bypass this:
/// unlike the unique-commits guard (which only discards recoverable
/// work), live-agent state is the one case where deletion would
/// actively corrupt a session. Mirrors Task 1.1's reset-status guard.
#[test]
fn purge_refuses_while_agent_is_running() {
    let d = TestDashboard::new();
    let plan = d.create_plan(
        "plan-running-branch",
        &minimal_plan("plan-running-branch", &d.project),
    );

    let live_br = format!("branchwork/{plan}/1.1");
    d.create_task_branch(&live_br, /* with_commit */ false);

    // Pin a live agent at this branch. We seed agents.id directly because
    // no public endpoint inserts a `running` row without spawning a real
    // PTY, which the test env can't satisfy.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO agents (id, plan_name, task_id, cwd, status, branch, started_at) \
         VALUES ('agent-live-1', ?1, '1.1', '/tmp/scratch', 'running', ?2, datetime('now'))",
        rusqlite::params![plan, live_br],
    )
    .unwrap();
    drop(conn);

    // Without force: refused with agent_running, branch survives.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": [&live_br]}),
    );
    assert_eq!(s, 200, "{body:?}");
    let r = &body["results"][0];
    assert_eq!(r["branch"], live_br);
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "agent_running");
    assert_eq!(r["agent_id"], "agent-live-1");
    assert!(d.local_branches().contains(&live_br));

    // With force: still refused — force does not lift the live-agent
    // guard, only the unique-commits guard.
    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": [&live_br], "force": true}),
    );
    assert_eq!(s, 200);
    assert_eq!(body["results"][0]["error"], "agent_running");
    assert!(d.local_branches().contains(&live_br));

    // List endpoint surfaces the live status so the UI can disable the
    // checkbox before the user even tries.
    let (_s, list_body) = d.get(&format!("/api/plans/{plan}/branches/stale"));
    let entry = list_body["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == live_br)
        .expect("entry for live branch");
    assert_eq!(entry["agentId"], "agent-live-1");
    assert_eq!(entry["agentStatus"], "running");

    // Once the agent transitions out of running/starting, purge succeeds.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE agents SET status = 'completed' WHERE id = 'agent-live-1'",
        rusqlite::params![],
    )
    .unwrap();
    drop(conn);

    let (s, body) = d.post(
        &format!("/api/plans/{plan}/branches/stale/purge"),
        json!({"branches": [&live_br]}),
    );
    assert_eq!(s, 200, "{body:?}");
    assert_eq!(body["results"][0]["ok"], true);
    assert!(!d.local_branches().contains(&live_br));
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
    let db_path = d.dir.path().join(".claude/branchwork.db");
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

/// The dismiss endpoint's docstring promises "Idempotent; dismissing twice is
/// a no-op" and `.ok()`-swallows the UPDATE so a missing row also 200s. Both
/// contracts are load-bearing: the UI fires DELETE without checking the row's
/// `dismissed_at`, and a stale `ciRunId` from an out-of-date plan refresh must
/// not surface as an error toast.
#[test]
fn dismiss_ci_run_is_idempotent_and_tolerates_missing_rows() {
    let d = TestDashboard::new();
    let plan = d.create_plan(
        "plan-dismiss-idem",
        &minimal_plan("plan-dismiss-idem", &d.project),
    );

    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha, updated_at) \
         VALUES (?1, '1.1', 'failure', 'deadbeef', datetime('now'))",
        rusqlite::params![plan],
    )
    .unwrap();
    let run_id = conn.last_insert_rowid();
    drop(conn);

    // First dismiss: 200, badge cleared.
    let (s, _) = d.delete(&format!("/api/ci/{run_id}"));
    assert_eq!(s, 200);

    // Second dismiss against the now-already-dismissed row: still 200.
    let (s, body) = d.delete(&format!("/api/ci/{run_id}"));
    assert_eq!(s, 200, "{body:?}");
    assert_eq!(body["ok"], true);

    // Dismiss against a row id that never existed: still 200, plan/task
    // fields null because the lookup found no row to broadcast about.
    let (s, body) = d.delete("/api/ci/9999999");
    assert_eq!(s, 200, "{body:?}");
    assert_eq!(body["ok"], true);
    assert!(body["plan_name"].is_null(), "{body:?}");
    assert!(body["task_number"].is_null(), "{body:?}");
}

#[test]
fn failure_log_serves_cached_text_and_404s_when_missing() {
    // Pre-seed a ci_runs row with a cached `failure_log` so the endpoint
    // serves from cache without ever shelling out to `gh` (which the test
    // env may not have, and which has no remote to query anyway). A second
    // row with no cache + no provider run_id covers the 404 path.
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-f", &minimal_plan("plan-f", &d.project));

    let db_path = d.dir.path().join(".claude/branchwork.db");
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

/// End-to-end for POST /api/actions/fix-ci. Verifies the validation branches
/// (bad run id, non-failure run, missing SHA) and — for a valid failing run
/// with a cached failure log and a real commit SHA in the scratch repo —
/// that the endpoint creates the `branchwork/fix/<plan>/<task>/<run_id>`
/// branch at the failing commit and records an agents row pointing at it.
///
/// Side-steps needing `claude` on PATH: the endpoint spawns a session daemon
/// that tries to exec `claude` and dies, but `start_pty_agent` still returns
/// the agent id either way (the row is inserted before the PTY spawn and
/// stays even if the daemon fails). We assert on the branch + DB row, not
/// on the live agent process.
#[test]
fn fix_ci_validates_run_state_and_creates_recovery_branch() {
    let d = TestDashboard::new();
    let plan = d.create_plan("plan-fix", &minimal_plan("plan-fix", &d.project));

    // Resolve the scratch repo's HEAD SHA so the `git checkout -b <fix> <sha>`
    // inside the endpoint has something real to branch off. Using a bogus
    // SHA would 500 on checkout and muddy the test.
    let head_sha = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&d.project)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // Row 1: real failure with SHA + cached log — the happy path.
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha, failure_log) \
         VALUES (?1, '1.1', 'failure', ?2, 'cargo test: mod::x FAILED')",
        rusqlite::params![plan, head_sha],
    )
    .unwrap();
    let good_id = conn.last_insert_rowid();
    // Row 2: still running — should 400.
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status, commit_sha) \
         VALUES (?1, '1.2', 'pending', ?2)",
        rusqlite::params![plan, head_sha],
    )
    .unwrap();
    let pending_id = conn.last_insert_rowid();
    // Row 3: failure but no commit_sha — can't branch, should 400.
    conn.execute(
        "INSERT INTO ci_runs (plan_name, task_number, status) \
         VALUES (?1, '1.1', 'failure')",
        rusqlite::params![plan],
    )
    .unwrap();
    let shaless_id = conn.last_insert_rowid();
    drop(conn);

    // Unknown run id → 404.
    let (s, body) = d.post(
        "/api/actions/fix-ci",
        json!({"planName": plan, "taskNumber": "1.1", "ciRunId": 999_999}),
    );
    assert_eq!(s, 404, "{body:?}");

    // Non-failure run → 400 with a clear message.
    let (s, body) = d.post(
        "/api/actions/fix-ci",
        json!({"planName": plan, "taskNumber": "1.2", "ciRunId": pending_id}),
    );
    assert_eq!(s, 400, "{body:?}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("nothing to fix"),
        "{body:?}"
    );

    // Failure run with no SHA → 400.
    let (s, body) = d.post(
        "/api/actions/fix-ci",
        json!({"planName": plan, "taskNumber": "1.1", "ciRunId": shaless_id}),
    );
    assert_eq!(s, 400, "{body:?}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("no commit SHA"),
        "{body:?}"
    );

    // Happy path. Response carries agentId + branch + ciRunId.
    let (s, body) = d.post(
        "/api/actions/fix-ci",
        json!({"planName": plan, "taskNumber": "1.1", "ciRunId": good_id}),
    );
    assert_eq!(s, 200, "{body:?}");
    let expected_branch = format!("branchwork/fix/{plan}/1.1/{good_id}");
    assert_eq!(body["branch"], expected_branch);
    assert_eq!(body["ciRunId"], good_id);
    let agent_id = body["agentId"].as_str().expect("agentId in response");
    assert!(!agent_id.is_empty());

    // Recovery branch exists in the scratch repo and points at the failing SHA.
    assert!(
        d.local_branches().contains(&expected_branch),
        "branch {expected_branch} missing from {:?}",
        d.local_branches()
    );
    let branch_sha = {
        let out = std::process::Command::new("git")
            .args(["rev-parse", &expected_branch])
            .current_dir(&d.project)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(
        branch_sha, head_sha,
        "fix branch didn't start at failing SHA"
    );

    // Agents row was inserted with that branch + the task bookkeeping the
    // merge flow needs. Prompt must carry the failure-log excerpt and the
    // pre-commit checks the task description mandates.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // Turbofish each column so the row type is inferred without a
    // 6-field tuple annotation (clippy::type_complexity).
    let (db_branch, db_plan, db_task, db_prompt, db_cwd, _db_status, db_spawn_error) = conn
        .query_row(
            "SELECT branch, plan_name, task_id, prompt, cwd, status, spawn_error FROM agents WHERE id = ?1",
            rusqlite::params![agent_id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .expect("agents row for fix-ci agent");
    assert_eq!(db_branch.as_deref(), Some(expected_branch.as_str()));
    assert_eq!(db_plan.as_deref(), Some(plan.as_str()));
    assert_eq!(db_task.as_deref(), Some("1.1"));
    let prompt = db_prompt.unwrap_or_default();
    assert!(prompt.contains("CI IS FAILING"), "prompt missing banner");
    assert!(
        prompt.contains("cargo test: mod::x FAILED"),
        "prompt missing failure log excerpt"
    );
    assert!(
        prompt.contains("cargo fmt") && prompt.contains("clippy"),
        "prompt missing pre-commit requirement"
    );

    // The recovery agent must actually start, not die at worktree setup.
    // Before the worktree wiring, Fix-CI pre-created the fix branch with
    // `git checkout -b` in the project root; with worktrees ON that left
    // the branch checked out in the main tree, so the agent's
    // `git worktree add <path> <fix_branch>` failed ("already used by
    // worktree") and `start_pty_agent` recorded a `failed` row whose
    // cwd is the project root. The assertions below pin the fix: the
    // recovery agent gets its OWN per-agent worktree (cwd under the
    // test's tempdir worktree base, carrying the `<plan>/<task>-<id8>`
    // path shape), never the shared project root.
    //
    // We assert on `spawn_error`, NOT `status`: in CI there is no `claude`
    // on PATH, so the driver legitimately fails to exec and the row lands
    // at `status='failed'` — but that is a *post*-worktree-setup failure
    // and irrelevant to this test. A worktree-setup failure is the one
    // signal we care about, and it is recorded distinctly: `start_pty_agent`
    // writes a `spawn_error` of "worktree setup failed: …" and pins cwd to
    // the project root. Both of those, not the bare `failed` status, are the
    // regression shape.
    let cwd = db_cwd.expect("fix-ci agent cwd recorded");
    assert!(
        !db_spawn_error
            .as_deref()
            .unwrap_or("")
            .contains("worktree setup failed"),
        "recovery agent must not fail at worktree setup (spawn_error={db_spawn_error:?}, cwd={cwd})"
    );
    assert_ne!(
        std::path::Path::new(&cwd),
        d.project.as_path(),
        "recovery agent must run in a per-agent worktree, not the shared project root"
    );
    assert!(
        cwd.contains(".branchwork-worktrees") && cwd.contains("plan-fix") && cwd.contains("1.1-"),
        "recovery agent cwd should be its per-agent worktree path, got: {cwd}"
    );
}
