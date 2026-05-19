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
use crate::saas::outbox;
use crate::saas::runner_protocol::{Envelope, WireMessage};
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
        "SELECT 1 FROM runners WHERE id = ?1 AND org_id = ?2 AND removed_at IS NULL",
        params![runner_id, org_id],
        |_row| Ok(()),
    )
    .is_ok()
}

/// Body for `POST /api/runners/{id}/shutdown`. Both fields are optional —
/// the dashboard sends an explicit confirmation flag for in-flight agents
/// (the modal's "I understand" checkbox) and a free-form reason that is
/// echoed back to the runner for the operator's log.
#[derive(Deserialize)]
pub struct RunnerShutdownBody {
    /// Free-form reason the operator typed (e.g. "host upgrade"); echoed
    /// in the runner's `[runner] shutdown requested by SaaS:` log line.
    /// `None` is fine — the runner just logs `(no reason given)`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Set by the dashboard after the user checks the "I understand this
    /// stops N in-flight agent(s)" box. The server still inspects the
    /// agents row count itself; this flag is documentation, not auth.
    #[serde(default, rename = "confirmInFlight")]
    pub _confirm_in_flight: Option<bool>,
}

/// `POST /api/runners/{id}/shutdown` — ask the runner to drain and exit.
///
/// Reliable delivery: enqueued in `inbox_pending` so a runner that's
/// transiently disconnected still picks up the request on its next
/// reconnect. The runner is free to ignore (or be wedged); revoking the
/// token via `DELETE /api/runners/{id}` is the next escalation.
///
/// Response: `200 { queued: true, inFlightAgents: [...] }` on success;
/// `404` when the runner doesn't belong to the caller's org or has been
/// soft-deleted already.
pub async fn shutdown_runner(
    State(state): State<AppState>,
    Path(runner_id): Path<String>,
    user: AuthUser,
    Json(body): Json<RunnerShutdownBody>,
) -> impl IntoResponse {
    if !runner_belongs_to_org(&state, &runner_id, &user.org_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "runner_not_found" })),
        )
            .into_response();
    }

    // Snapshot in-flight agents — both for the audit row and the response
    // body. The dashboard may surface this as a follow-up notification if
    // any of these agents fail to drain cleanly. Filter to runner-owned
    // (mode='remote') AND status in {starting,running} to match the
    // dashboard's "in-flight" semantics.
    let in_flight: Vec<String> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id FROM agents \
                 WHERE org_id = ?1 AND mode = 'remote' \
                   AND status IN ('starting', 'running')",
            )
            .expect("prepare agents query");
        stmt.query_map(params![user.org_id], |row| row.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };

    let message = WireMessage::ShutdownRequest {
        reason: body.reason.clone(),
    };
    let payload = serde_json::to_string(&message).unwrap_or_default();
    let seq = {
        let conn = state.db.lock().unwrap();
        outbox::enqueue_server_command(&conn, &runner_id, message.event_type(), &payload)
    };
    let envelope = Envelope::reliable("server".to_string(), seq, message);
    let env_json = serde_json::to_string(&envelope).unwrap_or_default();

    // Push immediately if the runner is currently online. The outbox
    // covers reconnect — push-immediately covers "online right now" so
    // the operator sees a fast response in the common case.
    if let Some(runner) = state.runners.lock().await.get(&runner_id) {
        let _ = runner.command_tx.send(env_json);
    }

    // Audit. Use the new RUNNER resource type so the activity-log filter
    // can group shutdown + revoke entries together.
    let diff = serde_json::json!({
        "runner_id": runner_id,
        "reason": body.reason,
        "in_flight_agents": in_flight,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(&user.id),
            Some(&user.email),
            audit::actions::RUNNER_SHUTDOWN_REQUESTED,
            audit::resources::RUNNER,
            Some(&runner_id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "queued": true,
        "seq": seq,
        "inFlightAgents": in_flight,
    }))
    .into_response()
}

