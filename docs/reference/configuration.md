# Configuration reference

This page covers the **non-flag** configuration surface: where Branchwork
stores its state on disk, every environment variable the code actually
reads, and what the runtime `/api/settings` endpoint can change. For the
flag-level reference (every `clap`-derived option on every binary) see
[reference/cli.md](cli.md).

The tables below are derived directly from the source — every row cites
the file and line that consumes it. Anything not listed here is not
read by Branchwork, even if the surrounding ecosystem (Claude Code, a
Helm chart, an init system) sets it.

---

## Filesystem layout

### Server (`branchwork-server`)

The server's state root is `--claude-dir`, defaulting to `~/.claude/`
([`server-rs/src/config.rs`](../../server-rs/src/config.rs)). On Unix
`~` is resolved via `dirs::home_dir()` (the `HOME` env var); on Windows
it resolves to `USERPROFILE`. Branchwork creates the root if missing.

Three things live under it. Everything else you may see in `~/.claude/`
(e.g. `CLAUDE.md`, `.credentials.json`, `projects/`, `history.jsonl`,
`hooks/`, `cache/`, …) is **owned by Claude Code itself**, not by
Branchwork — Branchwork only reads `.credentials.json` to detect that
the user has completed Claude Code OAuth (see
[`agents/driver.rs:332`](../../server-rs/src/agents/driver.rs)).

