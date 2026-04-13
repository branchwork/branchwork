use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::agents::{check_agent, pty_agent};
use crate::auto_status;
use crate::plan_parser;
use crate::state::AppState;

// ── GET /api/plans ───────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanListEntry {
    name: String,
    title: String,
    project: Option<String>,
    phase_count: usize,
    task_count: usize,
    done_count: usize,
    created_at: String,
    modified_at: String,
}

pub async fn list_plans(State(state): State<AppState>) -> impl IntoResponse {
    let summaries = plan_parser::list_plans(&state.plans_dir);

    let db = state.db.lock().unwrap();

    // Load all project overrides
    let mut overrides: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) = db.prepare("SELECT plan_name, project FROM plan_project")
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            overrides.insert(row.0, row.1);
        }
    }

    let entries: Vec<PlanListEntry> = summaries
        .into_iter()
        .map(|s| {
            // Parse the full plan to merge statuses and get accurate done count
            let done_count = plan_parser::find_plan_file(&state.plans_dir, &s.name)
                .and_then(|path| plan_parser::parse_plan_file(&path).ok())
                .map(|parsed| {
                    let mut status_map: HashMap<String, String> = HashMap::new();
                    if let Ok(mut stmt) = db
                        .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
                        && let Ok(rows) = stmt.query_map(params![s.name], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                    {
                        for row in rows.flatten() {
                            status_map.insert(row.0, row.1);
                        }
                    }

                    parsed
                        .phases
                        .iter()
                        .flat_map(|p| &p.tasks)
                        .filter(|t| {
                            let status = status_map
                                .get(&t.number)
                                .map(|s| s.as_str())
                                .unwrap_or("pending");
                            status == "completed" || status == "skipped"
                        })
                        .count()
                })
                .unwrap_or(0);

            let project = overrides.get(&s.name).cloned().or(s.project);

            PlanListEntry {
                name: s.name,
                title: s.title,
                project,
                phase_count: s.phase_count,
                task_count: s.task_count,
                done_count,
                created_at: s.created_at,
                modified_at: s.modified_at,
            }
        })
        .collect();

    Json(entries)
}

// ── GET /api/plans/:name ─────────────────────────────────────────────────────

pub async fn get_plan(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let mut plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan: {e}"),
                    "file": plan_path.to_string_lossy(),
                })),
            )
                .into_response();
        }
    };

    let db = state.db.lock().unwrap();

    // Merge task statuses
    if let Ok(mut stmt) =
        db.prepare("SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?")
    {
        let mut status_map: HashMap<String, (String, String)> = HashMap::new();
        if let Ok(rows) = stmt.query_map(params![name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                status_map.insert(row.0, (row.1, row.2));
            }
        }

        for phase in &mut plan.phases {
            for task in &mut phase.tasks {
                if let Some((status, updated_at)) = status_map.get(&task.number) {
                    task.status = Some(status.clone());
                    task.status_updated_at = Some(updated_at.clone());
                } else {
                    task.status = Some("pending".to_string());
                }
            }
        }
    }

    // Merge DB project override
    if let Ok(project) = db.query_row(
        "SELECT project FROM plan_project WHERE plan_name = ?",
        params![name],
        |row| row.get::<_, String>(0),
    ) {
        plan.project = Some(project);
    }

    Json(serde_json::to_value(plan).unwrap()).into_response()
}

// ── PUT /api/plans/:name/project ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ProjectBody {
    project: String,
}

pub async fn set_project(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ProjectBody>,
) -> impl IntoResponse {
    if body.project.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "project is required"})),
        );
    }

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO plan_project (plan_name, project, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(plan_name)
         DO UPDATE SET project = excluded.project, updated_at = excluded.updated_at",
        params![name, body.project],
    )
    .unwrap();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "plan_name": name,
            "project": body.project,
        })),
    )
}

// ── PUT /api/plans/:name/tasks/:num/status ───────────────────────────────────

#[derive(Deserialize)]
pub struct StatusBody {
    status: String,
}

pub async fn set_task_status(
    State(state): State<AppState>,
    Path((name, task_number)): Path<(String, String)>,
    Json(body): Json<StatusBody>,
) -> impl IntoResponse {
    let valid = [
        "pending",
        "in_progress",
        "completed",
        "failed",
        "skipped",
        "checking",
    ];
    if !valid.contains(&body.status.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Invalid status. Must be one of: {}", valid.join(", "))
            })),
        );
    }

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO task_status (plan_name, task_number, status, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(plan_name, task_number)
         DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
        params![name, task_number, body.status],
    )
    .unwrap();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "plan_name": name,
            "task_number": task_number,
            "status": body.status,
        })),
    )
}

