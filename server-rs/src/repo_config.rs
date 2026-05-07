//! Per-repo `branchwork.toml` config: blocking-workflows allowlist + the
//! phase-verification command. Loaded from `~/<project>/branchwork.toml`
//! during project resolution (the `infer_project` path in
//! [`crate::plan_parser`]).
//!
//! Phase 0 of the `branchwork-phase-verify-and-ci-filter` plan: this module
//! parses + caches the file. The resolution-order helper that combines
//! repo / plan / phase precedence lives in task 0.3, and consumption (CI
//! aggregate filter, phase-end Check agent) lands in phases 1 and 2.
//!
//! # File format
//!
//! ```toml
//! [ci]
//! # Allowlist of workflow names that block merges/auto-mode.
//! blocking_workflows = ["CI"]
//! # OR explicitly opt out:
//! # blocking_workflows_skip = ["Docker", "Deploy", "Publish"]
//!
//! [phase]
//! # Shell command run by the phase-end Check agent.
//! verification = "bash scripts/verify.sh"
//! ```
//!
//! Both sections are optional. Unknown top-level keys are silently
//! dropped; unknown keys *inside* `[ci]` / `[phase]` are also dropped, so
//! typos won't crash the parser.
//!
//! # Failure modes
//!
//! - File absent → returns `None`, no warning. This is the common case.
//! - File present but malformed → logs `[branchwork] warning: failed to
//!   parse …` to stderr, returns `None`. Never panics.
//! - Filesystem error other than `NotFound` → logs a warning, returns
//!   `None`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// `[ci]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CiConfig {
    /// Names of workflows that block merges / auto-mode advancement.
    /// When `Some`, any workflow not in the list is informative-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_workflows: Option<Vec<String>>,
    /// Names of workflows to explicitly mark non-blocking. Conceptually
    /// the inverse of [`Self::blocking_workflows`]; see the consumer in
    /// task 1.1 for the precedence rule when both are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_workflows_skip: Option<Vec<String>>,
}

/// `[phase]` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PhaseConfig {
    /// Shell command run by the phase-end Check agent (task 2.x).
    /// The string is passed verbatim to the agent prompt; commonly
    /// something like `"bash scripts/verify.sh"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// Top-level `branchwork.toml` content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoConfig {
    pub ci: CiConfig,
    pub phase: PhaseConfig,
}

impl RepoConfig {
    /// `true` when the config has no fields set — equivalent to no file
    /// at all. Used by the doc example to keep the cached entry small.
    #[allow(dead_code)] // consumed by 0.3+
    pub fn is_empty(&self) -> bool {
        self.ci.blocking_workflows.is_none()
            && self.ci.blocking_workflows_skip.is_none()
            && self.phase.verification.is_none()
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    /// `mtime` from the last successful stat. Used as the cache-key
    /// invalidator: if the file has changed on disk, we re-read.
    mtime: Option<SystemTime>,
    /// `None` when the file was absent or failed to parse — we cache
    /// the negative result too so we don't re-warn on every call.
    config: Option<RepoConfig>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Load `<project_dir>/branchwork.toml` if present.
///
/// Cached per canonical file path with mtime invalidation: a stable
/// file is parsed once for the lifetime of the process, an edited file
/// is picked up on the next call.
///
/// Returns `None` when the file is absent or malformed; parse errors
/// are logged to stderr as warnings and never propagate.
pub fn load_for_project_dir(project_dir: &Path) -> Option<RepoConfig> {
    let path = project_dir.join("branchwork.toml");

    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("[branchwork] warning: stat {} failed: {e}", path.display());
            return None;
        }
    };
    let mtime = metadata.modified().ok();
    let key = path.canonicalize().unwrap_or_else(|_| path.clone());

    {
        let cache = cache().lock().unwrap();
        if let Some(entry) = cache.get(&key)
            && entry.mtime == mtime
        {
            return entry.config.clone();
        }
    }

    let parsed = parse_file(&path);

    {
        let mut cache = cache().lock().unwrap();
        cache.insert(
            key,
            CacheEntry {
                mtime,
                config: parsed.clone(),
            },
        );
    }
    parsed
}

/// Convenience wrapper: resolves `~/<project>` and delegates to
/// [`load_for_project_dir`]. Callers that already have an absolute
/// project root should prefer the path-based form.
#[allow(dead_code)] // consumed by 0.3+
pub fn load_for_project(project: &str) -> Option<RepoConfig> {
    let home = dirs::home_dir()?;
    load_for_project_dir(&home.join(project))
}

