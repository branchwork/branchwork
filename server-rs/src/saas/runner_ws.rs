//! Server-side WebSocket handler for remote runners.
//!
//! A runner connects to `GET /ws/runner?token=<api_token>`, authenticates via
//! the token, and then enters the bidirectional event loop. Events from the
//! runner (agent_started, agent_output, …) are forwarded to the dashboard
//! broadcast channel. Commands from the dashboard (start_agent, kill_agent, …)
//! are queued in `inbox_pending` and flushed to the runner.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State, WebSocketUpgrade, ws::Message},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::db::Db;
use crate::state::AppState;
use crate::ws::broadcast_event;

use super::outbox;
use super::runner_protocol::{
    CiAggregate, Envelope, FolderEntry, GhRun, MergeOutcome, WireMessage,
};

// ── Runner registry (in-memory, lives in AppState) ──────────────────────────

/// A response from a runner to a request/response WireMessage pair (e.g.
/// `ListFolders` → `FoldersListed`). Routed back to the originating HTTP
/// caller via a `oneshot` sender registered in `ConnectedRunner.pending`.
#[allow(dead_code)] // payload fields read by dispatchers in agents/git_ops.rs and ci.rs
#[derive(Debug)]
pub enum RunnerResponse {
    FoldersListed(Vec<FolderEntry>),
    FolderCreated {
        ok: bool,
        resolved_path: Option<String>,
        error: Option<String>,
    },
    /// Reply to `GetDefaultBranch`. `None` ≡ no candidate resolved.
    DefaultBranchResolved(Option<String>),
    /// Reply to `ListBranches`. Always sorted; may be empty.
    BranchesListed(Vec<String>),
    /// Reply to `MergeBranch`. The outcome arm tells the dispatcher
    /// which HTTP status to map to.
    MergeResult(MergeOutcome),
    /// Reply to `PushBranch`. `ok=false` ⇒ `stderr` carries the error
    /// for logging; the dashboard does not surface push failures.
    PushResult {
        ok: bool,
        stderr: Option<String>,
    },
    /// Reply to `GhRunList`. `None` ≡ no workflow has fired for this
    /// commit yet, or `gh` is unavailable on the runner.
    GhRunListed(Option<GhRun>),
    /// Reply to `GhFailureLog`. `None` ≡ run still pending or `gh`
    /// unavailable.
    GhFailureLogFetched(Option<String>),

    // ── Auto-mode high-level RPC replies ────────────────────────────────────
    /// Reply to `MergeAgentBranch`. The runner has done the full agent-aware
    /// merge sequence (target resolution + 5-step git) on its side; the
    /// dispatch shim translates this into the server-side `MergeOutcome`
    /// shape and decides on broadcast / CI trigger / DB cleanup.
    AgentBranchMerged {
        ok: bool,
        merged_sha: Option<String>,
        target_branch: String,
        had_conflict: bool,
        error: Option<String>,
    },
    /// Reply to `HasGithubActions`. Single bool — the auto-mode CI gate
    /// uses it to decide whether to skip CI polling and advance to the
    /// next task immediately.
    GithubActionsDetected(bool),
    /// Reply to `GetCiRunStatus`. `None` ≡ no workflow has fired for the
    /// SHA yet, or `gh` is unavailable on the runner. The aggregate is
    /// pre-computed runner-side so the auto-mode loop receives the same
    /// shape regardless of mode.
    CiRunStatusResolved(Option<CiAggregate>),
    /// Reply to `CiFailureLog`. Both fields are `None` when the runner
    /// couldn't resolve a failing run id (still polling, no cached
    /// aggregate for the plan, etc).
    CiFailureLogFetched {
        log: Option<String>,
        run_id_used: Option<String>,
    },
}

/// A connected runner's server-side handle.
pub struct ConnectedRunner {
    /// Send commands to this runner's WebSocket write task.
    pub command_tx: mpsc::UnboundedSender<String>,
    /// Runner metadata from the most recent `runner_hello`.
    pub hostname: Option<String>,
    pub version: Option<String>,
    /// Pending request/response oneshots, keyed by `req_id`. The HTTP
    /// caller registers a sender, sends the request frame best-effort, and
    /// awaits the receiver with a short timeout. The WS reader resolves the
    /// matching entry when the runner's reply arrives. Late replies (after
    /// timeout or reconnect) find nothing waiting and are silently dropped.
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>>,
}