// ── GET /api/plans/:name/statuses ────────────────────────────────────────────

#[derive(Serialize)]
struct TaskStatusRow {
    task_number: String,
    status: String,
    updated_at: String,
}

pub async fn get_statuses(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?")
        .unwrap();

    let rows: Vec<TaskStatusRow> = stmt
        .query_map(params![name], |row| {
            Ok(TaskStatusRow {
                task_number: row.get(0)?,
                status: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

// ── POST /api/plans/:name/auto-status ───────────────────────────────────────

pub async fn auto_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response();
        }
    };

    let home = dirs::home_dir().unwrap();
    let project_dir = home.join(project);
    if !project_dir.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Project directory not found: {}", project_dir.display())})),
        )
            .into_response();
    }

    let db = state.db.lock().unwrap();

    // Load existing manual statuses
    let mut manual: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
        && let Ok(rows) = stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            manual.insert(row.0, row.1);
        }
    }

    let mut results = Vec::new();
    let mut summary: HashMap<String, usize> = HashMap::from([
        ("completed".into(), 0),
        ("in_progress".into(), 0),
        ("pending".into(), 0),
    ]);

    for phase in &plan.phases {
        for task in &phase.tasks {
            // If a status already exists in DB (manual or prior auto), keep it.
            // Only auto-infer for tasks with no DB row at all.
            if let Some(s) = manual.get(&task.number) {
                let s = s.clone();
                *summary.entry(s.clone()).or_insert(0) += 1;
                results.push(serde_json::json!({
                    "taskNumber": task.number,
                    "title": task.title,
                    "status": s,
                    "reason": "existing (kept)",
                }));
                continue;
            }

            let title_words: Vec<&str> = task
                .title
                .split_whitespace()
                .filter(|w| w.len() >= 5)
                .collect();

            let (status, reason) =
                auto_status::infer_status(&project_dir, &task.file_paths, &title_words);

            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                 VALUES (?1, ?2, ?3, datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
                params![name, task.number, status],
            )
            .ok();

            *summary.entry(status.to_string()).or_insert(0) += 1;
            results.push(serde_json::json!({
                "taskNumber": task.number,
                "title": task.title,
                "status": status,
                "reason": reason,
            }));
        }
    }

    Json(serde_json::json!({
        "plan": name,
        "project": project,
        "projectDir": project_dir.to_str(),
        "results": results,
        "summary": {
            "total": results.len(),
            "completed": summary.get("completed").unwrap_or(&0),
            "in_progress": summary.get("in_progress").unwrap_or(&0),
            "pending": summary.get("pending").unwrap_or(&0),
        }
    }))
    .into_response()
}

// ── POST /api/plans/sync-all ────────────────────────────────────────────────

