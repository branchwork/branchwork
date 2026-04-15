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

use crate::mcp::OrchestrAiMcp;

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
impl OrchestrAiMcp {
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

        // Reject `completed` on a dirty working tree — the most common way
        // tasks get silently dropped is an agent that edits files, calls
        // this tool, and exits without `git commit`. See
        // `agents::check_tree_clean_for_completion` for the full story.
        if req.status == "completed"
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

        {
            let db = self.ctx.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                 VALUES (?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
                params![req.plan, req.task, req.status],
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
        }

        crate::ws::broadcast_event(
            &self.ctx.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": req.plan,
                "task_number": req.task,
                "status": req.status,
            }),
        );

        if req.status == "completed" || req.status == "skipped" {
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
                "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                 VALUES (?1, ?2, 'failed', datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = 'failed', updated_at = excluded.updated_at",
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
