//! Per-repo `branchwork.toml` config: blocking-workflows allowlist, the
//! phase-verification command, and the dirty-tree-check ignore list.
//! Loaded from `~/<project>/branchwork.toml` during project resolution
//! (the `infer_project` path in [`crate::plan_parser`]).
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
//!
//! [auto_mode.dirty_tree]
//! # Paths whose dirty state should NOT trigger the
//! # "agent_left_uncommitted_work" pause. Globs supported.
//! ignore = ["*.log", ".bob/**", ".mcp.json"]
//! ```
//!
//! All sections are optional. Unknown top-level keys are silently
//! dropped; unknown keys *inside* known sections are also dropped, so
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

/// `[auto_mode]` table — auto-mode loop tuning.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AutoModeConfig {
    pub dirty_tree: DirtyTreeConfig,
}

/// `[auto_mode.dirty_tree]` table — controls the dirty-tree check that
/// gates auto-mode's pause-on-uncommitted-work behaviour
/// (see [`crate::agents::check_tree_clean_for_completion`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DirtyTreeConfig {
    /// Glob patterns for tracked file paths whose dirty state should
    /// NOT trigger the `agent_left_uncommitted_work` pause. Intended
    /// for known-operational files an agent might rewrite as a side
    /// effect (build logs, scratch notes, generated config) — the
    /// filter exists so noise doesn't pause plans, NOT to mask the
    /// agent forgetting to commit real code changes; agent code paths
    /// like `server-rs/`, `web/src/` still trip the pause.
    ///
    /// Matching is gitignore-style: a pattern without `/` matches the
    /// basename of a path at any depth; a pattern with `/` matches
    /// the full path. `*` matches any chars except `/`; `**` matches
    /// any chars including `/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

impl DirtyTreeConfig {
    /// Returns `true` when `path` (as emitted by
    /// `git status --porcelain`, i.e. forward-slash relative to repo
    /// root) matches any pattern in [`Self::ignore`]. Empty / absent
    /// ignore list returns `false` (no path is filtered).
    pub fn path_matches_ignore(&self, path: &str) -> bool {
        let Some(patterns) = self.ignore.as_deref() else {
            return false;
        };
        patterns.iter().any(|pat| matches_ignore_pattern(pat, path))
    }
}

/// Top-level `branchwork.toml` content.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoConfig {
    pub ci: CiConfig,
    pub phase: PhaseConfig,
    pub auto_mode: AutoModeConfig,
}

impl RepoConfig {
    /// `true` when the config has no fields set — equivalent to no file
    /// at all. Used by the doc example to keep the cached entry small.
    #[allow(dead_code)] // consumed by 0.3+
    pub fn is_empty(&self) -> bool {
        self.ci.blocking_workflows.is_none()
            && self.ci.blocking_workflows_skip.is_none()
            && self.phase.verification.is_none()
            && self.auto_mode.dirty_tree.ignore.is_none()
    }
}

/// Gitignore-style pattern matcher, slim subset.
///
/// - If `pattern` contains no `/`, it matches the basename of `path`
///   at any depth (gitignore behaviour for path-less patterns like
///   `*.log` or `.mcp.json`).
/// - Otherwise, it matches the full path.
/// - `*` matches any sequence of chars NOT including `/`.
/// - `**` matches any sequence of chars including `/`. Following the
///   common convention, a trailing `/` after `**` is consumed greedily
///   so e.g. `.bob/**` matches both `.bob/foo` and `.bob/foo/bar`.
///
/// Anchored at both ends (no implicit prefix or suffix). Backslash is
/// treated as a literal — paths from `git status --porcelain` use
/// forward slashes regardless of platform.
fn matches_ignore_pattern(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains('/') {
        return glob_match(pattern.as_bytes(), path.as_bytes());
    }
    // No-slash patterns apply to the basename at any depth.
    let basename = path.rsplit('/').next().unwrap_or(path);
    glob_match(pattern.as_bytes(), basename.as_bytes())
}

fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    // Recursive descent. Fast enough for the handful of patterns +
    // dozens of path lines we expect to see in practice.
    match (pat.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&b'*'), _) => {
            // Lookahead: ** matches across `/`, single * does not.
            if pat.get(1) == Some(&b'*') {
                // Consume `**` and an optional trailing `/`.
                let rest: &[u8] = if pat.get(2) == Some(&b'/') {
                    &pat[3..]
                } else {
                    &pat[2..]
                };
                // Try the rest at every offset, including the end.
                // `**` is allowed to match zero chars.
                let mut k = 0;
                loop {
                    if glob_match(rest, &s[k..]) {
                        return true;
                    }
                    if k >= s.len() {
                        return false;
                    }
                    k += 1;
                }
            } else {
                // Single `*` matches any sequence not including `/`.
                let rest = &pat[1..];
                let mut k = 0;
                loop {
                    if glob_match(rest, &s[k..]) {
                        return true;
                    }
                    if k >= s.len() {
                        return false;
                    }
                    if s[k] == b'/' {
                        return false;
                    }
                    k += 1;
                }
            }
        }
        (Some(_), None) => false,
        (Some(&p), Some(&c)) => {
            if p == c {
                glob_match(&pat[1..], &s[1..])
            } else {
                false
            }
        }
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

        let with_auto_mode = RepoConfig {
            auto_mode: AutoModeConfig {
                dirty_tree: DirtyTreeConfig {
                    ignore: Some(vec!["*.log".into()]),
                },
            },
            ..Default::default()
        };
        assert!(!with_auto_mode.is_empty());
    }

    #[test]
    fn parses_auto_mode_dirty_tree_ignore() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode.dirty_tree]
ignore = ["*.log", ".bob/**", ".mcp.json"]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(
            cfg.auto_mode.dirty_tree.ignore.as_deref(),
            Some(
                &[
                    "*.log".to_string(),
                    ".bob/**".to_string(),
                    ".mcp.json".to_string()
                ][..]
            )
        );
    }

    // ---- glob matcher --------------------------------------------

    #[test]
    fn glob_star_matches_basename_anywhere() {
        assert!(matches_ignore_pattern("*.log", "server.log"));
        assert!(matches_ignore_pattern("*.log", "nested/dir/server.log"));
    }

    #[test]
    fn glob_star_rejects_non_matching_suffix() {
        assert!(!matches_ignore_pattern("*.log", "server-rs/src/foo.rs"));
        assert!(!matches_ignore_pattern("*.log", "server.txt"));
    }

    #[test]
    fn glob_exact_filename_matches_at_any_depth() {
        assert!(matches_ignore_pattern(".mcp.json", ".mcp.json"));
        assert!(matches_ignore_pattern(".mcp.json", "subdir/.mcp.json"));
        assert!(!matches_ignore_pattern(".mcp.json", "mcp.json"));
        assert!(!matches_ignore_pattern(".mcp.json", "x.mcp.json"));
    }

    #[test]
    fn glob_dir_doublestar_matches_recursively() {
        assert!(matches_ignore_pattern(".bob/**", ".bob/foo"));
        assert!(matches_ignore_pattern(".bob/**", ".bob/foo/bar"));
        assert!(matches_ignore_pattern(".bob/**", ".bob/foo/bar/baz.txt"));
        // `**` doesn't have to match anything, but the trailing `/`
        // does — gitignore-wise this is the "match everything under
        // .bob/" idiom, not the bare directory itself.
        assert!(!matches_ignore_pattern(".bob/**", ".bob"));
        assert!(!matches_ignore_pattern(".bob/**", "other/.bob/foo"));
    }

    #[test]
    fn glob_full_path_pattern_anchors_at_root() {
        // Pattern containing `/` is matched against the FULL path, not
        // basename-at-any-depth.
        assert!(matches_ignore_pattern("docs/*.log", "docs/build.log"));
        assert!(!matches_ignore_pattern("docs/*.log", "src/docs/build.log"));
        // Single * does not cross `/`.
        assert!(!matches_ignore_pattern(
            "docs/*.log",
            "docs/build/output.log"
        ));
    }

    #[test]
    fn glob_empty_pattern_matches_nothing() {
        assert!(!matches_ignore_pattern("", "anything"));
        assert!(!matches_ignore_pattern("", ""));
    }

    #[test]
    fn dirty_tree_path_matches_ignore_walks_every_pattern() {
        let cfg = DirtyTreeConfig {
            ignore: Some(vec![
                "*.log".to_string(),
                ".bob/**".to_string(),
                ".mcp.json".to_string(),
            ]),
        };
        assert!(cfg.path_matches_ignore("server.log"));
        assert!(cfg.path_matches_ignore(".bob/scratch.txt"));
        assert!(cfg.path_matches_ignore(".mcp.json"));
        assert!(!cfg.path_matches_ignore("server-rs/src/foo.rs"));
        assert!(!cfg.path_matches_ignore("web/src/App.tsx"));
    }

    #[test]
    fn dirty_tree_path_matches_ignore_returns_false_when_no_patterns() {
        let cfg = DirtyTreeConfig::default();
        assert!(!cfg.path_matches_ignore("server.log"));
    }
}
