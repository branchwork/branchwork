//! REST surface for the org-shared learnings store.
//!
//! Two families of endpoints live in this module:
//!
//! Phase 1 (pending-learning queue):
//! - `GET  /api/learnings/pending`             — list `ci_failure_events`
//!   rows with `resolved_at IS NULL` (the Learnings due panel).
//! - `GET  /api/learnings/pending/{id}/log`    — failure-log tail for one
//!   pending row (drilldown).
//!
//! Phase 2.2 (org-shared learnings — append-only):
//! - `GET  /api/learnings`                     — list active learnings,
//!   filterable by `?category=` and `?kind=`. Pass `?include=archived` to
//!   surface tombstones for the Activity tab.
//! - `GET  /api/learnings/{id}`                — single learning with full
//!   `bodyMd`. Returns archived rows too so the audit view can render
//!   them; callers that want only active should check
//!   `archivedAt == null`.
//! - `POST /api/learnings/{id}/archive`        — soft-delete with required
//!   `{ reason: string }`. Idempotent at the row level (already-archived
//!   returns 410 Gone, not 200, so the operator can tell their action
//!   was a no-op). Editing is intentionally out of scope: corrections
//!   happen by archiving and writing a new entry, preserving the audit
//!   trail.
//!
//! All endpoints are org-scoped via [`OptionalAuthUser::org_id`] —
//! anonymous callers in standalone mode resolve to the default org, the
//! same convention the rest of `api/` uses. Writes (archive) emit an
//! [`audit::actions::LEARNING_ARCHIVE`] row on
//! [`audit::resources::LEARNING`]; refusal paths (422, 410) DO NOT
//! write audit rows so the trail records only consequential transitions.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::OptionalAuthUser;
use crate::db::{Learning, LearningError, LearningKind};
use crate::state::AppState;

/// A single pending CI-failure waiting on a learning capture. Wire shape
/// is the camelCase superset of `db::PendingCiFailure` plus the
/// authoring metadata the dashboard needs to render the row (agent +
/// plan + task + branch). All `Option<...>` fields collapse to `null` on
/// the wire so the frontend can treat missing data uniformly.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLearningRow {
    /// `ci_failure_events.id` — also the path parameter for the log
    /// drilldown endpoint.
    pub id: i64,
    pub agent_id: String,
    /// Current `agents.status` (running / starting / completed / failed /
    /// killed) so the panel can show the live state of the blocked
    /// agent. `null` when the agent row is gone (defensive — should not
    /// happen, since `record_ci_failure_observed` only inserts when a
    /// live agent owns the branch).
    pub agent_status: Option<String>,
    pub plan_name: String,
    pub task_number: Option<String>,
    pub branch: String,
    pub run_id: String,
    pub run_url: Option<String>,
    pub workflow: Option<String>,
    pub conclusion: Option<String>,
    pub failed_job: Option<String>,
    pub summary: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLearningsResponse {
    pub items: Vec<PendingLearningRow>,
}

/// GET /api/learnings/pending
///
/// Org-scoped list of `ci_failure_events` rows where `resolved_at IS
/// NULL`. Joined to `agents` so the dashboard can render the live
/// status of the blocked agent inline. Sorted oldest-first so the
/// dashboard's drilldown order matches the production order the agent
/// would see them in.
pub async fn list_pending(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let items = fetch_pending(&state, &org_id);
    Json(PendingLearningsResponse { items })
}

fn fetch_pending(state: &AppState, org_id: &str) -> Vec<PendingLearningRow> {
    let conn = state.db.lock().unwrap();
    let Ok(mut stmt) = conn.prepare(
        "SELECT e.id, e.agent_id, a.status, e.plan_name, e.task_number, e.branch, \
                e.run_id, e.run_url, e.workflow, e.conclusion, e.failed_job, e.summary, \
                e.observed_at \
           FROM ci_failure_events e \
           LEFT JOIN agents a ON a.id = e.agent_id \
          WHERE e.org_id = ?1 \
            AND e.resolved_at IS NULL \
          ORDER BY e.observed_at ASC, e.id ASC",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![org_id], |row| {
        Ok(PendingLearningRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            agent_status: row.get(2)?,
            plan_name: row.get(3)?,
            task_number: row.get(4)?,
            branch: row.get(5)?,
            run_id: row.get(6)?,
            run_url: row.get(7)?,
            workflow: row.get(8)?,
            conclusion: row.get(9)?,
            failed_job: row.get(10)?,
            summary: row.get(11)?,
            observed_at: row.get(12)?,
        })
    })
    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