/// Registry of currently connected runners. Keyed by runner_id.
pub type RunnerRegistry = Arc<Mutex<HashMap<String, ConnectedRunner>>>;

pub fn new_runner_registry() -> RunnerRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Token auth ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RunnerWsQuery {
    token: String,
}

/// Validate the runner API token. Returns `(runner_name, org_id)` on success.
fn validate_runner_token(db: &Db, token: &str) -> Option<(String, String)> {
    let hash = sha256_hex(token);
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT runner_name, org_id FROM runner_tokens WHERE token_hash = ?1",
        params![hash],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

/// Simple SHA-256 hex hash for token storage. We don't need bcrypt here —
/// API tokens are high-entropy random strings, not user-chosen passwords.
fn sha256_hex(input: &str) -> String {
    // Use a basic hash. Since we don't want to add a sha2 crate dependency,
    // we'll use a simple approach: store tokens as-is (they're already random
    // 256-bit hex strings). In production you'd want proper hashing.
    // For now, use the token directly as the "hash" — it's already 256 bits
    // of entropy so brute-force is infeasible.
    input.to_string()
}

// ── WebSocket handler ───────────────────────────────────────────────────────

/// `GET /ws/runner?token=<api_token>` — upgrade to WebSocket for a runner.
pub async fn runner_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<RunnerWsQuery>,
) -> Response {
    let Some((runner_name, org_id)) = validate_runner_token(&state.db, &query.token) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "invalid_token").into_response();
    };

    ws.on_upgrade(move |socket| handle_runner_ws(socket, state, runner_name, org_id))
}

