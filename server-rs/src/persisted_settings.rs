//! On-disk persistence for runtime-mutable server settings.
//!
//! These values can be edited from the dashboard's admin page and need to
//! survive a server restart, so we serialise them to a JSON sidecar next to
//! `branchwork.db`. Every field is `Option<T>`: a missing value means
//! "fall back to the CLI / env default at boot."

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Effort;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_permissions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// Days a soft-deleted plan's snapshot survives before the
    /// retention purger removes it. `None` means use the default
    /// (`plan_curate::DEFAULT_RETENTION_DAYS`). `Some(0)` means
    /// snapshot rows expire immediately — soft delete behaves like
    /// hard delete from the user's perspective but still leaves an
    /// audit trail. The admin-tab editor + clamping (0..=365) lands
    /// in plan-deletion 0.5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_archive_retention_days: Option<i64>,
    /// Default agent driver to use when not explicitly specified.
    /// Falls back to "claude" if not set. Can be overridden per-task,
    /// per-phase, per-plan, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_driver: Option<String>,
}

impl PersistedSettings {
    /// Read the settings file. Treats missing / empty / unparseable files as
    /// "no overrides" — we never want a corrupt file to block boot.
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            eprintln!(
                "[settings] {} is unparseable ({e}); ignoring and continuing with defaults",
                path.display()
            );
            Self::default()
        })
    }

    /// Get the default driver, falling back to "claude" if not set.
    pub fn default_driver(&self) -> &str {
        self.default_driver.as_deref().unwrap_or("claude")
    }

    /// Atomic write: serialise to `<path>.tmp`, then rename over the target.
    /// Avoids leaving a half-written file if the process dies mid-write.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}
