//! E2E tests for `GET /api/plans/:name/blocking-workflow-status`
//! (Task 3.2 of the `ci-split-task-tests-from-master-pipeline` plan).
//!
//! Surface contract:
//! - Returns `{ stalled: [{ workflow, branch, taskId, agentId }] }`.
//! - 404 when the plan name doesn't exist.
//! - Empty list when no blocking workflow is configured.
//! - Empty list when every configured workflow has a matching file in
//!   `.github/workflows/`.
//! - Empty list when no agent is currently running for the plan
//!   (banner is in-flight-only — operator does not need a warning for
//!   stale archived plans).
//! - Populated list when a configured workflow is NOT in the workflows
//!   directory AND a running agent has a branch + task_id.
//! - Phase-level override beats plan-level; plan-level beats
//!   `branchwork.toml`; classifier (level 4) is intentionally NOT
//!   consulted here.

mod support;

use serde_json::json;
use support::TestDashboard;

fn seed_running_agent(d: &TestDashboard, id: &str, plan: &str, task: &str, branch: &str) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, branch) \
         VALUES (?1, ?1, ?2, 'running', 'pty', ?3, ?4, ?5)",
        rusqlite::params![id, d.project.to_string_lossy(), plan, task, branch],
    )
    .unwrap();
}

fn write_workflow(project: &std::path::Path, file: &str, contents: &str) {
    let dir = project.join(".github").join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), contents).unwrap();
}

fn plan_yaml(name: &str, project_dir: &std::path::Path, blocking: &[&str]) -> String {
    let blocking_block = if blocking.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = blocking.iter().map(|w| format!("  - {w}")).collect();
        format!("ci_blocking_workflows:\n{}\n", lines.join("\n"))
    };
    format!(
        "title: {name}\nproject: {project}\ncontext: ''\n{blocking}phases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display(),
        blocking = blocking_block,
    )
}

#[test]
fn returns_404_for_unknown_plan() {
    let d = TestDashboard::new();
    let (s, body) = d.get("/api/plans/no-such-plan/blocking-workflow-status");
    assert_eq!(s, 404, "body: {body}");
}