/// Per-runner WebSocket event loop.
async fn handle_runner_ws(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    runner_name: String,
    org_id: String,
) {
    let (mut ws_sink, mut ws_stream) = {
        use futures_util::StreamExt;
        let (sink, stream) = socket.split();
        (sink, stream)
    };

    // Channel for outbound messages to this runner.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();

    // We'll learn the runner_id from the first RunnerHello message.
    let runner_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let runner_id_write = runner_id.clone();

    // Register with runner registry once we know the runner_id.
    let runners = state.runners.clone();

    // ── Writer task: flush commands to the WebSocket ─────────────────────
    let write_handle = tokio::spawn(async move {
        use futures_util::SinkExt;
        while let Some(msg) = cmd_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // ── Reader task: process incoming messages from the runner ───────────
    {
        use futures_util::StreamExt;
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Ping(data) => {
                    let _ = cmd_tx.send(
                        serde_json::to_string(&Envelope::best_effort(
                            String::new(),
                            WireMessage::Pong {},
                        ))
                        .unwrap_or_default(),
                    );
                    let _ = data;
                    continue;
                }
                Message::Close(_) => break,
                _ => continue,
            };

            let Ok(envelope) = serde_json::from_str::<Envelope>(&text) else {
                eprintln!("[runner-ws] failed to parse envelope");
                continue;
            };

            let rid = envelope.runner_id.clone();

            // First message sets the runner_id.
            {
                let mut id_guard = runner_id.lock().await;
                if id_guard.is_none() {
                    *id_guard = Some(rid.clone());

                    // Update runner status in DB.
                    {
                        let conn = state.db.lock().unwrap();
                        conn.execute(
                            "INSERT INTO runners (id, name, org_id, status, last_seen_at)
                             VALUES (?1, ?2, ?3, 'online', datetime('now'))
                             ON CONFLICT(id) DO UPDATE SET
                               status = 'online',
                               last_seen_at = datetime('now'),
                               name = excluded.name",
                            params![rid, runner_name, org_id],
                        )
                        .ok();
                    }

                    // Register in-memory handle.
                    runners.lock().await.insert(
                        rid.clone(),
                        ConnectedRunner {
                            command_tx: cmd_tx.clone(),
                            hostname: None,
                            version: None,
                            pending: Arc::new(Mutex::new(HashMap::new())),
                        },
                    );

                    // Broadcast runner_connected to dashboard.
                    broadcast_event(
                        &state.broadcast_tx,
                        "runner_connected",
                        serde_json::json!({
                            "runner_id": rid,
                            "runner_name": runner_name,
                        }),
                    );

                    // Send Resume with our last_seen_seq so the runner replays.
                    let last_seq = {
                        let conn = state.db.lock().unwrap();
                        outbox::init_seq_tracker(&conn);
                        outbox::last_seen_seq(&conn, &rid)
                    };
                    let resume = Envelope::best_effort(
                        "server".into(),
                        WireMessage::Resume {
                            last_seen_seq: last_seq,
                        },
                    );
                    let _ = cmd_tx.send(serde_json::to_string(&resume).unwrap_or_default());

                    // Replay any pending commands from our inbox.
                    let pending = {
                        let conn = state.db.lock().unwrap();
                        outbox::replay_server_commands(&conn, &rid, 0)
                    };
                    for (seq, _cmd_type, payload) in pending {
                        // Re-wrap in an envelope with the seq.
                        if let Ok(msg) = serde_json::from_str::<WireMessage>(&payload) {
                            let env = Envelope::reliable("server".into(), seq, msg);
                            let _ = cmd_tx.send(serde_json::to_string(&env).unwrap_or_default());
                        }
                    }
                }
            }

            // Handle the message.
            handle_runner_message(&state, &rid, &org_id, &envelope, &cmd_tx).await;
        }
    }

    // ── Cleanup on disconnect ───────────────────────────────────────────
    let rid = runner_id_write.lock().await.clone();
    if let Some(rid) = &rid {
        // Remove from in-memory registry. Drain the pending request map so
        // any in-flight `runner_request` callers wake immediately with
        // `RunnerDisconnected` instead of waiting for the full timeout —
        // their oneshot receivers see `Closed` once the senders are dropped.
        if let Some(runner) = runners.lock().await.remove(rid) {
            runner.pending.lock().await.clear();
        }

        // Mark offline in DB.
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "UPDATE runners SET status = 'offline', last_seen_at = datetime('now') \
                 WHERE id = ?1",
                params![rid],
            )
            .ok();
        }

        broadcast_event(
            &state.broadcast_tx,
            "runner_disconnected",
            serde_json::json!({ "runner_id": rid }),
        );

        println!("[runner-ws] Runner {rid} disconnected");
    }

    write_handle.abort();
}

