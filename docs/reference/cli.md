# CLI reference

Branchwork ships three executables:

| Binary | Source | Role |
|---|---|---|
| `branchwork-server` | [`server-rs/src/main.rs`](../../server-rs/src/main.rs) + [`config.rs`](../../server-rs/src/config.rs) | Dashboard server. Exposes the `session` and `mcp` subcommands. |
| `session_daemon` | [`server-rs/src/bin/session_daemon.rs`](../../server-rs/src/bin/session_daemon.rs) | Standalone equivalent of `branchwork-server session`. |
| `branchwork-runner` | [`server-rs/src/bin/branchwork_runner.rs`](../../server-rs/src/bin/branchwork_runner.rs) | SaaS-only runner that connects to a remote dashboard and spawns agents locally. |

The tables below are derived directly from the `clap` derive attributes on
each binary's `Cli` / `SessionArgs` struct — if a flag is here it is
parsed by the binary, and conversely if the source adds a flag without
this page being updated the acceptance test for task 3.1 fails. For the
binary split itself see [architecture/overview.md](../architecture/overview.md);
for the protocols the runner and session daemon speak see
[architecture/protocols.md](../architecture/protocols.md).

A dash (`—`) in the **Env** column means there is no environment-variable
fallback; the flag must be passed on the command line (or left at its
default if it has one).

---

## `branchwork-server`

The main dashboard server. Run with no subcommand to start the HTTP +
WebSocket listener; pass a subcommand to dispatch into one of the
embedded helpers.

> The `clap` `name` attribute is `branchwork`, so `--help` output and
> error messages refer to the program as `branchwork`. The installed
> executable is still `branchwork-server` (per
> [`server-rs/Cargo.toml`](../../server-rs/Cargo.toml)).

### Synopsis

```
branchwork-server [OPTIONS]
branchwork-server [OPTIONS] mcp
branchwork-server session --socket <PATH> --cwd <PATH> [--cols <N>] [--rows <N>] -- <CMD> [ARGS...]
```

`[OPTIONS]` are the top-level flags below. They are **not** marked
`global` in clap, so when running a subcommand they must appear *before*
the subcommand name (e.g. `branchwork-server --port 3200 mcp`, not
`branchwork-server mcp --port 3200`). `session` ignores the top-level
flags — it only reads its own `SessionArgs`.

### Top-level flags

Defined on `Cli` in [`server-rs/src/config.rs`](../../server-rs/src/config.rs).

| Flag | Default | Env | Description |
|---|---|---|---|
| `--port <PORT>` | `3100` | — | TCP port for the HTTP + WebSocket listener (binds `0.0.0.0`). |
| `--effort <low\|medium\|high\|max>` | `high` | — | Effort level passed to spawned Claude agents. Mutable at runtime via the dashboard `/api/settings` endpoint; this flag only sets the boot value. |
| `--claude-dir <PATH>` | `~/.claude` (resolved from `dirs::home_dir()`) | — | Root of Branchwork's per-user state. The server derives `<dir>/plans` (plan YAMLs), `<dir>/branchwork.db` (SQLite), and `<dir>/sessions/` (per-agent UDS / named-pipe + log files) from it. |
| `--webhook-url <URL>` | none | `BRANCHWORK_WEBHOOK_URL` | Optional webhook for agent-completion / phase-advance notifications. Accepts Slack incoming webhooks (posts `{"text": "..."}`) or any JSON-accepting endpoint. Empty / whitespace-only values are treated as unset. |

### `branchwork-server session` subcommand

Run as a detached **per-agent supervisor daemon** owning one PTY plus a
local IPC socket. Normally invoked by the server itself when starting an
agent; expose it directly only when debugging or writing alternate
hosts. On Unix the supervisor `fork()`s + `setsid()`s; on Windows the
parent must spawn it with `CREATE_NO_WINDOW | DETACHED_PROCESS` (no
in-process detach). See [architecture/session-daemon.md](../architecture/session-daemon.md)
for the full lifecycle.

