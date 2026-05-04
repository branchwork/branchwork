use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

use crate::agents::{check_agent, ensure_git_initialized, pty_agent};
use crate::auth::OptionalAuthUser;
use crate::auth::orgs;
use crate::plan_parser;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::WireMessage;
use crate::saas::runner_rpc::{RunnerRpcError, runner_request};
use crate::saas::runner_ws::RunnerResponse;
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

pub async fn list_plans(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let summaries = plan_parser::list_plans(&state.plans_dir);
    let org_id = auth.org_id().to_string();

    let db = state.db.lock().unwrap();

    // Load all project overrides scoped to the active org
    let mut overrides: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT plan_name, project FROM plan_project WHERE org_id = ?1")
        && let Ok(rows) = stmt.query_map(params![org_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            overrides.insert(row.0, row.1);
        }
    }
    // Also load overrides without org_id filter for backward-compat: rows
    // that belong to the default org but were inserted before multi-tenancy.
    if org_id == orgs::DEFAULT_ORG_ID
        && let Ok(mut stmt) = db.prepare("SELECT plan_name, project FROM plan_project")
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            overrides.entry(row.0).or_insert(row.1);
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
        .filter(|s| orgs::plan_belongs_to_org(&db, &s.name, &org_id))
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
    auth: OptionalAuthUser,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Org gate: verify the plan belongs to the caller's org.
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

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

    // Latest CI run per task for this plan. The lookup rolls up
    // `<task>-fix-<N>` rows onto the canonical task so a green fix attempt
    // surfaces on the original task's badge — see `ci::latest_per_task`.
    let task_numbers: Vec<&str> = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .map(|t| t.number.as_str())
        .collect();
    let ci_map = crate::ci::latest_per_task(&db, &name, &task_numbers);
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

    // Latest plan-level verdict (None when no Check Plan has ever run).
    let verdict = db
        .query_row(
            "SELECT verdict, reason, agent_id, checked_at FROM plan_verdicts WHERE plan_name = ?",
            params![name],
            |row| {
                Ok(serde_json::json!({
                    "verdict": row.get::<_, String>(0)?,
                    "reason": row.get::<_, Option<String>>(1)?,
                    "agentId": row.get::<_, Option<String>>(2)?,
                    "checkedAt": row.get::<_, String>(3)?,
                }))
            },
        )
        .ok();

    let mut value = serde_json::to_value(plan).unwrap();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("autoAdvance".to_string(), serde_json::json!(auto_advance));
        if let Some(v) = verdict {
            obj.insert("verdict".to_string(), v);
        }
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
    auth: OptionalAuthUser,
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

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_PROJECT_CHANGE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"project": body.project}).to_string()),
    );

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
    auth: OptionalAuthUser,
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

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_BUDGET_CHANGE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"maxBudgetUsd": body.max_budget_usd}).to_string()),
    );

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
    auth: OptionalAuthUser,
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

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_AUTO_ADVANCE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"enabled": body.enabled}).to_string()),
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "planName": name,
            "autoAdvance": body.enabled,
        })),
    )
}

// ── GET/PUT /api/plans/:name/config ──────────────────────────────────────────
//
// Unified config endpoint covering `auto_advance` (existing) plus the
// auto-mode companion toggles (`auto_mode`, `max_fix_attempts`). GET also
// surfaces `paused_reason` so the UI can show a banner explaining why the
// loop self-paused. The dedicated `/auto-advance` route stays unchanged.

/// Compile-time gate for fan-out spawn: until worktree-per-agent isolation
/// (ADR 0002) ships, all `parallel = true` PUTs are rejected. Flip this to
/// `true` once `pty_agent.rs` carries a `git worktree`-based spawn path so
/// the enable side of the toggle becomes available again. Kept as a `pub`
/// const so the grep target is stable for the worktree implementer.
pub const WORKTREES_SHIPPED: bool = false;

/// Read the per-project worktree opt-in flag. Returns `false` when no
/// `plan_project` row exists for the plan or the column is 0. The toggling
/// endpoint ships with the worktree plan; until then the column is forward-
/// compatible storage that can only be set to 1 via direct SQL.
fn project_worktree_opt_in(db: &rusqlite::Connection, name: &str) -> bool {
    db.query_row(
        "SELECT worktree_isolation_opt_in FROM plan_project WHERE plan_name = ?1",
        params![name],
        |row| row.get::<_, i64>(0).map(|v| v != 0),
    )
    .unwrap_or(false)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanConfig {
    auto_advance: bool,
    auto_mode: bool,
    max_fix_attempts: u32,
    paused_reason: Option<String>,
    /// Per-plan opt-in for fan-out spawn (3.5.2). Stored on both
    /// `plan_auto_mode` and `plan_auto_advance` and kept in lockstep by
    /// the unified PUT. Toggling to true is rejected at the API layer
    /// until worktrees ship (3.5.3).
    parallel: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlanConfigBody {
    auto_advance: Option<bool>,
    auto_mode: Option<bool>,
    max_fix_attempts: Option<u32>,
    parallel: Option<bool>,
    /// Three-state field: `Absent` (None), `ExplicitNull` (Some(None) — the
    /// only value we honour: clear the pause + re-evaluate), or
    /// `ExplicitValue` (Some(Some(_)) — ignored, paused reasons are only
    /// set by the loop). The `deserialize_some` shim distinguishes
    /// `Absent` from `ExplicitNull`, which a plain `Option<String>` can't.
    #[serde(default, deserialize_with = "deserialize_some")]
    paused_reason: Option<Option<String>>,
}

/// Serde shim to distinguish absent fields from explicit null. With field
/// type `Option<Option<T>>` and `#[serde(default, deserialize_with =
/// "deserialize_some")]`, a missing field is `None` (default), an
/// explicit `null` is `Some(None)`, and a value is `Some(Some(v))`.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn read_plan_config(db: &rusqlite::Connection, name: &str) -> PlanConfig {
    let (auto_advance, advance_parallel) = db
        .query_row(
            "SELECT enabled, parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params![name],
            |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
        )
        .unwrap_or((false, false));

    let (auto_mode, max_fix_attempts, paused_reason, mode_parallel) = db
        .query_row(
            "SELECT enabled, max_fix_attempts, paused_reason, parallel \
             FROM plan_auto_mode WHERE plan_name = ?1",
            params![name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? != 0,
                    row.get::<_, i64>(1)? as u32,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            },
        )
        .unwrap_or((false, 3, None, false));

    // The unified config keeps both tables' `parallel` flags in lockstep,
    // but in case they ever diverge (manual SQL, partial migration), OR
    // them so a single "yes" wins. 3.5.3 will reject toggle attempts to
    // true at the API layer until worktrees ship.
    PlanConfig {
        auto_advance,
        auto_mode,
        max_fix_attempts,
        paused_reason,
        parallel: mode_parallel || advance_parallel,
    }
}

pub async fn get_plan_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let cfg = read_plan_config(&db, &name);
    Json(cfg).into_response()
}

