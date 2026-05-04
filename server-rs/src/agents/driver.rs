//! Portable agent driver abstraction.
//!
//! Every supported AI CLI (claude, codex, gemini, ...) is represented by an
//! [`AgentDriver`] implementation. PTY spawning, readiness detection, cost
//! parsing, and verdict extraction all route through the trait so
//! `pty_agent.rs` stays AI-agnostic.
//!
//! The trait is deliberately object-safe — the registry (and per-agent
//! spawn paths) stores and passes around `&dyn AgentDriver` /
//! `Box<dyn AgentDriver>`.

use std::path::Path;
use std::sync::Arc;

use regex::Regex;

use crate::config::Effort;

/// Options passed to [`AgentDriver::spawn_args`] when building the argv for
/// a brand-new PTY session. Kept flat on purpose: every field is something
/// the caller already has to hand by the time it's wiring up the spawn.
pub struct SpawnOpts<'a> {
    /// Pre-generated session id; drivers that support resume pass this
    /// along as `--session-id` (or equivalent).
    pub session_id: &'a str,
    /// Working directory the CLI should treat as the project root.
    pub cwd: &'a Path,
    /// Effort / reasoning level selected by the operator.
    pub effort: Effort,
    /// Plan-level budget cap, if one is set. Drivers that can enforce a
    /// hard ceiling pass it to the CLI; others ignore it and rely on the
    /// server-side budget enforcement in `pty_agent`.
    pub max_budget_usd: Option<f64>,
    /// Path to a JSON file that registers the Branchwork MCP server. When
    /// set, drivers that [declare MCP injection][AgentDriver::mcp_config_json]
    /// add the appropriate flag (e.g. `--mcp-config <path>`) to their argv.
    /// The caller is responsible for having written the file before spawn.
    pub mcp_config_path: Option<&'a Path>,
    /// When true, drivers that support an unattended/skip-permissions mode
    /// pass the corresponding flag (e.g. Claude's `--dangerously-skip-permissions`).
    /// Drivers without such a concept ignore this.
    pub skip_permissions: bool,
}

/// Verdict produced by a check-agent run. Mirrors the
/// `{"status": "...", "reason": "..."}` JSON blob the CLI is asked to emit
/// at the end of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // consumed by check_agent in a later Phase 1 task
pub struct Verdict {
    pub status: String,
    pub reason: String,
}

/// Declared capabilities of a driver. Used by the UI to hide features the
/// backend can't actually populate (e.g. no cost column for CLIs that don't
/// report spend) and by the server to skip pointless work (e.g. don't pass
/// `--session-id` to a driver that has no concept of resume).
///
/// Defaults mirror the Claude CLI — the richest backend — so a new driver
/// that forgets to override `AgentDriver::capabilities` is assumed to be
/// fully featured. Each driver impl overrides the bools it can't deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DriverCapabilities {
    /// CLI reports a cumulative cost figure that [`AgentDriver::parse_cost`]
    /// can extract. False means the dashboard hides the cost column for
    /// this backend.
    pub supports_cost: bool,
    /// CLI can be asked to emit the `{"status": ..., "reason": ...}` JSON
    /// verdict blob that check-agents consume.
    pub supports_verdict: bool,
    /// CLI accepts a caller-supplied session id (for resume / continue).
    /// When false the server's session id is kept for bookkeeping only.
    pub supports_session_id: bool,
    /// CLI only runs as an interactive REPL — no headless / stream-json
    /// mode. Check-agents for these drivers fall back to the PTY path.
    pub interactive_only: bool,
}

impl Default for DriverCapabilities {
    fn default() -> Self {
        Self {
            supports_cost: true,
            supports_verdict: true,
            supports_session_id: true,
            interactive_only: false,
        }
    }
}

/// Auth state for an AI CLI, reported to the dashboard so it can disable
/// Start/Continue buttons for tools that can't authenticate and show the
/// user how to fix it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)] // surfaced by a later Phase 1 auth-detection task
pub enum AuthStatus {
    /// Binary not found on PATH — install the CLI first.
    NotInstalled,
    /// Binary is installed but the driver couldn't find valid credentials.
    /// `help` is a short markdown snippet shown in the UI (e.g. "Run
    /// `claude` and complete sign-in, or set `ANTHROPIC_API_KEY`").
    Unauthenticated { help: String },
    /// OAuth / subscription session (Claude Max, Claude Pro, …). `account`
    /// is a display hint (email or plan name) — `None` when we can't read it.
    Oauth { account: Option<String> },
    /// API key authentication (env var, `~/.anthropic/credentials`, …).
    ApiKey,
    /// Cloud-provider creds (Bedrock, Vertex) — grouped because they share
    /// the "tool trusts the ambient cloud SDK" shape.
    CloudProvider { provider: String },
    /// Driver checked but couldn't determine status (timeout, permission
    /// denied on the credentials file, …). The UI treats this as "probably
    /// works" — we don't want to block on a flaky detector.
    Unknown,
}

