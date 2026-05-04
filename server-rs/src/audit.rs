//! Audit log: "who did what, when, where" for every state-changing action.
//!
//! Every mutation (agent start/kill, task status change, merge, config change,
//! user invite) is recorded with user_id, timestamp, and a JSON diff. The log
//! is scoped to an organization and exposed via API as an activity feed,
//! exportable as CSV for compliance.

use rusqlite::{Connection, params};
use serde::Serialize;

/// Actions that can appear in the audit log.
#[allow(dead_code)]
pub mod actions {
    pub const AGENT_START: &str = "agent.start";
    pub const AGENT_KILL: &str = "agent.kill";
    pub const AGENT_FINISH: &str = "agent.finish";
    pub const TASK_STATUS_CHANGE: &str = "task.status_change";
    pub const BRANCH_MERGE: &str = "branch.merge";
    pub const BRANCH_DISCARD: &str = "branch.discard";
    pub const CONFIG_EFFORT_CHANGE: &str = "config.effort_change";
    pub const CONFIG_BUDGET_CHANGE: &str = "config.budget_change";
    pub const CONFIG_AUTO_ADVANCE: &str = "config.auto_advance";
    pub const CONFIG_AUTO_MODE: &str = "config.auto_mode";
    /// User clicked Resume on the auto-mode pill: clear `paused_reason` and
    /// re-evaluate auto-advance from the most recently completed task. Audit
    /// payload carries `last_completed_task` so the trail captures which task
    /// the resume re-anchored on (helpful when the loop subsequently spawns
    /// a fresh agent).
    pub const AUTO_MODE_RESUMED: &str = "auto_mode.resumed";
    pub const CONFIG_PROJECT_CHANGE: &str = "config.project_change";
    pub const CONFIG_KILL_SWITCH: &str = "config.kill_switch";
    pub const ORG_MEMBER_ADD: &str = "org.member_add";
    pub const ORG_MEMBER_REMOVE: &str = "org.member_remove";
    pub const ORG_MEMBER_ROLE_CHANGE: &str = "org.member_role_change";
    pub const PLAN_CREATE: &str = "plan.create";
    pub const PLAN_UPDATE: &str = "plan.update";
    pub const AUTH_SIGNUP: &str = "auth.signup";
    pub const AUTH_LOGIN: &str = "auth.login";
    pub const SSO_LOGIN: &str = "sso.login";
    pub const SSO_JIT_PROVISION: &str = "sso.jit_provision";
    pub const SSO_PROVIDER_CREATE: &str = "sso.provider_create";
    pub const SSO_PROVIDER_UPDATE: &str = "sso.provider_update";
    pub const SSO_PROVIDER_DELETE: &str = "sso.provider_delete";
}

/// Resource types for audit entries.
#[allow(dead_code)]
pub mod resources {
    pub const AGENT: &str = "agent";
    pub const TASK: &str = "task";
    pub const PLAN: &str = "plan";
    pub const ORG: &str = "org";
    pub const USER: &str = "user";
    pub const CONFIG: &str = "config";
    pub const SSO_PROVIDER: &str = "sso_provider";
}

/// A single audit log entry as returned by the API.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: i64,
    pub org_id: String,
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub diff: Option<String>,
    pub created_at: String,
}

/// Insert an audit log entry. Call this from any handler that mutates state.
///
/// - `user_id` / `user_email`: `None` for system-initiated actions (e.g.
///   auto-advance, heartbeat-based status changes).
/// - `diff`: free-form JSON describing what changed; callers typically use
///   `serde_json::json!({...}).to_string()`.
#[allow(clippy::too_many_arguments)]
pub fn log(
    conn: &Connection,
    org_id: &str,
    user_id: Option<&str>,
    user_email: Option<&str>,
    action: &str,
    resource_type: &str,
    resource_id: Option<&str>,
    diff: Option<&str>,
) {
    conn.execute(
        "INSERT INTO audit_logs (org_id, user_id, user_email, action, resource_type, resource_id, diff)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![org_id, user_id, user_email, action, resource_type, resource_id, diff],
    )
    .ok(); // Best-effort: never fail the parent operation because of audit logging
}

/// Insert an audit log entry and broadcast it via WebSocket so connected
/// dashboards show the activity feed in real-time.
#[allow(dead_code, clippy::too_many_arguments)]
pub fn log_and_broadcast(
    conn: &Connection,
    broadcast_tx: &tokio::sync::broadcast::Sender<String>,
    org_id: &str,
    user_id: Option<&str>,
    user_email: Option<&str>,
    action: &str,
    resource_type: &str,
    resource_id: Option<&str>,
    diff: Option<&str>,
) {
    log(
        conn,
        org_id,
        user_id,
        user_email,
        action,
        resource_type,
        resource_id,
        diff,
    );
    crate::ws::broadcast_event(
        broadcast_tx,
        "audit_log",
        serde_json::json!({
            "org_id": org_id,
            "user_email": user_email,
            "action": action,
            "resource_type": resource_type,
            "resource_id": resource_id,
        }),
    );
}

