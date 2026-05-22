//! Audit log: "who did what, when, where" for every state-changing action.
//!
//! Every mutation (agent start/kill, task status change, merge, config change,
//! user invite) is recorded with user_id, timestamp, and a JSON diff. The log
//! is scoped to an organization and exposed via API as an activity feed,
//! exportable as CSV for compliance.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use serde::Serialize;

/// Actions that can appear in the audit log.
#[allow(dead_code)]
pub mod actions {
    pub const AGENT_START: &str = "agent.start";
    pub const AGENT_KILL: &str = "agent.kill";
    pub const AGENT_FINISH: &str = "agent.finish";
    /// Auto-mode automatically sent the agent's graceful-exit
    /// sequence after detecting the agent had finished its turn
    /// (Stop hook) or had been idle past the timeout. Diff carries
    /// `{trigger: "stop_hook" | "idle_timeout"}`.
    pub const AGENT_AUTO_FINISH: &str = "agent.auto_finish";
    pub const TASK_STATUS_CHANGE: &str = "task.status_change";
    pub const BRANCH_MERGE: &str = "branch.merge";
    pub const BRANCH_DISCARD: &str = "branch.discard";
    pub const CONFIG_EFFORT_CHANGE: &str = "config.effort_change";
    pub const CONFIG_BUDGET_CHANGE: &str = "config.budget_change";
    pub const CONFIG_AUTO_ADVANCE: &str = "config.auto_advance";
    pub const CONFIG_AUTO_MODE: &str = "config.auto_mode";
    /// PUT /api/plans/:name/config tried to enable `parallel` while either
    /// (a) `WORKTREES_SHIPPED` is false in this build, or (b) the project
    /// is not opted in via `plan_project.worktree_isolation_opt_in`. The
    /// request returns 412 and this row records the attempt. Diff carries
    /// `{requested: true, reason: "worktrees_not_ready"}`.
    pub const CONFIG_PARALLEL_REFUSED: &str = "config.parallel_refused";
    /// PUT /api/plans/:name/settings wrote new values for
    /// `ci_blocking_workflows` and/or `phase_verification` to the plan
    /// YAML. Diff carries the post-update fields (camelCase) — explicit
    /// `null` is preserved so a remove shows up in the trail.
    pub const CONFIG_PLAN_SETTINGS: &str = "config.plan_settings";
    /// PUT /api/plans/:name/phases/:number/settings wrote a phase-level
    /// override for `phase_verification` (and/or, in the future, other
    /// phase-scoped fields) into the plan YAML. Diff carries
    /// `{phaseNumber, ...post-update fields}` (camelCase) — explicit
    /// `null` preserved so a remove (Inherit) shows up in the trail.
    pub const CONFIG_PHASE_SETTINGS: &str = "config.phase_settings";
    /// User clicked Resume on the auto-mode pill: clear `paused_reason` and
    /// re-evaluate auto-advance from the most recently completed task. Audit
    /// payload carries `last_completed_task` so the trail captures which task
    /// the resume re-anchored on (helpful when the loop subsequently spawns
    /// a fresh agent).
    pub const AUTO_MODE_RESUMED: &str = "auto_mode.resumed";
    pub const CONFIG_PROJECT_CHANGE: &str = "config.project_change";
    pub const CONFIG_KILL_SWITCH: &str = "config.kill_switch";
    /// `PUT /api/runners/{id}/config` set or cleared a per-runner override
    /// for `effort` / `skip_permissions`. Diff carries
    /// `{runner_id, effort: Option<String>, skip_permissions: Option<bool>}`,
    /// where `null` means the override was cleared (back to inherit).
    pub const CONFIG_RUNNER_OVERRIDE: &str = "config.runner_override";
    /// `PUT /api/plans/{name}/config` set or cleared the per-plan runner
    /// affinity (T11.4). Diff carries `{runnerId: Option<String>}` —
    /// `null` means the pin was cleared (back to "any online runner").
    pub const CONFIG_RUNNER_AFFINITY: &str = "config.runner_affinity";
    /// `POST /api/runners/{id}/shutdown` — operator clicked "Request
    /// shutdown" on the dashboard. The server enqueued a reliable
    /// `WireMessage::ShutdownRequest` to the runner; whether the runner
    /// honored it is a separate observation (the row flips to
    /// `runner_disconnected` if it did). Diff carries
    /// `{runner_id, reason: Option<String>, in_flight_agents: Vec<String>}`.
    pub const RUNNER_SHUTDOWN_REQUESTED: &str = "runner.shutdown_requested";
    /// `DELETE /api/runners/{id}` — operator revoked a runner from the
    /// dashboard: every `runner_tokens` row for the runner is deleted (so
    /// the next reconnect fails 401 at the WS upgrade) and the `runners`
    /// row is soft-deleted via `removed_at`. Diff carries
    /// `{runner_id, runner_name, tokens_revoked: usize}`.
    pub const RUNNER_REVOKED: &str = "runner.revoked";
    /// `POST /api/runners/{id}/version-mismatch-override` — operator opted in
    /// to dispatching agents to a runner whose `version_severity` is `red`.
    /// Diff carries `{runner_id, override: bool, runner_version, server_version}`.
    /// Audit value is the operator's typed acknowledgement of the version
    /// mismatch — if a future incident is rooted in protocol drift, this row
    /// is what shows that the operator was warned and accepted the risk.
    pub const RUNNER_VERSION_MISMATCH_OVERRIDE: &str = "runner.version_mismatch_override";
    /// `POST /api/runners/{id}/rotate-token` — operator minted a fresh API
    /// token for an existing runner without going through DELETE +
    /// re-enroll. Every previous `runner_tokens` row for the runner is
    /// deleted (so the old token is dead) and a new row is inserted with
    /// `claimed_runner_id` pre-bound to this runner so the next reconnect
    /// Verify-OKs immediately. The `runners` row + every dependent row
    /// (`agents`, `runner_config`, `plan_runner_affinity`, …) is untouched.
    /// Diff carries `{runner_id, runner_name, tokens_revoked: usize}`.
    pub const RUNNER_TOKEN_ROTATED: &str = "runner.token_rotated";
    /// `POST /api/runners/{id}/upgrade` — operator clicked "Upgrade" on
    /// the runner row to swap in the matching `branchwork-runner` +
    /// `branchwork-server` binaries. The server enqueues a reliable
    /// `WireMessage::UpgradeRunner` that the runner consumes by shelling
    /// `curl -fsSL <saas_url>/install-runner.sh | sh -s -- --just-binary`.
    /// Diff carries `{runner_id, reason: Option<String>, server_version,
    /// runner_version_before}`. Audit value: a future incident triggered
    /// by an upgrade can be traced back to this row.
    pub const RUNNER_UPGRADE_REQUESTED: &str = "runner.upgrade_requested";
    /// Defense-in-depth (T5.3): the WS handler observed a connect from a
    /// runner whose `runners` row has `removed_at IS NOT NULL`. After T5.1
    /// (token DELETE in `delete_runner`) + T5.2 (UPSERT clearing
    /// `removed_at` on the first message) this should be rare — the only
    /// way we land here is a legacy / stale-state row where the token
    /// DELETE was skipped, OR a fresh shape of this bug class we have not
    /// yet closed. The row is emitted BEFORE the un-hide UPSERT so the
    /// forensics survive even though normal operation continues. Diff
    /// carries `{runner_id, runner_name, removed_at, token_hash_prefix}`
    /// (the prefix is the first 8 chars of the stored token_hash — see
    /// `runner_ws::sha256_hex` for why we only log a fingerprint).
    /// Resource is `RUNNER` keyed by `runner_id`. A spike on this action
    /// is the smoke signal that something is soft-deleting runners
    /// without revoking their tokens.
    pub const RUNNER_GHOST_RECONNECT: &str = "runner.ghost_reconnect";
    /// `git_helpers::push_branch_local` was rejected as non-fast-forward
    /// and successfully rebased + retried. One row per retry attempt
    /// (so a 3-attempt push that succeeds on attempt 2 produces one
    /// row; a 3-attempt push that succeeds on attempt 3 produces two
    /// rows). Diff carries
    /// `{branch, attempt, last_rebase_sha, prior_remote_sha}`. Resource
    /// is the merged plan (`PLAN`) so the row groups with the
    /// `auto_mode.merged` event that triggered the push. Operator
    /// visibility — silent retries that succeed are fine, but a retry
    /// burst means the race window is widening and we should know.
    pub const AUTO_PUSH_REBASE_RETRY: &str = "auto_push_rebase_retry";
    /// `POST /api/git/push-lock` successfully acquired the per-branch
    /// advisory lock for an external caller (e.g. auto-bump CI). The
    /// row records the holder's metadata so an org admin can audit
    /// concurrent push attempts. Resource is the plan (`PLAN`) keyed by
    /// the branch name (which is conventionally the project's default
    /// branch, e.g. `master`).
    pub const PUSH_LOCK_ACQUIRED: &str = "push_lock.acquired";
    /// `POST /api/git/push-lock/release` was called. `released: true`
    /// in the diff means the row was deleted; `false` means the token
    /// didn't match (or the row was already gone — usually because the
    /// TTL evicted a previous crashed holder).
    pub const PUSH_LOCK_RELEASED: &str = "push_lock.released";
    pub const ORG_MEMBER_ADD: &str = "org.member_add";
    pub const ORG_MEMBER_REMOVE: &str = "org.member_remove";
    pub const ORG_MEMBER_ROLE_CHANGE: &str = "org.member_role_change";
    pub const PLAN_CREATE: &str = "plan.create";
    pub const PLAN_UPDATE: &str = "plan.update";
    /// `DELETE /api/plans/:name` removed (or archived) a plan and cascaded
    /// every `plan_name`-keyed row out of the DB. Diff carries
    /// `{file_path, archive_path, snapshot_id, hard, cascaded_rows}`;
    /// `archive_path` and `snapshot_id` are null on hard delete.
    pub const PLAN_DELETE: &str = "plan.delete";
    /// `POST /api/snapshots/:id/restore` replayed a `plan_snapshots` row
    /// (kind=delete|merge|rename|archive|rewrite_context) back into the
    /// active plans dir + cascade tables. Diff carries
    /// `{snapshot_id, plan_name, kind, replayed_rows}`. Resource is the
    /// restored plan name.
    pub const PLAN_RESTORE: &str = "plan.restore";
    /// The retention purger (`plan_curate::purge_expired`) hard-deleted
    /// an expired `plan_snapshots` row past its `expires_at`. Diff
    /// carries `{snapshot_id, kind, archive_path}`; resource is the
    /// snapshotted plan's name. Append-only: no second-level undo for
    /// purged snapshots.
    pub const PLAN_SNAPSHOT_PURGED: &str = "plan.snapshot_purged";
    /// `DELETE /api/snapshots/:id` immediately hard-deleted a snapshot
    /// row + its archive YAML at the user's request from the Archive
    /// panel — counterpart of `PLAN_RESTORE`. Diff carries
    /// `{snapshot_id, kind, archive_path}`; resource is the snapshotted
    /// plan's name. Append-only: no second-level undo.
    pub const PLAN_SNAPSHOT_PURGED_MANUAL: &str = "plan.snapshot_purged_manual";
    pub const AUTH_SIGNUP: &str = "auth.signup";
    pub const AUTH_LOGIN: &str = "auth.login";
    pub const SSO_LOGIN: &str = "sso.login";
    pub const SSO_JIT_PROVISION: &str = "sso.jit_provision";
    pub const SSO_PROVIDER_CREATE: &str = "sso.provider_create";
    pub const SSO_PROVIDER_UPDATE: &str = "sso.provider_update";
    pub const SSO_PROVIDER_DELETE: &str = "sso.provider_delete";
    /// `POST /api/projects` — operator created a project row via the
    /// dashboard's New Project flow (Phase 2.1 of
    /// runner-daemon-workspace). Diff carries `{project_id, name,
    /// repo_url, host, workspace_path}` so the activity feed can render a
    /// row pointing at the resolved on-disk location.
    pub const PROJECT_CREATE: &str = "project.create";
    /// `DELETE /api/projects/{id}` — operator removed a project. Diff
    /// carries `{project_id, name, repo_url, workspace_path,
    /// wipe_on_disk, wipe_failed}` so a post-mortem can tell whether the
    /// row was the only thing removed or whether the workspace was also
    /// wiped (and if so, whether the wipe succeeded).
    pub const PROJECT_DELETE: &str = "project.delete";
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
    pub const RUNNER: &str = "runner";
    pub const PROJECT: &str = "project";
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
    /// `diff.snapshot_id` parsed out at query time for actions that
    /// snapshot before mutating (`plan.delete` and friends). `None`
    /// when the diff is missing, malformed, or carried no snapshot
    /// (hard delete). The Activity-tab Undo affordance keys off this
    /// field plus `recoverable` / `restored_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<i64>,
    /// `true` iff `snapshot_id` resolves to a `plan_snapshots` row whose
    /// `restored_at IS NULL` AND `expires_at > datetime('now')`. `false`
    /// when the snapshot was purged, expired, or already restored.
    /// Always `false` when `snapshot_id` is `None`.
    pub recoverable: bool,
    /// `plan_snapshots.restored_at` for the row pointed at by
    /// `snapshot_id`. Set after the snapshot has been replayed via
    /// `POST /api/snapshots/:id/restore`; `None` while still
    /// recoverable, expired, or never snapshotted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_at: Option<String>,
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