/// GET /api/learnings/pending/{event_id}/log
///
/// Returns the cached-or-freshly-fetched failure log for the failing
/// GitHub Actions run referenced by a pending learning row, as
/// `text/plain`. Tail-trimmed to ~8 KB (the gh shell-out cap; the panel
/// renders the last 100 lines which is well inside that budget).
///
/// Pipeline:
/// 1. Look up the `ci_failure_events` row by id, gated on `org_id` so a
///    cross-org id leak returns 404.
/// 2. JOIN `ci_runs` on `(plan_name, run_id)` to recover the
///    autoincrement PK that [`crate::ci::fetch_failure_log`] uses as
///    its cache key.
/// 3. Delegate to that helper, which is already mode-aware (standalone
///    shells out locally, SaaS dispatches to the connected runner).
///
/// 404 paths: unknown event id, cross-org, no matching `ci_runs` row
/// (record was inserted by a path that didn't go through
/// `trigger_after_merge`), `gh` unavailable or run still pending.
pub async fn fetch_log(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(event_id): Path<i64>,
) -> Response {
    let org_id = auth.org_id().to_string();

    // Step 1+2: look up the event + its matching ci_runs.id in one SQL.
    // LEFT JOIN so a missing ci_runs row returns Some((..., None)) and
    // we can distinguish "event doesn't exist or wrong org" (404
    // event_not_found) from "event exists but no cached ci_runs row to
    // fetch from" (404 log_unavailable).
    let resolved: Option<(String, String, Option<i64>)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT e.plan_name, e.run_id, c.id \
               FROM ci_failure_events e \
               LEFT JOIN ci_runs c \
                      ON c.plan_name = e.plan_name AND c.run_id = e.run_id \
              WHERE e.id = ?1 AND e.org_id = ?2 \
              LIMIT 1",
            params![event_id, org_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .ok()
    };

    let Some((plan_name, run_id, ci_runs_id)) = resolved else {
        return not_found("event_not_found");
    };
    let Some(ci_runs_id) = ci_runs_id else {
        // Event exists but no ci_runs row to anchor the fetch. Surface
        // a distinct error code so the dashboard can copy-paste the
        // run_url instead of waiting on a refetch.
        return not_found_with_url(
            "log_unavailable",
            "no matching ci_runs row — open the run in GitHub directly",
        );
    };

    // Step 3: delegate. Mode-aware: standalone shells out locally,
    // SaaS dispatches to the connected runner.
    let _ = plan_name; // recovered for the JOIN, no longer needed here.
    let _ = run_id;
    match crate::ci::fetch_failure_log(
        &state.db,
        &state.runners,
        state.plans_dir.clone(),
        ci_runs_id,
    )
    .await
    {
        Some(log) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            log,
        )
            .into_response(),
        None => not_found_with_url(
            "log_unavailable",
            "failure log unavailable — run may still be pending, have no \
             remote, or `gh` is not installed",
        ),
    }
}

fn not_found(code: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": code})),
    )
        .into_response()
}

fn not_found_with_url(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": code, "message": message})),
    )
        .into_response()
}

// ── Org-shared learnings (Phase 2.2) ────────────────────────────────────────

/// Wrapper for `GET /api/learnings`. A bare array would force a future
/// envelope (paging, totals, kind facets) to be a breaking change;
/// `{ items: [...] }` keeps the surface forward-extensible the way the
/// pending list ([`PendingLearningsResponse`]) already is.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningsResponse {
    pub items: Vec<Learning>,
}

/// Query parameters for `GET /api/learnings`. All fields are optional;
/// unknown values for `kind` collapse to "no filter" rather than 400 —
/// the dashboard sends raw user input and a typo should yield an empty
/// list, not a fatal client error.
#[derive(Debug, Deserialize, Default)]
pub struct ListLearningsQuery {
    /// Restrict to one [`LearningKind`] wire form (`feedback` | `project`
    /// | `reference`). Unknown values are treated as "no filter".
    pub kind: Option<String>,
    /// Restrict to one category. Case-sensitive exact match — the
    /// dashboard's category dropdown is populated from the existing
    /// rows so casing matches by construction.
    pub category: Option<String>,
    /// Pass `?include=archived` to include soft-deleted rows in the
    /// response. Any other value (or absent) hides them. The archive
    /// view in the Activity tab is the only intended consumer.
    pub include: Option<String>,
}