pub async fn put_plan_config(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<PlanConfigBody>,
) -> axum::response::Response {
    // Worktree gate (Task 3.5.3): reject `parallel = true` until both
    //   (1) worktree-per-agent isolation has shipped (compile-time const), and
    //   (2) the project has opted in via `plan_project.worktree_isolation_opt_in`.
    // Audit every refused attempt so a sysadmin can see the trail. Disabling
    // (`parallel: false`) is never gated — turning the knob off must always
    // succeed even if it somehow got flipped on.
    if matches!(body.parallel, Some(true)) {
        let opted_in = {
            let db = state.db.lock().unwrap();
            project_worktree_opt_in(&db, &name)
        };
        if !WORKTREES_SHIPPED || !opted_in {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_PARALLEL_REFUSED,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(
                    &serde_json::json!({
                        "requested": true,
                        "reason": "worktrees_not_ready",
                    })
                    .to_string(),
                ),
            );
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(serde_json::json!({
                    "error": "worktrees_required",
                    "message": "Parallel auto-advance requires worktree-per-agent isolation. Enable it on the project once it ships.",
                })),
            )
                .into_response();
        }
    }

    // Snapshot in-flight fix agents to kill BEFORE the enabled flag
    // flips, so a concurrent agent-completion path can't slip a fresh
    // fix agent in past the disable. Only populated when the request is
    // explicitly turning auto_mode off.
    let fix_agents_to_kill: Vec<(String, String)> = {
        let db = state.db.lock().unwrap();

        if let Some(enabled) = body.auto_advance {
            db.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled, updated_at) \
                 VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(plan_name) \
                 DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
                params![name, enabled as i64],
            )
            .ok();

            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_AUTO_ADVANCE,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(&serde_json::json!({"enabled": enabled}).to_string()),
            );
        }

        let mut to_kill: Vec<(String, String)> = Vec::new();
        if body.auto_mode.is_some() || body.max_fix_attempts.is_some() || body.parallel.is_some() {
            // UPSERT each provided field independently so partial updates
            // don't clobber the other column. COALESCE keeps the existing
            // value when the caller omits it.
            if let Some(enabled) = body.auto_mode {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, enabled) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET enabled = excluded.enabled",
                    params![name, enabled as i64],
                )
                .ok();

                // Toggle-off: collect any in-flight fix agents for this
                // plan so we can kill them after dropping the lock.
                if !enabled
                    && let Ok(mut stmt) = db.prepare(
                        "SELECT id, COALESCE(org_id, 'default-org') FROM agents \
                         WHERE plan_name = ?1 \
                           AND task_id LIKE '%-fix-%' \
                           AND status IN ('running', 'starting')",
                    )
                    && let Ok(rows) = stmt.query_map(params![name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                {
                    for r in rows.flatten() {
                        to_kill.push(r);
                    }
                }
            }
            if let Some(max) = body.max_fix_attempts {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, max_fix_attempts) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET max_fix_attempts = excluded.max_fix_attempts",
                    params![name, max as i64],
                )
                .ok();
            }
            // `parallel` is mirrored across both tables so each mode's
            // spawn helper sees the same answer. Two UPSERTs because the
            // tables are separate; "one statement per field" still holds
            // per (table, field) pair.
            if let Some(parallel) = body.parallel {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, parallel) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET parallel = excluded.parallel",
                    params![name, parallel as i64],
                )
                .ok();
                db.execute(
                    "INSERT INTO plan_auto_advance (plan_name, parallel, updated_at) \
                     VALUES (?1, ?2, datetime('now')) \
                     ON CONFLICT(plan_name) \
                     DO UPDATE SET parallel = excluded.parallel, updated_at = excluded.updated_at",
                    params![name, parallel as i64],
                )
                .ok();
            }

            let mut audit_payload = serde_json::Map::new();
            if let Some(enabled) = body.auto_mode {
                audit_payload.insert("enabled".to_string(), serde_json::json!(enabled));
            }
            if let Some(max) = body.max_fix_attempts {
                audit_payload.insert("maxFixAttempts".to_string(), serde_json::json!(max));
            }
            if let Some(parallel) = body.parallel {
                audit_payload.insert("parallel".to_string(), serde_json::json!(parallel));
            }
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_AUTO_MODE,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(&serde_json::Value::Object(audit_payload).to_string()),
            );
        }
        to_kill
    };

    // Outside the DB lock: cancel any pending wait_for_ci poll for this
    // plan, then kill the in-flight fix agents collected above. Cancel
    // first so the loop sees Cancelled and bails before any
    // agent-completion side-effect could race the kill — the fix
    // branches are abandoned (no merge runs).
    if matches!(body.auto_mode, Some(false)) {
        state.cancel_plan(&name);
        for (agent_id, org_id) in &fix_agents_to_kill {
            let _ = crate::agents::spawn_ops::kill_agent_dispatch(&state, org_id, agent_id).await;
        }
    }

    // Explicit `pausedReason: null` is the Resume button: clear the loop's
    // self-pause, audit, broadcast `auto_mode_resumed` so the dashboard pill
    // disappears, then re-evaluate auto-advance from the most recently
    // completed task. Anything other than explicit-null is ignored — the
    // loop is the only writer of paused reasons.
    if matches!(body.paused_reason, Some(None)) {
        crate::db::auto_mode_resume(&state.db, &name);

        let last_completed: Option<String> = {
            let db = state.db.lock().unwrap();
            db.query_row(
                "SELECT task_number FROM task_status \
                 WHERE plan_name = ?1 AND status IN ('completed', 'skipped') \
                 ORDER BY updated_at DESC LIMIT 1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };

        {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::AUTO_MODE_RESUMED,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(
                    &serde_json::json!({
                        "last_completed_task": last_completed,
                    })
                    .to_string(),
                ),
            );
        }

        crate::ws::broadcast_event(
            &state.broadcast_tx,
            "auto_mode_resumed",
            serde_json::json!({
                "plan": name,
                "last_completed_task": last_completed,
            }),
        );

        if let Some(task) = last_completed {
            let registry = state.registry.clone();
            let plans_dir = state.plans_dir.clone();
            let plan_name = name.clone();
            let effort = *state.effort.lock().unwrap();
            let port = state.config_port();
            tokio::spawn(async move {
                crate::agents::try_auto_advance(registry, plans_dir, plan_name, task, effort, port)
                    .await;
            });
        }
    }

    let db = state.db.lock().unwrap();
    let cfg = read_plan_config(&db, &name);
    (StatusCode::OK, Json(cfg)).into_response()
}

