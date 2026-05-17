mod agents;
mod api;
mod audit;
mod auth;
mod auto_mode;
mod ci;
mod config;
mod db;
mod file_watcher;
mod git_helpers;
mod hooks;
mod mcp;
mod notifications;
mod persisted_settings;
mod plan_curate;
mod plan_parser;
mod repo_config;
mod saas;
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

/// `GET /health` and `GET /api/health`. Healthcheck (used by Docker /
/// Helm liveness probes) AND the read side for the admin Diagnostics
/// tab (Phase 9.2). The pre-existing `status` + `timestamp` fields are
/// preserved verbatim so external probes don't notice; the diagnostics
/// triage view consumes the additive `version`, `uptimeSeconds`, and
/// `wsConnections` fields.
///
/// `wsConnections` is the dashboard broadcast channel's current
/// receiver count — i.e. how many `/ws` clients are subscribed right
/// now. Runner WS connections (`/ws/runner`) are surfaced separately
/// via `/api/runners` so the diagnostics tab can render them as a
/// per-runner table.
async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": state.started_at.elapsed().as_secs(),
        "wsConnections": state.broadcast_tx.receiver_count(),
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
                eprintln!("[Branchwork] current_exe() failed: {e} — falling back to argv[0]");
                std::path::PathBuf::from(
                    std::env::args()
                        .next()
                        .unwrap_or_else(|| "branchwork-server".into()),
                )
            });
            let registry = agents::AgentRegistry::new(
                db.clone(),
                broadcast_tx.clone(),
                config.webhook_url.clone(),
                sockets_dir,
                server_exe,
                config.port,
                config.skip_permissions,
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
    let mut config = Config::from_cli(cli);
    let persisted = persisted_settings::PersistedSettings::load(&config.settings_path);
    config.apply_persisted(&persisted);
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
        eprintln!("[Branchwork] current_exe() failed: {e} — falling back to argv[0]");
        std::path::PathBuf::from(
            std::env::args()
                .next()
                .unwrap_or_else(|| "branchwork-server".into()),
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
        config.skip_permissions,
    );
    registry.cleanup_and_reattach().await;

    let state = AppState::new(&config, db.clone(), broadcast_tx.clone(), registry.clone());
    // Populate the registry's lazy back-reference so completion-side code
    // (e.g. `auto_mode::on_task_agent_completed` in `pty_agent::on_agent_exit`)
    // can dispatch through the shared runner registry. Set-once: the second
    // call would be a programmer error, so swallow the result silently.
    let _ = registry.app_state.set(state.clone());

    // Background: monitor PTY supervisor liveness every 30s. A daemon that
    // gets `kill -9`'d (OOM killer, operator typo, runaway debugger) leaves
    // the agent row at status='running' forever — `on_agent_exit` only
    // fires on socket EOF and a SIGKILL'd daemon may leave the kernel side
    // wedged before the read task observes EOF. The 30 s tick is the
    // backstop that flips dead-PID rows to `killed`/`supervisor_died` so
    // the dashboard unlocks the task card. Mode-gated to PTY because
    // remote rows live on a different host (no local PID to probe) and
    // stream-json rows have no supervisor at all (their status flips
    // directly through `check_agent`).
    let registry_monitor = registry.clone();
    let db_monitor = db;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            let agents: Vec<(String, i64)> = {
                let db = db_monitor.lock().unwrap();
                let mut stmt = match db.prepare(
                    "SELECT id, pid FROM agents \
                     WHERE status IN ('running', 'starting') \
                       AND mode = 'pty' AND pid IS NOT NULL",
                ) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .map(|it| it.flatten().collect())
                    .unwrap_or_default()
            };
            for (id, pid) in agents {
                if !agents::process_alive(pid) {
                    registry_monitor.mark_supervisor_died(
                        &id,
                        &format!("supervisor PID {pid} no longer alive"),
                    );
                }
            }
        }
    });

    // Start CI status poller (best-effort; no-op if `gh` isn't installed
    // and no runner is connected).
    ci::spawn_poller(state.clone());

    // One-shot startup backfill: re-evaluate every legacy `ci_runs` row
    // against the aggregate-aware dispatch path so single-run-path verdicts
    // (e.g. a passing Docker run shadowing a failing CI run on the same SHA)
    // get flipped to the correct multi-workflow verdict. Idempotent: a
    // settings-row gate makes subsequent boots a no-op. Detached so the
    // HTTP listener readiness probe is not blocked by the per-row dispatch.
    ci::spawn_backfill(state.clone());

    // Start the auto-mode idle poller (drivers without a Stop hook fall
    // back to a 60 s tick + idle threshold). Off by default; set
    // `BRANCHWORK_AUTO_FINISH_IDLE=1` to enable. Env vars are read once
    // here and cached for the process lifetime — reducing accidental
    // drift between poller iterations. Documented in
    // `docs/reference/configuration.md` under "Auto-mode (idle finish)".
    auto_mode::spawn_idle_poller(state.clone(), auto_mode::IdleFinishConfig::from_env());

    // Start the phase-end verify Check agent listener (Task 2.2). Always on
    // — it's a no-op for plans that don't configure `phase_verification`,
    // and for `phase_completed` events with a null `phase_sha` (manual
    // task completion paths). The listener subscribes to the dashboard
    // broadcast channel and spawns a Check agent + worktree per phase
    // boundary that has a verify command configured.
    agents::phase_check::spawn_listener(state.clone());

    // Hourly retention purger: hard-deletes plan_snapshots rows past
    // `expires_at` (and their archive YAML), audits one
    // `plan.snapshot_purged` row per snapshot.
    plan_curate::spawn_purger(state.clone());

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
        .route("/api/health/state", get(api::health::get_health_state))
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
            "/api/agents/{id}/merge-targets",
            get(api::agents::list_merge_targets),
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
        .route(
            "/api/plans/{name}",
            get(api::plans::get_plan)
                .put(api::plans::update_plan)
                .delete(api::plans::delete_plan),
        )
        .route("/api/plans/{name}/export", get(api::plans::export_plan))
        .route("/api/snapshots", get(api::plans::list_snapshots))
        .route("/api/snapshots/{id}", delete(api::plans::delete_snapshot))
        .route(
            "/api/snapshots/{id}/restore",
            post(api::plans::restore_snapshot),
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
            "/api/plans/{name}/config",
            get(api::plans::get_plan_config).put(api::plans::put_plan_config),
        )
        .route(
            "/api/plans/{name}/settings",
            get(api::plans::get_plan_settings).put(api::plans::put_plan_settings),
        )
        .route(
            "/api/plans/{name}/phases/{phase_number}/settings",
            get(api::plans::get_phase_settings).put(api::plans::put_phase_settings),
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
            "/api/plans/{name}/reset-status",
            post(api::plans::reset_plan_status),
        )
        .route(
            "/api/plans/{name}/tasks/{task_number}/reset-status",
            post(api::plans::reset_task_status),
        )
        .route(
            "/api/plans/{name}/branches/stale",
            get(api::plans::list_stale_branches),
        )
        .route(
            "/api/plans/{name}/branches/stale/purge",
            post(api::plans::purge_stale_branches),
        )
        .route("/api/plans/{name}/check", post(api::plans::check_plan))
        .route("/api/plans/{name}/check-all", post(api::plans::check_all))
        .route(
            "/api/plans/{name}/tasks/{task_number}/check",
            post(api::plans::check_task),
        )
        .route("/api/actions/start-task", post(api::plans::start_task))
        .route("/api/actions/fix-ci", post(api::ci::fix_ci))
        .route(
            "/api/ci/{ci_run_id}",
            axum::routing::delete(api::ci::dismiss_run),
        )
        .route("/api/ci/{ci_run_id}/failure-log", get(api::ci::failure_log))
        .route(
            "/api/plans/{name}/phases/{phase_number}/start",
            post(api::plans::start_phase_tasks),
        )
        .route(
            "/api/plans/{name}/start-session",
            post(api::plans::start_plan_session),
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
        // Organizations
        .route(
            "/api/orgs",
            get(auth::orgs::list_orgs).post(auth::orgs::create_org),
        )
        .route("/api/orgs/{slug}", get(auth::orgs::get_org))
        .route("/api/orgs/{slug}/members", post(auth::orgs::add_member))
        .route(
            "/api/orgs/{slug}/members/{user_id}",
            delete(auth::orgs::remove_member),
        )
        .route(
            "/api/orgs/{slug}/members/{user_id}/role",
            axum::routing::put(auth::orgs::update_member_role),
        )
        // SSO admin (per-org provider management)
        .route(
            "/api/orgs/{slug}/sso",
            get(auth::sso::list_providers).post(auth::sso::create_provider),
        )
        .route(
            "/api/orgs/{slug}/sso/{provider_id}",
            axum::routing::put(auth::sso::update_provider).delete(auth::sso::delete_provider),
        )
        // SSO public (login flow)
        .route(
            "/api/auth/sso/providers",
            get(auth::sso::discover_providers),
        )
        .route(
            "/api/auth/sso/{provider_id}/login",
            get(auth::sso::sso_login),
        )
        .route(
            "/api/auth/sso/{provider_id}/callback",
            get(auth::sso::oidc_callback),
        )
        .route(
            "/api/auth/sso/{provider_id}/saml/acs",
            post(auth::sso::saml_acs),
        )
        .route(
            "/api/auth/sso/{provider_id}/saml/metadata",
            get(auth::sso::saml_metadata),
        )
        // Org billing / budgets
        .route("/api/orgs/{slug}/usage", get(api::billing::get_usage))
        .route(
            "/api/orgs/{slug}/budget",
            get(api::billing::get_budget).put(api::billing::set_budget),
        )
        .route(
            "/api/orgs/{slug}/kill-switch",
            axum::routing::put(api::billing::toggle_kill_switch),
        )
        .route(
            "/api/orgs/{slug}/user-quotas",
            get(api::billing::list_user_quotas),
        )
        .route(
            "/api/orgs/{slug}/user-quotas/{user_id}",
            axum::routing::put(api::billing::set_user_quota),
        )
        // Audit log
        .route("/api/orgs/{slug}/audit-log", get(audit::list_audit_log))
        .route(
            "/api/orgs/{slug}/audit-log/export",
            get(audit::export_audit_log),
        )
        // Remote runners (SaaS)
        .route("/ws/runner", get(saas::runner_ws::runner_ws_handler))
        .route(
            "/api/runners/tokens",
            post(saas::runner_ws::create_runner_token),
        )
        // 4.8: one-line runner install — public script + auth-gated command
        // mint. Public script is templated with the dashboard's public URL
        // at request time so `curl|sh` lands a runner that knows where to
        // call home, no hand-edits needed.
        .route(
            "/install-runner.sh",
            get(saas::install_runner::serve_install_script),
        )
        .route(
            "/api/runners/install-command",
            post(saas::install_runner::issue_install_command),
        )
        .route("/api/runners", get(saas::runner_ws::list_runners))
        .route(
            "/api/runners/{runner_id}/commands",
            post(saas::runner_ws::send_runner_command),
        )
        .route(
            "/api/runners/{runner_id}/config",
            get(api::runners::get_runner_config).put(api::runners::put_runner_config),
        )
        .route(
            "/api/runners/{runner_id}/shutdown",
            post(api::runners::shutdown_runner),
        )
        .route(
            "/api/runners/{runner_id}",
            delete(api::runners::delete_runner),
        )
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
        "Branchwork server listening on http://localhost:{} (effort: {}, claude-dir: {})",
        config.port,
        config.effort,
        config.claude_dir.display()
    );
    axum::serve(listener, app).await.unwrap();
}
