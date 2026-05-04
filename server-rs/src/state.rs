use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

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
    /// Per-plan cancel signal for the auto-mode loop. The fix-on-red
    /// `wait_for_ci` poll selects against this so a user toggling
    /// `auto_mode` off mid-flight aborts the in-flight loop without
    /// waiting for the next 15 s tick. Removed (and freshly created on
    /// next get) when the user toggles off — a cancelled token cannot be
    /// reused, so the loop always reads a live one.
    pub cancellation_tokens: Arc<StdMutex<HashMap<String, CancellationToken>>>,
    /// Set of agent IDs that have already triggered an auto-finish
    /// graceful-exit on the Stop-hook path. Claude Code fires `Stop`
    /// once when we send `/exit` and again when the session actually
    /// ends; without dedupe the second hook would also pass the
    /// `status == 'running'` gate (the row only flips to `completed`
    /// inside `on_agent_exit`, which runs after the PTY actually
    /// closes) and we would re-fire `graceful_exit`, log a duplicate
    /// `agent.auto_finish` audit row, and broadcast a redundant
    /// `auto_finish_triggered` event. Insertion is the gate: the first
    /// Stop wins, every later Stop with the same `agent_id` is a
    /// no-op. Agent IDs are UUIDs allocated per spawn, so legitimate
    /// re-Stops after a respawn use a fresh key — the set grows once
    /// per agent for the process lifetime, which is bounded by total
    /// agent count.
    pub auto_finish_dedupe: Arc<StdMutex<HashSet<String>>>,
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
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    pub fn config_port(&self) -> u16 {
        self.port
    }

    /// Get (or lazily create) the cancellation token for `plan_name`. If
    /// the existing token has already been cancelled (i.e. a previous
    /// toggle-off ran and a fresh loop is now starting), it is replaced
    /// with a new one — cancelled tokens cannot be reused. Cloning a
    /// `CancellationToken` returns a handle to the same underlying
    /// signal, so the caller observes future cancellations.
    pub fn cancel_token_for(&self, plan_name: &str) -> CancellationToken {
        let mut map = self.cancellation_tokens.lock().unwrap();
        let entry = map.entry(plan_name.to_string()).or_default();
        if entry.is_cancelled() {
            *entry = CancellationToken::new();
        }
        entry.clone()
    }

    /// Cancel and forget the token for `plan_name`. The next
    /// [`Self::cancel_token_for`] call will create a fresh one. Idempotent
    /// — a missing key is a no-op (nothing in flight to abort).
    pub fn cancel_plan(&self, plan_name: &str) {
        let token = self.cancellation_tokens.lock().unwrap().remove(plan_name);
        if let Some(token) = token {
            token.cancel();
        }
    }
}