| Path | Created by | Contents | Lifetime |
|---|---|---|---|
| `<claude-dir>/branchwork.db` | [`db::init`](../../server-rs/src/db.rs) | SQLite database. WAL mode is enabled at boot, so the sibling `branchwork.db-wal` and `branchwork.db-shm` files appear automatically. Schema details: [architecture/persistence.md](../architecture/persistence.md). | Persistent. Migrations are idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ADD COLUMN`. |
| `<claude-dir>/plans/` | [`file_watcher::start`](../../server-rs/src/file_watcher.rs), then `POST /api/plans` | One YAML or Markdown file per plan (`<name>.{yaml,yml,md}`). The file watcher monitors this directory non-recursively and broadcasts `plan_updated` on changes. | Persistent. Authoritative source of plan structure (DB only stores per-task status). |
| `<claude-dir>/sessions/` | `branchwork-server` boot ([`main.rs:117`](../../server-rs/src/main.rs)) | Per-agent supervisor sibling files (see below). | Created on every boot. Files inside are tied to agent lifetime. |

Inside `sessions/`, every running or recently-running agent has up to
four sibling files keyed by its agent ID
([`agents/mod.rs:274–284`](../../server-rs/src/agents/mod.rs)):

| Sibling file | Owner | Purpose |
|---|---|---|
| `<id>.sock` | session daemon | Unix domain socket the daemon listens on. On Windows the file is companion-only — the actual named pipe is `\\.\pipe\<stem>` derived from the socket file's stem ([`agents/supervisor.rs:40`](../../server-rs/src/agents/supervisor.rs)). |
| `<id>.pid` | session daemon | Daemon's own PID. Removed on clean exit; presence after the process is gone is the canonical "supervisor crashed" signal ([`agents/pty_agent.rs` heartbeat path](../../server-rs/src/agents/pty_agent.rs)). |
| `<id>.log` | session daemon | Authoritative on-disk PTY transcript. Captures bytes emitted while the dashboard server was offline. Hint banner on `/terminal` flags any gap. |
| `<id>.mcp.json` | dashboard server (for `claude` driver only) | Auto-injected MCP server config registering the dashboard's `/mcp` endpoint with the spawned Claude Code agent ([`agents/driver.rs:280`](../../server-rs/src/agents/driver.rs)). |

Sibling lifetimes and the reattach protocol are documented in
[architecture/session-daemon.md](../architecture/session-daemon.md).

### Runner (`branchwork-runner`)

The SaaS runner has its own state, completely separate from
`~/.claude/`:

| Path | Default | Created by | Contents |
|---|---|---|---|
| `--db-path` | `~/.branchwork-runner/runner.db` ([`bin/branchwork_runner.rs:117`](../../server-rs/src/bin/branchwork_runner.rs)) | runner boot | SQLite outbox (`runner_outbox`), per-peer ACK cursor (`seq_tracker`), and the persisted runner ID. WAL mode enabled. Deleting this file forks the runner's identity. |
| `<--cwd>/.branchwork-runner-sessions/` | derived from `--cwd` ([`bin/branchwork_runner.rs:510`](../../server-rs/src/bin/branchwork_runner.rs)) | runner boot | Per-agent supervisor sockets the runner spawns. Same four-file scheme as the server's `sessions/` directory above — the runner shells out to `branchwork-server session` so the on-disk shape is identical. |

The runner does **not** write to `~/.claude/`. Everything stays under
the runner's own home (DB) and `--cwd` (per-agent sockets).

---

## Environment variables

Every variable below is read by some path in the source. The **Source**
column cites the read site directly; if a variable you expected to find
isn't here, the server is not consuming it (see
[Variables that look like config but aren't](#variables-that-look-like-config-but-arent)
at the bottom of this page).

### Branchwork-specific variables

These are the variables wired into a `clap` `env =` attribute so they
double as command-line flags. They are surfaced here because environment
configuration is often handled separately from CLI arguments (e.g.
systemd unit `Environment=`, Docker `-e`, Helm `env:`); see
[reference/cli.md](cli.md) for the equivalent flags.

| Variable | Binary | Default | Source | Description |
|---|---|---|---|---|
| `BRANCHWORK_WEBHOOK_URL` | `branchwork-server` | unset | [`config.rs:61`](../../server-rs/src/config.rs) | Webhook URL for agent-completion / phase-advance events. Slack incoming webhooks (`{"text": "..."}`) and any JSON-accepting endpoint both work. Empty / whitespace-only values are treated as unset ([`config.rs:107`](../../server-rs/src/config.rs)). |
| `BRANCHWORK_SAAS_URL` | `branchwork-runner` | _(required)_ | [`bin/branchwork_runner.rs:49`](../../server-rs/src/bin/branchwork_runner.rs) | Dashboard base URL the runner connects to. `https://` / `http://` are auto-rewritten to `wss://` / `ws://`; the runner appends `/ws/runner?token=…` itself. |
| `BRANCHWORK_RUNNER_TOKEN` | `branchwork-runner` | _(required)_ | [`bin/branchwork_runner.rs:53`](../../server-rs/src/bin/branchwork_runner.rs) | Runner API token issued by `POST /api/orgs/:slug/runner-tokens`. Sent as a query-string parameter on the WebSocket upgrade. |
| `BRANCHWORK_RUNNER_ID` | `branchwork-runner` | auto-generated `runner-<uuid>` and persisted in `seq_tracker` | [`bin/branchwork_runner.rs:61`](../../server-rs/src/bin/branchwork_runner.rs) | Stable identifier the dashboard uses to address the runner. Override only when forking or merging identities. |

### SMTP (budget-alert email)

These are read **only** when a SaaS dashboard sends a budget-alert
email; self-hosted deployments and runners never touch them.
`SMTP_HOST` is the gating variable — if it is unset, `SmtpConfig::from_env`
returns `None` and email is disabled entirely.

| Variable | Default | Source | Description |
|---|---|---|---|
| `SMTP_HOST` | _(disables email)_ | [`saas/billing.rs:377`](../../server-rs/src/saas/billing.rs) | Hostname of the relay (e.g. `smtp.sendgrid.net`). |
| `SMTP_PORT` | `587` | [`saas/billing.rs:380`](../../server-rs/src/saas/billing.rs) | TCP port. Parsed as `u16`; non-numeric values fall back to the default. |
| `SMTP_FROM` | `branchwork@localhost` | [`saas/billing.rs:384`](../../server-rs/src/saas/billing.rs) | `From:` address on outgoing alerts. Invalid addresses fall back to `branchwork@localhost`. |
| `SMTP_USERNAME` | unset | [`saas/billing.rs:385`](../../server-rs/src/saas/billing.rs) | Optional. Used together with `SMTP_PASSWORD` for SMTP AUTH. If only one of the two is set, authentication is skipped. |
| `SMTP_PASSWORD` | unset | [`saas/billing.rs:386`](../../server-rs/src/saas/billing.rs) | Optional, paired with `SMTP_USERNAME`. |