/// Query audit entries for an org, newest first.
pub fn list(
    conn: &Connection,
    org_id: &str,
    limit: i64,
    offset: i64,
    action_filter: Option<&str>,
    resource_type_filter: Option<&str>,
) -> Vec<AuditEntry> {
    // Build the query dynamically based on optional filters.
    let mut sql = String::from(
        "SELECT id, org_id, user_id, user_email, action, resource_type, resource_id, diff, created_at
         FROM audit_logs WHERE org_id = ?1",
    );
    let mut param_idx = 2;
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(org_id.to_string())];

    if let Some(action) = action_filter {
        sql.push_str(&format!(" AND action = ?{param_idx}"));
        params_vec.push(Box::new(action.to_string()));
        param_idx += 1;
    }
    if let Some(rt) = resource_type_filter {
        sql.push_str(&format!(" AND resource_type = ?{param_idx}"));
        params_vec.push(Box::new(rt.to_string()));
        param_idx += 1;
    }

    sql.push_str(&format!(
        " ORDER BY created_at DESC, id DESC LIMIT ?{} OFFSET ?{}",
        param_idx,
        param_idx + 1
    ));
    params_vec.push(Box::new(limit));
    params_vec.push(Box::new(offset));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    conn.prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map(param_refs.as_slice(), |row| {
                Ok(AuditEntry {
                    id: row.get(0)?,
                    org_id: row.get(1)?,
                    user_id: row.get(2)?,
                    user_email: row.get(3)?,
                    action: row.get(4)?,
                    resource_type: row.get(5)?,
                    resource_id: row.get(6)?,
                    diff: row.get(7)?,
                    created_at: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default()
}

/// Total count of audit entries for an org (for pagination).
pub fn count(
    conn: &Connection,
    org_id: &str,
    action_filter: Option<&str>,
    resource_type_filter: Option<&str>,
) -> i64 {
    let mut sql = String::from("SELECT COUNT(*) FROM audit_logs WHERE org_id = ?1");
    let mut param_idx = 2;
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(org_id.to_string())];

    if let Some(action) = action_filter {
        sql.push_str(&format!(" AND action = ?{param_idx}"));
        params_vec.push(Box::new(action.to_string()));
        param_idx += 1;
    }
    if let Some(rt) = resource_type_filter {
        sql.push_str(&format!(" AND resource_type = ?{param_idx}"));
        params_vec.push(Box::new(rt.to_string()));
        let _ = param_idx; // suppress unused warning
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or(0)
}

/// Generate CSV content from audit entries.
pub fn to_csv(entries: &[AuditEntry]) -> String {
    let mut out =
        String::from("id,timestamp,user_id,user_email,action,resource_type,resource_id,diff\n");
    for e in entries {
        // Escape CSV fields that may contain commas/quotes/newlines
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            e.id,
            csv_escape(&e.created_at),
            csv_escape(e.user_id.as_deref().unwrap_or("")),
            csv_escape(e.user_email.as_deref().unwrap_or("")),
            csv_escape(&e.action),
            csv_escape(&e.resource_type),
            csv_escape(e.resource_id.as_deref().unwrap_or("")),
            csv_escape(e.diff.as_deref().unwrap_or("")),
        ));
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ── API handlers ────────────────────────────────────────────────────────────

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::auth::AuthUser;
use crate::state::AppState;

#[derive(serde::Deserialize)]
pub struct AuditQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub action: Option<String>,
    pub resource_type: Option<String>,
}

/// GET /api/orgs/{slug}/audit-log
pub async fn list_audit_log(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let org_id = {
        let conn = state.db.lock().unwrap();
        match conn
            .query_row(
                "SELECT id FROM organizations WHERE slug = ?1 OR id = ?1",
                params![slug],
                |row| row.get::<_, String>(0),
            )
            .ok()
        {
            Some(id) => id,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "org_not_found"})),
                )
                    .into_response();
            }
        }
    };

    // Verify membership
    {
        let conn = state.db.lock().unwrap();
        let is_member: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM org_members WHERE org_id = ?1 AND user_id = ?2",
                params![org_id, user.id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !is_member {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "not_a_member"})),
            )
                .into_response();
        }
    }

    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);

    let conn = state.db.lock().unwrap();
    let entries = list(
        &conn,
        &org_id,
        limit,
        offset,
        q.action.as_deref(),
        q.resource_type.as_deref(),
    );
    let total = count(
        &conn,
        &org_id,
        q.action.as_deref(),
        q.resource_type.as_deref(),
    );

    Json(serde_json::json!({
        "entries": entries,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
    .into_response()
}

/// GET /api/orgs/{slug}/audit-log/export — CSV download
pub async fn export_audit_log(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let org_id = {
        let conn = state.db.lock().unwrap();
        match conn
            .query_row(
                "SELECT id FROM organizations WHERE slug = ?1 OR id = ?1",
                params![slug],
                |row| row.get::<_, String>(0),
            )
            .ok()
        {
            Some(id) => id,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "org_not_found"})),
                )
                    .into_response();
            }
        }
    };

    // Verify membership
    {
        let conn = state.db.lock().unwrap();
        let is_member: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM org_members WHERE org_id = ?1 AND user_id = ?2",
                params![org_id, user.id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !is_member {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "not_a_member"})),
            )
                .into_response();
        }
    }

    // Export up to 10k entries (compliance-friendly batch size)
    let limit = q.limit.unwrap_or(10_000).min(10_000);
    let conn = state.db.lock().unwrap();
    let entries = list(
        &conn,
        &org_id,
        limit,
        0,
        q.action.as_deref(),
        q.resource_type.as_deref(),
    );

    let csv = to_csv(&entries);
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"audit-log.csv\"",
            ),
        ],
        csv,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            "CREATE TABLE audit_logs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                org_id        TEXT NOT NULL DEFAULT 'default-org',
                user_id       TEXT,
                user_email    TEXT,
                action        TEXT NOT NULL,
                resource_type TEXT NOT NULL,
                resource_id   TEXT,
                diff          TEXT,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX idx_audit_org_created ON audit_logs(org_id, created_at DESC);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn log_and_list() {
        let conn = test_conn();
        log(
            &conn,
            "org-1",
            Some("user-1"),
            Some("alice@example.com"),
            actions::AGENT_START,
            resources::AGENT,
            Some("agent-abc"),
            Some(r#"{"plan":"my-plan","task":"1.1"}"#),
        );
        log(
            &conn,
            "org-1",
            Some("user-2"),
            Some("bob@example.com"),
            actions::TASK_STATUS_CHANGE,
            resources::TASK,
            Some("my-plan/1.1"),
            Some(r#"{"from":"pending","to":"completed"}"#),
        );
        // Different org — should not appear
        log(
            &conn,
            "org-2",
            Some("user-3"),
            Some("eve@example.com"),
            actions::AUTH_LOGIN,
            resources::USER,
            Some("user-3"),
            None,
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert_eq!(entries.len(), 2);
        // Newest first
        assert_eq!(entries[0].action, actions::TASK_STATUS_CHANGE);
        assert_eq!(entries[1].action, actions::AGENT_START);

        // Action filter
        let filtered = list(&conn, "org-1", 50, 0, Some(actions::AGENT_START), None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].resource_id.as_deref(), Some("agent-abc"));
    }

    #[test]
    fn count_respects_filters() {
        let conn = test_conn();
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::AGENT_START,
            resources::AGENT,
            None,
            None,
        );
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::AGENT_KILL,
            resources::AGENT,
            None,
            None,
        );
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_CREATE,
            resources::PLAN,
            None,
            None,
        );

        assert_eq!(count(&conn, "org-1", None, None), 3);
        assert_eq!(count(&conn, "org-1", Some(actions::AGENT_START), None), 1);
        assert_eq!(count(&conn, "org-1", None, Some(resources::AGENT)), 2);
    }

    #[test]
    fn csv_export() {
        let entries = vec![
            AuditEntry {
                id: 1,
                org_id: "org-1".into(),
                user_id: Some("u1".into()),
                user_email: Some("a@b.com".into()),
                action: actions::AGENT_START.into(),
                resource_type: resources::AGENT.into(),
                resource_id: Some("agent-1".into()),
                diff: Some(r#"{"key":"value"}"#.into()),
                created_at: "2026-04-16T00:00:00Z".into(),
            },
            AuditEntry {
                id: 2,
                org_id: "org-1".into(),
                user_id: None,
                user_email: None,
                action: actions::TASK_STATUS_CHANGE.into(),
                resource_type: resources::TASK.into(),
                resource_id: None,
                diff: None,
                created_at: "2026-04-16T01:00:00Z".into(),
            },
        ];
        let csv = to_csv(&entries);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert!(lines[0].starts_with("id,timestamp,"));
        assert!(lines[1].contains("agent.start"));
        assert!(lines[2].contains("task.status_change"));
    }

    #[test]
    fn csv_escape_special_chars() {
        assert_eq!(csv_escape("hello"), "hello");
        assert_eq!(csv_escape("hello,world"), "\"hello,world\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn pagination() {
        let conn = test_conn();
        for i in 0..10 {
            log(
                &conn,
                "org-1",
                None,
                None,
                actions::AGENT_START,
                resources::AGENT,
                Some(&format!("agent-{i}")),
                None,
            );
        }
        let page1 = list(&conn, "org-1", 3, 0, None, None);
        assert_eq!(page1.len(), 3);

        let page2 = list(&conn, "org-1", 3, 3, None, None);
        assert_eq!(page2.len(), 3);
        // No overlap
        assert_ne!(page1[0].id, page2[0].id);

        let total = count(&conn, "org-1", None, None);
        assert_eq!(total, 10);
    }
}
