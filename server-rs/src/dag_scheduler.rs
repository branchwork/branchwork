//! DAG scheduler (schema_version: 2 plans).
//!
//! Phase 2 of the DAG-based plan model. [`try_dag_advance`] replaces the
//! phase-walking [`crate::agents::try_auto_advance`] with pure graph
//! evaluation: it loads the [`DagPlan`], snapshots node statuses from the
//! `node_status` table, finds every actionable node via
//! [`crate::dag::ready_nodes`], and dispatches by node type — **spawning all
//! ready task nodes in parallel** (no `break`; worktree-per-agent isolation
//! makes concurrent agents safe).
//!
//! Dispatch by node type:
//! - **Task**: claim ([`claim_node`]) + spawn an agent in its own worktree.
//! - **Gate**: claim + run the Phase 3 engine ([`crate::gates::execute_gate`])
//!   — `Passed` ⇒ complete + advance, `Failed` ⇒ fail the node, `Blocked` ⇒
//!   leave `in_progress` (completion arrives out-of-band via the approval
//!   API or the CI poller; the scheduler does not re-poll). Gate execution
//!   needs the wired [`crate::state::AppState`]; when it's absent (pure unit
//!   tests) the gate is left pending — graceful degradation.
//! - **SubPlan**: claim the parent active + recurse into its children
//!   (scoped IDs `<parent>.<child>`). Parent-completion propagation is
//!   Phase 2.3.
//!
//! The mode router that decides v1 (phase walker) vs v2 (this scheduler) per
//! plan is Phase 2.2.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde_json::json;

use crate::agents::pty_agent::{self, StartPtyOpts};
use crate::agents::{AgentRegistry, auto_advance_enabled, build_task_prompt, remaining_budget};
use crate::config::Effort;
use crate::dag::{DagNode, DagPlan, NodeType, ready_nodes, ready_sub_plan_nodes};
use crate::db::Db;
use crate::plan_parser::{self, ParsedPlan, PlanPhase, PlanTask};
use crate::ws::broadcast_event;

/// A node's status row as read from the `node_status` table:
/// `(status, updated_at)`.
type NodeStatusRow = (String, Option<String>);

// ── Claim ────────────────────────────────────────────────────────────────

/// Atomically claim a DAG node for execution: flip its `node_status` row to
/// `in_progress` only if it is currently `pending` / `failed` (or absent).
/// Returns `true` if this caller won the claim.
///
/// Mirrors [`crate::agents::claim_task`] but **without** the per-plan
/// "no other agent already running" gate. That gate is what serializes the
/// phase walker (one agent per plan at a time); the DAG scheduler
/// deliberately omits it so all ready nodes can run concurrently in their
/// own worktrees. Claims are therefore gated only per-node.
///
/// The INSERT-or-conditional-UPDATE is a single SQL statement, so SQLite's
/// per-write serialization makes concurrent claims of the *same* node
/// race-free: the first writer creates (or flips) the row; every later
/// writer's `ON CONFLICT … DO UPDATE … WHERE status IN ('pending','failed')`
/// matches nothing (the row is now `in_progress`) and reports 0 rows
/// affected, so the claim fails.
fn claim_node(db: &Db, plan_name: &str, node_id: &str) -> bool {
    let conn = db.lock().unwrap();
    let updated = conn
        .execute(
            "INSERT INTO node_status (plan_name, node_id, status, source, updated_at)
             VALUES (?1, ?2, 'in_progress', 'auto', datetime('now'))
             ON CONFLICT(plan_name, node_id)
             DO UPDATE SET status = 'in_progress',
                           source = 'auto',
                           updated_at = datetime('now')
             WHERE node_status.status IN ('pending', 'failed')",
            params![plan_name, node_id],
        )
        .unwrap_or(0);
    updated > 0
}

/// Unconditionally set a node's `node_status` to `status` (upsert). Used by
/// the gate engine wiring to record a gate's terminal verdict
/// (`completed` / `failed`) or to revert a transiently-claimed node. The
/// `claim_node` flip is conditional (only pending/failed → in_progress);
/// this write is not — the scheduler has already decided the new status.
fn set_node_status(db: &Db, plan_name: &str, node_id: &str, status: &str) {
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "INSERT INTO node_status (plan_name, node_id, status, source, updated_at)
         VALUES (?1, ?2, ?3, 'auto', datetime('now'))
         ON CONFLICT(plan_name, node_id)
         DO UPDATE SET status = excluded.status,
                       source = 'auto',
                       updated_at = excluded.updated_at",
        params![plan_name, node_id, status],
    );
}