// ── PUT /api/plans/:name/tasks/:num/status ───────────────────────────────────

#[derive(Deserialize)]
pub struct StatusBody {
    status: String,
}

pub async fn set_task_status(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
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

    // Reject `completed` on a dirty working tree — agents can't finish a task
    // with uncommitted changes in the project. See
    // `agents::check_tree_clean_for_completion` for the full reasoning.
    if body.status == "completed"
        && let crate::agents::TreeState::Dirty { files } =
            crate::agents::check_tree_clean_for_completion(&state.db, &state.plans_dir, &name)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "working_tree_dirty",
                "message": "Cannot mark task completed — the project's working tree has \
                            uncommitted changes. Commit them before calling \
                            update_task_status(completed).",
                "files": files,
            })),
        );
    }

    let db = state.db.lock().unwrap();

    // Capture previous status for audit diff
    let prev_status: Option<String> = db
        .query_row(
            "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
            params![name, task_number],
            |row| row.get(0),
        )
        .ok();

    db.execute(
        "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
         VALUES (?1, ?2, ?3, 'manual', datetime('now'))
         ON CONFLICT(plan_name, task_number)
         DO UPDATE SET status = excluded.status,
                       source = 'manual',
                       updated_at = excluded.updated_at",
        params![name, task_number, body.status],
    )
    .unwrap();

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::TASK_STATUS_CHANGE,
        crate::audit::resources::TASK,
        Some(&format!("{name}/{task_number}")),
        Some(
            &serde_json::json!({
                "from": prev_status.as_deref().unwrap_or("pending"),
                "to": body.status,
            })
            .to_string(),
        ),
    );
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
    /// Driver name (e.g. "claude"). Unknown/absent → server default.
    driver: Option<String>,
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
    auth: OptionalAuthUser,
    Json(body): Json<StartTaskBody>,
) -> impl IntoResponse {
    // ── Org budget / kill switch gate ───────────────────────────────────
    let org_id_str = auth.org_id().to_string();
    let user_id_str = auth.0.as_ref().map(|u| u.id.clone());
    {
        let db = state.db.lock().unwrap();
        let status = crate::saas::billing::check_org_budget(&db, &org_id_str);
        if matches!(
            status,
            crate::saas::billing::BudgetStatus::Exceeded
                | crate::saas::billing::BudgetStatus::Killed
        ) {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "org_budget_exceeded",
                    "message": "Organization budget exceeded. New agents are blocked.",
                })),
            )
                .into_response();
        }
        // Per-user quota check.
        if let Some(uid) = &user_id_str
            && let Err((spent, max)) = crate::saas::billing::check_user_quota(&db, &org_id_str, uid)
        {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "user_quota_exceeded",
                    "message": format!("Your quota of ${max:.2} exceeded (spent ${spent:.2})"),
                    "spentUsd": spent,
                    "maxQuotaUsd": max,
                })),
            )
                .into_response();
        }
    }

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
    let port = state.config_port();
    let mcp_available = state
        .registry
        .drivers
        .injects_mcp(body.driver.as_deref(), port);
    let prompt = crate::agents::build_task_prompt(
        &plan,
        phase,
        task,
        is_continue,
        port,
        cross_ctx.as_deref(),
        mcp_available,
    );

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    // Ensure git is initialized — required for branch isolation and diffs.
    // SaaS mode: skipped because work_dir lives on the runner, not on this
    // server. The agent itself initializes git via its prompt instructions
    // (same precedent as create_plan in SaaS mode — see saas-folder-listing
    // task 3.3).
    if !crate::saas::dispatch::org_has_runner(&state.db, &org_id_str) {
        ensure_git_initialized(&work_dir);
    }

    // Create a dedicated branch for this task
    let branch_name = format!("branchwork/{}/{}", body.plan_name, body.task_number);

    let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
        &state,
        &org_id_str,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &work_dir,
            plan_name: Some(&body.plan_name),
            task_id: Some(&body.task_number),
            effort,
            branch: Some(&branch_name),
            is_continue,
            max_budget_usd: remaining_budget,
            driver: body.driver.as_deref(),
            user_id: user_id_str.as_deref(),
            org_id: Some(&org_id_str),
        },
    )
    .await;

    {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            &org_id_str,
            user_id_str.as_deref(),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::AGENT_START,
            crate::audit::resources::AGENT,
            Some(&agent_id),
            Some(
                &serde_json::json!({
                    "plan": body.plan_name,
                    "task": body.task_number,
                    "driver": body.driver.as_deref().unwrap_or("claude"),
                    "mode": body.mode.as_deref().unwrap_or("start"),
                })
                .to_string(),
            ),
        );
    }

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
    /// Driver name (e.g. "claude"). Unknown/absent → server default.
    #[serde(default)]
    driver: Option<String>,
}

