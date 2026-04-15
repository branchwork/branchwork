mod agents;
mod api;
mod auth;
mod auto_status;
mod ci;
mod config;
mod db;
mod file_watcher;
mod hooks;
mod mcp;
mod notifications;
mod plan_parser;
mod state;
mod static_files;
mod templates;
mod ws;

use axum::{
    Router,
    response::IntoResponse,
    routing::{delete, get, post},
};
use clap::Parser;
use config::{Cli, Command, Config};
use state::AppState;
use tower_http::cors::{Any, CorsLayer};

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
}

fn main() {
    let mut cli = Cli::parse();

    // `Session` must be dispatched before any tokio runtime starts — fork()
    // with an active multi-threaded runtime would leave the child in an
    // unusable state. So peel it off the enum first, then build the runtime,
    // then dispatch the remaining variants inside it.
    if matches!(cli.command, Some(Command::Session(_))) {
        let Some(Command::Session(args)) = cli.command.take() else {
            unreachable!();
        };
        if let Err(e) = agents::supervisor::run_session(args) {
            eprintln!("session daemon error: {e}");
            std::process::exit(1);
        }
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    match cli.command.take() {
        Some(Command::Mcp) => {
            // Reuse the same config/DB init as the server so stdio tools see
            // the same plans and state.
            let config = Config::from_cli(cli);
            let db = db::init(&config.db_path);
            let (broadcast_tx, _rx) = ws::create_broadcast();

            // Stdio mode still needs a registry for auto-advance triggered
            // by update_task_status. The spawned agents will outlive this
            // stdio process (they run under their own session daemons), so
            // auto-advance is still meaningful here.
            let sockets_dir = config.claude_dir.join("sessions");
            std::fs::create_dir_all(&sockets_dir).expect("failed to create sessions directory");
            let server_exe = std::env::current_exe().unwrap_or_else(|e| {
                eprintln!("[orchestrAI] current_exe() failed: {e} — falling back to argv[0]");
                std::path::PathBuf::from(
                    std::env::args()
                        .next()
                        .unwrap_or_else(|| "orchestrai-server".into()),
                )
            });
            let registry = agents::AgentRegistry::new(
                db.clone(),
                broadcast_tx.clone(),
                config.webhook_url.clone(),
                sockets_dir,
                server_exe,
                config.port,
            );

            let ctx = mcp::McpContext {
                plans_dir: config.plans_dir,
                db,
                broadcast_tx,
                registry,
                effort: std::sync::Arc::new(std::sync::Mutex::new(config.effort)),
                port: config.port,
            };
            if let Err(e) = rt.block_on(mcp::transport::run_stdio(ctx)) {
                eprintln!("mcp stdio error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Session(_)) => unreachable!("handled above"),
        None => rt.block_on(run_server(cli)),
    }
}

async fn run_server(cli: Cli) {
    let config = Config::from_cli(cli);
    let db = db::init(&config.db_path);
    let (broadcast_tx, _rx) = ws::create_broadcast();

    // Session daemons live under <claude_dir>/sessions/. Create on startup
    // so start_pty_agent doesn't have to per-spawn.
    let sockets_dir = config.claude_dir.join("sessions");
    std::fs::create_dir_all(&sockets_dir).expect("failed to create sessions directory");

    // Resolve our own binary path so we can respawn ourselves as the
    // `session` subcommand. Falls back to the clap-provided arg0 if
    // `current_exe()` isn't available for some reason.
    let server_exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("[orchestrAI] current_exe() failed: {e} — falling back to argv[0]");
        std::path::PathBuf::from(
            std::env::args()
                .next()
                .unwrap_or_else(|| "orchestrai-server".into()),
        )
    });

    // Agent registry
    let registry = agents::AgentRegistry::new(
        db.clone(),
        broadcast_tx.clone(),
        config.webhook_url.clone(),
        sockets_dir,
        server_exe,
        config.port,
    );
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
                let alive = agents::process_alive(pid);
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

    // Start CI status poller (best-effort; no-op if `gh` isn't installed)
    ci::spawn_poller(
        state.db.clone(),
        state.broadcast_tx.clone(),
        config.plans_dir.clone(),
    );

    // Start file watcher
    let _watcher =
        file_watcher::start(&config.plans_dir, broadcast_tx).expect("failed to start file watcher");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mcp_ctx = mcp::McpContext {
        plans_dir: state.plans_dir.clone(),
        db: state.db.clone(),
        broadcast_tx: state.broadcast_tx.clone(),
        registry: state.registry.clone(),
        effort: state.effort.clone(),
        port: state.port,
    };

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
        .route("/api/agents/{id}/finish", post(api::agents::finish_agent))
        .route("/api/drivers", get(api::agents::list_drivers))
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
            "/api/plans/{name}/budget",
            axum::routing::put(api::plans::set_budget),
        )
        .route(
            "/api/plans/{name}/auto-advance",
            axum::routing::put(api::plans::set_auto_advance),
        )
        .route(
            "/api/plans/{name}/tasks/{task_number}/status",
            axum::routing::put(api::plans::set_task_status),
        )
        .route("/api/plans/{name}/statuses", get(api::plans::get_statuses))
        .route(
            "/api/plans/{name}/tasks/{task_number}/learnings",
            get(api::plans::list_task_learnings).post(api::plans::add_task_learning),
        )
        .route("/api/plans/create", post(api::plans::create_plan))
        .route("/api/plans/convert-all", post(api::plans::convert_all))
        .route("/api/plans/{name}/convert", post(api::plans::convert_plan))
        .route(
            "/api/plans/{name}/auto-status",
            post(api::plans::auto_status),
        )
        .route(
            "/api/plans/{name}/reset-status",
            post(api::plans::reset_plan_status),
        )
        .route("/api/plans/{name}/check-all", post(api::plans::check_all))
        .route(
            "/api/plans/{name}/tasks/{task_number}/check",
            post(api::plans::check_task),
        )
        .route("/api/actions/start-task", post(api::plans::start_task))
        .route("/api/actions/fix-ci", post(api::ci::fix_ci))
        .route("/api/ci/{ci_run_id}/failure-log", get(api::ci::failure_log))
        .route(
            "/api/plans/{name}/phases/{phase_number}/start",
            post(api::plans::start_phase_tasks),
        )
        // Settings
        .route(
            "/api/settings",
            get(api::settings::get_settings).put(api::settings::put_settings),
        )
        .route("/api/folders", get(api::settings::list_folders))
        .route("/api/templates", get(templates::list_templates))
        // Auth
        .route("/api/auth/signup", post(auth::signup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        // Populate AuthUser on every request. Protected handlers opt in by
        // taking `AuthUser` as an extractor; public routes (health, login,
        // signup, static) are unaffected.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::populate_auth_user,
        ))
        // Static frontend (fallback)
        .fallback(get(static_files::serve_frontend))
        .with_state(state)
        // MCP server (streamable HTTP transport). Mounted via nest_service so
        // it runs alongside the dashboard API without sharing its AppState.
        .nest_service("/mcp", mcp::transport::build_http_service(mcp_ctx))
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