/// `DELETE /api/runners/{id}` — revoke the runner.
///
/// Server-side only. Two SQL operations under a single lock:
///  - delete every `runner_tokens` row whose `claimed_runner_id = ?1` so
///    the runner's persisted token can no longer authenticate; the next
///    WS upgrade attempt fails with 401 in `validate_runner_token`.
///  - set `runners.removed_at = datetime('now')` so `list_runners`
///    filters the row out. We do NOT hard-delete: the `agents` rows that
///    reference this runner stay resolvable for historic terminal logs
///    and audit links.
///
/// Response: `200 { revoked: true, tokensRevoked: <usize> }`. `404` when
/// the runner doesn't belong to the caller's org or has already been
/// revoked.
///
/// The current WS connection (if any) stays alive until the runner
/// closes it or the keepalive times out; the dashboard documents this in
/// the confirmation modal so the user doesn't expect a kill -9.
pub async fn delete_runner(
    State(state): State<AppState>,
    Path(runner_id): Path<String>,
    user: AuthUser,
) -> impl IntoResponse {
    let runner_name: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT name FROM runners WHERE id = ?1 AND org_id = ?2 AND removed_at IS NULL",
            params![runner_id, user.org_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };

    // Membership + double-revoke check rolled in: if the SELECT above came
    // back empty, the row either doesn't exist, isn't ours, or was already
    // revoked. All three map to 404 — the dashboard won't render a Revoke
    // button against a row in any of those states.
    if !runner_belongs_to_org(&state, &runner_id, &user.org_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "runner_not_found" })),
        )
            .into_response();
    }

    let tokens_revoked: usize = {
        let conn = state.db.lock().unwrap();
        // Delete tokens claimed by this runner. There may be 0 (token was
        // never claimed because the runner never connected) or 1+ (an
        // operator may have re-issued a token without revoking the old
        // one). Either way: every row that points at this runner_id goes.
        let revoked = conn
            .execute(
                "DELETE FROM runner_tokens WHERE claimed_runner_id = ?1 AND org_id = ?2",
                params![runner_id, user.org_id],
            )
            .unwrap_or(0);
        // Soft-delete the runner row.
        conn.execute(
            "UPDATE runners SET removed_at = datetime('now') WHERE id = ?1 AND org_id = ?2",
            params![runner_id, user.org_id],
        )
        .ok();
        revoked
    };

    // Drop the in-memory ConnectedRunner handle so future commands fail
    // fast instead of being silently sent to a now-doomed WS. We do NOT
    // close the WS here — the runner's read loop will see the next ACK
    // attempt fail (or the keepalive will fire) and shut itself down.
    state.runners.lock().await.remove(&runner_id);

    let diff = serde_json::json!({
        "runner_id": runner_id,
        "runner_name": runner_name,
        "tokens_revoked": tokens_revoked,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(&user.id),
            Some(&user.email),
            audit::actions::RUNNER_REVOKED,
            audit::resources::RUNNER,
            Some(&runner_id),
            Some(&diff),
        );
    }

    // Dashboard listens for `runner_disconnected` via the WS broadcast;
    // synthesize one here so the runner-store flips to offline / disappears
    // immediately, regardless of whether the runner WS is still up.
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "runner_disconnected",
        serde_json::json!({ "runner_id": runner_id, "reason": "revoked" }),
    );

    Json(serde_json::json!({
        "revoked": true,
        "tokensRevoked": tokens_revoked,
    }))
    .into_response()
}

/// Body for `POST /api/runners/{id}/version-mismatch-override`.
#[derive(Deserialize)]
pub struct VersionMismatchOverrideBody {
    /// `true` ⇒ lift the dispatch block for this runner (operator opted
    /// into "I know what I'm doing"). `false` ⇒ clear the override so
    /// the block re-engages on the next dispatch.
    pub r#override: bool,
}

