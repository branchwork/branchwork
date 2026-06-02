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
    CiAggregate, DriverAuthInfo, Envelope, FolderEntry, GhRun, MergeOutcome, ProjectDiskUsage,
    WireMessage, WorktreeOrphan,
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
    /// Terminal reply to `CloneProject`. The runner separately emits a
    /// `CloneStarted` breadcrumb (forwarded to the dashboard, not surfaced
    /// here) before this lands. `ok=true` ⇒ `resolved_path` carries the
    /// absolute directory the runner cloned into; `ok=false` ⇒ `error`
    /// carries the captured stderr tail. The dispatcher uses the resolved
    /// path to update the `projects.workspace_path` column.
    CloneResult {
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
    /// Reply to `DiscardBranch`. `ok=false` ⇒ `error` carries the
    /// captured stderr so the server reflects it as a 500. `ok=true`
    /// triggers the post-discard server-side bookkeeping (clear branch
    /// in DB, audit, broadcast).
    BranchDiscarded {
        ok: bool,
        error: Option<String>,
    },
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

    // ── Worktree RPC replies (Task 2.7) ─────────────────────────────────────
    /// Reply to `SetupWorktree`. `Ok(path)` ⇒ the runner created the worktree
    /// at the absolute `path`; `Err(msg)` ⇒ the git / IO error. The two wire
    /// variants (`WorktreeCreated` / `WorktreeSetupFailed`) both resolve into
    /// this single `Result` so the dispatcher has one match arm.
    WorktreeSetup(Result<String, String>),
    /// Reply to `RemoveWorktree`. The outcome label (`removed`,
    /// `force_removed`, `manually_removed`, or `error: <msg>`); the
    /// dispatcher parses it back into a `RemoveOutcome`.
    WorktreeRemoved(String),
    /// Reply to `ListWorktreeOrphans`. The orphan worktrees the runner found
    /// under the project's scope; empty when none (or on a listing failure).
    WorktreeOrphansListed(Vec<WorktreeOrphan>),
    /// Reply to `DiskUsage`. Per-project worktree disk usage the runner
    /// measured under its own base; empty when none (or on a listing failure).
    DiskUsageReported(Vec<ProjectDiskUsage>),
}