pub async fn sync_all(State(state): State<AppState>) -> impl IntoResponse {
    let summaries = plan_parser::list_plans(&state.plans_dir);
    let home = dirs::home_dir().unwrap();
    let db = state.db.lock().unwrap();

    let mut totals: HashMap<String, usize> = HashMap::from([
        ("completed".into(), 0),
        ("in_progress".into(), 0),
        ("pending".into(), 0),
    ]);

    // Match TypeScript: synced = number of plans that have a project set
    let plans_with_project: Vec<_> = summaries.iter().filter(|s| s.project.is_some()).collect();
    let synced = plans_with_project.len();

    for s in &plans_with_project {
        let project = s.project.as_deref().unwrap();
        let project_dir = home.join(project);
        if !project_dir.is_dir() {
            continue;
        }

        let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &s.name) {
            Some(p) => p,
            None => continue,
        };
        let plan = match plan_parser::parse_plan_file(&plan_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Load existing statuses
        let mut manual: HashMap<String, String> = HashMap::new();
        if let Ok(mut stmt) =
            db.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            && let Ok(rows) = stmt.query_map(params![s.name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
        {
            for row in rows.flatten() {
                manual.insert(row.0, row.1);
            }
        }

        for phase in &plan.phases {
            for task in &phase.tasks {
                // Keep any existing DB status (manual or prior auto)
                if let Some(s) = manual.get(&task.number) {
                    *totals.entry(s.clone()).or_insert(0) += 1;
                    continue;
                }

                let title_words: Vec<&str> = task
                    .title
                    .split_whitespace()
                    .filter(|w| w.len() >= 5)
                    .collect();

                let (status, _) =
                    auto_status::infer_status(&project_dir, &task.file_paths, &title_words);

                db.execute(
                    "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                     VALUES (?1, ?2, ?3, datetime('now'))
                     ON CONFLICT(plan_name, task_number)
                     DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at",
                    params![s.name, task.number, status],
                )
                .ok();

                *totals.entry(status.to_string()).or_insert(0) += 1;
            }
        }
    }

    Json(serde_json::json!({
        "synced": synced,
        "completed": totals.get("completed").unwrap_or(&0),
        "in_progress": totals.get("in_progress").unwrap_or(&0),
        "pending": totals.get("pending").unwrap_or(&0),
    }))
}

// ── POST /api/actions/start-task ────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartTaskBody {
    plan_name: String,
    phase_number: u32,
    task_number: String,
    cwd: Option<String>,
    mode: Option<String>,
    effort: Option<String>,
}

pub async fn start_task(
    State(state): State<AppState>,
    Json(body): Json<StartTaskBody>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &body.plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let phase = match plan.phases.iter().find(|p| p.number == body.phase_number) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Phase not found"})),
            )
                .into_response();
        }
    };

    let task = match phase.tasks.iter().find(|t| t.number == body.task_number) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Task not found"})),
            )
                .into_response();
        }
    };

    let is_continue = body.mode.as_deref() == Some("continue");
    let home = dirs::home_dir().unwrap();
    let work_dir = body.cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    });

    let files_section = if !task.file_paths.is_empty() {
        format!(
            "\nFiles involved:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        String::new()
    };

    let acceptance_section = if !task.acceptance.is_empty() {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    } else {
        String::new()
    };

    let prompt = format!(
        "{intro}\n\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Description:\n{description}\n\
         {files}{acceptance}\n\n\
         {instruction}\n\n\
         IMPORTANT: When you think you are done, do NOT stop. Instead:\n\
         1. Summarize what you did\n\
         2. Mark the task status by running: curl -s -X PUT http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/status -H \"Content-Type: application/json\" -d '{{\"status\":\"completed\"}}'\n\
         3. Ask the user if they need anything else\n\
         4. Only stop when the user explicitly says they are done",
        intro = if is_continue {
            "You are continuing work on a partially implemented task. Some parts may already exist — check the current state of the code before making changes."
        } else {
            "You are working on the following task from a plan."
        },
        plan_title = plan.title,
        phase_num = phase.number,
        phase_title = phase.title,
        task_num = task.number,
        task_title = task.title,
        description = task.description,
        files = files_section,
        acceptance = acceptance_section,
        instruction = if is_continue {
            "First, read the relevant files to understand what has already been done. Then complete the remaining work."
        } else {
            "Please implement this task. When done, summarize what you changed."
        },
        port = state.config_port(),
        plan_name = body.plan_name,
    );

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    let agent_id = pty_agent::start_pty_agent(
        &state.registry,
        prompt,
        &work_dir,
        Some(&body.plan_name),
        Some(&body.task_number),
        effort,
    )
    .await;

    Json(serde_json::json!({
        "agentId": agent_id,
        "taskId": body.task_number,
    }))
    .into_response()
}

// ── POST /api/plans/:name/tasks/:num/check ──────────────────────────────────

pub async fn check_task(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response();
        }
    };

    let phase = plan
        .phases
        .iter()
        .find(|p| p.tasks.iter().any(|t| t.number == task_number));
    let task = phase.and_then(|p| p.tasks.iter().find(|t| t.number == task_number));
    let (phase, task) = match (phase, task) {
        (Some(p), Some(t)) => (p, t),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Task not found"})),
            )
                .into_response();
        }
    };

    let home = dirs::home_dir().unwrap();
    let project_dir = home.join(project);

    let files_section = if !task.file_paths.is_empty() {
        format!(
            "\nFiles mentioned:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        String::new()
    };

    let acceptance_section = if !task.acceptance.is_empty() {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    } else {
        String::new()
    };

    let prompt = format!(
        "You are verifying whether a task from a plan has been implemented.\n\
         Answer with ONLY a JSON object, no other text: {{\"status\": \"completed\"|\"in_progress\"|\"pending\", \"reason\": \"brief explanation\"}}\n\n\
         Project directory: {project_dir}\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Task description:\n{description}\n\
         {files}{acceptance}\n\n\
         Check the project at {project_dir}. Read the relevant files. Determine if this task is:\n\
         - \"completed\": all described changes exist in the code\n\
         - \"in_progress\": some changes exist but the task is not fully done\n\
         - \"pending\": no evidence of this task being started\n\n\
         Respond with ONLY the JSON object.",
        project_dir = project_dir.display(),
        plan_title = plan.title,
        phase_num = phase.number,
        phase_title = phase.title,
        task_num = task.number,
        task_title = task.title,
        description = task.description,
        files = files_section,
        acceptance = acceptance_section,
    );

    // Set task to checking state
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at)
             VALUES (?1, ?2, 'checking', datetime('now'))
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = 'checking', updated_at = datetime('now')",
            params![plan_name, task_number],
        )
        .ok();
    }

    let effort = *state.effort.lock().unwrap();
    let agent_id = check_agent::start_check_agent(
        &state.registry,
        prompt,
        &project_dir,
        Some(&plan_name),
        Some(&task_number),
        effort,
    )
    .await;

    Json(serde_json::json!({
        "agentId": agent_id,
        "planName": plan_name,
        "taskNumber": task_number,
    }))
    .into_response()
}