/// Mirror a reported task status into the `node_status` table, but **only**
/// when `plan_name` resolves to a `schema_version: 2` (DAG) plan. A no-op for
/// v1 plans (which have no `node_status` rows).
///
/// This closes the "node-status write gap" the DAG scheduler opened in Task
/// 2.1: [`build_task_prompt`] instructs spawned agents to report completion
/// via the `update_task_status` MCP tool / `PUT /tasks/:id/status`, both of
/// which write the `task_status` table. The DAG scheduler, however, reads
/// `node_status`. Without this mirror a v2 plan would route to the scheduler
/// (Task 2.2's headline) but never advance — the just-completed node would be
/// invisible to [`try_dag_advance`]'s done-set.
///
/// Called from the two status-write sites **after** the `task_status` row is
/// written, so for a v2 plan the same status lands in both tables. Sub-plan
/// completion propagation (a node completing should auto-complete its parent
/// when every sibling is done) is layered on top of these writes in Task 2.3.
///
/// `source` is recorded as `'manual'` (the status was reported by an
/// agent/operator, not synthesised by [`claim_node`]'s `'auto'` claim).
pub fn record_node_status_if_v2(
    db: &Db,
    plans_dir: &Path,
    plan_name: &str,
    node_id: &str,
    status: &str,
) {
    if plan_parser::plan_file_schema_version(plans_dir, plan_name) < 2 {
        return;
    }
    let conn = db.lock().unwrap();
    let _ = conn.execute(
        "INSERT INTO node_status (plan_name, node_id, status, source, updated_at)
         VALUES (?1, ?2, ?3, 'manual', datetime('now'))
         ON CONFLICT(plan_name, node_id)
         DO UPDATE SET status = excluded.status,
                       source = 'manual',
                       updated_at = excluded.updated_at",
        params![plan_name, node_id, status],
    );
}

// ── Status snapshot + hydration ───────────────────────────────────────────

/// Snapshot every `node_status` row for a plan into a
/// `scoped_node_id -> (status, updated_at)` map.
fn snapshot_node_statuses(db: &Db, plan_name: &str) -> HashMap<String, NodeStatusRow> {
    let conn = db.lock().unwrap();
    let mut map = HashMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT node_id, status, updated_at FROM node_status WHERE plan_name = ?1")
    else {
        return map;
    };
    let rows = stmt.query_map(params![plan_name], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    });
    if let Ok(rows) = rows {
        for (node_id, status, updated_at) in rows.flatten() {
            map.insert(node_id, (status, updated_at));
        }
    }
    map
}

/// The set of node IDs (scoped) that count as "done" for dependency
/// resolution: `completed` or `skipped`.
fn done_node_ids(map: &HashMap<String, NodeStatusRow>) -> HashSet<String> {
    map.iter()
        .filter(|(_, (status, _))| status == "completed" || status == "skipped")
        .map(|(id, _)| id.clone())
        .collect()
}

/// Recursively populate each node's runtime `status` / `status_updated_at`
/// from the snapshot so [`crate::dag::ready_nodes`] and
/// [`crate::dag::ready_sub_plan_nodes`] (which read `node.status`) reflect
/// the DB. Sub-plan children are keyed by their scoped ID
/// (`<parent>.<child>`).
fn hydrate_statuses(
    nodes: &mut [DagNode],
    prefix: Option<&str>,
    map: &HashMap<String, NodeStatusRow>,
) {
    for node in nodes.iter_mut() {
        let scoped = scope_id(prefix, &node.id);
        if let Some((status, updated_at)) = map.get(&scoped) {
            node.status = Some(status.clone());
            node.status_updated_at = updated_at.clone();
        }
        if !node.nodes.is_empty() {
            hydrate_statuses(&mut node.nodes, Some(&scoped), map);
        }
    }
}

// ── Scoping helpers ────────────────────────────────────────────────────────

/// Scope a node ID under a sub-plan prefix: `Some("integration") + "wire"
/// => "integration.wire"`; `None + "wire" => "wire"`. Mirrors the scoping
/// `crate::dag::flatten_nodes` applies during validation.
fn scope_id(prefix: Option<&str>, id: &str) -> String {
    match prefix {
        Some(p) => format!("{p}.{id}"),
        None => id.to_string(),
    }
}

/// Whether all of `node`'s dependencies are satisfied at the given scope.
/// Sibling references (deps that name another node at this level) are scoped
/// under `prefix`; anything else is treated as an already-scoped /
/// parent-level reference — the same rule
/// [`crate::dag::ready_sub_plan_nodes`] uses.
fn node_deps_satisfied(
    prefix: Option<&str>,
    siblings: &[DagNode],
    node: &DagNode,
    done: &HashSet<String>,
) -> bool {
    let sibling_ids: HashSet<&str> = siblings.iter().map(|n| n.id.as_str()).collect();
    node.depends_on.iter().all(|d| {
        let scoped_dep = if sibling_ids.contains(d.as_str()) {
            scope_id(prefix, d)
        } else {
            d.clone()
        };
        done.contains(&scoped_dep)
    })
}

// ── Scheduling ──────────────────────────────────────────────────────────────

/// Shared context for the recursive scheduler — bundles the handful of
/// values every level needs so the recursion signature stays small.
struct SchedulerCtx<'a> {
    registry: &'a AgentRegistry,
    dag: &'a DagPlan,
    effort: Effort,
    port: u16,
    /// Remaining plan budget (`None` = unbounded). Threaded into spawned
    /// agents as `max_budget_usd`.
    budget: Option<f64>,
    work_dir: &'a Path,
}