/// Portable abstraction over an AI CLI that can drive a PTY agent.
///
/// Implementations must be `Send + Sync` — the registry shares them across
/// tasks. All methods take `&self` so a single driver instance can back
/// many concurrent agents.
pub trait AgentDriver: Send + Sync {
    /// Name of the CLI binary the driver invokes (e.g. `"claude"`). This
    /// is the first element of [`Self::spawn_args`]; exposed separately so
    /// UI / logs can report the backend without parsing argv.
    fn binary(&self) -> &str;

    /// Full argv (binary first) to spawn a new PTY session with the given
    /// options.
    fn spawn_args(&self, opts: &SpawnOpts<'_>) -> Vec<String>;

    /// Transform a raw task prompt before it's injected into the PTY.
    /// Default is identity — drivers override when their CLI expects a
    /// specific wrapper (e.g. slash commands, markdown fences).
    fn format_prompt(&self, text: &str) -> String {
        text.to_string()
    }

    /// Return true when the accumulated PTY output contains this CLI's
    /// ready-for-input indicator. Used to gate initial prompt injection
    /// so we don't type before the splash screen finishes.
    fn is_ready(&self, output: &[u8]) -> bool;

    /// Extract the total cost (USD) reported by the CLI, if any. Input is
    /// the concatenated PTY transcript (ANSI-tainted is fine — the driver
    /// strips what it needs).
    fn parse_cost(&self, output: &str) -> Option<f64>;

    /// Extract a check-agent verdict from the CLI's output. Returns `None`
    /// when no verdict JSON is found.
    #[allow(dead_code)] // consumed by check_agent in a later Phase 1 task
    fn parse_verdict(&self, output: &str) -> Option<Verdict>;

    /// Byte sequence to send over the PTY to ask the CLI to exit cleanly.
    /// The default of `"/exit\r"` matches Claude Code's slash-command exit;
    /// drivers whose CLI doesn't use slash commands override. `None` means
    /// there is no clean exit path and the dashboard should fall back to
    /// the kill path.
    fn graceful_exit_sequence(&self) -> Option<&[u8]> {
        Some(b"/exit\r")
    }

    /// What this driver can do. Default is the Claude-shaped profile (all
    /// true, not interactive-only); drivers with fewer features override.
    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::default()
    }

    /// Content of an `.mcp.json` file that registers the Branchwork MCP
    /// server with the CLI. `Some` means the driver supports config
    /// injection — the caller writes this string to a file and passes the
    /// path via [`SpawnOpts::mcp_config_path`]. `None` (the default) means
    /// the driver has no portable MCP integration and the file is skipped.
    ///
    /// `port` is the TCP port the dashboard's HTTP MCP endpoint is bound
    /// to. Drivers that prefer stdio (e.g. to avoid requiring the server
    /// to be reachable over HTTP) can ignore it and emit a stdio entry
    /// instead.
    fn mcp_config_json(&self, _port: u16) -> Option<String> {
        None
    }

    /// Returns the Stop-hook JSON fragment this driver wants written
    /// into a per-session settings file, or `None` if the driver has
    /// no portable stop-hook surface (server falls back to the idle
    /// timer for those — see Phase 4).
    ///
    /// `session_id` is the same id passed to `spawn_args` for resume.
    /// `hook_url` is the absolute URL the hook should POST to (e.g.
    /// `http://localhost:3100/hooks`). The driver returns the JSON
    /// the CLI's settings schema expects; the caller wraps it into
    /// the full settings file shape if any other top-level keys are
    /// needed.
    #[allow(dead_code)] // consumed by Phase 1.2+ (Claude impl + per-session settings writer)
    fn stop_hook_config(&self, _session_id: &str, _hook_url: &str) -> Option<serde_json::Value> {
        None
    }

    /// Check whether the CLI is installed and authenticated. Default
    /// implementation just confirms the binary is on PATH and reports
    /// [`AuthStatus::Unknown`] otherwise — drivers that know how to read
    /// their tool's credentials override.
    #[allow(dead_code)] // surfaced by a later Phase 1 auth-detection task
    fn auth_status(&self) -> AuthStatus {
        if !binary_on_path(self.binary()) {
            return AuthStatus::NotInstalled;
        }
        AuthStatus::Unknown
    }
}

/// Check whether a binary is resolvable on the current `PATH`. Cheap —
/// walks `PATH` once per call without spawning a process.
#[allow(dead_code)] // used by AuthStatus::NotInstalled, surfaced by a later task
pub fn binary_on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return true;
        }
        // Windows: also try `.exe`.
        #[cfg(windows)]
        {
            if candidate.with_extension("exe").is_file() {
                return true;
            }
        }
    }
    false
}

