use axum::{
    extract::{State, WebSocketUpgrade, ws::Message},
    response::IntoResponse,
};
use tokio::sync::broadcast;

use crate::state::AppState;

/// Create a broadcast channel for dashboard events.
pub fn create_broadcast() -> (broadcast::Sender<String>, broadcast::Receiver<String>) {
    broadcast::channel(256)
}

/// Broadcast a typed event to all connected dashboard WebSocket clients.
pub fn broadcast_event(tx: &broadcast::Sender<String>, event_type: &str, data: serde_json::Value) {
    let msg = serde_json::json!({
        "type": event_type,
        "data": data,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    // Ignore send errors (no receivers connected).
    let _ = tx.send(msg.to_string());
}

/// GET /ws — dashboard WebSocket endpoint.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_dashboard_ws(socket, state))
}

async fn handle_dashboard_ws(
    mut socket: axum::extract::ws::WebSocket,
    state: AppState,
) {
    // Send initial connected message
    let connected = serde_json::json!({
        "type": "connected",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    if socket.send(Message::Text(connected.to_string().into())).await.is_err() {
        return;
    }

    // Subscribe to broadcast channel
    let mut rx = state.broadcast_tx.subscribe();

    loop {
        tokio::select! {
            // Forward broadcast events to this client
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[ws] client lagged, skipped {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Handle incoming messages from client (ping/pong, close)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {} // ignore other messages
                }
            }
        }
    }
}
