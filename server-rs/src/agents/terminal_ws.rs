use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use base64::Engine;
use serde::Deserialize;

use crate::agents::session_protocol::Message as SessionMessage;
use crate::saas::runner_protocol::{Envelope, WireMessage};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct TerminalQuery {
    agent: String,
}

pub async fn terminal_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<TerminalQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_terminal(socket, query.agent, state))
}

async fn handle_terminal(mut socket: WebSocket, agent_id: String, state: AppState) {
    // Load buffered output from DB first. Users always want the historical
    // transcript, even when the live agent is still going. SaaS-mode rows
    // currently have no `agent_output` rows persisted server-side (the
    // bytes only flow through `broadcast_tx` as `agent_output` events), so
    // this query is empty for remote agents — replay-from-zero parity is
    // a known gap (track via `runner_protocol::TerminalReplay`).
    let buffered_rows: Vec<String> = {
        let db = state.registry.db.lock().unwrap();
        let mut stmt = db
            .prepare(
                "SELECT content FROM agent_output \
                 WHERE agent_id = ? AND message_type = 'pty' ORDER BY id",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![agent_id], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };

    // Subscribe to the live output broadcast (if the agent is still live).
    // `command_tx` is what lets us write keystrokes + resize into the
    // daemon's PTY.
    let subscription = {
        let agents = state.registry.agents.lock().await;
        agents
            .get(&agent_id)
            .map(|agent| (agent.output_tx.subscribe(), agent.command_tx.clone()))
    };

    for row in &buffered_rows {
        if socket
            .send(Message::Text(row.clone().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    let (mut output_rx, command_tx) = match subscription {
        Some(s) => s,
        None => {
            // Not live in-process — could be:
            //   1. SaaS: agent runs on a remote runner. The supervisor
            //      socket is in the runner container, not here. Bridge
            //      the dashboard WS to the runner via `broadcast_tx`
            //      (output) + the per-runner `command_tx` (input/resize).
            //   2. Standalone: server restarted mid-agent (the daemon is
            //      reattachable via `supervisor_socket` but the registry
            //      lost its in-process handle on boot — pre-existing
            //      degraded path).
            //   3. Truly finished — fall through to "session ended".
            let row: Option<(String, String)> = {
                let db = state.registry.db.lock().unwrap();
                db.query_row(
                    "SELECT mode, status FROM agents WHERE id = ?",
                    rusqlite::params![agent_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .ok()
            };
            let (mode, status) = match row {
                Some(r) => r,
                None => return,
            };
            let is_running = matches!(status.as_str(), "running" | "starting");
            if mode == "remote" && is_running {
                handle_terminal_remote(socket, agent_id, state).await;
                return;
            }
            if is_running {
                socket
                    .send(Message::Text(
                        "\r\n\x1b[33m--- terminal detached (server restarted while agent was running) ---\x1b[0m\r\n\
                         \x1b[33m--- agent is still alive — use Kill to stop it, or wait for it to finish ---\x1b[0m\r\n".into(),
                    ))
                    .await
                    .ok();
            } else {
                socket
                    .send(Message::Text(
                        "\r\n\x1b[90m--- session ended ---\x1b[0m\r\n".into(),
                    ))
                    .await
                    .ok();
            }
            return;
        }
    };

    // Live bidirectional proxy: supervisor → browser, browser → supervisor.
    loop {
        tokio::select! {
            // Live PTY output from the daemon → browser
            recv = output_rx.recv() => {
                match recv {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    // Broadcast buffer overflow: skip the lag and keep going.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // Browser → supervisor. Text frames may carry resize JSON or
            // literal keystrokes; binary frames are always keystrokes.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text.starts_with('{')
                            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                            && val.get("type").and_then(|t| t.as_str()) == Some("resize")
                        {
                            let cols = val.get("cols").and_then(|c| c.as_u64()).unwrap_or(120) as u16;
                            let rows = val.get("rows").and_then(|r| r.as_u64()).unwrap_or(40) as u16;
                            let _ = command_tx.send(SessionMessage::Resize { cols, rows });
                            continue;
                        }
                        let _ = command_tx.send(SessionMessage::Input(text.as_bytes().to_vec()));
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let _ = command_tx.send(SessionMessage::Input(data.to_vec()));
                    }
                    None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

/// SaaS bridge for the dashboard's per-agent terminal WS. The agent's
/// session daemon lives on the runner; the runner forwards every PTY
/// chunk to the server as `WireMessage::AgentOutput` (best-effort), which
/// `runner_ws.rs` rebroadcasts as a `agent_output` event on the global
/// `broadcast_tx`. We subscribe here, filter for our agent, base64-decode
/// and ship the bytes to the dashboard. The reverse path turns dashboard
/// keystrokes / resize JSON into `WireMessage::AgentInput` /
/// `ResizeTerminal` envelopes pushed onto the runner's outbound channel.
///
/// **Buffered replay caveat.** The standalone path replays
/// `agent_output` DB rows before subscribing live, so a late-joining
/// dashboard sees the full transcript. SaaS rows are not persisted
/// server-side today (only the runner's session-daemon log file has the
/// transcript), so the dashboard sees output starting from the moment it
/// connected. Closing this gap is a follow-up via
/// `WireMessage::TerminalReplay` (already in the wire protocol).
async fn handle_terminal_remote(mut socket: WebSocket, agent_id: String, state: AppState) {
    // Resolve the runner this agent lives on. We pick the most recently-
    // seen online runner for the agent's org — same heuristic as
    // `pick_runner_for_org` in spawn_ops, which is what dispatched the
    // agent in the first place. Resolved once per WS connection: a
    // runner reconnect mid-session would not pick up the new id, but
    // command_tx is keyed by runner_id and a fresh dashboard reload is
    // already required to repopulate xterm in that case.
    let runner_id: Option<String> = {
        let conn = state.registry.db.lock().unwrap();
        conn.query_row(
            "SELECT r.id FROM agents a \
             JOIN runners r ON r.org_id = a.org_id \
             WHERE a.id = ?1 \
             ORDER BY (r.status = 'online') DESC, r.last_seen_at DESC LIMIT 1",
            rusqlite::params![agent_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };

    let mut bc_rx = state.broadcast_tx.subscribe();
    let b64 = base64::engine::general_purpose::STANDARD;

    loop {
        tokio::select! {
            // Server bus → browser. Filter for our agent's `agent_output`.
            recv = bc_rx.recv() => {
                match recv {
                    Ok(json) => {
                        let Ok(ev) = serde_json::from_str::<serde_json::Value>(&json) else {
                            continue;
                        };
                        if ev.get("type").and_then(|t| t.as_str()) != Some("agent_output") {
                            continue;
                        }
                        let Some(data) = ev.get("data") else { continue; };
                        if data.get("agent_id").and_then(|a| a.as_str()) != Some(agent_id.as_str()) {
                            continue;
                        }
                        // Field renamed `data` → `content` in T5.9 to match
                        // the frontend's `AgentOutput` zod schema; pre-T5.9
                        // builds emitted `data`. We accept either so a
                        // server upgrade mid-session doesn't blank the
                        // dashboard for already-running agents.
                        let Some(payload_b64) = data
                            .get("content")
                            .or_else(|| data.get("data"))
                            .and_then(|d| d.as_str())
                        else {
                            continue;
                        };
                        let Ok(bytes) = b64.decode(payload_b64) else { continue; };
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            // Browser → runner. JSON resize → ResizeTerminal envelope;
            // anything else (text or binary) → AgentInput envelope. Both
            // are best-effort: dropping a keystroke on a transient runner
            // disconnect is preferable to outboxing characters that arrive
            // out-of-order with the agent's response stream.
            msg = socket.recv() => {
                let Some(rid) = runner_id.as_ref() else {
                    // No runner — nothing to forward to. Keep the WS open
                    // so output bytes still flow if a runner reconnects
                    // and starts emitting.
                    if let Some(Err(_)) | None = msg { break; }
                    continue;
                };
                let envelope = match msg {
                    Some(Ok(Message::Text(text))) => {
                        if text.starts_with('{')
                            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                            && val.get("type").and_then(|t| t.as_str()) == Some("resize")
                        {
                            let cols = val.get("cols").and_then(|c| c.as_u64()).unwrap_or(120) as u16;
                            let rows = val.get("rows").and_then(|r| r.as_u64()).unwrap_or(40) as u16;
                            Envelope::best_effort(
                                "server".to_string(),
                                WireMessage::ResizeTerminal {
                                    agent_id: agent_id.clone(),
                                    cols,
                                    rows,
                                },
                            )
                        } else {
                            Envelope::best_effort(
                                "server".to_string(),
                                WireMessage::AgentInput {
                                    agent_id: agent_id.clone(),
                                    data: b64.encode(text.as_bytes()),
                                },
                            )
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        Envelope::best_effort(
                            "server".to_string(),
                            WireMessage::AgentInput {
                                agent_id: agent_id.clone(),
                                data: b64.encode(&data),
                            },
                        )
                    }
                    None | Some(Err(_)) => break,
                    _ => continue,
                };
                if let Ok(env_json) = serde_json::to_string(&envelope) {
                    let runners = state.runners.lock().await;
                    if let Some(runner) = runners.get(rid) {
                        let _ = runner.command_tx.send(env_json);
                    }
                }
            }
        }
    }
}
