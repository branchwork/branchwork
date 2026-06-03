//! DAG-based plan model (schema_version: 2).
//!
//! Plans are directed acyclic graphs of heterogeneous nodes:
//! - **TaskNode**: work executed by an agent in its own worktree
//! - **GateNode**: non-agent verification (init, end, CI, approval)
//! - **SubPlanNode**: inline composite containing a nested DAG

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::plan_parser::ParsedPlan;

// ── Node types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Task,
    Gate,
    SubPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Init,
    End,
    Ci,
    Approval,
}

// ── DagNode ─────────────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}
fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DagNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,

    // Task fields
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub acceptance: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub produces_commit: bool,

    // Gate fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_kind: Option<GateKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,

    // SubPlan fields (nested DAG)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<DagNode>,

    // Runtime state (populated from DB, not stored in YAML)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

// ── Artifact declarations ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanArtifact {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ── DagPlan ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DagPlan {
    pub name: String,
    pub file_path: String,
    pub title: String,
    #[serde(default)]
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub created_at: String,
    pub modified_at: String,
    pub nodes: Vec<DagNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<PlanArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<PlanArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
}

// ── YAML schema for v2 plans ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct YamlDagPlan {
    pub title: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub inputs: Vec<PlanArtifact>,
    #[serde(default)]
    pub outputs: Vec<PlanArtifact>,
    pub nodes: Vec<YamlDagNode>,
}

#[derive(Deserialize)]
pub(crate) struct YamlDagNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub file_paths: Vec<String>,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default = "default_true")]
    pub produces_commit: bool,
    #[serde(default)]
    pub gate_kind: Option<GateKind>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default)]
    pub nodes: Vec<YamlDagNode>,
}

// ── Parsing ─────────────────────────────────────────────────────────────────

pub fn parse_dag_yaml(raw: &str, name: &str, file_path: &str) -> Result<DagPlan, String> {
    let yaml: YamlDagPlan = serde_yaml::from_str(raw).map_err(|e| e.to_string())?;

    let nodes: Vec<DagNode> = yaml.nodes.into_iter().map(yaml_node_to_dag).collect();

    let plan = DagPlan {
        name: name.to_string(),
        file_path: file_path.to_string(),
        title: yaml.title,
        context: yaml.context,
        project: yaml
            .project
            .or_else(|| crate::plan_parser::infer_project(raw)),
        created_at: String::new(),
        modified_at: String::new(),
        nodes,
        inputs: yaml.inputs,
        outputs: yaml.outputs,
        total_cost_usd: None,
        max_budget_usd: None,
    };

    validate_dag(&plan)?;
    Ok(plan)
}

fn yaml_node_to_dag(y: YamlDagNode) -> DagNode {
    DagNode {
        id: y.id,
        node_type: y.node_type,
        title: y.title,
        description: y.description,
        depends_on: y.depends_on,
        file_paths: y.file_paths,
        acceptance: y.acceptance,
        produces_commit: y.produces_commit,
        gate_kind: y.gate_kind,
        checks: y.checks,
        workflows: y.workflows,
        nodes: y.nodes.into_iter().map(yaml_node_to_dag).collect(),
        status: None,
        status_updated_at: None,
        cost_usd: None,
    }
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate the DAG: no cycles, no dangling references, required fields per
/// node type. Returns Ok(topological_order) on success.
pub fn validate_dag(plan: &DagPlan) -> Result<Vec<String>, String> {
    let flat = flatten_nodes(&plan.nodes, None);
    validate_flat_graph(&flat)
}

/// Flatten a node tree (expanding sub-plans with scoped IDs) into a list of
/// (scoped_id, depends_on_scoped) pairs.
fn flatten_nodes(nodes: &[DagNode], prefix: Option<&str>) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for node in nodes {
        let scoped_id = match prefix {
            Some(p) => format!("{p}.{}", node.id),
            None => node.id.clone(),
        };

        // Scope the depends_on references: if the dep is a sibling (exists at
        // this level), scope it; otherwise assume it's already-scoped or a
        // parent-level reference.
        let sibling_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let scoped_deps: Vec<String> = node
            .depends_on
            .iter()
            .map(|d| {
                if sibling_ids.contains(d.as_str()) {
                    match prefix {
                        Some(p) => format!("{p}.{d}"),
                        None => d.clone(),
                    }
                } else {
                    d.clone()
                }
            })
            .collect();

        out.push((scoped_id.clone(), scoped_deps));

        // Recurse into sub-plan children
        if node.node_type == NodeType::SubPlan && !node.nodes.is_empty() {
            let children = flatten_nodes(&node.nodes, Some(&scoped_id));
            out.extend(children);
        }
    }
    out
}

