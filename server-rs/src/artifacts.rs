//! Cross-plan artifact resolution (Phase 4 of the DAG-based plan model).
//!
//! A v2 plan can declare cross-plan dependencies via top-level `inputs:` and
//! `outputs:` (see [`crate::dag::PlanArtifact`]):
//!
//! ```yaml
//! inputs:
//!   - name: api-schema       # local name this plan refers to
//!     fromPlan: design-api    # the producing plan
//!     artifact: schema        # the output in that plan (defaults to `name`)
//! outputs:
//!   - name: wire-protocol
//! ```
//!
//! Resolution rides on the `plan_artifacts` table (Phase 1 schema):
//! `(plan_name, artifact_name, direction, value, satisfied_at)`.
//!
//! - A producing plan's **End** gate, on success, records each declared
//!   `outputs:` entry as `direction='output'` with a computed `value` (the
//!   merged commit SHA, a file hash, or a boolean marker) and a non-NULL
//!   `satisfied_at` ([`record_output_artifacts`]). It then re-advances any
//!   consumer whose Init gate was waiting on those outputs
//!   ([`reset_dependent_init_gates`]).
//! - A consuming plan's **Init** gate checks every declared input against the
//!   producing plan's satisfied output row ([`check_inputs_satisfied`]). Until
//!   they are all satisfied the gate stays `Blocked` with reason
//!   `waiting for <plan>/<artifact>` (built from [`first_unsatisfied_input`]).
//!
//! The Init/End gate wiring lives in [`crate::gates`]; this module is the pure
//! DB + plan-file resolution layer it calls into.

use std::path::Path;

use rusqlite::params;

use crate::dag::{DagNode, GateKind, NodeType, PlanArtifact};
use crate::db::Db;

// ── Input resolution ───────────────────────────────────────────────────────

/// Resolve the producing `(plan, artifact)` an input refers to.
///
/// The producing plan is `input.from_plan`; an input with no `from_plan` is a
/// malformed cross-plan reference we can't resolve a producer for. The produced
/// artifact name is `input.artifact` when set, else the input's own `name`
/// (the common case where the consumer reuses the upstream artifact's name).
///
/// Public so the plan-graph endpoint (`GET /api/plan-graph`) can render the
/// resolved producer + artifact for each declared input.
pub fn input_producer(input: &PlanArtifact) -> Option<(&str, &str)> {
    let from_plan = input.from_plan.as_deref()?;
    let artifact = input.artifact.as_deref().unwrap_or(input.name.as_str());
    Some((from_plan, artifact))
}

/// Connection-based core of [`output_satisfied`]: whether the producing plan
/// has recorded a satisfied output row for `artifact` (`direction='output'`
/// and `satisfied_at IS NOT NULL`).
///
/// Public so callers that already hold the DB lock (e.g. the plan-graph
/// endpoint, which checks many inputs/outputs under one lock) can reuse the
/// query without re-locking the non-reentrant mutex.
pub fn output_satisfied_conn(conn: &rusqlite::Connection, from_plan: &str, artifact: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM plan_artifacts \
         WHERE plan_name = ?1 AND artifact_name = ?2 \
           AND direction = 'output' AND satisfied_at IS NOT NULL",
        params![from_plan, artifact],
        |_| Ok(()),
    )
    .is_ok()
}

/// Whether the producing plan has recorded a satisfied output row for
/// `artifact`. Locks the DB internally; see [`output_satisfied_conn`] for the
/// already-locked variant.
fn output_satisfied(db: &Db, from_plan: &str, artifact: &str) -> bool {
    let conn = db.lock().unwrap();
    output_satisfied_conn(&conn, from_plan, artifact)
}

/// The first declared input not yet satisfied, as `(producing_plan, artifact)`.
/// Used by the Init gate to build its `waiting for <plan>/<artifact>` block
/// reason. An input with no `from_plan` can never resolve a producer, so it is
/// reported as waiting on `(unknown)/<name>`. Returns `None` when every input
/// is satisfied (or the slice is empty).
pub fn first_unsatisfied_input(db: &Db, inputs: &[PlanArtifact]) -> Option<(String, String)> {
    for input in inputs {
        match input_producer(input) {
            Some((from_plan, artifact)) => {
                if !output_satisfied(db, from_plan, artifact) {
                    return Some((from_plan.to_string(), artifact.to_string()));
                }
            }
            None => return Some(("(unknown)".to_string(), input.name.clone())),
        }
    }
    None
}

