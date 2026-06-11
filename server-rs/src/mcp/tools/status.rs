//! Agent-facing status tools: `update_task_status`, `report_cost`, and
//! `report_blocker`. These replace the `curl`-in-the-prompt pattern that
//! agents previously used to report progress — an MCP-speaking agent can
//! now call these directly instead of shelling out.
//!
//! All three tools:
//! - Write to the same tables as the equivalent HTTP endpoints
//!   (`task_status`, `task_learnings`, `agents`) so the dashboard reads the
//!   same state either way.
//! - Push a live event on [`McpContext::broadcast_tx`] so connected dashboard
//!   clients update without refetching.
//! - Mirror the HTTP `set_task_status` handler's post-write behaviour
//!   (auto-advance spawn on `completed`/`skipped`).

use rmcp::{ErrorData as McpError, Json, handler::server::wrapper::Parameters, tool, tool_router};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::mcp::BranchworkMcp;

const VALID_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "completed",
    "failed",
    "skipped",
    "checking",
];

// ── Request schemas ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateTaskStatusRequest {
    /// Plan name (file stem, e.g. `my-plan` for `my-plan.yaml`).
    pub plan: String,
    /// Task number as it appears in the plan, e.g. `2.3`.
    pub task: String,
    /// New status. One of: `pending`, `in_progress`, `completed`, `failed`,
    /// `skipped`, `checking`.
    pub status: String,
    /// Optional free-form explanation. When provided, recorded as a task
    /// learning so downstream tasks can see the context.
    #[serde(default)]
    pub reason: Option<String>,
    /// Pivot 2026-06-11 — label of the DECLARING agent (a foreign
    /// orchestrator such as a Claude Code session, or one of its
    /// sub-agents). Shown on the dashboard task card.
    #[serde(default)]
    pub agent: Option<String>,
    /// Artifact links this update produced (PR URLs, commit SHAs, CI run
    /// URLs). Persisted to `task_artifacts` and broadcast, so the dashboard
    /// can show observed ground truth next to the declared status.
    #[serde(default)]
    pub artifacts: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReportCostRequest {
    /// Plan name (file stem).
    pub plan: String,
    /// Task number as it appears in the plan, e.g. `2.3`.
    pub task: String,
    /// Cost in USD to attribute to this task.
    pub usd: f64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReportBlockerRequest {
    /// Plan name (file stem).
    pub plan: String,
    /// Task number as it appears in the plan, e.g. `2.3`.
    pub task: String,
    /// What is blocking progress. Stored as a learning and surfaced to
    /// whoever unblocks the task.
    pub reason: String,
}

// ── Response schemas ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaskStatusUpdate {
    pub ok: bool,
    pub plan_name: String,
    pub task_number: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CostReport {
    pub ok: bool,
    pub plan_name: String,
    pub task_number: String,
    pub amount_usd: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BlockerReport {
    pub ok: bool,
    pub plan_name: String,
    pub task_number: String,
    pub status: String,
    pub reason: String,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = status_router, vis = "pub")]
impl BranchworkMcp {
    #[tool(
        description = "Update a task's status and broadcast the change to the dashboard. \
                       Valid statuses: pending, in_progress, completed, failed, skipped, \
                       checking. If `reason` is provided, it is also recorded as a task \
                       learning. Completing or skipping a task may trigger auto-advance \
                       to the next ready task in the plan."
    )]
    pub async fn update_task_status(
        &self,
        Parameters(req): Parameters<UpdateTaskStatusRequest>,
    ) -> Result<Json<TaskStatusUpdate>, McpError> {
        if !VALID_STATUSES.contains(&req.status.as_str()) {
            return Err(McpError::invalid_params(
                format!(
                    "Invalid status: {}. Must be one of: {}",
                    req.status,
                    VALID_STATUSES.join(", ")
                ),
                None,
            ));
        }

        // Pivot 2026-06-11 — `mode: observe` plans are declared against by
        // FOREIGN agents; Branchwork tracks but never drives. The tree-clean
        // gate assumes a Branchwork-managed worktree and auto-advance assumes
        // Branchwork-spawned runners — both are skipped for observe plans.
        // The pending-learning CI gate stays: it is knowledge hygiene, not
        // execution machinery.
        let observe_mode = crate::plan_parser::find_plan_file(&self.ctx.plans_dir, &req.plan)
            .and_then(|p| crate::plan_parser::parse_plan_file(&p).ok())
            .is_some_and(|p| p.mode.as_deref() == Some("observe"));

        // Reject `completed` on a dirty working tree — the most common way
        // tasks get silently dropped is an agent that edits files, calls
        // this tool, and exits without `git commit`. See
        // `agents::check_tree_clean_for_completion` for the full story.
        if !observe_mode
            && req.status == "completed"
            && let crate::agents::TreeState::Dirty { files } =
                crate::agents::check_tree_clean_for_completion(
                    &self.ctx.db,
                    &self.ctx.plans_dir,
                    &req.plan,
                )
        {
            let preview = files.join(", ");
            return Err(McpError::invalid_params(
                format!(
                    "Cannot mark task completed — working tree has uncommitted changes: {preview}. \
                     Run `git add -A && git commit -m '<msg>'` in the project, then call \
                     update_task_status(completed) again."
                ),
                None,
            ));
        }

        // Phase 1, Task 1.2: pending-learning gate. A `ci_failure_events` row
        // landed for this task (T1.1 dispatcher) and no learning has cleared
        // it yet. Refuse with the structured `code: pending_learning` payload
        // so the agent can react programmatically — call
        // `report_blocker` (which writes a learning) or POST to
        // `/api/plans/<plan>/tasks/<task>/learnings`, then retry.
        if req.status == "completed" {
            let pending = crate::db::pending_ci_failures(&self.ctx.db, &req.plan, &req.task);
            if !pending.is_empty() {
                let summary = pending
                    .iter()
                    .map(|f| {
                        format!(
                            "run {} ({})",
                            f.run_id,
                            f.workflow.as_deref().unwrap_or("workflow ?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(McpError::invalid_params(
                    format!(
                        "Cannot mark task completed — pending_learning: {n} CI failure(s) \
                         need a learning before this task can be closed: {summary}. \
                         Call `report_blocker` or POST a learning to \
                         /api/plans/{plan}/tasks/{task}/learnings, then retry.",
                        n = pending.len(),
                        plan = req.plan,
                        task = req.task,
                    ),
                    Some(serde_json::json!({
                        "code": "pending_learning",
                        "plan": req.plan,
                        "task": req.task,
                        "failures": pending,
                    })),
                ));
            }
        }

        {
            let db = self.ctx.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, source, updated_at, agent)
                 VALUES (?1, ?2, ?3, 'manual', datetime('now'), ?4)
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = excluded.status,
                               source = 'manual',
                               updated_at = excluded.updated_at,
                               agent = COALESCE(excluded.agent, task_status.agent)",
                params![req.plan, req.task, req.status, req.agent],
            )
            .map_err(|e| {
                McpError::internal_error(format!("failed to write task_status: {e}"), None)
            })?;

            if let Some(reason) = req
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                // Best-effort; failing to store the reason shouldn't block the status change.
                let _ = db.execute(
                    "INSERT INTO task_learnings (plan_name, task_number, learning) \
                     VALUES (?1, ?2, ?3)",
                    params![req.plan, req.task, reason],
                );
            }

            // Pivot 2026-06-11 — persist artifact links (best-effort, like
            // the learning above): the ground truth the dashboard shows next
            // to the declared status.
            if let Some(artifacts) = req.artifacts.as_ref() {
                for url in artifacts.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    let _ = db.execute(
                        "INSERT INTO task_artifacts (plan_name, task_number, url, agent) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![req.plan, req.task, url, req.agent],
                    );
                }
            }
        }

        // Task 2.2 (DAG model): for `schema_version: 2` plans, `req.task` is a
        // DAG node ID. Mirror the reported status into `node_status` so the DAG
        // scheduler (which reads that table, not `task_status`) sees the
        // completion when the `try_auto_advance` trigger below routes to it.
        // No-op for v1 plans.
        crate::dag_scheduler::record_node_status_if_v2(
            &self.ctx.db,
            &self.ctx.plans_dir,
            &req.plan,
            &req.task,
            &req.status,
        );

        // Phase 1, Task 1.2: any learning capture clears the pending-learning
        // gate. Fires whenever `reason` was set (the branch above wrote a
        // task_learnings row); a no-reason status update is a no-op here.
        if req
            .reason
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
        {
            crate::db::mark_ci_failures_resolved(&self.ctx.db, &req.plan, &req.task);
        }

        crate::ws::broadcast_event(
            &self.ctx.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": req.plan,
                "task_number": req.task,
                "status": req.status,
                "agent": req.agent,
                "artifacts": req.artifacts,
            }),
        );

        if !observe_mode && (req.status == "completed" || req.status == "skipped") {
            let registry = self.ctx.registry.clone();
            let plans_dir = self.ctx.plans_dir.clone();
            let effort = *self.ctx.effort.lock().unwrap();
            let port = self.ctx.port;
            let plan_name = req.plan.clone();
            let task_number = req.task.clone();
            tokio::spawn(async move {
                crate::agents::try_auto_advance(
                    registry,
                    plans_dir,
                    plan_name,
                    task_number,
                    effort,
                    port,
                    None,
                )
                .await;
            });
        }

        Ok(Json(TaskStatusUpdate {
            ok: true,
            plan_name: req.plan,
            task_number: req.task,
            status: req.status,
        }))
    }

    #[tool(
        description = "Report additional USD cost against a task. Aggregates into the \
                       task's total cost shown on the dashboard. Intended for drivers \
                       that don't auto-report cost (e.g. Codex, Gemini) or for manual \
                       attribution."
    )]
    pub async fn report_cost(
        &self,
        Parameters(req): Parameters<ReportCostRequest>,
    ) -> Result<Json<CostReport>, McpError> {
        if !req.usd.is_finite() || req.usd < 0.0 {
            return Err(McpError::invalid_params(
                format!("usd must be a non-negative finite number, got {}", req.usd),
                None,
            ));
        }

        // Cost aggregation (see api/plans.rs) sums `agents.cost_usd` per task.
        // Reuse that by inserting a dedicated row rather than adding a new
        // table + changing the aggregation query.
        let synthetic_id = format!("cost-report-{}", Uuid::new_v4());
        {
            let db = self.ctx.db.lock().unwrap();
            db.execute(
                "INSERT INTO agents
                     (id, cwd, status, mode, plan_name, task_id, cost_usd,
                      started_at, finished_at)
                 VALUES (?1, '', 'completed', 'cost_report', ?2, ?3, ?4,
                         datetime('now'), datetime('now'))",
                params![synthetic_id, req.plan, req.task, req.usd],
            )
            .map_err(|e| McpError::internal_error(format!("failed to record cost: {e}"), None))?;
        }

        crate::ws::broadcast_event(
            &self.ctx.broadcast_tx,
            "task_cost_reported",
            serde_json::json!({
                "plan_name": req.plan,
                "task_number": req.task,
                "amount_usd": req.usd,
            }),
        );

        Ok(Json(CostReport {
            ok: true,
            plan_name: req.plan,
            task_number: req.task,
            amount_usd: req.usd,
        }))
    }

    #[tool(
        description = "Report that a task is blocked: marks it as `failed` and records \
                       the blocker reason as a task learning so whoever unblocks it \
                       has context. Broadcasts a task_status_changed event."
    )]
    pub async fn report_blocker(
        &self,
        Parameters(req): Parameters<ReportBlockerRequest>,
    ) -> Result<Json<BlockerReport>, McpError> {
        let reason = req.reason.trim();
        if reason.is_empty() {
            return Err(McpError::invalid_params(
                "reason is required".to_string(),
                None,
            ));
        }

        let learning = format!("BLOCKED: {reason}");
        {
            let db = self.ctx.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                 VALUES (?1, ?2, 'failed', 'manual', datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = 'failed',
                               source = 'manual',
                               updated_at = excluded.updated_at",
                params![req.plan, req.task],
            )
            .map_err(|e| {
                McpError::internal_error(format!("failed to write task_status: {e}"), None)
            })?;
            let _ = db.execute(
                "INSERT INTO task_learnings (plan_name, task_number, learning) \
                 VALUES (?1, ?2, ?3)",
                params![req.plan, req.task, learning],
            );
        }

        // Phase 1, Task 1.2: report_blocker writes a `BLOCKED:` learning, so
        // it clears the pending-learning gate as a side effect. An operator
        // who unblocks the task later can `update_task_status(completed)`
        // without re-tripping the gate on the originally-pending failure.
        crate::db::mark_ci_failures_resolved(&self.ctx.db, &req.plan, &req.task);

        crate::ws::broadcast_event(
            &self.ctx.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": req.plan,
                "task_number": req.task,
                "status": "failed",
                "reason": reason,
            }),
        );

        Ok(Json(BlockerReport {
            ok: true,
            plan_name: req.plan,
            task_number: req.task,
            status: "failed".to_string(),
            reason: reason.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the Phase 1, Task 1.2 pending-learning gate on the
    //! MCP side. The HTTP path is pinned in `tests/pending_learning_gate.rs`;
    //! these tests cover the McpError shape (code lives in `data`, not the
    //! JSON-RPC `code` field) and the resolve-on-`reason` side effect of
    //! `update_task_status` + `report_blocker`.
    //!
    //! Mirrors the McpContext construction pattern in `mcp/tools/plans.rs`.
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::config::Effort;
    use crate::db;
    use crate::mcp::{BranchworkMcp, McpContext};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    fn test_mcp() -> (BranchworkMcp, TempDir) {
        let dir = TempDir::new().unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let db = db::init(&dir.path().join("test.db"));
        let (tx, _rx) = broadcast::channel::<String>(32);
        let registry = AgentRegistry::new(
            db.clone(),
            tx.clone(),
            None,
            PathBuf::from("/tmp/branchwork-test-sockets"),
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        let ctx = McpContext {
            plans_dir,
            db,
            broadcast_tx: tx,
            registry,
            effort: Arc::new(Mutex::new(Effort::High)),
            port: 3100,
        };
        (BranchworkMcp::new(ctx), dir)
    }

    /// Seed a pending `ci_failure_events` row via direct SQL. Mirrors
    /// the `tests/pending_learning_gate.rs::seed_pending_failure` helper
    /// — kept inline because integration-test helpers cannot be reached
    /// from binary-crate unit tests.
    fn seed_pending_failure(mcp: &BranchworkMcp, plan: &str, task: &str, run_id: &str) {
        let conn = mcp.ctx.db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id, \
                  run_url, workflow, conclusion, summary, org_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'default-org')",
            params![
                format!("agent-{}", run_id),
                plan,
                task,
                format!("branchwork/{}/{}", plan, task),
                run_id,
                format!("https://github.com/owner/repo/actions/runs/{}", run_id),
                "tests.yml",
                "failure",
                format!("workflow tests.yml failed (run {})", run_id),
            ],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn update_task_status_completed_returns_mcp_error_with_pending_learning_data() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345");

        let res = mcp
            .update_task_status(Parameters(UpdateTaskStatusRequest {
                plan: "p".to_string(),
                task: "1.1".to_string(),
                status: "completed".to_string(),
                reason: None,
                agent: None,
                artifacts: None,
            }))
            .await;
        // rmcp::Json<T> doesn't impl Debug, so we can't .expect_err — pattern
        // match instead.
        let err = match res {
            Ok(_) => panic!("expected pending_learning McpError, got Ok"),
            Err(e) => e,
        };

        // The JSON-RPC `code` field stays INVALID_PARAMS (-32602) per the
        // protocol; the structured `code: "pending_learning"` payload
        // lives in `data` so agents that grep on the message string OR
        // on data.code both detect it.
        assert!(
            err.message.contains("pending_learning"),
            "message must mention pending_learning: {}",
            err.message
        );
        let data = err.data.expect("data payload is required");
        assert_eq!(data["code"], "pending_learning", "{data}");
        assert_eq!(data["plan"], "p");
        assert_eq!(data["task"], "1.1");
        let failures = data["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0]["runId"], "12345");
    }

    #[tokio::test]
    async fn update_task_status_non_completed_bypasses_pending_learning_gate() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345");

        for st in ["in_progress", "failed", "skipped", "pending"] {
            let res = mcp
                .update_task_status(Parameters(UpdateTaskStatusRequest {
                    plan: "p".to_string(),
                    task: "1.1".to_string(),
                    status: st.to_string(),
                    reason: None,
                    agent: None,
                    artifacts: None,
                }))
                .await;
            // rmcp::Json<T> doesn't impl Debug; report the error message
            // on failure rather than the whole Result.
            if let Err(err) = res {
                panic!("status={st} should bypass gate, got Err: {}", err.message);
            }
        }
    }

    #[tokio::test]
    async fn update_task_status_with_reason_resolves_the_gate_for_next_call() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345");

        // status=in_progress + reason="..." writes a learning, which
        // resolves the pending failure. Subsequent completed succeeds.
        mcp.update_task_status(Parameters(UpdateTaskStatusRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            status: "in_progress".to_string(),
            reason: Some("CI tripped because of fmt".to_string()),
            agent: None,
            artifacts: None,
        }))
        .await
        .expect("in_progress+reason should succeed");

        // Verify resolved.
        let pending = crate::db::pending_ci_failures(&mcp.ctx.db, "p", "1.1");
        assert!(pending.is_empty(), "reason should have cleared the gate");

        // Retry completed: succeeds.
        mcp.update_task_status(Parameters(UpdateTaskStatusRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            status: "completed".to_string(),
            reason: None,
            agent: None,
            artifacts: None,
        }))
        .await
        .expect("post-resolve completed should succeed");
    }

    #[tokio::test]
    async fn report_blocker_writes_blocked_learning_and_resolves_pending_failures() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345");

        mcp.report_blocker(Parameters(ReportBlockerRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            reason: "waiting on flaky CI runner upstream".to_string(),
        }))
        .await
        .expect("report_blocker should succeed");

        let pending = crate::db::pending_ci_failures(&mcp.ctx.db, "p", "1.1");
        assert!(
            pending.is_empty(),
            "report_blocker must resolve pending failures so an unblock-then-complete \
             flow does not re-trip the gate"
        );
    }

    /// A task with NO pending failures must complete cleanly via MCP —
    /// the gate is a strict guard, not an annotation that requires
    /// learnings for every task.
    #[tokio::test]
    async fn update_task_status_completed_succeeds_when_no_pending_failures() {
        let (mcp, _dir) = test_mcp();
        // No seed → no pending rows.
        mcp.update_task_status(Parameters(UpdateTaskStatusRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            status: "completed".to_string(),
            reason: None,
            agent: None,
            artifacts: None,
        }))
        .await
        .expect("no pending rows → completed must succeed");
    }
}