// ── POST /api/plans/create ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePlanBody {
    description: String,
    folder: String,
    create_folder: Option<bool>,
}

pub async fn create_plan(
    State(state): State<AppState>,
    Json(body): Json<CreatePlanBody>,
) -> impl IntoResponse {
    if body.description.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "description is required"})),
        )
            .into_response();
    }
    if body.folder.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "folder is required"})),
        )
            .into_response();
    }

    let home = dirs::home_dir().unwrap();
    let resolved = if body.folder.starts_with('~') {
        home.join(body.folder[1..].trim_start_matches('/'))
    } else {
        std::path::PathBuf::from(&body.folder)
    };

    if !resolved.exists() {
        if body.create_folder != Some(true) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "folder_not_found",
                    "message": format!("Directory does not exist: {}", resolved.display()),
                    "resolvedFolder": resolved.to_str(),
                })),
            )
                .into_response();
        }
        std::fs::create_dir_all(&resolved).ok();
    }

    if !resolved.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Not a directory: {}", resolved.display())})),
        )
            .into_response();
    }

    let plans_dir = state.plans_dir.display();
    let prompt = format!(
        "You are creating an implementation plan for a project.\n\n\
         Working directory: {folder}\n\n\
         Request:\n{description}\n\n\
         Create a detailed implementation plan as a YAML file.\n\
         First explore the working directory to understand the existing codebase (if any).\n\
         Then write the plan to a file at {plans_dir}/<generated-name>.yaml using the Write tool.\n\
         The filename should be a short kebab-case slug derived from the plan title.\n\n\
         Use this exact YAML structure:\n\
         ```yaml\n\
         title: \"Plan Title\"\n\
         context: |\n\
         \x20 Brief background and motivation.\n\
         phases:\n\
         \x20 - number: 0\n\
         \x20   title: \"Phase Title\"\n\
         \x20   description: \"Phase description\"\n\
         \x20   tasks:\n\
         \x20     - number: \"0.1\"\n\
         \x20       title: \"Task Title\"\n\
         \x20       description: |\n\
         \x20         What needs to be done.\n\
         \x20       file_paths:\n\
         \x20         - path/to/file.rs\n\
         \x20       acceptance: \"Success criteria\"\n\
         \x20       dependencies: []\n\
         ```\n\n\
         Continue with Phase 1, 2, etc. Task numbers use phase.index format (0.1, 0.2, 1.1, etc.).\n\
         The dependencies field lists task numbers this task depends on (e.g. [\"0.1\", \"0.2\"]).\n\n\
         IMPORTANT: When you are finished, do NOT stop. Instead:\n\
         1. Summarize the plan you created\n\
         2. Ask the user if they want to adjust anything\n\
         3. Only stop when the user explicitly says they are done",
        folder = resolved.display(),
        description = body.description,
        plans_dir = plans_dir,
    );

    let effort = *state.effort.lock().unwrap();
    let agent_id =
        pty_agent::start_pty_agent(&state.registry, prompt, &resolved, None, None, effort).await;

    let project_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    Json(serde_json::json!({
        "agentId": agent_id,
        "folder": resolved.to_str(),
        "projectName": project_name,
    }))
    .into_response()
}