### Driver authentication

These are the API-key variables Branchwork **probes** when reporting
which drivers are usable on the host
([`agents/driver.rs::auth_status`](../../server-rs/src/agents/driver.rs)
for the server, [`bin/branchwork_runner.rs::collect_driver_auth`](../../server-rs/src/bin/branchwork_runner.rs)
for the runner). Branchwork itself does not call any of these APIs — it
just checks whether the variable is set so the dashboard can show a
"driver is authenticated" badge. The spawned agent CLI (`claude`,
`aider`, `codex`, `gemini`) inherits Branchwork's environment, so the
underlying CLI is what actually authenticates with the variable.

| Variable | Drivers that probe it | Source | Effect on `auth_status` |
|---|---|---|---|
| `ANTHROPIC_API_KEY` | `claude`, `aider` | [`driver.rs:306`](../../server-rs/src/agents/driver.rs), [`:459`](../../server-rs/src/agents/driver.rs) | `AuthStatus::ApiKey` when set non-empty. Short-circuits the OAuth `.credentials.json` check for the `claude` driver. |
| `CLAUDE_CODE_USE_BEDROCK` | `claude` | [`driver.rs:315`](../../server-rs/src/agents/driver.rs) | `AuthStatus::CloudProvider { provider: "bedrock" }`. Presence-only check — the value is not parsed. |
| `CLAUDE_CODE_USE_VERTEX` | `claude` | [`driver.rs:320`](../../server-rs/src/agents/driver.rs) | `AuthStatus::CloudProvider { provider: "vertex" }`. Presence-only. |
| `OPENAI_API_KEY` | `aider`, `codex` | [`driver.rs:458`](../../server-rs/src/agents/driver.rs), [`:539`](../../server-rs/src/agents/driver.rs) | `AuthStatus::ApiKey` for either driver. |
| `GEMINI_API_KEY` | `aider`, `gemini` | [`driver.rs:460`](../../server-rs/src/agents/driver.rs), [`:600`](../../server-rs/src/agents/driver.rs) | `AuthStatus::ApiKey`. |
| `GOOGLE_API_KEY` | `gemini` | [`driver.rs:603`](../../server-rs/src/agents/driver.rs) | `AuthStatus::ApiKey` (alternative to `GEMINI_API_KEY`). |
| `DEEPSEEK_API_KEY` | `aider` | [`driver.rs:461`](../../server-rs/src/agents/driver.rs) | `AuthStatus::ApiKey`. Aider only — `aider` accepts any of the four keys it checks. |