Defined on `SessionArgs` in
[`server-rs/src/agents/supervisor.rs`](../../server-rs/src/agents/supervisor.rs).

| Flag | Default | Env | Description |
|---|---|---|---|
| `--socket <PATH>` | _(required)_ | — | Path the daemon listens on. On Unix this is the UDS file. On Windows the named-pipe name is derived from the file stem (`\\.\pipe\<stem>`) while the on-disk path is reused for the transcript log (`<path>.log`). |
| `--cwd <PATH>` | _(required)_ | — | Working directory the PTY command is spawned in. |
| `--cols <COLS>` | `120` | — | Initial PTY column count. The first connected client may immediately resize via a `Resize` frame. |
| `--rows <ROWS>` | `40` | — | Initial PTY row count. |
| `-- <CMD> [ARGS...]` | _(required, ≥ 1)_ | — | The driver command line, after the `--` separator. `trailing_var_arg` + `allow_hyphen_values` are set, so any flags belonging to the driver pass through unchanged. |

### `branchwork-server mcp` subcommand

Serve the Branchwork MCP server over **stdio** for MCP clients that
prefer to spawn the server as a child process and exchange JSON-RPC on
stdin/stdout. The same MCP handler is also mounted at `/mcp` on the
HTTP listener when running without a subcommand, so this mode is purely
for stdio transport.

The `mcp` subcommand reads the top-level `--port`, `--effort`,
`--claude-dir`, and `--webhook-url` flags so it shares the same
SQLite DB and plans directory as a co-running HTTP server.

### Examples

```bash
# Default self-hosted server on :3100, ~/.claude state.
branchwork-server

# Custom port, lower effort, alternate state dir, Slack webhook.
BRANCHWORK_WEBHOOK_URL=https://hooks.slack.com/services/T.../B.../xxx \
  branchwork-server --port 3200 --effort medium --claude-dir /var/lib/branchwork

# MCP over stdio (e.g. registered in a Claude Code .mcp.json).
branchwork-server --claude-dir /var/lib/branchwork mcp

# Manually run the per-agent supervisor (rare — usually spawned by the server).
branchwork-server session \
  --socket /tmp/agent-debug.sock \
  --cwd /home/me/project \
  --cols 160 --rows 48 \
  -- claude --effort high
```

---

## `session_daemon`

Standalone build of the per-agent supervisor. Equivalent in every
respect to `branchwork-server session …` — both binaries dispatch into
[`supervisor::run_session`](../../server-rs/src/agents/supervisor.rs).
The standalone binary exists so tests and alternate callers can invoke
the daemon without depending on the main server binary's subcommand
layout.

### Synopsis

```
session_daemon --socket <PATH> --cwd <PATH> [--cols <N>] [--rows <N>] -- <CMD> [ARGS...]
```

### Flags

`Cli` in [`server-rs/src/bin/session_daemon.rs`](../../server-rs/src/bin/session_daemon.rs)
flattens `SessionArgs` into the top level via `#[command(flatten)]`, so
the flag set is **identical** to `branchwork-server session` above:

| Flag | Default | Env | Description |
|---|---|---|---|
| `--socket <PATH>` | _(required)_ | — | UDS / named-pipe path. |
| `--cwd <PATH>` | _(required)_ | — | Working directory for the PTY command. |
| `--cols <COLS>` | `120` | — | Initial PTY columns. |
| `--rows <ROWS>` | `40` | — | Initial PTY rows. |
| `-- <CMD> [ARGS...]` | _(required, ≥ 1)_ | — | Driver command line. |

### Example

```bash
session_daemon \
  --socket /tmp/agent-7f3.sock \
  --cwd /home/me/project \
  -- aider --no-auto-commits
```

---

## `branchwork-runner`

SaaS-only runner. Keeps a single authenticated WebSocket open to a
remote Branchwork dashboard and spawns agents locally on the customer's
machine. The runner re-uses `branchwork-server session` as its
per-agent supervisor, so a `branchwork-server` binary must be reachable
on `$PATH` (or supplied via `--server-bin`). For the full lifecycle and
outbox semantics see [architecture/runner.md](../architecture/runner.md).

