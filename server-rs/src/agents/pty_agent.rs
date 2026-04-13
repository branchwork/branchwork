use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use regex::Regex;
use rusqlite::params;

use crate::agents::{
    AgentRegistry, ManagedAgent, git_checkout_branch, git_current_branch, git_head_sha,
};
use crate::config::Effort;
use crate::ws::broadcast_event;

fn tmux_session_name(agent_id: &str) -> String {
    format!("oai-{}", &agent_id[..8.min(agent_id.len())])
}

pub struct StartPtyOpts<'a> {
    pub prompt: String,
    pub cwd: &'a Path,
    pub plan_name: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub effort: Effort,
    pub branch: Option<&'a str>,
    pub is_continue: bool,
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
    } = opts;
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let tmux_name = tmux_session_name(&id);

    // Capture base commit and source branch BEFORE switching
    let base_commit = git_head_sha(cwd);
    let source_branch = git_current_branch(cwd);

    // Checkout the task branch if specified
    if let Some(branch_name) = branch {
        git_checkout_branch(cwd, branch_name, is_continue);
    }

    // Insert into DB
    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt, base_commit, branch, source_branch)
             VALUES (?1, ?2, ?3, 'starting', 'pty', ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )
        .ok();
    }

    // Create tmux session running claude
    let claude_cmd = format!(
        "claude --session-id {} --add-dir {} --verbose --effort {}",
        session_id,
        shell_escape(cwd.to_str().unwrap_or(".")),
        effort,
    );

    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-x",
            "120",
            "-y",
            "40",
            &claude_cmd,
        ])
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .status();

    let spawn_failed = match &status {
        Err(_) => true,
        Ok(s) => !s.success(),
    };
    if spawn_failed {
        let err_msg = match status {
            Err(e) => format!("tmux spawn error: {e}"),
            Ok(s) => format!("tmux exited with {}", s.code().unwrap_or(-1)),
        };
        eprintln!(
            "[agent {}] Failed to create tmux session: {err_msg}",
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

    // Get the PID of the process inside tmux
    let pid = get_tmux_pid(&tmux_name).unwrap_or(0);

    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET pid = ?1, status = 'running' WHERE id = ?2",
            params![pid as i64, id],
        )
        .ok();
    }

    broadcast_event(
        &registry.broadcast_tx,
        "agent_started",
        serde_json::json!({"id": id, "sessionId": session_id, "planName": plan_name, "taskId": task_id, "pid": pid, "mode": "pty"}),
    );

    // Attach to the tmux session via a PTY (gives us clean terminal output)
    let agent = attach_to_tmux(registry, &id, &tmux_name).await;
    if agent.is_none() {
        eprintln!("[agent {}] Failed to attach to tmux session", &id[..8]);
    }

    // Send initial prompt once claude is ready
    let prompt_sent = Arc::new(AtomicBool::new(false));
    let tmux_for_ready = tmux_name.clone();
    let prompt_for_ready = prompt;
    let prompt_sent_for_ready = prompt_sent.clone();

    tokio::spawn(async move {
        // Poll tmux for ready signal
        for _ in 0..80 {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            if prompt_sent_for_ready.load(Ordering::Relaxed) {
                return;
            }

            if let Ok(output) = Command::new("tmux")
                .args(["capture-pane", "-t", &tmux_for_ready, "-p"])
                .output()
            {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.contains('❯') || text.contains('\u{276f}') {
                    if prompt_sent_for_ready.swap(true, Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    send_to_tmux(&tmux_for_ready, &prompt_for_ready);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    send_to_tmux_key(&tmux_for_ready, "Enter");
                    return;
                }
            }
        }
        // Fallback after 16s
        if !prompt_sent_for_ready.swap(true, Ordering::Relaxed) {
            send_to_tmux(&tmux_for_ready, &prompt_for_ready);
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            send_to_tmux_key(&tmux_for_ready, "Enter");
        }
    });

    id
}

/// Reattach to an existing tmux session (for agents that survived a server restart)
pub async fn reattach_agent(registry: &AgentRegistry, agent_id: &str, tmux_name: &str) {
    if attach_to_tmux(registry, agent_id, tmux_name)
        .await
        .is_some()
    {
        println!(
            "[orchestrAI] Reattached agent {} to tmux session {}",
            &agent_id[..8],
            tmux_name
        );
    } else {
        println!(
            "[orchestrAI] Failed to reattach agent {} to tmux {}",
            &agent_id[..8],
            tmux_name
        );
    }
}