/// Strip the common ANSI escape sequences emitted by terminal UIs. Shared
/// helper for driver impls — most CLIs put cost/verdict lines inside
/// color-coded summaries, and every driver wants a clean string before
/// regex-matching.
pub fn strip_ansi(s: &str) -> String {
    // OSC strings (\x1b]...\x07) and CSI sequences (\x1b[...<final>).
    let re = Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\x1b\[.*?[@-~]").unwrap();
    re.replace_all(s, "").to_string()
}

/// Driver for Anthropic's `claude` CLI — the only driver wired up today.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeDriver;

impl ClaudeDriver {
    pub const fn new() -> Self {
        Self
    }
}

/// Claude Code's prompt glyph (U+276F `❯`). Presence in the rolling output
/// accumulator means the CLI is accepting keystrokes.
const CLAUDE_PROMPT_GLYPH: &[u8] = "❯".as_bytes();

impl AgentDriver for ClaudeDriver {
    fn binary(&self) -> &str {
        "claude"
    }

    fn spawn_args(&self, opts: &SpawnOpts<'_>) -> Vec<String> {
        let mut cmd: Vec<String> = vec![
            self.binary().to_string(),
            "--session-id".to_string(),
            opts.session_id.to_string(),
            "--add-dir".to_string(),
            opts.cwd.to_string_lossy().to_string(),
            "--verbose".to_string(),
            "--effort".to_string(),
            opts.effort.to_string(),
        ];
        if opts.skip_permissions {
            cmd.push("--dangerously-skip-permissions".to_string());
        }
        if let Some(v) = opts.max_budget_usd {
            cmd.push("--max-budget-usd".to_string());
            cmd.push(v.to_string());
        }
        if let Some(path) = opts.mcp_config_path {
            cmd.push("--mcp-config".to_string());
            cmd.push(path.to_string_lossy().to_string());
        }
        cmd
    }

    fn is_ready(&self, output: &[u8]) -> bool {
        output
            .windows(CLAUDE_PROMPT_GLYPH.len())
            .any(|w| w == CLAUDE_PROMPT_GLYPH)
    }

    fn mcp_config_json(&self, port: u16) -> Option<String> {
        // Claude Code's `.mcp.json` schema: top-level `mcpServers` map,
        // one entry per server. We emit a streamable-HTTP entry pointing
        // at the dashboard's `/mcp` endpoint (same transport the server
        // already exposes at that path). Server name "branchwork" maps to
        // tool names like `mcp__branchwork__list_plans` on the Claude side.
        let cfg = serde_json::json!({
            "mcpServers": {
                "branchwork": {
                    "type": "http",
                    "url": format!("http://127.0.0.1:{port}/mcp"),
                }
            }
        });
        Some(cfg.to_string())
    }

    fn stop_hook_config(&self, session_id: &str, hook_url: &str) -> Option<serde_json::Value> {
        // Claude Code's settings.json hook schema:
        //   { "hooks": { "Stop": [ { "matcher": "...", "hooks": [...] } ] } }
        // Stop has no matcher concept (unlike PreToolUse), so the field is
        // empty. The session_id is hard-coded into the curl payload at
        // write-time so `hooks::receive_hook` can route to the right agent;
        // `--max-time 5` caps the POST so a stuck server can't hang the
        // agent's own exit.
        let body = format!(r#"{{"session_id":"{session_id}","hook_event_name":"Stop"}}"#);
        let command = format!(
            "curl -sS --max-time 5 -X POST {hook_url} -H 'Content-Type: application/json' -d '{body}'"
        );
        Some(serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            {
                                "type": "command",
                                "command": command,
                            }
                        ]
                    }
                ]
            }
        }))
    }

    fn parse_cost(&self, output: &str) -> Option<f64> {
        let clean = strip_ansi(output);
        let re = Regex::new(r"(?i)total\s+cost[:\s]*\$(\d+\.?\d*)").ok()?;
        re.captures(&clean)?.get(1)?.as_str().parse::<f64>().ok()
    }

    fn parse_verdict(&self, output: &str) -> Option<Verdict> {
        parse_status_json_verdict(output)
    }

    fn auth_status(&self) -> AuthStatus {
        if !binary_on_path(self.binary()) {
            return AuthStatus::NotInstalled;
        }

        // API key env var short-circuits everything else the claude CLI does.
        if std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
        {
            return AuthStatus::ApiKey;
        }

        // Bedrock / Vertex bypasses check the same env vars claude does.
        if std::env::var("CLAUDE_CODE_USE_BEDROCK").is_ok() {
            return AuthStatus::CloudProvider {
                provider: "bedrock".into(),
            };
        }
        if std::env::var("CLAUDE_CODE_USE_VERTEX").is_ok() {
            return AuthStatus::CloudProvider {
                provider: "vertex".into(),
            };
        }

        // OAuth / subscription (Max, Pro). Claude Code writes an OAuth
        // credentials blob here once sign-in completes; we don't parse it
        // (scopes change between versions), just use its existence as the
        // signal. Account name is best-effort — read from the SDK-written
        // sidecar if present, otherwise leave None.
        if let Some(home) = dirs::home_dir() {
            let creds = home.join(".claude").join(".credentials.json");
            if creds.exists() {
                let account = std::fs::read_to_string(&creds)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .and_then(|v| {
                        v.get("claudeAiOauth")
                            .and_then(|oa| oa.get("subscriptionType"))
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                    });
                return AuthStatus::Oauth { account };
            }
        }

        AuthStatus::Unauthenticated {
            help: "Run `claude` in a terminal to complete OAuth sign-in, or set \
                   `ANTHROPIC_API_KEY` and restart Branchwork."
                .into(),
        }
    }
}