/// Schedule every ready node at one scope (top level when `prefix` is
/// `None`, inside a sub-plan otherwise), then recurse into active sub-plans.
/// Returns nothing; claimed task node IDs are pushed onto `spawned`.
///
/// `executed_gates` tracks gate node IDs already run in this advance call so
/// the [`try_dag_advance`] re-loop (which re-evaluates after a gate passes)
/// never executes the same gate twice. `progressed` is set when a gate
/// transitions to `completed`, signalling the caller to re-snapshot and
/// re-schedule (downstream nodes the gate just unblocked).
///
/// `async fn` recursion is boxed at the call site (`Box::pin(...).await`) to
/// keep the future finite.
async fn schedule_scope(
    ctx: &SchedulerCtx<'_>,
    nodes: &[DagNode],
    prefix: Option<&str>,
    done: &HashSet<String>,
    spawned: &mut Vec<String>,
    executed_gates: &mut HashSet<String>,
    progressed: &mut bool,
) {
    // Step 3 of the algorithm: find actionable nodes at this scope.
    let ready: Vec<&DagNode> = match prefix {
        None => ready_nodes(ctx.dag, done),
        Some(p) => ready_sub_plan_nodes(p, nodes, done),
    };

    for node in ready {
        let scoped = scope_id(prefix, &node.id);
        match node.node_type {
            // Step 4: spawn an agent per ready task node — NO break, so
            // siblings fan out in parallel.
            NodeType::Task => {
                if claim_node(&ctx.registry.db, &ctx.dag.name, &scoped) {
                    broadcast_event(
                        &ctx.registry.broadcast_tx,
                        "node_status_changed",
                        json!({
                            "plan_name": ctx.dag.name,
                            "node_id": scoped,
                            "status": "in_progress",
                        }),
                    );
                    spawn_dag_task_node(ctx, node, &scoped).await;
                    spawned.push(scoped);
                }
            }
            // Step 5: gate execution (init / end / CI / approval) — the
            // Phase 3 engine. Gates run only when the `AppState` is wired
            // (production + integration tests); in pure scheduler unit tests
            // it's absent and the gate is left pending (graceful
            // degradation, the plan correctly blocks at the gate).
            NodeType::Gate => {
                let Some(state) = ctx.registry.app_state.get() else {
                    continue;
                };
                // Run each gate at most once per advance call — the re-loop
                // below re-evaluates downstream after a gate passes, and we
                // must not re-execute (e.g. re-run an End gate's compiles).
                if executed_gates.contains(&scoped) {
                    continue;
                }
                // Claim so concurrent advances don't double-execute (an End
                // gate runs cargo build/test; two in the same dir clobber).
                // A lost claim means another advance owns it, or it's
                // already terminal — either way leave it.
                if !claim_node(&ctx.registry.db, &ctx.dag.name, &scoped) {
                    continue;
                }
                executed_gates.insert(scoped.clone());
                broadcast_event(
                    &ctx.registry.broadcast_tx,
                    "node_status_changed",
                    json!({
                        "plan_name": ctx.dag.name,
                        "node_id": scoped,
                        "status": "in_progress",
                    }),
                );
                match crate::gates::execute_gate(state, &ctx.dag.name, node).await {
                    crate::gates::GateOutcome::Passed => {
                        set_node_status(&ctx.registry.db, &ctx.dag.name, &scoped, "completed");
                        broadcast_event(
                            &ctx.registry.broadcast_tx,
                            "node_status_changed",
                            json!({
                                "plan_name": ctx.dag.name,
                                "node_id": scoped,
                                "status": "completed",
                            }),
                        );
                        // The done-set changed — re-loop to advance whatever
                        // this gate just unblocked.
                        *progressed = true;
                    }
                    crate::gates::GateOutcome::Failed(reason) => {
                        set_node_status(&ctx.registry.db, &ctx.dag.name, &scoped, "failed");
                        broadcast_event(
                            &ctx.registry.broadcast_tx,
                            "node_status_changed",
                            json!({
                                "plan_name": ctx.dag.name,
                                "node_id": scoped,
                                "status": "failed",
                            }),
                        );
                        broadcast_event(
                            &ctx.registry.broadcast_tx,
                            "gate_failed",
                            json!({
                                "plan_name": ctx.dag.name,
                                "node_id": scoped,
                                "reason": reason,
                            }),
                        );
                    }
                    // Blocked: the claim already set the node `in_progress`,
                    // and `execute_gate` broadcast the
                    // `gate_ready_for_approval` / `gate_awaiting_approval`
                    // event. The scheduler does NOT re-poll a blocked gate;
                    // completion arrives out-of-band — the approval API
                    // (init / approval gates) or the CI poller (ci / end
                    // gates whose CI is still in flight; that hook is a
                    // follow-up). Leaving it `in_progress` keeps `ready_nodes`
                    // from re-evaluating it.
                    crate::gates::GateOutcome::Blocked(_reason) => {}
                }
            }
            // Step 6: mark the sub-plan active so it isn't re-claimed; the
            // recursion below schedules its children.
            NodeType::SubPlan => {
                let _ = claim_node(&ctx.registry.db, &ctx.dag.name, &scoped);
            }
        }
    }

    // Recurse into every sub-plan at this scope whose deps are satisfied and
    // that isn't itself done. This covers both first-entry (the sub-plan was
    // just claimed above) and re-entry on later ticks (scheduling children
    // that became ready after an earlier child finished). Parent-completion
    // propagation — marking the sub-plan `completed` once all children
    // finish — lands in Phase 2.3.
    for node in nodes {
        if node.node_type != NodeType::SubPlan {
            continue;
        }
        let scoped = scope_id(prefix, &node.id);
        if done.contains(&scoped) {
            continue;
        }
        if !node_deps_satisfied(prefix, nodes, node, done) {
            continue;
        }
        Box::pin(schedule_scope(
            ctx,
            &node.nodes,
            Some(&scoped),
            done,
            spawned,
            executed_gates,
            progressed,
        ))
        .await;
    }
}

