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
use crate::templates;

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
    total_cost_usd: f64,
    max_budget_usd: Option<f64>,
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

    // Aggregate cost per plan in one query
    let mut plan_costs: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT plan_name, COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE plan_name IS NOT NULL AND cost_usd IS NOT NULL GROUP BY plan_name",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        for row in rows.flatten() {
            plan_costs.insert(row.0, row.1);
        }
    }

    // Load all plan budgets
    let mut plan_budgets: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare("SELECT plan_name, max_budget_usd FROM plan_budget")
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })
    {
        for row in rows.flatten() {
            plan_budgets.insert(row.0, row.1);
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
            let total_cost_usd = plan_costs.get(&s.name).copied().unwrap_or(0.0);
            let max_budget_usd = plan_budgets.get(&s.name).copied();

            PlanListEntry {
                name: s.name,
                title: s.title,
                project,
                phase_count: s.phase_count,
                task_count: s.task_count,
                done_count,
                created_at: s.created_at,
                modified_at: s.modified_at,
                total_cost_usd,
                max_budget_usd,
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

    // Aggregate agent costs per task and total for this plan
    let mut task_costs: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT task_id, COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE plan_name = ? AND task_id IS NOT NULL AND cost_usd IS NOT NULL GROUP BY task_id",
    ) && let Ok(rows) = stmt.query_map(params![name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        for row in rows.flatten() {
            task_costs.insert(row.0, row.1);
        }
    }

    let plan_total: f64 = db
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
             WHERE plan_name = ? AND cost_usd IS NOT NULL",
            params![name],
            |row| row.get::<_, f64>(0),
        )
        .unwrap_or(0.0);

    plan.total_cost_usd = Some(plan_total);

    // Latest CI run per task for this plan.
    let ci_map = crate::ci::latest_per_task(&db, &name);
    for phase in &mut plan.phases {
        for task in &mut phase.tasks {
            if let Some(c) = task_costs.get(&task.number) {
                task.cost_usd = Some(*c);
            }
            if let Some(ci) = ci_map.get(&task.number) {
                task.ci = Some(ci.clone());
            }
        }
    }

    // Load budget for this plan
    plan.max_budget_usd = db
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?",
            params![name],
            |row| row.get::<_, f64>(0),
        )
        .ok();

    // Load auto-advance flag (opt-in, default off)
    let auto_advance = db
        .query_row(
            "SELECT enabled FROM plan_auto_advance WHERE plan_name = ?",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false);

    let mut value = serde_json::to_value(plan).unwrap();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("autoAdvance".to_string(), serde_json::json!(auto_advance));
    }
    Json(value).into_response()
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

// ── PUT /api/plans/:name/budget ──────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetBody {
    /// Set to null to clear the budget.
    max_budget_usd: Option<f64>,
}

pub async fn set_budget(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<BudgetBody>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    match body.max_budget_usd {
        Some(v) if v > 0.0 => {
            db.execute(
                "INSERT INTO plan_budget (plan_name, max_budget_usd, updated_at)
                 VALUES (?1, ?2, datetime('now'))
                 ON CONFLICT(plan_name)
                 DO UPDATE SET max_budget_usd = excluded.max_budget_usd, updated_at = excluded.updated_at",
                params![name, v],
            )
            .ok();
        }
        _ => {
            db.execute("DELETE FROM plan_budget WHERE plan_name = ?", params![name])
                .ok();
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "planName": name,
            "maxBudgetUsd": body.max_budget_usd,
        })),
    )
}

// ── PUT /api/plans/:name/auto-advance ────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoAdvanceBody {
    enabled: bool,
}

pub async fn set_auto_advance(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<AutoAdvanceBody>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO plan_auto_advance (plan_name, enabled, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(plan_name)
         DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
        params![name, body.enabled as i64],
    )
    .ok();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "planName": name,
            "autoAdvance": body.enabled,
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
    drop(db);

    // Broadcast so the dashboard updates in real-time
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "task_status_changed",
        serde_json::json!({
            "plan_name": name,
            "task_number": task_number,
            "status": body.status,
        }),
    );

    // Auto-advance: if this completed the last task in its phase, kick off the
    // next phase's ready tasks (opt-in per plan). Spawn off so we don't block
    // the HTTP response.
    if body.status == "completed" || body.status == "skipped" {
        let registry = state.registry.clone();
        let plans_dir = state.plans_dir.clone();
        let plan_name = name.clone();
        let task = task_number.clone();
        let effort = *state.effort.lock().unwrap();
        let port = state.config_port();
        tokio::spawn(async move {
            crate::agents::try_auto_advance(registry, plans_dir, plan_name, task, effort, port)
                .await;
        });
    }

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