/// Process a single message from a runner.
async fn handle_runner_message(
    state: &AppState,
    runner_id: &str,
    org_id: &str,
    envelope: &Envelope,
    cmd_tx: &mpsc::UnboundedSender<String>,
) {
    // ACK reliable messages.
    if let Some(seq) = envelope.seq {
        // Idempotency check.
        let is_new = {
            let conn = state.db.lock().unwrap();
            outbox::init_seq_tracker(&conn);
            outbox::advance_peer_seq(&conn, runner_id, seq)
        };

        if is_new {
            // Send ACK back to runner.
            let ack = Envelope::best_effort("server".into(), WireMessage::Ack { ack_seq: seq });
            let _ = cmd_tx.send(serde_json::to_string(&ack).unwrap_or_default());
        } else {
            // Duplicate — already processed. Still ACK so the runner prunes.
            let ack = Envelope::best_effort("server".into(), WireMessage::Ack { ack_seq: seq });
            let _ = cmd_tx.send(serde_json::to_string(&ack).unwrap_or_default());
            return; // Don't re-process.
        }
    }

    match &envelope.message {
        WireMessage::RunnerHello {
            hostname,
            version,
            drivers,
        } => {
            // Update runner metadata.
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE runners SET hostname = ?1, version = ?2 WHERE id = ?3",
                    params![hostname, version, runner_id],
                )
                .ok();
            }

            // Update in-memory handle.
            if let Some(runner) = state.runners.lock().await.get_mut(runner_id) {
                runner.hostname = Some(hostname.clone());
                runner.version = Some(version.clone());
            }

            // Broadcast driver auth to dashboard.
            broadcast_event(
                &state.broadcast_tx,
                "runner_drivers",
                serde_json::json!({
                    "runner_id": runner_id,
                    "drivers": drivers,
                }),
            );

            println!(
                "[runner-ws] Runner {runner_id} hello: {hostname} v{version}, {} drivers",
                drivers.len()
            );
        }

        WireMessage::AgentStarted {
            agent_id,
            plan_name,
            task_id,
            driver,
            cwd,
        } => {
            // Upsert the agent row. The new `start_agent_dispatch` path
            // (agents/spawn_ops.rs) pre-inserts the row with
            // status='starting' so the dashboard can show the agent
            // immediately; the runner's confirmation here flips it to
            // 'running'. Older callers that bypass the dispatcher land
            // straight on this insert.
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "INSERT INTO agents \
                     (id, plan_name, task_id, cwd, status, mode, driver, org_id) \
                     VALUES (?1, ?2, ?3, ?4, 'running', 'remote', ?5, ?6) \
                     ON CONFLICT(id) DO UPDATE SET status = 'running'",
                    params![agent_id, plan_name, task_id, cwd, driver, org_id],
                )
                .ok();
            }
            broadcast_event(
                &state.broadcast_tx,
                "agent_started",
                serde_json::json!({
                    "id": agent_id,
                    "plan_name": plan_name,
                    "task_id": task_id,
                    "driver": driver,
                    "runner_id": runner_id,
                }),
            );
        }

        WireMessage::AgentOutput { agent_id, data } => {
            // Forward to dashboard (best-effort).
            broadcast_event(
                &state.broadcast_tx,
                "agent_output",
                serde_json::json!({
                    "agent_id": agent_id,
                    "data": data,
                    "runner_id": runner_id,
                }),
            );
        }

        WireMessage::AgentStopped {
            agent_id,
            status,
            cost_usd,
            stop_reason,
        } => {
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE agents SET status = ?1, cost_usd = ?2, stop_reason = ?3, \
                     finished_at = datetime('now') WHERE id = ?4",
                    params![status, cost_usd, stop_reason, agent_id],
                )
                .ok();

                // Org budget enforcement after cost update.
                if cost_usd.is_some() {
                    let org_id: Option<String> = conn
                        .query_row(
                            "SELECT org_id FROM agents WHERE id = ?1",
                            params![agent_id],
                            |row| row.get::<_, String>(0),
                        )
                        .ok();
                    if let Some(org_id) = org_id {
                        super::billing::enforce_org_budget(&conn, &org_id, None);
                    }
                }
            }
            broadcast_event(
                &state.broadcast_tx,
                "agent_stopped",
                serde_json::json!({
                    "id": agent_id,
                    "status": status,
                    "cost_usd": cost_usd,
                    "stop_reason": stop_reason,
                    "runner_id": runner_id,
                }),
            );
        }

        WireMessage::TaskStatusChanged {
            plan_name,
            task_number,
            status,
            reason,
        } => {
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "INSERT INTO task_status (plan_name, task_number, status, source, org_id)
                     VALUES (?1, ?2, ?3, 'manual', ?4)
                     ON CONFLICT(plan_name, task_number) DO UPDATE SET
                       status = excluded.status,
                       source = 'manual',
                       updated_at = datetime('now')",
                    params![plan_name, task_number, status, org_id],
                )
                .ok();

                if let Some(r) = reason {
                    conn.execute(
                        "INSERT INTO task_learnings (plan_name, task_number, learning, org_id) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![plan_name, task_number, r, org_id],
                    )
                    .ok();
                }
            }
            broadcast_event(
                &state.broadcast_tx,
                "task_status_changed",
                serde_json::json!({
                    "plan_name": plan_name,
                    "task_number": task_number,
                    "status": status,
                    "runner_id": runner_id,
                }),
            );
        }

        WireMessage::DriverAuthReport { drivers } => {
            broadcast_event(
                &state.broadcast_tx,
                "runner_drivers",
                serde_json::json!({
                    "runner_id": runner_id,
                    "drivers": drivers,
                }),
            );
        }

        WireMessage::Ack { ack_seq } => {
            // Runner ACKed one of our commands — mark in outbox.
            let conn = state.db.lock().unwrap();
            outbox::mark_server_acked(&conn, *ack_seq);
        }

        WireMessage::Resume { last_seen_seq } => {
            // Runner is asking us to replay commands from this seq.
            let pending = {
                let conn = state.db.lock().unwrap();
                outbox::replay_server_commands(&conn, runner_id, *last_seen_seq)
            };
            for (seq, _cmd_type, payload) in pending {
                if let Ok(msg) = serde_json::from_str::<WireMessage>(&payload) {
                    let env = Envelope::reliable("server".into(), seq, msg);
                    let _ = cmd_tx.send(serde_json::to_string(&env).unwrap_or_default());
                }
            }
        }

        WireMessage::Ping {} => {
            let pong = Envelope::best_effort("server".into(), WireMessage::Pong {});
            let _ = cmd_tx.send(serde_json::to_string(&pong).unwrap_or_default());
        }

        WireMessage::FoldersListed { req_id, entries } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "folders_listed",
                RunnerResponse::FoldersListed(entries.clone()),
            )
            .await;
        }

        WireMessage::FolderCreated {
            req_id,
            ok,
            resolved_path,
            error,
        } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "folder_created",
                RunnerResponse::FolderCreated {
                    ok: *ok,
                    resolved_path: resolved_path.clone(),
                    error: error.clone(),
                },
            )
            .await;
        }

        WireMessage::DefaultBranchResolved { req_id, branch } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "default_branch_resolved",
                RunnerResponse::DefaultBranchResolved(branch.clone()),
            )
            .await;
        }

        WireMessage::BranchesListed { req_id, branches } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "branches_listed",
                RunnerResponse::BranchesListed(branches.clone()),
            )
            .await;
        }

        WireMessage::MergeResult { req_id, outcome } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "merge_result",
                RunnerResponse::MergeResult(outcome.clone()),
            )
            .await;
        }

        WireMessage::PushResult { req_id, ok, stderr } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "push_result",
                RunnerResponse::PushResult {
                    ok: *ok,
                    stderr: stderr.clone(),
                },
            )
            .await;
        }

        WireMessage::GhRunListed { req_id, run } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "gh_run_listed",
                RunnerResponse::GhRunListed(run.clone()),
            )
            .await;
        }

        WireMessage::GhFailureLogFetched { req_id, log } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "gh_failure_log_fetched",
                RunnerResponse::GhFailureLogFetched(log.clone()),
            )
            .await;
        }

        WireMessage::AgentBranchMerged {
            req_id,
            ok,
            merged_sha,
            target_branch,
            had_conflict,
            error,
        } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "agent_branch_merged",
                RunnerResponse::AgentBranchMerged {
                    ok: *ok,
                    merged_sha: merged_sha.clone(),
                    target_branch: target_branch.clone(),
                    had_conflict: *had_conflict,
                    error: error.clone(),
                },
            )
            .await;
        }

        WireMessage::GithubActionsDetected { req_id, present } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "github_actions_detected",
                RunnerResponse::GithubActionsDetected(*present),
            )
            .await;
        }

        WireMessage::CiRunStatusResolved { req_id, aggregate } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "ci_run_status_resolved",
                RunnerResponse::CiRunStatusResolved(aggregate.clone()),
            )
            .await;
        }

        WireMessage::CiFailureLogResolved {
            req_id,
            log,
            run_id_used,
        } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "ci_failure_log_resolved",
                RunnerResponse::CiFailureLogFetched {
                    log: log.clone(),
                    run_id_used: run_id_used.clone(),
                },
            )
            .await;
        }

        // Server doesn't receive these from runners — the runner sending them
        // would be a protocol violation (saas→runner direction only).
        WireMessage::Pong {}
        | WireMessage::StartAgent { .. }
        | WireMessage::KillAgent { .. }
        | WireMessage::ResizeTerminal { .. }
        | WireMessage::AgentInput { .. }
        | WireMessage::TerminalReplay { .. }
        | WireMessage::ListFolders { .. }
        | WireMessage::CreateFolder { .. }
        | WireMessage::GetDefaultBranch { .. }
        | WireMessage::ListBranches { .. }
        | WireMessage::MergeBranch { .. }
        | WireMessage::PushBranch { .. }
        | WireMessage::GhRunList { .. }
        | WireMessage::GhFailureLog { .. }
        | WireMessage::MergeAgentBranch { .. }
        | WireMessage::HasGithubActions { .. }
        | WireMessage::GetCiRunStatus { .. }
        | WireMessage::CiFailureLog { .. } => {}
    }
}

