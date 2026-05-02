# Architecture: branchwork-server

`branchwork-server` is the dashboard backend — an axum HTTP+WebSocket
server that owns the SQLite database, watches the plans directory,
spawns local agents (via the per-session supervisor), serves the
embedded SPA, and exposes an MCP server for AI clients. This page maps
each subsystem to its source file so contributors can jump straight to
the code.

For the cross-binary picture (server / session_daemon / runner) start
with [overview.md](overview.md). This page zooms into the contents of
`server-rs/src/` and assumes you have already read the overview.

## Process entry point

`server-rs/src/main.rs` is the single entry point for all three
subcommands the binary exposes:

| `cli.command`           | What runs                                                                                       |
| ----------------------- | ----------------------------------------------------------------------------------------------- |
| `Some(Command::Session)` | `agents::supervisor::run_session` — the per-agent supervisor (see [session-daemon.md](session-daemon.md)). Dispatched **before** the tokio runtime starts because `fork()` and a multi-threaded runtime do not mix. |
| `Some(Command::Mcp)`    | `mcp::transport::run_stdio` — MCP server on stdin/stdout, sharing config and DB with the dashboard. |
| `None` (default)        | `run_server` — builds `AppState`, starts background tasks, mounts the axum router, and serves on `0.0.0.0:<port>`. |

`run_server` (`main.rs:109`) is the place to start when tracing what
happens at boot: open DB → create broadcast channel → build
`AgentRegistry` → call `cleanup_and_reattach` (recovers PTY agents
that survived a server restart) → start the CI poller and the file
watcher → mount the router.

## Shared state

`server-rs/src/state.rs` defines `AppState`, the `Clone` value handed
to every axum handler. It is a bag of `Arc`-shareable references:

- `db: Db` — `Arc<Mutex<Connection>>` (see `db.rs`).
- `plans_dir: PathBuf` — usually `~/.claude/plans/`.
- `port: u16` — needed by the MCP context to advertise the right URL.
- `effort: Arc<Mutex<Effort>>` — runtime-tunable agent effort knob.
- `broadcast_tx: broadcast::Sender<String>` — the dashboard WebSocket
  fan-out (see `ws.rs`).
- `registry: AgentRegistry` — local agent registry.
- `runners: RunnerRegistry` — in-memory map of currently connected
  remote runners (SaaS only).

The MCP HTTP service does **not** share `AppState`; it gets its own
`McpContext` because `nest_service` requires a stateless inner
service. The two are kept in sync at `run_server`-time by copying the
relevant fields.

## Persistence

`server-rs/src/db.rs` is the single SQLite layer. `init(db_path)`
opens (or creates) the file at the configured path, sets
`journal_mode = WAL` and `foreign_keys = ON`, then runs `migrate()`,
which is an idempotent block of `CREATE TABLE IF NOT EXISTS`
statements followed by additive `ALTER TABLE` migrations and a
post-migration `cleanup_stale_auto_completed` sweep.

The exposed surface is small:

- `pub type Db = Arc<Mutex<Connection>>` — the handle stored on
  `AppState`.
- `init(&Path) -> Db` — open + migrate.
- `completed_task_numbers(&Connection, plan_name) -> HashSet<String>`
  — used by the dependency gate.
- `task_learnings(&Connection, plan_name, task_number) -> Vec<String>`
  — most-recent-first list of recorded learnings.

Everything else (agents, hook_events, ci_runs, audit_log, sso_*,
runner_outbox, …) is a private `migrate()` table accessed directly by
the module that owns it — there is no ORM.

There is **no Postgres backend in the Rust code**. The Helm chart
exposes a `sqlite` vs `postgres` mode switch at the deployment layer,
but the server binary itself only speaks SQLite. The schema details
and any future Postgres path are deferred to
[persistence.md](persistence.md).

## HTTP and WebSocket router

The full route table lives in `run_server` in `main.rs:206`. It is
intentionally laid out flat (no nested `Router::nest` for the API)
because the route list doubles as the API reference. Routes group
into the modules below.

### `api/` — REST handlers

