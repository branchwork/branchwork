pub mod pty_agent;
pub mod check_agent;
pub mod terminal_ws;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::Db;
use crate::ws::broadcast_event;

pub type AgentId = String;

#[derive(Clone)]
pub struct AgentRegistry {
    pub agents: Arc<Mutex<HashMap<AgentId, ManagedAgent>>>,
    pub db: Db,
    pub broadcast_tx: tokio::sync::broadcast::Sender<String>,
}

pub struct ManagedAgent {
    pub id: String,
    pub session_id: String,
    pub plan_name: Option<String>,
    pub task_id: Option<String>,
    pub mode: AgentMode,
    pub pty: Option<Box<dyn portable_pty::Child + Send>>,
    pub pty_writer: Option<Box<dyn std::io::Write + Send>>,
    pub pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub terminals: Vec<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum AgentMode {
    Pty,
    StreamJson,
}

impl AgentRegistry {
    pub fn new(db: Db, broadcast_tx: tokio::sync::broadcast::Sender<String>) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            db,
            broadcast_tx,
        }
    }

    /// Clean up agents whose PIDs are dead (from previous server runs)
    pub fn cleanup_stale(&self) {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT id, pid FROM agents WHERE status IN ('running', 'starting')")
            .unwrap();
        let stale: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten()
            .collect();

        for (id, pid) in stale {
            // Check if PID is alive
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                db.execute(
                    "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                    rusqlite::params![id],
                )
                .ok();
                println!("[orchestrAI] Cleaned stale agent {} (pid {})", &id[..8], pid);
            }
        }
    }

    pub async fn kill_agent(&self, agent_id: &str) -> bool {
        let mut agents = self.agents.lock().await;
        if let Some(mut agent) = agents.remove(agent_id) {
            if let Some(ref mut child) = agent.pty {
                child.kill().ok();
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
        false
    }
}