/// Spawn an agent for a ready task node in its own worktree. Reuses
/// [`crate::agents::build_task_prompt`] by adapting the [`DagNode`] to a
/// throwaway [`ParsedPlan`] / [`PlanPhase`] / [`PlanTask`] — that gives the
/// agent the full unattended-execution contract (commit-before-status
/// mandate, MCP-tool / curl status instruction) for free.
///
/// NOTE (Phase 2.2): `build_task_prompt`'s status instruction points the
/// agent at the `task_status` write path (`update_task_status` MCP tool /
/// `PUT /tasks/:id/status`), not `node_status`. Routing DAG-node completion
/// back into `node_status` (so this scheduler re-advances) is wired by Phase
/// 2.2 ("Route try_auto_advance by plan version"). Task 2.1 owns the
/// dispatch + claim machinery only.
async fn spawn_dag_task_node(ctx: &SchedulerCtx<'_>, node: &DagNode, scoped_id: &str) {
    // Auto-advance spawns use the default driver; check whether it
    // auto-registers MCP so the prompt picks the MCP tool over curl.
    let mcp_available = ctx.registry.drivers.injects_mcp(None);

    let synthetic_task = PlanTask {
        number: scoped_id.to_string(),
        title: node.title.clone(),
        description: node.description.clone(),
        file_paths: node.file_paths.clone(),
        acceptance: node.acceptance.clone(),
        dependencies: node.depends_on.clone(),
        produces_commit: node.produces_commit,
        status: node.status.clone(),
        status_updated_at: node.status_updated_at.clone(),
        cost_usd: node.cost_usd,
        ci: None,
    };
    let synthetic_phase = PlanPhase {
        number: 1,
        title: ctx.dag.title.clone(),
        description: ctx.dag.context.clone(),
        tasks: vec![synthetic_task.clone()],
        ci_blocking_workflows: None,
        phase_verification: None,
    };
    let synthetic_plan = ParsedPlan {
        name: ctx.dag.name.clone(),
        file_path: ctx.dag.file_path.clone(),
        title: ctx.dag.title.clone(),
        context: ctx.dag.context.clone(),
        project: ctx.dag.project.clone(),
        created_at: ctx.dag.created_at.clone(),
        modified_at: ctx.dag.modified_at.clone(),
        phases: vec![synthetic_phase.clone()],
        verification: None,
        ci_blocking_workflows: None,
        phase_verification: None,
        total_cost_usd: ctx.dag.total_cost_usd,
        max_budget_usd: ctx.dag.max_budget_usd,
    };

    // Cross-plan artifact context (plan `inputs`) is Phase 4 — pass None.
    let prompt = build_task_prompt(
        &synthetic_plan,
        &synthetic_phase,
        &synthetic_task,
        false,
        ctx.port,
        None,
        mcp_available,
    );

    // Branch naming for DAG task nodes is refined in Phase 2.4; the
    // `branchwork/<plan>/<node>` shape matches the v1 convention and is a
    // safe default (scoped IDs like `integration.wire` are valid git refs).
    let branch_name = format!("branchwork/{}/{}", ctx.dag.name, scoped_id);

    pty_agent::start_pty_agent(
        ctx.registry,
        StartPtyOpts {
            prompt,
            cwd: ctx.work_dir,
            plan_name: Some(&ctx.dag.name),
            task_id: Some(scoped_id),
            effort: ctx.effort,
            branch: Some(&branch_name),
            is_continue: false,
            max_budget_usd: ctx.budget,
            driver: None,
            user_id: None,
            org_id: None,
            runner_id: None,
        },
    )
    .await;
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Evaluate a v2 (DAG) plan's graph and spawn agents for every ready node.
///
/// Mirrors [`crate::agents::try_auto_advance`] but operates on a [`DagPlan`]
/// and the `node_status` table (instead of a `ParsedPlan` and `task_status`),
/// and spawns **all** ready task nodes in parallel (no sequential `break`).
///
/// `completed_node_id` is the node whose completion triggered this advance
/// (used only for the `node_advanced` broadcast); it is `None` for the
/// initial kick when a plan starts.
///
/// Takes `registry` by value so callers (Phase 2.2's version router) can
/// `tokio::spawn` it without lifetime gymnastics — same shape as
/// `try_auto_advance`.
pub async fn try_dag_advance(
    registry: AgentRegistry,
    plans_dir: PathBuf,
    plan_name: String,
    completed_node_id: Option<String>,
    effort: Effort,
    port: u16,
) {
    // Same master gate as try_auto_advance: advance only when auto-advance
    // (phase-level opt-in) or auto-mode (which already excludes paused plans
    // via `paused_reason IS NULL`) is enabled for the plan.
    if !auto_advance_enabled(&registry.db, &plan_name)
        && !crate::db::auto_mode_enabled(&registry.db, &plan_name)
    {
        return;
    }

    // Step 1: load the DagPlan (v1 plans auto-convert; v2 parse directly).
    let Some(plan_path) = plan_parser::find_plan_file(&plans_dir, &plan_name) else {
        return;
    };
    let mut dag = match plan_parser::parse_plan_file_as_dag(&plan_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Budget + work_dir (mirrors try_auto_advance). Budget exhausted ⇒
    // return without spawning. Both are plan-static, so they're computed
    // once and reused across the re-loop below.
    let budget = match remaining_budget(&registry.db, &plan_name) {
        Ok(b) => b,
        Err(_) => return,
    };
    let work_dir: PathBuf = {
        let home = dirs::home_dir().unwrap();
        dag.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    };

    let mut spawned: Vec<String> = Vec::new();
    // Gate nodes already executed in this advance call (run at most once).
    let mut executed_gates: HashSet<String> = HashSet::new();

    // Re-loop: a gate transitioning to `completed` (Passed) changes the
    // done-set, so re-snapshot and re-schedule to advance whatever the gate
    // just unblocked within the same call. Bounded: each gate executes at
    // most once (`executed_gates` + the per-node claim) and task spawns
    // never set `progressed`, so the loop terminates once the last reachable
    // gate settles. The counter is a backstop against an unforeseen cycle.
    let mut guard = 0u32;
    loop {
        guard += 1;
        if guard > 64 {
            eprintln!("[dag-scheduler] advance re-loop guard tripped for plan {plan_name}");
            break;
        }

        // Step 2 (per pass): snapshot node statuses → hydrate the in-memory
        // plan + rebuild the done-set (completed | skipped node IDs).
        let status_map = snapshot_node_statuses(&registry.db, &plan_name);
        hydrate_statuses(&mut dag.nodes, None, &status_map);
        let done_set = done_node_ids(&status_map);

        let ctx = SchedulerCtx {
            registry: &registry,
            dag: &dag,
            effort,
            port,
            budget,
            work_dir: &work_dir,
        };
        let mut progressed = false;
        schedule_scope(
            &ctx,
            &dag.nodes,
            None,
            &done_set,
            &mut spawned,
            &mut executed_gates,
            &mut progressed,
        )
        .await;

        if !progressed {
            break;
        }
    }

    // `node_advanced` fires only when at least one node was actually claimed
    // + spawned. If every candidate lost the claim race (a concurrent
    // advance trigger already took them), stay quiet.
    if !spawned.is_empty() {
        broadcast_event(
            &registry.broadcast_tx,
            "node_advanced",
            json!({
                "plan": plan_name,
                "from_node": completed_node_id,
                "to_nodes": spawned,
            }),
        );
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::parse_dag_yaml;
    use tempfile::TempDir;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        (crate::db::init(&path), dir)
    }

    fn test_registry(db: Db) -> (AgentRegistry, tokio::sync::broadcast::Receiver<String>) {
        let (tx, rx) = tokio::sync::broadcast::channel::<String>(64);
        let mut registry = AgentRegistry::new(
            db,
            tx,
            None,
            PathBuf::from("/tmp/branchwork-dag-test-sockets"),
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        // The plan's `project` resolves to a `branchwork-no-such-dir-*`
        // path, so `git worktree add` inside `start_pty_agent` always fails
        // — but `setup_worktree` `create_dir_all`s the base first. Pin it
        // under the OS temp dir so pollution never lands in the real
        // `~/.branchwork/worktrees`. Never set `$BRANCHWORK_WORKTREE_BASE`
        // here: a process-wide env write would race parallel test binaries.
        registry.worktree_base_override =
            Some(std::env::temp_dir().join("branchwork-dag-test-worktrees"));
        (registry, rx)
    }

    /// Write a v2 (DAG) plan whose `project` resolves (via
    /// `dirs::home_dir().join(...)`) to a non-existent path, so the agent
    /// worktree setup fails harmlessly instead of polluting the test repo.
    /// `nodes_yaml` must be the 2-space-indented body under `nodes:`.
    fn write_dag_plan(plans_dir: &Path, plan_name: &str, nodes_yaml: &str) {
        std::fs::create_dir_all(plans_dir).unwrap();
        let path = plans_dir.join(format!("{plan_name}.yaml"));
        let yaml = format!(
            "schema_version: 2\n\
             title: \"{plan_name} title\"\n\
             project: branchwork-no-such-dir-{plan_name}\n\
             nodes:\n{nodes_yaml}"
        );
        std::fs::write(path, yaml).unwrap();
    }

    fn collect_events(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    /// Wire an `AppState` into the registry so gate nodes execute (the
    /// production path). The `OnceLock` is `Arc`-shared, so setting it on
    /// `registry` makes it visible to the clone `try_dag_advance` receives by
    /// value. `plans_dir` must match the one passed to `try_dag_advance` so
    /// the gate engine's `project_dir_for` resolves. The circular
    /// `registry → AppState → registry` `Arc` leaks at process end — fine for
    /// a test.
    fn wire_app_state(registry: &AgentRegistry, db: Db, plans_dir: PathBuf) {
        use std::sync::{Arc, Mutex as StdMutex};
        let state = crate::state::AppState {
            db,
            plans_dir,
            port: 3100,
            effort: Arc::new(StdMutex::new(Effort::High)),
            broadcast_tx: registry.broadcast_tx.clone(),
            registry: registry.clone(),
            runners: crate::saas::runner_ws::new_runner_registry(),
            settings_path: PathBuf::from("/tmp/branchwork-dag-gate-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(HashSet::new())),
            started_at: std::time::Instant::now(),
        };
        let _ = registry.app_state.set(state);
    }

    // ── claim_node ──────────────────────────────────────────────────────

    #[test]
    fn claim_node_only_first_caller_wins() {
        let (db, _dir) = fresh_db();
        // No row yet → INSERT wins.
        assert!(claim_node(&db, "p", "n1"));
        // Row is now in_progress → second claim loses.
        assert!(!claim_node(&db, "p", "n1"));
    }

    #[test]
    fn claim_node_reclaims_failed() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES ('p', 'n1', 'failed')",
                [],
            )
            .unwrap();
        }
        // failed → reclaimable.
        assert!(claim_node(&db, "p", "n1"));
        // now in_progress → not reclaimable.
        assert!(!claim_node(&db, "p", "n1"));
    }

    #[test]
    fn claim_node_skips_completed_and_skipped() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES ('p', 'done', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES ('p', 'skip', 'skipped')",
                [],
            )
            .unwrap();
        }
        assert!(!claim_node(&db, "p", "done"));
        assert!(!claim_node(&db, "p", "skip"));
    }

    #[test]
    fn claim_node_has_no_per_plan_running_agent_gate() {
        // The headline difference from `claim_task`: a running agent on the
        // SAME plan does NOT block claiming a different node. This is what
        // lets the DAG scheduler fan out parallel agents.
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, plan_name)
                 VALUES ('ag1', '/tmp/x', 'running', 'pty', 'p')",
                [],
            )
            .unwrap();
        }
        assert!(claim_node(&db, "p", "n1"));
        // n2 claims even though n1's (and ag1's) work is in flight.
        assert!(claim_node(&db, "p", "n2"));
    }

    // ── record_node_status_if_v2 (node-status write gap) ──────────────────

    #[test]
    fn record_node_status_writes_for_v2_plan() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan = "rec-v2";
        write_dag_plan(&plans_dir, plan, "  - id: a\n    type: task\n");

        record_node_status_if_v2(&db, &plans_dir, plan, "a", "completed");

        let conn = db.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn record_node_status_is_noop_for_v1_plan() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let plan = "rec-v1";
        // Legacy (v1) plan: no schema_version field.
        std::fs::write(
            plans_dir.join(format!("{plan}.yaml")),
            "title: t\nphases:\n  - number: 1\n    title: P1\n    tasks:\n      - number: \"1.1\"\n        title: T\n",
        )
        .unwrap();

        record_node_status_if_v2(&db, &plans_dir, plan, "1.1", "completed");

        let conn = db.lock().unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_status WHERE plan_name = ?1",
                params![plan],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "v1 plan must not get a node_status row");
    }

    #[test]
    fn record_node_status_overwrites_existing_claim() {
        // claim_node writes 'in_progress'; a later completion report from the
        // status-write site overwrites it to 'completed' — the path that lets
        // the DAG advance past a node.
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan = "rec-overwrite";
        write_dag_plan(&plans_dir, plan, "  - id: a\n    type: task\n");

        assert!(claim_node(&db, plan, "a"));
        record_node_status_if_v2(&db, &plans_dir, plan, "a", "completed");

        let conn = db.lock().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    // ── ready-node identification (acceptance: diamond) ───────────────────

    /// Acceptance: a diamond (init → A + B → merge → end) correctly
    /// identifies ready nodes at each step as the done-set grows.
    #[test]
    fn diamond_identifies_ready_nodes_at_each_step() {
        let yaml = r#"
schema_version: 2
title: "Diamond"
nodes:
  - id: init
    type: gate
    gate_kind: init
  - id: A
    type: task
    depends_on: [init]
  - id: B
    type: task
    depends_on: [init]
  - id: merge
    type: task
    depends_on: [A, B]
  - id: end
    type: gate
    gate_kind: end
    depends_on: [merge]
"#;
        let plan = parse_dag_yaml(yaml, "diamond", "diamond.yaml").unwrap();

        let ready_ids = |done: &HashSet<String>| -> Vec<String> {
            let mut ids: Vec<String> = ready_nodes(&plan, done)
                .iter()
                .map(|n| n.id.clone())
                .collect();
            ids.sort();
            ids
        };

        // Nothing done → only init (no deps).
        let done: HashSet<String> = HashSet::new();
        assert_eq!(ready_ids(&done), vec!["init"]);

        // init done → A and B ready in parallel.
        let done: HashSet<String> = ["init".to_string()].into();
        assert_eq!(ready_ids(&done), vec!["A", "B"]);

        // init + A done → only B (merge still needs B).
        let done: HashSet<String> = ["init".to_string(), "A".to_string()].into();
        assert_eq!(ready_ids(&done), vec!["B"]);

        // init + A + B done → merge ready.
        let done: HashSet<String> = ["init".to_string(), "A".to_string(), "B".to_string()].into();
        assert_eq!(ready_ids(&done), vec!["merge"]);

        // + merge done → end ready.
        let done: HashSet<String> = ["init", "A", "B", "merge"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(ready_ids(&done), vec!["end"]);

        // Everything done → nothing ready.
        let done: HashSet<String> = ["init", "A", "B", "merge", "end"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(ready_ids(&done).is_empty());
    }

    // ── try_dag_advance integration (acceptance: parallel spawn) ──────────

    /// Acceptance: two task nodes whose shared dependency is satisfied are
    /// BOTH claimed (to `in_progress`) within a single `try_dag_advance`
    /// call — proving the scheduler fans out in parallel (no `break`).
    #[tokio::test]
    async fn two_task_nodes_spawn_concurrently() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan_name = "dag-parallel";

        // init (gate) → a, b (tasks). init is already completed, so both
        // tasks are immediately ready.
        let nodes = r#"  - id: init
    type: gate
    gate_kind: init
  - id: a
    type: task
    title: "Task A"
    depends_on: [init]
  - id: b
    type: task
    title: "Task B"
    depends_on: [init]
"#;
        write_dag_plan(&plans_dir, plan_name, nodes);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, 'init', 'completed')",
                params![plan_name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params![plan_name],
            )
            .unwrap();
        }

        let (registry, mut rx) = test_registry(db.clone());
        try_dag_advance(
            registry,
            plans_dir.clone(),
            plan_name.to_string(),
            Some("init".to_string()),
            Effort::High,
            3100,
        )
        .await;

        // Both task nodes claimed within the one advance call.
        let conn = db.lock().unwrap();
        let status_a: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        let status_b: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'b'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status_a, "in_progress", "node a must be claimed");
        assert_eq!(status_b, "in_progress", "node b must be claimed");

        // The init gate is untouched (Phase 3 owns gate execution); it stays
        // completed and is never re-claimed.
        let status_init: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'init'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status_init, "completed");

        // start_pty_agent inserted an agents row for each spawned node
        // (worktree setup fails on the fake project dir, so they land as
        // `failed` — but the row, like the claim, persists).
        let agent_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE plan_name = ?1",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(agent_count, 2, "both task nodes must have spawned an agent");
        drop(conn);

        // A single `node_advanced` event names both nodes.
        let events = collect_events(&mut rx);
        let advanced: Vec<&String> = events
            .iter()
            .filter(|e| e.contains("\"type\":\"node_advanced\""))
            .collect();
        assert_eq!(advanced.len(), 1, "exactly one node_advanced broadcast");
        let payload = advanced[0];
        assert!(
            payload.contains("\"a\""),
            "node_advanced names a: {payload}"
        );
        assert!(
            payload.contains("\"b\""),
            "node_advanced names b: {payload}"
        );
    }

    #[tokio::test]
    async fn no_op_when_auto_advance_disabled() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan_name = "dag-disabled";
        let nodes = r#"  - id: init
    type: gate
    gate_kind: init
  - id: a
    type: task
    depends_on: [init]
