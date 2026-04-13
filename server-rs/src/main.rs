mod agents;
mod api;
mod auto_status;
mod config;
mod db;
mod file_watcher;
mod hooks;
mod plan_parser;
mod state;
mod static_files;
mod ws;

use axum::{
    Router,
    response::IntoResponse,
    routing::{delete, get, post},
};
use clap::Parser;
use config::{Cli, Config};
use state::AppState;
use tower_http::cors::{Any, CorsLayer};

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
}

#[tokio::main]
async fn main() {
    let config = Config::from_cli(Cli::parse());
    let db = db::init(&config.db_path);
    let (broadcast_tx, _rx) = ws::create_broadcast();

    // Agent registry
    let registry = agents::AgentRegistry::new(db.clone(), broadcast_tx.clone());
    registry.cleanup_and_reattach().await;

    let state = AppState::new(&config, db.clone(), broadcast_tx.clone(), registry.clone());

    // Background: monitor detached agents (check PIDs every 30s)
    let db_monitor = db;
    let tx_monitor = broadcast_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let db = db_monitor.lock().unwrap();
            let mut stmt = db
                .prepare("SELECT id, pid FROM agents WHERE status IN ('running', 'starting')")
                .unwrap();
            let agents: Vec<(String, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .flatten()
                .collect();
            for (id, pid) in agents {
                let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
                if !alive {
                    db.execute(
                        "UPDATE agents SET status = 'completed', finished_at = datetime('now') WHERE id = ?",
                        rusqlite::params![id],
                    ).ok();
                    crate::ws::broadcast_event(
                        &tx_monitor,
                        "agent_stopped",
                        serde_json::json!({"id": id, "status": "completed", "exit_code": 0}),
                    );
                    println!(
                        "[orchestrAI] Detached agent {} (pid {}) finished",
                        &id[..8.min(id.len())],
                        pid
                    );
                }
            }
        }
    });

    // Start file watcher
    let _watcher =
        file_watcher::start(&config.plans_dir, broadcast_tx).expect("failed to start file watcher");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/health", get(health))
        .route("/hooks", post(hooks::receive_hook))
        .route("/ws", get(ws::ws_handler))
        .route("/terminal", get(agents::terminal_ws::terminal_ws_handler))
        // Agent routes (use registry from AppState)
        .route("/api/agents", get(api::agents::list_agents))
        .route(
            "/api/agents/{id}/output",
            get(api::agents::get_agent_output),
        )
        .route("/api/agents/{id}/diff", get(api::agents::get_agent_diff))
        .route(
            "/api/agents/{id}/merge",
            post(api::agents::merge_agent_branch),
        )
        .route(
            "/api/agents/{id}/discard",
            post(api::agents::discard_agent_branch),
        )
        .route("/api/agents/{id}", delete(api::agents::kill_agent))
        .route("/api/events", get(api::agents::get_events))
        // Plan routes
        .route("/api/plans", get(api::plans::list_plans))
        .route("/api/plans/sync-all", post(api::plans::sync_all))
        .route(
            "/api/plans/{name}",
            get(api::plans::get_plan).put(api::plans::update_plan),
        )
        .route(
            "/api/plans/{name}/project",
            axum::routing::put(api::plans::set_project),
        )
        .route(
            "/api/plans/{name}/tasks/{task_number}/status",
            axum::routing::put(api::plans::set_task_status),
        )
        .route("/api/plans/{name}/statuses", get(api::plans::get_statuses))
        .route("/api/plans/create", post(api::plans::create_plan))
        .route("/api/plans/convert-all", post(api::plans::convert_all))
        .route("/api/plans/{name}/convert", post(api::plans::convert_plan))
        .route(
            "/api/plans/{name}/auto-status",
            post(api::plans::auto_status),
        )
        .route(
            "/api/plans/{name}/tasks/{task_number}/check",
            post(api::plans::check_task),
        )
        .route("/api/actions/start-task", post(api::plans::start_task))
        // Settings
        .route(
            "/api/settings",
            get(api::settings::get_settings).put(api::settings::put_settings),
        )
        .route("/api/folders", get(api::settings::list_folders))
        // Static frontend (fallback)
        .fallback(get(static_files::serve_frontend))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!(
        "orchestrAI server listening on http://localhost:{} (effort: {}, claude-dir: {})",
        config.port,
        config.effort,
        config.claude_dir.display()
    );
    axum::serve(listener, app).await.unwrap();
}
