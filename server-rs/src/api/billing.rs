//! API endpoints for per-org usage tracking, budget management, and kill switch.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::Deserialize;

use crate::auth::AuthUser;
use crate::auth::orgs::{ROLE_ADMIN, ROLE_OWNER};
use crate::saas::billing;
use crate::state::AppState;

// ── Helpers ────────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Resolve org_id from slug and verify the caller is a member. Returns
/// `(org_id, caller_role)` on success.
fn resolve_org(
    state: &AppState,
    user: &AuthUser,
    slug: &str,
) -> Result<(String, String), Box<Response>> {
    let conn = state.db.lock().unwrap();
    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .ok();
    let org_id = match org_id {
        Some(id) => id,
        None => return Err(Box::new(err(StatusCode::NOT_FOUND, "org_not_found"))),
    };
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |row| row.get(0),
        )
        .ok();
    match role {
        Some(r) => Ok((org_id, r)),
        None => Err(Box::new(err(StatusCode::FORBIDDEN, "not_a_member"))),
    }
}

fn require_admin(role: &str) -> Result<(), Box<Response>> {
    if role == ROLE_OWNER || role == ROLE_ADMIN {
        Ok(())
    } else {
        Err(Box::new(err(StatusCode::FORBIDDEN, "admin_required")))
    }
}

// ── GET /api/orgs/:slug/usage ──────────────────────────────────────────────

/// Returns org usage summary + per-user cost breakdown for the current period.
pub async fn get_usage(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let (org_id, _role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let conn = state.db.lock().unwrap();
    let summary = billing::org_usage_summary(&conn, &org_id);
    let period = billing::current_period_key();
    let users = billing::user_costs_for_period(&conn, &org_id, &period);
    Json(serde_json::json!({
        "summary": summary,
        "users": users,
    }))
    .into_response()
}

// ── GET /api/orgs/:slug/budget ─────────────────────────────────────────────

pub async fn get_budget(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let (org_id, _role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let conn = state.db.lock().unwrap();
    let budget = billing::get_org_budget(&conn, &org_id);
    let killed = billing::is_kill_switch_active(&conn, &org_id);
    Json(serde_json::json!({
        "budget": budget,
        "killSwitchActive": killed,
    }))
    .into_response()
}

// ── PUT /api/orgs/:slug/budget ─────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBudgetBody {
    pub max_budget_usd: Option<f64>,
}

pub async fn set_budget(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<SetBudgetBody>,
) -> Response {
    let (org_id, role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = require_admin(&role) {
        return *r;
    }
    let conn = state.db.lock().unwrap();
    match body.max_budget_usd {
        Some(max) if max > 0.0 => billing::set_org_budget(&conn, &org_id, max),
        _ => billing::delete_org_budget(&conn, &org_id),
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

// ── PUT /api/orgs/:slug/kill-switch ────────────────────────────────────────

#[derive(Deserialize)]
pub struct KillSwitchBody {
    pub active: bool,
    pub reason: Option<String>,
}

pub async fn toggle_kill_switch(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<KillSwitchBody>,
) -> Response {
    let (org_id, role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = require_admin(&role) {
        return *r;
    }
    let conn = state.db.lock().unwrap();
    billing::set_kill_switch(&conn, &org_id, body.active, body.reason.as_deref());
    Json(serde_json::json!({"ok": true, "active": body.active})).into_response()
}

// ── GET /api/orgs/:slug/user-quotas ────────────────────────────────────────

pub async fn list_user_quotas(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let (org_id, _role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let conn = state.db.lock().unwrap();
    let quotas = billing::list_user_quotas(&conn, &org_id);
    Json(quotas).into_response()
}

// ── PUT /api/orgs/:slug/user-quotas/:user_id ──────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetQuotaBody {
    pub max_budget_usd: Option<f64>,
}

pub async fn set_user_quota(
    State(state): State<AppState>,
    user: AuthUser,
    Path((slug, target_user_id)): Path<(String, String)>,
    Json(body): Json<SetQuotaBody>,
) -> Response {
    let (org_id, role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = require_admin(&role) {
        return *r;
    }
    let conn = state.db.lock().unwrap();
    match body.max_budget_usd {
        Some(max) if max > 0.0 => billing::set_user_quota(&conn, &org_id, &target_user_id, max),
        _ => billing::delete_user_quota(&conn, &org_id, &target_user_id),
    }
    Json(serde_json::json!({"ok": true})).into_response()
}
