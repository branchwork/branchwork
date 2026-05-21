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
//! [auto_mode]
//! # When auto-mode should merge a task's branch into master:
//! #   "task"  — every completed task (legacy, fastest feedback)
//! #   "phase" — at phase boundary (default for new plans)
//! #   "plan"  — only at end of plan (one shipped version per plan)
//! merge_cadence = "phase"
//!
//! # Optional pre-merge gate: shell checks that must pass before
//! # auto-mode merges a task branch into the canonical default.
//! # Empty (the default) = gate inactive — the feature is opt-in.
//! pre_merge_checks = [
//!   { name = "rust-fmt",    cmd = "cargo fmt --check", timeout_secs = 30, cwd = "server-rs" },
//!   { name = "rust-clippy", cmd = "cargo clippy --release --bin branchwork-server -- -D warnings", timeout_secs = 300, cwd = "server-rs" },
//! ]
//! # Wall-clock cap on the entire pre-merge gate. A runaway check can't
//! # wedge the merge pipeline past this many seconds. Default 600.
//! pre_merge_total_timeout_secs = 600
//!
//! # Optional escalation: when a `pre_merge_checks` entry fails AND its
//! # name has a `fix_recipe` mapping, auto-mode can optionally spawn a
//! # "fix agent" to apply the canonical fix. The agent commits straight
//! # to the task branch and Branchwork re-runs the gate. Default off —
//! # operator must opt in, because a fix-spawn agent silently writing to
//! # the branch is something you want to consciously turn on.
//! auto_fix_on_gate_failure = false
//! [auto_mode.fix_recipe]
//! "web-format" = "pnpm --filter @branchwork/web format"
//! "rust-fmt"   = "cargo fmt"
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

/// When auto-mode should merge a task's branch into the canonical default
/// (typically `master`). Drives the merge-cadence state machine added by
/// the `ci-cadence-build-vs-test-configurable` plan.
///
/// Wire form is lowercase (`"task"` / `"phase"` / `"plan"`). Any other
/// string fails to deserialize — at the file level that surfaces as a
/// `[branchwork] warning: failed to parse …` plus the load returning
/// `None`, which matches the existing malformed-TOML contract; future
/// HTTP surfaces (`POST /api/projects` and friends) should map the
/// `toml::de::Error` to a 400 with the deserializer's own field-pinned
/// message ("unknown variant `X`, expected one of `task`, `phase`,
/// `plan`").
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MergeCadence {
    /// Merge after every completed task (legacy auto-mode behaviour;
    /// fastest feedback, highest CI volume).
    Task,
    /// Merge once at each phase boundary. Default for new plans.
    #[default]
    Phase,
    /// Merge once at the end of the plan (one shipped version per plan).
    Plan,
}

/// Default value for [`AutoModeConfig::pre_merge_total_timeout_secs`].
/// 600 seconds (10 minutes) is the brief-mandated default ceiling on the
/// whole pre-merge gate.
fn default_pre_merge_total_timeout_secs() -> u32 {
    600
}

/// Skip emitting the field when it matches the default — keeps serialized
/// `branchwork.toml` round-trips small.
fn is_default_pre_merge_total_timeout_secs(v: &u32) -> bool {
    *v == default_pre_merge_total_timeout_secs()
}