/// Whether every declared cross-plan input is satisfied — i.e. the producing
/// plan's End gate has recorded a satisfied output row for each one.
///
/// An empty `inputs` slice is vacuously satisfied (a plan with no cross-plan
/// dependencies). `plan_name` is the consuming plan; it is used only for log
/// context (the producing plan + artifact come from each input).
pub fn check_inputs_satisfied(db: &Db, plan_name: &str, inputs: &[PlanArtifact]) -> bool {
    match first_unsatisfied_input(db, inputs) {
        None => true,
        Some((from_plan, artifact)) => {
            eprintln!("[artifacts] plan '{plan_name}' input waiting for {from_plan}/{artifact}");
            false
        }
    }
}

// ── Output recording ───────────────────────────────────────────────────────

/// Record a plan's declared outputs as satisfied. Called by the End gate on
/// success: upserts `(plan_name, artifact_name, 'output', value, satisfied_at)`
/// for every declared output, stamping `satisfied_at` so downstream Init gates
/// see them.
///
/// `value` is the computed artifact value — the merged commit SHA (the End
/// gate's natural point-in-time marker), a file hash, or a boolean. `None`
/// (SaaS, or when HEAD can't resolve) is stored as the boolean marker `"true"`.
/// A re-run of the End gate updates the existing row (idempotent).
pub fn record_output_artifacts(
    db: &Db,
    plan_name: &str,
    outputs: &[PlanArtifact],
    value: Option<&str>,
) {
    if outputs.is_empty() {
        return;
    }
    let value = value.unwrap_or("true");
    let conn = db.lock().unwrap();
    for output in outputs {
        if let Err(e) = conn.execute(
            "INSERT INTO plan_artifacts (plan_name, artifact_name, direction, value, satisfied_at) \
             VALUES (?1, ?2, 'output', ?3, datetime('now')) \
             ON CONFLICT(plan_name, artifact_name, direction) \
             DO UPDATE SET value = excluded.value, satisfied_at = excluded.satisfied_at",
            params![plan_name, output.name, value],
        ) {
            eprintln!(
                "[artifacts] failed to record output '{}' for plan '{plan_name}': {e}",
                output.name
            );
        }
    }
}

// ── Dependent re-advance (push) ────────────────────────────────────────────

/// Plans that declare an input produced by `producing_plan` (i.e. an input
/// whose resolved `from_plan` equals `producing_plan`). Scans the top-level
/// plan files in `plans_dir` (archives under `plans_dir/archive/` are nested in
/// a sub-directory and so are skipped by the file check). Markdown / v1 plans
/// can't express `inputs:`, so they never match.
///
/// The producing plan itself is excluded (a plan can't depend on its own
/// output).
fn dependent_plans(plans_dir: &Path, producing_plan: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(plans_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !crate::plan_parser::is_plan_ext(&path) {
            continue;
        }
        let Ok(dag) = crate::plan_parser::parse_plan_file_as_dag(&path) else {
            continue;
        };
        if dag.name == producing_plan {
            continue;
        }
        let depends = dag
            .inputs
            .iter()
            .filter_map(input_producer)
            .any(|(from_plan, _)| from_plan == producing_plan);
        if depends {
            out.push(dag.name);
        }
    }
    out
}

/// Collect the scoped IDs of every `init` gate node in a node tree, walking
/// sub-plans with the same scoping convention the scheduler uses
/// (`parent.child`).
fn collect_init_gate_ids(nodes: &[DagNode], prefix: Option<&str>, out: &mut Vec<String>) {
    for node in nodes {
        let scoped = match prefix {
            Some(p) => format!("{p}.{}", node.id),
            None => node.id.clone(),
        };
        if node.node_type == NodeType::Gate && node.gate_kind == Some(GateKind::Init) {
            out.push(scoped.clone());
        }
        if node.node_type == NodeType::SubPlan && !node.nodes.is_empty() {
            collect_init_gate_ids(&node.nodes, Some(&scoped), out);
        }
    }
}

