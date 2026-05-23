//! PTY-mode agents.
//!
//! Each agent runs inside a detached session daemon (one per agent) that owns
//! a PTY running the `claude` CLI. The server process is a *client* of that
//! daemon: it connects via a local socket (Unix domain socket on Unix, named
//! pipe on Windows), receives framed `Output` messages, and forwards `Input`
//! / `Resize` / `Kill` in the other direction. Daemons survive server
//! restarts — this module's [`reattach_agent`] re-establishes the client
//! side during [`crate::agents::AgentRegistry::cleanup_and_reattach`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use interprocess::local_socket::ConnectOptions;
use interprocess::local_socket::tokio::prelude::*;
use rusqlite::params;
use tokio::sync::Notify;

use crate::agents::driver::{AgentDriver, SpawnOpts};
use crate::agents::session_protocol::{self, Message as SessionMessage};
use crate::agents::session_settings;
use crate::agents::supervisor;
use crate::agents::{
    AgentRegistry, ManagedAgent, git_checkout_branch, git_default_branch, git_head_sha,
};
use crate::config::Effort;
use crate::ws::broadcast_event;

/// Bound on the rolling accumulator used during readiness detection. Keeps
/// memory flat when the CLI emits a lot of early chatter.
const READINESS_BUFFER_CAP: usize = 16 * 1024;

pub struct StartPtyOpts<'a> {
    pub prompt: String,
    pub cwd: &'a Path,
    pub plan_name: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub effort: Effort,
    pub branch: Option<&'a str>,
    pub is_continue: bool,
    pub max_budget_usd: Option<f64>,
    /// Requested driver name (e.g. `"claude"`). Unknown / `None` values
    /// fall back to [`crate::agents::driver::DEFAULT_DRIVER`].
    pub driver: Option<&'a str>,
    /// User who spawned this agent (for per-user cost allocation).
    pub user_id: Option<&'a str>,
    /// Org this agent belongs to (for org-level budget tracking).
    pub org_id: Option<&'a str>,
    /// Explicit runner pin from the caller (T11.4). When set, the SaaS
    /// dispatcher routes the spawn to this runner, ignoring any
    /// `plan_runner_affinity` row. Used by the plan-creation flow
    /// (`NewPlanForm`) where no plan name exists yet, but the user has
    /// chosen a target runner. Standalone path ignores this field.
    pub runner_id: Option<&'a str>,
}

pub async fn start_pty_agent(registry: &AgentRegistry, opts: StartPtyOpts<'_>) -> String {
    let StartPtyOpts {
        prompt,
        cwd,
        plan_name,
        task_id,
        effort,
        branch,
        is_continue,
        max_budget_usd,
        driver: driver_name,
        user_id,
        org_id,
        runner_id: _runner_id, // standalone path ignores it
    } = opts;
    let default_driver = if let Some(app_state) = registry.app_state.get() {
        crate::persisted_settings::PersistedSettings::load(&app_state.settings_path)
            .default_driver()
            .to_string()
    } else {
        "claude".to_string()
    };
    let (driver_name, driver) = registry
        .drivers
        .get_or_default_with(driver_name, Some(&default_driver));
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Capture base commit BEFORE switching to the task branch.
    let base_commit = git_head_sha(cwd);

    // Record the canonical merge target rather than whatever
    // branch HEAD happens to be on — see plan
    // merge-target-canonical-default-branch. This field is now
    // informational; the merge resolver re-resolves at merge
    // time, but the UI uses this to seed the default selection
    // in the merge dropdown.
    let source_branch = git_default_branch(cwd);

    // Checkout the task branch if specified
    if let Some(branch_name) = branch {
        git_checkout_branch(cwd, branch_name, is_continue);
    }

    // Insert into DB (socket path filled in once the daemon reports its PID)
    let socket_path = registry.socket_for(&id);
    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt, base_commit, branch, source_branch, supervisor_socket, driver, user_id, org_id)
             VALUES (?1, ?2, ?3, 'starting', 'pty', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                session_id,
                cwd.to_str().unwrap_or(""),
                plan_name,
                task_id,
                prompt,
                base_commit,
                branch,
                source_branch,
                socket_path.to_string_lossy().to_string(),
                driver_name,
                user_id,
                org_id.unwrap_or("default-org"),
            ],
        )
        .ok();
    }

    // If the driver supports MCP config injection, materialise the config
    // file alongside the agent's socket so `--mcp-config` can point at it.
    // Failure to write is non-fatal: the agent still runs, just without
    // Branchwork tool access. Log and continue.
    // Standalone: agent runs on the same host as the server, so loopback
    // is the right base URL. SaaS dispatch lives in
    // `spawn_ops::start_agent_via_runner` and uses the public dashboard
    // URL there — see Task 5.7 / ADR 0003 §SaaS for context.
    let mcp_url = format!("http://127.0.0.1:{}/mcp", registry.port);
    let mcp_config_path = driver.mcp_config_json(&mcp_url).and_then(|json| {
        let path = registry.mcp_config_for(&id);
        match std::fs::write(&path, &json) {
            Ok(()) => Some(path),
            Err(e) => {
                eprintln!(
                    "[agent {}] Failed to write MCP config at {}: {e} — continuing without MCP injection",
                    &id[..8.min(id.len())],
                    path.display()
                );
                None
            }
        }
    });

    // If the driver declares a Stop hook (Claude only today), write a
    // per-session settings file so the spawned CLI POSTs to /hooks on exit.
    // Same best-effort posture as the MCP config: log and continue on error.
    let hook_url = format!("http://localhost:{}/hooks", registry.port);
    let settings_path = match session_settings::write_for_agent(
        &session_id,
        driver.as_ref(),
        &hook_url,
    ) {
        Ok(maybe_path) => maybe_path,
        Err(e) => {
            eprintln!(
                "[agent {}] Failed to write per-session settings: {e} — continuing without Stop hook",
                &id[..8.min(id.len())],
            );
            None
        }
    };

    // Build the CLI argv via the driver. No shell involved — `portable-pty`
    // spawns it directly in the daemon, so we don't need to escape spaces.
    let skip_permissions = registry
        .skip_permissions
        .load(std::sync::atomic::Ordering::Relaxed);
    let cli_cmd = driver.spawn_args(&SpawnOpts {
        session_id: &session_id,
        cwd,
        effort,
        max_budget_usd,
        mcp_config_path: mcp_config_path.as_deref(),
        settings_path: settings_path.as_deref(),
        skip_permissions,
    });
    let extra_env = driver.extra_env();
    let formatted_prompt = driver.format_prompt(&prompt);

    let daemon_pid = match supervisor::spawn_session_daemon(
        &registry.server_exe,
        &socket_path,
        cwd,
        120,
        40,
        &extra_env,
        &cli_cmd,
    )
    .await
    {
        Ok(pid) => pid,
        Err(e) => {
            let err_msg = format!("supervisor spawn error: {e}");
            eprintln!(
                "[agent {}] Failed to start session daemon: {err_msg}",
                &id[..8]
            );
            let db = registry.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                params![id],
            )
            .ok();
            broadcast_event(
                &registry.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": id, "status": "failed", "error": err_msg}),
            );
            return id;
        }
    };

    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET pid = ?1, status = 'running' WHERE id = ?2",
            params![daemon_pid as i64, id],
        )
        .ok();
    }

    broadcast_event(
        &registry.broadcast_tx,
        "agent_started",
        serde_json::json!({"id": id, "sessionId": session_id, "planName": plan_name, "taskId": task_id, "pid": daemon_pid, "mode": "pty"}),
    );

    // Connect to the daemon and wire up reader + writer tasks. Any failure
    // here means the daemon is running but unreachable from us — that's
    // worth a 'failed' so the UI can let the user kill it.
    if let Err(e) = connect_and_wire(registry, &id, &socket_path).await {
        eprintln!(
            "[agent {}] Failed to connect to session daemon at {}: {e}",
            &id[..8],
            socket_path.display()
        );
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
            params![id],
        )
        .ok();
        broadcast_event(
            &registry.broadcast_tx,
            "agent_stopped",
            serde_json::json!({"id": id, "status": "failed", "error": format!("connect: {e}")}),
        );
        return id;
    }

    // Kick off the prompt-injection task. It subscribes to the live output
    // broadcast so it sees every byte the PTY emits, and drops out either
    // when the driver reports readiness or after a fallback timeout.
    {
        let agents_guard = registry.agents.lock().await;
        if let Some(agent) = agents_guard.get(&id) {
            let output_rx = agent.output_tx.subscribe();
            let command_tx = agent.command_tx.clone();
            let agent_id_short = id[..8.min(id.len())].to_string();
            let driver = driver.clone();
            tokio::spawn(async move {
                inject_prompt_when_ready(
                    driver,
                    output_rx,
                    command_tx,
                    formatted_prompt,
                    agent_id_short,
                )
                .await;
            });
        }
    }

    id
}

