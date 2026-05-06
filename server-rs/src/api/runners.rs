//! Per-runner config overrides (`effort`, `skip_permissions`).
//!
//! Each runner can override the server-wide AdminPage settings: a beefy
//! desktop runner may take `effort=max` while a laptop runner stays on
//! `effort=high` to save battery; one runner might be sandboxed and run
//! safely with `skip_permissions=true` while another is the user's daily
//! driver where they want per-tool approval.
//!
//! ## Endpoints
//!
//! - `GET /api/runners/{id}/config` returns the *effective* config (the
//!   resolved values the dispatcher would ship in `StartAgent`) plus the
//!   raw override row so the UI can distinguish "inherits server default"
//!   from "explicit override that happens to equal the default".
//! - `PUT /api/runners/{id}/config` accepts `{ effort?, skip_permissions? }`.
//!   Each field is `Option<Option<_>>`-shaped via `serde_json::Value`:
//!   missing → no change, explicit `null` → clear the override, value →
//!   set the override.
//!
//! ## Spawn-time resolution
//!
//! `agents::spawn_ops::start_agent_via_runner` resolves
//! `per-runner override → server default` and ships the resolved values in
//! the `StartAgent` envelope. The runner does not re-resolve.

use axum::{Json, extract::Path, extract::State, http::StatusCode, response::IntoResponse};
use rusqlite::params;
use serde::Deserialize;

use crate::audit;
use crate::auth::AuthUser;
use crate::config::Effort;
use crate::db::{RunnerConfig, runner_config, set_runner_config};
use crate::state::AppState;

/// Body for `PUT /api/runners/{id}/config`. Each field uses
/// [`serde_json::Value`] to distinguish *missing* (no change), *null*
/// (clear the override), and a typed value (set the override).
#[derive(Deserialize)]
pub struct RunnerConfigBody {
    #[serde(default)]
    pub effort: serde_json::Value,
    #[serde(default)]
    pub skip_permissions: serde_json::Value,
}

/// `GET /api/runners/{id}/config` — effective config (override OR server default).
///
/// Response shape:
/// ```json
/// {
///   "runnerId": "...",
///   "effort": "high",
///   "skipPermissions": false,
///   "override": {
///     "effort": null,           // null = inherit
///     "skipPermissions": false  // explicit override
///   }
/// }
/// ```
pub async fn get_runner_config(
    State(state): State<AppState>,
    Path(runner_id): Path<String>,
    user: AuthUser,
) -> impl IntoResponse {
    if !runner_belongs_to_org(&state, &runner_id, &user.org_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "runner_not_found" })),
        )
            .into_response();
    }

    let cfg = runner_config(&state.db, &runner_id);
    let server_effort = state.effort.lock().unwrap().to_string();
    let server_skip = state
        .registry
        .skip_permissions
        .load(std::sync::atomic::Ordering::Relaxed);

    let effective_effort = cfg.effort.clone().unwrap_or(server_effort);
    let effective_skip = cfg.skip_permissions.unwrap_or(server_skip);

    Json(serde_json::json!({
        "runnerId": runner_id,
        "effort": effective_effort,
        "skipPermissions": effective_skip,
        "override": {
            "effort": cfg.effort,
            "skipPermissions": cfg.skip_permissions,
        }
    }))
    .into_response()
}