/// Re-advance any plan whose Init gate is blocked waiting on `producing_plan`'s
/// outputs (which were just recorded by [`record_output_artifacts`]).
///
/// For each dependent consumer whose inputs are now **all** satisfied, flip its
/// blocked Init gate(s) — node_status `in_progress` (the scheduler's claim
/// leaves a `Blocked` gate `in_progress`) — back to `pending` so a subsequent
/// `try_dag_advance` re-claims and re-runs them (now past the inputs check). A
/// consumer still waiting on a *different* producer is left alone (its inputs
/// aren't all satisfied yet).
///
/// Returns the consumer plan names that had a gate reset — the caller (the End
/// gate) spawns `try_dag_advance` for each. This is the producer-push analogue
/// of the approve / retry paths (reset → re-advance), and is pure DB + plan
/// files so it stays unit-testable.
pub fn reset_dependent_init_gates(db: &Db, plans_dir: &Path, producing_plan: &str) -> Vec<String> {
    let mut resumed = Vec::new();
    for consumer in dependent_plans(plans_dir, producing_plan) {
        let Some(path) = crate::plan_parser::find_plan_file(plans_dir, &consumer) else {
            continue;
        };
        let Ok(dag) = crate::plan_parser::parse_plan_file_as_dag(&path) else {
            continue;
        };
        // Only re-advance once every input is satisfied — a consumer that also
        // depends on a still-pending producer keeps waiting.
        if !check_inputs_satisfied(db, &consumer, &dag.inputs) {
            continue;
        }
        let mut init_ids = Vec::new();
        collect_init_gate_ids(&dag.nodes, None, &mut init_ids);
        let mut reset_any = false;
        {
            let conn = db.lock().unwrap();
            for id in &init_ids {
                let updated = conn
                    .execute(
                        "UPDATE node_status \
                         SET status = 'pending', source = 'auto', updated_at = datetime('now') \
                         WHERE plan_name = ?1 AND node_id = ?2 AND status = 'in_progress'",
                        params![consumer, id],
                    )
                    .unwrap_or(0);
                if updated > 0 {
                    reset_any = true;
                }
            }
        }
        if reset_any {
            resumed.push(consumer);
        }
    }
    resumed
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("branchwork.db");
        (crate::db::init(&path), dir)
    }

    /// A consuming input: `name` produced by `from_plan` (artifact name
    /// defaults to `name` unless `artifact` is given).
    fn input(name: &str, from_plan: &str, artifact: Option<&str>) -> PlanArtifact {
        PlanArtifact {
            name: name.to_string(),
            from_plan: Some(from_plan.to_string()),
            artifact: artifact.map(str::to_string),
            description: None,
        }
    }

    /// A produced output named `name`.
    fn output(name: &str) -> PlanArtifact {
        PlanArtifact {
            name: name.to_string(),
            from_plan: None,
            artifact: None,
            description: None,
        }
    }

    fn write_plan(plans_dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(plans_dir).unwrap();
        std::fs::write(plans_dir.join(format!("{name}.yaml")), body).unwrap();
    }

    fn seed_node_status(db: &Db, plan: &str, node: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, ?2, ?3)",
            params![plan, node, status],
        )
        .unwrap();
    }

    fn node_status(db: &Db, plan: &str, node: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = ?2",
            params![plan, node],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    // ── check_inputs_satisfied ─────────────────────────────────────────────

    #[test]
    fn empty_inputs_are_vacuously_satisfied() {
        let (db, _d) = fresh_db();
        assert!(check_inputs_satisfied(&db, "b", &[]));
        assert!(first_unsatisfied_input(&db, &[]).is_none());
    }

    #[test]
    fn unsatisfied_input_blocks_until_output_recorded() {
        let (db, _d) = fresh_db();
        let inputs = vec![input("schema", "a", None)];

        // No output recorded yet → not satisfied, waiting for a/schema.
        assert!(!check_inputs_satisfied(&db, "b", &inputs));
        assert_eq!(
            first_unsatisfied_input(&db, &inputs),
            Some(("a".to_string(), "schema".to_string()))
        );

        // Plan A records the output → now satisfied (acceptance core).
        record_output_artifacts(&db, "a", &[output("schema")], Some("deadbeef"));
        assert!(check_inputs_satisfied(&db, "b", &inputs));
        assert!(first_unsatisfied_input(&db, &inputs).is_none());
    }

    #[test]
    fn input_artifact_name_defaults_to_local_name() {
        let (db, _d) = fresh_db();
        // Consumer input named `schema`, no explicit `artifact:` → matches the
        // producer's output named `schema`.
        record_output_artifacts(&db, "a", &[output("schema")], None);
        assert!(check_inputs_satisfied(
            &db,
            "b",
            &[input("schema", "a", None)]
        ));
        // A different local name with `artifact: schema` also resolves.
        assert!(check_inputs_satisfied(
            &db,
            "b",
            &[input("api", "a", Some("schema"))]
        ));
        // A local name with no matching output stays unsatisfied.
        assert!(!check_inputs_satisfied(
            &db,
            "b",
            &[input("api", "a", None)]
        ));
    }

    #[test]
    fn input_without_a_producer_can_never_satisfy() {
        let (db, _d) = fresh_db();
        let inputs = vec![PlanArtifact {
            name: "orphan".to_string(),
            from_plan: None,
            artifact: None,
            description: None,
        }];
        assert!(!check_inputs_satisfied(&db, "b", &inputs));
        assert_eq!(
            first_unsatisfied_input(&db, &inputs),
            Some(("(unknown)".to_string(), "orphan".to_string()))
        );
    }

    #[test]
    fn multi_input_reports_the_first_unsatisfied() {
        let (db, _d) = fresh_db();
        record_output_artifacts(&db, "a", &[output("x")], Some("sha-a"));
        let inputs = vec![input("x", "a", None), input("y", "c", None)];
        // x is satisfied, y (from c) is not → block on c/y.
        assert!(!check_inputs_satisfied(&db, "b", &inputs));
        assert_eq!(
            first_unsatisfied_input(&db, &inputs),
            Some(("c".to_string(), "y".to_string()))
        );
    }

    // ── record_output_artifacts ────────────────────────────────────────────

    #[test]
    fn record_stores_value_and_satisfied_at_and_is_idempotent() {
        let (db, _d) = fresh_db();
        record_output_artifacts(&db, "a", &[output("schema")], Some("sha-1"));
        {
            let conn = db.lock().unwrap();
            let (value, satisfied): (String, Option<String>) = conn
                .query_row(
                    "SELECT value, satisfied_at FROM plan_artifacts \
                     WHERE plan_name='a' AND artifact_name='schema' AND direction='output'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(value, "sha-1");
            assert!(satisfied.is_some());
        }

        // Re-record updates the value in place (single row, ON CONFLICT).
        record_output_artifacts(&db, "a", &[output("schema")], Some("sha-2"));
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM plan_artifacts \
                 WHERE plan_name='a' AND artifact_name='schema' AND direction='output'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "upsert must not insert a duplicate row");
        let value: String = conn
            .query_row(
                "SELECT value FROM plan_artifacts \
                 WHERE plan_name='a' AND artifact_name='schema' AND direction='output'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "sha-2");
    }

    #[test]
    fn record_none_value_stores_boolean_marker() {
        let (db, _d) = fresh_db();
        record_output_artifacts(&db, "a", &[output("done")], None);
        let conn = db.lock().unwrap();
        let value: String = conn
            .query_row(
                "SELECT value FROM plan_artifacts \
                 WHERE plan_name='a' AND artifact_name='done' AND direction='output'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "true");
    }

    #[test]
    fn record_empty_outputs_is_a_noop() {
        let (db, _d) = fresh_db();
        record_output_artifacts(&db, "a", &[], Some("sha"));
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM plan_artifacts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // ── dependent_plans + reset_dependent_init_gates ───────────────────────

    /// A v2 consumer plan whose Init gate gates on an input from `producer`.
    /// Node-level keys are snake_case (`gate_kind`, `depends_on`); input-level
    /// keys are camelCase (`fromPlan`) per `PlanArtifact`'s serde rename.
    fn consumer_plan_yaml(producer: &str, artifact: &str) -> String {
        format!(
            "schema_version: 2\n\
             title: Consumer\n\
             inputs:\n\
             \x20 - name: {artifact}\n\
             \x20   fromPlan: {producer}\n\
             nodes:\n\
             \x20 - id: init\n\
             \x20   type: gate\n\
             \x20   gate_kind: init\n\
             \x20 - id: work\n\
             \x20   type: task\n\
             \x20   depends_on: [init]\n"
        )
    }

    #[test]
    fn dependent_plans_finds_consumers_referencing_the_producer() {
        let (_db, d) = fresh_db();
        let plans_dir = d.path().join("plans");
        write_plan(
            &plans_dir,
            "consumer",
            &consumer_plan_yaml("producer", "schema"),
        );
        // An unrelated plan that depends on a different producer.
        write_plan(
            &plans_dir,
            "other",
            &consumer_plan_yaml("elsewhere", "thing"),
        );

        let deps = dependent_plans(&plans_dir, "producer");
        assert_eq!(deps, vec!["consumer".to_string()]);
    }

    #[test]
    fn reset_flips_blocked_init_gate_back_to_pending() {
        let (db, d) = fresh_db();
        let plans_dir = d.path().join("plans");
        write_plan(
            &plans_dir,
            "consumer",
            &consumer_plan_yaml("producer", "schema"),
        );

        // Consumer's init gate is blocked (claimed → in_progress).
        seed_node_status(&db, "consumer", "init", "in_progress");

        // Producer records the output the consumer was waiting on.
        record_output_artifacts(&db, "producer", &[output("schema")], Some("sha"));

        let resumed = reset_dependent_init_gates(&db, &plans_dir, "producer");
        assert_eq!(resumed, vec!["consumer".to_string()]);
        assert_eq!(
            node_status(&db, "consumer", "init").as_deref(),
            Some("pending")
        );
    }

    #[test]
    fn reset_skips_consumer_still_waiting_on_another_producer() {
        let (db, d) = fresh_db();
        let plans_dir = d.path().join("plans");
        // Consumer depends on BOTH `producer` and `other`.
        let body = "schema_version: 2\n\
             title: Consumer\n\
             inputs:\n\
             \x20 - name: a\n\
             \x20   fromPlan: producer\n\
             \x20 - name: b\n\
             \x20   fromPlan: other\n\
             nodes:\n\
             \x20 - id: init\n\
             \x20   type: gate\n\
             \x20   gate_kind: init\n";
        write_plan(&plans_dir, "consumer", body);
        seed_node_status(&db, "consumer", "init", "in_progress");

        // Only `producer` has recorded its output → consumer still waits on `other`.
        record_output_artifacts(&db, "producer", &[output("a")], Some("sha"));
        let resumed = reset_dependent_init_gates(&db, &plans_dir, "producer");
        assert!(
            resumed.is_empty(),
            "must not re-advance while another input is unsatisfied"
        );
        assert_eq!(
            node_status(&db, "consumer", "init").as_deref(),
            Some("in_progress")
        );

        // Now `other` records its output too → consumer fully satisfied.
        record_output_artifacts(&db, "other", &[output("b")], Some("sha"));
        let resumed = reset_dependent_init_gates(&db, &plans_dir, "other");
        assert_eq!(resumed, vec!["consumer".to_string()]);
        assert_eq!(
            node_status(&db, "consumer", "init").as_deref(),
            Some("pending")
        );
    }

    #[test]
    fn reset_leaves_a_completed_init_gate_alone() {
        let (db, d) = fresh_db();
        let plans_dir = d.path().join("plans");
        write_plan(
            &plans_dir,
            "consumer",
            &consumer_plan_yaml("producer", "schema"),
        );
        // Init gate already completed (approved) — must not be re-opened.
        seed_node_status(&db, "consumer", "init", "completed");
        record_output_artifacts(&db, "producer", &[output("schema")], Some("sha"));

        let resumed = reset_dependent_init_gates(&db, &plans_dir, "producer");
        assert!(resumed.is_empty());
        assert_eq!(
            node_status(&db, "consumer", "init").as_deref(),
            Some("completed")
        );
    }

    /// Build a bare `DagNode` of `kind` (gate kind only set for gates).
    fn node(
        id: &str,
        ty: NodeType,
        gate_kind: Option<GateKind>,
        children: Vec<DagNode>,
    ) -> DagNode {
        DagNode {
            id: id.to_string(),
            node_type: ty,
            title: String::new(),
            description: String::new(),
            depends_on: Vec::new(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: false,
            gate_kind,
            checks: Vec::new(),
            workflows: Vec::new(),
            nodes: children,
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    #[test]
    fn collect_init_gate_ids_scopes_sub_plan_gates() {
        let nodes = vec![
            node("init", NodeType::Gate, Some(GateKind::Init), vec![]),
            node("end", NodeType::Gate, Some(GateKind::End), vec![]),
            node(
                "sub",
                NodeType::SubPlan,
                None,
                vec![node("init", NodeType::Gate, Some(GateKind::Init), vec![])],
            ),
        ];
        let mut ids = Vec::new();
        collect_init_gate_ids(&nodes, None, &mut ids);
        // Top-level init + the nested sub-plan's init (scoped), but not the
        // End gate.
        assert_eq!(ids, vec!["init".to_string(), "sub.init".to_string()]);
    }
}