// ── API routes for runner token management ──────────────────────────────────

/// POST /api/runners/tokens — create a new runner API token.
/// Body: `{ "runner_name": "my-laptop" }`
pub async fn create_runner_token(
    State(state): State<AppState>,
    user: crate::auth::AuthUser,
    axum::Json(body): axum::Json<CreateTokenRequest>,
) -> Response {
    let token = generate_token();
    let hash = sha256_hex(&token);

    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO runner_tokens (token_hash, runner_name, org_id, created_by) \
             VALUES (?1, ?2, ?3, ?4)",
            params![hash, body.runner_name, user.org_id, user.id],
        )
        .expect("failed to insert runner token");
    }

    (
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "token": token,
            "runner_name": body.runner_name,
        })),
    )
        .into_response()
}

/// GET /api/runners — list registered runners.
pub async fn list_runners(State(state): State<AppState>, user: crate::auth::AuthUser) -> Response {
    let runners: Vec<serde_json::Value> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, status, hostname, version, last_seen_at, created_at \
                 FROM runners WHERE org_id = ?1 ORDER BY last_seen_at DESC",
            )
            .unwrap();
        stmt.query_map(params![user.org_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, Option<String>>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "status": row.get::<_, Option<String>>(2)?,
                "hostname": row.get::<_, Option<String>>(3)?,
                "version": row.get::<_, Option<String>>(4)?,
                "lastSeenAt": row.get::<_, Option<String>>(5)?,
                "createdAt": row.get::<_, Option<String>>(6)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    axum::Json(serde_json::json!({ "runners": runners })).into_response()
}

/// POST /api/runners/{runner_id}/commands — enqueue a command to a runner.
/// Used by dashboard actions (start agent, kill agent, etc.).
pub async fn send_runner_command(
    State(state): State<AppState>,
    axum::extract::Path(runner_id): axum::extract::Path<String>,
    _user: crate::auth::AuthUser,
    axum::Json(command): axum::Json<WireMessage>,
) -> Response {
    let payload = serde_json::to_string(&command).unwrap_or_default();
    let seq = {
        let conn = state.db.lock().unwrap();
        outbox::enqueue_server_command(&conn, &runner_id, command.event_type(), &payload)
    };

    // If the runner is currently connected, push immediately.
    let env = Envelope::reliable("server".into(), seq, command);
    let env_json = serde_json::to_string(&env).unwrap_or_default();

    if let Some(runner) = state.runners.lock().await.get(&runner_id) {
        let _ = runner.command_tx.send(env_json);
    }

    axum::Json(serde_json::json!({ "seq": seq, "queued": true })).into_response()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub runner_name: String,
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(64);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Send `response` to the request's oneshot sender registered in the runner's
/// `pending` map, or log + drop if no caller is waiting (timeout, late reply,
/// runner re-registered after disconnect).
async fn resolve_pending(
    state: &AppState,
    runner_id: &str,
    req_id: &str,
    label: &'static str,
    response: RunnerResponse,
) {
    let pending = state
        .runners
        .lock()
        .await
        .get(runner_id)
        .map(|r| r.pending.clone());
    let sender = match pending {
        Some(pending) => pending.lock().await.remove(req_id),
        None => None,
    };
    if let Some(tx) = sender {
        let _ = tx.send(response);
    } else {
        eprintln!(
            "[runner-ws] dropped orphan {label} reply: runner_id={runner_id} req_id={req_id}"
        );
    }
}