### Synopsis

```
branchwork-runner --saas-url <URL> --token <TOKEN> [OPTIONS]
```

### Flags

Defined on `Cli` in
[`server-rs/src/bin/branchwork_runner.rs`](../../server-rs/src/bin/branchwork_runner.rs).

| Flag | Default | Env | Description |
|---|---|---|---|
| `--saas-url <URL>` | _(required)_ | `BRANCHWORK_SAAS_URL` | Dashboard base URL. Both `wss://app.branchwork.dev` and `ws://localhost:3100` are accepted; `https://` / `http://` are auto-rewritten to `wss://` / `ws://`. The runner appends `/ws/runner?token=<TOKEN>` itself. |
| `--token <TOKEN>` | _(required)_ | `BRANCHWORK_RUNNER_TOKEN` | API token issued by the dashboard's runner-management endpoint (`POST /api/orgs/:slug/runner-tokens`). Sent as a query-string param on the WebSocket upgrade. |
| `--cwd <PATH>` | `.` (canonicalised at startup) | — | Default working directory for spawned agents. The dashboard may override per agent; if it sends an empty `cwd` the runner falls back to this value. |
| `--runner-id <ID>` | auto-generated `runner-<uuid>` and persisted in the local `seq_tracker` table; subsequent starts reuse it | `BRANCHWORK_RUNNER_ID` | Stable identifier the dashboard uses to address this runner. Override only when you want to fork or merge identities — the persisted value is keyed off the local DB, so deleting `--db-path` is the supported way to forget the auto-generated ID. |
| `--db-path <PATH>` | `~/.branchwork-runner/runner.db` (parent dir auto-created) | — | Local SQLite DB holding the outbox (`runner_outbox`), per-peer ACK cursors (`seq_tracker`), and the persisted runner ID. WAL mode is enabled at startup. |
| `--server-bin <PATH>` | first `branchwork-server` found on `$PATH` (else literal `branchwork-server`, which will fail at spawn time if absent) | — | Path to the `branchwork-server` executable used as the per-agent supervisor (`branchwork-server session …`). Override when running a runner alongside a build of the server that is not on the runner's `$PATH`. |

The runner has no `--port` of its own — it is an outbound-only client.
There is also no `--effort` flag; effort is selected per-agent by the
dashboard and forwarded inside `WireMessage::StartAgent` (`effort`
field), which the runner appends to the spawned `claude` command line.

### Example

```bash
# Typical SaaS install: secrets via env, everything else inferred.
export BRANCHWORK_SAAS_URL=wss://app.branchwork.dev
export BRANCHWORK_RUNNER_TOKEN=bwr_live_xxxxxxxxxxxxxxxxxxxx
branchwork-runner --cwd /home/me/projects/api

# Fully explicit, useful for systemd units.
branchwork-runner \
  --saas-url   wss://app.branchwork.dev \
  --token      bwr_live_xxxxxxxxxxxxxxxxxxxx \
  --cwd        /var/lib/branchwork-runner/work \
  --db-path    /var/lib/branchwork-runner/runner.db \
  --server-bin /usr/local/bin/branchwork-server
```

---

## See also

- [reference/configuration.md](configuration.md) — non-flag
  configuration: `~/.claude/` layout, SMTP env vars (budget alerts),
  driver API-key probes, and the variables that look like config but
  aren't (`DATABASE_URL`, `JWT_SECRET`, `branchwork.toml`).
- [architecture/overview.md](../architecture/overview.md) — which
  binary runs where, and how they cooperate.
- [architecture/session-daemon.md](../architecture/session-daemon.md) —
  what the supervisor does once `branchwork-server session` /
  `session_daemon` is running.
- [architecture/runner.md](../architecture/runner.md) — runner
  reconnect, outbox, and identity-persistence details.