/// One entry in [`AutoModeConfig::pre_merge_checks`] — a single shell
/// command that must exit 0 before auto-mode merges a task branch into
/// the canonical default.
///
/// Phase 1 (this struct) only parses and validates the schema; phase 2+
/// of the `pre-merge-gate` plan wires execution and the per-check /
/// whole-gate timeouts into the merge pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreMergeCheck {
    /// Human-readable identifier shown in the dashboard / audit log.
    /// Must be unique within the `pre_merge_checks` array; uniqueness is
    /// validated at deserialize time.
    pub name: String,
    /// Shell command to run. Passed verbatim to the runner; empty
    /// strings are rejected at deserialize time.
    pub cmd: String,
    /// Per-check timeout in seconds. Must be in `1..=600`.
    pub timeout_secs: u32,
    /// Optional working directory relative to the project root. When
    /// `None`, the command runs at the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `[auto_mode]` table — auto-mode loop tuning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AutoModeConfig {
    /// See [`MergeCadence`]. Defaults to [`MergeCadence::Phase`] for new
    /// plans; the day-1 migration in task 1.2 of the cadence plan back-
    /// fills existing plans to [`MergeCadence::Task`] so behaviour is
    /// unchanged immediately after the field lands.
    #[serde(default)]
    pub merge_cadence: MergeCadence,
    pub dirty_tree: DirtyTreeConfig,
    /// Optional pre-merge gate. Each entry is a shell check that must
    /// pass (exit 0) before a task's branch is merged into the canonical
    /// default in auto-mode. Empty (default) = gate inactive — the
    /// feature is strictly opt-in.
    ///
    /// Validated at deserialize time: `name` must be unique within the
    /// list, `cmd` must be non-empty, and `timeout_secs` must be in
    /// `1..=600`. Validation errors surface at `toml::from_str` time and
    /// name the offending check, so HTTP callers (a future `POST
    /// /api/projects` validator, say) can echo the deserializer's
    /// message verbatim to the user.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_pre_merge_checks"
    )]
    pub pre_merge_checks: Vec<PreMergeCheck>,
    /// Wall-clock cap on the entire pre-merge gate (default 600s, ten
    /// minutes). When the cumulative runtime of all checks exceeds this,
    /// the gate fails closed and pauses the plan — keeps a runaway check
    /// from wedging the merge pipeline indefinitely.
    #[serde(
        default = "default_pre_merge_total_timeout_secs",
        skip_serializing_if = "is_default_pre_merge_total_timeout_secs"
    )]
    pub pre_merge_total_timeout_secs: u32,
    /// Optional escalation: when a pre-merge check fails and its name
    /// matches a [`Self::fix_recipe`] entry, auto-mode can spawn a
    /// brief "fix agent" that runs the recipe, commits the result on
    /// the task branch, and lets the gate re-run.
    ///
    /// **Off by default** — a fix-spawn agent silently writing to the
    /// task branch is the kind of thing an operator should consciously
    /// turn on (the agent isn't sandboxed beyond the recipe prompt; if
    /// the recipe is wrong it can produce a misleading clean gate).
    /// When false (the default) or when no recipe matches, gate failure
    /// pauses the plan exactly as Phase 1.3 wired it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_fix_on_gate_failure: bool,
    /// Map of `pre_merge_checks.name` → shell command that fixes the
    /// failure. The command is run in the same worktree the gate failed
    /// in (via the gate-fix agent's prompt), and the agent then commits
    /// the result on the task branch.
    ///
    /// Keyed by check `name` (the unique identifier from the
    /// `pre_merge_checks` array) so a project can selectively opt in
    /// per-check — e.g. recipe a fast formatter but leave clippy fixes
    /// to the human. Recipes that don't correspond to a configured
    /// check name are silently ignored at gate time.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub fix_recipe: HashMap<String, String>,
}

/// `skip_serializing_if` companion for [`AutoModeConfig::auto_fix_on_gate_failure`]
/// — keeps `bool` round-trips quiet when the value matches the default.
fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for AutoModeConfig {
    fn default() -> Self {
        Self {
            merge_cadence: MergeCadence::default(),
            dirty_tree: DirtyTreeConfig::default(),
            pre_merge_checks: Vec::new(),
            pre_merge_total_timeout_secs: default_pre_merge_total_timeout_secs(),
            auto_fix_on_gate_failure: false,
            fix_recipe: HashMap::new(),
        }
    }
}

