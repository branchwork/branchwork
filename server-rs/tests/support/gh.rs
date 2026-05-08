//! Optional scratch GitHub repo helper for end-to-end tests.
//!
//! Gated behind `BRANCHWORK_TEST_GH_REPOS=1`. When the gate is off,
//! `maybe_create` returns `None` and tests proceed without a remote.
//! This keeps the default `cargo test` invocation free of external
//! side effects — creating + deleting real GitHub repos under the
//! user's account is something a test run must opt into explicitly.
//!
//! When the gate is on, the helper:
//!   1. Generates a unique name `branchwork-test-<pid>-<ns>` so
//!      simultaneous test runs cannot collide.
//!   2. Calls `gh repo create <name> --private --source . --push`
//!      from the project dir so the GitHub repo is created, `origin`
//!      is set, and the current `master` branch is pushed.
//!   3. Resolves `nameWithOwner` + `url` via `gh repo view --json`
//!      so the caller can refer to the repo without parsing the
//!      `gh repo create` stdout (which is fragile).
//!
//! `Drop` runs `gh repo delete <name> --yes`. Failures are logged
//! to stderr with the repo name so an operator can clean up
//! manually if a test process is killed before Drop fires.

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Handle to a scratch GitHub repo. `Drop` deletes it.
pub struct GhRepo {
    /// `owner/repo` form, e.g. `cpoder/branchwork-test-12345-1715212345000000000`.
    pub full_name: String,
    /// Repo URL as reported by `gh repo view --json url`.
    pub url: String,
}

/// Returns `Some(GhRepo)` only when ALL of:
///   - `BRANCHWORK_TEST_GH_REPOS=1` is set, AND
///   - `gh` is on PATH and exits 0 from `gh --version`, AND
///   - `gh repo create` succeeds against the project dir.
///
/// Returns `None` otherwise — the gate, a missing CLI, an
/// unauthenticated user, or a network error all collapse to the
/// same "no remote available" signal so tests can branch on it.
pub fn maybe_create(project: &Path) -> Option<GhRepo> {
    if std::env::var("BRANCHWORK_TEST_GH_REPOS").as_deref() != Ok("1") {
        return None;
    }

    if Command::new("gh")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_none()
    {
        eprintln!("[gh] gh CLI not on PATH; skipping repo create");
        return None;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("branchwork-test-{}-{}", std::process::id(), nanos);

    let create = Command::new("gh")
        .args([
            "repo",
            "create",
            &name,
            "--private",
            "--source",
            ".",
            "--push",
        ])
        .current_dir(project)
        .output();
    let create = match create {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "[gh] repo create {} failed (exit {}): {}",
                name,
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr)
            );
            return None;
        }
        Err(e) => {
            eprintln!("[gh] repo create {} spawn err: {e}", name);
            return None;
        }
    };
    drop(create);

    // gh repo create prints the URL on stdout, but its format has
    // changed across versions. Resolve canonical fields via JSON view.
    let view = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner,url"])
        .current_dir(project)
        .output();
    let view = match view {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "[gh] repo view failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            // Orphan cleanup: best-effort delete using just the bare name.
            let _ = Command::new("gh")
                .args(["repo", "delete", &name, "--yes"])
                .output();
            return None;
        }
        Err(e) => {
            eprintln!("[gh] repo view spawn err: {e}");
            let _ = Command::new("gh")
                .args(["repo", "delete", &name, "--yes"])
                .output();
            return None;
        }
    };
    let body: serde_json::Value = match serde_json::from_slice(&view.stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gh] repo view stdout was not JSON: {e}");
            let _ = Command::new("gh")
                .args(["repo", "delete", &name, "--yes"])
                .output();
            return None;
        }
    };
    let full_name = body
        .get("nameWithOwner")
        .and_then(|v| v.as_str())?
        .to_string();
    let url = body
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Some(GhRepo { full_name, url })
}

impl Drop for GhRepo {
    fn drop(&mut self) {
        let out = Command::new("gh")
            .args(["repo", "delete", &self.full_name, "--yes"])
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!(
                    "[gh] repo delete {} failed: {}",
                    self.full_name,
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Err(e) => {
                eprintln!("[gh] repo delete {} spawn err: {e}", self.full_name);
            }
        }
    }
}
