use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::agents::AgentRegistry;
use crate::config::{Config, Effort};
use crate::db::Db;
use crate::saas::runner_ws::RunnerRegistry;

/// Shared application state, cheaply cloneable via Arc.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub plans_dir: PathBuf,
    pub port: u16,
    pub effort: Arc<std::sync::Mutex<Effort>>,
    pub broadcast_tx: broadcast::Sender<String>,
    pub registry: AgentRegistry,
    /// In-memory registry of currently connected remote runners.
    pub runners: RunnerRegistry,
    /// Disk path for runtime-mutable settings overrides (effort,
    /// skip_permissions, webhook_url). Lives next to `branchwork.db`.
    pub settings_path: PathBuf,
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
            port: config.port,
            effort: Arc::new(std::sync::Mutex::new(config.effort)),
            broadcast_tx,
            registry,
            runners: crate::saas::runner_ws::new_runner_registry(),
            settings_path: config.settings_path.clone(),
        }
    }

    pub fn config_port(&self) -> u16 {
        self.port
    }
}
