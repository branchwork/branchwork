use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::config::Effort;
use crate::state::AppState;

// ── GET /api/settings ────────────────────────────────────────────────────────

pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let effort = *state.effort.lock().unwrap();
    Json(serde_json::json!({ "effort": effort }))
}

// ── PUT /api/settings ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SettingsBody {
    effort: Option<String>,
}

pub async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<SettingsBody>,
) -> impl IntoResponse {
    if let Some(ref effort_str) = body.effort {
        let parsed: Result<Effort, _> = match effort_str.as_str() {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "max" => Ok(Effort::Max),
            _ => Err(()),
        };
        match parsed {
            Ok(e) => *state.effort.lock().unwrap() = e,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "effort must be one of: low, medium, high, max"
                    })),
                )
                    .into_response();
            }
        }
    }

    let effort = *state.effort.lock().unwrap();
    Json(serde_json::json!({ "effort": effort })).into_response()
}

// ── GET /api/folders ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FolderEntry {
    name: String,
    path: String,
}

pub async fn list_folders() -> impl IntoResponse {
    let home = dirs::home_dir().unwrap_or_default();
    let entries = match std::fs::read_dir(&home) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| FolderEntry {
                name: e.file_name().to_string_lossy().to_string(),
                path: e.path().to_string_lossy().to_string(),
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Json(entries)
}