fn parse_file(path: &Path) -> Option<RepoConfig> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[branchwork] warning: read {} failed: {e}", path.display());
            return None;
        }
    };
    match toml::from_str::<RepoConfig>(&text) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!(
                "[branchwork] warning: failed to parse {}: {e}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
/// Drop the in-process cache. Test-only helper so independent test cases
/// don't see each other's cached entries.
pub fn clear_cache_for_tests() {
    cache().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn missing_file_returns_none() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        assert!(load_for_project_dir(dir.path()).is_none());
    }

    #[test]
    fn present_file_with_full_schema_round_trips() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[ci]
blocking_workflows = ["CI", "lint"]
blocking_workflows_skip = ["Docker", "Deploy"]

[phase]
verification = "bash scripts/verify.sh"
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(
            cfg.ci.blocking_workflows.as_deref(),
            Some(&["CI".to_string(), "lint".to_string()][..])
        );
        assert_eq!(
            cfg.ci.blocking_workflows_skip.as_deref(),
            Some(&["Docker".to_string(), "Deploy".to_string()][..])
        );
        assert_eq!(
            cfg.phase.verification.as_deref(),
            Some("bash scripts/verify.sh")
        );
    }

    #[test]
    fn partial_schema_with_only_ci_section() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[ci]
blocking_workflows = ["CI"]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(
            cfg.ci.blocking_workflows.as_deref(),
            Some(&["CI".to_string()][..])
        );
        assert!(cfg.ci.blocking_workflows_skip.is_none());
        assert!(cfg.phase.verification.is_none());
    }

    #[test]
    fn partial_schema_with_only_phase_section() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[phase]
verification = "make verify"
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(cfg.phase.verification.as_deref(), Some("make verify"));
        assert!(cfg.ci.blocking_workflows.is_none());
        assert!(cfg.ci.blocking_workflows_skip.is_none());
    }

    #[test]
    fn empty_file_parses_to_default() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(dir.path(), "branchwork.toml", "");
        let cfg = load_for_project_dir(dir.path()).expect("empty TOML is valid");
        assert!(cfg.is_empty());
    }

    #[test]
    fn unknown_top_level_keys_are_dropped() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[gibberish]
foo = "bar"

[ci]
blocking_workflows = ["CI"]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("unknown keys should not crash");
        assert_eq!(
            cfg.ci.blocking_workflows.as_deref(),
            Some(&["CI".to_string()][..])
        );
    }

    #[test]
    fn malformed_toml_logs_warning_and_returns_none() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        // Unbalanced bracket → toml parser error.
        write(
            dir.path(),
            "branchwork.toml",
            "[ci\nblocking_workflows = []",
        );
        assert!(load_for_project_dir(dir.path()).is_none());
        // Calling again with the same broken file does NOT re-parse:
        // the negative result is cached. We can't directly observe the
        // warning count without capturing stderr, but the cache hit is
        // implicit — second call is a HashMap lookup.
        assert!(load_for_project_dir(dir.path()).is_none());
    }

    #[test]
    fn cached_result_invalidates_on_mtime_change() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            "[ci]\nblocking_workflows = [\"a\"]\n",
        );
        let first = load_for_project_dir(dir.path()).unwrap();
        assert_eq!(
            first.ci.blocking_workflows.as_deref(),
            Some(&["a".to_string()][..])
        );

        // Bump mtime forward (filetime resolution on some FS is 1s, so
        // sleep before rewrite).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(
            dir.path(),
            "branchwork.toml",
            "[ci]\nblocking_workflows = [\"b\"]\n",
        );

        let second = load_for_project_dir(dir.path()).unwrap();
        assert_eq!(
            second.ci.blocking_workflows.as_deref(),
            Some(&["b".to_string()][..])
        );
    }

    #[test]
    fn load_for_project_resolves_relative_to_home() {
        clear_cache_for_tests();
        // We can't safely mutate $HOME mid-test (other parallel tests
        // resolve dirs::home_dir()). Instead, verify that the wrapper
        // delegates by passing a project name that does not exist
        // anywhere — the result MUST be `None`, never a panic.
        assert!(load_for_project("definitely-not-a-real-project-xyz").is_none());
    }

    #[test]
    fn is_empty_predicate() {
        let blank = RepoConfig::default();
        assert!(blank.is_empty());

        let with_ci = RepoConfig {
            ci: CiConfig {
                blocking_workflows: Some(vec!["CI".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_ci.is_empty());

        let with_phase = RepoConfig {
            phase: PhaseConfig {
                verification: Some("make".into()),
            },
            ..Default::default()
        };
        assert!(!with_phase.is_empty());
    }
}
