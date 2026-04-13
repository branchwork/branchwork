use axum::{Json, extract::State, response::IntoResponse};
use rusqlite::params;
use serde::Deserialize;

use crate::state::AppState;
use crate::ws::broadcast_event;

#[derive(Deserialize)]
pub struct HookEvent {
    session_id: Option<String>,
    hook_event_name: Option<String>,
    hook_type: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
}

/// POST /hooks
pub async fn receive_hook(
    State(state): State<AppState>,
    Json(event): Json<HookEvent>,
) -> impl IntoResponse {
    let session_id = event.session_id.as_deref().unwrap_or("unknown");
    let hook_type = event
        .hook_event_name
        .as_deref()
        .or(event.hook_type.as_deref())
        .unwrap_or("unknown");
    let tool_name = event.tool_name.as_deref();
    let tool_input = event.tool_input.as_ref().map(|v| v.to_string());

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO hook_events (session_id, hook_type, tool_name, tool_input) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, hook_type, tool_name, tool_input],
    )
    .unwrap();

    // Update agent last_tool if we track this session
    if let Some(tn) = tool_name {
        db.execute(
            "UPDATE agents SET last_tool = ?1, last_activity_at = datetime('now') WHERE session_id = ?2 AND status IN ('starting', 'running')",
            params![tn, session_id],
        )
        .ok();
    }

    drop(db); // release lock before broadcast

    broadcast_event(&state.broadcast_tx, "hook_event", serde_json::json!({
        "session_id": session_id,
        "hook_type": hook_type,
        "tool_name": tool_name,
        "tool_input": event.tool_input,
    }));

    Json(serde_json::json!({ "ok": true }))
}