/// `POST /api/runners/{id}/version-mismatch-override` — operator override
/// that lifts the version-mismatch dispatch block on this runner.
///
/// When `runner.versionSeverity == "red"`, `spawn_ops::start_agent_via_runner`
/// refuses to send `StartAgent` and pauses the plan with
/// `paused_reason='runner_version_mismatch'`. Setting this override flips
/// the block off; the operator has explicitly accepted the protocol-drift
/// risk. The override is per-runner (not per-org) so the operator can
/// selectively enable one specific runner without weakening the rule
/// globally.
///
/// Response: `200 { runnerId, override }`. `404` when the runner does
/// not belong to the caller's org.
pub async fn set_version_mismatch_override(
    State(state): State<AppState>,
    Path(runner_id): Path<String>,
    user: AuthUser,
    Json(body): Json<VersionMismatchOverrideBody>,
) -> impl IntoResponse {
    if !runner_belongs_to_org(&state, &runner_id, &user.org_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "runner_not_found" })),
        )
            .into_response();
    }

    // Capture the current runner_version for the audit row so a future
    // operator can see what version was running at the time of the
    // override — useful when investigating why a misbehaving runner got
    // green-lit.
    let runner_version: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT version FROM runners WHERE id = ?1",
            params![runner_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };

    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE runners SET version_mismatch_override = ?1 WHERE id = ?2",
            params![if body.r#override { 1 } else { 0 }, runner_id],
        )
        .ok();
    }

    let server_version = env!("CARGO_PKG_VERSION");
    let diff = serde_json::json!({
        "runner_id": runner_id,
        "override": body.r#override,
        "runner_version": runner_version,
        "server_version": server_version,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(&user.id),
            Some(&user.email),
            audit::actions::RUNNER_VERSION_MISMATCH_OVERRIDE,
            audit::resources::RUNNER,
            Some(&runner_id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "runnerId": runner_id,
        "override": body.r#override,
    }))
    .into_response()
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

    // ── Shutdown / revoke endpoints (Task 11.2) ──────────────────────────────

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use rusqlite::params;
    use tokio::sync::{Mutex, mpsc, oneshot};

    use crate::auth::AuthUser;
    use crate::saas::runner_protocol::Envelope;
    use crate::saas::runner_ws::{
        ConnectedRunner, RunnerRegistry, RunnerResponse, new_runner_registry,
    };

    fn full_db() -> (crate::db::Db, tempfile::TempDir) {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        (db, tempdir)
    }

    fn seed_runner_with_token(
        db: &crate::db::Db,
        runner_id: &str,
        org_id: &str,
        user_id: &str,
        token_hash: &str,
    ) {
        let conn = db.lock().unwrap();
        // The migration auto-creates a default-user inside default-org. For
        // any other org we'd seed our own user, but every test here uses
        // default-org to keep the FK chain happy.
        conn.execute(
            "INSERT OR IGNORE INTO users (id, email, password_hash) \
             VALUES (?1, 'tester@example.com', 'x')",
            params![user_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
             VALUES (?1, 'test', ?2, 'online', datetime('now'))",
            params![runner_id, org_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runner_tokens \
             (token_hash, runner_name, org_id, created_by, claimed_runner_id) \
             VALUES (?1, 'test', ?2, ?3, ?4)",
            params![token_hash, org_id, user_id, runner_id],
        )
        .unwrap();
    }

    async fn install_capturing_runner(
        registry: &RunnerRegistry,
        runner_id: &str,
    ) -> mpsc::UnboundedReceiver<String> {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (cmd_tx, rx) = mpsc::unbounded_channel::<String>();
        registry.lock().await.insert(
            runner_id.to_string(),
            ConnectedRunner {
                command_tx: cmd_tx,
                hostname: None,
                version: None,
                drivers: None,
                pending,
                server_url: "http://localhost:3100".to_string(),
            },
        );
        rx
    }

    fn test_app_state(db: crate::db::Db, runners: RunnerRegistry) -> AppState {
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans-runners");
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/tmp/branchwork-test-claude-runners"),
            0,
            true,
        );
        AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings-runners.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
        }
    }

    fn auth_user(org_id: &str, user_id: &str) -> AuthUser {
        AuthUser {
            id: user_id.to_string(),
            email: "tester@example.com".to_string(),
            org_id: org_id.to_string(),
            org_role: "owner".to_string(),
        }
    }

    /// Drain the response body into a JSON Value so the assertions can poke
    /// at fields without re-deriving the response type.
    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn shutdown_endpoint_enqueues_reliable_command_and_pushes_to_runner() {
        let (db, _td) = full_db();
        let user_id = "tester-1";
        seed_runner_with_token(&db, "runner-shut-1", "default-org", user_id, "hash-1");

        let runners = new_runner_registry();
        let mut envelope_rx = install_capturing_runner(&runners, "runner-shut-1").await;
        let state = test_app_state(db.clone(), runners);

        let resp = shutdown_runner(
            axum::extract::State(state.clone()),
            axum::extract::Path("runner-shut-1".to_string()),
            auth_user("default-org", user_id),
            axum::Json(RunnerShutdownBody {
                reason: Some("host upgrade".into()),
                _confirm_in_flight: Some(true),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["queued"], serde_json::json!(true));
        assert!(json["seq"].is_number(), "seq missing in {json}");

        // Online runner must have received the envelope on its command_tx.
        let raw = tokio::time::timeout(std::time::Duration::from_millis(500), envelope_rx.recv())
            .await
            .expect("envelope arrives within 500ms")
            .expect("channel still open");
        let env: Envelope = serde_json::from_str(&raw).unwrap();
        assert_eq!(env.message.event_type(), "shutdown_request");
        match env.message {
            WireMessage::ShutdownRequest { reason } => {
                assert_eq!(reason.as_deref(), Some("host upgrade"));
            }
            other => panic!("expected ShutdownRequest, got {other:?}"),
        }

        // Reliable: must have hit the outbox so a flap also picks it up.
        let pending_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM inbox_pending \
                 WHERE runner_id = 'runner-shut-1' AND command_type = 'shutdown_request'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(pending_count, 1);
    }

    #[tokio::test]
    async fn shutdown_endpoint_404s_for_unknown_runner() {
        let (db, _td) = full_db();
        let runners = new_runner_registry();
        let state = test_app_state(db, runners);

        let resp = shutdown_runner(
            axum::extract::State(state),
            axum::extract::Path("never-existed".to_string()),
            auth_user("default-org", "default-user"),
            axum::Json(RunnerShutdownBody {
                reason: None,
                _confirm_in_flight: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_endpoint_revokes_tokens_and_soft_deletes_runner() {
        let (db, _td) = full_db();
        let user_id = "tester-2";
        seed_runner_with_token(&db, "runner-revoke-1", "default-org", user_id, "hash-r1");

        let runners = new_runner_registry();
        let _envelope_rx = install_capturing_runner(&runners, "runner-revoke-1").await;
        let state = test_app_state(db.clone(), runners.clone());

        // Precondition: token present, runner not soft-deleted.
        let tokens_pre: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM runner_tokens WHERE claimed_runner_id = ?1",
                params!["runner-revoke-1"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(tokens_pre, 1);

        let resp = delete_runner(
            axum::extract::State(state.clone()),
            axum::extract::Path("runner-revoke-1".to_string()),
            auth_user("default-org", user_id),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["revoked"], serde_json::json!(true));
        assert_eq!(json["tokensRevoked"], serde_json::json!(1));

        // Tokens are gone — next reconnect with the now-revoked token will
        // fail validate_runner_token (NULL row return) at the WS upgrade.
        let tokens_post: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM runner_tokens WHERE claimed_runner_id = ?1",
                params!["runner-revoke-1"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(tokens_post, 0);

        // Runner row soft-deleted via removed_at — list_runners filters on it.
        let removed_at: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT removed_at FROM runners WHERE id = ?1",
                params!["runner-revoke-1"],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        assert!(removed_at.is_some(), "removed_at must be set");

        // In-memory ConnectedRunner handle must be dropped so future
        // commands fail fast instead of routing to a doomed WS.
        assert!(
            !runners.lock().await.contains_key("runner-revoke-1"),
            "ConnectedRunner handle must be removed on revoke"
        );
    }

    #[tokio::test]
    async fn delete_endpoint_404s_on_double_revoke() {
        let (db, _td) = full_db();
        let user_id = "tester-3";
        seed_runner_with_token(&db, "runner-double", "default-org", user_id, "hash-double");

        let runners = new_runner_registry();
        let state = test_app_state(db, runners);

        let r1 = delete_runner(
            axum::extract::State(state.clone()),
            axum::extract::Path("runner-double".to_string()),
            auth_user("default-org", user_id),
        )
        .await
        .into_response();
        assert_eq!(r1.status(), StatusCode::OK);

        let r2 = delete_runner(
            axum::extract::State(state),
            axum::extract::Path("runner-double".to_string()),
            auth_user("default-org", user_id),
        )
        .await
        .into_response();
        assert_eq!(
            r2.status(),
            StatusCode::NOT_FOUND,
            "second revoke must 404 because removed_at is set"
        );
    }

    // ── T1.3: version-mismatch override endpoint ─────────────────────────

    #[tokio::test]
    async fn version_mismatch_override_writes_column_and_audits() {
        let (db, _td) = full_db();
        let user_id = "tester-vmo";
        seed_runner_with_token(&db, "runner-vmo", "default-org", user_id, "hash-vmo");
        let runners = new_runner_registry();
        let state = test_app_state(db.clone(), runners);

        let resp = set_version_mismatch_override(
            axum::extract::State(state.clone()),
            axum::extract::Path("runner-vmo".to_string()),
            auth_user("default-org", user_id),
            axum::Json(VersionMismatchOverrideBody { r#override: true }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["override"], serde_json::json!(true));
        assert_eq!(json["runnerId"], serde_json::json!("runner-vmo"));

        // Column is set on the runners row.
        let stored: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT version_mismatch_override FROM runners WHERE id = ?1",
                params!["runner-vmo"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(stored, 1);

        // Audit row exists with the matching action.
        let action_count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE action = ?1 AND resource_id = ?2",
                params![
                    audit::actions::RUNNER_VERSION_MISMATCH_OVERRIDE,
                    "runner-vmo"
                ],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(action_count, 1);
    }

    #[tokio::test]
    async fn version_mismatch_override_can_clear_back_to_zero() {
        let (db, _td) = full_db();
        let user_id = "tester-vmo-2";
        seed_runner_with_token(&db, "runner-vmo-2", "default-org", user_id, "hash-vmo-2");
        // Pre-set the override to 1 so we can verify clearing works.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE runners SET version_mismatch_override = 1 WHERE id = ?1",
                params!["runner-vmo-2"],
            )
            .unwrap();
        }
        let runners = new_runner_registry();
        let state = test_app_state(db.clone(), runners);

        let resp = set_version_mismatch_override(
            axum::extract::State(state),
            axum::extract::Path("runner-vmo-2".to_string()),
            auth_user("default-org", user_id),
            axum::Json(VersionMismatchOverrideBody { r#override: false }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let stored: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT version_mismatch_override FROM runners WHERE id = ?1",
                params!["runner-vmo-2"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn version_mismatch_override_404s_for_unknown_runner() {
        let (db, _td) = full_db();
        let runners = new_runner_registry();
        let state = test_app_state(db, runners);

        let resp = set_version_mismatch_override(
            axum::extract::State(state),
            axum::extract::Path("nope-such-runner".to_string()),
            auth_user("default-org", "default-user"),
            axum::Json(VersionMismatchOverrideBody { r#override: true }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn revoked_runner_is_filtered_from_runner_belongs_to_org() {
        // Defends the pivot in `runner_belongs_to_org` against accidental
        // regression: shutdown/config/delete must all 404 on a soft-deleted
        // runner so the dashboard cannot keep mutating a runner the user
        // already revoked.
        let (db, _td) = full_db();
        let user_id = "tester-4";
        seed_runner_with_token(&db, "runner-soft", "default-org", user_id, "hash-soft");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE runners SET removed_at = datetime('now') WHERE id = ?1",
                params!["runner-soft"],
            )
            .unwrap();
        }
        let runners = new_runner_registry();
        let state = test_app_state(db, runners);
        assert!(!runner_belongs_to_org(&state, "runner-soft", "default-org"));
    }
}