"#;
        write_dag_plan(&plans_dir, plan_name, nodes);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, 'init', 'completed')",
                params![plan_name],
            )
            .unwrap();
            // NOTE: no plan_auto_advance row, no auto_mode → gate closed.
        }

        let (registry, _rx) = test_registry(db.clone());
        try_dag_advance(
            registry,
            plans_dir,
            plan_name.to_string(),
            None,
            Effort::High,
            3100,
        )
        .await;

        // Node `a` must NOT be claimed — the advance is gated off.
        let conn = db.lock().unwrap();
        let claimed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claimed, 0, "no node claimed when advance is gated off");
    }

    /// Graceful degradation: when the `AppState` is not wired into the
    /// registry (pure scheduler unit test), gate nodes are left `pending`
    /// (no claim, no spawn) and downstream task nodes stay blocked. Real
    /// gate execution is exercised by the app_state-wired tests below and by
    /// the unit tests in `crate::gates`.
    #[tokio::test]
    async fn gate_left_pending_when_app_state_unwired() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan_name = "dag-gate-block";
        let nodes = r#"  - id: init
    type: gate
    gate_kind: init
  - id: gate
    type: gate
    gate_kind: ci
    depends_on: [init]
  - id: a
    type: task
    depends_on: [gate]
"#;
        write_dag_plan(&plans_dir, plan_name, nodes);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, 'init', 'completed')",
                params![plan_name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params![plan_name],
            )
            .unwrap();
        }

        let (registry, _rx) = test_registry(db.clone());
        // NOTE: app_state intentionally NOT wired.
        try_dag_advance(
            registry,
            plans_dir,
            plan_name.to_string(),
            Some("init".to_string()),
            Effort::High,
            3100,
        )
        .await;

        let conn = db.lock().unwrap();
        let gate_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_status WHERE plan_name = ?1 AND node_id = 'gate'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gate_rows, 0, "gate must stay pending without app_state");
        let a_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_rows, 0, "task behind an un-executed gate must not spawn");
    }

    /// With `AppState` wired, an **approval** gate executes, returns
    /// `Blocked`, and the scheduler leaves it `in_progress` (claimed) while
    /// keeping downstream tasks blocked — and broadcasts
    /// `gate_awaiting_approval` so the dashboard can surface it.
    #[tokio::test]
    async fn approval_gate_blocks_downstream_with_app_state() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan_name = "dag-approval-block";
        let nodes = r#"  - id: init
    type: gate
    gate_kind: init
  - id: approve
    type: gate
    gate_kind: approval
    depends_on: [init]
  - id: a
    type: task
    depends_on: [approve]