/// `GET /api/learnings`
///
/// Org-scoped list of active learnings, newest first. Filterable by
/// `?kind=<wire>` and `?category=<exact>`. Pass `?include=archived` to
/// surface tombstones alongside active rows (the Activity tab's
/// archive view).
///
/// Unknown `kind` values fall through to "no filter" (the dashboard's
/// category dropdown is the only authority for valid values; a typo
/// returning an empty list would be misleading). Unknown `include`
/// values are treated as "active only" so a future `?include=foo`
/// doesn't accidentally surface archived rows.
pub async fn list_learnings(
    State(state): State<AppState>,
    Query(q): Query<ListLearningsQuery>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let kind_filter = q.kind.as_deref().and_then(LearningKind::parse);
    let include_archived = q.include.as_deref() == Some("archived");
    let mut items =
        crate::db::list_learnings_for_org(&state.db, &org_id, kind_filter, include_archived);
    if let Some(cat) = q.category.as_deref() {
        items.retain(|l| l.category == cat);
    }
    Json(LearningsResponse { items })
}

/// `GET /api/learnings/{id}`
///
/// Single learning with full `bodyMd`. Returns archived rows too so the
/// audit view can render them; callers that want only active should
/// check `archivedAt == null`.
///
/// Status codes:
/// - 200: row found in the caller's org.
/// - 404: id does not exist, OR belongs to a different org.
///
/// The cross-org case collapses to a generic 404 so the absence of a
/// row in this org can't be distinguished from "no such row anywhere"
/// — same convention `delete_credential` uses for its
/// `credential_not_found` branch.
pub async fn get_learning(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: OptionalAuthUser,
) -> Response {
    let org_id = auth.org_id().to_string();
    match crate::db::get_learning(&state.db, &id) {
        Ok(row) if row.org_id == org_id => Json(row).into_response(),
        Ok(_) => not_found("learning_not_found"),
        Err(LearningError::NotFound) => not_found("learning_not_found"),
        Err(LearningError::SlugCollision) => {
            // SlugCollision is structurally impossible on a read path
            // (the helper never inserts), but the From<rusqlite::Error>
            // shim could theoretically classify a stray constraint
            // error this way. Map to a 500 rather than 404 so a corrupt
            // row surfaces loudly instead of silently disappearing.
            eprintln!("[learnings] unexpected SlugCollision from get_learning({id})");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "lookup_failed" })),
            )
                .into_response()
        }
        Err(LearningError::Db(e)) => {
            eprintln!("[learnings] get_learning({id}) db error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "lookup_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Body for `POST /api/learnings/{id}/archive`. `reason` is required —
/// the audit trail's whole purpose is to record WHY a once-canonical
/// lesson is no longer active, so we reject empty/missing here rather
/// than letting the DB write a NULL.
#[derive(Debug, Deserialize)]
pub struct ArchiveLearningBody {
    pub reason: Option<String>,
}

/// `POST /api/learnings/{id}/archive`
///
/// Soft-delete a learning with an operator-supplied `reason`. The row
/// stays in the DB; only `archived_at` / `archived_reason` flip. List
/// endpoints hide archived rows by default; pass `?include=archived` to
/// see them.
///
/// Status codes:
/// - 200: row archived. Body: `{ ok: true, id, archivedAt }`.
/// - 404: id not found in caller's org (same 404-on-cross-org masking
///   convention as `get_learning`).
/// - 410: already archived. Body includes the original `archivedAt` +
///   `archivedReason` so the operator can see what happened.
/// - 422: `reason` missing / empty after trim.
///
/// Refusal paths (404 / 410 / 422) DO NOT write an audit row — the
/// trail records only consequential transitions.
pub async fn archive_learning(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: OptionalAuthUser,
    Json(body): Json<ArchiveLearningBody>,
) -> Response {
    let reason_raw = body.reason.unwrap_or_default();
    let reason = reason_raw.trim();
    if reason.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "reason_required",
                "message": "archive requires a non-empty `reason`",
            })),
        )
            .into_response();
    }

    // Pre-fetch the row scoped to the caller's org. This serves three
    // purposes: (a) 404 on unknown / cross-org id without giving the
    // archive helper a chance to flip the wrong row, (b) capture the
    // kind/slug/category for the audit diff, (c) distinguish "already
    // archived" (410) from "freshly archived" (200) without relying on
    // the helper's bool return.
    let org_id = auth.org_id().to_string();
    let existing = match crate::db::get_learning(&state.db, &id) {
        Ok(row) if row.org_id == org_id => row,
        Ok(_) => return not_found("learning_not_found"),
        Err(LearningError::NotFound) => return not_found("learning_not_found"),
        Err(e) => {
            eprintln!("[learnings] archive lookup({id}) failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "archive_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    if existing.archived_at.is_some() {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "already_archived",
                "message": "learning is already archived",
                "id": existing.id,
                "archivedAt": existing.archived_at,
                "archivedReason": existing.archived_reason,
            })),
        )
            .into_response();
    }

    // Flip the row. The helper is itself idempotent (returns Ok(false)
    // if already archived), but we already handled that case above; a
    // concurrent caller racing us through the gap converges on the
    // first-writer-wins outcome — both operators see the same final
    // state.
    let flipped = match crate::db::archive_learning(&state.db, &id, reason) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[learnings] archive_learning({id}) failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "archive_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    if !flipped {
        // Race: another caller archived between our SELECT and UPDATE.
        // Re-fetch so the response carries their archivedAt/reason, not
        // ours. Matches the 410 convergence above.
        let now_archived = crate::db::get_learning(&state.db, &id).ok();
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "already_archived",
                "message": "learning was archived by another caller",
                "id": id,
                "archivedAt": now_archived.as_ref().and_then(|r| r.archived_at.clone()),
                "archivedReason": now_archived.as_ref().and_then(|r| r.archived_reason.clone()),
            })),
        )
            .into_response();
    }

    // Re-read so the response (and the audit diff) carries the actual
    // archivedAt the DB stamped, not our local clock.
    let archived_at = crate::db::get_learning(&state.db, &id)
        .ok()
        .and_then(|r| r.archived_at);

    let diff = serde_json::json!({
        "learning_id": existing.id,
        "kind": existing.kind,
        "slug": existing.slug,
        "category": existing.category,
        "reason": reason,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            audit::actions::LEARNING_ARCHIVE,
            audit::resources::LEARNING,
            Some(&existing.id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "ok": true,
        "id": existing.id,
        "archivedAt": archived_at,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saas::runner_ws::{RunnerRegistry, new_runner_registry};
    use rusqlite::Connection;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Build a minimal `AppState` whose DB carries just the tables this
    /// module touches. Mirrors the pattern other `api/*` tests use
    /// (`runners::tests::test_app_state`). The registry/runners fields
    /// are unused on the read paths exercised here.
    fn test_state() -> AppState {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ci_failure_events (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id     TEXT    NOT NULL,
                plan_name    TEXT    NOT NULL,
                task_number  TEXT,
                branch       TEXT    NOT NULL,
                run_id       TEXT    NOT NULL,
                run_url      TEXT,
                workflow     TEXT,
                conclusion   TEXT,
                failed_job   TEXT,
                summary      TEXT,
                org_id       TEXT    NOT NULL DEFAULT 'default-org',
                observed_at  TEXT    NOT NULL DEFAULT (datetime('now')),
                resolved_at  TEXT
             );
             CREATE TABLE agents (
                id              TEXT PRIMARY KEY,
                status          TEXT,
                started_at      TEXT DEFAULT (datetime('now'))
             );
             CREATE TABLE ci_runs (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                plan_name    TEXT NOT NULL,
                task_number  TEXT NOT NULL,
                run_id       TEXT,
                failure_log  TEXT
             );
             -- Org-shared learnings (Phase 2.2). Schema mirrors db.rs migrate().
             CREATE TABLE learnings (
                id                 TEXT PRIMARY KEY,
                org_id             TEXT NOT NULL DEFAULT 'default-org',
                kind               TEXT NOT NULL,
                category           TEXT NOT NULL,
                slug               TEXT NOT NULL,
                body_md            TEXT NOT NULL,
                source_agent_id    TEXT,
                source_ci_run_id   INTEGER,
                created_at         TEXT NOT NULL DEFAULT (datetime('now')),
                archived_at        TEXT,
                archived_reason    TEXT
             );
             CREATE UNIQUE INDEX idx_learnings_org_kind_slug
                ON learnings(org_id, kind, slug);
             -- audit_logs: archive endpoint writes here.
             CREATE TABLE audit_logs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                org_id        TEXT NOT NULL DEFAULT 'default-org',
                user_id       TEXT,
                user_email    TEXT,
                action        TEXT NOT NULL,
                resource_type TEXT NOT NULL,
                resource_id   TEXT,
                diff          TEXT,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .unwrap();
        let db: crate::db::Db = Arc::new(std::sync::Mutex::new(conn));
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans-learnings");
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/tmp/branchwork-test-claude-learnings"),
            0,
            true,
        );
        let runners: RunnerRegistry = new_runner_registry();
        AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings-learnings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(HashSet::new())),
            started_at: std::time::Instant::now(),
        }
    }

    #[allow(clippy::too_many_arguments)] // every arg pins a wire-shape column the test asserts on.
    fn seed_event(
        state: &AppState,
        agent_id: &str,
        agent_status: Option<&str>,
        plan: &str,
        task: &str,
        run_id: &str,
        org_id: &str,
        resolved: Option<&str>,
    ) -> i64 {
        let conn = state.db.lock().unwrap();
        if let Some(status) = agent_status {
            conn.execute(
                "INSERT OR REPLACE INTO agents (id, status) VALUES (?1, ?2)",
                params![agent_id, status],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO ci_failure_events \
                (agent_id, plan_name, task_number, branch, run_id, run_url, \
                 workflow, conclusion, failed_job, summary, org_id, observed_at, resolved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, datetime('now'), ?11)",
            params![
                agent_id,
                plan,
                task,
                format!("branchwork/{plan}/{task}"),
                run_id,
                format!("https://example.test/runs/{run_id}"),
                "tests",
                "failure",
                format!("{} workflow failed", "tests"),
                org_id,
                resolved,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[tokio::test]
    async fn list_pending_returns_only_unresolved_for_org() {
        let state = test_state();
        // Two pending in default-org, one resolved (excluded), one in
        // a different org (excluded).
        let id_one = seed_event(
            &state,
            "agent-1",
            Some("running"),
            "plan-a",
            "1.4",
            "run-1",
            "default-org",
            None,
        );
        let id_two = seed_event(
            &state,
            "agent-2",
            Some("running"),
            "plan-b",
            "2.1",
            "run-2",
            "default-org",
            None,
        );
        let _resolved = seed_event(
            &state,
            "agent-3",
            Some("completed"),
            "plan-a",
            "1.5",
            "run-3",
            "default-org",
            Some("2026-01-01 00:00:00"),
        );
        let _other_org = seed_event(
            &state,
            "agent-4",
            Some("running"),
            "plan-c",
            "1.1",
            "run-4",
            "other-org",
            None,
        );

        let items = fetch_pending(&state, "default-org");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, id_one);
        assert_eq!(items[0].agent_status.as_deref(), Some("running"));
        assert_eq!(items[0].plan_name, "plan-a");
        assert_eq!(items[0].task_number.as_deref(), Some("1.4"));
        assert_eq!(items[1].id, id_two);
    }

    #[tokio::test]
    async fn list_pending_is_empty_when_all_resolved() {
        let state = test_state();
        seed_event(
            &state,
            "agent-1",
            Some("running"),
            "p",
            "1.1",
            "r",
            "default-org",
            Some("2026-01-01 00:00:00"),
        );
        assert!(fetch_pending(&state, "default-org").is_empty());
    }

    #[tokio::test]
    async fn list_pending_surfaces_null_agent_status_when_agent_row_missing() {
        // Defensive — record_ci_failure_observed only inserts when a
        // live agent owns the branch, so this is unreachable through
        // the production path, but the LEFT JOIN must not drop the row.
        let state = test_state();
        seed_event(
            &state,
            "ghost-agent",
            None,
            "p",
            "1.1",
            "r",
            "default-org",
            None,
        );
        let items = fetch_pending(&state, "default-org");
        assert_eq!(items.len(), 1);
        assert!(items[0].agent_status.is_none());
    }

    #[tokio::test]
    async fn list_pending_orders_oldest_first() {
        let state = test_state();
        // Insert in reverse order; observed_at defaults to NOW so
        // direct INSERTs in the same statement order match insertion.
        // The ORDER BY tie-breaker is `id ASC`, so we rely on that
        // here (millisecond precision in datetime('now') ties at this
        // resolution).
        let a = seed_event(
            &state,
            "a",
            Some("running"),
            "p",
            "1.1",
            "r1",
            "default-org",
            None,
        );
        let b = seed_event(
            &state,
            "b",
            Some("running"),
            "p",
            "1.2",
            "r2",
            "default-org",
            None,
        );
        let items = fetch_pending(&state, "default-org");
        assert_eq!(items[0].id, a);
        assert_eq!(items[1].id, b);
    }

    // ── Phase 2.2 — list / get / archive ─────────────────────────────────

    use crate::auth::AuthUser;
    use crate::db::{LearningInput, archive_learning as db_archive, create_learning};

    /// Build a `LearningInput` quickly. Caller picks id+kind+slug+category+org;
    /// body is fixed so tests don't have to invent prose.
    fn input<'a>(
        id: &'a str,
        org_id: &'a str,
        kind: LearningKind,
        category: &'a str,
        slug: &'a str,
    ) -> LearningInput<'a> {
        LearningInput {
            id,
            org_id,
            kind,
            category,
            slug,
            body_md: "**Why:** test\n\n**How to apply:** in tests.",
            source_agent_id: None,
            source_ci_run_id: None,
        }
    }

    /// Decode an `IntoResponse`-returning future down to (status, json).
    /// Pattern mirrors what other api/* tests do — works for both `impl
    /// IntoResponse` and `Response` returns.
    async fn drive<R: axum::response::IntoResponse>(r: R) -> (u16, serde_json::Value) {
        let resp = r.into_response();
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
        };
        (status, value)
    }

    fn anon() -> OptionalAuthUser {
        OptionalAuthUser(None)
    }

    fn authed(user_id: &str, email: &str, org_id: &str) -> OptionalAuthUser {
        OptionalAuthUser(Some(AuthUser {
            id: user_id.to_string(),
            email: email.to_string(),
            org_id: org_id.to_string(),
            org_role: "member".to_string(),
        }))
    }

    #[tokio::test]
    async fn list_returns_active_rows_only_by_default() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Project, "deploy", "b"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "outdated").unwrap();

        let (status, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery::default()),
                anon(),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "only active rows by default: {body}");
        assert_eq!(items[0]["id"], "L-2");
    }

    #[tokio::test]
    async fn list_with_include_archived_surfaces_tombstones() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Feedback, "testing", "b"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "outdated").unwrap();

        let (status, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    include: Some("archived".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "include=archived surfaces both: {body}");
        let archived: Vec<&str> = items
            .iter()
            .filter(|l| !l["archivedAt"].is_null())
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(archived, vec!["L-1"]);
    }

    #[tokio::test]
    async fn list_filters_by_kind_and_category() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Feedback, "deploy", "b"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-3", "default-org", LearningKind::Project, "testing", "c"),
        )
        .unwrap();

        // kind=feedback only.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("feedback".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"L-1"));
        assert!(ids.contains(&"L-2"));

        // category=testing only.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    category: Some("testing".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"L-1"));
        assert!(ids.contains(&"L-3"));

        // kind=feedback + category=testing → just L-1.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("feedback".to_string()),
                    category: Some("testing".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["L-1"]);
    }

    #[tokio::test]
    async fn list_unknown_kind_falls_through_to_no_filter() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();

        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("user".to_string()), // not a valid wire form
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        // Unknown kind → no filter → row still visible. The deliberate
        // permissive behaviour is documented inline.
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_is_org_scoped() {
        let state = test_state();
        // Seed a second org's organisations row so the FK is happy
        // (though we don't actually have an organizations table in the
        // minimal test schema — defer to default-org for the seed and
        // use the column directly).
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "org-2", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery::default()),
                authed("u-2", "u2@example.com", "org-2"),
            )
            .await,
        )
        .await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "L-2");
        assert_eq!(items[0]["orgId"], "org-2");
    }

    #[tokio::test]
    async fn get_returns_full_body_for_active_row() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();

        let (status, body) =
            drive(get_learning(State(state.clone()), Path("L-1".to_string()), anon()).await).await;
        assert_eq!(status, 200);
        assert_eq!(body["id"], "L-1");
        assert!(body["bodyMd"].as_str().unwrap().starts_with("**Why:**"));
        assert!(body["archivedAt"].is_null());
    }

    #[tokio::test]
    async fn get_returns_archived_rows_too() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "superseded").unwrap();

        let (status, body) =
            drive(get_learning(State(state.clone()), Path("L-1".to_string()), anon()).await).await;
        assert_eq!(status, 200);
        assert_eq!(body["archivedReason"], "superseded");
        assert!(!body["archivedAt"].is_null());
    }

    #[tokio::test]
    async fn get_404s_on_unknown_id() {
        let state = test_state();
        let (status, body) =
            drive(get_learning(State(state.clone()), Path("nope".to_string()), anon()).await).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "learning_not_found");
    }

    #[tokio::test]
    async fn get_404s_on_cross_org() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "org-1", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            get_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-2", "u2@example.com", "org-2"),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404, "cross-org reads must 404, not 403: {body}");
    }

    #[tokio::test]
    async fn archive_happy_path_flips_row_and_writes_audit() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-1", "alice@example.com", "default-org"),
                Json(ArchiveLearningBody {
                    reason: Some("superseded by ADR 0009".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["id"], "L-1");
        assert!(!body["archivedAt"].is_null());

        // Audit row landed with the right shape.
        let (action, resource, resource_id, diff_str): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT action, resource_type, resource_id, diff \
                   FROM audit_logs WHERE resource_id = ?1 \
                  ORDER BY id DESC LIMIT 1",
                params!["L-1"],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(action, "learning.archive");
        assert_eq!(resource, "learning");
        assert_eq!(resource_id.as_deref(), Some("L-1"));
        let diff: serde_json::Value = serde_json::from_str(diff_str.as_deref().unwrap()).unwrap();
        assert_eq!(diff["learning_id"], "L-1");
        assert_eq!(diff["kind"], "feedback");
        assert_eq!(diff["slug"], "no-mocks");
        assert_eq!(diff["category"], "testing");
        assert_eq!(diff["reason"], "superseded by ADR 0009");
    }

    #[tokio::test]
    async fn archive_422s_on_missing_reason() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody { reason: None }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "reason_required");

        // No audit row, no row state change.
        let audit_count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(audit_count, 0);
        let got = crate::db::get_learning(&state.db, "L-1").unwrap();
        assert!(got.archived_at.is_none());
    }

    #[tokio::test]
    async fn archive_422s_on_whitespace_only_reason() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("   \n\t  ".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "reason_required");
    }

    #[tokio::test]
    async fn archive_410s_on_already_archived() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "first-reason").unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("second-reason".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 410);
        assert_eq!(body["error"], "already_archived");
        // Response carries the ORIGINAL archive metadata, not the new
        // attempt's reason.
        assert_eq!(body["archivedReason"], "first-reason");

        // No new audit row from the refusal.
        let audit_count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(audit_count, 0);
    }

    #[tokio::test]
    async fn archive_404s_on_unknown_id() {
        let state = test_state();
        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("nope".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("doesn't matter".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "learning_not_found");
    }

    #[tokio::test]
    async fn archive_404s_on_cross_org() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "org-1", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, _body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-2", "u2@example.com", "org-2"),
                Json(ArchiveLearningBody {
                    reason: Some("wrong-org".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404, "cross-org archive must 404");

        // Row in org-1 must not have been flipped.
        let got = crate::db::get_learning(&state.db, "L-1").unwrap();
        assert!(got.archived_at.is_none(), "cross-org caller cannot flip");
    }

    #[tokio::test]
    async fn archive_records_user_attribution_when_authed() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let _ = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-7", "bob@example.com", "default-org"),
                Json(ArchiveLearningBody {
                    reason: Some("ok".to_string()),
                }),
            )
            .await,
        )
        .await;

        let (user_id, user_email): (Option<String>, Option<String>) = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT user_id, user_email FROM audit_logs WHERE resource_id = ?1",
                params!["L-1"],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(user_id.as_deref(), Some("u-7"));
        assert_eq!(user_email.as_deref(), Some("bob@example.com"));
    }
}