/// Reattach to a running session daemon after a server restart. Returns
/// true when the socket connect goes through and the in-process
/// `ManagedAgent` is re-registered. The daemon's PID stays in the `pid`
/// DB column; this function doesn't need it.
pub async fn reattach_agent(
    registry: &AgentRegistry,
    agent_id: &str,
    socket_path: &Path,
    _daemon_pid: u32,
) -> bool {
    match connect_and_wire(registry, agent_id, socket_path).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[agent {}] reattach failed for {}: {e}",
                &agent_id[..8.min(agent_id.len())],
                socket_path.display()
            );
            false
        }
    }
}

/// Connect to the supervisor's local socket, spawn the reader+writer tasks,
/// and register the live `ManagedAgent`. Idempotent if called twice for the
/// same agent id — the second registration wins.
async fn connect_and_wire(
    registry: &AgentRegistry,
    agent_id: &str,
    socket_path: &Path,
) -> std::io::Result<()> {
    let name = supervisor::socket_name(socket_path)?;
    let stream = ConnectOptions::new().name(name).connect_tokio().await?;
    let (read_half, write_half) = stream.split();

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<SessionMessage>();
    let (output_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(1024);

    // Shared heartbeat state: reader resets the miss counter on any inbound
    // frame; heartbeat task increments before each Ping and gives up after
    // three consecutive misses (~45s) without a response. `cancel` is fired
    // when the reader exits so the heartbeat doesn't outlive the session.
    let unanswered = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(Notify::new());

    spawn_writer_task(write_half, command_rx);
    spawn_reader_task(
        registry.clone(),
        agent_id.to_string(),
        read_half,
        output_tx.clone(),
        unanswered.clone(),
        cancel.clone(),
    );
    spawn_heartbeat_task(
        registry.clone(),
        agent_id.to_string(),
        command_tx.clone(),
        unanswered,
        cancel,
    );

    let agent = ManagedAgent {
        command_tx,
        output_tx,
    };
    registry
        .agents
        .lock()
        .await
        .insert(agent_id.to_string(), agent);

    Ok(())
}

/// Drain `command_rx` and frame each message out to the daemon. Ends when
/// the sender side drops (registry eviction) or the socket write fails.
fn spawn_writer_task(
    write_half: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    mut command_rx: tokio::sync::mpsc::UnboundedReceiver<SessionMessage>,
) {
    tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(msg) = command_rx.recv().await {
            if session_protocol::write_frame(&mut write_half, &msg)
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

/// Read framed messages from the daemon until EOF, forwarding `Output`
/// bytes to the output broadcast and to `agent_output` in the DB. On EOF
/// we treat the agent as completed (mirrors the tmux-era behaviour where
/// the `tmux attach` reader thread exiting meant the session was gone).
fn spawn_reader_task(
    registry: AgentRegistry,
    agent_id: String,
    read_half: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    unanswered: Arc<AtomicU32>,
    cancel: Arc<Notify>,
) {
    tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            match session_protocol::read_frame(&mut read_half).await {
                Ok(SessionMessage::Output(bytes)) => {
                    // Any inbound frame means the supervisor is alive.
                    unanswered.store(0, Ordering::SeqCst);
                    // Persist; use lossy so bad utf-8 doesn't drop the chunk.
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    {
                        let db = registry.db.lock().unwrap();
                        db.execute(
                            "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, 'pty', ?2)",
                            params![agent_id, text],
                        )
                        .ok();
                    }
                    // Broadcast to attached terminals. `send` only errors
                    // when there are zero subscribers; that's fine.
                    let _ = output_tx.send(bytes);
                }
                Ok(_) => {
                    // Pong / non-Output messages: count as liveness signal
                    // but don't persist or forward.
                    unanswered.store(0, Ordering::SeqCst);
                }
                Err(_) => break,
            }
        }

        // Release the heartbeat task before invoking exit; on_agent_exit
        // may touch registry state the heartbeat also reads.
        cancel.notify_waiters();
        // Supervisor closed the connection: treat as agent completion.
        on_agent_exit(&registry, &agent_id).await;
    });
}