"#;
        write_dag_plan(&plans_dir, plan_name, nodes);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, 'init', 'completed')",
                params![plan_name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params![plan_name],
            )
            .unwrap();
        }

        let (registry, mut rx) = test_registry(db.clone());
        wire_app_state(&registry, db.clone(), plans_dir.clone());
        try_dag_advance(
            registry,
            plans_dir,
            plan_name.to_string(),
            Some("init".to_string()),
            Effort::High,
            3100,
        )
        .await;

        let conn = db.lock().unwrap();
        let approve_status: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'approve'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            approve_status, "in_progress",
            "blocked approval gate stays in_progress (not re-polled)"
        );
        let a_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_rows, 0, "task behind a blocked gate must not spawn");
        drop(conn);

        let events = collect_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("\"type\":\"gate_awaiting_approval\"")),
            "expected gate_awaiting_approval broadcast, got {events:?}"
        );
    }

    /// With `AppState` wired, a **CI** gate over a project with no
    /// `.github/workflows` (the test's project dir does not exist on disk) is
    /// vacuously green ⇒ `Passed`. The scheduler completes it and the re-loop
    /// advances the downstream task in the *same* `try_dag_advance` call.
    #[tokio::test]
    async fn vacuous_ci_gate_passes_and_unblocks_downstream() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        let plan_name = "dag-ci-vacuous";
        let nodes = r#"  - id: init
    type: gate
    gate_kind: init
  - id: gate
    type: gate
    gate_kind: ci
    depends_on: [init]
  - id: a
    type: task
    title: "After CI"
    depends_on: [gate]
