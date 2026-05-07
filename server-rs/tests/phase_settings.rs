//! E2E tests for `GET/PUT /api/plans/:name/phases/:number/settings`
//! (Task 3.3). The plan-level endpoint is covered by `plan_settings.rs`;
//! this file pins the per-phase override surface specifically.
//!
//! Covers:
//! - GET surfaces phase-level + plan-level + repo-level
//!   `phase_verification` so the UI can render the inheritance chain.
//! - PUT round-trips a phase-level override into the plan YAML
//!   (acceptance: visible in the diff).
//! - PUT with explicit `null` removes the override (Inherit).
//! - PUT only touches the targeted phase block — sibling phases stay
//!   pristine.
//! - 404 on unknown plan or phase, 4xx on Markdown plan / malformed YAML.

mod support;

use serde_json::json;
use support::TestDashboard;

fn yaml_two_phases(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "# Header comment that must survive every PUT.\n\
         title: {name}\n\
         project: {project}\n\
         context: ''\n\
         phase_verification: bash plan-level.sh\n\
         phases:\n  \
         - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: T11\n        description: ''\n        acceptance: ''\n  \
         - number: 2\n    title: Phase 2\n    description: ''\n    tasks:\n      - number: '2.1'\n        title: T21\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

#[test]
fn get_returns_phase_level_and_plan_level_inheritance() {
    let d = TestDashboard::new();
    d.create_plan("phs-inherit", &yaml_two_phases("phs-inherit", &d.project));

    let (s, body) = d.get("/api/plans/phs-inherit/phases/1/settings");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["phaseNumber"], 1);
    assert!(
        body["phaseVerification"].is_null(),
        "phase has no override yet: {body}"
    );
    assert_eq!(body["planVerification"], "bash plan-level.sh");
    assert!(body["repoVerification"].is_null(), "{body}");
}

#[test]
fn get_surfaces_repo_default_when_no_plan_or_phase_override() {
    let d = TestDashboard::new();
    d.create_plan("phs-repo", &yaml_two_phases("phs-repo", &d.project));
    std::fs::write(
        d.project.join("branchwork.toml"),
        "[phase]\nverification = \"make verify\"\n",
    )
    .unwrap();

    let (s, body) = d.get("/api/plans/phs-repo/phases/1/settings");
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["repoVerification"], "make verify");
}

#[test]
fn put_round_trips_phase_override_into_yaml() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("phs-rt.yaml");
    std::fs::write(&path, yaml_two_phases("phs-rt", &d.project)).unwrap();

    let (s, body) = d.put(
        "/api/plans/phs-rt/phases/1/settings",
        json!({"phaseVerification": "bash phase1.sh"}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["phaseVerification"], "bash phase1.sh");
    // Plan-level value still inherited (not clobbered).
    assert_eq!(body["planVerification"], "bash plan-level.sh");

    let after = std::fs::read_to_string(&path).unwrap();
    // The override must show up in the YAML so a `git diff` will see it.
    assert!(
        after.contains("phase_verification: bash phase1.sh"),
        "phase override not in YAML:\n{after}"
    );
    // Header comment survives the edit.
    assert!(after.contains("# Header comment that must survive every PUT."));
    // Plan-level field still there.
    assert!(after.contains("phase_verification: bash plan-level.sh"));
    // Sibling phase unchanged: parse and confirm phase 2 has no override.
    let v: serde_yaml::Value = serde_yaml::from_str(&after).expect("parses");
    let phases = v
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("phases".into())))
        .and_then(|p| p.as_sequence())
        .expect("phases sequence");
    let phase_two = phases
        .iter()
        .find(|p| {
            p.as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("number".into())))
                .and_then(|v| v.as_u64())
                == Some(2)
        })
        .expect("phase 2");
    assert!(
        phase_two
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("phase_verification".into())))
            .is_none(),
        "phase 2 must not have inherited the phase 1 edit:\n{after}"
    );
}

#[test]
fn put_explicit_null_removes_the_phase_override() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("phs-clear.yaml");
    std::fs::write(&path, yaml_two_phases("phs-clear", &d.project)).unwrap();

    // First, write an override.
    let (s, _) = d.put(
        "/api/plans/phs-clear/phases/2/settings",
        json!({"phaseVerification": "bash phase2.sh"}),
    );
    assert_eq!(s, 200);
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("phase_verification: bash phase2.sh"));

    // Then clear it.
    let (s, body) = d.put(
        "/api/plans/phs-clear/phases/2/settings",
        json!({"phaseVerification": null}),
    );
    assert_eq!(s, 200, "body: {body}");
    assert!(body["phaseVerification"].is_null());

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        !after.contains("phase_verification: bash phase2.sh"),
        "override not cleared:\n{after}"
    );
    // Plan-level field still present.
    assert!(after.contains("phase_verification: bash plan-level.sh"));
}

#[test]
fn put_partial_body_leaves_unspecified_field_alone() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("phs-partial.yaml");
    std::fs::write(&path, yaml_two_phases("phs-partial", &d.project)).unwrap();

    // Empty body must still 200 — no key was specified.
    let (s, body) = d.put("/api/plans/phs-partial/phases/1/settings", json!({}));
    assert_eq!(s, 200, "body: {body}");
    assert!(body["phaseVerification"].is_null(), "{body}");

    // File untouched.
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("# Header comment that must survive every PUT."));
}

#[test]
fn get_returns_404_for_unknown_phase() {
    let d = TestDashboard::new();
    d.create_plan("phs-404", &yaml_two_phases("phs-404", &d.project));
    let (s, body) = d.get("/api/plans/phs-404/phases/99/settings");
    assert_eq!(s, 404, "body: {body}");
    assert_eq!(body["error"], "phase_not_found");
}

#[test]
fn put_returns_404_for_unknown_phase() {
    let d = TestDashboard::new();
    d.create_plan("phs-404p", &yaml_two_phases("phs-404p", &d.project));
    let (s, body) = d.put(
        "/api/plans/phs-404p/phases/99/settings",
        json!({"phaseVerification": "x"}),
    );
    assert_eq!(s, 404, "body: {body}");
}

#[test]
fn get_returns_404_for_unknown_plan() {
    let d = TestDashboard::new();
    let (s, _) = d.get("/api/plans/no-such-plan/phases/1/settings");
    assert_eq!(s, 404);
}

#[test]
fn put_rejects_markdown_plan() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("phs-md.md");
    std::fs::write(
        &path,
        "# Settings MD\n\n## Phase 1\n\n### Task 1.1\n\nDo a thing.\n",
    )
    .unwrap();
    let (s, body) = d.put(
        "/api/plans/phs-md/phases/1/settings",
        json!({"phaseVerification": "x"}),
    );
    assert!(
        (400..500).contains(&s),
        "expected 4xx for markdown plan, got {s}: {body}"
    );
}

#[test]
fn put_returns_4xx_on_malformed_yaml() {
    let d = TestDashboard::new();
    let path = d.plans_dir.join("phs-bad.yaml");
    std::fs::write(&path, "title: 'unterminated\nphases: [").unwrap();
    let (s, body) = d.put(
        "/api/plans/phs-bad/phases/1/settings",
        json!({"phaseVerification": "x"}),
    );
    assert!(
        (400..500).contains(&s),
        "expected 4xx for malformed YAML, got {s}: {body}"
    );
    // Failed PUT must not have written.
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.starts_with("title: 'unterminated"), "{after}");
}
