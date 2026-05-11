use std::time::Duration;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::OptionalAuthUser;
use crate::config::Effort;
use crate::persisted_settings::PersistedSettings;
use crate::plan_curate::DEFAULT_RETENTION_DAYS;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::WireMessage;
use crate::saas::runner_rpc::{RunnerRpcError, runner_request};
use crate::saas::runner_ws::RunnerResponse;
use crate::state::AppState;

/// Inclusive bounds enforced server-side on
/// `plan_archive_retention_days`. 0 collapses soft delete to hard
/// delete (snapshot is written, then purged on the next tick); 365
/// caps single-org retention at one year so the snapshot table does
/// not grow without bound.
pub const RETENTION_DAYS_MIN: i64 = 0;
pub const RETENTION_DAYS_MAX: i64 = 365;

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
    /// Days a soft-deleted plan's snapshot survives before the
    /// retention purger removes it. Validated server-side against
    /// `RETENTION_DAYS_MIN..=RETENTION_DAYS_MAX`; out-of-range
    /// values produce a 400. Missing means "no change" — the
    /// existing on-disk value (or the default) is preserved.
    plan_archive_retention_days: Option<i64>,
    /// Default agent driver to use when not explicitly specified.
    /// Falls back to "claude" if not set. Can be overridden per-task,
    /// per-phase, per-plan, etc.
    default_driver: Option<String>,
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

    // plan_archive_retention_days lives only on disk (read by
    // `plan_curate::snapshot_plan` per delete via PersistedSettings::load).
    // Range-check up front; out-of-range PUTs are 400 and never persist.
    if let Some(days) = body.plan_archive_retention_days
        && !(RETENTION_DAYS_MIN..=RETENTION_DAYS_MAX).contains(&days)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "plan_archive_retention_days must be between {RETENTION_DAYS_MIN} and {RETENTION_DAYS_MAX}",
                ),
            })),
        )
            .into_response();
    }

    // Persist the *current* in-memory state (not just the diff) so the file
    // always reflects what the server is using right now.
    let mut snap = snapshot_for_persist(&state);
    if let Some(days) = body.plan_archive_retention_days {
        snap.plan_archive_retention_days = Some(days);
    }
    if let Some(driver) = body.default_driver {
        snap.default_driver = if driver.trim().is_empty() {
            None
        } else {
            Some(driver.trim().to_string())
        };
    }
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
    // Read retention straight off disk — it's not cached on AppState
    // because the only consumer (`plan_curate::snapshot_plan`) is also
    // the cold path, and reading once per admin GET keeps the source
    // of truth single.
    let settings = PersistedSettings::load(&state.settings_path);
    let plan_archive_retention_days = settings
        .plan_archive_retention_days
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    let default_driver = settings
        .default_driver
        .unwrap_or_else(|| "claude".to_string());
    serde_json::json!({
        "effort": effort,
        "skip_permissions": skip_permissions,
        "webhook_url": webhook_url,
        "plan_archive_retention_days": plan_archive_retention_days,
        "default_driver": default_driver,
    })
}

fn snapshot_for_persist(state: &AppState) -> PersistedSettings {
    // Preserve any setting written by code paths that don't live on
    // AppState yet (e.g. `plan_archive_retention_days`, which lands
    // on the admin tab in plan-deletion 0.5). Loading-then-overwriting
    // is the forward-compatible pattern: this fn never clobbers a
    // field it doesn't know about.
    let mut existing = PersistedSettings::load(&state.settings_path);
    existing.effort = Some(*state.effort.lock().unwrap());
    existing.skip_permissions = Some(
        state
            .registry
            .skip_permissions
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    existing.webhook_url = state.registry.webhook_url.read().unwrap().clone();
    existing
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