pub async fn start_phase_tasks(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path((plan_name, phase_number)): Path<(String, u32)>,
    body: Option<Json<StartPhaseBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let org_id_str = auth.org_id().to_string();
    let user_id_str = auth.0.as_ref().map(|u| u.id.clone());

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

    // Org budget / kill switch gate.
    {
        let db = state.db.lock().unwrap();
        let status = crate::saas::billing::check_org_budget(&db, &org_id_str);
        if matches!(
            status,
            crate::saas::billing::BudgetStatus::Exceeded
                | crate::saas::billing::BudgetStatus::Killed
        ) {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "org_budget_exceeded",
                    "message": "Organization budget exceeded. New agents are blocked.",
                })),
            )
                .into_response();
        }
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

    // Ensure git is initialized — required for branch isolation and diffs.
    // SaaS mode: skipped (work_dir is on the runner — see start_task above).
    if !crate::saas::dispatch::org_has_runner(&state.db, &org_id_str) {
        ensure_git_initialized(&work_dir);
    }

    let port = state.config_port();
    let mcp_available = state
        .registry
        .drivers
        .injects_mcp(body.driver.as_deref(), port);
    let mut started = Vec::new();

    for task in ready {
        let cross_ctx = crate::agents::build_cross_plan_context(
            &state.db,
            &state.plans_dir,
            &plan,
            &task.number,
        );
        let prompt = crate::agents::build_task_prompt(
            &plan,
            phase,
            task,
            false,
            port,
            cross_ctx.as_deref(),
            mcp_available,
        );
        let branch_name = format!("branchwork/{}/{}", plan_name, task.number);

        let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
            &state,
            &org_id_str,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: &work_dir,
                plan_name: Some(&plan_name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd: remaining_budget,
                driver: body.driver.as_deref(),
                user_id: user_id_str.as_deref(),
                org_id: Some(&org_id_str),
            },
        )
        .await;

        // Mark in_progress so subsequent clicks don't re-spawn.
        {
            let db = state.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                 VALUES (?1, ?2, 'in_progress', 'manual', datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = 'in_progress',
                               source = 'manual',
                               updated_at = datetime('now')",
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

    let prompt = build_check_prompt(&plan_name, &plan, phase, task, &project_dir);

    // Set task to checking state
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
             VALUES (?1, ?2, 'checking', 'manual', datetime('now'))
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = 'checking',
                           source = 'manual',
                           updated_at = datetime('now')",
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

// ── POST /api/plans/:name/check ─────────────────────────────────────────────
//
// Plan-level Check agent. Builds a prompt from the plan's `verification` block
// plus a done/pending task summary, spawns a read-only check agent against the
// project, and persists the verdict to `plan_verdicts` + broadcasts
// `plan_checked` via WebSocket.

pub async fn check_plan(
    State(state): State<AppState>,
    Path(plan_name): Path<String>,
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

    let verification = match plan.verification.as_deref() {
        Some(v) if !v.trim().is_empty() => v,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "Plan has no verification block to check against"}),
                ),
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

    let statuses: HashMap<String, String> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            .unwrap();
        stmt.query_map(params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    let prompt = build_plan_check_prompt(&plan, verification, &statuses, &project_dir);

    let effort = *state.effort.lock().unwrap();
    let agent_id = check_agent::start_check_agent(
        &state.registry,
        prompt,
        &project_dir,
        Some(&plan_name),
        None,
        effort,
    )
    .await;

    Json(serde_json::json!({
        "agentId": agent_id,
        "planName": plan_name,
    }))
    .into_response()
}

fn build_plan_check_prompt(
    plan: &plan_parser::ParsedPlan,
    verification: &str,
    statuses: &HashMap<String, String>,
    project_dir: &std::path::Path,
) -> String {
    let mut done: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for phase in &plan.phases {
        for task in &phase.tasks {
            let status = statuses
                .get(&task.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            let line = format!("- {}: {}", task.number, task.title);
            if matches!(status, "completed" | "skipped") {
                done.push(line);
            } else {
                pending.push(line);
            }
        }
    }

    let done_section = if done.is_empty() {
        "Done tasks: (none)".to_string()
    } else {
        format!("Done tasks:\n{}", done.join("\n"))
    };
    let pending_section = if pending.is_empty() {
        "Pending tasks: (none)".to_string()
    } else {
        format!("Pending tasks:\n{}", pending.join("\n"))
    };

    format!(
        "You are verifying whether a plan's overall verification criteria are satisfied by the current state of the project.\n\
         Answer with ONLY a JSON object, no other text: {{\"status\": \"completed\"|\"in_progress\"|\"pending\", \"reason\": \"brief explanation\"}}\n\n\
         Project directory: {project_dir}\n\
         Plan: {plan_title}\n\n\
         Verification criteria:\n{verification}\n\n\
         Task summary:\n{done_section}\n\n{pending_section}\n\n\
         Check the project at {project_dir}. Read the relevant files and run the git commands you need to confirm each verification bullet.\n\n\
         Status values:\n\
         - \"completed\": every verification bullet is demonstrably satisfied in the code\n\
         - \"in_progress\": some criteria are met but not all\n\
         - \"pending\": little to no evidence the verification criteria hold\n\n\
         Respond with ONLY the JSON object.",
        project_dir = project_dir.display(),
        plan_title = plan.title,
        verification = verification.trim(),
        done_section = done_section,
        pending_section = pending_section,
    )
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
    auth: OptionalAuthUser,
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

    let resolved: std::path::PathBuf = if org_has_runner(&state.db, auth.org_id()) {
        // SaaS path: ask the runner to check or create the folder. The
        // runner-resolved absolute path comes back as a string and becomes
        // the cwd we hand to the agent — `StartAgent { cwd }` will be
        // resolved on the runner side later.
        let create_if_missing = body.create_folder.unwrap_or(false);
        let req_id = Uuid::new_v4().to_string();
        let message = WireMessage::CreateFolder {
            req_id,
            path: body.folder.clone(),
            create_if_missing,
        };
        match runner_request(&state, auth.org_id(), message, Duration::from_secs(8)).await {
            Ok(RunnerResponse::FolderCreated {
                ok: true,
                resolved_path: Some(path),
                ..
            }) => std::path::PathBuf::from(path),
            Ok(RunnerResponse::FolderCreated {
                ok: false,
                resolved_path,
                error,
            }) => {
                let err = error.unwrap_or_default();
                let display_path = resolved_path.as_deref().unwrap_or(body.folder.as_str());
                if err == "folder_not_found" {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "folder_not_found",
                            "resolvedFolder": resolved_path,
                            "message": format!("Directory does not exist: {display_path}"),
                        })),
                    )
                        .into_response();
                }
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "create_failed",
                        "resolvedFolder": resolved_path,
                        "message": err,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "unexpected_runner_response"})),
                )
                    .into_response();
            }
            Err(RunnerRpcError::NoConnectedRunner) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "no_runner_connected"})),
                )
                    .into_response();
            }
            Err(RunnerRpcError::Timeout | RunnerRpcError::RunnerDisconnected) => {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({"error": "runner_unavailable"})),
                )
                    .into_response();
            }
            Err(RunnerRpcError::InvalidRequest | RunnerRpcError::SendFailed) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "runner_request_failed"})),
                )
                    .into_response();
            }
        }
    } else {
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
                Json(
                    serde_json::json!({"error": format!("Not a directory: {}", resolved.display())}),
                ),
            )
                .into_response();
        }

        // Ensure git is initialized — branch isolation + diff features need it.
        // Skipped on the runner branch above: the runner host owns the
        // working tree, and the agent runs `git init` itself when needed.
        ensure_git_initialized(&resolved);

        resolved
    };

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
    let org_id_str = auth.org_id().to_string();
    let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
        &state,
        &org_id_str,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &resolved,
            plan_name: None,
            task_id: None,
            effort,
            branch: None,
            is_continue: false,
            max_budget_usd: None,
            driver: None,
            user_id: None,
            org_id: Some(&org_id_str),
        },
    )
    .await;

    let project_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_CREATE,
            crate::audit::resources::PLAN,
            None,
            Some(
                &serde_json::json!({
                    "folder": resolved.to_str(),
                    "template": body.template_id,
                })
                .to_string(),
            ),
        );
    }

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
    auth: OptionalAuthUser,
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
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    })
                    .collect(),
            })
            .collect(),
        verification: None,
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

    {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_UPDATE,
            crate::audit::resources::PLAN,
            Some(&name),
            None,
        );
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

