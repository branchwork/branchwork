//! Plan/task tools.
//!
//! Read-only queries: `list_plans`, `get_plan`, `get_task`, `get_task_context`.
//! Writes: `create_plan` (used by plan-creator agents to hand a generated YAML
//! body back to the server — see ADR / dashboard-stability T5.4).
//!
//! Give MCP clients (agents) structured access to plan data without having to
//! parse markdown/YAML themselves, and without ever needing direct filesystem
//! access to the server's plans directory.
//!
//! Each tool returns [`rmcp::Json<T>`] so its schema is surfaced as
//! `structured_content` in the MCP result.

use std::collections::HashMap;

use rmcp::{ErrorData as McpError, Json, handler::server::wrapper::Parameters, tool, tool_router};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreatePlanRequest {
    /// Short kebab-case slug used as the plan's filename (without
    /// extension). Letters, digits, hyphens, underscores; max 64
    /// chars; must not start or end with a hyphen.
    pub name: String,
    /// Full plan YAML body. Must parse via the same schema the
    /// dashboard reads (title, context, phases, etc.).
    pub yaml_body: String,
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
pub struct CreatePlanResult {
    pub ok: bool,
    /// The plan slug as it was stored (echoed from the request).
    pub name: String,
    /// Absolute path of the YAML file the server wrote.
    pub path: String,
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
    /// Pivot 2026-06-11 — project practices whose scope (path globs /
    /// keywords) matches this task's file_paths or title/description.
    /// Advisory rules the agent should apply while doing the work.
    pub practices: Vec<crate::mcp::tools::practices::PracticeHit>,
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
        description = "Create a new plan from a YAML body. The server validates the \
                       YAML, atomically writes it under the server's plans directory \
                       as `<name>.yaml`, and broadcasts `plan_updated` so the \
                       dashboard refreshes. Use this from a plan-creator agent — the \
                       agent's host may not be the server's host, so writing the file \
                       directly is unreliable. Returns 400-equivalent on invalid name, \
                       invalid YAML, or name collision."
    )]
    pub async fn create_plan(
        &self,
        Parameters(req): Parameters<CreatePlanRequest>,
    ) -> Result<Json<CreatePlanResult>, McpError> {
        let name = req.name.trim();
        if !is_valid_plan_slug(name) {
            return Err(McpError::invalid_params(
                format!(
                    "Invalid plan name {name:?}: must be a non-empty kebab-case slug \
                     (lowercase letters, digits, `-`, `_`; no leading/trailing hyphen; \
                     max 64 chars)."
                ),
                None,
            ));
        }

        // Reject collision up-front. POSIX rename is atomic but it would silently
        // overwrite an existing file — a noisy 4xx-style error is friendlier than
        // a clobbered plan.
        let final_path = self.ctx.plans_dir.join(format!("{name}.yaml"));
        if final_path.exists() {
            return Err(McpError::invalid_params(
                format!(
                    "Plan {name:?} already exists. Pick a different slug or update \
                     the existing plan via the dashboard."
                ),
                None,
            ));
        }

        // Validate the YAML parses against the dashboard's schema before we
        // persist it. Reject malformed input so the file watcher never sees a
        // half-baked plan.
        plan_parser::parse_plan_yaml(&req.yaml_body, name, "<unsaved>")
            .map_err(|e| McpError::invalid_params(format!("Invalid plan YAML: {e}"), None))?;

        // Atomic rename pattern: write to a sibling temp file (same filesystem,
        // so `rename` is atomic on POSIX), then rename onto the final path. No
        // torn writes possible — concurrent `create_plan` calls for the same
        // name will each rename their own temp atomically; one wins, the other
        // sees a complete file or its temp gets cleaned up on error.
        if let Err(e) = std::fs::create_dir_all(&self.ctx.plans_dir) {
            return Err(McpError::internal_error(
                format!("create plans dir: {e}"),
                None,
            ));
        }
        let tmp_path = self
            .ctx
            .plans_dir
            .join(format!(".{}.{}.tmp", name, Uuid::new_v4()));
        if let Err(e) = std::fs::write(&tmp_path, &req.yaml_body) {
            return Err(McpError::internal_error(
                format!("write tmp plan file: {e}"),
                None,
            ));
        }
        if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(McpError::internal_error(
                format!("rename plan file into place: {e}"),
                None,
            ));
        }

        // Broadcast `plan_updated` immediately so the dashboard refetches
        // without waiting for the file watcher's 500 ms debounce. The watcher
        // will also fire shortly; the dashboard's plan-store dedupes by mtime.
        if let Ok(parsed) = plan_parser::parse_plan_file(&final_path) {
            crate::ws::broadcast_event(
                &self.ctx.broadcast_tx,
                "plan_updated",
                serde_json::json!({
                    "action": "changed",
                    "plan": parsed,
                }),
            );
        }

        Ok(Json(CreatePlanResult {
            ok: true,
            name: name.to_string(),
            path: final_path.to_string_lossy().to_string(),
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

        // Pivot 2026-06-11 — scoped practices ride along with the context:
        // the rule reaches the agent at the exact moment it starts the work
        // that needs it (see mcp/tools/practices.rs).
        let practices = {
            let conn = self.ctx.db.lock().unwrap();
            crate::mcp::tools::practices::practices_for_task(
                &conn,
                &task.file_paths,
                &format!("{} {}", task.title, task.description),
            )
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
            practices,
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

/// Strict whitelist for plan slugs accepted by `create_plan`.
///
/// - Lowercase ASCII letters, digits, `-`, `_`
/// - Length 1..=64
/// - Must not start or end with a hyphen
///
/// Rejects path traversal (`..`), separators (`/`, `\`), and dotfiles (e.g.
/// `.hidden`) because none of those characters are in the allowed set.
fn is_valid_plan_slug(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::config::Effort;
    use crate::db;
    use crate::mcp::McpContext;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    fn test_mcp() -> (BranchworkMcp, broadcast::Receiver<String>, TempDir) {
        let dir = TempDir::new().unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let db = db::init(&dir.path().join("test.db"));
        let (tx, rx) = broadcast::channel::<String>(32);
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
        (BranchworkMcp::new(ctx), rx, dir)
    }

    fn valid_yaml(title: &str) -> String {
        format!(
            "title: \"{title}\"\n\
             context: |\n  Test plan.\n\
             phases:\n  - number: 0\n    title: Phase\n    description: ''\n    \
             tasks:\n      - number: '0.1'\n        title: First\n        \
             description: ''\n        acceptance: ''\n"
        )
    }

    #[test]
    fn slug_validator_accepts_kebab_and_snake() {
        for ok in [
            "x",
            "auth-revamp",
            "user_auth",
            "fix-plan-done-in-progress",
            "plan-2",
            "p1",
            "a-b-c-d-e-f-g-h-i-j-k",
        ] {
            assert!(is_valid_plan_slug(ok), "expected accepted: {ok:?}");
        }
    }

    #[test]
    fn slug_validator_rejects_bad_inputs() {
        let too_long = "a".repeat(65);
        let cases: &[&str] = &[
            "",
            "-leading",
            "trailing-",
            "../etc",
            "/etc/passwd",
            "..",
            ".hidden",
            "Capital",
            "spaces here",
            "a\u{00e9}b",
            "name.with.dots",
            &too_long,
        ];
        for bad in cases {
            assert!(!is_valid_plan_slug(bad), "expected rejected: {bad:?}");
        }
    }

    #[tokio::test]
    async fn create_plan_writes_file_and_broadcasts() {
        let (mcp, mut rx, _dir) = test_mcp();
        let result = mcp
            .create_plan(Parameters(CreatePlanRequest {
                name: "test-plan".to_string(),
                yaml_body: valid_yaml("Test Plan"),
            }))
            .await
            .expect("create_plan ok");
        assert!(result.0.ok);
        assert_eq!(result.0.name, "test-plan");
        // File appeared at the expected path.
        let path = std::path::Path::new(&result.0.path);
        assert!(path.is_file(), "expected file at {}", result.0.path);
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert!(on_disk.contains("Test Plan"));
        // plan_updated broadcast was emitted with action=changed.
        let msg = rx
            .try_recv()
            .expect("expected plan_updated broadcast on the channel");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "plan_updated");
        assert_eq!(v["data"]["action"], "changed");
        assert_eq!(v["data"]["plan"]["title"], "Test Plan");
    }

    fn unwrap_err<T>(r: Result<rmcp::Json<T>, McpError>) -> McpError {
        match r {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        }
    }

    #[tokio::test]
    async fn create_plan_rejects_invalid_yaml_no_file_written() {
        let (mcp, mut rx, dir) = test_mcp();
        let plans_dir = dir.path().join("plans");
        let err = unwrap_err(
            mcp.create_plan(Parameters(CreatePlanRequest {
                name: "bad-yaml".to_string(),
                yaml_body: "this is not: valid: yaml: at all".to_string(),
            }))
            .await,
        );
        assert!(
            err.message.contains("Invalid plan YAML"),
            "unexpected error message: {}",
            err.message
        );
        // No file should have been created (final or temp).
        let entries: Vec<_> = std::fs::read_dir(&plans_dir).unwrap().collect();
        assert!(
            entries.is_empty(),
            "expected empty plans dir, got: {entries:?}"
        );
        // No broadcast either.
        assert!(rx.try_recv().is_err(), "should not broadcast on error");
    }

    #[tokio::test]
    async fn create_plan_rejects_invalid_slug() {
        let (mcp, _rx, _dir) = test_mcp();
        let err = unwrap_err(
            mcp.create_plan(Parameters(CreatePlanRequest {
                name: "../escape".to_string(),
                yaml_body: valid_yaml("Doesn't Matter"),
            }))
            .await,
        );
        assert!(
            err.message.contains("Invalid plan name"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn create_plan_rejects_collision() {
        let (mcp, _rx, _dir) = test_mcp();
        let req = || CreatePlanRequest {
            name: "dup".to_string(),
            yaml_body: valid_yaml("First"),
        };
        mcp.create_plan(Parameters(req())).await.expect("first ok");
        let err = unwrap_err(mcp.create_plan(Parameters(req())).await);
        assert!(
            err.message.contains("already exists"),
            "unexpected error message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn concurrent_calls_same_name_no_torn_writes() {
        // Many writers race for the same slug; exactly one should land a
        // complete, parseable YAML on disk and the rest should fail with the
        // collision error. None of the failures should leave a torn file.
        let (mcp, _rx, dir) = test_mcp();
        let plans_dir = dir.path().join("plans");
        let mcp = std::sync::Arc::new(mcp);
        let mut handles = Vec::new();
        for i in 0..8 {
            let mcp = mcp.clone();
            handles.push(tokio::spawn(async move {
                mcp.create_plan(Parameters(CreatePlanRequest {
                    name: "race".to_string(),
                    yaml_body: valid_yaml(&format!("Writer {i}")),
                }))
                .await
            }));
        }
        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            results.push(h.await.expect("task panicked"));
        }
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok_count, 1, "expected exactly one successful create");
        // The single file on disk parses cleanly.
        let final_path = plans_dir.join("race.yaml");
        let parsed = plan_parser::parse_plan_file(&final_path).expect("parses");
        assert!(parsed.title.starts_with("Writer "));
        // No leftover *.tmp siblings.
        let tmp_files: Vec<_> = std::fs::read_dir(&plans_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmp_files.is_empty(), "leftover temp files: {tmp_files:?}");
    }
}
