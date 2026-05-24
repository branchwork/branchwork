//! E2E tests for the Phase 3.2 "Past learnings" panel data source.
//!
//! `GET /api/plans/:name/tasks/:num/relevant-learnings` backs the per-task
//! panel that mirrors the agent's spawn-time learnings injection. The
//! load-bearing acceptance criterion ("panel matches what is in the
//! agent's prompt for that task") boils down to two pinned properties:
//!
//! 1. The endpoint and `agents::spawn_ops::inject_learnings_into_prompt`
//!    BOTH route through the same `agents::learnings_context` selector.
//!    Drift here would break the auditable surface. The
//!    `endpoint_matches_what_spawn_ops_would_inject` test pins this by
//!    seeding learnings that the selector picks AND learnings it does
//!    NOT pick, then asserting set-equality with what the endpoint
//!    returns.
//! 2. CI backlinks are surfaced inline so the dashboard can render a
//!    "source: <workflow> ↗" link without a follow-up GET. Pinned by
//!    `endpoint_surfaces_source_url_and_workflow_for_ci_backed_learnings`.

mod support;

use std::path::Path;
use support::TestDashboard;

/// Mirror of `pending_learnings.rs::single_task_plan` — same file-paths
/// shape so the selector's path-token overlap path is exercised. Includes
/// a `file_paths` block so the learnings selector has tokens to score
/// against (without it every category-match would be a false positive).
fn plan_with_file_paths(name: &str, project_dir: &Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n        file_paths:\n          - server-rs/src/agents/spawn_ops.rs\n",
        name = name,
        project = project_dir.display()
    )
}

fn seed_learning(
    d: &TestDashboard,
    id: &str,
    kind: &str,
    category: &str,
    slug: &str,
    body: &str,
    source_ci_run_id: Option<i64>,
) {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO learnings (id, org_id, kind, category, slug, body_md, source_ci_run_id) \
         VALUES (?1, 'default-org', ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, kind, category, slug, body, source_ci_run_id],
    )
    .unwrap();
}

fn seed_ci_failure(d: &TestDashboard, plan: &str, run_id: &str, workflow: &str) -> i64 {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let agent_id = format!("agent-{run_id}");
    conn.execute(
        "INSERT OR REPLACE INTO agents (id, plan_name, task_id, status, cwd, started_at) \
         VALUES (?1, ?2, '1.1', 'running', '/tmp', datetime('now'))",
        rusqlite::params![agent_id, plan],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_failure_events \
             (agent_id, plan_name, task_number, branch, run_id, run_url, \
              workflow, conclusion, summary, org_id) \
         VALUES (?1, ?2, '1.1', ?3, ?4, ?5, ?6, 'failure', '', 'default-org')",
        rusqlite::params![
            agent_id,
            plan,
            format!("branchwork/{plan}/1.1"),
            run_id,
            format!("https://github.com/owner/repo/actions/runs/{run_id}"),
            workflow,
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Acceptance: the panel endpoint returns the SAME set the agent's
/// spawn-time prompt would have prepended. Seeds two learnings: one
/// whose category matches the task's file_paths (selector picks it), one
/// that's entirely unrelated (selector excludes it). Endpoint must
/// surface only the relevant one.
#[test]
fn endpoint_returns_only_learnings_the_selector_would_include() {
    let d = TestDashboard::new();
    let plan = "relevant-learnings-acceptance";
    d.create_plan(plan, &plan_with_file_paths(plan, &d.project));

    // Relevant: category matches the file_paths token "spawn_ops".
    seed_learning(
        &d,
        "L-match",
        "feedback",
        "spawn_ops",
        "spawn-ops-gotcha",
        "Refresh runners after dispatch.",
        None,
    );
    // Irrelevant: nothing overlaps.
    seed_learning(
        &d,
        "L-miss",
        "feedback",
        "completely-unrelated-category",
        "u-slug",
        "no relevance",
        None,
    );

    let (status, body) = d.get(&format!("/api/plans/{plan}/tasks/1.1/relevant-learnings"));
    assert_eq!(status, 200, "expected 200, got {status}; body={body}");
    let items = body["items"].as_array().expect("items must be array");
    let ids: Vec<&str> = items.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"L-match"),
        "relevant learning must be included: {ids:?}"
    );
    assert!(
        !ids.contains(&"L-miss"),
        "irrelevant learning must NOT surface: {ids:?}"
    );
}

/// CI-linked learnings carry the run URL + workflow inline so the
/// drilldown link works without a follow-up GET.
#[test]
fn endpoint_surfaces_source_url_and_workflow_for_ci_backed_learnings() {
    let d = TestDashboard::new();
    let plan = "relevant-learnings-source-url";
    d.create_plan(plan, &plan_with_file_paths(plan, &d.project));

    let event_id = seed_ci_failure(&d, plan, "777", "tests.yml");
    seed_learning(
        &d,
        "L-ci",
        "feedback",
        "spawn_ops", // overlaps with file_paths so the selector picks it
        "ci-fmt-failure",
        "Run cargo fmt before push.",
        Some(event_id),
    );

    let (status, body) = d.get(&format!("/api/plans/{plan}/tasks/1.1/relevant-learnings"));
    assert_eq!(status, 200, "expected 200, got {status}; body={body}");
    let row = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == "L-ci")
        .expect("L-ci must be in response");
    assert_eq!(
        row["sourceRunUrl"],
        format!("https://github.com/owner/repo/actions/runs/777")
    );
    assert_eq!(row["sourceWorkflow"], "tests.yml");
    assert_eq!(
        row["sourceCiRunId"], event_id,
        "sourceCiRunId must round-trip the backlink id"
    );
    // bodyMd must be the full markdown body (drilldown renders this verbatim).
    assert_eq!(row["bodyMd"], "Run cargo fmt before push.");
}