// ── POST /api/plans/:name/tasks/:num/learnings ───────────────────────────────

#[derive(Deserialize)]
pub struct LearningBody {
    learning: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LearningRow {
    id: i64,
    learning: String,
    created_at: String,
}

pub async fn add_task_learning(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
    Json(body): Json<LearningBody>,
) -> impl IntoResponse {
    let learning = body.learning.trim();
    if learning.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "learning is required"})),
        )
            .into_response();
    }

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
        params![plan_name, task_number, learning],
    )
    .unwrap();
    let id = db.last_insert_rowid();

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "id": id,
            "planName": plan_name,
            "taskNumber": task_number,
            "learning": learning,
        })),
    )
        .into_response()
}

pub async fn list_task_learnings(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let rows: Vec<LearningRow> = db
        .prepare(
            "SELECT id, learning, created_at FROM task_learnings \
             WHERE plan_name = ?1 AND task_number = ?2 ORDER BY id DESC",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![plan_name, task_number], |row| {
                Ok(LearningRow {
                    id: row.get(0)?,
                    learning: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

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

fn plan_remaining_budget(
    db: &rusqlite::Connection,
    plan_name: &str,
) -> Result<Option<f64>, (f64, f64)> {
    let budget: Option<f64> = db
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?",
            params![plan_name],
            |row| row.get::<_, f64>(0),
        )
        .ok();
    match budget {
        Some(max) => {
            let spent: f64 = db
                .query_row(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
                     WHERE plan_name = ? AND cost_usd IS NOT NULL",
                    params![plan_name],
                    |row| row.get::<_, f64>(0),
                )
                .unwrap_or(0.0);
            if spent >= max {
                Err((spent, max))
            } else {
                Ok(Some((max - spent).max(0.0)))
            }
        }
        None => Ok(None),
    }
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

    // Dependency gate: refuse to start if any declared dep is not completed.
    if !task.dependencies.is_empty() {
        let done = {
            let conn = state.db.lock().unwrap();
            crate::db::completed_task_numbers(&conn, &body.plan_name)
        };
        let missing: Vec<String> = task
            .dependencies
            .iter()
            .filter(|d| !done.contains(*d))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "dependencies_not_met",
                    "message": format!("Blocked by task(s): {}", missing.join(", ")),
                    "missing": missing,
                })),
            )
                .into_response();
        }
    }

    // Compute per-agent budget headroom. If plan has a max budget and it's
    // already exhausted, block the start. Otherwise pass the remaining budget
    // to the spawned agent so it self-terminates on overrun.
    let remaining_budget: Option<f64> = {
        let db = state.db.lock().unwrap();
        match plan_remaining_budget(&db, &body.plan_name) {
            Ok(b) => b,
            Err((spent, max)) => {
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(serde_json::json!({
                        "error": "budget_exceeded",
                        "message": format!("Plan budget of ${max:.2} exhausted (spent ${spent:.2})"),
                        "spentUsd": spent,
                        "maxBudgetUsd": max,
                    })),
                )
                    .into_response();
            }
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

    let cross_ctx =
        crate::agents::build_cross_plan_context(&state.db, &state.plans_dir, &plan, &task.number);
    let prompt = crate::agents::build_task_prompt(
        &plan,
        phase,
        task,
        is_continue,
        state.config_port(),
        cross_ctx.as_deref(),
    );

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    // Create a dedicated branch for this task
    let branch_name = format!("orchestrai/{}/{}", body.plan_name, body.task_number);

    let agent_id = pty_agent::start_pty_agent(
        &state.registry,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &work_dir,
            plan_name: Some(&body.plan_name),
            task_id: Some(&body.task_number),
            effort,
            branch: Some(&branch_name),
            is_continue,
            max_budget_usd: remaining_budget,
        },
    )
    .await;

    Json(serde_json::json!({
        "agentId": agent_id,
        "taskId": body.task_number,
        "branch": branch_name,
    }))
    .into_response()
}

// ── POST /api/plans/:name/phases/:num/start ─────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartPhaseBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