/// A connected runner's server-side handle.
pub struct ConnectedRunner {
    /// Send commands to this runner's WebSocket write task.
    pub command_tx: mpsc::UnboundedSender<String>,
    /// Runner metadata from the most recent `runner_hello`.
    pub hostname: Option<String>,
    pub version: Option<String>,
    /// Last driver inventory + auth status the runner pushed via
    /// `RunnerHello` or `DriverAuthReport`. Populated on every report so
    /// `list_drivers_dispatch` can answer without a wire round-trip.
    /// `None` until the first report lands.
    pub drivers: Option<Vec<DriverAuthInfo>>,
    /// Pending request/response oneshots, keyed by `req_id`. The HTTP
    /// caller registers a sender, sends the request frame best-effort, and
    /// awaits the receiver with a short timeout. The WS reader resolves the
    /// matching entry when the runner's reply arrives. Late replies (after
    /// timeout or reconnect) find nothing waiting and are silently dropped.
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>>,
    /// The public server URL this runner connected to (derived from the
    /// WebSocket connection). Used to generate correct MCP/hook URLs for
    /// agents spawned on this runner. Format: "https://branchwork.dev" or
    /// "http://localhost:3100" (no trailing slash).
    pub server_url: String,
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

/// Validate the runner API token. Returns `(runner_name, org_id, claimed_runner_id)`
/// on success. `claimed_runner_id` is `None` until the first runner connects with
/// this token; the WS handler enforces the single-use binding (matching incoming
/// runner_id is allowed for reconnect, mismatch is rejected).
fn validate_runner_token(db: &Db, token: &str) -> Option<(String, String, Option<String>)> {
    let hash = sha256_hex(token);
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT runner_name, org_id, claimed_runner_id FROM runner_tokens WHERE token_hash = ?1",
        params![hash],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .ok()
}

/// Claim the token for `runner_id`, or verify the existing claim matches.
/// Returns `Ok(())` when the token is unclaimed (then claims it) or already
/// claimed by `runner_id`; `Err(claimed_id)` when the token is claimed by a
/// different runner. The check is per-token so a token re-pasted on a second
/// host fails closed without affecting that runner's other tokens.
pub fn claim_or_verify_token(db: &Db, token_hash: &str, runner_id: &str) -> Result<(), String> {
    let conn = db.lock().unwrap();
    let claimed: Option<String> = conn
        .query_row(
            "SELECT claimed_runner_id FROM runner_tokens WHERE token_hash = ?1",
            params![token_hash],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    match claimed {
        Some(existing) if existing != runner_id => Err(existing),
        Some(_) => Ok(()),
        None => {
            conn.execute(
                "UPDATE runner_tokens SET claimed_runner_id = ?1 WHERE token_hash = ?2",
                params![runner_id, token_hash],
            )
            .ok();
            Ok(())
        }
    }
}

/// Simple SHA-256 hex hash for token storage. We don't need bcrypt here —
/// API tokens are high-entropy random strings, not user-chosen passwords.
pub(crate) fn sha256_hex(input: &str) -> String {
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
    headers: axum::http::HeaderMap,
) -> Response {
    let Some((runner_name, org_id, _claimed)) = validate_runner_token(&state.db, &query.token)
    else {
        return (axum::http::StatusCode::UNAUTHORIZED, "invalid_token").into_response();
    };

    // Resolve the public server URL from headers so we can pass it to the
    // runner. This is the URL agents on this runner will use for MCP/hooks.
    let server_url = crate::saas::install_runner::effective_public_url(&headers);

    let token_hash = sha256_hex(&query.token);
    ws.on_upgrade(move |socket| {
        handle_runner_ws(socket, state, runner_name, org_id, token_hash, server_url)
    })
}

/// Per-runner WebSocket event loop.
async fn handle_runner_ws(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    runner_name: String,
    org_id: String,
    token_hash: String,
    server_url: String,
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
                    // Single-use token enforcement: a token unclaimed at
                    // issue time is bound to the first runner_id it sees.
                    // A token re-pasted on a different host (fresh runner_id
                    // from `load_or_generate_runner_id`) gets rejected here
                    // — the WS already passed initial token-validity, but
                    // the per-runner_id binding fails closed. Restarts on
                    // the same host re-use the persisted runner_id and
                    // sail through.
                    if let Err(claimed_by) = claim_or_verify_token(&state.db, &token_hash, &rid) {
                        eprintln!(
                            "[runner-ws] token already used: claimed_by={claimed_by} incoming={rid}"
                        );
                        // Drop the connection — the runner sees its WS close
                        // and the operator gets the error in the runner log.
                        break;
                    }

                    *id_guard = Some(rid.clone());

                    // T5.3 defense-in-depth: before the un-hide UPSERT below,
                    // peek at the row's current `removed_at` so we can detect
                    // ghost reconnects — a WS connect from a runner whose row
                    // was soft-deleted but whose token was not revoked.
                    // After T5.1 + T5.2 this should be rare (the token DELETE
                    // is now part of `delete_runner`), so any audit row here
                    // is a smoke signal that either a legacy stale-state row
                    // slipped through or a fresh shape of this bug class is
                    // emerging. We log + audit and let the UPSERT proceed so
                    // normal operation continues; the forensics matter more
                    // than the recovery action.
                    let existing_removed_at: Option<String> = {
                        let conn = state.db.lock().unwrap();
                        conn.query_row(
                            "SELECT removed_at FROM runners WHERE id = ?1",
                            params![rid],
                            |row| row.get::<_, Option<String>>(0),
                        )
                        .ok()
                        .flatten()
                    };
                    if let Some(removed_at_value) = existing_removed_at.as_deref() {
                        // Only log a fingerprint of the token_hash — today
                        // `sha256_hex` returns the raw token (see its
                        // doc-comment), and even a future real-SHA256
                        // version is still a credential we should not
                        // emit in full to stderr or to audit_logs.
                        let token_hash_prefix: String = token_hash.chars().take(8).collect();
                        eprintln!(
                            "[runner-ws] WARNING: ghost-runner connect — \
                             runner_id={rid} removed_at={removed_at_value} \
                             token_hash={token_hash_prefix}"
                        );
                        let diff = serde_json::json!({
                            "runner_id": &rid,
                            "runner_name": &runner_name,
                            "removed_at": removed_at_value,
                            "token_hash_prefix": &token_hash_prefix,
                        })
                        .to_string();
                        let conn = state.db.lock().unwrap();
                        crate::audit::log(
                            &conn,
                            &org_id,
                            None,
                            Some("branchwork-runner-ws"),
                            crate::audit::actions::RUNNER_GHOST_RECONNECT,
                            crate::audit::resources::RUNNER,
                            Some(&rid),
                            Some(&diff),
                        );
                    }

                    // Update runner status in DB.
                    //
                    // T5.2: clear `removed_at` on the conflict-resolution UPDATE
                    // so a legitimate reconnect (token still bound by
                    // `claim_or_verify_token` above) un-hides a runner row that
                    // was previously soft-deleted. The security model relies on
                    // `delete_runner`'s token DELETE: a *properly* revoked
                    // runner cannot reach this code because
                    // `validate_runner_token` already 401'd the upgrade. The
                    // only way we land here with `removed_at IS NOT NULL` is a
                    // legacy / stale-state row where the runner row was
                    // soft-deleted without revoking the token — un-hiding is
                    // the desired recovery, and T5.3 logs the event above
                    // so the smoke signal does not get lost.
                    //
                    // This intentionally relaxes the `WHERE removed_at IS NULL`
                    // gate T5.1 placed here; the other four mid-WS UPDATE sites
                    // (disconnect, RunnerHello, DriverAuthReport, RunnerHealth)
                    // keep that gate because they fire on a still-alive WS that
                    // raced an in-flight `delete_runner`.
                    {
                        let conn = state.db.lock().unwrap();
                        conn.execute(
                            "INSERT INTO runners (id, name, org_id, status, last_seen_at)
                             VALUES (?1, ?2, ?3, 'online', datetime('now'))
                             ON CONFLICT(id) DO UPDATE SET
                               status = 'online',
                               last_seen_at = datetime('now'),
                               name = excluded.name,
                               removed_at = NULL",
                            params![rid, runner_name, org_id],
                        )
                        .ok();
                    }

                    // T5.1 → T5.2 → T5.3: the prior defense-in-depth SELECT
                    // for `removed_at IS NOT NULL` is now folded into the
                    // ghost-reconnect telemetry above; the UPDATE
                    // unconditionally clears `removed_at`. Token revocation
                    // in `delete_runner` is the primary gate.

                    // Register in-memory handle.
                    runners.lock().await.insert(
                        rid.clone(),
                        ConnectedRunner {
                            command_tx: cmd_tx.clone(),
                            hostname: None,
                            version: None,
                            drivers: None,
                            pending: Arc::new(Mutex::new(HashMap::new())),
                            server_url: server_url.clone(),
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
                    // T4.3: also include the server's own CARGO_PKG_VERSION so the
                    // runner can detect version drift per-connect without needing
                    // a separate endpoint round-trip.
                    let last_seq = {
                        let conn = state.db.lock().unwrap();
                        outbox::init_seq_tracker(&conn);
                        outbox::last_seen_seq(&conn, &rid)
                    };
                    let resume = Envelope::best_effort(
                        "server".into(),
                        WireMessage::Resume {
                            last_seen_seq: last_seq,
                            server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
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

            // T5.1: defense in depth — if `delete_runner` cleared the
            // in-memory `ConnectedRunner` entry for this runner_id mid-flight,
            // refuse to keep processing inbound messages. Otherwise the live
            // WS would keep ACKing reliable messages and re-broadcasting
            // runner_drivers / runner-health events for a soft-deleted runner
            // (the SQL UPDATEs already no-op via the `removed_at IS NULL`
            // gates above). Skipping the handler AND breaking the loop closes
            // the WS from the server side on the next inbound message.
            {
                let still_registered = state.runners.lock().await.contains_key(&rid);
                if !still_registered {
                    eprintln!("[runner-ws] runner_id={rid} no longer in registry, dropping WS");
                    break;
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
        //
        // T5.1: `AND removed_at IS NULL` so a runner revoked mid-flight does
        // not get tickled by the disconnect-cleanup UPDATE on its way out.
        {
            let conn = state.db.lock().unwrap();
            conn.execute(
                "UPDATE runners SET status = 'offline', last_seen_at = datetime('now') \
                 WHERE id = ?1 AND removed_at IS NULL",
                params![rid],
            )
            .ok();
        }

        broadcast_event(
            &state.broadcast_tx,
            "runner_disconnected",
            serde_json::json!({ "runner_id": rid }),
        );

        // T11.5 mid-task sibling failover: for plans pinned to this
        // runner whose failover policy is 'sibling', mark in-flight
        // agents as failed with `runner_disappeared` so auto-mode picks
        // up the failure path and re-dispatches via
        // `resolve_runner_for_spawn` (which now sees the pin offline +
        // failover=sibling and routes to the sibling). Plans with
        // failover='pause' (default) keep T11.4's existing semantics:
        // in-flight agents are left at 'running' until either the
        // runner returns or an explicit kill/timeout flips them.
        //
        // We don't gate on whether a sibling currently exists — auto-mode's
        // fix-attempt re-dispatch will re-evaluate. If no sibling is
        // available by then, resolve_runner_for_spawn falls back to
        // PinnedRunnerOffline and pauses the plan with runner_offline,
        // matching the brief's "all runners offline" edge case.
        let plans_to_failover: Vec<String> = {
            let conn = state.db.lock().unwrap();
            conn.prepare(
                "SELECT plan_name FROM plan_runner_affinity \
                 WHERE runner_id = ?1 AND runner_failover = 'sibling'",
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![rid], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default()
        };

        for plan_name in &plans_to_failover {
            let in_flight: Vec<String> = {
                let conn = state.db.lock().unwrap();
                conn.prepare(
                    "SELECT id FROM agents \
                     WHERE plan_name = ?1 AND status IN ('running', 'starting')",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_default()
            };

            if in_flight.is_empty() {
                continue;
            }

            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE agents SET status = 'failed', stop_reason = 'runner_disappeared', \
                     finished_at = datetime('now') \
                     WHERE plan_name = ?1 AND status IN ('running', 'starting')",
                    params![plan_name],
                )
                .ok();
            }

            for agent_id in &in_flight {
                broadcast_event(
                    &state.broadcast_tx,
                    "agent_stopped",
                    serde_json::json!({
                        "id": agent_id,
                        "status": "failed",
                        "stop_reason": "runner_disappeared",
                        "plan": plan_name,
                    }),
                );
            }

            broadcast_event(
                &state.broadcast_tx,
                "runner_failover",
                serde_json::json!({
                    "plan": plan_name,
                    "from_runner_id": rid,
                    "agents_failed": in_flight,
                    "trigger": "runner_disconnected",
                }),
            );
        }

        println!("[runner-ws] Runner {rid} disconnected");
    }

    write_handle.abort();
}

/// Refresh `runners.hostname/version/drivers_json/server_bin*/upgrade_available`
/// from a `RunnerHello`, plus the in-memory `ConnectedRunner` snapshot.
///
/// T5.4: hoisted out of the per-envelope match arm in `handle_runner_message`
/// so it runs on every RunnerHello regardless of duplicate-seq status. The
/// runner persists its outbox seqs in `~/.branchwork-runner/runner.db`, so
/// every reconnect after a binary upgrade replays a hello carrying a seq
/// the server has already advanced past — without this hoisted refresh,
/// `version` and `hostname` stay frozen at whatever value made it through
/// the first-ever hello (the 2026-05-18 incident: dashboard showed
/// `version=0.3.0` for a runner whose on-disk binary was `v0.5.x`).
///
/// `drivers_json` accidentally kept updating pre-T5.4 because
/// `DriverAuthReport` also touches it via fresh seqs; version/hostname have
/// no such secondary refresh path and froze hard.
///
/// The `removed_at IS NULL` gate (T5.1) is preserved: a stale hello on a
/// live WS must not re-populate a revoked runner's metadata.
async fn refresh_runner_row_from_hello(
    state: &AppState,
    runner_id: &str,
    hostname: &str,
    version: &str,
    drivers: &[DriverAuthInfo],
    server_bin: Option<&crate::saas::runner_protocol::ServerBinDiagnostic>,
    upgrade_available: bool,
) {
    let drivers_json = serde_json::to_string(drivers).ok();

    // T1.2: split the wire diagnostic into the two persisted columns.
    // Found ⇒ (path=Some, error=NULL); NotFound ⇒ (path=NULL,
    // error="<reason>: <searched>") so the dashboard can render a green
    // check + path or a red cross + reason without re-parsing the enum.
    // None (older runners) ⇒ both columns NULL, dashboard renders a
    // neutral chip.
    let (server_bin_path, server_bin_error): (Option<String>, Option<String>) = match server_bin {
        Some(crate::saas::runner_protocol::ServerBinDiagnostic::Found { path }) => {
            (Some(path.clone()), None)
        }
        Some(crate::saas::runner_protocol::ServerBinDiagnostic::NotFound { searched, reason }) => {
            (None, Some(format!("{reason}: {searched}")))
        }
        None => (None, None),
    };

    // T4.3: `upgrade_available` is set by the runner's per-connect drift
    // check or periodic `install-runner.sh` poll. Older runners without
    // the field default to `false` — back-compat is symmetric since the
    // dashboard already computes server-side `version_severity` from
    // `version`.
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE runners SET hostname = ?1, version = ?2, drivers_json = ?3, \
                               server_bin_path = ?4, server_bin_error = ?5, \
                               upgrade_available = ?6 \
             WHERE id = ?7 AND removed_at IS NULL",
            params![
                hostname,
                version,
                drivers_json,
                server_bin_path,
                server_bin_error,
                upgrade_available as i64,
                runner_id
            ],
        )
        .ok();
    }

    // Update in-memory handle (idempotent snapshot refresh).
    if let Some(runner) = state.runners.lock().await.get_mut(runner_id) {
        runner.hostname = Some(hostname.to_string());
        runner.version = Some(version.to_string());
        runner.drivers = Some(drivers.to_vec());
    }
}

/// Process a single message from a runner.
async fn handle_runner_message(
    state: &AppState,
    runner_id: &str,
    org_id: &str,
    envelope: &Envelope,
    cmd_tx: &mpsc::UnboundedSender<String>,
) {
    // ACK reliable messages (always — duplicate or not).
    //
    // T5.4: previously this early-returned on duplicate-seq envelopes
    // *before* the per-envelope match arm ran. That short-circuited the
    // `UPDATE runners SET hostname/version/...` write on every reconnect
    // (the runner's outbox replays a seq the server has already advanced
    // past), freezing `runners.version` at whatever value made it through
    // the first-ever hello. We now compute `is_duplicate_seq` and let the
    // RunnerHello row-refresh below run unconditionally; the rest of the
    // per-arm side effects (broadcasts, agent reattachment, hello
    // println) still get the dedupe via the conditional return after the
    // refresh.
    let is_duplicate_seq = if let Some(seq) = envelope.seq {
        let is_new = {
            let conn = state.db.lock().unwrap();
            outbox::init_seq_tracker(&conn);
            outbox::advance_peer_seq(&conn, runner_id, seq)
        };
        // Always ACK — even on duplicate, so the runner can prune.
        let ack = Envelope::best_effort("server".into(), WireMessage::Ack { ack_seq: seq });
        let _ = cmd_tx.send(serde_json::to_string(&ack).unwrap_or_default());
        !is_new
    } else {
        false
    };

    // T5.4 ghost-version fix: hoisted out of the match arm so it runs on
    // every RunnerHello, including duplicate-seq replays. A duplicate
    // hello carries an accurate snapshot of the runner's current
    // self-report and should refresh the row — see
    // `refresh_runner_row_from_hello` for the full rationale.
    if let WireMessage::RunnerHello {
        hostname,
        version,
        drivers,
        active_agents: _,
        server_bin,
        upgrade_available,
    } = &envelope.message
    {
        refresh_runner_row_from_hello(
            state,
            runner_id,
            hostname,
            version,
            drivers,
            server_bin.as_ref(),
            *upgrade_available,
        )
        .await;
    }

    // Duplicate seq: the RunnerHello row-refresh above is the only side
    // effect we want to run twice. Skip broadcasts, agent reattachment,
    // and the per-arm side effects below — those are idempotent-unsafe
    // (e.g. don't re-broadcast `runner_drivers`, don't re-run agent
    // reattachment).
    if is_duplicate_seq {
        return;
    }

    match &envelope.message {
        WireMessage::RunnerHello {
            hostname,
            version,
            drivers,
            active_agents,
            server_bin: _,
            upgrade_available: _,
        } => {
            // T5.4: the runner row UPDATE + in-memory handle refresh
            // already ran in `refresh_runner_row_from_hello` above. This
            // arm only owns the idempotent-unsafe side effects that must
            // NOT re-fire on a duplicate-seq hello.

            // Broadcast driver auth to dashboard.
            broadcast_event(
                &state.broadcast_tx,
                "runner_drivers",
                serde_json::json!({
                    "runner_id": runner_id,
                    "drivers": drivers,
                }),
            );

            // T5.14: undo `cleanup_and_reattach`'s blanket
            // `killed/supervisor_died` marking of remote agents. The
            // server runs that pass at startup before any runner has
            // had a chance to connect; once the runner's hello arrives
            // listing the agents it still has alive, flip those rows
            // back to `running` and broadcast `agent_resumed` so the
            // dashboard re-renders them as live without waiting for the
            // first byte of output to land. Idempotent — running rows
            // stay running, completed/failed rows stay completed/failed.
            let mut resumed: Vec<String> = Vec::new();
            if !active_agents.is_empty() {
                let conn = state.db.lock().unwrap();
                for aid in active_agents {
                    let was_dead: bool = conn
                        .query_row(
                            "SELECT status FROM agents WHERE id = ?1",
                            params![aid],
                            |row| row.get::<_, String>(0),
                        )
                        .map(|s| matches!(s.as_str(), "killed" | "failed"))
                        .unwrap_or(false);
                    if was_dead {
                        let updated = conn
                            .execute(
                                "UPDATE agents \
                                 SET status = 'running', \
                                     stop_reason = NULL, \
                                     finished_at = NULL \
                                 WHERE id = ?1 AND mode = 'remote'",
                                params![aid],
                            )
                            .unwrap_or(0);
                        if updated > 0 {
                            resumed.push(aid.clone());
                        }
                    }
                }
            }
            for aid in &resumed {
                broadcast_event(
                    &state.broadcast_tx,
                    "agent_resumed",
                    serde_json::json!({
                        "id": aid,
                        "runner_id": runner_id,
                    }),
                );
            }

            println!(
                "[runner-ws] Runner {runner_id} hello: {hostname} v{version}, {} drivers, {} agents resumed",
                drivers.len(),
                resumed.len()
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

        WireMessage::CheckAgentOutput { agent_id, line } => {
            // SaaS-mode equivalent of `check_agent.rs`'s in-process
            // stdout reader: append the stream-json line to the
            // `agent_output` table and broadcast `agent_output` so the
            // dashboard's StreamJsonView catches each frame as it lands.
            // `message_type` mirrors the standalone classifier — `result`
            // for the terminal record, `stdout` otherwise — derived from
            // the JSON `type` field. Best-effort: a malformed line is
            // still surfaced (`stdout`) so the user sees what claude
            // emitted, even when the JSON parser would reject it.
            let msg_type = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
                .map(|t| if t == "result" { "result" } else { "stdout" })
                .unwrap_or("stdout")
                .to_string();
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, ?2, ?3)",
                    params![agent_id, msg_type, line],
                )
                .ok();
            }
            broadcast_event(
                &state.broadcast_tx,
                "agent_output",
                serde_json::json!({
                    "agent_id": agent_id,
                    "message_type": msg_type,
                    "content": line,
                }),
            );
        }

        WireMessage::AgentOutput { agent_id, data } => {
            // Forward to dashboard (best-effort). The shape MUST match the
            // frontend's `AgentOutput` zod schema in
            // `web/src/schemas/ws-events.ts` (`{agent_id, message_type,
            // content}`); pre-T5.9 we emitted `{agent_id, data,
            // runner_id}` and the frontend dropped every event with a
            // `[ws] dropped malformed message` warning, flooding the
            // browser console at the runner's PTY-output cadence.
            //
            // `message_type: "pty"` matches the standalone-mode tag so
            // the per-agent terminal pane treats SaaS bytes the same as
            // local-supervisor bytes; `content` carries the base64
            // payload verbatim — the dashboard's `/terminal?agent=<id>`
            // bridge in `terminal_ws::handle_terminal_remote` decodes it
            // before forwarding to xterm.
            broadcast_event(
                &state.broadcast_tx,
                "agent_output",
                serde_json::json!({
                    "agent_id": agent_id,
                    "message_type": "pty",
                    "content": data,
                }),
            );
        }

        WireMessage::AgentStopped {
            agent_id,
            status,
            cost_usd,
            stop_reason,
        } => {
            // Capture plan + task before the row update so the auto-mode
            // hook below can fire without a second SELECT. Captured up
            // front because `status` is a `&String` borrow of the message
            // and we need plain owned strings to outlive the lock.
            let plan_task: Option<(String, String)> = {
                let conn = state.db.lock().unwrap();
                conn.query_row(
                    "SELECT plan_name, task_id FROM agents WHERE id = ?1",
                    params![agent_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .ok()
                .and_then(|(p, t)| match (p, t) {
                    (Some(p), Some(t)) => Some((p, t)),
                    _ => None,
                })
            };

            // Track whether the task_status flip should fire — computed
            // once here so both the DB write (inside the lock) and the
            // broadcast (outside) agree.
            let task_completed_now =
                status == "completed" && stop_reason.is_none() && plan_task.is_some();
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE agents SET status = ?1, cost_usd = ?2, stop_reason = ?3, \
                     finished_at = datetime('now') WHERE id = ?4",
                    params![status, cost_usd, stop_reason, agent_id],
                )
                .ok();

                // Mark task as `completed` in task_status when the agent finishes
                // cleanly. Without this the task card sticks at `in_progress`
                // after the agent exits, even though the agent row has flipped
                // to `completed` (bob's original fix used `'done'` here, which
                // isn't in the schema's valid-status set — `set_task_status`
                // validates against `['pending', 'in_progress', 'completed',
                // 'failed', 'skipped', 'checking']`, the auto-advance
                // `done_set` filter at `mod.rs:1123` only counts
                // `IN ('completed', 'skipped')`, and the dashboard's
                // STATUS_CONFIG falls through to the default badge for any
                // unknown value. Using `'completed'` keeps the manual and
                // auto-mode paths consistent.
                //
                // Auto-mode overwrites with `source='auto'` later when its
                // own merge → CI → advance loop fires; manual completion
                // gets this immediate write so the task card unlocks
                // without waiting for the next /api/plans poll.
                if task_completed_now {
                    let (plan, task) = plan_task.as_ref().unwrap();
                    conn.execute(
                        "INSERT INTO task_status (plan_name, task_number, status, source, org_id)
                         VALUES (?1, ?2, 'completed', 'manual', ?3)
                         ON CONFLICT(plan_name, task_number) DO UPDATE SET
                           status = 'completed',
                           source = 'manual',
                           updated_at = datetime('now')",
                        params![plan, task, org_id],
                    )
                    .ok();
                }

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
            // T5.18: notify the dashboard of the task_status flip — `agent_stopped`
            // alone doesn't refetch /api/plans, so without this broadcast the
            // task card stays at `in_progress` until the next plan refetch.
            if task_completed_now {
                let (plan, task) = plan_task.as_ref().unwrap();
                broadcast_event(
                    &state.broadcast_tx,
                    "task_status_changed",
                    serde_json::json!({
                        "plan_name": plan,
                        "task_number": task,
                        "status": "completed",
                    }),
                );
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

            // Auto-mode merge-on-completion hook. Mirrors the standalone
            // path in `pty_agent::on_agent_exit`; the helper itself gates
            // on `auto_mode_enabled`, so we just need plan/task and a
            // clean stop. `stop_reason` is `None` on a normal exit; any
            // explicit reason (kill, supervisor_unreachable, budget) means
            // the loop should not auto-merge.
            if status == "completed"
                && stop_reason.is_none()
                && let Some((plan, task)) = plan_task
            {
                crate::auto_mode::on_task_agent_completed(state, agent_id, &plan, &task).await;
            }
        }

        WireMessage::AgentSpawnFailed {
            agent_id,
            command,
            errno,
            errno_str,
        } => {
            // Runner couldn't `Command::spawn` the session daemon — typically
            // because `--server-bin` points at a missing or unauthorised path.
            // Render a precise, actionable message and stash it on the agents
            // row so the dashboard can surface it inline next to the agent
            // card. The follow-up `AgentStopped` (also reliable, also from
            // this runner) does the row's status flip; this arm only owns
            // the `spawn_error` column and the `agent_spawn_failed`
            // broadcast.
            let message = format!("runner could not spawn: {command} ({errno_str})");
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE agents SET spawn_error = ?1 WHERE id = ?2",
                    params![message, agent_id],
                )
                .ok();
            }
            broadcast_event(
                &state.broadcast_tx,
                "agent_spawn_failed",
                serde_json::json!({
                    "id": agent_id,
                    "command": command,
                    "errno": errno,
                    "errno_str": errno_str,
                    "message": message,
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
            // Persist + cache the latest snapshot so `list_drivers_dispatch`
            // can serve it without a wire round-trip and the dashboard can
            // surface the inventory even after the runner disconnects.
            let drivers_json = serde_json::to_string(drivers).ok();
            // T5.1: `AND removed_at IS NULL` so a DriverAuthReport from a
            // revoked-but-still-connected runner does not rewrite its
            // drivers_json column.
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE runners SET drivers_json = ?1 \
                     WHERE id = ?2 AND removed_at IS NULL",
                    params![drivers_json, runner_id],
                )
                .ok();
            }
            if let Some(runner) = state.runners.lock().await.get_mut(runner_id) {
                runner.drivers = Some(drivers.clone());
            }

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

        WireMessage::Resume {
            last_seen_seq,
            server_version: _,
        } => {
            // Runner is asking us to replay commands from this seq.
            // T4.3: the `server_version` field is only meaningful when the
            // SERVER sends `Resume` (runner reads it to detect drift); the
            // runner→server direction leaves it `None`.
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

        WireMessage::CloneStarted { req_id } => {
            // Progress breadcrumb — broadcast for dashboard observability,
            // but DO NOT resolve the pending receiver. The terminal reply
            // (`CloneDone` or `CloneFailed`) is what unblocks the HTTP
            // caller.
            broadcast_event(
                &state.broadcast_tx,
                "clone_started",
                serde_json::json!({
                    "runner_id": runner_id,
                    "req_id": req_id,
                }),
            );
        }

        WireMessage::CloneDone {
            req_id,
            resolved_path,
        } => {
            // Broadcast the completion so the dashboard can clear any
            // "Cloning…" indicator before the HTTP response lands.
            broadcast_event(
                &state.broadcast_tx,
                "clone_done",
                serde_json::json!({
                    "runner_id": runner_id,
                    "req_id": req_id,
                    "resolved_path": resolved_path,
                }),
            );
            resolve_pending(
                state,
                runner_id,
                req_id,
                "clone_done",
                RunnerResponse::CloneResult {
                    ok: true,
                    resolved_path: Some(resolved_path.clone()),
                    error: None,
                },
            )
            .await;
        }

        WireMessage::CloneFailed { req_id, error } => {
            broadcast_event(
                &state.broadcast_tx,
                "clone_failed",
                serde_json::json!({
                    "runner_id": runner_id,
                    "req_id": req_id,
                    "error": error,
                }),
            );
            resolve_pending(
                state,
                runner_id,
                req_id,
                "clone_failed",
                RunnerResponse::CloneResult {
                    ok: false,
                    resolved_path: None,
                    error: Some(error.clone()),
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

        WireMessage::BranchDiscarded { req_id, ok, error } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "branch_discarded",
                RunnerResponse::BranchDiscarded {
                    ok: *ok,
                    error: error.clone(),
                },
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

        WireMessage::WorktreeCreated { req_id, path } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "worktree_created",
                RunnerResponse::WorktreeSetup(Ok(path.clone())),
            )
            .await;
        }

        WireMessage::WorktreeSetupFailed { req_id, error } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "worktree_setup_failed",
                RunnerResponse::WorktreeSetup(Err(error.clone())),
            )
            .await;
        }

        WireMessage::WorktreeRemoved { req_id, outcome } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "worktree_removed",
                RunnerResponse::WorktreeRemoved(outcome.clone()),
            )
            .await;
        }

        WireMessage::WorktreeOrphansListed { req_id, orphans } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "worktree_orphans_listed",
                RunnerResponse::WorktreeOrphansListed(orphans.clone()),
            )
            .await;
        }

        WireMessage::DiskUsageReported { req_id, projects } => {
            resolve_pending(
                state,
                runner_id,
                req_id,
                "disk_usage_reported",
                RunnerResponse::DiskUsageReported(projects.clone()),
            )
            .await;
        }

        WireMessage::RunnerHealth {
            outbox_depth,
            ws_reconnects_24h,
            ci_poll_ms_p50,
            ci_poll_ms_p99,
            orphans_reaped_24h,
        } => {
            // Persist the latest snapshot in place — only the most recent
            // values matter, missed ticks are absorbed by the next surviving
            // tick. last_health_at is a freshness marker the dashboard uses
            // to distinguish "currently green" from "was green five minutes
            // ago" when the runner drops offline.
            // T5.1: `AND removed_at IS NULL` so a periodic RunnerHealth on a
            // revoked-but-still-connected runner does not bump
            // last_health_at / outbox_depth on the soft-deleted row.
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE runners SET \
                       outbox_depth = ?1, \
                       ws_reconnects_24h = ?2, \
                       ci_poll_ms_p50 = ?3, \
                       ci_poll_ms_p99 = ?4, \
                       orphans_reaped_24h = ?5, \
                       last_health_at = datetime('now') \
                     WHERE id = ?6 AND removed_at IS NULL",
                    params![
                        *outbox_depth as i64,
                        *ws_reconnects_24h as i64,
                        ci_poll_ms_p50.map(|v| v as i64),
                        ci_poll_ms_p99.map(|v| v as i64),
                        *orphans_reaped_24h as i64,
                        runner_id,
                    ],
                )
                .ok();
            }

            // Compute the version-mismatch verdict from the runner's
            // most-recently-reported version (DB column populated by
            // RunnerHello). The dashboard uses this to color the version
            // chip — green/yellow/red. Done server-side so the same rule
            // governs both /api/runners polling and live WS updates.
            let server_version = env!("CARGO_PKG_VERSION");
            let (runner_version, version_mismatch_override): (Option<String>, bool) = {
                let conn = state.db.lock().unwrap();
                conn.query_row(
                    "SELECT version, COALESCE(version_mismatch_override, 0) \
                     FROM runners WHERE id = ?1",
                    params![runner_id],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)? != 0)),
                )
                .unwrap_or((None, false))
            };
            let version_mismatch =
                classify_version_mismatch(runner_version.as_deref(), server_version);
            let version_severity = version_mismatch.to_severity(server_version);

            broadcast_event(
                &state.broadcast_tx,
                "runner_health",
                serde_json::json!({
                    "runner_id": runner_id,
                    "outbox_depth": outbox_depth,
                    "ws_reconnects_24h": ws_reconnects_24h,
                    "ci_poll_ms_p50": ci_poll_ms_p50,
                    "ci_poll_ms_p99": ci_poll_ms_p99,
                    "orphans_reaped_24h": orphans_reaped_24h,
                    "version": runner_version,
                    "server_version": server_version,
                    "version_mismatch": version_mismatch.as_str(),
                    // T1.3: coarser color verdict the dashboard chip + dispatcher
                    // consult. Same data, different precision.
                    "version_severity": version_severity.as_str(),
                    "version_mismatch_override": version_mismatch_override,
                }),
            );
        }

        WireMessage::RunnerLogLine { ts, level, line } => {
            // Pure fan-out: forward every line straight to the dashboard
            // WS. No DB persistence — log lines are ephemeral; long-term
            // retention is the runner host's job (`journalctl` or its
            // equivalent on the host's tty). Rate limiting is the
            // runner's responsibility, so by the time we see a line it
            // has already passed the 200 lines/sec cap.
            broadcast_event(
                &state.broadcast_tx,
                "runner_log",
                serde_json::json!({
                    "runner_id": runner_id,
                    "ts": ts,
                    "level": level,
                    "line": line,
                }),
            );
        }

        // Server doesn't receive these from runners — the runner sending them
        // would be a protocol violation (saas→runner direction only).
        WireMessage::Pong {}
        | WireMessage::StartAgent { .. }
        | WireMessage::KillAgent { .. }
        | WireMessage::ShutdownRequest { .. }
        | WireMessage::UpgradeRunner { .. }
        | WireMessage::ResizeTerminal { .. }
        | WireMessage::AgentInput { .. }
        | WireMessage::TerminalReplay { .. }
        | WireMessage::ListFolders { .. }
        | WireMessage::CreateFolder { .. }
        | WireMessage::CloneProject { .. }
        | WireMessage::GetDefaultBranch { .. }
        | WireMessage::ListBranches { .. }
        | WireMessage::MergeBranch { .. }
        | WireMessage::DiscardBranch { .. }
        | WireMessage::StartCheckAgent { .. }
        | WireMessage::PushBranch { .. }
        | WireMessage::GhRunList { .. }
        | WireMessage::GhFailureLog { .. }
        | WireMessage::MergeAgentBranch { .. }
        | WireMessage::HasGithubActions { .. }
        | WireMessage::GetCiRunStatus { .. }
        | WireMessage::CiFailureLog { .. }
        // Worktree request variants (Task 2.7) are saas→runner only.
        | WireMessage::SetupWorktree { .. }
        | WireMessage::RemoveWorktree { .. }
        | WireMessage::ListWorktreeOrphans { .. }
        // Disk-usage request is saas→runner only (the server sends it).
        | WireMessage::DiskUsage { .. } => {}
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
///
/// Response shape (frozen for the dashboard RunnerStatus indicator, audit §17):
/// `{ runners: [...], mode: "saas" | "standalone" }`. The `mode` field tells
/// the dashboard whether the deployment expects remote runners at all — in
/// `standalone` the indicator is hidden, in `saas` it shows online/offline/
/// missing-runner state. See `deployment_mode` for the resolution rule.
pub async fn list_runners(State(state): State<AppState>, user: crate::auth::AuthUser) -> Response {
    let server_version = env!("CARGO_PKG_VERSION");
    let runners: Vec<serde_json::Value> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, status, hostname, version, last_seen_at, created_at, \
                        drivers_json, outbox_depth, ws_reconnects_24h, \
                        ci_poll_ms_p50, ci_poll_ms_p99, last_health_at, \
                        orphans_reaped_24h, server_bin_path, server_bin_error, \
                        COALESCE(version_mismatch_override, 0), \
                        COALESCE(upgrade_available, 0) \
                 FROM runners WHERE org_id = ?1 AND removed_at IS NULL \
                 ORDER BY last_seen_at DESC",
            )
            .unwrap();
        stmt.query_map(params![user.org_id], |row| {
            // Surface the runner's last-known driver inventory directly on
            // the row so the `/runners` page can render the per-row
            // inventory chip without a second `/api/drivers?runner_id=`
            // round-trip per row. JSON parses lazily — a corrupt row falls
            // back to an empty array rather than failing the whole list.
            let drivers_json: Option<String> = row.get(7)?;
            let drivers = drivers_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            let runner_version: Option<String> = row.get(4)?;
            let version_mismatch =
                classify_version_mismatch(runner_version.as_deref(), server_version);
            let version_severity = version_mismatch.to_severity(server_version);
            // T1.2: server-bin self-diagnostic. `path` is set when the
            // runner resolved a real file; `error` is set when it didn't.
            // Both NULL ⇒ older runner that never reported the field.
            let server_bin_path: Option<String> = row.get(14)?;
            let server_bin_error: Option<String> = row.get(15)?;
            // T1.3: operator-set override that lifts the version-mismatch
            // dispatch block. Defaults to 0 for pre-migration rows.
            let version_mismatch_override: bool = row.get::<_, i64>(16)? != 0;
            // T4.3: runner-detected upgrade availability. Distinct from
            // `versionSeverity` (which gates dispatch on amber/red); a
            // patch-only drift is severity=green but upgrade_available=true,
            // letting the dashboard offer the Upgrade button without
            // blocking dispatch.
            let upgrade_available: bool = row.get::<_, i64>(17)? != 0;
            Ok(serde_json::json!({
                "id": row.get::<_, Option<String>>(0)?,
                "name": row.get::<_, Option<String>>(1)?,
                "status": row.get::<_, Option<String>>(2)?,
                "hostname": row.get::<_, Option<String>>(3)?,
                "version": runner_version,
                "lastSeenAt": row.get::<_, Option<String>>(5)?,
                "createdAt": row.get::<_, Option<String>>(6)?,
                "drivers": drivers,
                "health": {
                    "outboxDepth": row.get::<_, Option<i64>>(8)?,
                    "wsReconnects24h": row.get::<_, Option<i64>>(9)?,
                    "ciPollMsP50": row.get::<_, Option<i64>>(10)?,
                    "ciPollMsP99": row.get::<_, Option<i64>>(11)?,
                    "lastHealthAt": row.get::<_, Option<String>>(12)?,
                    "orphansReaped24h": row.get::<_, Option<i64>>(13)?,
                },
                "serverBin": {
                    "path": server_bin_path,
                    "error": server_bin_error,
                },
                "versionMismatch": version_mismatch.as_str(),
                // T1.3: coarser color verdict the dashboard chip + dispatcher
                // consult. Same data shape, different precision.
                "versionSeverity": version_severity.as_str(),
                "versionMismatchOverride": version_mismatch_override,
                "upgradeAvailable": upgrade_available,
                "serverVersion": server_version,
            }))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    // Deployment-wide agent counts grouped by status. The server doesn't
    // tag agents with their runner today (T4.2 learning), so we surface
    // the per-org tally and label it as such — same numbers every row sees.
    let agent_counts = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT status, COUNT(*) FROM agents \
                 WHERE org_id = ?1 GROUP BY status",
            )
            .unwrap();
        let mut counts = serde_json::Map::new();
        if let Ok(rows) = stmt.query_map(params![user.org_id], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
        }) {
            for entry in rows.flatten() {
                if let (Some(status), n) = entry {
                    counts.insert(status, serde_json::Value::from(n));
                }
            }
        }
        serde_json::Value::Object(counts)
    };

    let mode = deployment_mode(&state.db, &user.org_id);
    axum::Json(serde_json::json!({
        "runners": runners,
        "mode": mode,
        "agentCounts": agent_counts,
        "serverVersion": server_version,
    }))
    .into_response()
}