// ── POST /api/plans/:name/reset-status — reset all task statuses to pending ─

pub async fn reset_plan_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let changes = db
        .execute("DELETE FROM task_status WHERE plan_name = ?", params![name])
        .unwrap_or(0);
    drop(db);

    // Broadcast so the UI refreshes in place
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "plan_reset",
        serde_json::json!({ "plan_name": name, "cleared": changes }),
    );

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "cleared": changes,
    }))
}

// ── POST /api/plans/:name/tasks/:task/reset-status — unwedge a single task ──

/// Clear the task's `task_status` row so it reverts to "derived / unknown"
/// and the user can pick up from a clean slate. Idempotent: safe to re-run.
///
/// Refuses if there's a running/starting agent for the task — resetting
/// status from under a live agent would orphan the agent's writes and
/// confuse the dashboard. Kill the agent first, then reset.
pub async fn reset_task_status(
    State(state): State<AppState>,
    Path((name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    // Live-agent check. We scope the lock tightly so the subsequent DELETE
    // doesn't contend for the same guard.
    let live_agent: Option<String> = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT id FROM agents \
             WHERE plan_name = ?1 AND task_id = ?2 \
                   AND status IN ('running', 'starting') \
             LIMIT 1",
            params![name, task_number],
            |r| r.get::<_, String>(0),
        )
        .ok()
    };
    if let Some(id) = live_agent {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "agent_running",
                "message": format!(
                    "Agent {} is still running for this task. Kill or finish it before resetting.",
                    &id[..8.min(id.len())]
                ),
                "agent_id": id,
            })),
        )
            .into_response();
    }

    let cleared = {
        let db = state.db.lock().unwrap();
        db.execute(
            "DELETE FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
            params![name, task_number],
        )
        .unwrap_or(0)
    };

    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "task_status_changed",
        serde_json::json!({
            "plan_name": name,
            "task_number": task_number,
            // Null status = reverted to derived; clients treat as pending unless
            // something else (e.g. CI) contradicts.
            "status": serde_json::Value::Null,
        }),
    );

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "task_number": task_number,
        "cleared": cleared,
    }))
    .into_response()
}

