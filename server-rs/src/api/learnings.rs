//! `GET /api/learnings/pending` and `GET /api/learnings/pending/{id}/log`
//! — dashboard surface for the Phase 1 pending-learning queue.
//!
//! Backed by `ci_failure_events` (Task 1.1) filtered through the same
//! `resolved_at IS NULL` predicate the gate (Task 1.2) and the
//! `capture_learning` MCP tool (Task 1.3) consume. Org-scoped via
//! `OptionalAuthUser::org_id()` — the dashboard's `LearningsDuePanel`
//! reads `items[]` and only renders when the queue is non-empty.
//!
//! The list endpoint joins `agents` to surface the agent's current status
//! so the panel can show whether the blocked agent is still running or
//! has gone offline. The log endpoint defers to
//! [`crate::ci::fetch_failure_log`] after joining `ci_runs` on
//! `(plan_name, run_id)` to recover the cache key — both standalone and
//! SaaS dispatch paths flow through that helper, so the dashboard does
//! not need a separate runner-aware codepath here.

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::Serialize;

use crate::auth::OptionalAuthUser;
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
}