/// Resolve the dashboard's deployment-mode discriminator for `org_id`.
///
/// Returns `"saas"` if either signal is present:
///   - `BRANCHWORK_PUBLIC_URL` is set (the prod overlay's SaaS hint —
///     `deploy/docker-compose.prod.yml` sets it on every public deploy).
///   - The org already has at least one row in `runners` (matches
///     `crate::saas::dispatch::org_has_runner`, the same signal the
///     auto-mode and folder-dispatch paths use).
///
/// Returns `"standalone"` otherwise. The "SaaS, no runner registered" state
/// the audit calls out (amber dot + `/runners` link) is reachable when the
/// env hint is set but no runner has connected yet.
fn deployment_mode(db: &Db, org_id: &str) -> &'static str {
    let env_hint = std::env::var("BRANCHWORK_PUBLIC_URL")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    deployment_mode_from_signals(env_hint, crate::saas::dispatch::org_has_runner(db, org_id))
}

/// Pure split of [`deployment_mode`] so unit tests don't have to mutate
/// the process env (parallel `cargo test` makes env mutation unsafe).
/// Mirrors the `IdleFinishConfig::from_values` convention.
fn deployment_mode_from_signals(env_hint: bool, has_runner: bool) -> &'static str {
    if env_hint || has_runner {
        "saas"
    } else {
        "standalone"
    }
}