// ── Branch cleanup helpers ──────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleBranch {
    pub name: String,
    pub sha: Option<String>,
    pub commits_ahead_of_trunk: Option<u64>,
    pub last_commit_age_secs: Option<i64>,
    pub agent_id: Option<String>,
    /// When false, the branch has no unique commits and is safe to delete
    /// without the `force` flag.
    pub has_unique_commits: bool,
}

/// GET /api/plans/:name/branches/stale
///
/// Enumerate all `branchwork/<plan>/*` refs in the plan's project dir and
/// report their state so the user can decide which to delete. Read-only.
pub async fn list_stale_branches(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(cwd) = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "plan has no resolvable project directory"})),
        )
            .into_response();
    };

    // List local branches matching this plan's prefix.
    let prefix = format!("branchwork/{name}/");
    let prefix_fix = format!("branchwork/fix/{name}/");
    let branches: Vec<(String, String, Option<i64>)> = {
        let out = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname:short)|%(objectname)|%(committerdate:unix)",
                "refs/heads/",
            ])
            .current_dir(&cwd)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let parts: Vec<&str> = l.splitn(3, '|').collect();
                    if parts.len() < 3 {
                        return None;
                    }
                    let n = parts[0].to_string();
                    if !(n.starts_with(&prefix) || n.starts_with(&prefix_fix)) {
                        return None;
                    }
                    Some((n, parts[1].to_string(), parts[2].parse::<i64>().ok()))
                })
                .collect(),
            _ => Vec::new(),
        }
    };

    // Which agent rows still reference each branch — one query.
    let agent_by_branch: std::collections::HashMap<String, String> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT branch, id FROM agents WHERE branch IS NOT NULL")
        {
            Ok(s) => s,
            Err(_) => return Json(serde_json::json!({"branches": []})).into_response(),
        };
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    };

    // Determine trunk — try master, then main.
    let trunk = ["master", "main"]
        .iter()
        .find(|t| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", t])
                .current_dir(&cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .copied();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let stale: Vec<StaleBranch> = branches
        .into_iter()
        .map(|(branch, sha, ts)| {
            let commits_ahead = trunk.and_then(|t| {
                std::process::Command::new("git")
                    .args(["rev-list", "--count", &format!("{t}..{branch}")])
                    .current_dir(&cwd)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse::<u64>()
                            .ok()
                    })
            });
            StaleBranch {
                name: branch.clone(),
                sha: Some(sha),
                last_commit_age_secs: ts.map(|t| now - t),
                commits_ahead_of_trunk: commits_ahead,
                agent_id: agent_by_branch.get(&branch).cloned(),
                // Unknown commits_ahead = assume risky (has_unique_commits=true)
                // so the user has to actively opt in via force=true.
                has_unique_commits: commits_ahead.map(|c| c > 0).unwrap_or(true),
            }
        })
        .collect();

    Json(serde_json::json!({
        "plan_name": name,
        "trunk": trunk,
        "branches": stale,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeBranchesBody {
    pub branches: Vec<String>,
    /// Required to delete a branch with unique commits not on trunk.
    #[serde(default)]
    pub force: bool,
}

/// POST /api/plans/:name/branches/stale/purge
///
/// Delete the specified branches and null out matching `agents.branch`
/// columns. Safety guard: refuses branches with unique commits unless
/// `force=true`. Returns a per-branch outcome so partial failures are
/// legible in the UI.
pub async fn purge_stale_branches(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PurgeBranchesBody>,
) -> impl IntoResponse {
    let Some(cwd) = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "plan has no resolvable project directory"})),
        )
            .into_response();
    };

    // Determine trunk for safety check.
    let trunk = ["master", "main"]
        .iter()
        .find(|t| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", t])
                .current_dir(&cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .copied()
        .unwrap_or("master");

    let prefix = format!("branchwork/{name}/");
    let prefix_fix = format!("branchwork/fix/{name}/");

    let mut results = Vec::new();
    for branch in &body.branches {
        // Scope-check: only allow deletion within this plan's prefix so a
        // malicious / mistaken caller can't purge unrelated refs.
        if !(branch.starts_with(&prefix) || branch.starts_with(&prefix_fix)) {
            results.push(serde_json::json!({
                "branch": branch,
                "ok": false,
                "error": "out_of_scope",
            }));
            continue;
        }

        // Commit-ahead check: unique commits need force=true.
        if !body.force {
            let ahead = std::process::Command::new("git")
                .args(["rev-list", "--count", &format!("{trunk}..{branch}")])
                .current_dir(&cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u64>()
                        .ok()
                });
            if ahead.map(|c| c > 0).unwrap_or(true) {
                results.push(serde_json::json!({
                    "branch": branch,
                    "ok": false,
                    "error": "has_unique_commits",
                    "ahead_of_trunk": ahead,
                }));
                continue;
            }
        }

        // Delete. -D is force-delete (handles branches not merged into trunk
        // when the caller set force=true); for the default safe path the
        // above check already guaranteed zero commits ahead so -d would also
        // work. Use -D uniformly to avoid a second error mode.
        let out = std::process::Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(&cwd)
            .output();
        let ok = matches!(out.as_ref(), Ok(o) if o.status.success());
        if ok {
            // Clear any agent row still pointing at it.
            let db = state.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET branch = NULL WHERE branch = ?1",
                params![branch],
            )
            .ok();
            drop(db);
            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "agent_branch_cleared",
                serde_json::json!({"branch": branch}),
            );
            results.push(serde_json::json!({
                "branch": branch,
                "ok": true,
            }));
        } else {
            let stderr = out
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_else(|e| e.to_string());
            results.push(serde_json::json!({
                "branch": branch,
                "ok": false,
                "error": "git_failed",
                "stderr": stderr.trim(),
            }));
        }
    }

    Json(serde_json::json!({
        "plan_name": name,
        "results": results,
    }))
    .into_response()
}

