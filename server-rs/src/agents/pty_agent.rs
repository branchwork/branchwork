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

use interprocess::local_socket::ConnectOptions;
use interprocess::local_socket::tokio::prelude::*;
use rusqlite::params;

use crate::agents::driver::{AgentDriver, SpawnOpts};
use crate::agents::session_protocol::{self, Message as SessionMessage};
use crate::agents::supervisor;
use crate::agents::{
    AgentRegistry, ManagedAgent, git_checkout_branch, git_current_branch, git_head_sha,
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
    } = opts;
    let (driver_name, driver) = registry.drivers.get_or_default(driver_name);
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Capture base commit and source branch BEFORE switching
    let base_commit = git_head_sha(cwd);
    let source_branch = git_current_branch(cwd);

    // Checkout the task branch if specified
    if let Some(branch_name) = branch {
        git_checkout_branch(cwd, branch_name, is_continue);
    }

    // Insert into DB (socket path filled in once the daemon reports its PID)
    let socket_path = registry.socket_for(&id);
    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt, base_commit, branch, source_branch, supervisor_socket, driver)
             VALUES (?1, ?2, ?3, 'starting', 'pty', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            ],
        )
        .ok();
    }

    // If the driver supports MCP config injection, materialise the config
    // file alongside the agent's socket so `--mcp-config` can point at it.
    // Failure to write is non-fatal: the agent still runs, just without
    // orchestrAI tool access. Log and continue.
    let mcp_config_path = driver.mcp_config_json(registry.port).and_then(|json| {
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

    // Build the CLI argv via the driver. No shell involved — `portable-pty`
    // spawns it directly in the daemon, so we don't need to escape spaces.
    let cli_cmd = driver.spawn_args(&SpawnOpts {
        session_id: &session_id,
        cwd,
        effort,
        max_budget_usd,
        mcp_config_path: mcp_config_path.as_deref(),
    });
    let formatted_prompt = driver.format_prompt(&prompt);

    let daemon_pid = match supervisor::spawn_session_daemon(
        &registry.server_exe,
        &socket_path,
        cwd,
        120,
        40,
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

    spawn_writer_task(write_half, command_rx);
    spawn_reader_task(
        registry.clone(),
        agent_id.to_string(),
        read_half,
        output_tx.clone(),
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
) {
    tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            match session_protocol::read_frame(&mut read_half).await {
                Ok(SessionMessage::Output(bytes)) => {
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
                Ok(_) => { /* Pong / non-Output messages: ignore */ }
                Err(_) => break,
            }
        }

        // Supervisor closed the connection: treat as agent completion.
        on_agent_exit(&registry, &agent_id).await;
    });
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
    let (_, driver) = registry.drivers.get_or_default(driver_name.as_deref());
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

    // Only flip `running → completed`. If the row is already `killed` (from
    // kill_agent) or `failed` we leave the terminal status alone — but we
    // still stamp cost_usd so the UI reports spend accurately regardless of
    // how the agent ended.
    let marked_completed = {
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET cost_usd = ?2 WHERE id = ?1 AND cost_usd IS NULL",
            params![agent_id, cost_usd],
        )
        .ok();
        let n = db
            .execute(
                "UPDATE agents SET status = 'completed', finished_at = datetime('now') WHERE id = ?1 AND status = 'running'",
                params![agent_id],
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

    // Clean up the per-agent socket / pidfile siblings. Log stays so the
    // full transcript is still recoverable post-mortem.
    let socket_path = registry.socket_for(agent_id);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(supervisor::pidfile_path(&socket_path));

    // Only broadcast completion when we actually flipped the row. Otherwise
    // kill_agent already emitted an `agent_stopped:killed` event and a
    // duplicate here would confuse the dashboard.
    if marked_completed {
        broadcast_event(
            &registry.broadcast_tx,
            "agent_stopped",
            serde_json::json!({"id": agent_id, "status": "completed", "exit_code": 0}),
        );
    }

    // Webhook only when we actually completed; kill_agent owns user-visible
    // messaging for the kill path.
    if marked_completed && registry.webhook_url.is_some() {
        let (plan, task, branch) = meta.unwrap_or((None, None, None));
        let msg = crate::notifications::agent_completion_message(
            plan.as_deref(),
            task.as_deref(),
            agent_id,
            "completed",
            branch.as_deref(),
            cost_usd,
        );
        crate::notifications::notify(registry.webhook_url.clone(), msg);
    }

    if let Some(plan) = over_budget_plan {
        kill_plan_agents(registry, &plan).await;
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
            "[orchestrAI] Killed agent {} -- plan '{}' exceeded budget",
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
