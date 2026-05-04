# Driver reference

A **driver** is the per-CLI shim that lets Branchwork drive any AI coding
tool through a PTY. PTY spawning, readiness detection, cost parsing,
verdict extraction, graceful exit, MCP auto-injection, and auth probing
all route through one trait — so [`agents/pty_agent.rs`](../../server-rs/src/agents/pty_agent.rs)
stays AI-agnostic and a new tool plugs in by implementing the trait and
registering an instance.

This page documents the trait surface, the four drivers shipped today
([`ClaudeDriver`](#claude), [`AiderDriver`](#aider), [`CodexDriver`](#codex),
[`GeminiDriver`](#gemini)), and what to override when authoring a fifth.
Source of truth: [`server-rs/src/agents/driver.rs`](../../server-rs/src/agents/driver.rs).

For where the trait is called from — argv build at spawn, readiness
gating before prompt injection, cost rollup at agent exit, exit
sequence on Finish — see the inline cross-references below and
[architecture/session-daemon.md](../architecture/session-daemon.md).

---

## Drivers shipped today

[`DriverRegistry::with_defaults`](../../server-rs/src/agents/driver.rs)
registers four entries; the default is `"claude"`
([`DEFAULT_DRIVER`](../../server-rs/src/agents/driver.rs)). Lookups by an
unknown name fall back to the default
([`get_or_default`](../../server-rs/src/agents/driver.rs)).

| Name | Binary | Install | Cost | Session-id / resume | Interactive-only | Auto-injects MCP |
|---|---|---|---|---|---|---|
| `claude` | `claude` | `npm install -g @anthropic-ai/claude-code` | yes | yes | no | **yes** |
| `aider` | `aider` | `pip install aider-chat` | yes | no | yes | no |
| `codex` | `codex` | `npm install -g @openai/codex` | no | no | yes | no |
| `gemini` | `gemini` | `npm install -g @google/gemini-cli` | no | no | yes | no |

The capability columns map directly to fields on
[`DriverCapabilities`](#drivercapabilities); the dashboard uses them to
hide features the backend can't actually populate (no cost column for
`codex`/`gemini`, no `--session-id` plumbing for `aider`, etc.).

The dashboard exposes this list at `GET /api/drivers`
([`api/agents.rs::list_drivers`](../../server-rs/src/api/agents.rs)),
returning `{ drivers: [{name, binary, capabilities, auth_status}], default }`.

---

## The `AgentDriver` trait

```rust
pub trait AgentDriver: Send + Sync {
    fn binary(&self) -> &str;
    fn spawn_args(&self, opts: &SpawnOpts<'_>) -> Vec<String>;
    fn format_prompt(&self, text: &str) -> String;            // default: identity
    fn is_ready(&self, output: &[u8]) -> bool;
    fn parse_cost(&self, output: &str) -> Option<f64>;
    fn parse_verdict(&self, output: &str) -> Option<Verdict>;
    fn graceful_exit_sequence(&self) -> Option<&[u8]>;        // default: Some(b"/exit\r")
    fn capabilities(&self) -> DriverCapabilities;             // default: all-true
    fn mcp_config_json(&self, port: u16) -> Option<String>;   // default: None
    fn stop_hook_config(&self, session_id: &str, hook_url: &str) -> Option<serde_json::Value>; // default: None
    fn auth_status(&self) -> AuthStatus;                      // default: NotInstalled / Unknown
}
```

All methods take `&self` so a single driver instance can back many
concurrent agents. The trait is deliberately **object-safe** —
[`DriverRegistry`](../../server-rs/src/agents/driver.rs) stores
`Arc<dyn AgentDriver>` and a unit test
([`trait_is_object_safe`](../../server-rs/src/agents/driver.rs)) compiles
out the moment a generic method or `Self: Sized` bound is added.

### Method semantics

| Method | Called from | Default | What an impl must / may override |
|---|---|---|---|
| `binary` | logs, `auth_status` default, `spawn_args` first arg | — | **Required.** Must be the bare CLI name; the supervisor invokes the binary directly via `portable-pty` (no shell), so `$PATH` lookup is on the spawn side. |
| `spawn_args` | [`pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs) | — | **Required.** Returns the full argv (binary first). Use [`SpawnOpts`](#spawnopts) for caller-supplied flags. Reading any field off `SpawnOpts` is optional — drivers that lack a feature simply ignore the field. |
| `format_prompt` | [`pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs) (passed to `inject_prompt_when_ready`) | identity | Override when the CLI expects a wrapper (slash command, fenced block). All four shipped drivers use the default. |
| `is_ready` | [`pty_agent.rs::inject_prompt_when_ready`](../../server-rs/src/agents/pty_agent.rs) | — | **Required.** Inspects the rolling 16 KiB tail of PTY output (`READINESS_BUFFER_CAP`); return `true` when the CLI has finished its splash and is accepting keystrokes. A 16-second fallback fires the prompt anyway if the marker is never seen. |
| `parse_cost` | [`pty_agent.rs::parse_cost_from_pty_output`](../../server-rs/src/agents/pty_agent.rs) at agent exit | — | **Required.** Receives the joined last ~50 `agent_output` rows for the agent. Returning `None` is fine — the dashboard's cost column shows blank. Use [`strip_ansi`](#shared-helpers) before regex-matching. |
| `parse_verdict` | check-agent verdict extraction | — | **Required.** All four shipped drivers delegate to [`parse_status_json_verdict`](#shared-helpers); only override if your CLI emits a different verdict shape. |
| `graceful_exit_sequence` | [`agents/mod.rs::graceful_exit`](../../server-rs/src/agents/mod.rs) on Finish | `Some(b"/exit\r")` | Most CLIs accept `/exit`. Override with a different sequence (e.g. `b"\x04"` for Ctrl-D), or return `None` to force the dashboard to fall through to Kill. |
| `capabilities` | UI gating, `GET /api/drivers` | all true, not interactive | Override the bools your CLI can't deliver. See [`DriverCapabilities`](#drivercapabilities). |
| `mcp_config_json` | [`pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs) (file is written next to the agent's socket and passed via `SpawnOpts.mcp_config_path`) | `None` | Return the contents of an `.mcp.json` file that registers the Branchwork MCP server with this CLI. Only `claude` ships one today. |
| `stop_hook_config` | [`pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs) (written to a per-session settings file and passed via `SpawnOpts.settings_path`) | `None` | Return the JSON to splice into the per-session settings file so the CLI fires a Stop hook back at the server when the model finishes its turn. Drivers that return `None` fall back to the idle-timer poller. See [Stop hooks and unattended auto-mode](#stop-hooks-and-unattended-auto-mode). |
| `auth_status` | `GET /api/drivers`, dashboard Drivers panel | `NotInstalled` if binary absent, else `Unknown` | Override when you can read the CLI's credentials yourself (env var, dotfile). Returning `Unknown` is fine — the UI treats it as "probably works" and doesn't block Start/Continue. |

### `SpawnOpts`

Flat struct passed to `spawn_args` ([`driver.rs:22`](../../server-rs/src/agents/driver.rs)):

| Field | Type | Set by | Used by |
|---|---|---|---|
| `session_id` | `&str` | server (uuid v4) | drivers that support resume — `claude` passes it as `--session-id`. Drivers without resume ignore it; the value is still kept for bookkeeping in the `agents` row. |
| `cwd` | `&Path` | server / runner | every driver — passed to the supervisor before `exec`, so the CLI inherits it. `claude` *also* passes `--add-dir <cwd>` because Claude Code's `--add-dir` adds an explicit allowed-paths entry on top of cwd. |
| `effort` | `Effort` (`low`/`medium`/`high`/`max`) | `--effort` flag or runtime `/api/settings` | `claude` only; other drivers have no equivalent flag. |
| `max_budget_usd` | `Option<f64>` | per-plan budget, if set | `claude` only — passed as `--max-budget-usd <v>` so the CLI itself enforces a hard ceiling. Other drivers rely on Branchwork's server-side budget enforcement in `pty_agent`. |
| `mcp_config_path` | `Option<&Path>` | server (only when `mcp_config_json` returned `Some`) | drivers that opt into MCP injection — `claude` passes it as `--mcp-config <path>`. The caller writes the file before spawn; the driver only reads `mcp_config_json` to know *what* to write. |

### `DriverCapabilities`

```rust
pub struct DriverCapabilities {
    pub supports_cost: bool,         // dashboard hides cost column when false
    pub supports_verdict: bool,      // check-agent verdict JSON is supported
    pub supports_session_id: bool,   // CLI accepts --session-id (or equivalent) for resume
    pub interactive_only: bool,      // CLI is REPL-only — check-agents fall back to PTY path
}
```

Defaults mirror the Claude profile (all true, not interactive-only) so a
new driver that forgets to override `capabilities()` is *assumed* fully
featured. Each driver explicitly sets the bools it can't deliver.

### `AuthStatus`

Reported per driver to the dashboard (snake_case JSON via
`#[serde(tag = "kind", rename_all = "snake_case")]`):

| Variant | Meaning | Dashboard behaviour |
|---|---|---|
| `not_installed` | Binary not on `PATH` ([`binary_on_path`](../../server-rs/src/agents/driver.rs)). | Greys out Start/Continue, shows install hint. |
| `unauthenticated { help }` | Binary present but no credentials detected. `help` is a short markdown string the UI surfaces verbatim. | Greys out Start/Continue, surfaces `help` as the fix-it. |
| `oauth { account }` | OAuth/subscription session detected (Claude Max / Pro / …). `account` is best-effort (subscription type or email). | Treated as authenticated. |
| `api_key` | An expected API-key env var is set non-empty. | Treated as authenticated. |
| `cloud_provider { provider }` | Cloud-SDK creds — Bedrock / Vertex. | Treated as authenticated, badge shows the provider. |
| `unknown` | Probe couldn't determine state (timeout, permission denied, no probe defined). | Treats as "probably works" — does not block Start/Continue. |

### Shared helpers

Free functions in `driver.rs` that drivers compose:

| Helper | What it does |
|---|---|
| [`binary_on_path`](../../server-rs/src/agents/driver.rs) | Walks `PATH` looking for the binary file; on Windows also tries `.exe`. Cheap — no process spawn. Used by every driver's `auth_status` default. |
| [`strip_ansi`](../../server-rs/src/agents/driver.rs) | Strips OSC and CSI escape sequences from PTY output. Most CLIs colour their summary lines, so `parse_cost` always wants this first. |
| [`parse_status_json_verdict`](../../server-rs/src/agents/driver.rs) | Walks from the end of the output looking for the last `"status"` key, snaps back to the enclosing `{`, then tries progressively-longer prefixes until one parses as JSON. Status values are filtered to `completed` / `in_progress` / `pending` (anything else folds to `pending`). All four shipped drivers delegate to this. |

---

## `claude`

Anthropic's [Claude Code](https://www.anthropic.com/claude-code) CLI.
The richest backend and the only one with full capabilities — it sets
the defaults the rest of the trait is calibrated against.

**Source:** [`ClaudeDriver`](../../server-rs/src/agents/driver.rs) at
[`driver.rs:228`](../../server-rs/src/agents/driver.rs).

### Install

```bash
npm install -g @anthropic-ai/claude-code
```

### `spawn_args`

```
claude --session-id <session>
       --add-dir <cwd>
       --verbose
       --effort <low|medium|high|max>
       [--max-budget-usd <v>]
       [--mcp-config <path>]
```

`--session-id` is what enables resume. `--add-dir` repeats the cwd as
an explicit allowed directory — Claude Code's permissions model is
opt-in per directory even when the binary is launched from inside one.
`--verbose` is required to make the verdict / cost summary lines
appear in the transcript reliably.

### Auth

Probed in this order ([`auth_status`](../../server-rs/src/agents/driver.rs)):

1. Binary on `PATH` — else `not_installed`.
2. `ANTHROPIC_API_KEY` set non-empty → `api_key`. **Short-circuits the
   rest** — same precedence Claude Code uses internally, so the OAuth
   credentials file is ignored when the env var is present.
3. `CLAUDE_CODE_USE_BEDROCK` set (any value) → `cloud_provider { provider: "bedrock" }`.
4. `CLAUDE_CODE_USE_VERTEX` set (any value) → `cloud_provider { provider: "vertex" }`.
5. `~/.claude/.credentials.json` exists → `oauth { account }`. The file
   is parsed best-effort to read `claudeAiOauth.subscriptionType` (e.g.
   `"max"`, `"pro"`); the contents are not relied on for auth, only for
   labelling.
6. Otherwise `unauthenticated { help: "Run \`claude\` in a terminal to complete OAuth sign-in, or set \`ANTHROPIC_API_KEY\` and restart Branchwork." }`.

### Ready signal

Looks for the prompt glyph `❯` (U+276F) anywhere in the rolling 16 KiB
tail of PTY output. The glyph appears once Claude Code has finished its
splash and is accepting keystrokes.

### Cost parser

```
regex: (?i)total\s+cost[:\s]*\$(\d+\.?\d*)
```

Run after `strip_ansi`, against the joined last ~50 `agent_output` rows.
Matches the `Total cost: $0.1234` line Claude Code prints in its summary.

### Verdict parser

Shared [`parse_status_json_verdict`](#shared-helpers) — same `{"status": ..., "reason": ...}` blob shape as the other drivers.

### Graceful exit

`/exit\r` (the trait default).

### MCP auto-injection

`mcp_config_json(port)` returns the JSON for an `.mcp.json` file:

```json
{
  "mcpServers": {
    "branchwork": {
      "type": "http",
      "url": "http://127.0.0.1:<port>/mcp"
    }
  }
}
```

The server materialises this as `<sessions>/<id>.mcp.json` and the
spawned `claude` process picks it up via `--mcp-config <path>`. Tool
names appear on the Claude side as `mcp__branchwork__list_plans`,
`mcp__branchwork__update_task_status`, etc. Failure to write the file
is non-fatal — the agent runs without MCP injection and the prompt
falls back to curl against the HTTP API.

### Quirks

- `ANTHROPIC_API_KEY` short-circuits OAuth detection. If you have both
  set, the dashboard shows `api_key`, not `oauth`.
- `subscriptionType` schema in `.credentials.json` changes between
  Claude Code releases; the auth probe only uses its existence as the
  signal and treats the `account` field as best-effort.
- Only driver that auto-injects MCP. Spawned `aider`/`codex`/`gemini`
  agents reach Branchwork via the curl fallback the prompt template
  emits — see [`build_task_prompt`](../../server-rs/src/agents/mod.rs).

---

## `aider`

[Aider](https://aider.chat) — paul-gauthier/aider. Provider-agnostic
(OpenAI / Anthropic / Gemini / DeepSeek), driven via Aider's REPL.

**Source:** [`AiderDriver`](../../server-rs/src/agents/driver.rs) at
[`driver.rs:388`](../../server-rs/src/agents/driver.rs).

### Install

```bash
pip install aider-chat
```

### `spawn_args`

```
aider --yes-always
```

`--yes-always` suppresses the interactive y/n confirmations that would
otherwise stall an unattended session. Aider picks up `cwd` from the
PTY daemon (the supervisor sets it before `exec`) and discovers the git
root on its own. `SpawnOpts.session_id`, `.effort`, `.max_budget_usd`,
and `.mcp_config_path` are **all ignored** — Aider has no flags for
them.

### Auth

Aider accepts any of several provider keys; the driver probes them in
order ([`driver.rs:457`](../../server-rs/src/agents/driver.rs)) and
reports `api_key` on the first non-empty hit:

1. `OPENAI_API_KEY`
2. `ANTHROPIC_API_KEY`
3. `GEMINI_API_KEY`
4. `DEEPSEEK_API_KEY`

If none of the four is set: `unauthenticated { help: "Set an API key for your preferred model (\`OPENAI_API_KEY\`, \`ANTHROPIC_API_KEY\`, \`GEMINI_API_KEY\`, etc.) and restart Branchwork." }`.

> The runner's `collect_driver_auth`
> ([`bin/branchwork_runner.rs:766`](../../server-rs/src/bin/branchwork_runner.rs))
> only probes `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` for `aider`. So
> on a SaaS host that authenticates Aider via `GEMINI_API_KEY` or
> `DEEPSEEK_API_KEY`, the **server** reports `api_key` (Aider running
> via its own probe path) but the **runner panel** shows `unknown`.
> Cosmetic only — agent execution is unaffected.

### Ready signal

Line-anchored prompt marker: `\n> `. Aider renders `> ` at column zero
once `prompt_toolkit` has finished its splash. Anchoring on a fresh
line avoids matching the `>` inside banner help text like
`Use /help <...>`.

### Cost parser

```
regex: (?i)\$(\d+\.?\d*)\s+session
```

Run after `strip_ansi`, takes the **last** match. Aider prints one
summary line per message:

```
Tokens: 3.2k sent, 287 received. Cost: $0.0156 message, $0.0234 session.
```

We want the cumulative `session` figure — taking the last match means
intermediate rows are overwritten by the final total.

### Verdict parser

Shared [`parse_status_json_verdict`](#shared-helpers).

### Graceful exit

`/exit\r` (the trait default — Aider's slash-command exit matches
Claude Code's).

### Capabilities

```rust
DriverCapabilities {
    supports_cost: true,
    supports_verdict: true,
    supports_session_id: false,  // no --session-id / resume
    interactive_only: true,      // REPL-only
}
```

### Quirks

- Aider makes its **own** git commits per message. Branchwork lets
  these drive task progress through the existing file-watcher /
  auto-status path rather than reinventing detection. `auto_status`
  caps at `in_progress` regardless ([per-task progress is signalled by file appearance](../architecture/server.md)).
- No MCP auto-injection — the prompt template's curl fallback is what
  reaches the Branchwork API.
- The session cost line only appears once Aider has actually completed
  a turn; cost may be `None` until then.

---

## `codex`

OpenAI's [Codex CLI](https://github.com/openai/codex). Skeleton driver —
spawns the REPL, detects readiness, no cost parsing yet.

**Source:** [`CodexDriver`](../../server-rs/src/agents/driver.rs) at
[`driver.rs:485`](../../server-rs/src/agents/driver.rs).

### Install

```bash
npm install -g @openai/codex
```

### `spawn_args`

```
codex
```

Binary only — Codex CLI infers `cwd` from the daemon and has no stable
session-id / effort / budget flags yet. All `SpawnOpts` fields are
ignored.

### Auth

```
binary on PATH? no  → not_installed
OPENAI_API_KEY set? → api_key
                else → unauthenticated { help: "Set `OPENAI_API_KEY` and restart Branchwork, or sign in by running `codex` once in a terminal." }
```

### Ready signal

Line-anchored `\n> ` (shared `GENERIC_REPL_PROMPT_MARKER`). Same shape
as `aider` and `gemini`.

### Cost parser

`None` — stub. The Codex CLI doesn't yet print a summary line in a
format we have committed to scraping. Override here when a stable cost
format ships.

### Verdict parser

Shared [`parse_status_json_verdict`](#shared-helpers). Will return
`Some(...)` only when the prompt template successfully steers Codex
into emitting the `{"status": ..., "reason": ...}` blob — same
contract as the other skeleton drivers.

### Graceful exit

`/exit\r` (trait default).

### Capabilities

```rust
DriverCapabilities {
    supports_cost: false,
    supports_verdict: true,
    supports_session_id: false,
    interactive_only: true,
}
```

### Quirks

- No cost rollup — the dashboard's cost column shows blank for tasks
  driven by Codex.
- No MCP auto-injection.
- No session-id / resume — finishing and restarting an agent starts a
  fresh Codex conversation.
- Verdict extraction is best-effort and depends on the prompt template
  steering the model into the JSON blob; if the model freelances,
  `parse_verdict` returns `None` and the check agent falls back to the
  PTY transcript.

---

## `gemini`

Google's [Gemini CLI](https://github.com/google-gemini/gemini-cli).
Skeleton driver, same shape as `codex`.

**Source:** [`GeminiDriver`](../../server-rs/src/agents/driver.rs) at
[`driver.rs:555`](../../server-rs/src/agents/driver.rs).

### Install

```bash
npm install -g @google/gemini-cli
```

### `spawn_args`

```
gemini
```

Binary only — same minimal argv as `codex`; all `SpawnOpts` fields
ignored.

### Auth

```
binary on PATH? no   → not_installed
GEMINI_API_KEY set?  → api_key
GOOGLE_API_KEY set?  → api_key   (alternative — either works)
                else → unauthenticated { help: "Set `GEMINI_API_KEY` or `GOOGLE_API_KEY` and restart Branchwork." }
```

### Ready signal

Line-anchored `\n> ` (shared `GENERIC_REPL_PROMPT_MARKER`).

### Cost parser

`None` — stub, same status as `codex`.

### Verdict parser

Shared [`parse_status_json_verdict`](#shared-helpers).

### Graceful exit

`/exit\r` (trait default).

### Capabilities

Same as `codex`:

```rust
DriverCapabilities {
    supports_cost: false,
    supports_verdict: true,
    supports_session_id: false,
    interactive_only: true,
}
```

### Quirks

- Mirrors `codex`: no cost, no MCP, no resume, verdict depends on the
  prompt template.
- Two equivalent env-var spellings (`GEMINI_API_KEY` / `GOOGLE_API_KEY`)
  — Gemini CLI accepts both, and so does the auth probe.

---

## Stop hooks and unattended auto-mode

Auto-mode (the per-plan toggle that auto-merges completed tasks and
spawns the next ready one) needs a way to detect that an agent is
*done thinking* — not just that the PTY is still open. The trait
exposes one optional method that drives this:

```rust
fn stop_hook_config(
    &self,
    session_id: &str,
    hook_url: &str,
) -> Option<serde_json::Value>;   // default: None
```

The server calls it once at spawn time and, if the result is
`Some(value)`, splices `value` into a per-session settings file the
CLI is asked to load. The expected wire-side outcome is the CLI
posting a Stop event back to `hook_url` (`http://localhost:<port>/hooks`)
when the model finishes its turn. The server's hook handler then runs
`graceful_exit` on the agent — gated on a clean working tree, so a
dirty tree pauses the plan instead of overwriting unauthored work.
End-to-end latency is "as soon as the model emits its final message".

Drivers that return `None` (the default) **still auto-finish under
auto-mode**, just via the periodic idle poller: any `running` agent
whose `last_activity_at` is older than the configured threshold gets
the same `graceful_exit` + audit + broadcast treatment, with
`trigger:"idle_timeout"` instead of `trigger:"stop_hook"`. The
fallback is **off by default** — set
[`BRANCHWORK_AUTO_FINISH_IDLE=1`](configuration.md#auto-mode-idle-finish)
to enable it (per-plan auto-mode must also be on for the agent's
plan). The threshold is
[`BRANCHWORK_AUTO_FINISH_IDLE_SECS`](configuration.md#auto-mode-idle-finish)
(default `300` s). A driver with `Some(...)` is excluded from the
idle-poller sweep so the same agent can't be auto-finished twice.

| Driver | `stop_hook_config` | Auto-finish path |
|---|---|---|
| `claude` | `Some(...)` | Stop hook (deterministic, no env-var required) |
| `aider`  | `None`      | Idle timer (only when `BRANCHWORK_AUTO_FINISH_IDLE=1`) |
| `codex`  | `None`      | Idle timer (only when `BRANCHWORK_AUTO_FINISH_IDLE=1`) |
| `gemini` | `None`      | Idle timer (only when `BRANCHWORK_AUTO_FINISH_IDLE=1`) |

Claude is the only `Some(...)` impl today because Claude Code is the
only shipped CLI with a settings-driven Stop hook. The other three
inherit the trait default; per-driver follow-ups for promoting them
off the idle fallback live in
`~/.claude/plans/backlog/auto-mode-stop-hook-<driver>.yaml`.

See [ADR 0003 — unattended auto-mode](../adrs/0003-unattended-auto-mode.md)
for the full design rationale: per-session settings file path, hook
URL contract, dedupe of stop-hook vs idle-timer, dirty-tree pause
semantics, and the explicit scope decision to keep this feature
standalone-only (the SaaS path is a separate backlog plan).

---

## Authoring a new driver

Five things to wire up (in this order):

1. **Implement the trait.** New struct in `driver.rs` (or its own
   submodule) implementing `AgentDriver`. Reuse [`strip_ansi`](#shared-helpers)
   for cost parsing and [`parse_status_json_verdict`](#shared-helpers)
   for verdicts unless your CLI emits a different verdict shape.
2. **Override `capabilities`.** Default is the Claude profile (all
   true). Set the bools your CLI cannot deliver — the dashboard reads
   these to gate UI.
3. **Override `auth_status`.** The default just checks the binary is
   on `PATH`. Read whatever credentials your CLI consults (env var,
   dotfile, OAuth credentials) and return the matching `AuthStatus`
   variant. Returning `Unknown` is acceptable when detection is hard;
   the UI treats it as "probably works".
4. **Register in [`DriverRegistry::with_defaults`](../../server-rs/src/agents/driver.rs).**
   Add an `Arc::new(MyDriver::new())` entry under the lookup name. The
   name becomes the value of `agents.driver` in SQLite and the value
   the dashboard sends back as `driver` on `POST /api/agents`. Lookup
   is case-sensitive.
5. **(Optional) Add an MCP config.** Override `mcp_config_json(port)`
   to return the contents of an `.mcp.json` for your CLI. The server
   writes the file next to the agent's socket and passes the path via
   `SpawnOpts.mcp_config_path`. Without this the spawned agent uses
   the prompt template's curl fallback to reach the Branchwork API.

### Test surface

`driver.rs` ships an in-file `#[cfg(test)] mod tests` block that's the
canonical regression suite for trait wiring. New drivers should mirror
the existing patterns — one test per behaviour:

| Pattern | Existing example |
|---|---|
| Spawn argv shape | [`claude_spawn_args_includes_core_flags`](../../server-rs/src/agents/driver.rs) |
| Optional argv flags appear/omit | [`claude_spawn_args_appends_budget_when_set`](../../server-rs/src/agents/driver.rs) |
| Ready signal matches/misses | [`claude_is_ready_matches_prompt_glyph`](../../server-rs/src/agents/driver.rs), [`aider_is_ready_matches_prompt_line`](../../server-rs/src/agents/driver.rs) |
| Cost regex with ANSI input | [`claude_parse_cost_matches_summary_line`](../../server-rs/src/agents/driver.rs), [`aider_parse_cost_strips_ansi`](../../server-rs/src/agents/driver.rs) |
| Verdict via shared walker | [`aider_parse_verdict_uses_shared_walker`](../../server-rs/src/agents/driver.rs) |
| Capabilities are correct | [`aider_capabilities_drop_session_id_and_mark_interactive`](../../server-rs/src/agents/driver.rs) |
| Registered in `with_defaults` | [`registry_includes_aider`](../../server-rs/src/agents/driver.rs) |

Run the suite with `cargo test --bin branchwork-server driver`.

---

## See also

- [reference/configuration.md](configuration.md) — every env var the
  driver `auth_status` probes consult, plus what the runner's
  `collect_driver_auth` covers vs. skips.
- [architecture/session-daemon.md](../architecture/session-daemon.md) —
  what happens to the spawned CLI on the supervisor side: PTY,
  reattach, replay.
- [architecture/server.md](../architecture/server.md) — the server
  modules that consume the trait (`pty_agent`, `agents/mod.rs`,
  `api/agents.rs`).
- [architecture/runner.md](../architecture/runner.md) — how the runner
  reports per-driver auth state to the dashboard.
- [`server-rs/src/agents/driver.rs`](../../server-rs/src/agents/driver.rs)
  — the single source of truth this page is derived from.
