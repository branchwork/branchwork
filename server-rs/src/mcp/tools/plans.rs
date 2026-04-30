//! Plan/task query tools: `list_plans`, `get_plan`, `get_task`,
//! `get_task_context`. Give MCP clients (agents) structured access to plan
//! data without having to parse markdown/YAML themselves.
//!
//! Each tool returns [`rmcp::Json<T>`] so its schema is surfaced as
//! `structured_content` in the MCP result.

use std::collections::HashMap;

use rmcp::{ErrorData as McpError, Json, handler::server::wrapper::Parameters, tool, tool_router};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::build_cross_plan_context;
use crate::db as dbmod;
use crate::mcp::BranchworkMcp;
use crate::plan_parser;

// ── Request schemas ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetPlanRequest {
    /// Plan name (file stem, e.g. `my-plan` for `my-plan.yaml`).
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetTaskRequest {
    /// Plan name (file stem).
    pub plan: String,
    /// Task number as it appears in the plan, e.g. `2.3`.
    pub task_number: String,
}

// ── Response schemas ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanListItem {
    pub name: String,
    pub title: String,
    pub project: Option<String>,
    pub phase_count: usize,
    pub task_count: usize,
    pub done_count: usize,
}

/// Wrapper so the tool's outputSchema root is an object (MCP requirement).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanList {
    pub plans: Vec<PlanListItem>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaskEntry {
    pub number: String,
    pub title: String,
    pub status: String,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PhaseEntry {
    pub number: u32,
    pub title: String,
    pub description: String,
    pub tasks: Vec<TaskEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanDetail {
    pub name: String,
    pub title: String,
    pub project: Option<String>,
    pub context: String,
    pub phases: Vec<PhaseEntry>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaskDetail {
    pub plan_name: String,
    pub phase_number: u32,
    pub number: String,
    pub title: String,
    pub description: String,
    pub file_paths: Vec<String>,
    pub acceptance: String,
    pub dependencies: Vec<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaskContext {
    pub plan_name: String,
    pub number: String,
    pub title: String,
    pub description: String,
    pub file_paths: Vec<String>,
    pub acceptance: String,
    pub dependencies: Vec<String>,
    pub status: String,
    /// Learnings previously recorded on this task.
    pub learnings: Vec<String>,
    /// Tasks from the same project (this plan + sibling plans) that have
    /// already completed or were skipped — with their recorded learnings, so
    /// callers inherit context from predecessors.
    pub prior_related_tasks: Vec<String>,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = plans_router, vis = "pub")]
impl BranchworkMcp {
    #[tool(
        description = "List all plans with name, title, project, and task counts \
                       (including how many tasks are already completed or skipped)."
    )]
    pub async fn list_plans(&self) -> Result<Json<PlanList>, McpError> {
        let summaries = plan_parser::list_plans(&self.ctx.plans_dir);

        let db = self.ctx.db.lock().unwrap();

        // Project overrides.
        let overrides: HashMap<String, String> = db
            .prepare("SELECT plan_name, project FROM plan_project")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()
            })
            .unwrap_or_default();

        let mut out = Vec::with_capacity(summaries.len());
        for s in summaries {
            let done_count = plan_parser::find_plan_file(&self.ctx.plans_dir, &s.name)
                .and_then(|p| plan_parser::parse_plan_file(&p).ok())
                .map(|parsed| {
                    let done = dbmod::completed_task_numbers(&db, &s.name);
                    parsed
                        .phases
                        .iter()
                        .flat_map(|p| &p.tasks)
                        .filter(|t| done.contains(&t.number))
                        .count()
                })
                .unwrap_or(0);

            let project = overrides.get(&s.name).cloned().or(s.project);
            out.push(PlanListItem {
                name: s.name,
                title: s.title,
                project,
                phase_count: s.phase_count,
                task_count: s.task_count,
                done_count,
            });
        }

        Ok(Json(PlanList { plans: out }))
    }

    #[tool(
        description = "Return a plan by name, with all phases, tasks (number, title, \
                       dependencies), and their current status from the DB."
    )]
    pub async fn get_plan(
        &self,
        Parameters(req): Parameters<GetPlanRequest>,
    ) -> Result<Json<PlanDetail>, McpError> {
        let plan = load_plan(self, &req.name)?;
        let statuses = task_status_map(&self.ctx.db, &req.name);

        let phases = plan
            .phases
            .into_iter()
            .map(|ph| PhaseEntry {
                number: ph.number,
                title: ph.title,
                description: ph.description,
                tasks: ph
                    .tasks
                    .into_iter()
                    .map(|t| TaskEntry {
                        status: statuses
                            .get(&t.number)
                            .cloned()
                            .unwrap_or_else(|| "pending".to_string()),
                        number: t.number,
                        title: t.title,
                        dependencies: t.dependencies,
                    })
                    .collect(),
            })
            .collect();

        Ok(Json(PlanDetail {
            name: plan.name,
            title: plan.title,
            project: plan.project,
            context: plan.context,
            phases,
        }))
    }

    #[tool(
        description = "Return a single task by plan name + task number (e.g. \"2.3\"), \
                       including description, file paths, acceptance criteria, \
                       dependencies, and current status."
    )]
    pub async fn get_task(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskDetail>, McpError> {
        let plan = load_plan(self, &req.plan)?;
        let (phase_number, task) = find_task(&plan, &req.task_number)?;
        let status = task_status(&self.ctx.db, &req.plan, &req.task_number);

        Ok(Json(TaskDetail {
            plan_name: plan.name,
            phase_number,
            number: task.number,
            title: task.title,
            description: task.description,
            file_paths: task.file_paths,
            acceptance: task.acceptance,
            dependencies: task.dependencies,
            status,
        }))
    }

    #[tool(
        description = "Return rich context for a task: its fields plus this task's \
                       recorded learnings and a listing of prior related tasks \
                       (completed/skipped in the same project) with their learnings."
    )]
    pub async fn get_task_context(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskContext>, McpError> {
        let plan = load_plan(self, &req.plan)?;
        let (_phase_number, task) = find_task(&plan, &req.task_number)?;
        let status = task_status(&self.ctx.db, &req.plan, &req.task_number);

        let learnings = {
            let conn = self.ctx.db.lock().unwrap();
            dbmod::task_learnings(&conn, &req.plan, &req.task_number)
        };

        // Reuse the same listing agent prompts get. Emits a pre-formatted
        // block with "- Task X: title — files: ..." lines and indented "    •"
        // learnings; we split it into lines for structured consumption.
        let prior_related_tasks =
            build_cross_plan_context(&self.ctx.db, &self.ctx.plans_dir, &plan, &req.task_number)
                .map(|s| {
                    s.lines()
                        .skip(1) // drop the "Related work…" header line
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

        Ok(Json(TaskContext {
            plan_name: plan.name,
            number: task.number,
            title: task.title,
            description: task.description,
            file_paths: task.file_paths,
            acceptance: task.acceptance,
            dependencies: task.dependencies,
            status,
            learnings,
            prior_related_tasks,
        }))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn load_plan(mcp: &BranchworkMcp, name: &str) -> Result<plan_parser::ParsedPlan, McpError> {
    let path = plan_parser::find_plan_file(&mcp.ctx.plans_dir, name)
        .ok_or_else(|| McpError::invalid_params(format!("plan not found: {name}"), None))?;
    plan_parser::parse_plan_file(&path)
        .map_err(|e| McpError::internal_error(format!("failed to parse plan {name}: {e}"), None))
}

fn find_task(
    plan: &plan_parser::ParsedPlan,
    task_number: &str,
) -> Result<(u32, plan_parser::PlanTask), McpError> {
    for phase in &plan.phases {
        for task in &phase.tasks {
            if task.number == task_number {
                return Ok((phase.number, task.clone()));
            }
        }
    }
    Err(McpError::invalid_params(
        format!("task {task_number} not found in plan {}", plan.name),
        None,
    ))
}

fn task_status_map(db: &crate::db::Db, plan_name: &str) -> HashMap<String, String> {
    let conn = db.lock().unwrap();
    conn.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?1")
        .and_then(|mut stmt| {
            stmt.query_map(params![plan_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<_, _>>()
        })
        .unwrap_or_default()
}

fn task_status(db: &crate::db::Db, plan_name: &str, task_number: &str) -> String {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
        params![plan_name, task_number],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "pending".to_string())
}
