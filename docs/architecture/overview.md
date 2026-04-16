# Architecture overview

orchestrAI ships as three cooperating binaries:

- **`orchestrai-server`** — the dashboard. Serves the SPA, the HTTP API
  (`/api/*`, `/hooks`, `/mcp`), and the WebSockets (`/ws` for dashboard
  events, `/terminal` for xterm.js, `/ws/runner` for remote runners in
  SaaS mode). Also ships the `orchestrai-server session` subcommand.
- **`session_daemon`** — one per agent. Owns a PTY, forwards bytes over a
  local IPC socket, and survives server restarts. `session_daemon` and
  `orchestrai-server session` are the same code (see
  [`supervisor.rs`](../../server-rs/src/agents/supervisor.rs)) invoked
  two different ways.
- **`orchestrai-runner`** — SaaS only. Lives on the customer's machine,
  connects outbound to the hosted dashboard over an authenticated
  WebSocket, and reuses `orchestrai-server session` as its per-agent
  supervisor.

The diagram below shows both deployment shapes on one canvas so the
runner's place in the SaaS path is easy to compare against the
self-hosted path.

## Component diagram

```mermaid
flowchart TB
  %% ── Self-hosted: one binary on one host ────────────────────────────
  subgraph SH["Self-hosted"]
    direction LR
    BrowserA["Browser<br/>dashboard SPA"]
    ServerA["orchestrai-server<br/>HTTP + WS"]
    DaemonA["session_daemon<br/>= orchestrai-server session<br/>one per agent"]
    PtyA["PTY<br/>portable_pty"]
    CliA["AI CLI<br/>claude / aider / codex / gemini"]

    BrowserA <-->|"HTTP /api/*, /hooks, /mcp<br/>WS /ws, /terminal"| ServerA
    ServerA <-->|"local socket (UDS on Unix, named pipe on Win)<br/>length-prefixed postcard frames<br/>Message: Input / Output / Resize / Kill / Ping / Pong"| DaemonA
    DaemonA --- PtyA
    PtyA --- CliA
  end

  %% ── SaaS: hosted dashboard + customer-side runner ──────────────────
  subgraph SAAS["SaaS"]
    direction LR
    BrowserB["Browser<br/>dashboard SPA"]
    ServerB["orchestrai-server<br/>hosted (multi-tenant)"]
    RunnerB["orchestrai-runner<br/>customer machine"]
    DaemonB["session_daemon<br/>= orchestrai-server session"]
    PtyB["PTY"]
    CliB["AI CLI<br/>claude / aider / codex / gemini"]

    BrowserB <-->|"HTTPS /api/*<br/>WSS /ws"| ServerB
    ServerB <-->|"WSS /ws/runner?token=...<br/>JSON WireMessage tagged union<br/>SQLite outbox, at-least-once + seq-ACK replay<br/>(runner_outbox / inbox_pending)"| RunnerB
    RunnerB <-->|"local socket + postcard frames<br/>(same protocol as self-hosted)"| DaemonB
    DaemonB --- PtyB
    PtyB --- CliB
  end

  %% ── Filesystem touchpoints (shared shapes) ─────────────────────────
  Plans[("~/.claude/plans/*.yaml<br/>plan source of truth")]
  Logs[("~/.claude/sessions/agent.sock<br/>+ .log (PTY transcript) + .pid")]
  Git[("project git worktree<br/>task branches: orchestrai/plan/task<br/>fix branches: orchestrai/fix/...")]

  ServerA -. "notify watcher + CRUD" .-> Plans
  DaemonA -. "appends PTY bytes" .-> Logs
  ServerA -. "checkout / commit / merge" .-> Git

  RunnerB -. "CRUD via MCP (tunneled)" .-> Plans
  DaemonB -. "appends PTY bytes" .-> Logs
  RunnerB -. "checkout / commit / merge" .-> Git
```

## Legend

| Line | Meaning |
|------|---------|
| Solid arrow `<-->` | Live bidirectional channel (HTTP, WebSocket, or local socket). |
| Solid line `---` | In-process handoff (file descriptor, spawn). |
| Dashed arrow `-.->` | Filesystem read or write (not a live connection). |

## Key invariants the diagram encodes

- **One protocol, two transports.** The session IPC frame format
  (4-byte big-endian length + postcard-encoded
  [`Message`](../../server-rs/src/agents/session_protocol.rs) payload,
  capped at 8 MiB) is identical in both deployments. Only the hop that
  reaches it differs: the dashboard server talks to the daemon directly
  in self-hosted mode, whereas in SaaS the runner is the client.
- **Daemons outlive the server.** The `session_daemon` fork+setsids
  itself on Unix (or is spawned with `DETACHED_PROCESS` on Windows) so
  agent sessions survive a server restart and are reattached from the
  `<socket>.log` transcript.
- **Plans are files, not rows.** Every dashboard reads and writes
  `~/.claude/plans/*.yaml` as the source of truth; SQLite stores
  runtime state (agents, task status, cost, outbox) but not the plan
  definition itself.
- **Task work is a git branch.** Agents run on a dedicated branch
  (`orchestrai/<plan>/<task>`), and the merge button is gated on the
  branch having commits — nothing is persisted through the dashboard
  alone.
- **SaaS adds a WebSocket hop, not a new protocol.** The
  `orchestrai-runner` speaks a JSON
  [`WireMessage`](../../server-rs/src/saas/runner_protocol.rs) envelope
  upstream; downstream it reuses `orchestrai-server session` verbatim.

## See also

- [architecture/server.md](server.md) _(stub)_ — dashboard internals.
- [architecture/session-daemon.md](session-daemon.md) _(stub)_ — PTY
  and reattach details.
- [architecture/runner.md](runner.md) _(stub)_ — runner lifecycle,
  outbox, reconnect.
- [architecture/protocols.md](protocols.md) _(stub)_ — frame formats
  and WS event vocabulary.
- [architecture/persistence.md](persistence.md) _(stub)_ — SQLite /
  Postgres schema and what survives restart.