// ── PUT /api/plans/:name ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlanBody {
    title: String,
    context: String,
    project: Option<String>,
    phases: Vec<UpdatePhaseBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePhaseBody {
    number: u32,
    title: String,
    #[serde(default)]
    description: String,
    tasks: Vec<UpdateTaskBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskBody {
    number: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    file_paths: Vec<String>,
    #[serde(default)]
    acceptance: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

pub async fn update_plan(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpdatePlanBody>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    // Build a ParsedPlan from the update body
    let plan = plan_parser::ParsedPlan {
        name: name.clone(),
        file_path: plan_path.to_string_lossy().to_string(),
        title: body.title,
        context: body.context,
        project: body.project,
        created_at: String::new(),
        modified_at: String::new(),
        phases: body
            .phases
            .into_iter()
            .map(|p| plan_parser::PlanPhase {
                number: p.number,
                title: p.title,
                description: p.description,
                tasks: p
                    .tasks
                    .into_iter()
                    .map(|t| plan_parser::PlanTask {
                        number: t.number,
                        title: t.title,
                        description: t.description,
                        file_paths: t.file_paths,
                        acceptance: t.acceptance,
                        dependencies: t.dependencies,
                        status: None,
                        status_updated_at: None,
                    })
                    .collect(),
            })
            .collect(),
    };

    // Always write as YAML
    let yaml = match plan_parser::serialize_plan_yaml(&plan) {
        Ok(y) => y,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize: {e}")})),
            )
                .into_response();
        }
    };

    // Write to .yaml (migrate from .md if needed)
    let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
    if let Err(e) = std::fs::write(&yaml_path, &yaml) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write: {e}")})),
        )
            .into_response();
    }

    // Remove old .md if we just migrated
    if plan_path.extension().is_some_and(|e| e == "md") {
        std::fs::remove_file(&plan_path).ok();
    }

    Json(serde_json::json!({"ok": true, "name": name})).into_response()
}

// ── POST /api/plans/:name/convert ──────────────────────────────────────────

pub async fn convert_plan(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let md_path = state.plans_dir.join(format!("{name}.md"));
    if !md_path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No .md plan found with that name"})),
        )
            .into_response();
    }

    let plan = match plan_parser::parse_plan_file(&md_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to parse plan: {e}")})),
            )
                .into_response();
        }
    };

    let yaml = match plan_parser::serialize_plan_yaml(&plan) {
        Ok(y) => y,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize YAML: {e}")})),
            )
                .into_response();
        }
    };

    let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
    if let Err(e) = std::fs::write(&yaml_path, &yaml) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write YAML: {e}")})),
        )
            .into_response();
    }

    // Remove the .md file now that .yaml exists
    std::fs::remove_file(&md_path).ok();

    Json(serde_json::json!({
        "ok": true,
        "name": name,
        "yamlPath": yaml_path.to_str(),
    }))
    .into_response()
}

// ── POST /api/plans/convert-all ────────────────────────────────────────────

pub async fn convert_all(State(state): State<AppState>) -> impl IntoResponse {
    let entries = match std::fs::read_dir(&state.plans_dir) {
        Ok(e) => e,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to read plans directory: {e}")
            }))
            .into_response();
        }
    };

    let md_files: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();

    let mut converted = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for entry in &md_files {
        let path = entry.path();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Skip if .yaml already exists
        let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
        if yaml_path.exists() {
            skipped.push(name);
            continue;
        }

        let plan = match plan_parser::parse_plan_file(&path) {
            Ok(p) => p,
            Err(e) => {
                failed.push(serde_json::json!({"name": name, "error": e.to_string()}));
                continue;
            }
        };

        // Skip plans that parsed poorly (0 tasks)
        let task_count: usize = plan.phases.iter().map(|p| p.tasks.len()).sum();
        if task_count == 0 {
            failed.push(
                serde_json::json!({"name": name, "error": "0 tasks parsed — needs manual review"}),
            );
            continue;
        }

        match plan_parser::serialize_plan_yaml(&plan) {
            Ok(yaml) => {
                if let Err(e) = std::fs::write(&yaml_path, &yaml) {
                    failed.push(serde_json::json!({"name": name, "error": e.to_string()}));
                } else {
                    std::fs::remove_file(&path).ok();
                    converted.push(name);
                }
            }
            Err(e) => {
                failed.push(serde_json::json!({"name": name, "error": e}));
            }
        }
    }

    Json(serde_json::json!({
        "converted": converted.len(),
        "skipped": skipped.len(),
        "failed": failed.len(),
        "convertedNames": converted,
        "skippedNames": skipped,
        "failures": failed,
    }))
    .into_response()
}