"#;
        write_dag_plan(&plans_dir, plan_name, nodes);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, 'init', 'completed')",
                params![plan_name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params![plan_name],
            )
            .unwrap();
        }

        let (registry, mut rx) = test_registry(db.clone());
        wire_app_state(&registry, db.clone(), plans_dir.clone());
        try_dag_advance(
            registry,
            plans_dir,
            plan_name.to_string(),
            Some("init".to_string()),
            Effort::High,
            3100,
        )
        .await;

        let conn = db.lock().unwrap();
        let gate_status: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'gate'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gate_status, "completed",
            "vacuous CI gate (no workflows) must pass"
        );
        // The re-loop advanced `a` after the gate completed.
        let a_status: String = conn
            .query_row(
                "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            a_status, "in_progress",
            "task must spawn once CI gate passes"
        );
        // start_pty_agent inserted an agents row for `a` (worktree setup
        // fails on the fake project dir, but the row + claim persist).
        let agent_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE plan_name = ?1 AND task_id = 'a'",
                params![plan_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(agent_count, 1, "downstream task must have spawned an agent");
        drop(conn);

        // A single `node_advanced` names the spawned task.
        let events = collect_events(&mut rx);
        let advanced: Vec<&String> = events
            .iter()
            .filter(|e| e.contains("\"type\":\"node_advanced\""))
            .collect();
        assert_eq!(advanced.len(), 1, "exactly one node_advanced broadcast");
        assert!(advanced[0].contains("\"a\""), "names a: {}", advanced[0]);
    }
}