The runner's `collect_driver_auth` repeats this probe set so the
dashboard can show authentication state for the runner host as well.
The runner side covers `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, and `GOOGLE_API_KEY`
([`bin/branchwork_runner.rs:770–778`](../../server-rs/src/bin/branchwork_runner.rs));
it does not probe `CLAUDE_CODE_USE_BEDROCK`/`VERTEX` or `DEEPSEEK_API_KEY`.

### System variables

Standard system variables Branchwork relies on indirectly:

| Variable | Read by | Purpose |
|---|---|---|
| `PATH` | [`agents/driver.rs:197`](../../server-rs/src/agents/driver.rs) (`binary_on_path`), [`bin/branchwork_runner.rs:712`](../../server-rs/src/bin/branchwork_runner.rs) (`which`) | Used to locate driver binaries (`claude`, `aider`, `codex`, `gemini`) and, on the runner, to find the `branchwork-server` executable for `--server-bin`. The runner falls back to the literal name `branchwork-server` if it is not on `PATH` (which fails at spawn time). |
| `HOSTNAME` | [`bin/branchwork_runner.rs:694`](../../server-rs/src/bin/branchwork_runner.rs) | Reported as `RunnerHello.hostname` on the WebSocket upgrade so the dashboard can label runners. |
| `COMPUTERNAME` | [`bin/branchwork_runner.rs:695`](../../server-rs/src/bin/branchwork_runner.rs) | Windows fallback for `HOSTNAME`. If neither is set, the runner falls back to `libc::gethostname` on Unix and the literal string `"unknown"` elsewhere. |
| `HOME` (Unix) / `USERPROFILE` (Windows) | `dirs::home_dir()` (transitively, in [`config.rs:83`](../../server-rs/src/config.rs), [`bin/branchwork_runner.rs:117`](../../server-rs/src/bin/branchwork_runner.rs), and [`agents/driver.rs:331`](../../server-rs/src/agents/driver.rs)) | Resolves the default `--claude-dir`, the runner's default `--db-path`, and the path to Claude Code's `.credentials.json`. Override the dependent paths via flags rather than mutating these variables. |

---

## Runtime settings

Settings the dashboard can mutate at runtime via `PUT /api/settings`
([`api/settings.rs`](../../server-rs/src/api/settings.rs)):

| Setting | Type | Source of truth | Survives restart? |
|---|---|---|---|
| `effort` | `low \| medium \| high \| max` | `state.effort` (an `Arc<Mutex<Effort>>`) | **No** — settings live in process memory only. The boot value comes from `--effort` (default `high`). To change the effort permanently, restart the server with a different `--effort`. |

There is no other persisted-in-the-DB settings table; per-org budgets
live in the SaaS `org_budgets` schema and are managed through the
billing API rather than this endpoint.

---

## Variables that look like config but aren't

The plan brief that drove this page mentioned several variables that
seem like they ought to exist. They do not — none of them is read
anywhere in `server-rs/src/`. If you came here looking for one, this is
the explanation:

| Looks like… | Reality |
|---|---|
| `DATABASE_URL` / `POSTGRES_URL` | The Helm chart [`deploy/helm/branchwork/templates/deployment.yaml:69–79`](../../deploy/helm/branchwork/templates/deployment.yaml) sets `DATABASE_URL` when `database.mode=postgres`, but the Rust binary does not read it. `db::init` is SQLite-only ([`db.rs:46`](../../server-rs/src/db.rs)); the chart's `postgres` mode is currently a placeholder. See the [persistence doc](../architecture/persistence.md) for the full story. |
| `JWT_SECRET` | Branchwork does not issue JWTs. The `jsonwebtoken` crate is used only to **validate** OIDC ID tokens during SSO sign-in, against keys fetched from the IdP's JWKS endpoint ([`auth/sso.rs:486`](../../server-rs/src/auth/sso.rs)). |
| `OAUTH_CLIENT_ID` / `OAUTH_CLIENT_SECRET` | SSO credentials are stored **per organisation** in the SQLite `org_sso_config` table ([`auth/sso.rs:276`](../../server-rs/src/auth/sso.rs)) and managed through the SSO admin API, not via env vars. |
| `AUTH_COOKIE_SECRET` / `SESSION_SECRET` | Session tokens are random bytes generated at sign-in and stored in the `auth_sessions` table; there is no signing key to configure. |
| `branchwork.toml` / `~/.branchworkrc` | Branchwork has no TOML or rc-file configuration layer. All knobs are CLI flags or environment variables, both of which are documented in this section and in [reference/cli.md](cli.md). |

If you find a variable in the source that this page does not list, the
acceptance criterion for this doc has been violated — please open an
issue or amend the table directly.

---

## See also

- [reference/cli.md](cli.md) — flag-level reference for all three
  binaries (the `clap`-derived counterpart of the `BRANCHWORK_*`
  section above).
- [architecture/persistence.md](../architecture/persistence.md) —
  full SQLite schema, migrations, and what survives a restart.
- [architecture/session-daemon.md](../architecture/session-daemon.md) —
  lifecycle of the four sibling files inside `<claude-dir>/sessions/`.
- [architecture/runner.md](../architecture/runner.md) — runner DB,
  outbox, and the `<cwd>/.branchwork-runner-sessions/` directory.
