//! End-to-end tests for `GET /api/plan-graph` — the cross-plan dependency
//! overlay the `MultiPlanBoard` graph view renders (Task 4.3 of the
//! dag-based-plan-model plan).
//!
//! The endpoint returns, for every `schema_version: 2` plan that declares
//! cross-plan `inputs:`/`outputs:`, the resolved producer for each input plus
//! the satisfaction state of each input/output (read from `plan_artifacts`).
//! Plans without cross-plan artifacts — including every v1 plan — are omitted.

mod support;

use serde_json::Value;
use support::TestDashboard;

/// A v2 producer plan that declares a single `outputs:` entry named `schema`.
fn producer_plan(name: &str, project: &str) -> String {
    // Raw string with literal column-0 indentation — a `\<newline>` format!
    // continuation would strip the leading whitespace and flatten the YAML.
    // Input/output keys are camelCase (`fromPlan`) per `PlanArtifact`'s serde
    // rename; node-level keys (`gate_kind`, `depends_on`) are snake_case.
    format!(
        r#"schema_version: 2
title: "{name} title"
project: {project}
outputs:
  - name: schema
nodes:
  - id: init
    type: gate
    gate_kind: init
  - id: a
    type: task
    depends_on: [init]
  - id: end
    type: gate
    gate_kind: end
    depends_on: [a]
"#
    )
}

/// A v2 consumer plan whose Init gate gates on `schema` produced by `producer`.
fn consumer_plan(name: &str, project: &str, producer: &str) -> String {
    format!(
        r#"schema_version: 2
title: "{name} title"
project: {project}
inputs:
  - name: schema
    fromPlan: {producer}
nodes:
  - id: init
    type: gate
    gate_kind: init
  - id: b
    type: task
    depends_on: [init]
  - id: end
    type: gate
    gate_kind: end
    depends_on: [b]
"#
    )
}

/// A v2 plan with no cross-plan inputs or outputs (must be omitted).
fn standalone_v2_plan(name: &str, project: &str) -> String {
    format!(
        r#"schema_version: 2
title: "{name} title"
project: {project}
nodes:
  - id: a
    type: task
"#
    )
}

/// A legacy v1 (phase-based) plan (must be omitted — can't express artifacts).
fn v1_plan(name: &str, project: &str) -> String {
    format!(
        r#"title: "{name} title"
project: {project}
phases:
  - number: 1
    title: "Phase 1"
    tasks:
      - number: "1.1"
        title: "Do a thing"
"#
    )
}

fn write_plan(d: &TestDashboard, name: &str, body: String) {
    std::fs::write(d.plans_dir.join(format!("{name}.yaml")), body).unwrap();
}

fn db_conn(d: &TestDashboard) -> rusqlite::Connection {
    rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap()
}

/// Simulate a producer's End gate recording a satisfied output row.
fn record_output(d: &TestDashboard, plan: &str, artifact: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO plan_artifacts \
             (plan_name, artifact_name, direction, value, satisfied_at) \
             VALUES (?1, ?2, 'output', 'sha', datetime('now')) \
             ON CONFLICT(plan_name, artifact_name, direction) \
             DO UPDATE SET value = excluded.value, satisfied_at = excluded.satisfied_at",
            rusqlite::params![plan, artifact],
        )
        .unwrap();
}

/// Find the entry for `plan` in the response array.
fn entry<'a>(body: &'a Value, plan: &str) -> Option<&'a Value> {
    body.as_array()?
        .iter()
        .find(|e| e["plan"].as_str() == Some(plan))
}

// ── Acceptance: two interdependent plans, one blocked ────────────────────────