/// Send a `Ping` every 15 s and count unanswered ones. Three consecutive
/// misses (~45 s of silence) means the supervisor is unreachable — likely
/// wedged without closing the socket — so mark the agent stopped and
/// broadcast. Exits cleanly when `cancel` is notified by the reader on EOF.
///
/// Order matters: we send first, then sleep, then check. With a 15 s
/// interval and 3-miss threshold this yields detection at ~45 s after the
/// supervisor goes silent (ping at t=0 lost, ping at t=15 lost, ping at
/// t=30 lost, check at t=45 trips the threshold). Sleeping first would
/// push the first ping to t=15 and detection to t=60.
fn spawn_heartbeat_task(
    registry: AgentRegistry,
    agent_id: String,
    command_tx: tokio::sync::mpsc::UnboundedSender<SessionMessage>,
    unanswered: Arc<AtomicU32>,
    cancel: Arc<Notify>,
) {
    const INTERVAL: Duration = Duration::from_secs(15);
    const MAX_MISSES: u32 = 3;
    tokio::spawn(async move {
        loop {
            if unanswered.load(Ordering::SeqCst) >= MAX_MISSES {
                registry.mark_supervisor_unreachable(&agent_id).await;
                break;
            }
            unanswered.fetch_add(1, Ordering::SeqCst);
            // If the writer's receiver has dropped (agent killed / evicted
            // from the registry) the send fails and we're done.
            if command_tx.send(SessionMessage::Ping).is_err() {
                break;
            }
            tokio::select! {
                _ = cancel.notified() => break,
                _ = tokio::time::sleep(INTERVAL) => {}
            }
        }
    });
}

/// Returns `Some(true)` if `branch` has zero commits ahead of the canonical
/// default branch in `cwd`, `Some(false)` if it has at least one, and `None`
/// if either branch fails to resolve (default unknown, branch missing, repo
/// not a git workdir, …). Sole consumer is the unattended-contract
/// diagnostic in [`on_agent_exit`].
fn branch_has_no_commits_ahead_of_trunk(cwd: &Path, branch: &str) -> Option<bool> {
    let default = git_default_branch(cwd)?;
    let out = std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{default}..{branch}")])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let count: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(count == 0)
}