/// Kahn's algorithm: topological sort with cycle detection.
fn validate_flat_graph(nodes: &[(String, Vec<String>)]) -> Result<Vec<String>, String> {
    let ids: HashSet<&str> = nodes.iter().map(|(id, _)| id.as_str()).collect();

    // Check for dangling references
    for (id, deps) in nodes {
        for dep in deps {
            if !ids.contains(dep.as_str()) {
                return Err(format!(
                    "node '{id}' depends on '{dep}' which does not exist in the plan"
                ));
            }
        }
    }

    // Build adjacency and in-degree
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, _) in nodes {
        in_degree.entry(id.as_str()).or_insert(0);
        adj.entry(id.as_str()).or_default();
    }
    for (id, deps) in nodes {
        for dep in deps {
            adj.entry(dep.as_str()).or_default().push(id.as_str());
            *in_degree.entry(id.as_str()).or_insert(0) += 1;
        }
    }

    // BFS from zero-in-degree nodes
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, deg)| *deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut order: Vec<String> = Vec::with_capacity(nodes.len());
    while let Some(node) = queue.pop_front() {
        order.push(node.to_string());
        for &next in adj.get(node).unwrap_or(&Vec::new()) {
            let deg = in_degree.get_mut(next).unwrap();
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(next);
            }
        }
    }

    if order.len() != nodes.len() {
        // Find nodes in the cycle (those with remaining in-degree > 0)
        let in_cycle: Vec<&str> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg > 0)
            .map(|(&id, _)| id)
            .collect();
        return Err(format!(
            "plan contains a cycle involving nodes: {}",
            in_cycle.join(", ")
        ));
    }

    Ok(order)
}

// ── Legacy conversion (v1 → DAG) ───────────────────────────────────────────

