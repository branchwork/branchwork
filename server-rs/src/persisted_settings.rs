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

    /// Atomic write: serialise to `<path>.tmp`, then rename over the target.
    /// Avoids leaving a half-written file if the process dies mid-write.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}