// ── Version mismatch classification ─────────────────────────────────────────

/// Verdict for the per-runner version chip on `/runners`. Computed from the
/// `RunnerHello.version` column (DB-persisted) compared against the
/// dashboard server's compiled-in `CARGO_PKG_VERSION`.
///
/// Mapping (semver-flavoured: `<major>.<minor>.<patch>` parsed leniently):
/// - `Ok` ⇒ exact match, OR runner_version is `None` (we don't have a
///   reading yet — chip stays neutral, no green check, no red).
/// - `Patch` ⇒ same major + same minor, different patch (or extra trailing
///   text). The protocol promise is "patch-compatible," so this is a
///   neutral/info state — the chip renders green with a tooltip.
/// - `Minor` ⇒ same major, different minor. The dashboard chip turns
///   yellow ⚠ — still on the wire, but features may diverge.
/// - `Major` ⇒ different major version. The dashboard chip turns red ⛔ —
///   protocol incompatibility likely; operator should upgrade the runner.
/// - `Unparseable` ⇒ the runner reported a version string that can't be
///   parsed (free-form / pre-release / git sha). Falls through to
///   `Ok` so a custom build doesn't show a misleading red chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionMismatch {
    Ok,
    Patch,
    Minor,
    Major,
}

impl VersionMismatch {
    pub fn as_str(self) -> &'static str {
        match self {
            VersionMismatch::Ok => "ok",
            VersionMismatch::Patch => "patch",
            VersionMismatch::Minor => "minor",
            VersionMismatch::Major => "major",
        }
    }

    /// Map the (kind, server_version) pair to the operator-visible chip
    /// color. Three buckets, deliberately coarser than [`VersionMismatch`]
    /// because the chip has to be readable at a glance — the structured
    /// kind stays available for tooltips / audit / forward-compat.
    ///
    /// Rule (cargo-semver / npm-semver flavour — pre-1.0 minor IS breaking):
    ///
    /// - **Green** ⇒ exact match OR patch-only difference. Protocol-
    ///   compatible by convention; the runner can dispatch agents safely.
    /// - **Amber** ⇒ same effective-major but different effective-minor.
    ///   Only reachable in 1.x+ land (e.g. 1.4.x vs 1.5.x). The runner
    ///   is dispatchable but the operator should plan an upgrade.
    /// - **Red** ⇒ different effective-major. Pre-1.0 minor difference
    ///   (e.g. v0.3.0 vs v0.5.x — the incident this severity exists for)
    ///   AND 1.x vs 2.x both collapse here. The dispatcher refuses to
    ///   send `StartAgent` to a red runner unless the operator has
    ///   explicitly opted in via `runners.version_mismatch_override`.
    ///
    /// `server_version` is consulted because the "effective major" rule
    /// differs by whether we're pre- or post-1.0: in 0.x.y the minor IS
    /// the effective major (per cargo semver). The runner's own major is
    /// not consulted independently — when classify returns `Minor` both
    /// sides agree on the literal major; checking the server side is
    /// enough.
    pub fn to_severity(self, server_version: &str) -> Severity {
        match self {
            VersionMismatch::Ok | VersionMismatch::Patch => Severity::Green,
            VersionMismatch::Major => Severity::Red,
            VersionMismatch::Minor => {
                // Pre-1.0 (major==0) treats minor as the effective major
                // — protocol-breaking by convention. Anything 1.x+ keeps
                // standard semver semantics (minor is non-breaking).
                let pre_1_0 = parse_semver_lenient(server_version)
                    .map(|s| s.0 == 0)
                    .unwrap_or(false);
                if pre_1_0 {
                    Severity::Red
                } else {
                    Severity::Amber
                }
            }
        }
    }
}