/// Validate the `pre_merge_checks` array during deserialization.
///
/// - `cmd` must be non-empty (rejecting accidental blank entries that
///   would silently pass the gate).
/// - `timeout_secs` must be in `1..=600` (0 is meaningless; >600
///   exceeds the whole-gate ceiling and is almost certainly a typo).
/// - Each `name` must be unique within the list (used as a stable ID
///   in dashboard payloads + audit rows; collisions would conflate
///   results).
///
/// Errors are returned via `D::Error::custom` and include the offending
/// `name` so a caller can route the message back to the user verbatim.
fn deserialize_pre_merge_checks<'de, D>(deserializer: D) -> Result<Vec<PreMergeCheck>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let checks = Vec::<PreMergeCheck>::deserialize(deserializer)?;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for c in &checks {
        if c.cmd.is_empty() {
            return Err(D::Error::custom(format!(
                "pre_merge_checks: cmd is empty for check {:?}",
                c.name
            )));
        }
        if c.timeout_secs == 0 || c.timeout_secs > 600 {
            return Err(D::Error::custom(format!(
                "pre_merge_checks: timeout_secs must be in 1..=600 for check {:?}, got {}",
                c.name, c.timeout_secs
            )));
        }
        if !seen.insert(c.name.as_str()) {
            return Err(D::Error::custom(format!(
                "pre_merge_checks: duplicate name {:?}",
                c.name
            )));
        }
    }
    Ok(checks)
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
            && self.auto_mode.merge_cadence == MergeCadence::default()
            && self.auto_mode.pre_merge_checks.is_empty()
            && self.auto_mode.pre_merge_total_timeout_secs == default_pre_merge_total_timeout_secs()
            && !self.auto_mode.auto_fix_on_gate_failure
            && self.auto_mode.fix_recipe.is_empty()
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
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_auto_mode.is_empty());

        // A non-default merge_cadence alone counts as non-empty.
        let with_cadence_only = RepoConfig {
            auto_mode: AutoModeConfig {
                merge_cadence: MergeCadence::Task,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_cadence_only.is_empty());

        // A populated pre_merge_checks alone counts as non-empty.
        let with_pre_merge_checks = RepoConfig {
            auto_mode: AutoModeConfig {
                pre_merge_checks: vec![PreMergeCheck {
                    name: "fmt".into(),
                    cmd: "cargo fmt --check".into(),
                    timeout_secs: 30,
                    cwd: None,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_pre_merge_checks.is_empty());

        // A non-default whole-gate timeout alone counts as non-empty.
        let with_custom_total_timeout = RepoConfig {
            auto_mode: AutoModeConfig {
                pre_merge_total_timeout_secs: 1200,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_custom_total_timeout.is_empty());

        // Toggling `auto_fix_on_gate_failure` alone counts as non-empty.
        let with_auto_fix_only = RepoConfig {
            auto_mode: AutoModeConfig {
                auto_fix_on_gate_failure: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_auto_fix_only.is_empty());

        // A non-empty `fix_recipe` map alone counts as non-empty.
        let mut recipe = HashMap::new();
        recipe.insert("web-format".into(), "pnpm format".into());
        let with_recipe_only = RepoConfig {
            auto_mode: AutoModeConfig {
                fix_recipe: recipe,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!with_recipe_only.is_empty());
    }

    // ---- merge_cadence ------------------------------------------

    #[test]
    fn merge_cadence_defaults_to_phase_when_absent() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        // Empty file → default config, cadence is phase.
        write(dir.path(), "branchwork.toml", "");
        let cfg = load_for_project_dir(dir.path()).expect("empty TOML is valid");
        assert_eq!(cfg.auto_mode.merge_cadence, MergeCadence::Phase);

        // `[auto_mode]` table present but no `merge_cadence` key → still phase.
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode.dirty_tree]
ignore = ["*.log"]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(cfg.auto_mode.merge_cadence, MergeCadence::Phase);
    }

    #[test]
    fn merge_cadence_parses_each_valid_variant() {
        for (literal, expected) in [
            ("task", MergeCadence::Task),
            ("phase", MergeCadence::Phase),
            ("plan", MergeCadence::Plan),
        ] {
            clear_cache_for_tests();
            let dir = TempDir::new().unwrap();
            write(
                dir.path(),
                "branchwork.toml",
                &format!("[auto_mode]\nmerge_cadence = \"{literal}\"\n"),
            );
            let cfg = load_for_project_dir(dir.path())
                .unwrap_or_else(|| panic!("merge_cadence = \"{literal}\" should parse"));
            assert_eq!(cfg.auto_mode.merge_cadence, expected);
        }
    }

    #[test]
    fn merge_cadence_round_trips_via_default() {
        // Default config is silent on the wire (toml omits the key when
        // the value equals the field default — but `MergeCadence` has no
        // `skip_serializing_if`, so it always emits). Round-trip is the
        // important property: serialize the default → parse → still phase.
        let default = RepoConfig::default();
        let text = toml::to_string(&default).unwrap();
        let reparsed: RepoConfig = toml::from_str(&text).unwrap();
        assert_eq!(reparsed.auto_mode.merge_cadence, MergeCadence::Phase);
        assert_eq!(reparsed, default);
    }

    #[test]
    fn merge_cadence_invalid_value_fails_to_parse() {
        // Direct parse path: the toml deserializer rejects unknown
        // variants with a field-pinned error message. This is the same
        // surface a hypothetical `POST /api/projects` validator would
        // consume to produce a 400 — the error string already names the
        // expected variants, so callers can echo it back to the user
        // verbatim.
        let err = toml::from_str::<RepoConfig>("[auto_mode]\nmerge_cadence = \"weekly\"\n")
            .expect_err("unknown variant must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant") && msg.contains("weekly"),
            "expected toml error to name the bad variant, got: {msg}"
        );
        assert!(
            msg.contains("task") && msg.contains("phase") && msg.contains("plan"),
            "expected toml error to enumerate accepted variants, got: {msg}"
        );

        // File-level path: malformed values are folded into the existing
        // "warn + return None" contract by `parse_file`, matching how
        // every other invalid-toml case already behaves.
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            "[auto_mode]\nmerge_cadence = \"weekly\"\n",
        );
        assert!(
            load_for_project_dir(dir.path()).is_none(),
            "invalid merge_cadence should be rejected like any other malformed TOML"
        );
    }

    #[test]
    fn merge_cadence_is_case_sensitive() {
        // `#[serde(rename_all = "lowercase")]` rejects "Phase", "PHASE",
        // etc. — guards against accidental typos sneaking through with
        // host-OS case-insensitive matching.
        for bad in ["Phase", "PHASE", "Task", "Plan "] {
            let err =
                toml::from_str::<RepoConfig>(&format!("[auto_mode]\nmerge_cadence = \"{bad}\"\n"))
                    .expect_err("non-lowercase variant must be rejected");
            assert!(
                err.to_string().contains("unknown variant"),
                "expected unknown-variant error for {bad:?}, got: {err}"
            );
        }
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

    // ---- pre_merge_checks ----------------------------------------

    #[test]
    fn pre_merge_checks_default_to_empty_vec_when_absent() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        // Empty file → empty checks list + default total timeout.
        write(dir.path(), "branchwork.toml", "");
        let cfg = load_for_project_dir(dir.path()).expect("empty TOML is valid");
        assert!(cfg.auto_mode.pre_merge_checks.is_empty());
        assert_eq!(cfg.auto_mode.pre_merge_total_timeout_secs, 600);

        // `[auto_mode]` table present but pre_merge_checks absent → same.
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            "[auto_mode]\nmerge_cadence = \"task\"\n",
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert!(cfg.auto_mode.pre_merge_checks.is_empty());
        assert_eq!(cfg.auto_mode.pre_merge_total_timeout_secs, 600);
    }

    #[test]
    fn pre_merge_checks_parses_full_block_with_optional_cwd() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode]
pre_merge_total_timeout_secs = 1200
pre_merge_checks = [
  { name = "web-format",   cmd = "pnpm --filter @branchwork/web format:check", timeout_secs = 60 },
  { name = "web-lint",     cmd = "pnpm --filter @branchwork/web lint",         timeout_secs = 60 },
  { name = "web-types",    cmd = "pnpm --filter @branchwork/web tsc --noEmit", timeout_secs = 120 },
  { name = "rust-fmt",     cmd = "cargo fmt --check",                          timeout_secs = 30, cwd = "server-rs" },
  { name = "rust-clippy",  cmd = "cargo clippy --release --bin branchwork-server -- -D warnings", timeout_secs = 300, cwd = "server-rs" },
]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(cfg.auto_mode.pre_merge_checks.len(), 5);
        assert_eq!(cfg.auto_mode.pre_merge_total_timeout_secs, 1200);

        // First entry: no cwd.
        let first = &cfg.auto_mode.pre_merge_checks[0];
        assert_eq!(first.name, "web-format");
        assert_eq!(first.cmd, "pnpm --filter @branchwork/web format:check");
        assert_eq!(first.timeout_secs, 60);
        assert!(first.cwd.is_none());

        // Fourth entry: cwd set.
        let rust_fmt = &cfg.auto_mode.pre_merge_checks[3];
        assert_eq!(rust_fmt.name, "rust-fmt");
        assert_eq!(rust_fmt.cmd, "cargo fmt --check");
        assert_eq!(rust_fmt.timeout_secs, 30);
        assert_eq!(rust_fmt.cwd.as_deref(), Some("server-rs"));

        // Boundary: timeout_secs = 300 is within 1..=600.
        let rust_clippy = &cfg.auto_mode.pre_merge_checks[4];
        assert_eq!(rust_clippy.timeout_secs, 300);
    }

    #[test]
    fn pre_merge_checks_zero_timeout_fails_to_parse_with_check_name() {
        // Direct toml::from_str path: the deserializer rejects with a
        // custom error containing the offending check name. This is the
        // surface a hypothetical `POST /api/projects` validator would
        // echo back as a 400 body.
        let toml_src = r#"
[auto_mode]
pre_merge_checks = [
  { name = "rust-fmt", cmd = "cargo fmt --check", timeout_secs = 0 },
]
"#;
        let err =
            toml::from_str::<RepoConfig>(toml_src).expect_err("timeout_secs = 0 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("rust-fmt"),
            "expected error to name the offending check, got: {msg}"
        );
        assert!(
            msg.contains("timeout_secs"),
            "expected error to mention timeout_secs, got: {msg}"
        );
        assert!(
            msg.contains("1..=600"),
            "expected error to state the accepted range, got: {msg}"
        );

        // File-level path: malformed values fold into the existing
        // "warn + return None" contract, matching every other invalid
        // TOML case.
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(dir.path(), "branchwork.toml", toml_src);
        assert!(
            load_for_project_dir(dir.path()).is_none(),
            "invalid timeout should be rejected like any other malformed TOML"
        );
    }

    #[test]
    fn pre_merge_checks_oversize_timeout_fails_to_parse_with_check_name() {
        let toml_src = r#"
[auto_mode]
pre_merge_checks = [
  { name = "rust-clippy", cmd = "cargo clippy", timeout_secs = 1000 },
]
"#;
        let err = toml::from_str::<RepoConfig>(toml_src)
            .expect_err("timeout_secs = 1000 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("rust-clippy"),
            "expected error to name the offending check, got: {msg}"
        );
        assert!(
            msg.contains("1000"),
            "expected error to echo the offending value, got: {msg}"
        );
        assert!(
            msg.contains("1..=600"),
            "expected error to state the accepted range, got: {msg}"
        );
    }

    #[test]
    fn pre_merge_checks_boundary_timeouts_are_accepted() {
        // 1 and 600 are inside the accepted range; both must parse cleanly.
        let toml_src = r#"
[auto_mode]
pre_merge_checks = [
  { name = "low",  cmd = "true",  timeout_secs = 1 },
  { name = "high", cmd = "true",  timeout_secs = 600 },
]
"#;
        let cfg: RepoConfig = toml::from_str(toml_src).expect("boundary timeouts must parse");
        assert_eq!(cfg.auto_mode.pre_merge_checks[0].timeout_secs, 1);
        assert_eq!(cfg.auto_mode.pre_merge_checks[1].timeout_secs, 600);
    }

    #[test]
    fn pre_merge_checks_empty_cmd_fails_to_parse_with_check_name() {
        let toml_src = r#"
[auto_mode]
pre_merge_checks = [
  { name = "noop", cmd = "", timeout_secs = 60 },
]
"#;
        let err = toml::from_str::<RepoConfig>(toml_src).expect_err("empty cmd must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("noop"),
            "expected error to name the offending check, got: {msg}"
        );
        assert!(
            msg.contains("cmd is empty"),
            "expected error to mention empty cmd, got: {msg}"
        );
    }

    #[test]
    fn pre_merge_checks_duplicate_name_fails_to_parse() {
        let toml_src = r#"
[auto_mode]
pre_merge_checks = [
  { name = "fmt", cmd = "cargo fmt --check",    timeout_secs = 30 },
  { name = "fmt", cmd = "pnpm format:check",    timeout_secs = 60 },
]
"#;
        let err =
            toml::from_str::<RepoConfig>(toml_src).expect_err("duplicate name must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate"),
            "expected error to mention duplication, got: {msg}"
        );
        assert!(
            msg.contains("\"fmt\""),
            "expected error to name the duplicated check, got: {msg}"
        );
    }

    #[test]
    fn pre_merge_checks_default_total_timeout_when_only_checks_set() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode]
