//! Per-session Claude settings file writer.
//!
//! Each PTY agent gets its own `~/.claude/sessions/<session_id>.settings.json`
//! that bolts a Stop hook (and any other future keys) onto the agent's CLI
//! invocation via `claude --settings <path>`. The file is owned by the
//! supervisor and removed on agent exit; see ADR 0003.
//!
//! Driver-shape sourcing is delegated to [`AgentDriver::stop_hook_config`]
//! so non-Claude drivers (which return `None`) cleanly opt out — the writer
//! returns `Ok(None)` and the spawn path skips the `--settings` flag.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::driver::AgentDriver;

/// Write the per-session settings file for `session_id` based on `driver`'s
/// stop-hook contribution. Returns the path written, or `Ok(None)` when the
/// driver has no stop-hook surface (and therefore no reason to write a file).
#[allow(dead_code)] // wired into the spawn path by Phase 1.4
pub fn write_for_agent(
    session_id: &str,
    driver: &dyn AgentDriver,
    hook_url: &str,
) -> io::Result<Option<PathBuf>> {
    let home = dirs::home_dir().unwrap_or_default();
    write_for_agent_with_home(&home, session_id, driver, hook_url)
}

/// Test-friendly variant: takes an explicit `home` so tests can point at a
/// tempdir without mutating `$HOME`. Phase 1.4 will keep using
/// [`write_for_agent`]; this entry point exists only for unit tests.
#[allow(dead_code)] // exercised by the Phase 1.3 unit tests; kept pub(crate) for future test reuse
pub(crate) fn write_for_agent_with_home(
    home: &Path,
    session_id: &str,
    driver: &dyn AgentDriver,
    hook_url: &str,
) -> io::Result<Option<PathBuf>> {
    let Some(stop_hook) = driver.stop_hook_config(session_id, hook_url) else {
        return Ok(None);
    };

    let path = path_for(home, session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let body = build_settings_body(stop_hook);
    let pretty = serde_json::to_string_pretty(&body).map_err(io::Error::other)?;
    fs::write(&path, pretty)?;
    Ok(Some(path))
}

/// Best-effort cleanup of the per-session settings file written by
/// [`write_for_agent`]. Called from [`crate::agents::pty_agent::on_agent_exit`].
///
/// Returns `Ok(())` when the file did not exist — drivers that returned
/// `None` from `stop_hook_config` never wrote one, so a "missing file" is
/// the normal path on cleanup, not an error.
#[allow(dead_code)] // wired into on_agent_exit by Phase 1.5
pub fn delete_for_agent(session_id: &str) -> io::Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    delete_for_agent_with_home(&home, session_id)
}

/// Test-friendly variant of [`delete_for_agent`]: takes `home` explicitly
/// so unit tests can point at a tempdir without mutating `$HOME`.
#[allow(dead_code)] // exercised by the Phase 1.5 unit tests
pub(crate) fn delete_for_agent_with_home(home: &Path, session_id: &str) -> io::Result<()> {
    let path = path_for(home, session_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Resolve the per-session settings path under `home`. Shared between
/// the writer and the deleter so they cannot drift.
fn path_for(home: &Path, session_id: &str) -> PathBuf {
    home.join(".claude")
        .join("sessions")
        .join(format!("{session_id}.settings.json"))
}

/// Small builder so future top-level settings keys (permissions, env, …)
/// can be added without reshaping the driver trait. Today the only input
/// is the driver's stop-hook contribution; we splice its top-level keys
/// into the root settings object.
#[allow(dead_code)] // called by write_for_agent_with_home
fn build_settings_body(stop_hook: serde_json::Value) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    if let serde_json::Value::Object(map) = stop_hook {
        for (k, v) in map {
            root.insert(k, v);
        }
    }
    serde_json::Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::driver::{AiderDriver, ClaudeDriver};
    use tempfile::TempDir;

    #[test]
    fn writes_settings_file_for_claude() {
        let home = TempDir::new().unwrap();
        // Pre-create the parent dir to isolate this test from
        // `creates_parent_dir_when_missing` below.
        fs::create_dir_all(home.path().join(".claude/sessions")).unwrap();
        let driver = ClaudeDriver::new();
        let hook_url = "http://localhost:3100/hooks";
        let session_id = "sess-abc";

        let path = write_for_agent_with_home(home.path(), session_id, &driver, hook_url)
            .unwrap()
            .expect("Claude driver writes a settings file");
        assert_eq!(
            path,
            home.path().join(".claude/sessions/sess-abc.settings.json")
        );
        assert!(path.exists(), "settings file should exist on disk");

        let contents = fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        let stop_arr = json["hooks"]["Stop"].as_array().unwrap();
        assert!(!stop_arr.is_empty());
        let cmd = stop_arr[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains(session_id), "command should embed session_id");
        assert!(cmd.contains(hook_url), "command should embed hook_url");
    }

    #[test]
    fn returns_none_for_driver_without_stop_hook() {
        let home = TempDir::new().unwrap();
        let driver = AiderDriver::new();
        let result =
            write_for_agent_with_home(home.path(), "sess-xyz", &driver, "http://x/hooks").unwrap();
        assert!(
            result.is_none(),
            "AiderDriver returns None and writes nothing"
        );
        assert!(
            !home
                .path()
                .join(".claude/sessions/sess-xyz.settings.json")
                .exists(),
            "no file written when driver has no stop-hook config",
        );
    }

    #[test]
    fn creates_parent_dir_when_missing() {
        let home = TempDir::new().unwrap();
        // Deliberately do NOT mkdir ~/.claude/sessions/ — the writer must.
        let driver = ClaudeDriver::new();
        let path = write_for_agent_with_home(
            home.path(),
            "sess-mkdir",
            &driver,
            "http://localhost:3100/hooks",
        )
        .unwrap()
        .expect("write succeeds even without precreated parent dir");
        assert!(path.exists());
        assert!(home.path().join(".claude/sessions").is_dir());
    }

    /// Round-trip: a file written by `write_for_agent_with_home` must be
    /// removed by `delete_for_agent_with_home`. Catches drift between the
    /// writer's path resolution and the deleter's.
    #[test]
    fn delete_removes_file_written_by_writer() {
        let home = TempDir::new().unwrap();
        let driver = ClaudeDriver::new();
        let path = write_for_agent_with_home(
            home.path(),
            "sess-roundtrip",
            &driver,
            "http://localhost:3100/hooks",
        )
        .unwrap()
        .expect("Claude driver writes a settings file");
        assert!(path.exists());

        delete_for_agent_with_home(home.path(), "sess-roundtrip").unwrap();
        assert!(!path.exists(), "settings file should be removed");
    }

    /// NotFound is the normal path on cleanup (drivers without a stop-hook
    /// never wrote a file). The deleter must return `Ok(())`, not surface an
    /// error that the caller would log spuriously.
    #[test]
    fn delete_succeeds_when_file_missing() {
        let home = TempDir::new().unwrap();
        // No write — file does not exist.
        delete_for_agent_with_home(home.path(), "sess-never-existed")
            .expect("missing file should be a no-op, not an error");
    }
}