    let mut entries = conn
        .prepare(&sql)
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
                    snapshot_id: None,
                    recoverable: false,
                    restored_at: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    enrich_with_snapshot_state(conn, &mut entries);
    entries
}

/// Decorate each `AuditEntry` with `snapshot_id`, `recoverable`, and
/// `restored_at` by parsing the diff and (when a snapshot id is
/// present) joining against `plan_snapshots`. Missing-table or
/// JSON-parse errors degrade silently — the entry simply renders
/// without an Undo affordance.
fn enrich_with_snapshot_state(conn: &Connection, entries: &mut [AuditEntry]) {
    let mut indices: Vec<(usize, i64)> = Vec::new();
    for (i, e) in entries.iter_mut().enumerate() {
        let Some(diff) = e.diff.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(diff) else {
            continue;
        };
        let Some(sid) = value.get("snapshot_id").and_then(|n| n.as_i64()) else {
            continue;
        };
        e.snapshot_id = Some(sid);
        indices.push((i, sid));
    }
    if indices.is_empty() {
        return;
    }
    let ids: Vec<i64> = indices.iter().map(|(_, s)| *s).collect();
    let placeholders = (0..ids.len())
        .map(|i| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, restored_at, (expires_at > datetime('now')) AS is_live \
         FROM plan_snapshots WHERE id IN ({placeholders})",
    );
    let id_params: Vec<&dyn rusqlite::types::ToSql> = ids
        .iter()
        .map(|i| i as &dyn rusqlite::types::ToSql)
        .collect();
    let rows: HashMap<i64, (Option<String>, bool)> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map(id_params.as_slice(), |row| {
                let id: i64 = row.get(0)?;
                let restored_at: Option<String> = row.get(1)?;
                let is_live: i64 = row.get(2)?;
                Ok((id, (restored_at, is_live != 0)))
            })?
            .collect::<Result<HashMap<_, _>, _>>()
        })
        .unwrap_or_default();
    for (i, sid) in indices {
        let entry = &mut entries[i];
        if let Some((restored_at, is_live)) = rows.get(&sid) {
            entry.restored_at = restored_at.clone();
            entry.recoverable = restored_at.is_none() && *is_live;
        }
    }
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
            CREATE INDEX idx_audit_org_created ON audit_logs(org_id, created_at DESC);
            CREATE TABLE plan_snapshots (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                plan_name     TEXT NOT NULL,
                kind          TEXT NOT NULL,
                created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                expires_at    TEXT NOT NULL,
                org_id        TEXT NOT NULL DEFAULT 'default-org',
                archive_path  TEXT,
                yaml_body     TEXT NOT NULL,
                cascade_json  TEXT NOT NULL,
                restored_at   TEXT
            );",
        )
        .unwrap();
        conn
    }

    /// Insert a fake `plan_snapshots` row and return its id. Tests use
    /// this to drive the recoverable/restored/expired branches of
    /// `enrich_with_snapshot_state` end-to-end.
    fn seed_snapshot(
        conn: &Connection,
        plan_name: &str,
        expires_at: &str,
        restored_at: Option<&str>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO plan_snapshots \
                 (plan_name, kind, expires_at, yaml_body, cascade_json, restored_at) \
             VALUES (?1, 'delete', ?2, '', '{}', ?3)",
            params![plan_name, expires_at, restored_at],
        )
        .unwrap();
        conn.last_insert_rowid()
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
    fn log_and_list_agent_auto_finish() {
        let conn = test_conn();
        // Stop-hook trigger (Claude returned a Stop event to the local hook URL).
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::AGENT_AUTO_FINISH,
            resources::AGENT,
            Some("agent-stop"),
            Some(r#"{"trigger":"stop_hook"}"#),
        );
        // Idle-timeout trigger (BRANCHWORK_AUTO_FINISH_IDLE fallback fired).
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::AGENT_AUTO_FINISH,
            resources::AGENT,
            Some("agent-idle"),
            Some(r#"{"trigger":"idle_timeout"}"#),
        );
        // Non-auto-finish row that must NOT appear under the action filter.
        log(
            &conn,
            "org-1",
            Some("user-1"),
            Some("alice@example.com"),
            actions::AGENT_FINISH,
            resources::AGENT,
            Some("agent-manual"),
            None,
        );

        let filtered = list(
            &conn,
            "org-1",
            50,
            0,
            Some(actions::AGENT_AUTO_FINISH),
            None,
        );
        assert_eq!(filtered.len(), 2);
        // Newest first — idle_timeout was inserted last.
        assert_eq!(filtered[0].resource_id.as_deref(), Some("agent-idle"));
        assert_eq!(
            filtered[0].diff.as_deref(),
            Some(r#"{"trigger":"idle_timeout"}"#)
        );
        assert_eq!(filtered[1].resource_id.as_deref(), Some("agent-stop"));
        assert_eq!(
            filtered[1].diff.as_deref(),
            Some(r#"{"trigger":"stop_hook"}"#)
        );
        // Manual-finish row stays out of the filtered slice.
        assert!(
            filtered
                .iter()
                .all(|e| e.resource_id.as_deref() != Some("agent-manual"))
        );
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
                snapshot_id: None,
                recoverable: false,
                restored_at: None,
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
                snapshot_id: None,
                recoverable: false,
                restored_at: None,
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

    #[test]
    fn list_surfaces_recoverable_for_unrestored_unexpired_snapshot() {
        let conn = test_conn();
        let snap = seed_snapshot(&conn, "doomed", "2099-01-01 00:00:00", None);
        log(
            &conn,
            "org-1",
            Some("u"),
            Some("u@example.com"),
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("doomed"),
            Some(&format!(
                r#"{{"file_path":"/p/doomed.yaml","snapshot_id":{snap},"hard":false}}"#
            )),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].snapshot_id, Some(snap));
        assert!(entries[0].recoverable);
        assert!(entries[0].restored_at.is_none());
    }

    #[test]
    fn list_marks_already_restored_snapshot_as_unrecoverable() {
        let conn = test_conn();
        let snap = seed_snapshot(
            &conn,
            "saved",
            "2099-01-01 00:00:00",
            Some("2026-04-12 09:00:00"),
        );
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("saved"),
            Some(&format!(r#"{{"snapshot_id":{snap},"hard":false}}"#)),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert_eq!(entries[0].snapshot_id, Some(snap));
        assert!(!entries[0].recoverable);
        assert_eq!(
            entries[0].restored_at.as_deref(),
            Some("2026-04-12 09:00:00")
        );
    }

    #[test]
    fn list_marks_expired_snapshot_as_unrecoverable_without_restored_at() {
        let conn = test_conn();
        let snap = seed_snapshot(&conn, "expired", "2000-01-01 00:00:00", None);
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("expired"),
            Some(&format!(r#"{{"snapshot_id":{snap},"hard":false}}"#)),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert_eq!(entries[0].snapshot_id, Some(snap));
        assert!(!entries[0].recoverable);
        assert!(entries[0].restored_at.is_none());
    }

    #[test]
    fn list_marks_purged_snapshot_id_as_unrecoverable() {
        // The retention purger may have hard-deleted the snapshot row;
        // the audit row's diff still references the (now stale) id.
        let conn = test_conn();
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("ghost"),
            Some(r#"{"snapshot_id":99999,"hard":false}"#),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert_eq!(entries[0].snapshot_id, Some(99999));
        assert!(!entries[0].recoverable);
        assert!(entries[0].restored_at.is_none());
    }

    #[test]
    fn list_leaves_non_snapshot_actions_alone() {
        let conn = test_conn();
        log(
            &conn,
            "org-1",
            Some("u"),
            Some("u@example.com"),
            actions::AGENT_START,
            resources::AGENT,
            Some("agent-1"),
            Some(r#"{"plan":"p","task":"1.1"}"#),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert!(entries[0].snapshot_id.is_none());
        assert!(!entries[0].recoverable);
        assert!(entries[0].restored_at.is_none());
    }

    #[test]
    fn list_handles_hard_delete_without_snapshot_id() {
        // Hard delete carries `snapshot_id: null` in the diff. Same
        // contract as actions that never snapshot — no Undo affordance.
        let conn = test_conn();
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("hard-removed"),
            Some(r#"{"snapshot_id":null,"hard":true}"#),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        assert!(entries[0].snapshot_id.is_none());
        assert!(!entries[0].recoverable);
        assert!(entries[0].restored_at.is_none());
    }

    #[test]
    fn list_serializes_snapshot_fields_in_camelcase() {
        let conn = test_conn();
        let snap = seed_snapshot(&conn, "p", "2099-01-01 00:00:00", None);
        log(
            &conn,
            "org-1",
            None,
            None,
            actions::PLAN_DELETE,
            resources::PLAN,
            Some("p"),
            Some(&format!(r#"{{"snapshot_id":{snap}}}"#)),
        );

        let entries = list(&conn, "org-1", 50, 0, None, None);
        let json = serde_json::to_string(&entries[0]).unwrap();
        assert!(
            json.contains("\"snapshotId\""),
            "json must use camelCase: {json}"
        );
        assert!(
            json.contains("\"recoverable\":true"),
            "recoverable must be true: {json}"
        );
        // restoredAt is omitted when None per skip_serializing_if.
        assert!(
            !json.contains("\"restoredAt\""),
            "restoredAt should be omitted when None: {json}"
        );
    }
}