pre_merge_checks = [
  { name = "fmt", cmd = "cargo fmt --check", timeout_secs = 30 },
]
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert_eq!(cfg.auto_mode.pre_merge_checks.len(), 1);
        // The whole-gate ceiling defaults to 600s even when checks are
        // configured but the operator hasn't overridden it.
        assert_eq!(cfg.auto_mode.pre_merge_total_timeout_secs, 600);
    }

    #[test]
    fn pre_merge_checks_round_trip_via_default() {
        // Default `AutoModeConfig` round-trips through TOML cleanly: the
        // skip_serializing_if attributes keep the wire output small, so
        // re-parsing the serialized form yields the same default value.
        let default = RepoConfig::default();
        let text = toml::to_string(&default).unwrap();
        let reparsed: RepoConfig = toml::from_str(&text).unwrap();
        assert_eq!(
            reparsed.auto_mode.pre_merge_checks,
            Vec::<PreMergeCheck>::new()
        );
        assert_eq!(reparsed.auto_mode.pre_merge_total_timeout_secs, 600);
        assert_eq!(reparsed, default);
    }

    // ---- auto_fix_on_gate_failure + fix_recipe (T2.2) ---------------

    #[test]
    fn auto_fix_on_gate_failure_defaults_to_false() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(dir.path(), "branchwork.toml", "");
        let cfg = load_for_project_dir(dir.path()).expect("empty TOML is valid");
        assert!(
            !cfg.auto_mode.auto_fix_on_gate_failure,
            "auto_fix_on_gate_failure must default to false — the brief is explicit \
             that this is an opt-in escalation"
        );
        assert!(cfg.auto_mode.fix_recipe.is_empty());
    }

    #[test]
    fn auto_fix_on_gate_failure_parses_when_set() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode]