/// Shared exit handler for both "supervisor closed the socket on us" and
/// explicit kill paths. Updates status, parses cost, emits WS events,
/// optionally notifies a webhook, and enforces plan budgets.
async fn on_agent_exit(registry: &AgentRegistry, agent_id: &str) {
    // Look up which driver spawned this agent so we parse cost using the
    // right regex / summary format. NULL rows (pre-driver-column) fall back
    // to the default driver via `get_or_default`.
    let driver_name: Option<String> = {
        let db = registry.db.lock().unwrap();
        db.query_row(
            "SELECT driver FROM agents WHERE id = ?1",
            params![agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };
    let default_driver = if let Some(app_state) = registry.app_state.get() {
        crate::persisted_settings::PersistedSettings::load(&app_state.settings_path)
            .default_driver()
            .to_string()
    } else {
        "claude".to_string()
    };
    let (_, driver) = registry
        .drivers
        .get_or_default_with(driver_name.as_deref(), Some(&default_driver));
    let cost_usd = parse_cost_from_pty_output(driver.as_ref(), &registry.db, agent_id);

    // Pull plan/task/branch for the eventual notification before we mutate
    // the row, since those columns don't change on completion.
    let meta: Option<(Option<String>, Option<String>, Option<String>)> = {
        let db = registry.db.lock().unwrap();
        db.query_row(
            "SELECT plan_name, task_id, branch FROM agents WHERE id = ?1",
            params![agent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    };

    // Distinguish clean supervisor exit from a crash. The daemon removes
    // its pidfile as the final step of orderly shutdown (PTY child exited,
    // or we sent it a `Kill`); a SIGKILL skips that cleanup and leaves the
    // pidfile on disk. Reading its presence right before we unlink it below
    // gives us a reliable crash signal — much faster than waiting for the
    // heartbeat to trip (~45 s), which the acceptance test does not.
    let socket_path = registry.socket_for(agent_id);
    let supervisor_crashed = supervisor::pidfile_path(&socket_path).exists();

    // The supervisor writes the PTY child's exit code to a sibling file
    // just before unlinking the pidfile; absent file means "old daemon,
    // no signal" and we fall back to the pre-fix completed default.
    // Non-zero exit means the agent process died with an error
    // (panic / scripted_agent crash / real-claude SIGKILL during a hook /
    // ...) and the row must surface that rather than masquerade as a
    // clean completion. See the diamond_dispatch harness's
    // `agent_crash_before_commit` + `agent_crash_after_commit` probes.
    let child_exit_code: Option<i32> =
        std::fs::read_to_string(supervisor::exitcode_path(&socket_path))
            .ok()
            .and_then(|s| s.trim().parse().ok());

    // Only flip `running → <terminal>`. If the row is already `killed` (from
    // kill_agent) or `failed` we leave the terminal status alone — but we
    // still stamp cost_usd so the UI reports spend accurately regardless of
    // how the agent ended.
    let (new_status, stop_reason): (&str, Option<String>) = if supervisor_crashed {
        ("failed", Some("supervisor_unreachable".to_string()))
    } else if let Some(c) = child_exit_code
        && c != 0
    {
        ("failed", Some(format!("agent_exit_{c}")))
    } else {
        ("completed", None)
    };
    let marked = {
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET cost_usd = ?2 WHERE id = ?1 AND cost_usd IS NULL",
            params![agent_id, cost_usd],
        )
        .ok();
        let n = db
            .execute(
                "UPDATE agents SET status = ?1, stop_reason = ?2, finished_at = datetime('now') \
                 WHERE id = ?3 AND status = 'running'",
                params![new_status, stop_reason, agent_id],
            )
            .unwrap_or(0);
        n > 0
    };

    let over_budget_plan: Option<String> = {
        let db = registry.db.lock().unwrap();
        db.query_row(
            "SELECT a.plan_name FROM agents a \
             JOIN plan_budget b ON b.plan_name = a.plan_name \
             WHERE a.id = ?1 \
               AND (SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
                    WHERE plan_name = a.plan_name AND cost_usd IS NOT NULL) >= b.max_budget_usd",
            params![agent_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };

    registry.agents.lock().await.remove(agent_id);

    // Clean up the per-agent socket / pidfile / exitcode siblings. Log
    // stays so the full transcript is still recoverable post-mortem.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(supervisor::pidfile_path(&socket_path));
    let _ = std::fs::remove_file(supervisor::exitcode_path(&socket_path));

    // Clean up the per-session settings file written by `start_pty_agent`.
    // Best-effort, mirrors the silence of the socket/pidfile lines above:
    // drivers without a Stop hook never wrote one, so the deleter treats
    // NotFound as Ok.
    let session_id_for_cleanup: Option<String> = {
        let db = registry.db.lock().unwrap();
        db.query_row(
            "SELECT session_id FROM agents WHERE id = ?1",
            params![agent_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };
    if let Some(session_id) = session_id_for_cleanup.as_deref() {
        let _ = session_settings::delete_for_agent(session_id);
    }

    // Only broadcast when we actually flipped the row. Otherwise kill_agent
    // already emitted an `agent_stopped:killed` event and a duplicate here
    // would confuse the dashboard.
    if marked {
        let mut payload = serde_json::json!({"id": agent_id, "status": new_status});
        if let Some(reason) = stop_reason {
            payload["stop_reason"] = serde_json::Value::String(reason.to_string());
            payload["reason"] =
                serde_json::Value::String("supervisor exited without clean shutdown".to_string());
        } else {
            payload["exit_code"] = serde_json::Value::from(0);
        }
        broadcast_event(&registry.broadcast_tx, "agent_stopped", payload);
    }

    // Auto-mode unattended-contract diagnostic. If the agent exited cleanly
    // (no supervisor crash, row freshly flipped to `completed`) but its
    // task branch has no commits ahead of the canonical default, log a
    // visible line so the cause shows up in the server log without needing
    // to inspect the diff. The actual pause is owned by the no-commit
    // guard wired in by the auto-mode-loop completion handler — this is
    // diagnostics only.
    if marked
        && !supervisor_crashed
        && let Some((_, _, Some(branch))) = meta.as_ref()
    {
        let cwd: Option<String> = {
            let db = registry.db.lock().unwrap();
            db.query_row(
                "SELECT cwd FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        if let Some(cwd) = cwd
            && let Some(true) =
                branch_has_no_commits_ahead_of_trunk(std::path::Path::new(&cwd), branch)
        {
            eprintln!(
                "[auto_mode] agent {agent_id} exited clean but left no commits — likely violated the unattended contract"
            );
        }
    }

    // Auto-mode merge-on-completion hook. Fires only on clean exit with
    // both `plan_name` and `task_id` present; the auto-mode helper itself
    // gates on `auto_mode_enabled` so this is a no-op for plans that
    // haven't opted in. `app_state` is unset in test fixtures that build
    // only an `AgentRegistry`, so those tests skip the hook silently.
    if marked
        && !supervisor_crashed
        && let Some((Some(plan), Some(task), _)) = meta.as_ref()
        && let Some(state) = registry.app_state.get()
    {
        crate::auto_mode::on_task_agent_completed(state, agent_id, plan, task).await;
    }

    // Webhook only when we cleanly completed; kill_agent owns user-visible
    // messaging for the kill path, and supervisor crashes are already
    // surfaced via the WS event.
    let webhook_snapshot = registry.webhook_url.read().unwrap().clone();
    if marked && !supervisor_crashed && webhook_snapshot.is_some() {
        let (plan, task, branch) = meta.unwrap_or((None, None, None));
        let msg = crate::notifications::agent_completion_message(
            plan.as_deref(),
            task.as_deref(),
            agent_id,
            "completed",
            branch.as_deref(),
            cost_usd,
        );
        crate::notifications::notify(webhook_snapshot.clone(), msg);
    }

    if let Some(plan) = over_budget_plan {
        kill_plan_agents(registry, &plan).await;
    }

    // ── Org-level budget enforcement ───────────────────────────────────
    // Check thresholds and fire alerts when an agent finishes with a cost.
    if cost_usd.is_some() {
        let db = registry.db.lock().unwrap();
        let org_id: Option<String> = db
            .query_row(
                "SELECT org_id FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if let Some(org_id) = org_id {
            crate::saas::billing::enforce_org_budget(&db, &org_id, webhook_snapshot.as_deref());
        }
    }
}

/// Kill all running agents belonging to a plan that has exceeded its budget.
/// Marks them as 'killed' with reason 'budget_exceeded'.
async fn kill_plan_agents(registry: &AgentRegistry, plan_name: &str) {
    let victims: Vec<String> = {
        let db = registry.db.lock().unwrap();
        let mut stmt = match db.prepare(
            "SELECT id FROM agents \
             WHERE plan_name = ? AND status IN ('running', 'starting')",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))
            .ok()
            .map(|rs| rs.flatten().collect())
            .unwrap_or_default()
    };

    for id in victims {
        let agent = registry.agents.lock().await.remove(&id);
        if let Some(agent) = agent {
            let _ = agent.command_tx.send(SessionMessage::Kill);
        }
        // Even if we don't have an in-process handle, fall back to the
        // daemon PID from the DB so a dangling row doesn't stay 'running'.
        let pid = {
            let db = registry.db.lock().unwrap();
            db.query_row("SELECT pid FROM agents WHERE id = ?", params![id], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .ok()
            .flatten()
        };
        if let Some(p) = pid {
            crate::agents::process_terminate(p);
        }
        {
            let db = registry.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?",
                params![id],
            )
            .ok();
        }
        broadcast_event(
            &registry.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": id,
                "status": "killed",
                "reason": "budget_exceeded",
            }),
        );
        println!(
            "[Branchwork] Killed agent {} -- plan '{}' exceeded budget",
            &id[..8.min(id.len())],
            plan_name
        );
    }
}

/// Watch the supervisor's output stream until the driver reports the CLI
/// is ready for input, then inject the initial task prompt as keystrokes.
/// Falls back to sending the prompt after ~16s so users aren't permanently
/// stuck on an unrecognised splash screen.
async fn inject_prompt_when_ready(
    driver: Arc<dyn AgentDriver>,
    mut output_rx: tokio::sync::broadcast::Receiver<Vec<u8>>,
    command_tx: tokio::sync::mpsc::UnboundedSender<SessionMessage>,
    prompt: String,
    agent_id_short: String,
) {
    use tokio::sync::broadcast::error::RecvError;

    let mut acc: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(16);

    let prompt_seen = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break false,
            res = output_rx.recv() => {
                match res {
                    Ok(bytes) => {
                        acc.extend_from_slice(&bytes);
                        if acc.len() > READINESS_BUFFER_CAP {
                            let cut = acc.len() - READINESS_BUFFER_CAP;
                            acc.drain(..cut);
                        }
                        if driver.is_ready(&acc) {
                            break true;
                        }
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break false,
                }
            }
        }
    };

    // Small settle so claude has finished rendering the prompt line before
    // we start sending keystrokes.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    if !prompt_seen {
        println!("[agent {agent_id_short}] readiness timeout — injecting prompt anyway",);
    }

    let _ = command_tx.send(SessionMessage::Input(prompt.into_bytes()));
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // `\r` matches what a real Enter keystroke delivers through a PTY.
    let _ = command_tx.send(SessionMessage::Input(b"\r".to_vec()));
}

/// Pull the last ~50 PTY output lines for `agent_id` out of the DB, join
/// them, and defer the actual parsing to the driver. Drivers that don't
/// report cost (or whose CLI was interrupted before printing a summary)
/// return `None`.
fn parse_cost_from_pty_output(
    driver: &dyn AgentDriver,
    db: &crate::db::Db,
    agent_id: &str,
) -> Option<f64> {
    let db = db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT content FROM agent_output WHERE agent_id = ? ORDER BY id DESC LIMIT 50")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map(params![agent_id], |row| row.get(0))
        .unwrap()
        .flatten()
        .collect();

    let combined = rows.into_iter().rev().collect::<String>();
    driver.parse_cost(&combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::db::Db;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        (crate::db::init(&path), dir)
    }

    fn test_registry(
        db: Db,
        sockets_dir: PathBuf,
    ) -> (AgentRegistry, tokio::sync::broadcast::Receiver<String>) {
        let (tx, rx) = tokio::sync::broadcast::channel::<String>(32);
        let registry = AgentRegistry::new(
            db,
            tx,
            None,
            sockets_dir,
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        (registry, rx)
    }

    fn insert_running_agent(db: &Db, id: &str, socket: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, pid, supervisor_socket) \
             VALUES (?1, '/tmp', 'running', 'pty', 1, ?2)",
            params![id, socket],
        )
        .unwrap();
    }

    fn status_and_reason(db: &Db, id: &str) -> (String, Option<String>) {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status, stop_reason FROM agents WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
    }

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    /// Clean-exit path: the supervisor's orderly shutdown removes its
    /// pidfile before exiting. on_agent_exit must see "pidfile missing" and
    /// mark the agent as `completed`, not `supervisor_unreachable`.
    #[tokio::test]
    async fn on_agent_exit_marks_completed_when_pidfile_missing() {
        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone(), sockets_dir.clone());

        let agent_id = "clean-exit";
        let socket = sockets_dir.join(format!("{agent_id}.sock"));
        insert_running_agent(&db, agent_id, &socket.to_string_lossy());
        // No pidfile on disk: simulates the supervisor running its normal
        // cleanup (remove pidfile → remove socket → exit).

        on_agent_exit(&registry, agent_id).await;

        let (status, reason) = status_and_reason(&db, agent_id);
        assert_eq!(status, "completed", "pidfile absent => clean exit");
        assert_eq!(reason, None);

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains(agent_id)
                && e.contains("completed")),
            "expected completed broadcast: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.contains("supervisor_unreachable")),
            "should not mention supervisor_unreachable: {events:?}"
        );
    }

    /// Crash path: the supervisor was SIGKILL'd mid-flight and left its
    /// pidfile sibling on disk. on_agent_exit must read that as "supervisor
    /// died without cleaning up" and mark the agent `failed` /
    /// `supervisor_unreachable` — which is what unlocks the task card.
    #[tokio::test]
    async fn on_agent_exit_marks_supervisor_unreachable_when_pidfile_present() {
        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone(), sockets_dir.clone());

        let agent_id = "crashed";
        let socket = sockets_dir.join(format!("{agent_id}.sock"));
        insert_running_agent(&db, agent_id, &socket.to_string_lossy());
        // Supervisor didn't run cleanup, so the pidfile is still on disk.
        std::fs::write(supervisor::pidfile_path(&socket), "99999").unwrap();

        on_agent_exit(&registry, agent_id).await;

        let (status, reason) = status_and_reason(&db, agent_id);
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("supervisor_unreachable"));

        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains(agent_id)
                && e.contains("supervisor_unreachable")),
            "expected supervisor_unreachable broadcast: {events:?}"
        );

        // Side-effect: the on_agent_exit cleanup also removes the pidfile
        // so a future reboot doesn't mistake it for a live daemon.
        assert!(
            !supervisor::pidfile_path(&socket).exists(),
            "on_agent_exit should remove the stale pidfile"
        );
    }

    fn git_init_master_with_initial_commit(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        run(&["init", "-b", "master"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
    }

    /// Diagnostic predicate: a task branch sitting at master's tip has zero
    /// commits ahead of trunk and should trip the unattended-contract log.
    #[test]
    fn branch_has_no_commits_ahead_when_branch_at_trunk_tip() {
        let dir = TempDir::new().unwrap();
        git_init_master_with_initial_commit(dir.path());
        let ok = std::process::Command::new("git")
            .args(["branch", "feature/empty"])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "failed to create feature branch");
        assert_eq!(
            branch_has_no_commits_ahead_of_trunk(dir.path(), "feature/empty"),
            Some(true)
        );
    }

    /// Inverse: a branch with at least one commit should NOT trip the
    /// diagnostic. Guards against false positives that would spam the log
    /// for every well-behaved agent.
    #[test]
    fn branch_has_commits_returns_some_false() {
        let dir = TempDir::new().unwrap();
        git_init_master_with_initial_commit(dir.path());
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok);
        };
        run(&["checkout", "-b", "feature/work"]);
        run(&["commit", "--allow-empty", "-m", "real work"]);
        assert_eq!(
            branch_has_no_commits_ahead_of_trunk(dir.path(), "feature/work"),
            Some(false)
        );
    }

    /// Non-git cwd: the predicate must return None so the diagnostic stays
    /// silent (default branch is unknown — we can't make a claim).
    #[test]
    fn branch_has_no_commits_returns_none_for_non_git_cwd() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            branch_has_no_commits_ahead_of_trunk(dir.path(), "anything"),
            None
        );
    }

    // The settings-file lifecycle tests below redirect `dirs::home_dir()` to
    // a tempdir by overriding `$HOME`. That works on Unix because the dirs
    // crate reads `$HOME`, but on Windows `dirs::home_dir()` calls
    // `SHGetKnownFolderPath(FOLDERID_Profile)` which ignores env vars — the
    // override has no effect, so the production deleter unlinks a path under
    // the runner's real profile while the test plants and asserts on the
    // tempdir path. Cross-platform coverage of the writer/deleter lives in
    // `agents::session_settings::tests::*`, which take an explicit `home`
    // argument. The tests below pin only the wiring (`on_agent_exit` calls
    // `delete_for_agent`, `start_pty_agent` calls `write_for_agent`) — gating
    // them off on Windows leaves no production behaviour unverified.
    /// Scope-restored override of `$HOME` so that `dirs::home_dir()` calls
    /// inside `on_agent_exit` resolve to a tempdir for the test's duration.
    /// Tests that touch `$HOME` must acquire [`HOME_TEST_LOCK`] before
    /// constructing one — cargo runs tests in a thread pool, and a second
    /// `HomeOverride` racing the first would silently flip the env var out
    /// from under the in-flight test.
    #[cfg(not(windows))]
    struct HomeOverride {
        prev: Option<std::ffi::OsString>,
    }

    #[cfg(not(windows))]
    impl HomeOverride {
        fn set(new_home: &Path) -> Self {
            let prev = std::env::var_os("HOME");
            // SAFETY: edition 2024 marks `set_var` unsafe because of
            // concurrent-reader hazards. See struct doc-comment for why
            // the residual race is tolerable here.
            unsafe {
                std::env::set_var("HOME", new_home);
            }
            Self { prev }
        }
    }

    #[cfg(not(windows))]
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            // SAFETY: see `HomeOverride::set`.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// Serialises `$HOME`-mutating tests in this module. `cargo test` runs
    /// test fns concurrently across a thread pool; without this lock,
    /// overlapping `HomeOverride` scopes would clobber each other. We use
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) so it's safe to hold
    /// the guard across `.await` points without tripping
    /// `clippy::await_holding_lock`.
    #[cfg(not(windows))]
    static HOME_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Take the home-test lock. `tokio::sync::Mutex` has no poisoning
    /// semantics, so a panicked predecessor releases the lock cleanly.
    #[cfg(not(windows))]
    async fn lock_home() -> tokio::sync::MutexGuard<'static, ()> {
        HOME_TEST_LOCK.lock().await
    }

    /// Settings-cleanup path: the supervisor wrote a per-session
    /// `~/.claude/sessions/<session_id>.settings.json` at spawn time, and
    /// `on_agent_exit` must remove it so a future spawn with the same
    /// session_id doesn't re-use stale config. Mirrors the side-effect
    /// assertion pattern from `on_agent_exit_marks_supervisor_unreachable_when_pidfile_present`.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn on_agent_exit_removes_per_session_settings_file() {
        // Hold the home-test lock for the entire test: any concurrent
        // test using `HomeOverride` would otherwise flip `$HOME` and
        // misroute `dirs::home_dir()`.
        let _home_lock = lock_home().await;

        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, _rx) = test_registry(db.clone(), sockets_dir.clone());

        let agent_id = "settings-cleanup-agent";
        let session_id = "sess-cleanup-7f4a2c";
        let socket = sockets_dir.join(format!("{agent_id}.sock"));
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, session_id, cwd, status, mode, pid, supervisor_socket) \
                 VALUES (?1, ?2, '/tmp', 'running', 'pty', 1, ?3)",
                params![agent_id, session_id, socket.to_string_lossy().to_string()],
            )
            .unwrap();
        }

        // Override `$HOME` so `dirs::home_dir()` inside `on_agent_exit`'s
        // cleanup resolves to a tempdir we own.
        let home = TempDir::new().unwrap();
        let _home_guard = HomeOverride::set(home.path());

        // Plant the settings file at the path the deleter will compute.
        let settings_path = home
            .path()
            .join(".claude")
            .join("sessions")
            .join(format!("{session_id}.settings.json"));
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, b"{\"hooks\":{}}").unwrap();
        assert!(
            settings_path.exists(),
            "test setup: settings file should exist before on_agent_exit"
        );

        on_agent_exit(&registry, agent_id).await;

        assert!(
            !settings_path.exists(),
            "on_agent_exit should remove the per-session settings file at {}",
            settings_path.display()
        );
    }

    /// Full lifecycle for a Claude-driver agent: `start_pty_agent` writes
    /// the per-session settings file at the path the driver's argv will
    /// reference via `--settings`, and `on_agent_exit` cleans it up.
    ///
    /// The session-daemon spawn fails fast because `server_exe` points at
    /// a nonexistent path — but `start_pty_agent` inserts the agent row
    /// (with `session_id`) and writes the settings file *before* the
    /// supervisor spawn, which is exactly the side effect this test
    /// asserts on. Same insert-then-fail pattern the recovery / auto-mode
    /// integration tests rely on.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn start_pty_agent_writes_settings_and_on_agent_exit_cleans_up_for_claude() {
        let _home_lock = lock_home().await;

        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, _rx) = test_registry(db.clone(), sockets_dir.clone());

        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        // Override `$HOME` so the writer + deleter inside `start_pty_agent`
        // / `on_agent_exit` both resolve to our tempdir.
        let home = TempDir::new().unwrap();
        let _home_guard = HomeOverride::set(home.path());

        let agent_id = start_pty_agent(
            &registry,
            StartPtyOpts {
                prompt: "do the work".to_string(),
                cwd: &cwd,
                plan_name: None,
                task_id: None,
                effort: Effort::Medium,
                branch: None,
                is_continue: false,
                max_budget_usd: None,
                driver: Some("claude"),
                user_id: None,
                org_id: None,
                runner_id: None,
            },
        )
        .await;

        // Pull the session_id the spawn path generated so we can compute
        // the settings-file path the driver's argv will point at.
        let session_id: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT session_id FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get::<_, String>(0),
            )
            .expect("session_id is recorded on the agents row")
        };

        // Settings file exists at the path session_settings::path_for
        // computes (home/.claude/sessions/<session_id>.settings.json).
        let settings_path = home
            .path()
            .join(".claude")
            .join("sessions")
            .join(format!("{session_id}.settings.json"));
        assert!(
            settings_path.exists(),
            "start_pty_agent should write the per-session settings file at {}",
            settings_path.display()
        );

        // File contents: top-level `hooks.Stop` array with at least one
        // entry whose curl command embeds the session_id (so the receiver
        // can route the hook back to this agent).
        let raw = std::fs::read_to_string(&settings_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let stop_arr = json["hooks"]["Stop"]
            .as_array()
            .expect("hooks.Stop is an array");
        assert!(!stop_arr.is_empty(), "hooks.Stop is non-empty");
        let cmd = stop_arr[0]["hooks"][0]["command"]
            .as_str()
            .expect("Stop[0].hooks[0].command is a string");
        assert!(
            cmd.contains(&session_id),
            "stop hook command must embed session_id: {cmd}"
        );

        // Argv check: re-invoke the same driver.spawn_args the spawn path
        // built (settings_path = the file we just verified) and confirm
        // the resulting argv carries `--settings <path>`. This pins the
        // T1.4 wiring — start_pty_agent passes the written path through
        // to driver.spawn_args.
        let driver_arc = registry.drivers.get("claude").unwrap();
        let argv = driver_arc.spawn_args(&SpawnOpts {
            session_id: &session_id,
            cwd: &cwd,
            effort: Effort::Medium,
            max_budget_usd: None,
            mcp_config_path: None,
            settings_path: Some(&settings_path),
            skip_permissions: registry
                .skip_permissions
                .load(std::sync::atomic::Ordering::Relaxed),
        });
        let i = argv
            .iter()
            .position(|a| a == "--settings")
            .unwrap_or_else(|| panic!("--settings not in claude argv: {argv:?}"));
        assert_eq!(
            argv.get(i + 1).map(String::as_str),
            Some(settings_path.to_string_lossy().as_ref()),
            "--settings should be followed by the written settings path: {argv:?}"
        );

        // Lifecycle close-out: on_agent_exit removes the file. Status was
        // 'failed' (spawn never reached 'running'), so the cleanup block
        // is what we're really testing — it runs regardless of the
        // running→completed flip.
        on_agent_exit(&registry, &agent_id).await;
        assert!(
            !settings_path.exists(),
            "on_agent_exit should remove the per-session settings file at {}",
            settings_path.display()
        );
    }

    /// Aider negative path: drivers without a `stop_hook_config` write no
    /// settings file at spawn time, no `--settings` flag is appended, and
    /// `on_agent_exit` runs cleanly even though there's nothing to delete
    /// (the deleter treats NotFound as Ok).
    #[cfg(not(windows))]
    #[tokio::test]
    async fn start_pty_agent_writes_no_settings_file_for_aider() {
        let _home_lock = lock_home().await;

        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, _rx) = test_registry(db.clone(), sockets_dir.clone());

        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();

        let home = TempDir::new().unwrap();
        let _home_guard = HomeOverride::set(home.path());

        let agent_id = start_pty_agent(
            &registry,
            StartPtyOpts {
                prompt: "do the work".to_string(),
                cwd: &cwd,
                plan_name: None,
                task_id: None,
                effort: Effort::Medium,
                branch: None,
                is_continue: false,
                max_budget_usd: None,
                driver: Some("aider"),
                user_id: None,
                org_id: None,
                runner_id: None,
            },
        )
        .await;

        let session_id: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT session_id FROM agents WHERE id = ?1",
                params![agent_id],
                |row| row.get::<_, String>(0),
            )
            .expect("session_id is recorded on the agents row")
        };

        // No settings file at the path Claude would have written — the
        // sessions dir itself need not even exist (write_for_agent
        // returns early before mkdir).
        let settings_path = home
            .path()
            .join(".claude")
            .join("sessions")
            .join(format!("{session_id}.settings.json"));
        assert!(
            !settings_path.exists(),
            "Aider must not write a settings file (got {})",
            settings_path.display()
        );

        // Argv check: aider's spawn_args ignores settings_path entirely,
        // so the resulting argv carries no --settings flag (even when
        // we artificially pass Some).
        let driver_arc = registry.drivers.get("aider").unwrap();
        let stub_path = home.path().join("never-written.settings.json");
        let argv = driver_arc.spawn_args(&SpawnOpts {
            session_id: &session_id,
            cwd: &cwd,
            effort: Effort::Medium,
            max_budget_usd: None,
            mcp_config_path: None,
            settings_path: Some(&stub_path),
            skip_permissions: false,
        });
        assert!(
            !argv.iter().any(|a| a == "--settings"),
            "aider argv must not include --settings: {argv:?}"
        );

        // on_agent_exit on a row that never had a settings file written:
        // the deleter sees NotFound and returns Ok, the function does not
        // panic, and the path stays absent.
        on_agent_exit(&registry, &agent_id).await;
        assert!(
            !settings_path.exists(),
            "settings file must still not exist after on_agent_exit"
        );
    }

    /// Double-exit safety: if the row is already `killed` (kill_agent ran
    /// first), on_agent_exit must not rewrite the status or re-broadcast.
    #[tokio::test]
    async fn on_agent_exit_leaves_killed_row_alone() {
        let (db, dir) = fresh_db();
        let sockets_dir = dir.path().join("sockets");
        std::fs::create_dir_all(&sockets_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone(), sockets_dir.clone());

        let agent_id = "user-killed";
        let socket = sockets_dir.join(format!("{agent_id}.sock"));
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, cwd, status, mode, pid, supervisor_socket) \
                 VALUES (?1, '/tmp', 'killed', 'pty', 1, ?2)",
                params![agent_id, socket.to_string_lossy().to_string()],
            )
            .unwrap();
        }

        on_agent_exit(&registry, agent_id).await;

        let (status, reason) = status_and_reason(&db, agent_id);
        assert_eq!(status, "killed", "terminal row must not be rewritten");
        assert_eq!(reason, None);

        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e.contains("agent_stopped") && e.contains(agent_id)),
            "must not broadcast a duplicate stop: {events:?}"
        );
    }

    /// Regression for the 2026-05-06 trust-dialog incident: a runner-spawned
    /// claude on a freshly-created folder blocked at the "Trust this
    /// workspace?" prompt, the supervisor exited, and the dashboard saw
    /// nothing. The fix is `CLAUDE_CODE_SANDBOXED=1` in the env passed to
    /// claude — the binary's first check inside its trust-workspace gate
    /// short-circuits to allow when this var is set. Verify the var flows
    /// from `ClaudeDriver::extra_env()` all the way into the argv handed to
    /// `branchwork-server session …` (i.e. `--env CLAUDE_CODE_SANDBOXED=1`).
    #[test]
    fn pty_spawn_args_include_claude_sandboxed_env() {
        use crate::agents::driver::{AgentDriver, ClaudeDriver};
        use std::path::PathBuf;
        use supervisor::build_session_daemon_args;

        let driver = ClaudeDriver::new();
        let extra_env = driver.extra_env();
        assert!(
            extra_env
                .iter()
                .any(|(k, v)| *k == "CLAUDE_CODE_SANDBOXED" && *v == "1"),
            "claude driver must declare CLAUDE_CODE_SANDBOXED=1: {extra_env:?}",
        );

        let socket = PathBuf::from("/tmp/agent-abc.sock");
        let cwd = PathBuf::from("/tmp/project");
        let cmd: Vec<String> = vec!["claude".into(), "--session-id".into(), "abc".into()];
        let args = build_session_daemon_args(&socket, &cwd, 120, 40, &extra_env, &cmd);

        // The `--env` flag must appear before the `--` separator so clap on
        // the daemon side parses it as a SessionArgs flag, not part of `cmd`.
        let separator = args
            .iter()
            .position(|a| a == "--")
            .expect("argv must contain '--' separator");
        let env_idx = args
            .iter()
            .position(|a| a == "--env")
            .expect("argv must include --env flag for claude driver");
        assert!(env_idx < separator, "--env must come before --: {args:?}",);
        assert_eq!(
            args.get(env_idx + 1).map(String::as_str),
            Some("CLAUDE_CODE_SANDBOXED=1"),
            "--env flag must carry CLAUDE_CODE_SANDBOXED=1: {args:?}",
        );

        // And the cmd portion (post-`--`) is unchanged: drivers don't get to
        // rewrite their own argv via extra_env.
        let post_sep: Vec<String> = args[separator + 1..].to_vec();
        assert_eq!(post_sep, cmd, "post-'--' argv must equal cli cmd: {args:?}");
    }
}