/// `PUT /api/runners/{id}/config` — set or clear per-runner overrides.
///
/// Per-field semantics on the wire:
/// - missing key: leave the existing override untouched
/// - `null`: clear the override (back to inherit)
/// - typed value: replace the override
pub async fn put_runner_config(
    State(state): State<AppState>,
    Path(runner_id): Path<String>,
    user: AuthUser,
    Json(body): Json<RunnerConfigBody>,
) -> impl IntoResponse {
    if !runner_belongs_to_org(&state, &runner_id, &user.org_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "runner_not_found" })),
        )
            .into_response();
    }

    let mut cfg = runner_config(&state.db, &runner_id);

    // ── effort ──────────────────────────────────────────────────────────
    match &body.effort {
        serde_json::Value::Null => {
            cfg.effort = None;
        }
        serde_json::Value::String(s) => {
            // Validate against the same Effort enum the AdminPage uses; an
            // unknown string is a 400 (matches `put_settings`).
            let parsed: Result<Effort, _> = match s.as_str() {
                "low" => Ok(Effort::Low),
                "medium" => Ok(Effort::Medium),
                "high" => Ok(Effort::High),
                "max" => Ok(Effort::Max),
                _ => Err(()),
            };
            match parsed {
                Ok(_) => cfg.effort = Some(s.clone()),
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
        // Missing or wrong-type: leave alone. (The dashboard always sends
        // either null or a string; a wrong type from a hand-rolled curl
        // call falls through silently rather than 500ing.)
        _ => {}
    }

    // ── skip_permissions ────────────────────────────────────────────────
    match &body.skip_permissions {
        serde_json::Value::Null => {
            cfg.skip_permissions = None;
        }
        serde_json::Value::Bool(b) => {
            cfg.skip_permissions = Some(*b);
        }
        _ => {}
    }

    set_runner_config(&state.db, &runner_id, &user.org_id, &cfg);

    // Audit the change. Resource type is the existing CONFIG bucket — there
    // is no per-runner resource type and adding one would require touching
    // every existing audit-log filter dropdown.
    let diff = serde_json::json!({
        "runner_id": runner_id,
        "effort": cfg.effort,
        "skip_permissions": cfg.skip_permissions,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(&user.id),
            Some(&user.email),
            audit::actions::CONFIG_RUNNER_OVERRIDE,
            audit::resources::CONFIG,
            Some(&runner_id),
            Some(&diff),
        );
    }

    // Return the same shape as GET so the client can update its in-memory
    // store from the PUT response without a follow-up GET.
    let server_effort = state.effort.lock().unwrap().to_string();
    let server_skip = state
        .registry
        .skip_permissions
        .load(std::sync::atomic::Ordering::Relaxed);
    let effective_effort = cfg.effort.clone().unwrap_or(server_effort);
    let effective_skip = cfg.skip_permissions.unwrap_or(server_skip);

    Json(serde_json::json!({
        "runnerId": runner_id,
        "effort": effective_effort,
        "skipPermissions": effective_skip,
        "override": {
            "effort": cfg.effort,
            "skipPermissions": cfg.skip_permissions,
        }
    }))
    .into_response()
}

fn runner_belongs_to_org(state: &AppState, runner_id: &str, org_id: &str) -> bool {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT 1 FROM runners WHERE id = ?1 AND org_id = ?2",
        params![runner_id, org_id],
        |_row| Ok(()),
    )
    .is_ok()
}

/// Resolve the values the SaaS dispatcher should ship in `StartAgent`:
/// per-runner override (if set) → caller-supplied default. Pure function so
/// it can be unit-tested without a live `AppState`.
pub fn resolve_for_dispatch(
    cfg: &RunnerConfig,
    default_effort: &str,
    default_skip: bool,
) -> (String, bool) {
    let effort = cfg
        .effort
        .clone()
        .unwrap_or_else(|| default_effort.to_string());
    let skip = cfg.skip_permissions.unwrap_or(default_skip);
    (effort, skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_effort_wins_when_set() {
        let cfg = RunnerConfig {
            effort: Some("max".into()),
            skip_permissions: None,
        };
        let (effort, skip) = resolve_for_dispatch(&cfg, "medium", true);
        assert_eq!(effort, "max");
        assert!(skip, "skip should fall through to the server default");
    }

    #[test]
    fn no_override_falls_through_to_default() {
        let cfg = RunnerConfig::default();
        let (effort, skip) = resolve_for_dispatch(&cfg, "high", false);
        assert_eq!(effort, "high");
        assert!(!skip);
    }

    #[test]
    fn skip_override_false_wins_over_server_true() {
        // The acceptance criterion from the task spec — user opts a
        // sandbox-less laptop runner *out* of `--dangerously-skip-
        // permissions` even when the server-wide default is `true`.
        let cfg = RunnerConfig {
            effort: None,
            skip_permissions: Some(false),
        };
        let (_, skip) = resolve_for_dispatch(&cfg, "high", true);
        assert!(!skip);
    }

    #[test]
    fn skip_override_true_wins_over_server_false() {
        let cfg = RunnerConfig {
            effort: None,
            skip_permissions: Some(true),
        };
        let (_, skip) = resolve_for_dispatch(&cfg, "high", false);
        assert!(skip);
    }
}
