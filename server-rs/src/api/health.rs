//! `GET /api/health/state` — structured self-check for the dashboard's
//! sidebar footer indicator.
//!
//! Walks the database and reports counts that hint at a wedged state:
//! agents/task_status rows with unknown status values, tasks stuck in
//! `checking` with no live agent, time since the boot orphan sweep, and
//! how many CI runs are dismissed. Safe to hit anytime — pure read.
//!
//! The sidebar footer widget polls this and flips to amber when
//! `warnings` is non-empty. Each warning carries enough context
//! (category + count + plan list) for the UI to deep-link to the
//! existing per-task / per-plan recovery flows from Phase 1.

use axum::{Json, extract::State, response::IntoResponse};
use rusqlite::params;
use serde::Serialize;

use crate::state::AppState;

/// Status values an `agents.status` row can legally hold. Anything else
/// is a wedged state: a manual SQL edit, a half-finished migration, or
/// a future-version row left behind after a downgrade. Surfaced as the
/// `agents.unknown_status` warning so the user can reset the row.
const KNOWN_AGENT_STATUSES: &[&str] = &["starting", "running", "completed", "failed", "killed"];

/// Status values a `task_status.status` row can legally hold. Anything
/// else surfaces as `task_status.unknown_status`.
const KNOWN_TASK_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "checking",
    "completed",
    "failed",
    "skipped",
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthStateResponse {
    /// "ok" when warnings is empty, "warnings" otherwise.
    status: &'static str,
    /// ISO-8601 UTC timestamp of when this report was computed.
    checked_at: String,
    /// Process uptime — useful context for the sweep timestamps.
    uptime_seconds: u64,
    categories: Categories,
    warnings: Vec<Warning>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Categories {
    agents: AgentsCategory,
    task_status: TaskStatusCategory,
    branches: BranchesCategory,
    ci_runs: CiRunsCategory,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentsCategory {
    /// Counts keyed by status string. Includes unknown statuses so the
    /// UI can show the offending value rather than just a count.
    by_status: serde_json::Map<String, serde_json::Value>,
    unknown_status_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskStatusCategory {
    by_status: serde_json::Map<String, serde_json::Value>,
    unknown_status_count: u32,
    /// `task_status.status = 'checking'` rows with no running/starting
    /// agent attached. Boot sweep clears these (Task 2.1), but the
    /// counter surfaces any that drift in mid-session.
    stuck_checking_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BranchesCategory {
    /// ISO timestamp of the last `reconcile_orphaned_branches` run, or
    /// null if the server has never swept (brand-new install).
    last_sweep_at: Option<String>,
    /// Seconds elapsed since `last_sweep_at`. Null when `last_sweep_at`
    /// is null. Computed server-side from `datetime('now')` to avoid
    /// clock-skew between server and dashboard.
    seconds_since_last_sweep: Option<i64>,
    /// How many orphaned branch refs the last sweep cleared. 0 in the
    /// steady state.
    last_sweep_cleared_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CiRunsCategory {
    dismissed_count: u32,
    total_count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Warning {
    /// "agents", "taskStatus", "branches" — matches the keys in `categories`.
    category: &'static str,
    /// Stable identifier for the kind of warning, e.g. "unknown_status",
    /// "stuck_checking", "sweep_never_ran".
    kind: &'static str,
    count: u32,
    /// Human-readable copy for the indicator's expanded panel.
    message: String,
    /// Plan names directly affected. Lets the sidebar widget render
    /// per-plan deep-links to the recovery UI. Empty when the warning
    /// is global (e.g. boot sweep never ran).
    affected_plans: Vec<String>,
    /// Hint for the UI on which Phase-1 recovery flow to direct the
    /// user to. One of: "reset_task_status", "clean_stale_branches",
    /// "reset_ci_badge", or "boot_required".
    recovery: &'static str,
}

/// Pretty-print the offending status when surfacing a warning.
fn format_unknown_values(values: &[String]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let preview: Vec<String> = values.iter().take(3).map(|v| format!("\"{v}\"")).collect();
    if values.len() > 3 {
        format!("{} (+{} more)", preview.join(", "), values.len() - 3)
    } else {
        preview.join(", ")
    }
}

/// `GET /api/health/state`
///
/// Returns a structured self-check report. Always 200 — even with
/// warnings, the endpoint itself succeeded; the `status` field
/// distinguishes ok vs warnings.
pub async fn get_health_state(State(state): State<AppState>) -> impl IntoResponse {
    let mut warnings: Vec<Warning> = Vec::new();

    // ── agents ───────────────────────────────────────────────────────
    let (agent_by_status, unknown_agent_status_values, unknown_agent_plans): (
        serde_json::Map<String, serde_json::Value>,
        Vec<String>,
        Vec<String>,
    ) = {
        let conn = state.db.lock().unwrap();
        let mut by_status = serde_json::Map::new();
        let mut unknown_values = Vec::new();
        let mut unknown_plans = std::collections::BTreeSet::new();

        let mut stmt = match conn
            .prepare("SELECT status, plan_name, COUNT(*) FROM agents GROUP BY status, plan_name")
        {
            Ok(s) => s,
            Err(_) => {
                return Json(HealthStateResponse {
                    status: "ok",
                    checked_at: chrono::Utc::now().to_rfc3339(),
                    uptime_seconds: state.started_at.elapsed().as_secs(),
                    categories: empty_categories(),
                    warnings: vec![],
                })
                .into_response();
            }
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();

        for (status, plan_name, count) in rows {
            let bucket = by_status
                .entry(status.clone())
                .or_insert_with(|| serde_json::Value::Number(0.into()));
            if let Some(n) = bucket.as_i64() {
                *bucket = serde_json::Value::Number((n + count).into());
            }
            if !KNOWN_AGENT_STATUSES.contains(&status.as_str()) {
                if !unknown_values.contains(&status) {
                    unknown_values.push(status.clone());
                }
                if let Some(p) = plan_name {
                    unknown_plans.insert(p);
                }
            }
        }
        (
            by_status,
            unknown_values,
            unknown_plans.into_iter().collect(),
        )
    };

    let unknown_agent_count = agent_by_status
        .iter()
        .filter(|(k, _)| !KNOWN_AGENT_STATUSES.contains(&k.as_str()))
        .filter_map(|(_, v)| v.as_i64())
        .sum::<i64>() as u32;

    if unknown_agent_count > 0 {
        warnings.push(Warning {
            category: "agents",
            kind: "unknown_status",
            count: unknown_agent_count,
            message: format!(
                "{unknown_agent_count} agent row(s) have an unrecognised status value: {}",
                format_unknown_values(&unknown_agent_status_values)
            ),
            affected_plans: unknown_agent_plans,
            recovery: "reset_task_status",
        });
    }

    // ── task_status ──────────────────────────────────────────────────
    type TaskStatusScan = (
        serde_json::Map<String, serde_json::Value>,
        Vec<String>,
        Vec<String>,
        u32,
        Vec<String>,
    );
    let (
        task_by_status,
        unknown_task_values,
        unknown_task_plans,
        stuck_checking_count,
        stuck_checking_plans,
    ): TaskStatusScan = {
        let conn = state.db.lock().unwrap();
        let mut by_status = serde_json::Map::new();
        let mut unknown_values: Vec<String> = Vec::new();
        let mut unknown_plans = std::collections::BTreeSet::new();

        if let Ok(mut stmt) = conn.prepare(
            "SELECT status, plan_name, COUNT(*) FROM task_status GROUP BY status, plan_name",
        ) {
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (status, plan_name, count) in rows {
                let bucket = by_status
                    .entry(status.clone())
                    .or_insert_with(|| serde_json::Value::Number(0.into()));
                if let Some(n) = bucket.as_i64() {
                    *bucket = serde_json::Value::Number((n + count).into());
                }
                if !KNOWN_TASK_STATUSES.contains(&status.as_str()) {
                    if !unknown_values.contains(&status) {
                        unknown_values.push(status.clone());
                    }
                    unknown_plans.insert(plan_name);
                }
            }
        }

        // Stuck `checking`: no live agent for the (plan, task) pair.
        let mut stuck = 0u32;
        let mut stuck_plans = std::collections::BTreeSet::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT t.plan_name, t.task_number FROM task_status t \
             WHERE t.status = 'checking' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM agents a \
                    WHERE a.plan_name = t.plan_name \
                      AND a.task_id   = t.task_number \
                      AND a.status IN ('running', 'starting') \
               )",
        ) {
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (plan, _task) in rows {
                stuck += 1;
                stuck_plans.insert(plan);
            }
        }

        (
            by_status,
            unknown_values,
            unknown_plans.into_iter().collect(),
            stuck,
            stuck_plans.into_iter().collect(),
        )
    };

    let unknown_task_count = task_by_status
        .iter()
        .filter(|(k, _)| !KNOWN_TASK_STATUSES.contains(&k.as_str()))
        .filter_map(|(_, v)| v.as_i64())
        .sum::<i64>() as u32;

    if unknown_task_count > 0 {
        warnings.push(Warning {
            category: "taskStatus",
            kind: "unknown_status",
            count: unknown_task_count,
            message: format!(
                "{unknown_task_count} task_status row(s) have an unrecognised status value: {}",
                format_unknown_values(&unknown_task_values)
            ),
            affected_plans: unknown_task_plans,
            recovery: "reset_task_status",
        });
    }
    if stuck_checking_count > 0 {
        warnings.push(Warning {
            category: "taskStatus",
            kind: "stuck_checking",
            count: stuck_checking_count,
            message: format!(
                "{stuck_checking_count} task(s) stuck in 'checking' with no live agent",
            ),
            affected_plans: stuck_checking_plans,
            recovery: "reset_task_status",
        });
    }

    // ── branches: orphan-sweep telemetry ─────────────────────────────
    let (last_sweep_at, seconds_since_last_sweep, last_sweep_cleared) = {
        let conn = state.db.lock().unwrap();
        let last_at: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'last_orphan_sweep_at'",
                params![],
                |r| r.get(0),
            )
            .ok();
        let cleared: u32 = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'last_orphan_sweep_cleared'",
                params![],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let secs: Option<i64> = conn
            .query_row(
                "SELECT CAST(strftime('%s','now') AS INTEGER) - \
                        CAST(strftime('%s', value) AS INTEGER) \
                 FROM settings WHERE key = 'last_orphan_sweep_at'",
                params![],
                |r| r.get(0),
            )
            .ok();
        (last_at, secs, cleared)
    };

    if last_sweep_at.is_none() {
        warnings.push(Warning {
            category: "branches",
            kind: "sweep_never_ran",
            count: 0,
            message: "Boot orphan-branch sweep has not run yet — restart the server".to_string(),
            affected_plans: vec![],
            recovery: "boot_required",
        });
    }

    // ── ci_runs ──────────────────────────────────────────────────────
    let (dismissed_count, total_count): (u32, u32) = {
        let conn = state.db.lock().unwrap();
        let dismissed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ci_runs WHERE dismissed_at IS NOT NULL",
                params![],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM ci_runs", params![], |r| r.get(0))
            .unwrap_or(0);
        (dismissed as u32, total as u32)
    };

    let response = HealthStateResponse {
        status: if warnings.is_empty() {
            "ok"
        } else {
            "warnings"
        },
        checked_at: chrono::Utc::now().to_rfc3339(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        categories: Categories {
            agents: AgentsCategory {
                by_status: agent_by_status,
                unknown_status_count: unknown_agent_count,
            },
            task_status: TaskStatusCategory {
                by_status: task_by_status,
                unknown_status_count: unknown_task_count,
                stuck_checking_count,
            },
            branches: BranchesCategory {
                last_sweep_at,
                seconds_since_last_sweep,
                last_sweep_cleared_count: last_sweep_cleared,
            },
            ci_runs: CiRunsCategory {
                dismissed_count,
                total_count,
            },
        },
        warnings,
    };

    Json(response).into_response()
}

fn empty_categories() -> Categories {
    Categories {
        agents: AgentsCategory {
            by_status: serde_json::Map::new(),
            unknown_status_count: 0,
        },
        task_status: TaskStatusCategory {
            by_status: serde_json::Map::new(),
            unknown_status_count: 0,
            stuck_checking_count: 0,
        },
        branches: BranchesCategory {
            last_sweep_at: None,
            seconds_since_last_sweep: None,
            last_sweep_cleared_count: 0,
        },
        ci_runs: CiRunsCategory {
            dismissed_count: 0,
            total_count: 0,
        },
    }
}