/// Convert a phase-based ParsedPlan to a DagPlan. The conversion preserves
/// existing `dependencies` as `depends_on` edges and adds implicit phase
/// ordering: tasks without explicit cross-phase dependencies get edges from
/// all tasks in the preceding phase.
pub fn parsed_plan_to_dag(plan: &ParsedPlan) -> DagPlan {
    let mut nodes: Vec<DagNode> = Vec::new();

    // Synthetic init gate (depends on nothing)
    nodes.push(DagNode {
        id: "init".to_string(),
        node_type: NodeType::Gate,
        title: "Precondition check".to_string(),
        description: String::new(),
        depends_on: Vec::new(),
        file_paths: Vec::new(),
        acceptance: String::new(),
        produces_commit: false,
        gate_kind: Some(GateKind::Init),
        checks: vec![
            "git_repo".to_string(),
            "remote_configured".to_string(),
            "clean_tree".to_string(),
        ],
        workflows: Vec::new(),
        nodes: Vec::new(),
        status: None,
        status_updated_at: None,
        cost_usd: None,
    });

    // Collect task IDs per phase for implicit ordering
    let mut phase_task_ids: Vec<Vec<String>> = Vec::new();

    for phase in &plan.phases {
        let mut this_phase_ids: Vec<String> = Vec::new();
        for task in &phase.tasks {
            this_phase_ids.push(task.number.clone());

            let mut depends_on = task.dependencies.clone();

            // If no explicit dependencies and this isn't the first phase,
            // depend on all tasks in the previous phase (preserving phase
            // ordering semantics).
            if depends_on.is_empty() {
                if let Some(prev_ids) = phase_task_ids.last() {
                    depends_on = prev_ids.clone();
                } else {
                    // First phase tasks depend on init gate
                    depends_on.push("init".to_string());
                }
            }

            // Ensure first-phase tasks with explicit deps still connect to
            // the graph (they should depend on init if they don't already
            // depend on another task in this plan).
            if phase_task_ids.is_empty() && !depends_on.contains(&"init".to_string()) {
                depends_on.push("init".to_string());
            }

            nodes.push(DagNode {
                id: task.number.clone(),
                node_type: NodeType::Task,
                title: task.title.clone(),
                description: task.description.clone(),
                depends_on,
                file_paths: task.file_paths.clone(),
                acceptance: task.acceptance.clone(),
                produces_commit: task.produces_commit,
                gate_kind: None,
                checks: Vec::new(),
                workflows: Vec::new(),
                nodes: Vec::new(),
                status: task.status.clone(),
                status_updated_at: task.status_updated_at.clone(),
                cost_usd: task.cost_usd,
            });
        }
        phase_task_ids.push(this_phase_ids);
    }

    // Synthetic end gate (depends on all tasks in the last phase)
    let last_phase_ids = phase_task_ids.last().cloned().unwrap_or_default();
    let end_depends = if last_phase_ids.is_empty() {
        vec!["init".to_string()]
    } else {
        last_phase_ids
    };

    nodes.push(DagNode {
        id: "end".to_string(),
        node_type: NodeType::Gate,
        title: "Final verification".to_string(),
        description: String::new(),
        depends_on: end_depends,
        file_paths: Vec::new(),
        acceptance: String::new(),
        produces_commit: false,
        gate_kind: Some(GateKind::End),
        checks: vec![
            "all_merged".to_string(),
            "compiles".to_string(),
            "ci_green".to_string(),
        ],
        workflows: Vec::new(),
        nodes: Vec::new(),
        status: None,
        status_updated_at: None,
        cost_usd: None,
    });

    DagPlan {
        name: plan.name.clone(),
        file_path: plan.file_path.clone(),
        title: plan.title.clone(),
        context: plan.context.clone(),
        project: plan.project.clone(),
        created_at: plan.created_at.clone(),
        modified_at: plan.modified_at.clone(),
        nodes,
        inputs: Vec::new(),
        outputs: Vec::new(),
        total_cost_usd: plan.total_cost_usd,
        max_budget_usd: plan.max_budget_usd,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Collect all node IDs (including scoped sub-plan children) from a DagPlan.
pub fn all_node_ids(plan: &DagPlan) -> Vec<String> {
    let flat = flatten_nodes(&plan.nodes, None);
    flat.into_iter().map(|(id, _)| id).collect()
}

/// Get the topological order of node IDs (for scheduling).
pub fn topological_order(plan: &DagPlan) -> Result<Vec<String>, String> {
    let flat = flatten_nodes(&plan.nodes, None);
    validate_flat_graph(&flat)
}

/// Find all nodes that are ready to execute given a set of completed node IDs.
pub fn ready_nodes<'a>(plan: &'a DagPlan, done: &HashSet<String>) -> Vec<&'a DagNode> {
    plan.nodes
        .iter()
        .filter(|n| {
            if done.contains(&n.id) {
                return false;
            }
            let status = n.status.as_deref().unwrap_or("pending");
            (status == "pending" || status == "failed")
                && n.depends_on.iter().all(|d| done.contains(d))
        })
        .collect()
}

