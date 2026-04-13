use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    if let Ok(mut stmt) = db.prepare("SELECT plan_name, project FROM plan_project") {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                overrides.insert(row.0, row.1);
            }
        }
    }

    let entries: Vec<PlanListEntry> = summaries
        .into_iter()
        .map(|s| {
            // Parse the full plan to merge statuses and get accurate done count
            let plan_path = state.plans_dir.join(format!("{}.md", s.name));
            let done_count = if let Ok(parsed) = plan_parser::parse_plan_file(&plan_path) {
                // Load statuses for this plan
                let mut status_map: HashMap<String, String> = HashMap::new();
                if let Ok(mut stmt) = db.prepare(
                    "SELECT task_number, status FROM task_status WHERE plan_name = ?",
                ) {
                    if let Ok(rows) = stmt.query_map(params![s.name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    }) {
                        for row in rows.flatten() {
                            status_map.insert(row.0, row.1);
                        }
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
            } else {
                0
            };

            let project = overrides
                .get(&s.name)
                .cloned()
                .or(s.project);

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
    let plan_path = state.plans_dir.join(format!("{name}.md"));
    let mut plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Plan not found"}))).into_response(),
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
    let valid = ["pending", "in_progress", "completed", "failed", "skipped", "checking"];
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
    let plan_path = state.plans_dir.join(format!("{name}.md"));
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response()
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response()
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
    {
        if let Ok(rows) = stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                manual.insert(row.0, row.1);
            }
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
            if let Some(s) = manual.get(&task.number).filter(|s| s.as_str() != "pending") {
                let s = s.clone();
                *summary.entry(s.clone()).or_insert(0) += 1;
                results.push(serde_json::json!({
                    "taskNumber": task.number,
                    "title": task.title,
                    "status": s,
                    "reason": "manual (kept)",
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
    let mut synced = 0;

    for s in &summaries {
        let project = match s.project.as_deref() {
            Some(p) => p,
            None => continue,
        };
        let project_dir = home.join(project);
        if !project_dir.is_dir() {
            continue;
        }

        let plan_path = state.plans_dir.join(format!("{}.md", s.name));
        let plan = match plan_parser::parse_plan_file(&plan_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Load existing statuses
        let mut manual: HashMap<String, String> = HashMap::new();
        if let Ok(mut stmt) =
            db.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
        {
            if let Ok(rows) = stmt.query_map(params![s.name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    manual.insert(row.0, row.1);
                }
            }
        }

        for phase in &plan.phases {
            for task in &phase.tasks {
                let existing = manual.get(&task.number).cloned();
                if existing.as_deref().is_some_and(|s| s != "pending") {
                    let s = existing.unwrap();
                    *totals.entry(s.as_str().to_string()).or_insert(0) += 1;
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
        synced += 1;
    }

    Json(serde_json::json!({
        "synced": synced,
        "completed": totals.get("completed").unwrap_or(&0),
        "in_progress": totals.get("in_progress").unwrap_or(&0),
        "pending": totals.get("pending").unwrap_or(&0),
    }))
}