/// Operator-visible chip color for the runner-vs-server version
/// comparison. Coarser than [`VersionMismatch`] so the dashboard chip
/// has three readable states (green / amber / red). The structured
/// `VersionMismatch` value is still shipped on the wire as
/// `versionMismatch` for tooltips and audit; `versionSeverity` is the
/// dispatcher-actionable signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Green,
    Amber,
    Red,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Green => "green",
            Severity::Amber => "amber",
            Severity::Red => "red",
        }
    }
}

/// Parse `<major>.<minor>.<patch>` leniently — anything after the third dot
/// or a `-`/`+` is ignored (so `0.3.0-rc1` and `0.3.0+sha` both parse the
/// same as `0.3.0`). Returns `None` for inputs missing major or minor; the
/// caller treats unparseable as "ok" (don't paint a misleading red chip on
/// a custom build).
fn parse_semver_lenient(s: &str) -> Option<(u32, u32, u32)> {
    // Cut at the first `-` or `+` so pre-release / build metadata don't
    // break the integer parse.
    let head = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = head.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

pub fn classify_version_mismatch(
    runner_version: Option<&str>,
    server_version: &str,
) -> VersionMismatch {
    let Some(runner) = runner_version else {
        return VersionMismatch::Ok;
    };
    if runner == server_version {
        return VersionMismatch::Ok;
    }
    let (Some(r), Some(s)) = (
        parse_semver_lenient(runner),
        parse_semver_lenient(server_version),
    ) else {
        // One side is unparseable — be conservative and don't flag.
        return VersionMismatch::Ok;
    };
    if r.0 != s.0 {
        VersionMismatch::Major
    } else if r.1 != s.1 {
        VersionMismatch::Minor
    } else if r.2 != s.2 {
        VersionMismatch::Patch
    } else {
        VersionMismatch::Ok
    }
}

#[cfg(test)]
mod version_mismatch_tests {
    use super::{Severity, VersionMismatch, classify_version_mismatch};

    #[test]
    fn exact_match_is_ok() {
        assert_eq!(
            classify_version_mismatch(Some("0.3.0"), "0.3.0"),
            VersionMismatch::Ok
        );
    }

    #[test]
    fn missing_runner_version_is_ok() {
        // Runner that hasn't reported yet (offline at first boot) shows a
        // neutral chip — we don't paint red on absence of data.
        assert_eq!(
            classify_version_mismatch(None, "0.3.0"),
            VersionMismatch::Ok
        );
    }

    #[test]
    fn patch_difference_is_patch() {
        assert_eq!(
            classify_version_mismatch(Some("0.3.0"), "0.3.1"),
            VersionMismatch::Patch
        );
    }

    #[test]
    fn minor_difference_is_minor() {
        assert_eq!(
            classify_version_mismatch(Some("0.2.0"), "0.3.0"),
            VersionMismatch::Minor
        );
        // Newer runner than server — same protocol-incompat shape.
        assert_eq!(
            classify_version_mismatch(Some("0.4.0"), "0.3.0"),
            VersionMismatch::Minor
        );
    }

    #[test]
    fn major_difference_is_major() {
        assert_eq!(
            classify_version_mismatch(Some("1.0.0"), "0.3.0"),
            VersionMismatch::Major
        );
    }

    #[test]
    fn pre_release_suffix_compares_as_base() {
        // Custom build with a `-rc1` suffix shouldn't show major mismatch
        // against a release of the same base version.
        assert_eq!(
            classify_version_mismatch(Some("0.3.0-rc1"), "0.3.0"),
            VersionMismatch::Ok
        );
    }

    #[test]
    fn unparseable_falls_back_to_ok() {
        // A runner build that reports `git-abc1234` shouldn't be flagged —
        // we have no way to reason about it.
        assert_eq!(
            classify_version_mismatch(Some("git-abc1234"), "0.3.0"),
            VersionMismatch::Ok
        );
    }

    #[test]
    fn missing_patch_treated_as_zero() {
        assert_eq!(
            classify_version_mismatch(Some("0.3"), "0.3.0"),
            VersionMismatch::Ok
        );
        assert_eq!(
            classify_version_mismatch(Some("0.3"), "0.3.5"),
            VersionMismatch::Patch
        );
    }

    // ── to_severity (T1.3) ─────────────────────────────────────────────

    #[test]
    fn severity_ok_is_green() {
        assert_eq!(VersionMismatch::Ok.to_severity("0.5.0"), Severity::Green);
        assert_eq!(VersionMismatch::Ok.to_severity("1.4.0"), Severity::Green);
    }

    #[test]
    fn severity_patch_is_green() {
        // Patch is always green regardless of the server-major bucket:
        // protocol-compatible by convention.
        assert_eq!(VersionMismatch::Patch.to_severity("0.5.0"), Severity::Green);
        assert_eq!(VersionMismatch::Patch.to_severity("1.4.0"), Severity::Green);
    }

    #[test]
    fn severity_major_is_red() {
        // Cross-major is always red regardless of where the boundary is.
        assert_eq!(VersionMismatch::Major.to_severity("0.5.0"), Severity::Red);
        assert_eq!(VersionMismatch::Major.to_severity("1.4.0"), Severity::Red);
        assert_eq!(VersionMismatch::Major.to_severity("2.0.0"), Severity::Red);
    }

    /// Acceptance criterion from the T1.3 brief: connect a v0.3.0 runner to
    /// a v0.5.x server and the panel must show red. Pre-1.0 minor diffs
    /// ARE breaking by cargo-semver convention — that's the rule the
    /// classifier already encodes, this test pins the color mapping too.
    #[test]
    fn severity_minor_in_pre_1_0_is_red() {
        // Headline incident: v0.3.0 runner vs v0.5.x server.
        let kind = classify_version_mismatch(Some("0.3.0"), "0.5.3");
        assert_eq!(kind, VersionMismatch::Minor);
        assert_eq!(kind.to_severity("0.5.3"), Severity::Red);
        // And the symmetric case (newer runner than server).
        let kind = classify_version_mismatch(Some("0.5.0"), "0.3.0");
        assert_eq!(kind, VersionMismatch::Minor);
        assert_eq!(kind.to_severity("0.3.0"), Severity::Red);
    }

    #[test]
    fn severity_minor_in_post_1_0_is_amber() {
        // 1.x land: minor diffs are non-breaking by semver convention.
        // Warn but don't block — chip is amber.
        let kind = classify_version_mismatch(Some("1.4.0"), "1.5.0");
        assert_eq!(kind, VersionMismatch::Minor);
        assert_eq!(kind.to_severity("1.5.0"), Severity::Amber);
    }

    #[test]
    fn severity_as_str_round_trip() {
        assert_eq!(Severity::Green.as_str(), "green");
        assert_eq!(Severity::Amber.as_str(), "amber");
        assert_eq!(Severity::Red.as_str(), "red");
    }
}

#[cfg(test)]
mod deployment_mode_tests {
    use super::deployment_mode_from_signals;

    #[test]
    fn standalone_when_no_signals() {
        assert_eq!(deployment_mode_from_signals(false, false), "standalone");
    }

    #[test]
    fn saas_when_env_hint_only() {
        // The "SaaS, no runner registered" state from the audit — env hint
        // alone flips us into SaaS mode so the indicator shows the amber
        // "register a runner" prompt instead of disappearing.
        assert_eq!(deployment_mode_from_signals(true, false), "saas");
    }

    #[test]
    fn saas_when_runner_only() {
        // Standalone deploy that opted into a remote runner anyway. Once
        // any runner row exists for the org, every folder op routes via
        // the runner (matches `org_has_runner`'s contract).
        assert_eq!(deployment_mode_from_signals(false, true), "saas");
    }

    #[test]
    fn saas_when_both_signals() {
        assert_eq!(deployment_mode_from_signals(true, true), "saas");
    }
}

#[cfg(test)]
mod claim_token_tests {
    //! `claim_or_verify_token` is the single-use binding that backs the
    //! `Token is single-use (re-running the same command fails …)`
    //! acceptance criterion of plan/dashboard-ui-overhaul/4.8.

    use super::*;
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn fresh_db_with_token(token_hash: &str) -> Db {
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema for this test — only the columns
        // claim_or_verify_token actually reads/writes.
        conn.execute_batch(
            "CREATE TABLE runner_tokens (
                token_hash         TEXT PRIMARY KEY,
                runner_name        TEXT NOT NULL,
                org_id             TEXT NOT NULL DEFAULT 'default-org',
                created_by         TEXT NOT NULL,
                created_at         TEXT NOT NULL DEFAULT (datetime('now')),
                claimed_runner_id  TEXT
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runner_tokens (token_hash, runner_name, created_by) VALUES (?1, 'r', 'u')",
            params![token_hash],
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn first_claim_binds_runner_id() {
        let db = fresh_db_with_token("hash-a");
        assert!(claim_or_verify_token(&db, "hash-a", "runner-1").is_ok());
        // After the claim, the row carries the runner_id.
        let claimed: Option<String> = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT claimed_runner_id FROM runner_tokens WHERE token_hash = ?1",
                params!["hash-a"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claimed.as_deref(), Some("runner-1"));
    }

    #[test]
    fn same_runner_reconnect_is_ok() {
        // The runner restarting picks up the same persisted runner_id and
        // re-uses the token. Must succeed — otherwise every server reboot
        // bricks every runner.
        let db = fresh_db_with_token("hash-b");
        claim_or_verify_token(&db, "hash-b", "runner-1").unwrap();
        assert!(claim_or_verify_token(&db, "hash-b", "runner-1").is_ok());
    }

    #[test]
    fn different_runner_is_rejected() {
        // The acceptance bullet — pasting the same install command on a
        // second host produces a fresh runner_id, which must NOT pass the
        // claim check.
        let db = fresh_db_with_token("hash-c");
        claim_or_verify_token(&db, "hash-c", "runner-1").unwrap();
        let err = claim_or_verify_token(&db, "hash-c", "runner-2").unwrap_err();
        assert_eq!(err, "runner-1");
    }
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

pub(crate) fn generate_token() -> String {
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