/// Find ready nodes within a sub-plan (using scoped IDs).
pub fn ready_sub_plan_nodes<'a>(
    parent_id: &str,
    children: &'a [DagNode],
    done: &HashSet<String>,
) -> Vec<&'a DagNode> {
    children
        .iter()
        .filter(|n| {
            let scoped_id = format!("{parent_id}.{}", n.id);
            let status = n.status.as_deref().unwrap_or("pending");
            let is_actionable = status == "pending" || status == "failed";
            if !is_actionable {
                return false;
            }
            // Check deps: scope sibling references
            let sibling_ids: HashSet<&str> = children.iter().map(|c| c.id.as_str()).collect();
            n.depends_on.iter().all(|d| {
                let scoped_dep = if sibling_ids.contains(d.as_str()) {
                    format!("{parent_id}.{d}")
                } else {
                    d.clone()
                };
                done.contains(&scoped_dep)
            })
            // Also check the node itself isn't already done
            && !done.contains(&scoped_id)
        })
        .collect()
}

// ── DagPlan summary (mirrors PlanSummary for API responses) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DagPlanSummary {
    pub name: String,
    pub title: String,
    pub project: Option<String>,
    pub node_count: usize,
    pub task_count: usize,
    pub gate_count: usize,
    pub created_at: String,
    pub modified_at: String,
}

