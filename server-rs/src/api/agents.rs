use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::Deserialize;

use crate::state::AppState;

pub async fn list_agents(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT id, session_id, pid, parent_agent_id, plan_name, task_id, cwd, status, mode, prompt, started_at, finished_at, last_tool, last_activity_at, base_commit, branch, source_branch, cost_usd FROM agents ORDER BY started_at DESC LIMIT 50")
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "session_id": row.get::<_, String>(1)?,
                "pid": row.get::<_, Option<i64>>(2)?,
                "parent_agent_id": row.get::<_, Option<String>>(3)?,
                "plan_name": row.get::<_, Option<String>>(4)?,
                "task_id": row.get::<_, Option<String>>(5)?,
                "cwd": row.get::<_, String>(6)?,
                "status": row.get::<_, String>(7)?,
                "mode": row.get::<_, String>(8)?,
                "prompt": row.get::<_, Option<String>>(9)?,
                "started_at": row.get::<_, String>(10)?,
                "finished_at": row.get::<_, Option<String>>(11)?,
                "last_tool": row.get::<_, Option<String>>(12)?,
                "last_activity_at": row.get::<_, Option<String>>(13)?,
                "base_commit": row.get::<_, Option<String>>(14)?,
                "branch": row.get::<_, Option<String>>(15)?,
                "source_branch": row.get::<_, Option<String>>(16)?,
                "cost_usd": row.get::<_, Option<f64>>(17)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    200
}

pub async fn get_agent_output(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<OutputQuery>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT id, agent_id, message_type, content, timestamp FROM agent_output WHERE agent_id = ? ORDER BY id ASC LIMIT ? OFFSET ?")
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map(params![id, q.limit, q.offset], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "message_type": row.get::<_, String>(2)?,
                "content": row.get::<_, String>(3)?,
                "timestamp": row.get::<_, String>(4)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

pub async fn kill_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.registry.kill_agent(&id).await {
        (StatusCode::OK, Json(serde_json::json!({"ok": true})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
    }
}

pub async fn get_agent_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up the agent's cwd and base_commit
    let (cwd, base_commit): (String, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, base_commit FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let base = match base_commit {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": "No base commit recorded (not a git repo?)"
            }))
            .into_response();
        }
    };

    // Run git diff
    let diff_output = std::process::Command::new("git")
        .args(["diff", &base, "--no-color"])
        .current_dir(&cwd)
        .output();

    let diff = match diff_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": format!("git diff failed: {stderr}")
            }))
            .into_response();
        }
        Err(e) => {
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": format!("Failed to run git: {e}")
            }))
            .into_response();
        }
    };

    // Get list of changed files with stats
    let stat_output = std::process::Command::new("git")
        .args(["diff", &base, "--stat", "--no-color"])
        .current_dir(&cwd)
        .output();

    let stat = match stat_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        _ => String::new(),
    };

    // Get changed file names
    let name_output = std::process::Command::new("git")
        .args(["diff", &base, "--name-only"])
        .current_dir(&cwd)
        .output();

    let files: Vec<String> = match name_output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        _ => Vec::new(),
    };

    Json(serde_json::json!({
        "diff": diff,
        "stat": stat,
        "files": files,
        "base_commit": base,
    }))
    .into_response()
}

pub async fn merge_agent_branch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up agent details
    let (cwd, branch, source_branch): (String, Option<String>, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, source_branch FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let task_branch = match branch {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Agent has no task branch"})),
            )
                .into_response();
        }
    };

    let target = source_branch.unwrap_or_else(|| "main".to_string());

    // Checkout the target branch
    let checkout = std::process::Command::new("git")
        .args(["checkout", &target])
        .current_dir(&cwd)
        .output();

    match checkout {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to checkout {target}: {stderr}")
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to run git: {e}")
                })),
            )
                .into_response();
        }
        _ => {}
    }

    // Merge the task branch
    let merge = std::process::Command::new("git")
        .args(["merge", &task_branch, "--no-edit"])
        .current_dir(&cwd)
        .output();

    match merge {
        Ok(output) if output.status.success() => {
            // Delete the task branch after successful merge
            std::process::Command::new("git")
                .args(["branch", "-d", &task_branch])
                .current_dir(&cwd)
                .output()
                .ok();

            Json(serde_json::json!({
                "ok": true,
                "merged": task_branch,
                "into": target,
            }))
            .into_response()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Abort the failed merge
            std::process::Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(&cwd)
                .output()
                .ok();
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Merge conflict: {stderr}"),
                    "branch": task_branch,
                    "target": target,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to run git merge: {e}")
            })),
        )
            .into_response(),
    }
}

pub async fn discard_agent_branch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up agent details
    let (cwd, branch, source_branch): (String, Option<String>, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, source_branch FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let task_branch = match branch {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Agent has no task branch"})),
            )
                .into_response();
        }
    };

    let target = source_branch.unwrap_or_else(|| "main".to_string());

    // Checkout the target branch first
    let checkout = std::process::Command::new("git")
        .args(["checkout", &target])
        .current_dir(&cwd)
        .output();

    if let Ok(output) = checkout
        && !output.status.success()
    {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to checkout {target}: {stderr}")
            })),
        )
            .into_response();
    }

    // Force-delete the task branch
    let delete = std::process::Command::new("git")
        .args(["branch", "-D", &task_branch])
        .current_dir(&cwd)
        .output();

    match delete {
        Ok(output) if output.status.success() => Json(serde_json::json!({
            "ok": true,
            "deleted": task_branch,
            "switched_to": target,
        }))
        .into_response(),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to delete branch: {stderr}")
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to run git: {e}")
            })),
        )
            .into_response(),
    }
}

pub async fn get_events(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT * FROM hook_events ORDER BY id DESC LIMIT 50")
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "session_id": row.get::<_, String>(1)?,
                "hook_type": row.get::<_, String>(2)?,
                "tool_name": row.get::<_, Option<String>>(3)?,
                "tool_input": row.get::<_, Option<String>>(4)?,
                "timestamp": row.get::<_, String>(5)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}
