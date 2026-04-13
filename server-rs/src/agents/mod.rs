pub mod check_agent;
pub mod pty_agent;
pub mod terminal_ws;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::Db;
use crate::ws::broadcast_event;

pub type AgentId = String;

/// Get the current HEAD commit SHA in the given directory.
/// Returns None if the directory is not a git repo or git is unavailable.
pub fn git_head_sha(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the current branch name in the given directory.
pub fn git_current_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name == "HEAD" {
            // Detached HEAD — not on a branch
            None
        } else {
            Some(name)
        }
    } else {
        None
    }
}

/// Create or checkout a git branch. Returns true if successful.
/// For "start" mode: creates the branch (or checks it out if it already exists).
/// For "continue" mode: checks out the existing branch.
pub fn git_checkout_branch(cwd: &std::path::Path, branch: &str, is_continue: bool) -> bool {
    if is_continue {
        // Try to checkout the existing branch
        let status = std::process::Command::new("git")
            .args(["checkout", branch])
            .current_dir(cwd)
            .output();
        match status {
            Ok(output) if output.status.success() => {
                println!("[orchestrAI] Checked out existing branch: {branch}");
                return true;
            }
            _ => {
                // Branch doesn't exist yet — fall through to create it
                println!("[orchestrAI] Branch {branch} not found for continue, creating it");
            }
        }
    }

    // Try to create the branch
    let status = std::process::Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(cwd)
        .output();
    match status {
        Ok(output) if output.status.success() => {
            println!("[orchestrAI] Created and checked out branch: {branch}");
            true
        }
        _ => {
            // Branch already exists — just check it out
            let fallback = std::process::Command::new("git")
                .args(["checkout", branch])
                .current_dir(cwd)
                .output();
            match fallback {
                Ok(output) if output.status.success() => {
                    println!("[orchestrAI] Checked out existing branch: {branch}");
                    true
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[orchestrAI] Failed to checkout branch {branch}: {stderr}");
                    false
                }
                Err(e) => {
                    eprintln!("[orchestrAI] Failed to run git checkout: {e}");
                    false
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AgentRegistry {
    pub agents: Arc<Mutex<HashMap<AgentId, ManagedAgent>>>,
    pub db: Db,
    pub broadcast_tx: tokio::sync::broadcast::Sender<String>,
}

pub struct ManagedAgent {
    /// Kept alive to prevent the child process from being dropped/killed.
    #[allow(dead_code)]
    pub pty: Option<Box<dyn portable_pty::Child + Send>>,
    pub pty_writer: Option<Box<dyn std::io::Write + Send>>,
    pub pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub tmux_session: Option<String>,
    pub terminals: Vec<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

impl AgentRegistry {
    pub fn new(db: Db, broadcast_tx: tokio::sync::broadcast::Sender<String>) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            db,
            broadcast_tx,
        }
    }

    /// Clean up dead agents and reattach alive ones (from previous server runs)
    pub async fn cleanup_and_reattach(&self) {
        let stale: Vec<(String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db
                .prepare("SELECT id, pid FROM agents WHERE status IN ('running', 'starting')")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .flatten()
                .collect()
        };

        for (id, pid) in stale {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                    rusqlite::params![id],
                ).ok();
                println!(
                    "[orchestrAI] Cleaned stale agent {} (pid {}) — process dead",
                    &id[..8],
                    pid
                );
                continue;
            }

            // Check if tmux session still exists
            let tmux_name = format!("oai-{}", &id[..8]);
            let tmux_exists = std::process::Command::new("tmux")
                .args(["has-session", "-t", &tmux_name])
                .status()
                .is_ok_and(|s| s.success());

            if tmux_exists {
                // Reattach!
                pty_agent::reattach_agent(self, &id, &tmux_name).await;
            } else {
                println!(
                    "[orchestrAI] Agent {} (pid {}) alive but no tmux session — detached",
                    &id[..8],
                    pid
                );
            }
        }
    }

    pub async fn kill_agent(&self, agent_id: &str) -> bool {
        // Try in-memory registry first (live agents)
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.remove(agent_id) {
            // Kill tmux session if it exists
            if let Some(ref tmux) = agent.tmux_session {
                std::process::Command::new("tmux")
                    .args(["kill-session", "-t", tmux])
                    .status()
                    .ok();
            }
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?",
                rusqlite::params![agent_id],
            )
            .ok();
            broadcast_event(
                &self.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": agent_id, "status": "killed"}),
            );
            return true;
        }
        drop(agents);

        // Fallback: try to find tmux session by naming convention
        let tmux_name = format!("oai-{}", &agent_id[..8.min(agent_id.len())]);
        let tmux_exists = std::process::Command::new("tmux")
            .args(["has-session", "-t", &tmux_name])
            .status()
            .is_ok_and(|s| s.success());

        if tmux_exists {
            std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .status()
                .ok();
        } else {
            // Last resort: kill by PID
            let db = self.db.lock().unwrap();
            if let Ok(pid) = db.query_row(
                "SELECT pid FROM agents WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
                |row| row.get::<_, i64>(0),
            ) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }

        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?",
            rusqlite::params![agent_id],
        )
        .ok();
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({"id": agent_id, "status": "killed"}),
        );
        true
    }
}