pub async fn start_phase_tasks(
    State(state): State<AppState>,
    Path((plan_name, phase_number)): Path<(String, u32)>,
    body: Option<Json<StartPhaseBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();

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

    let phase = match plan.phases.iter().find(|p| p.number == phase_number) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Phase not found"})),
            )
                .into_response();
        }
    };

    // Merge in manual statuses from the DB and compute the set of tasks
    // already completed (for dep gating).
    let (status_map, mut done_set) = {
        let db = state.db.lock().unwrap();
        let mut statuses: HashMap<String, String> = HashMap::new();
        if let Ok(mut stmt) =
            db.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            && let Ok(rows) = stmt.query_map(params![plan_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
        {
            for row in rows.flatten() {
                statuses.insert(row.0, row.1);
            }
        }
        let done = crate::db::completed_task_numbers(&db, &plan_name);
        (statuses, done)
    };

    // Ready = status is pending/failed AND all dependencies satisfied.
    // Skip anything already running, completed, or skipped.
    let ready: Vec<&plan_parser::PlanTask> = phase
        .tasks
        .iter()
        .filter(|t| {
            let status = status_map
                .get(&t.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            if !(status == "pending" || status == "failed") {
                return false;
            }
            t.dependencies.iter().all(|d| done_set.contains(d))
        })
        .collect();

    if ready.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "started": [],
                "skipped": [],
                "reason": "no ready tasks in phase",
            })),
        )
            .into_response();
    }

    // Budget check — if exhausted, refuse. Otherwise pass headroom to every
    // spawned agent; they self-terminate once they've consumed it.
    let remaining_budget: Option<f64> = {
        let db = state.db.lock().unwrap();
        match plan_remaining_budget(&db, &plan_name) {
            Ok(b) => b,
            Err((spent, max)) => {
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(serde_json::json!({
                        "error": "budget_exceeded",
                        "message": format!("Plan budget of ${max:.2} exhausted (spent ${spent:.2})"),
                        "spentUsd": spent,
                        "maxBudgetUsd": max,
                    })),
                )
                    .into_response();
            }
        }
    };

    let home = dirs::home_dir().unwrap();
    let work_dir = body.cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    });

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    let port = state.config_port();
    let mut started = Vec::new();

    for task in ready {
        let cross_ctx = crate::agents::build_cross_plan_context(
            &state.db,
            &state.plans_dir,
            &plan,
            &task.number,
        );
        let prompt =
            crate::agents::build_task_prompt(&plan, phase, task, false, port, cross_ctx.as_deref());
        let branch_name = format!("orchestrai/{}/{}", plan_name, task.number);

        let agent_id = pty_agent::start_pty_agent(
            &state.registry,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: &work_dir,
                plan_name: Some(&plan_name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd: remaining_budget,
            },
        )
        .await;

        // Mark in_progress so subsequent clicks don't re-spawn.
        {
            let db = state.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                 VALUES (?1, ?2, 'in_progress', datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = 'in_progress', updated_at = datetime('now')",
                params![plan_name, task.number],
            )
            .ok();
        }
        crate::ws::broadcast_event(
            &state.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": plan_name,
                "task_number": task.number,
                "status": "in_progress",
            }),
        );

        // Track as "done" for this run so later tasks in the same click
        // whose only unmet dep was this one aren't treated as ready mid-loop.
        // (They won't be — we already filtered up front — but we keep the
        // invariant just in case the filter is relaxed later.)
        done_set.insert(task.number.clone());

        started.push(serde_json::json!({
            "taskId": task.number,
            "title": task.title,
            "agentId": agent_id,
            "branch": branch_name,
        }));
    }

    Json(serde_json::json!({
        "planName": plan_name,
        "phaseNumber": phase_number,
        "started": started,
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
    template_id: Option<String>,
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

    let template_section = body
        .template_id
        .as_deref()
        .and_then(templates::find)
        .map(|t| {
            format!(
                "Template: {name}\n\
                 {skeleton}\n\n\
                 Adapt the skeleton to the specifics in the request above — \
                 rename phases, add or drop tasks, and change details to fit.\n\n",
                name = t.name,
                skeleton = t.skeleton,
            )
        })
        .unwrap_or_default();

    let prompt = format!(
        "You are creating an implementation plan for a project.\n\n\
         Working directory: {folder}\n\n\
         Request:\n{description}\n\n\
         {template_section}\
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
    let agent_id = pty_agent::start_pty_agent(
        &state.registry,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &resolved,
            plan_name: None,
            task_id: None,
            effort,
            branch: None,
            is_continue: false,
            max_budget_usd: None,
        },
    )
    .await;

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
                        cost_usd: None,
                        ci: None,
                    })
                    .collect(),
            })
            .collect(),
        total_cost_usd: None,
        max_budget_usd: None,
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