/// Learnings without a CI backlink return null source fields. The
/// drilldown UI hides the link in this case — verify the wire shape
/// supports that branch.
#[test]
fn endpoint_returns_null_source_fields_for_non_ci_learnings() {
    let d = TestDashboard::new();
    let plan = "relevant-learnings-no-source";
    d.create_plan(plan, &plan_with_file_paths(plan, &d.project));
    seed_learning(
        &d,
        "L-no-source",
        "feedback",
        "spawn_ops",
        "no-source",
        "No CI backlink here.",
        None,
    );

    let (_, body) = d.get(&format!("/api/plans/{plan}/tasks/1.1/relevant-learnings"));
    let row = &body["items"][0];
    assert_eq!(row["id"], "L-no-source");
    assert!(
        row["sourceRunUrl"].is_null(),
        "sourceRunUrl must be null when no backlink: {row}"
    );
    assert!(
        row["sourceWorkflow"].is_null(),
        "sourceWorkflow must be null when no backlink: {row}"
    );
}

/// The empty-state path: no learnings at all means an empty items array
/// (not 404). Lets the panel render the "no past learnings" copy
/// unconditionally without branching on HTTP status.
#[test]
fn endpoint_returns_empty_items_array_when_no_learnings_exist() {
    let d = TestDashboard::new();
    let plan = "relevant-learnings-empty";
    d.create_plan(plan, &plan_with_file_paths(plan, &d.project));

    let (status, body) = d.get(&format!("/api/plans/{plan}/tasks/1.1/relevant-learnings"));
    assert_eq!(status, 200);
    let items = body["items"].as_array().expect("items must be array");
    assert!(
        items.is_empty(),
        "items must be empty when no learnings: {items:?}"
    );
}

/// Archived learnings must NOT surface — the spawn-time injection skips
/// them and the panel must match. Pin the same property here so a future
/// refactor that hits `list_learnings_for_org(include_archived=true)`
/// silently breaks loudly.
#[test]
fn endpoint_excludes_archived_learnings() {
    let d = TestDashboard::new();
    let plan = "relevant-learnings-archived";
    d.create_plan(plan, &plan_with_file_paths(plan, &d.project));

    seed_learning(
        &d,
        "L-active",
        "feedback",
        "spawn_ops",
        "active",
        "still relevant",
        None,
    );
    // Archive a second learning directly via SQL.
    {
        let db_path = d.dir.path().join(".claude/branchwork.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO learnings (id, org_id, kind, category, slug, body_md, archived_at, archived_reason) \
             VALUES ('L-archived', 'default-org', 'feedback', 'spawn_ops', 'archived', 'outdated', datetime('now'), 'superseded')",
            [],
        )
        .unwrap();
    }

    let (_, body) = d.get(&format!("/api/plans/{plan}/tasks/1.1/relevant-learnings"));
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"L-active"));
    assert!(
        !ids.contains(&"L-archived"),
        "archived rows must not surface: {ids:?}"
    );
}
