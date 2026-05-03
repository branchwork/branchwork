use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::OptionalAuthUser;
use crate::config::Effort;
use crate::persisted_settings::PersistedSettings;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::WireMessage;
use crate::saas::runner_rpc::{RunnerRpcError, runner_request};
use crate::saas::runner_ws::RunnerResponse;
use crate::state::AppState;

// ── GET /api/settings ────────────────────────────────────────────────────────

pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    Json(snapshot(&state))
}

// ── PUT /api/settings ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SettingsBody {
    effort: Option<String>,
    skip_permissions: Option<bool>,
    /// Treated as set-or-clear: an explicit `null` clears the URL, an empty
    /// string also clears it, and a non-empty string replaces it. Use
    /// `serde_json::Value` so we can distinguish missing vs. null.
    #[serde(default)]
    webhook_url: serde_json::Value,
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

    if let Some(skip) = body.skip_permissions {
        state
            .registry
            .skip_permissions
            .store(skip, std::sync::atomic::Ordering::Relaxed);
    }

    // webhook_url: missing → no change, null/"" → clear, string → set.
    match &body.webhook_url {
        serde_json::Value::Null => {
            *state.registry.webhook_url.write().unwrap() = None;
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            *state.registry.webhook_url.write().unwrap() = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        _ => { /* missing or wrong type → leave alone */ }
    }

    // Persist the *current* in-memory state (not just the diff) so the file
    // always reflects what the server is using right now.
    let snap = snapshot_for_persist(&state);
    if let Err(e) = snap.save(&state.settings_path) {
        eprintln!(
            "[settings] failed to persist to {}: {e}",
            state.settings_path.display()
        );
    }

    Json(snapshot(&state)).into_response()
}

fn snapshot(state: &AppState) -> serde_json::Value {
    let effort = *state.effort.lock().unwrap();
    let skip_permissions = state
        .registry
        .skip_permissions
        .load(std::sync::atomic::Ordering::Relaxed);
    let webhook_url = state.registry.webhook_url.read().unwrap().clone();
    serde_json::json!({
        "effort": effort,
        "skip_permissions": skip_permissions,
        "webhook_url": webhook_url,
    })
}

fn snapshot_for_persist(state: &AppState) -> PersistedSettings {
    PersistedSettings {
        effort: Some(*state.effort.lock().unwrap()),
        skip_permissions: Some(
            state
                .registry
                .skip_permissions
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        webhook_url: state.registry.webhook_url.read().unwrap().clone(),
    }
}

// ── GET /api/folders ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FolderEntry {
    name: String,
    path: String,
}

pub async fn list_folders(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    if !org_has_runner(&state.db, auth.org_id()) {
        return Json(local_home_folders()).into_response();
    }

    let req_id = Uuid::new_v4().to_string();
    let message = WireMessage::ListFolders { req_id };
    match runner_request(&state, auth.org_id(), message, Duration::from_secs(8)).await {
        Ok(RunnerResponse::FoldersListed(entries)) => Json(entries).into_response(),
        Ok(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "unexpected_runner_response" })),
        )
            .into_response(),
        Err(RunnerRpcError::NoConnectedRunner) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no_runner_connected" })),
        )
            .into_response(),
        Err(RunnerRpcError::Timeout | RunnerRpcError::RunnerDisconnected) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "runner_unavailable" })),
        )
            .into_response(),
        Err(RunnerRpcError::InvalidRequest | RunnerRpcError::SendFailed) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "runner_request_failed" })),
        )
            .into_response(),
    }
}

fn local_home_folders() -> Vec<FolderEntry> {
    let home = dirs::home_dir().unwrap_or_default();
    match std::fs::read_dir(&home) {
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
    }
}
