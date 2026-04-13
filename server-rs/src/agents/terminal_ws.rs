use axum::{
    extract::{Query, State, ws::{Message, WebSocket, WebSocketUpgrade}},
    response::IntoResponse,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::agents::AgentRegistry;
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
    ws.on_upgrade(move |socket| handle_terminal(socket, query.agent, state.registry))
}

async fn handle_terminal(mut socket: WebSocket, agent_id: String, registry: AgentRegistry) {
    // Create a channel for PTY output → WebSocket
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Load buffered output from DB first (before locking agents)
    let buffered_rows: Vec<String> = {
        let db = registry.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT content FROM agent_output WHERE agent_id = ? AND message_type = 'pty' ORDER BY id")
            .unwrap();
        stmt.query_map(rusqlite::params![agent_id], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };

    // Try to attach to a live agent
    let attached = {
        let mut agents = registry.agents.lock().await;
        if let Some(agent) = agents.get_mut(&agent_id) {
            agent.terminals.push(output_tx);
            true
        } else {
            false
        }
    };

    // Send buffered output
    for row in &buffered_rows {
        if socket.send(Message::Text(row.clone().into())).await.is_err() {
            return;
        }
    }

    if !attached {
        // Check if agent is still running but detached (server restarted)
        let is_running = {
            let db = registry.db.lock().unwrap();
            db.query_row(
                "SELECT status FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .is_some_and(|s| s == "running" || s == "starting")
        };

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

    // Bidirectional: PTY output → WS, WS input → PTY
    loop {
        tokio::select! {
            // PTY output → send to browser
            Some(data) = output_rx.recv() => {
                let text = String::from_utf8_lossy(&data).to_string();
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            // Browser input → send to PTY
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Check for resize messages
                        if text.starts_with('{') {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&*text) {
                                if val.get("type").and_then(|t| t.as_str()) == Some("resize") {
                                    let cols = val.get("cols").and_then(|c| c.as_u64()).unwrap_or(120) as u16;
                                    let rows = val.get("rows").and_then(|r| r.as_u64()).unwrap_or(40) as u16;
                                    let mut agents = registry.agents.lock().await;
                                    if let Some(agent) = agents.get_mut(&agent_id) {
                                        if let Some(ref mut master) = agent.pty_master {
                                            master.resize(portable_pty::PtySize {
                                                rows,
                                                cols,
                                                pixel_width: 0,
                                                pixel_height: 0,
                                            }).ok();
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        // Regular input — forward to PTY
                        let mut agents = registry.agents.lock().await;
                        if let Some(agent) = agents.get_mut(&agent_id) {
                            if let Some(ref mut writer) = agent.pty_writer {
                                use std::io::Write;
                                writer.write_all(text.as_bytes()).ok();
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let mut agents = registry.agents.lock().await;
                        if let Some(agent) = agents.get_mut(&agent_id) {
                            if let Some(ref mut writer) = agent.pty_writer {
                                use std::io::Write;
                                writer.write_all(&data).ok();
                            }
                        }
                    }
                    None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}