/// Attach a PTY running `tmux attach -t <session>` and start reading its output.
/// Returns Some(()) on success.
async fn attach_to_tmux(registry: &AgentRegistry, agent_id: &str, tmux_name: &str) -> Option<()> {
    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .ok()?;

    let mut cmd = CommandBuilder::new("tmux");
    cmd.arg("attach-session");
    cmd.arg("-t");
    cmd.arg(tmux_name);
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd).ok()?;
    let mut reader = pair.master.try_clone_reader().ok()?;
    let pty_writer = pair.master.take_writer().ok()?;
    let master = pair.master;

    // Store in agent registry
    let agent = ManagedAgent {
        pty: Some(child),
        pty_writer: Some(pty_writer),
        pty_master: Some(master),
        tmux_session: Some(tmux_name.to_string()),
        terminals: Vec::new(),
    };
    registry
        .agents
        .lock()
        .await
        .insert(agent_id.to_string(), agent);

    // Spawn reader thread
    let db = registry.db.clone();
    let agents = registry.agents.clone();
    let tx = registry.broadcast_tx.clone();
    let id = agent_id.to_string();
    let tmux = tmux_name.to_string();

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = &buf[..n];
                    let text = String::from_utf8_lossy(data).to_string();

                    // Store in DB
                    {
                        let db = db.lock().unwrap();
                        db.execute(
                            "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, 'pty', ?2)",
                            params![id, text],
                        ).ok();
                    }

                    // Forward to connected WebSocket terminals
                    let guard = agents.blocking_lock();
                    if let Some(agent) = guard.get(&id) {
                        for terminal_tx in &agent.terminals {
                            terminal_tx.send(data.to_vec()).ok();
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // tmux attach exited — check if the tmux session is gone
        let session_dead = !Command::new("tmux")
            .args(["has-session", "-t", &tmux])
            .status()
            .is_ok_and(|s| s.success());

        if session_dead {
            // Parse cost from recent PTY output (Claude Code prints "Total cost: $X.XX")
            let cost_usd = parse_cost_from_pty_output(&db, &id);

            let db_guard = db.lock().unwrap();
            db_guard.execute(
                "UPDATE agents SET status = 'completed', finished_at = datetime('now'), cost_usd = ?2 WHERE id = ?1 AND status = 'running'",
                params![id, cost_usd],
            ).ok();
            drop(db_guard);
            agents.blocking_lock().remove(&id);
            broadcast_event(
                &tx,
                "agent_stopped",
                serde_json::json!({"id": id, "status": "completed", "exit_code": 0}),
            );
        }
    });

    Some(())
}

fn send_to_tmux(session: &str, text: &str) {
    Command::new("tmux")
        .args(["send-keys", "-t", session, "-l", text])
        .status()
        .ok();
    Command::new("tmux")
        .args(["send-keys", "-t", session, "Enter"])
        .status()
        .ok();
}

fn send_to_tmux_key(session: &str, key: &str) {
    Command::new("tmux")
        .args(["send-keys", "-t", session, key])
        .status()
        .ok();
}

fn get_tmux_pid(session: &str) -> Option<u32> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", session, "-F", "#{pane_pid}"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn shell_escape(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

/// Strip ANSI escape sequences from terminal output.
fn strip_ansi(s: &str) -> String {
    let re = Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\x1b\[.*?[@-~]").unwrap();
    re.replace_all(s, "").to_string()
}

/// Parse cost from the last few PTY output lines.
/// Claude Code prints lines like "Total cost:      $0.1234" at session end.
fn parse_cost_from_pty_output(db: &crate::db::Db, agent_id: &str) -> Option<f64> {
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
    let clean = strip_ansi(&combined);

    // Match patterns like "Total cost:  $0.1234" or "total cost: $12.34"
    let re = Regex::new(r"(?i)total\s+cost[:\s]*\$(\d+\.?\d*)").unwrap();
    re.captures(&clean)
        .and_then(|caps| caps.get(1)?.as_str().parse::<f64>().ok())
}