/// Shared verdict-JSON extractor. The check-agent contract (status + reason
/// blob) is CLI-independent — every driver wraps the same walk. Walks from
/// the end: find the last `"status"` anchor, snap back to the enclosing `{`,
/// then try progressively-longer prefixes until one parses as JSON.
fn parse_status_json_verdict(output: &str) -> Option<Verdict> {
    let start = output.rfind(r#""status""#)?;
    let json_start = output[..start].rfind('{')?;
    let remainder = &output[json_start..];

    let parsed = (1..=remainder.len())
        .filter(|&i| remainder.as_bytes().get(i - 1) == Some(&b'}'))
        .find_map(|i| serde_json::from_str::<serde_json::Value>(&remainder[..i]).ok())?;

    let status = parsed
        .get("status")
        .and_then(|s| s.as_str())
        .filter(|s| ["completed", "in_progress", "pending"].contains(s))
        .unwrap_or("pending")
        .to_string();
    let reason = parsed
        .get("reason")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    Some(Verdict { status, reason })
}

/// Driver for the `aider` CLI. Spawned in interactive PTY mode — the task
/// prompt is typed at Aider's `>` prompt once the splash is rendered. Aider
/// makes its own git commits per message; we let those drive progress
/// signals through the existing file-watcher / auto-status path rather than
/// reinventing detection here.
#[derive(Debug, Default, Clone, Copy)]
pub struct AiderDriver;

impl AiderDriver {
    pub const fn new() -> Self {
        Self
    }
}

/// Aider's default input prompt when prompt_toolkit has finished rendering.
/// Line-anchored to avoid matching the `>` that appears inside banner text
/// like `Use /help <...>`.
const AIDER_PROMPT_MARKER: &[u8] = b"\n> ";

impl AgentDriver for AiderDriver {
    fn binary(&self) -> &str {
        "aider"
    }

    fn spawn_args(&self, _opts: &SpawnOpts<'_>) -> Vec<String> {
        // Aider picks up cwd from the PTY daemon (supervisor sets it before
        // exec) and discovers the git root on its own. `--yes-always`
        // suppresses the interactive y/n confirmations that would otherwise
        // stall an unattended task. Aider has no session-id / effort /
        // max-budget concepts, so SpawnOpts' remaining fields are unused.
        vec![self.binary().to_string(), "--yes-always".to_string()]
    }

    fn is_ready(&self, output: &[u8]) -> bool {
        output
            .windows(AIDER_PROMPT_MARKER.len())
            .any(|w| w == AIDER_PROMPT_MARKER)
    }

    fn parse_cost(&self, output: &str) -> Option<f64> {
        // Aider prints one summary line per message:
        //   Tokens: 3.2k sent, 287 received. Cost: $0.0156 message, $0.0234 session.
        // We want the cumulative `session` figure — take the last match so
        // mid-run rows are overwritten by the final total.
        let clean = strip_ansi(output);
        let re = Regex::new(r"(?i)\$(\d+\.?\d*)\s+session").ok()?;
        re.captures_iter(&clean)
            .last()?
            .get(1)?
            .as_str()
            .parse::<f64>()
            .ok()
    }

    fn parse_verdict(&self, output: &str) -> Option<Verdict> {
        parse_status_json_verdict(output)
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Aider prints its own per-message `$X session` total (parse_cost
        // picks it up) and responds to the same JSON verdict convention,
        // but has no session-id / resume flag and only runs as a REPL.
        DriverCapabilities {
            supports_cost: true,
            supports_verdict: true,
            supports_session_id: false,
            interactive_only: true,
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if !binary_on_path(self.binary()) {
            return AuthStatus::NotInstalled;
        }
        // Aider accepts any of several provider keys — we only need one.
        for var in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GEMINI_API_KEY",
            "DEEPSEEK_API_KEY",
        ] {
            if std::env::var(var)
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
            {
                return AuthStatus::ApiKey;
            }
        }
        AuthStatus::Unauthenticated {
            help: "Set an API key for your preferred model (`OPENAI_API_KEY`, \
                   `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, etc.) and restart Branchwork."
                .into(),
        }
    }
}

/// Driver for OpenAI's `codex` CLI. Skeleton only — spawns the REPL in
/// interactive mode and detects readiness via the common `> ` input prompt.
/// Cost and verdict parsing return `None` until a user actually adopts this
/// backend and we learn the CLI's summary format; the generic JSON-verdict
/// walker would kick in automatically if the prompt template is updated to
/// emit the same `{"status": ..., "reason": ...}` blob at end-of-run.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodexDriver;

impl CodexDriver {
    pub const fn new() -> Self {
        Self
    }
}

/// Line-anchored prompt marker for codex / gemini REPLs. Both CLIs render a
/// `> ` input line once their splash is done, same as aider — keeping the
/// anchor on a fresh line avoids matching the `>` inside banner help text.
const GENERIC_REPL_PROMPT_MARKER: &[u8] = b"\n> ";

impl AgentDriver for CodexDriver {
    fn binary(&self) -> &str {
        "codex"
    }

    fn spawn_args(&self, _opts: &SpawnOpts<'_>) -> Vec<String> {
        // Codex CLI infers cwd from the daemon and has no stable session-id /
        // effort flags yet — keep the argv minimal so the binary's own
        // defaults drive behaviour.
        vec![self.binary().to_string()]
    }

    fn is_ready(&self, output: &[u8]) -> bool {
        output
            .windows(GENERIC_REPL_PROMPT_MARKER.len())
            .any(|w| w == GENERIC_REPL_PROMPT_MARKER)
    }

    fn parse_cost(&self, _output: &str) -> Option<f64> {
        None
    }

    fn parse_verdict(&self, output: &str) -> Option<Verdict> {
        parse_status_json_verdict(output)
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Codex CLI currently has no cost summary we can scrape and no
        // session-id / headless mode — interactive-only skeleton.
        DriverCapabilities {
            supports_cost: false,
            supports_verdict: true,
            supports_session_id: false,
            interactive_only: true,
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if !binary_on_path(self.binary()) {
            return AuthStatus::NotInstalled;
        }
        if std::env::var("OPENAI_API_KEY")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return AuthStatus::ApiKey;
        }
        AuthStatus::Unauthenticated {
            help: "Set `OPENAI_API_KEY` and restart Branchwork, or sign in by \
                   running `codex` once in a terminal."
                .into(),
        }
    }
}

/// Driver for Google's `gemini` CLI. Skeleton, same shape as [`CodexDriver`].
#[derive(Debug, Default, Clone, Copy)]
pub struct GeminiDriver;

impl GeminiDriver {
    pub const fn new() -> Self {
        Self
    }
}

impl AgentDriver for GeminiDriver {
    fn binary(&self) -> &str {
        "gemini"
    }

    fn spawn_args(&self, _opts: &SpawnOpts<'_>) -> Vec<String> {
        vec![self.binary().to_string()]
    }

    fn is_ready(&self, output: &[u8]) -> bool {
        output
            .windows(GENERIC_REPL_PROMPT_MARKER.len())
            .any(|w| w == GENERIC_REPL_PROMPT_MARKER)
    }

    fn parse_cost(&self, _output: &str) -> Option<f64> {
        None
    }

    fn parse_verdict(&self, output: &str) -> Option<Verdict> {
        parse_status_json_verdict(output)
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Mirrors Codex: interactive-only skeleton, no cost scraping yet.
        DriverCapabilities {
            supports_cost: false,
            supports_verdict: true,
            supports_session_id: false,
            interactive_only: true,
        }
    }

    fn auth_status(&self) -> AuthStatus {
        if !binary_on_path(self.binary()) {
            return AuthStatus::NotInstalled;
        }
        if std::env::var("GEMINI_API_KEY")
            .ok()
            .is_some_and(|v| !v.trim().is_empty())
            || std::env::var("GOOGLE_API_KEY")
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
        {
            return AuthStatus::ApiKey;
        }
        AuthStatus::Unauthenticated {
            help: "Set `GEMINI_API_KEY` or `GOOGLE_API_KEY` and restart Branchwork.".into(),
        }
    }
}

/// Name that identifies the default driver in the registry and on the
/// `agents.driver` DB column. Exposed as a constant so API, DB, and UI
/// layers all agree on the spelling.
pub const DEFAULT_DRIVER: &str = "claude";

/// Immutable map of driver name → driver impl, built once at startup.
///
/// Cloning is cheap: the inner `HashMap` is wrapped in an `Arc`, and each
/// driver value is an `Arc<dyn AgentDriver>` so the same instance is shared
/// across every agent using it. Lookups are by exact name; unknown names
/// fall back to [`DEFAULT_DRIVER`] in [`Self::get_or_default`].
#[derive(Clone)]
pub struct DriverRegistry {
    drivers: Arc<std::collections::HashMap<String, Arc<dyn AgentDriver>>>,
}

impl DriverRegistry {
    /// Build the default registry: just `claude` today, but this is the one
    /// entry point future drivers (codex, gemini, aider, ...) plug into.
    pub fn with_defaults() -> Self {
        let mut map: std::collections::HashMap<String, Arc<dyn AgentDriver>> =
            std::collections::HashMap::new();
        map.insert(
            DEFAULT_DRIVER.to_string(),
            Arc::new(ClaudeDriver::new()) as Arc<dyn AgentDriver>,
        );
        map.insert(
            "aider".to_string(),
            Arc::new(AiderDriver::new()) as Arc<dyn AgentDriver>,
        );
        map.insert(
            "codex".to_string(),
            Arc::new(CodexDriver::new()) as Arc<dyn AgentDriver>,
        );
        map.insert(
            "gemini".to_string(),
            Arc::new(GeminiDriver::new()) as Arc<dyn AgentDriver>,
        );
        Self {
            drivers: Arc::new(map),
        }
    }

    /// Exact lookup by driver name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentDriver>> {
        self.drivers.get(name).cloned()
    }

    /// Lookup by name, falling back to [`DEFAULT_DRIVER`] when the name is
    /// unknown or `None`. Always returns a driver — the default is always
    /// present in a registry built via [`Self::with_defaults`].
    pub fn get_or_default(&self, name: Option<&str>) -> (String, Arc<dyn AgentDriver>) {
        if let Some(n) = name
            && let Some(d) = self.drivers.get(n)
        {
            return (n.to_string(), d.clone());
        }
        let d = self
            .drivers
            .get(DEFAULT_DRIVER)
            .expect("DEFAULT_DRIVER missing from registry")
            .clone();
        (DEFAULT_DRIVER.to_string(), d)
    }

    /// Sorted list of driver names — for `GET /api/drivers` and for the
    /// UI's driver dropdown.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.drivers.keys().cloned().collect();
        out.sort();
        out
    }

    /// Whether the driver resolved from `name` (falling back to the default)
    /// auto-registers the Branchwork MCP server with its CLI. Callers use
    /// this to decide whether a spawned agent can reach MCP tools like
    /// `update_task_status`, or whether the prompt needs to fall back to
    /// curl against the HTTP API.
    pub fn injects_mcp(&self, name: Option<&str>, port: u16) -> bool {
        self.get_or_default(name).1.mcp_config_json(port).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn trait_is_object_safe() {
        // Purely a compile-time check: if AgentDriver gained a generic
        // method or a `Self: Sized` bound this line would stop compiling.
        let _driver: Box<dyn AgentDriver> = Box::new(ClaudeDriver::new());
    }

    #[test]
    fn claude_spawn_args_includes_core_flags() {
        let driver = ClaudeDriver::new();
        let cwd = PathBuf::from("/tmp/project");
        let args = driver.spawn_args(&SpawnOpts {
            session_id: "sess-abc",
            cwd: &cwd,
            effort: Effort::High,
            max_budget_usd: None,
            mcp_config_path: None,
            skip_permissions: true,
        });
        assert_eq!(args.first().map(String::as_str), Some("claude"));
        assert!(args.iter().any(|a| a == "--session-id"));
        assert!(args.iter().any(|a| a == "sess-abc"));
        assert!(args.iter().any(|a| a == "--add-dir"));
        assert!(args.iter().any(|a| a == "/tmp/project"));
        assert!(args.iter().any(|a| a == "--effort"));
        assert!(args.iter().any(|a| a == "high"));
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(!args.iter().any(|a| a == "--max-budget-usd"));
        assert!(!args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn claude_spawn_args_omits_skip_permissions_when_disabled() {
        let driver = ClaudeDriver::new();
        let cwd = PathBuf::from("/tmp/project");
        let args = driver.spawn_args(&SpawnOpts {
            session_id: "s",
            cwd: &cwd,
            effort: Effort::High,
            max_budget_usd: None,
            mcp_config_path: None,
            skip_permissions: false,
        });
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_spawn_args_appends_budget_when_set() {
        let driver = ClaudeDriver::new();
        let cwd = PathBuf::from("/tmp/project");
        let args = driver.spawn_args(&SpawnOpts {
            session_id: "s",
            cwd: &cwd,
            effort: Effort::Low,
            max_budget_usd: Some(2.50),
            mcp_config_path: None,
            skip_permissions: true,
        });
        let i = args.iter().position(|a| a == "--max-budget-usd").unwrap();
        assert_eq!(args[i + 1], "2.5");
    }

    #[test]
    fn claude_spawn_args_appends_mcp_config_when_set() {
        let driver = ClaudeDriver::new();
        let cwd = PathBuf::from("/tmp/project");
        let mcp_path = PathBuf::from("/tmp/agent-abc.mcp.json");
        let args = driver.spawn_args(&SpawnOpts {
            session_id: "s",
            cwd: &cwd,
            effort: Effort::Low,
            max_budget_usd: None,
            mcp_config_path: Some(&mcp_path),
            skip_permissions: true,
        });
        let i = args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(args[i + 1], "/tmp/agent-abc.mcp.json");
    }

    #[test]
    fn claude_mcp_config_json_has_expected_shape() {
        let driver = ClaudeDriver::new();
        let raw = driver.mcp_config_json(3100).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &parsed["mcpServers"]["branchwork"];
        assert_eq!(entry["type"], "http");
        assert_eq!(entry["url"], "http://127.0.0.1:3100/mcp");
    }

    #[test]
    fn claude_is_ready_matches_prompt_glyph() {
        let driver = ClaudeDriver::new();
        assert!(!driver.is_ready(b"starting up..."));
        let mut buf = b"starting\n".to_vec();
        buf.extend_from_slice("❯ ".as_bytes());
        assert!(driver.is_ready(&buf));
    }

    #[test]
    fn claude_parse_cost_matches_summary_line() {
        let driver = ClaudeDriver::new();
        assert_eq!(
            driver.parse_cost("some output\nTotal cost:      $0.1234\nmore\n"),
            Some(0.1234)
        );
        assert_eq!(
            driver.parse_cost("\x1b[32mTotal cost: $12.34\x1b[0m"),
            Some(12.34)
        );
        assert_eq!(driver.parse_cost("no cost here"), None);
    }

    #[test]
    fn claude_parse_verdict_pulls_status_and_reason() {
        let driver = ClaudeDriver::new();
        let output = r#"blah {"status": "completed", "reason": "done"} trailing"#;
        let v = driver.parse_verdict(output).unwrap();
        assert_eq!(v.status, "completed");
        assert_eq!(v.reason, "done");
    }

    #[test]
    fn claude_parse_verdict_defaults_unknown_status_to_pending() {
        let driver = ClaudeDriver::new();
        let output = r#"{"status": "weird", "reason": "r"}"#;
        let v = driver.parse_verdict(output).unwrap();
        assert_eq!(v.status, "pending");
    }

    #[test]
    fn claude_parse_verdict_returns_none_when_absent() {
        let driver = ClaudeDriver::new();
        assert!(driver.parse_verdict("nothing to see here").is_none());
    }

    #[test]
    fn format_prompt_default_is_identity() {
        let driver = ClaudeDriver::new();
        assert_eq!(driver.format_prompt("hello"), "hello");
    }

    #[test]
    fn registry_has_default_driver() {
        let reg = DriverRegistry::with_defaults();
        assert!(reg.names().contains(&DEFAULT_DRIVER.to_string()));
        let (name, _) = reg.get_or_default(None);
        assert_eq!(name, DEFAULT_DRIVER);
    }

    #[test]
    fn registry_unknown_name_falls_back_to_default() {
        let reg = DriverRegistry::with_defaults();
        let (name, _) = reg.get_or_default(Some("does-not-exist"));
        assert_eq!(name, DEFAULT_DRIVER);
    }

    #[test]
    fn registry_exact_lookup_matches() {
        let reg = DriverRegistry::with_defaults();
        let (name, _) = reg.get_or_default(Some("claude"));
        assert_eq!(name, "claude");
        assert!(reg.get("claude").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn registry_includes_aider() {
        let reg = DriverRegistry::with_defaults();
        assert!(reg.names().iter().any(|n| n == "aider"));
        let (name, driver) = reg.get_or_default(Some("aider"));
        assert_eq!(name, "aider");
        assert_eq!(driver.binary(), "aider");
    }

    #[test]
    fn aider_spawn_args_is_yes_always() {
        let driver = AiderDriver::new();
        let cwd = PathBuf::from("/tmp/project");
        let args = driver.spawn_args(&SpawnOpts {
            session_id: "ignored",
            cwd: &cwd,
            effort: Effort::High,
            max_budget_usd: Some(5.0),
            mcp_config_path: None,
            skip_permissions: true,
        });
        assert_eq!(args, vec!["aider".to_string(), "--yes-always".to_string()]);
    }

    #[test]
    fn aider_is_ready_matches_prompt_line() {
        let driver = AiderDriver::new();
        assert!(!driver.is_ready(b"Aider v0.1\nModels: foo\n"));
        // The readiness marker is "\n> " at column zero of the input line.
        assert!(driver.is_ready(b"Aider v0.1\nUse /help <...>\n> "));
    }

    #[test]
    fn aider_parse_cost_picks_last_session_total() {
        let driver = AiderDriver::new();
        let output = "\
Tokens: 100 sent, 50 received. Cost: $0.0100 message, $0.0100 session.
... edits ...
Tokens: 200 sent, 75 received. Cost: $0.0150 message, $0.0250 session.
";
        assert_eq!(driver.parse_cost(output), Some(0.0250));
        assert_eq!(driver.parse_cost("no cost here"), None);
    }

    #[test]
    fn aider_parse_cost_strips_ansi() {
        let driver = AiderDriver::new();
        let output = "\x1b[32mCost: $0.01 message, $0.99 session.\x1b[0m";
        assert_eq!(driver.parse_cost(output), Some(0.99));
    }

    #[test]
    fn registry_includes_codex_and_gemini() {
        let reg = DriverRegistry::with_defaults();
        let names = reg.names();
        assert!(names.iter().any(|n| n == "codex"));
        assert!(names.iter().any(|n| n == "gemini"));
        assert_eq!(reg.get("codex").unwrap().binary(), "codex");
        assert_eq!(reg.get("gemini").unwrap().binary(), "gemini");
    }

    #[test]
    fn codex_and_gemini_spawn_args_are_binary_only() {
        let cwd = PathBuf::from("/tmp/project");
        let opts = SpawnOpts {
            session_id: "ignored",
            cwd: &cwd,
            effort: Effort::High,
            max_budget_usd: Some(1.0),
            mcp_config_path: None,
            skip_permissions: true,
        };
        assert_eq!(CodexDriver::new().spawn_args(&opts), vec!["codex"]);
        assert_eq!(GeminiDriver::new().spawn_args(&opts), vec!["gemini"]);
    }

    #[test]
    fn non_claude_drivers_have_no_mcp_config() {
        // Default trait impl returns None for drivers that haven't opted
        // in to MCP injection. Only Claude ships a config today.
        assert!(AiderDriver::new().mcp_config_json(3100).is_none());
        assert!(CodexDriver::new().mcp_config_json(3100).is_none());
        assert!(GeminiDriver::new().mcp_config_json(3100).is_none());
    }

    #[test]
    fn non_claude_drivers_have_no_stop_hook_config() {
        // Default trait impl returns None — Phase 1.1 shipped the surface
        // with no driver overrides; only Claude plugs in (Phase 1.2).
        let url = "http://localhost:3100/hooks";
        assert!(AiderDriver::new().stop_hook_config("sess", url).is_none());
        assert!(CodexDriver::new().stop_hook_config("sess", url).is_none());
        assert!(GeminiDriver::new().stop_hook_config("sess", url).is_none());
    }

    #[test]
    fn claude_stop_hook_config_has_expected_shape() {
        let driver = ClaudeDriver::new();
        let session_id = "sess-abc";
        let hook_url = "http://localhost:3100/hooks";
        let cfg = driver.stop_hook_config(session_id, hook_url).unwrap();

        let stop_arr = cfg["hooks"]["Stop"]
            .as_array()
            .expect("hooks.Stop is an array");
        assert!(!stop_arr.is_empty(), "hooks.Stop is non-empty");

        let entry = &stop_arr[0];
        assert_eq!(entry["matcher"], "");
        let inner = entry["hooks"]
            .as_array()
            .expect("Stop[0].hooks is an array");
        assert!(!inner.is_empty(), "Stop[0].hooks is non-empty");
        assert_eq!(inner[0]["type"], "command");

        let cmd = inner[0]["command"].as_str().expect("command is a string");
        assert!(
            cmd.contains(session_id),
            "command should contain session_id: {cmd}",
        );
        assert!(
            cmd.contains(hook_url),
            "command should contain hook_url: {cmd}",
        );
        assert!(
            cmd.contains("--max-time 5"),
            "command should cap with --max-time 5: {cmd}",
        );
    }

    #[test]
    fn codex_and_gemini_is_ready_matches_repl_prompt() {
        assert!(!CodexDriver::new().is_ready(b"loading..."));
        assert!(CodexDriver::new().is_ready(b"Codex ready\n> "));
        assert!(!GeminiDriver::new().is_ready(b"loading..."));
        assert!(GeminiDriver::new().is_ready(b"Gemini ready\n> "));
    }

    #[test]
    fn codex_and_gemini_parse_cost_is_stubbed() {
        assert_eq!(CodexDriver::new().parse_cost("Total cost: $0.50"), None);
        assert_eq!(GeminiDriver::new().parse_cost("Total cost: $0.50"), None);
    }

    #[test]
    fn codex_and_gemini_parse_verdict_use_shared_walker() {
        let output = r#"blah {"status": "completed", "reason": "ok"} tail"#;
        let v = CodexDriver::new().parse_verdict(output).unwrap();
        assert_eq!(v.status, "completed");
        let v = GeminiDriver::new().parse_verdict(output).unwrap();
        assert_eq!(v.reason, "ok");
    }

    #[test]
    fn claude_capabilities_are_fully_featured() {
        let caps = ClaudeDriver::new().capabilities();
        assert!(caps.supports_cost);
        assert!(caps.supports_verdict);
        assert!(caps.supports_session_id);
        assert!(!caps.interactive_only);
    }

    #[test]
    fn aider_capabilities_drop_session_id_and_mark_interactive() {
        let caps = AiderDriver::new().capabilities();
        assert!(caps.supports_cost);
        assert!(caps.supports_verdict);
        assert!(!caps.supports_session_id);
        assert!(caps.interactive_only);
    }

    #[test]
    fn codex_and_gemini_capabilities_drop_cost() {
        for caps in [
            CodexDriver::new().capabilities(),
            GeminiDriver::new().capabilities(),
        ] {
            assert!(!caps.supports_cost);
            assert!(caps.supports_verdict);
            assert!(!caps.supports_session_id);
            assert!(caps.interactive_only);
        }
    }

    #[test]
    fn aider_parse_verdict_uses_shared_walker() {
        let driver = AiderDriver::new();
        let output = r#"blah {"status": "completed", "reason": "done"} tail"#;
        let v = driver.parse_verdict(output).unwrap();
        assert_eq!(v.status, "completed");
        assert_eq!(v.reason, "done");
        assert!(driver.parse_verdict("no json").is_none());
    }
}