#[test]
fn interdependent_plans_expose_edge_and_block_state() {
    let d = TestDashboard::new();
    write_plan(
        &d,
        "producer-acc",
        producer_plan("producer-acc", "graph-acc"),
    );
    write_plan(
        &d,
        "consumer-acc",
        consumer_plan("consumer-acc", "graph-acc", "producer-acc"),
    );

    // Before the producer records its output: consumer input is unsatisfied,
    // producer output is unsatisfied. This is the "blocked plan shows waiting
    // for X from Y" case the acceptance criterion calls for.
    let (code, body) = d.get("/api/plan-graph");
    assert_eq!(code, 200, "body: {body}");

    let consumer = entry(&body, "consumer-acc").expect("consumer entry present");
    let inputs = consumer["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["name"], "schema");
    assert_eq!(inputs[0]["fromPlan"], "producer-acc");
    assert_eq!(inputs[0]["artifact"], "schema");
    assert_eq!(
        inputs[0]["satisfied"], false,
        "input must be unsatisfied before the producer records output"
    );
    assert!(consumer["outputs"].as_array().unwrap().is_empty());

    let producer = entry(&body, "producer-acc").expect("producer entry present");
    assert!(producer["inputs"].as_array().unwrap().is_empty());
    let outputs = producer["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["name"], "schema");
    assert_eq!(outputs[0]["satisfied"], false);

    // Producer's End gate records the output → consumer input + producer
    // output both flip to satisfied.
    record_output(&d, "producer-acc", "schema");
    let (code, body) = d.get("/api/plan-graph");
    assert_eq!(code, 200);
    let consumer = entry(&body, "consumer-acc").unwrap();
    assert_eq!(
        consumer["inputs"][0]["satisfied"], true,
        "input satisfied once producer recorded output"
    );
    let producer = entry(&body, "producer-acc").unwrap();
    assert_eq!(producer["outputs"][0]["satisfied"], true);
}

// ── Omissions: standalone v2 + v1 plans don't appear ─────────────────────────

#[test]
fn plans_without_artifacts_are_omitted() {
    let d = TestDashboard::new();
    write_plan(
        &d,
        "producer-omit",
        producer_plan("producer-omit", "graph-omit"),
    );
    write_plan(
        &d,
        "standalone-omit",
        standalone_v2_plan("standalone-omit", "graph-omit"),
    );
    write_plan(&d, "legacy-omit", v1_plan("legacy-omit", "graph-omit"));

    let (code, body) = d.get("/api/plan-graph");
    assert_eq!(code, 200, "body: {body}");
    let arr = body.as_array().unwrap();
    // Only the producer (it declares an output) shows up.
    assert_eq!(arr.len(), 1, "got {arr:?}");
    assert_eq!(arr[0]["plan"], "producer-omit");
}

// ── Malformed input (no producer) ────────────────────────────────────────────

#[test]
fn input_without_a_producer_renders_unsatisfied_with_null_producer() {
    let d = TestDashboard::new();
    let body = r#"schema_version: 2
title: "Orphan consumer"
project: graph-orphan
inputs:
  - name: dangling
nodes:
  - id: init
    type: gate
    gate_kind: init
"#
    .to_string();
    write_plan(&d, "orphan-consumer", body);

    let (code, body) = d.get("/api/plan-graph");
    assert_eq!(code, 200, "body: {body}");
    let consumer = entry(&body, "orphan-consumer").expect("present");
    let inp = &consumer["inputs"][0];
    assert_eq!(inp["name"], "dangling");
    assert!(inp["fromPlan"].is_null(), "no producer → null fromPlan");
    assert!(inp["artifact"].is_null());
    assert_eq!(inp["satisfied"], false);
}

// ── Resolved artifact name when input renames it ─────────────────────────────

#[test]
fn input_artifact_override_resolves_the_produced_name() {
    let d = TestDashboard::new();
    write_plan(
        &d,
        "producer-rename",
        producer_plan("producer-rename", "graph-rename"),
    );
    // Consumer's local input name is `api`, but it points at the producer's
    // `schema` output via an explicit `artifact:` override.
    let body = r#"schema_version: 2
title: "Renaming consumer"
project: graph-rename
inputs:
  - name: api
    fromPlan: producer-rename
    artifact: schema
nodes:
  - id: init
    type: gate
    gate_kind: init
"#
    .to_string();
    write_plan(&d, "consumer-rename", body);
    record_output(&d, "producer-rename", "schema");

    let (code, body) = d.get("/api/plan-graph");
    assert_eq!(code, 200, "body: {body}");
    let consumer = entry(&body, "consumer-rename").unwrap();
    let inp = &consumer["inputs"][0];
    assert_eq!(inp["name"], "api", "local name preserved");
    assert_eq!(inp["fromPlan"], "producer-rename");
    assert_eq!(inp["artifact"], "schema", "resolved produced name");
    assert_eq!(
        inp["satisfied"], true,
        "override resolves to the producer's recorded output"
    );
}