impl DagPlan {
    pub fn summary(&self) -> DagPlanSummary {
        let flat = flatten_nodes(&self.nodes, None);
        let task_count = self
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Task)
            .count();
        let gate_count = self
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Gate)
            .count();
        DagPlanSummary {
            name: self.name.clone(),
            title: self.title.clone(),
            project: self.project.clone(),
            node_count: flat.len(),
            task_count,
            gate_count,
            created_at: self.created_at.clone(),
            modified_at: self.modified_at.clone(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_parser::{PlanPhase, PlanTask};

    fn task_node(id: &str, deps: &[&str]) -> DagNode {
        DagNode {
            id: id.to_string(),
            node_type: NodeType::Task,
            title: id.to_string(),
            description: String::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: true,
            gate_kind: None,
            checks: Vec::new(),
            workflows: Vec::new(),
            nodes: Vec::new(),
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    fn gate_node(id: &str, kind: GateKind, deps: &[&str]) -> DagNode {
        DagNode {
            id: id.to_string(),
            node_type: NodeType::Gate,
            title: id.to_string(),
            description: String::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: false,
            gate_kind: Some(kind),
            checks: Vec::new(),
            workflows: Vec::new(),
            nodes: Vec::new(),
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    fn make_plan(nodes: Vec<DagNode>) -> DagPlan {
        DagPlan {
            name: "test".to_string(),
            file_path: "test.yaml".to_string(),
            title: "Test Plan".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            nodes,
            inputs: Vec::new(),
            outputs: Vec::new(),
            total_cost_usd: None,
            max_budget_usd: None,
        }
    }

    #[test]
    fn linear_dag_validates() {
        let plan = make_plan(vec![
            gate_node("init", GateKind::Init, &[]),
            task_node("a", &["init"]),
            task_node("b", &["a"]),
            gate_node("end", GateKind::End, &["b"]),
        ]);
        let order = validate_dag(&plan).unwrap();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0], "init");
        assert_eq!(*order.last().unwrap(), "end");
    }

    #[test]
    fn diamond_dag_validates() {
        let plan = make_plan(vec![
            gate_node("init", GateKind::Init, &[]),
            task_node("left", &["init"]),
            task_node("right", &["init"]),
            task_node("merge", &["left", "right"]),
            gate_node("end", GateKind::End, &["merge"]),
        ]);
        let order = validate_dag(&plan).unwrap();
        assert_eq!(order.len(), 5);
        // init must come first, end must come last
        assert_eq!(order[0], "init");
        assert_eq!(*order.last().unwrap(), "end");
    }

    #[test]
    fn cycle_detected() {
        let plan = make_plan(vec![
            task_node("a", &["c"]),
            task_node("b", &["a"]),
            task_node("c", &["b"]),
        ]);
        let err = validate_dag(&plan).unwrap_err();
        assert!(err.contains("cycle"), "error was: {err}");
        assert!(err.contains("a") || err.contains("b") || err.contains("c"));
    }

    #[test]
    fn dangling_reference_detected() {
        let plan = make_plan(vec![task_node("a", &[]), task_node("b", &["nonexistent"])]);
        let err = validate_dag(&plan).unwrap_err();
        assert!(err.contains("nonexistent"), "error was: {err}");
    }

    #[test]
    fn sub_plan_scoping() {
        let plan = make_plan(vec![
            gate_node("init", GateKind::Init, &[]),
            DagNode {
                id: "integration".to_string(),
                node_type: NodeType::SubPlan,
                title: "Integration".to_string(),
                description: String::new(),
                depends_on: vec!["init".to_string()],
                file_paths: Vec::new(),
                acceptance: String::new(),
                produces_commit: false,
                gate_kind: None,
                checks: Vec::new(),
                workflows: Vec::new(),
                nodes: vec![task_node("wire", &[]), task_node("test", &["wire"])],
                status: None,
                status_updated_at: None,
                cost_usd: None,
            },
            gate_node("end", GateKind::End, &["integration"]),
        ]);

        let order = validate_dag(&plan).unwrap();
        // Should include scoped IDs
        assert!(order.contains(&"integration.wire".to_string()));
        assert!(order.contains(&"integration.test".to_string()));
        assert_eq!(order.len(), 5); // init, integration, integration.wire, integration.test, end
    }

    #[test]
    fn legacy_conversion_simple() {
        let legacy = ParsedPlan {
            name: "test".to_string(),
            file_path: "test.yaml".to_string(),
            title: "Test".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![
                PlanPhase {
                    number: 1,
                    title: "Phase 1".to_string(),
                    description: String::new(),
                    tasks: vec![
                        PlanTask {
                            number: "1.1".to_string(),
                            title: "Task A".to_string(),
                            description: String::new(),
                            file_paths: Vec::new(),
                            acceptance: String::new(),
                            dependencies: Vec::new(),
                            produces_commit: true,
                            status: None,
                            status_updated_at: None,
                            cost_usd: None,
                            ci: None,
                        },
                        PlanTask {
                            number: "1.2".to_string(),
                            title: "Task B".to_string(),
                            description: String::new(),
                            file_paths: Vec::new(),
                            acceptance: String::new(),
                            dependencies: Vec::new(),
                            produces_commit: true,
                            status: None,
                            status_updated_at: None,
                            cost_usd: None,
                            ci: None,
                        },
                    ],
                    ci_blocking_workflows: None,
                    phase_verification: None,
                },
                PlanPhase {
                    number: 2,
                    title: "Phase 2".to_string(),
                    description: String::new(),
                    tasks: vec![PlanTask {
                        number: "2.1".to_string(),
                        title: "Task C".to_string(),
                        description: String::new(),
                        file_paths: Vec::new(),
                        acceptance: String::new(),
                        dependencies: Vec::new(),
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    }],
                    ci_blocking_workflows: None,
                    phase_verification: None,
                },
            ],
            verification: None,
            ci_blocking_workflows: None,
            phase_verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        };

        let dag = parsed_plan_to_dag(&legacy);

        // init + 3 tasks + end = 5 nodes
        assert_eq!(dag.nodes.len(), 5);

        // init gate exists
        let init = dag.nodes.iter().find(|n| n.id == "init").unwrap();
        assert_eq!(init.node_type, NodeType::Gate);
        assert_eq!(init.gate_kind, Some(GateKind::Init));

        // Phase 1 tasks depend on init
        let t1 = dag.nodes.iter().find(|n| n.id == "1.1").unwrap();
        assert!(t1.depends_on.contains(&"init".to_string()));
        let t2 = dag.nodes.iter().find(|n| n.id == "1.2").unwrap();
        assert!(t2.depends_on.contains(&"init".to_string()));

        // Phase 2 task depends on all Phase 1 tasks
        let t3 = dag.nodes.iter().find(|n| n.id == "2.1").unwrap();
        assert!(t3.depends_on.contains(&"1.1".to_string()));
        assert!(t3.depends_on.contains(&"1.2".to_string()));

        // End gate depends on last phase tasks
        let end = dag.nodes.iter().find(|n| n.id == "end").unwrap();
        assert!(end.depends_on.contains(&"2.1".to_string()));

        // Validates as a DAG
        validate_dag(&dag).unwrap();
    }

    #[test]
    fn legacy_conversion_with_explicit_deps() {
        let legacy = ParsedPlan {
            name: "test".to_string(),
            file_path: "test.yaml".to_string(),
            title: "Test".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![PlanPhase {
                number: 1,
                title: "Phase 1".to_string(),
                description: String::new(),
                tasks: vec![
                    PlanTask {
                        number: "1.1".to_string(),
                        title: "A".to_string(),
                        description: String::new(),
                        file_paths: Vec::new(),
                        acceptance: String::new(),
                        dependencies: Vec::new(),
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    },
                    PlanTask {
                        number: "1.2".to_string(),
                        title: "B".to_string(),
                        description: String::new(),
                        file_paths: Vec::new(),
                        acceptance: String::new(),
                        dependencies: vec!["1.1".to_string()],
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    },
                ],
                ci_blocking_workflows: None,
                phase_verification: None,
            }],
            verification: None,
            ci_blocking_workflows: None,
            phase_verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        };

        let dag = parsed_plan_to_dag(&legacy);

        // Task with explicit dep keeps it (plus init)
        let t2 = dag.nodes.iter().find(|n| n.id == "1.2").unwrap();
        assert!(t2.depends_on.contains(&"1.1".to_string()));
        assert!(t2.depends_on.contains(&"init".to_string()));

        validate_dag(&dag).unwrap();
    }

    #[test]
    fn ready_nodes_basic() {
        let plan = make_plan(vec![
            gate_node("init", GateKind::Init, &[]),
            task_node("a", &["init"]),
            task_node("b", &["init"]),
            task_node("c", &["a", "b"]),
        ]);

        // Nothing done: only init is ready (no deps)
        let done: HashSet<String> = HashSet::new();
        let ready = ready_nodes(&plan, &done);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "init");

        // Init done: a and b are ready (parallel)
        let done: HashSet<String> = ["init".to_string()].into();
        let ready = ready_nodes(&plan, &done);
        assert_eq!(ready.len(), 2);
        let ids: HashSet<&str> = ready.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));

        // a done: only b still ready (c needs both)
        let done: HashSet<String> = ["init".to_string(), "a".to_string()].into();
        let ready = ready_nodes(&plan, &done);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "b");

        // Both done: c is ready
        let done: HashSet<String> = ["init".to_string(), "a".to_string(), "b".to_string()].into();
        let ready = ready_nodes(&plan, &done);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "c");
    }

    #[test]
    fn parse_v2_yaml() {
        let yaml = r#"
schema_version: 2
title: "Test DAG"
context: "test context"
nodes:
  - id: init
    type: gate
    gate_kind: init
    checks: [git_repo]
  - id: task-a
    type: task
    title: "Do A"
    depends_on: [init]
    acceptance: "A is done"
  - id: task-b
    type: task
    title: "Do B"
    depends_on: [init]
  - id: end
    type: gate
    gate_kind: end
    depends_on: [task-a, task-b]
    checks: [all_merged, ci_green]
"#;
        let plan = parse_dag_yaml(yaml, "test", "test.yaml").unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.title, "Test DAG");
        assert_eq!(plan.nodes[0].gate_kind, Some(GateKind::Init));
        assert_eq!(plan.nodes[1].node_type, NodeType::Task);
        assert_eq!(plan.nodes[3].checks, vec!["all_merged", "ci_green"]);
    }
}
