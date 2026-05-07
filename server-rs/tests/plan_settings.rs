//! E2E tests for `GET/PUT /api/plans/:name/settings` (Task 3.1).
//!
//! Covers:
//! - GET returns current values + workflows enumerated from
//!   `<project>/.github/workflows/*.yml` (top-level `name:` or fallback
//!   to filename stem).
//! - GET surfaces `branchwork.toml` repo defaults under `repoDefaults`.
//! - PUT round-trips fields back into the plan YAML.
//! - PUT preserves comments and unrelated keys (the comment-preservation
//!   acceptance criterion).
//! - PUT with explicit `null` removes the key.
//! - 404 on unknown plan, 4xx on malformed YAML (never 5xx).

mod support;

use serde_json::json;
use support::TestDashboard;

fn yaml_with_settings(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "# Header comment that must survive every PUT.\n\
         title: {name}\n\
         project: {project}\n\
         context: ''\n\
         ci_blocking_workflows:\n  - CI\n  - lint\n\
         phase_verification: bash scripts/verify.sh\n\
         phases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

fn yaml_without_settings(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\n\
         project: {project}\n\
         context: ''\n\
         phases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

fn write_workflow(project: &std::path::Path, file: &str, contents: &str) {
    let dir = project.join(".github").join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), contents).unwrap();
}

#[test]
fn get_returns_current_plan_values() {
    let d = TestDashboard::new();
    d.create_plan(
        "settings-cur",
        &yaml_with_settings("settings-cur", &d.project),
    );

    let (s, body) = d.get("/api/plans/settings-cur/settings");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["ciBlockingWorkflows"], json!(["CI", "lint"]));
    assert_eq!(body["phaseVerification"], "bash scripts/verify.sh");
    assert!(body["availableWorkflows"].is_array(), "{body}");
    assert!(body["repoDefaults"].is_object(), "{body}");
}

#[test]
fn get_returns_null_when_fields_absent() {
    let d = TestDashboard::new();
    d.create_plan(
        "settings-empty",
        &yaml_without_settings("settings-empty", &d.project),
    );

    let (s, body) = d.get("/api/plans/settings-empty/settings");
    assert_eq!(s, 200, "body: {body}");
    assert!(body["ciBlockingWorkflows"].is_null(), "got: {body}");
    assert!(body["phaseVerification"].is_null(), "got: {body}");
    assert_eq!(body["availableWorkflows"], json!([]));
}

#[test]
fn get_enumerates_workflows_from_github_dir() {
    let d = TestDashboard::new();
    d.create_plan(
        "settings-wf",
        &yaml_without_settings("settings-wf", &d.project),
    );

    write_workflow(
        &d.project,
        "ci.yml",
        "name: Continuous Integration\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );
    write_workflow(
        &d.project,
        "docker.yml",
        "name: Docker Build\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );
    // No top-level `name:` → fallback to filename stem.
    write_workflow(
        &d.project,
        "bench.yml",
        "on: [push]\njobs:\n  bench:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    );

    let (s, body) = d.get("/api/plans/settings-wf/settings");
    assert_eq!(s, 200, "body: {body}");
    let workflows = body["availableWorkflows"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        workflows.contains(&"Continuous Integration".to_string()),
        "got: {workflows:?}"
    );
    assert!(
        workflows.contains(&"Docker Build".to_string()),
        "got: {workflows:?}"
    );
    assert!(
        workflows.contains(&"bench".to_string()),
        "got: {workflows:?}"
    );
}

#[test]
fn get_surfaces_branchwork_toml_repo_defaults() {
    let d = TestDashboard::new();
    d.create_plan(
        "settings-repo",
        &yaml_without_settings("settings-repo", &d.project),
    );

    std::fs::write(
        d.project.join("branchwork.toml"),
        r#"
[ci]
blocking_workflows = ["CI", "lint"]
blocking_workflows_skip = ["Docker"]

[phase]
verification = "make verify"
"#,
    )
    .unwrap();

    let (s, body) = d.get("/api/plans/settings-repo/settings");
    assert_eq!(s, 200, "body: {body}");
    let repo = &body["repoDefaults"];
    assert_eq!(repo["ciBlockingWorkflows"], json!(["CI", "lint"]));
    assert_eq!(repo["ciBlockingWorkflowsSkip"], json!(["Docker"]));
    assert_eq!(repo["phaseVerification"], "make verify");
}

#[test]
fn put_round_trips_to_yaml_and_preserves_comments() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-rt.yaml");
    let initial = format!(
        "# header preserved\ntitle: Round Trip\nproject: {project}\ncontext: ''\nci_blocking_workflows:\n  - CI\n  - lint\nphase_verification: bash scripts/verify.sh\n# trailing comment about phases\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        project = d.project.display()
    );
    std::fs::write(&path, &initial).unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-rt/settings",
        json!({
            "ciBlockingWorkflows": ["CI", "audit"],
            "phaseVerification": "make verify"
        }),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["ciBlockingWorkflows"], json!(["CI", "audit"]));
    assert_eq!(body["phaseVerification"], "make verify");

    // Re-read raw file: comments + other keys must survive.
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("# header preserved"),
        "header comment dropped:\n{after}"
    );
    assert!(
        after.contains("# trailing comment about phases"),
        "trailing comment dropped:\n{after}"
    );
    assert!(after.contains("- audit"));
    assert!(after.contains("make verify"));
    assert!(!after.contains("- lint"));
    assert!(!after.contains("bash scripts/verify.sh"));
}

