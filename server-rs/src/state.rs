use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::agents::AgentRegistry;
use crate::config::{Config, Effort};
use crate::db::Db;

/// Shared application state, cheaply cloneable via Arc.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub plans_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub effort: Arc<std::sync::Mutex<Effort>>,
    pub broadcast_tx: broadcast::Sender<String>,
    pub registry: AgentRegistry,
}

impl AppState {
    pub fn new(
        config: &Config,
        db: Db,
        broadcast_tx: broadcast::Sender<String>,
        registry: AgentRegistry,
    ) -> Self {
        Self {
            db,
            plans_dir: config.plans_dir.clone(),
            claude_dir: config.claude_dir.clone(),
            effort: Arc::new(std::sync::Mutex::new(config.effort)),
            broadcast_tx,
            registry,
        }
    }
}