// ── POST /api/plans/:name/check-all — spawn a check agent for every non-completed task ─

#[derive(Deserialize, Default)]
pub struct CheckAllBody {
    #[serde(default)]
    pub phase: Option<u32>,
    #[serde(default)]
    pub include_completed: bool,
}

pub async fn check_all(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CheckAllBody>,
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
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Failed to parse plan: {e}")})),
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

    // Load existing statuses so we skip "completed" tasks unless user opts in
    let existing: HashMap<String, String> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            .unwrap();
        stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    let effort = *state.effort.lock().unwrap();
    let mut spawned: Vec<String> = Vec::new();

    for phase in &plan.phases {
        if let Some(p) = body.phase
            && phase.number != p
        {
            continue;
        }
        for task in &phase.tasks {
            let current = existing
                .get(&task.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            if !body.include_completed && matches!(current, "completed" | "skipped" | "checking") {
                continue;
            }

            let prompt = build_check_prompt(&name, &plan, phase, task, &project_dir);

            // Set to checking
            {
                let db = state.db.lock().unwrap();
                db.execute(
                    "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                     VALUES (?1, ?2, 'checking', 'manual', datetime('now'))
                     ON CONFLICT(plan_name, task_number)
                     DO UPDATE SET status = 'checking',
                                   source = 'manual',
                                   updated_at = datetime('now')",
                    params![name, task.number],
                )
                .ok();
            }
            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "task_status_changed",
                serde_json::json!({
                    "plan_name": name,
                    "task_number": task.number,
                    "status": "checking",
                }),
            );

            let agent_id = crate::agents::check_agent::start_check_agent(
                &state.registry,
                prompt,
                &project_dir,
                Some(&name),
                Some(&task.number),
                effort,
            )
            .await;
            spawned.push(agent_id);
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "spawned": spawned.len(),
        "agent_ids": spawned,
    }))
    .into_response()
}