#[test]
fn put_explicit_null_removes_field() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-null.yaml");
    std::fs::write(&path, yaml_with_settings("settings-null", &d.project)).unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-null/settings",
        json!({"ciBlockingWorkflows": null}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert!(body["ciBlockingWorkflows"].is_null(), "got: {body}");
    // The other field stays.
    assert_eq!(body["phaseVerification"], "bash scripts/verify.sh");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        !after.contains("ci_blocking_workflows"),
        "key still present:\n{after}"
    );
    assert!(after.contains("phase_verification:"));
}

#[test]
fn put_inserts_field_when_absent() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-insert.yaml");
    std::fs::write(&path, yaml_without_settings("settings-insert", &d.project)).unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-insert/settings",
        json!({"phaseVerification": "bash verify.sh"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["phaseVerification"], "bash verify.sh");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("phase_verification: bash verify.sh"));
    // Sanity: the file still parses.
    let _: serde_yaml::Value = serde_yaml::from_str(&after).unwrap();
}

#[test]
fn put_partial_body_leaves_unspecified_field_alone() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-partial.yaml");
    std::fs::write(&path, yaml_with_settings("settings-partial", &d.project)).unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-partial/settings",
        json!({"phaseVerification": "make verify"}),
    );
    assert_eq!(s, 200, "body: {body}");
    // Unspecified `ciBlockingWorkflows` must NOT have been clobbered.
    assert_eq!(body["ciBlockingWorkflows"], json!(["CI", "lint"]));
    assert_eq!(body["phaseVerification"], "make verify");
}

#[test]
fn get_returns_404_for_unknown_plan() {
    let d = TestDashboard::new();
    let (s, body) = d.get("/api/plans/no-such-plan/settings");
    assert_eq!(s, 404, "body: {body}");
}

#[test]
fn put_returns_404_for_unknown_plan() {
    let d = TestDashboard::new();
    let (s, body) = d.put(
        "/api/plans/no-such-plan/settings",
        json!({"phaseVerification": "x"}),
    );
    assert_eq!(s, 404, "body: {body}");
}

#[test]
fn get_returns_4xx_on_malformed_yaml() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-bad.yaml");
    // Unbalanced bracket → YAML parse fails.
    std::fs::write(&path, "title: 'unterminated\nphases: [").unwrap();

    let (s, body) = d.get("/api/plans/settings-bad/settings");
    assert!(
        (400..500).contains(&s),
        "expected 4xx for malformed YAML, got {s}: {body}"
    );
}

#[test]
fn put_returns_4xx_on_malformed_yaml() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("settings-badp.yaml");
    std::fs::write(&path, "title: 'unterminated\nphases: [").unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-badp/settings",
        json!({"phaseVerification": "x"}),
    );
    assert!(
        (400..500).contains(&s),
        "expected 4xx for malformed YAML, got {s}: {body}"
    );

    // Failed PUT must NOT have written to disk.
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.starts_with("title: 'unterminated"), "{after}");
}

#[test]
fn put_rejects_markdown_plan() {
    let d = TestDashboard::new();
    // A bare .md plan with no settings fields.
    let path = d.plans_dir.join("settings-md.md");
    std::fs::write(
        &path,
        "# Settings MD\n\n## Phase 1\n\n### Task 1.1\n\nDo a thing.\n",
    )
    .unwrap();

    let (s, body) = d.put(
        "/api/plans/settings-md/settings",
        json!({"phaseVerification": "x"}),
    );
    assert!(
        (400..500).contains(&s),
        "expected 4xx for markdown plan, got {s}: {body}"
    );
}