auto_fix_on_gate_failure = true
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert!(cfg.auto_mode.auto_fix_on_gate_failure);
    }

    #[test]
    fn fix_recipe_parses_keyed_by_check_name() {
        clear_cache_for_tests();
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "branchwork.toml",
            r#"
[auto_mode]
auto_fix_on_gate_failure = true

[auto_mode.fix_recipe]
"web-format" = "pnpm --filter @branchwork/web format"
"rust-fmt"   = "cargo fmt"
"#,
        );
        let cfg = load_for_project_dir(dir.path()).expect("config should parse");
        assert!(cfg.auto_mode.auto_fix_on_gate_failure);
        assert_eq!(cfg.auto_mode.fix_recipe.len(), 2);
        assert_eq!(
            cfg.auto_mode
                .fix_recipe
                .get("web-format")
                .map(String::as_str),
            Some("pnpm --filter @branchwork/web format")
        );
        assert_eq!(
            cfg.auto_mode.fix_recipe.get("rust-fmt").map(String::as_str),
            Some("cargo fmt")
        );
    }

    #[test]
    fn fix_recipe_round_trips() {
        // Populated config round-trips through toml: serialize then re-
        // parse should match the original.
        let mut recipe = HashMap::new();
        recipe.insert("web-format".into(), "pnpm format".into());
        recipe.insert("rust-fmt".into(), "cargo fmt".into());
        let cfg = RepoConfig {
            auto_mode: AutoModeConfig {
                auto_fix_on_gate_failure: true,
                fix_recipe: recipe,
                ..Default::default()
            },
            ..Default::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        let reparsed: RepoConfig = toml::from_str(&text).unwrap();
        assert_eq!(reparsed, cfg);
    }

    #[test]
    fn auto_fix_default_is_omitted_from_serialized_toml() {
        // `skip_serializing_if = is_false` keeps the wire short when the
        // operator hasn't opted in. Equally, an empty `fix_recipe` map
        // doesn't show up. Both matter because the file is round-tripped
        // by a future `POST /api/projects` validator and we don't want it
        // to grow noise.
        let default = RepoConfig::default();
        let text = toml::to_string(&default).unwrap();
        assert!(
            !text.contains("auto_fix_on_gate_failure"),
            "default `auto_fix_on_gate_failure` must be omitted from \
             serialized TOML, got:\n{text}"
        );
        assert!(
            !text.contains("fix_recipe"),
            "empty `fix_recipe` must be omitted from serialized TOML, got:\n{text}"
        );
    }

    #[test]
    fn pre_merge_checks_round_trip_with_full_block() {
        // Populated config round-trips: serialize then re-parse should
        // re-validate without raising, and the deserialized form must
        // equal the original.
        let cfg = RepoConfig {
            auto_mode: AutoModeConfig {
                pre_merge_checks: vec![
                    PreMergeCheck {
                        name: "rust-fmt".into(),
                        cmd: "cargo fmt --check".into(),
                        timeout_secs: 30,
                        cwd: Some("server-rs".into()),
                    },
                    PreMergeCheck {
                        name: "rust-clippy".into(),
                        cmd: "cargo clippy --release --bin branchwork-server -- -D warnings".into(),
                        timeout_secs: 300,
                        cwd: Some("server-rs".into()),
                    },
                ],
                pre_merge_total_timeout_secs: 1200,
                ..Default::default()
            },
            ..Default::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        let reparsed: RepoConfig = toml::from_str(&text).unwrap();
        assert_eq!(reparsed, cfg);
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