| Module                             | Owns                                                                                                |
| ---------------------------------- | --------------------------------------------------------------------------------------------------- |
| `api/plans.rs`                     | `/api/plans*` — list/get/create/update plans, set project/budget/auto-advance, task status, learnings, auto-status, reset, stale-branch sweep, plan and task `Check`, `start-task`, `start-phase-tasks`. The biggest file in the tree. |
| `api/agents.rs`                    | `/api/agents*` — list, output, diff, merge, discard, kill, finish, drivers, events. Includes the empty-branch merge guard. |
| `api/ci.rs`                        | `/api/actions/fix-ci`, `/api/ci/{id}*` — Fix-CI agent spawn, CI run dismiss, failure-log fetch.    |
| `api/settings.rs`                  | `/api/settings`, `/api/folders` — global server settings and project-folder browser.                 |
| `api/billing.rs`                   | `/api/orgs/{slug}/{usage,budget,kill-switch,user-quotas}` — SaaS-only billing surface.               |
| `api/mod.rs`                       | Pure module declarations.                                                                            |

### `ws.rs` — dashboard WebSocket fan-out

`ws.rs` exposes two helpers and one handler:

- `create_broadcast()` — capacity-256 `tokio::sync::broadcast` channel
  built once at boot.
- `broadcast_event(tx, event_type, data)` — the canonical way for any
  module to push a typed event (`agent_started`, `task_status_changed`,
  `plan_updated`, `plan_checked`, …) to every connected dashboard
  client.
- `ws_handler` — the `GET /ws` endpoint. Per-client state is just a
  `broadcast::Receiver`; there is no fan-in (the dashboard has no
  business sending messages to the server over WS, and ping/pong is
  handled but otherwise ignored). The full event vocabulary is
  deferred to [protocols.md](protocols.md).

The PTY terminal WebSocket is **not** here — it lives at `GET
/terminal` and is handled by `agents::terminal_ws`, because it is
agent-scoped, not dashboard-scoped.

### `mcp/` — Model Context Protocol server

`mcp/mod.rs` is the rmcp `ServerHandler`; one handler powers two
transports mounted at boot:

- HTTP (`mcp::transport::build_http_service`) — nested at `/mcp` on
  the main listener. Used by remote MCP clients (e.g. the Inspector).
- stdio (`mcp::transport::run_stdio`) — entered via
  `branchwork-server mcp`, used by clients that spawn the server as a
  child process (Claude Code).

The handler is stateful via `mcp::McpContext` (plans dir, DB, broadcast
sender, agent registry, effort, port). Tools live in `mcp/tools/`
(`plans.rs`, `status.rs`, …). Wire-format details are deferred to
[protocols.md](protocols.md).

### `saas/` — SaaS-only multi-runner surface

`saas/mod.rs` documents itself well; relevant files:

- `runner_protocol.rs` — the `WireMessage` JSON tagged union
  exchanged with runners.
- `outbox.rs` — SQLite-backed `runner_outbox` and `inbox_pending` for
  at-least-once delivery and seq-ACK replay.
- `runner_ws.rs` — `GET /ws/runner` upgrade handler, `POST
  /api/runners/tokens`, `GET /api/runners`, `POST
  /api/runners/{id}/commands`, plus the in-memory `RunnerRegistry`
  stored on `AppState`.
- `billing.rs` — usage accounting surface used by `api/billing.rs`.

### `auth/` — authentication and organizations

`auth/mod.rs` defines the `AuthUser` extractor and the
`populate_auth_user` middleware mounted on every request. Public
routes (health, login, signup, static fallback) ignore the extension;
protected handlers opt in by taking `AuthUser` as an extractor.

- `auth/sessions.rs` — opaque session-cookie storage.
- `auth/orgs.rs` — `/api/orgs*` CRUD plus member/role management.
- `auth/sso.rs` — `/api/orgs/{slug}/sso*` admin endpoints and the
  public OIDC/SAML login flow under `/api/auth/sso/*`.

The security model (cookie flags, bcrypt rounds, token TTLs, JIT
provisioning) is deferred to a future `security.md`.

## Plan files: parser and watcher

The plan files in `plans_dir` are the source of truth for plan
structure. Two modules cover them:

- `plan_parser.rs` — `ParsedPlan`, `PlanPhase`, `PlanTask` types, the
  YAML+Markdown parsers, and serde defaults like `default_true` for
  `produces_commit`. Round-trippable: `update_plan` reserialises via
  the same types so an in-place edit preserves shape (with a few
  known holes — `verification` and `produces_commit` propagation
  through `UpdateTaskBody`, both tracked in
  [design-produces-commit.md](../design-produces-commit.md)).
- `file_watcher.rs` — `start(&plans_dir, broadcast_tx)` returns an
  RAII `Drop` handle that runs a debounced filesystem watcher over
  `*.md`, `*.yaml`, `*.yml`. Each debounced event broadcasts a
  `plan_updated` event so the SPA refetches without polling.

## Auto-status

`auto_status.rs` implements the file-existence heuristic that seeds
`task_status` rows for tasks whose work has clearly already been done.
The key entry point is `infer_status(project_dir, file_paths,
_title_words)`. Policy is deliberately conservative since the
[navbar false-completion bug](../repro-navbar-false-completion.md):

- No paths to check → `pending`.
- No files exist → `pending`.
- At least one file exists → `in_progress`.

It **never** returns `completed` — only an explicit user/agent action
sets that. Callers (`api/plans.rs::auto_status`, `sync_all`) only
write inferred rows when `task_status` has no row yet, and write them
with `source = 'auto'` so a later cleanup migration can purge them
without touching manual rows.

## Hooks

`hooks.rs` exposes `POST /hooks`, the receiver for Claude Code hook
events configured in `~/.claude/settings.json`. The handler persists
the event to `hook_events` and broadcasts a `hook_event` over the
dashboard WebSocket. Hook-payload shape is deferred to
[protocols.md](protocols.md).

## Embedded frontend

`static_files.rs` uses `rust-embed` to bake `web/dist/` into the
binary at compile time. `serve_frontend` is mounted as the axum
fallback: any path not matched by an API/WS/MCP route falls through
here, which serves the asset if it exists or `index.html` otherwise
(SPA-friendly behaviour).

## Background tasks

Two long-lived tasks are spawned from `run_server`:

- A 30-second loop that scans for `running`/`starting` agents whose
  PIDs are no longer alive and marks them `completed`. Defensive
  net for cases the supervisor heartbeat path missed.
- `ci::spawn_poller` — periodically asks `gh` for run status on rows
  in `ci_runs`, updates the row, and broadcasts changes.

## Other top-level modules

These are the remaining files under `server-rs/src/`:

| File              | Role                                                                                                                                                |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config.rs`       | `clap` definitions for `Cli`, `Command` (`Session`, `Mcp`), `SessionArgs`, the `Effort` enum, and `Config::from_cli`.                                 |
| `audit.rs`        | `audit_log` table + `record_action` helper + `GET/EXPORT /api/orgs/{slug}/audit-log`. Used by every state-changing handler in `api/` and `auth/`.   |
| `notifications.rs`| Fire-and-forget Slack-compatible webhook poster (`notify(webhook_url, text)`); no-ops when no URL is configured.                                     |
| `templates/mod.rs`| Static plan-skeleton catalogue served at `GET /api/templates` for the New Plan form.                                                                 |
| `ci.rs`           | CI integration core: post-merge push to origin, `ci_runs` schema, the background `spawn_poller`, and the `fetch_failure_log` helper used by Fix-CI. |
| `agents/`         | Local agent registry, drivers, PTY agent, supervisor (= `branchwork-server session`), check agent, terminal WebSocket. Detailed in [session-daemon.md](session-daemon.md). |
| `bin/`            | Auxiliary binaries (`branchwork_runner`, `session_daemon`). Detailed in [runner.md](runner.md) and [session-daemon.md](session-daemon.md).         |

Every top-level module under `server-rs/src/` is now either described
above or explicitly deferred to a sibling architecture page. Wire
formats (session IPC, `WireMessage`, dashboard WS event vocabulary,
hook payloads) are concentrated in [protocols.md](protocols.md);
schema and storage details are concentrated in
[persistence.md](persistence.md).