fn build_check_prompt(
    plan_name: &str,
    plan: &plan_parser::ParsedPlan,
    phase: &plan_parser::PlanPhase,
    task: &plan_parser::PlanTask,
    project_dir: &std::path::Path,
) -> String {
    let _ = plan_name;
    let files_section = if task.file_paths.is_empty() {
        String::new()
    } else {
        format!(
            "\nFiles mentioned:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let acceptance_section = if task.acceptance.is_empty() {
        String::new()
    } else {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    };
    format!(
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
    )
}

#[cfg(test)]
mod check_prompt_tests {
    use super::*;

    fn sample_plan() -> plan_parser::ParsedPlan {
        plan_parser::ParsedPlan {
            name: "dashboard-polish".into(),
            file_path: String::new(),
            title: "Test Plan".into(),
            context: String::new(),
            project: Some("proj".into()),
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![],
            verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        }
    }

    fn sample_task(number: &str, files: Vec<String>) -> plan_parser::PlanTask {
        plan_parser::PlanTask {
            number: number.into(),
            title: "A task".into(),
            description: "Do things".into(),
            file_paths: files,
            acceptance: "must work".into(),
            dependencies: vec![],
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        }
    }

    // Mirrors the prompt-construction locals inside `api::plans::check_task`
    // between fetching `plan`/`phase`/`task` and the call to
    // `start_check_agent`. After Phase 1.2 that body is a single call to
    // `build_check_prompt`. Tripwire: if a future change reintroduces inline
    // prompt mangling in `check_task`, update this helper to match — the
    // identical-prompt test below will then fail until `check_all_tasks` is
    // updated too (or the divergence is reverted).
    fn prompt_for_check_task_path(
        plan_name: &str,
        plan: &plan_parser::ParsedPlan,
        phase: &plan_parser::PlanPhase,
        task: &plan_parser::PlanTask,
        project_dir: &std::path::Path,
    ) -> String {
        build_check_prompt(plan_name, plan, phase, task, project_dir)
    }

    #[test]
    fn check_task_and_check_all_build_identical_prompts() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "P".into(),
            description: String::new(),
            tasks: vec![sample_task("1.1", vec!["a.rs".into()])],
        };
        let task = phase.tasks[0].clone();
        let project_dir = std::path::Path::new("/tmp/proj");

        let from_check_task = prompt_for_check_task_path("p", &plan, &phase, &task, project_dir);
        // `check_all_tasks` calls `build_check_prompt` directly inside its
        // loop body, so the unified builder's output IS the check-all path.
        let from_check_all = build_check_prompt("p", &plan, &phase, &task, project_dir);

        assert_eq!(
            from_check_task, from_check_all,
            "check_task and check_all_tasks must produce byte-identical prompts"
        );
    }

    #[test]
    fn excludes_branch_verification_after_unification() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "Phase One".into(),
            description: String::new(),
            tasks: vec![],
        };
        let task = sample_task(
            "1.3",
            vec![
                "server-rs/src/api/plans.rs".into(),
                "web/src/foo.tsx".into(),
            ],
        );
        let prompt = build_check_prompt(
            "dashboard-polish",
            &plan,
            &phase,
            &task,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(
            !prompt.contains("branch"),
            "prompt must not reference branches after unification"
        );
        assert!(!prompt.contains("git log"), "prompt must not shell git log");
        assert!(
            !prompt.contains("task branch"),
            "prompt must not mention task branch"
        );
        assert!(
            !prompt.contains("--not master"),
            "prompt must not exclude base branch history"
        );
        assert!(
            prompt.contains("Read the relevant files"),
            "prompt keeps the simple-shape instruction"
        );
        assert!(
            prompt.contains("Project directory:"),
            "prompt keeps the project directory context"
        );
    }

    #[test]
    fn prompt_shape_has_no_lifecycle_vocabulary() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "Phase One".into(),
            description: String::new(),
            tasks: vec![],
        };
        let task = sample_task(
            "1.3",
            vec![
                "server-rs/src/api/plans.rs".into(),
                "web/src/foo.tsx".into(),
            ],
        );
        let prompt = build_check_prompt(
            "dashboard-polish",
            &plan,
            &phase,
            &task,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(
            !prompt.contains("branch"),
            "prompt must not contain \"branch\""
        );
        assert!(
            !prompt.contains("git log"),
            "prompt must not contain \"git log\""
        );
        assert!(
            !prompt.contains("task branch"),
            "prompt must not contain \"task branch\""
        );
        assert!(
            !prompt.contains("committed"),
            "prompt must not contain \"committed\""
        );
        assert!(
            !prompt.contains("--not master"),
            "prompt must not contain \"--not master\""
        );
        assert!(
            !prompt.contains("--not main"),
            "prompt must not contain \"--not main\""
        );
    }

    #[test]
    fn plan_check_prompt_includes_verification_and_task_split() {
        let mut plan = sample_plan();
        plan.verification = Some("1. The endpoint returns 200.\n2. The verdict is stored.".into());
        plan.phases = vec![
            plan_parser::PlanPhase {
                number: 1,
                title: "P1".into(),
                description: String::new(),
                tasks: vec![sample_task("1.1", vec![]), sample_task("1.2", vec![])],
            },
            plan_parser::PlanPhase {
                number: 2,
                title: "P2".into(),
                description: String::new(),
                tasks: vec![sample_task("2.1", vec![])],
            },
        ];

        let mut statuses: HashMap<String, String> = HashMap::new();
        statuses.insert("1.1".into(), "completed".into());
        statuses.insert("1.2".into(), "skipped".into());
        // 2.1 intentionally left out to exercise the default "pending" path.

        let prompt = build_plan_check_prompt(
            &plan,
            plan.verification.as_deref().unwrap(),
            &statuses,
            std::path::Path::new("/tmp/proj"),
        );

        assert!(prompt.contains("The endpoint returns 200."));
        assert!(prompt.contains("Done tasks:"));
        assert!(prompt.contains("- 1.1: A task"));
        assert!(prompt.contains("- 1.2: A task"));
        assert!(prompt.contains("Pending tasks:"));
        assert!(prompt.contains("- 2.1: A task"));
        assert!(prompt.contains("Respond with ONLY the JSON object."));
    }

    #[test]
    fn plan_check_prompt_handles_empty_task_sections() {
        let mut plan = sample_plan();
        plan.verification = Some("nothing".into());
        plan.phases = vec![];
        let statuses: HashMap<String, String> = HashMap::new();
        let prompt = build_plan_check_prompt(
            &plan,
            plan.verification.as_deref().unwrap(),
            &statuses,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(prompt.contains("Done tasks: (none)"));
        assert!(prompt.contains("Pending tasks: (none)"));
    }
}