#[test]
fn returns_empty_when_no_blocking_workflows_configured() {
    let d = TestDashboard::new();
    d.create_plan(
        "bws-no-config",
        &plan_yaml("bws-no-config", &d.project, &[]),
    );
    seed_running_agent(&d, "agent-noc", "bws-no-config", "1.1", "branchwork/x/1.1");

    let (s, body) = d.get("/api/plans/bws-no-config/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["stalled"], json!([]));
}

#[test]
fn returns_empty_when_workflow_exists_in_repo() {
    let d = TestDashboard::new();
    d.create_plan(
        "bws-exists",
        &plan_yaml("bws-exists", &d.project, &["task-tests"]),
    );
    write_workflow(
        &d.project,
        "task-tests.yml",
        "name: task-tests\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );
    seed_running_agent(&d, "agent-ok", "bws-exists", "1.1", "branchwork/x/1.1");

    let (s, body) = d.get("/api/plans/bws-exists/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["stalled"], json!([]));
}

#[test]
fn returns_empty_when_no_running_agent() {
    let d = TestDashboard::new();
    d.create_plan(
        "bws-no-agent",
        &plan_yaml("bws-no-agent", &d.project, &["task-tests-typo"]),
    );
    // No agent seeded — banner should not fire even though the gate is
    // misconfigured.

    let (s, body) = d.get("/api/plans/bws-no-agent/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["stalled"], json!([]));
}

#[test]
fn returns_stalled_when_workflow_missing_and_agent_running() {
    let d = TestDashboard::new();
    d.create_plan(
        "bws-typo",
        &plan_yaml("bws-typo", &d.project, &["task-tests-typo"]),
    );
    // task-tests.yml EXISTS but the plan gate names task-tests-typo —
    // the canonical acceptance case: non-existent workflow name.
    write_workflow(
        &d.project,
        "task-tests.yml",
        "name: task-tests\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );
    seed_running_agent(
        &d,
        "agent-typo",
        "bws-typo",
        "1.1",
        "branchwork/bws-typo/1.1",
    );

    let (s, body) = d.get("/api/plans/bws-typo/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    let stalled = body["stalled"].as_array().expect("array");
    assert_eq!(stalled.len(), 1, "body: {body}");
    assert_eq!(stalled[0]["workflow"], "task-tests-typo");
    assert_eq!(stalled[0]["branch"], "branchwork/bws-typo/1.1");
    assert_eq!(stalled[0]["taskId"], "1.1");
    assert_eq!(stalled[0]["agentId"], "agent-typo");
}

#[test]
fn returns_one_entry_per_missing_workflow_per_running_agent() {
    let d = TestDashboard::new();
    d.create_plan(
        "bws-multi",
        &plan_yaml("bws-multi", &d.project, &["missing-a", "missing-b"]),
    );
    seed_running_agent(
        &d,
        "agent-multi",
        "bws-multi",
        "1.1",
        "branchwork/bws-multi/1.1",
    );

    let (s, body) = d.get("/api/plans/bws-multi/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    let stalled = body["stalled"].as_array().expect("array");
    assert_eq!(stalled.len(), 2, "body: {body}");
    let names: Vec<&str> = stalled
        .iter()
        .map(|e| e["workflow"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"missing-a"));
    assert!(names.contains(&"missing-b"));
}

#[test]
fn ignores_completed_or_killed_agents() {
    let d = TestDashboard::new();
    d.create_plan("bws-dead", &plan_yaml("bws-dead", &d.project, &["missing"]));
    // Insert a 'completed' agent — the banner is in-flight-only so this
    // must not trigger.
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, branch) \
         VALUES ('agent-done', 'agent-done', ?1, 'completed', 'pty', 'bws-dead', '1.1', 'branchwork/bws-dead/1.1')",
        rusqlite::params![d.project.to_string_lossy()],
    )
    .unwrap();

    let (s, body) = d.get("/api/plans/bws-dead/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["stalled"], json!([]));
}

#[test]
fn falls_back_to_repo_defaults_when_plan_has_no_explicit_config() {
    let d = TestDashboard::new();
    d.create_plan("bws-repo", &plan_yaml("bws-repo", &d.project, &[]));
    std::fs::write(
        d.project.join("branchwork.toml"),
        "[ci]\nblocking_workflows = [\"repo-missing\"]\n",
    )
    .unwrap();
    seed_running_agent(&d, "agent-repo", "bws-repo", "1.1", "branchwork/x/1.1");

    let (s, body) = d.get("/api/plans/bws-repo/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    let stalled = body["stalled"].as_array().expect("array");
    assert_eq!(stalled.len(), 1, "body: {body}");
    assert_eq!(stalled[0]["workflow"], "repo-missing");
}

#[test]
fn phase_level_override_takes_precedence() {
    let d = TestDashboard::new();
    // Plan-level says ["plan-level"], phase-level says ["phase-level"].
    // Phase wins for an agent on a task in that phase.
    let yaml = format!(
        "title: bws-phase\nproject: {project}\ncontext: ''\nci_blocking_workflows:\n  - plan-level\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    ci_blocking_workflows:\n      - phase-level\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        project = d.project.display()
    );
    d.create_plan("bws-phase", &yaml);
    seed_running_agent(
        &d,
        "agent-phase",
        "bws-phase",
        "1.1",
        "branchwork/bws-phase/1.1",
    );

    let (s, body) = d.get("/api/plans/bws-phase/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    let stalled = body["stalled"].as_array().expect("array");
    assert_eq!(stalled.len(), 1, "body: {body}");
    assert_eq!(stalled[0]["workflow"], "phase-level");
}

#[test]
fn ignores_agents_without_branch() {
    let d = TestDashboard::new();
    d.create_plan("bws-nobr", &plan_yaml("bws-nobr", &d.project, &["missing"]));
    // Insert a running agent with branch=NULL — pre-spawn or
    // post-discard rows shouldn't trip the banner.
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, branch) \
         VALUES ('agent-nobr', 'agent-nobr', ?1, 'running', 'pty', 'bws-nobr', '1.1', NULL)",
        rusqlite::params![d.project.to_string_lossy()],
    )
    .unwrap();

    let (s, body) = d.get("/api/plans/bws-nobr/blocking-workflow-status");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["stalled"], json!([]));
}
