# Architecture: runner

The runner (`branchwork-runner`) is the third Branchwork binary and exists
**only in SaaS mode**. It is a small Rust process the customer installs on
their own machine (laptop, beefy build box, CI worker) which keeps a
single authenticated WebSocket open to the hosted dashboard at
`wss://app.branchwork.dev/ws/runner`. The dashboard sends commands
("spawn this agent on plan X, task Y"); the runner spawns the agent
locally — using the same supervisor daemon as self-hosted mode — and
streams events back. Local SQLite outboxes on both ends provide
**at-least-once delivery** for reliable events across reconnects.

This page is the focused reference for that one binary. For the
three-binary picture start with [overview.md](overview.md). The session
daemon the runner reuses as its supervisor is documented at
[session-daemon.md](session-daemon.md). The wire protocol is summarised
inline below; the canonical reference is `WireMessage` in
[`server-rs/src/saas/runner_protocol.rs`](../../server-rs/src/saas/runner_protocol.rs).

## Where the runner fits

```
┌────────────────────────┐         WSS (authenticated)          ┌──────────────────────┐
│  Branchwork SaaS       │◄────────────────────────────────────►│   branchwork-runner  │
│  (dashboard server)    │   /ws/runner?token=<api-token>       │   (customer machine) │
│  inbox_pending table   │   JSON Envelope frames               │   runner_outbox      │
│  per-runner ACK gate   │                                      │   seq_tracker        │
└────────────┬───────────┘                                      └──────────┬───────────┘
             │ broadcast_event                                              │ spawns
             ▼                                                              ▼
       browser dashboard                                       branchwork-server session
                                                               (per-agent supervisor)
                                                                          │
                                                                          ▼
                                                                    PTY + driver
                                                                    (claude / aider / ...)
```

The runner is the only piece of customer infrastructure SaaS deployments
need. The driver CLI itself, the source tree, the agent's API key, and
the resulting commits all stay on the customer side; the dashboard sees
only the events the runner chooses to forward.

## Process lifecycle

Single binary, single tokio runtime, one outer reconnect loop:

```
main()
  └── run(cli)                           # outer loop, lives forever
        │
        ├── canonicalise --cwd
        ├── open ~/.branchwork-runner/runner.db (SQLite, WAL)
        ├── init_runner_outbox + init_seq_tracker
        ├── load_or_generate_runner_id   # persisted in seq_tracker table
        │
        └── loop {
              connect_and_run(...)        # one WS lifetime
              sleep(jittered backoff)     # 1s → 2s → 4s … cap 30s, ±25 % jitter
            }
```

Inside one connection (`connect_and_run`):

```
tokio_tungstenite::connect_async(ws_url)
   ↓
spawn ws_writer  (drains channel → WS sink)
spawn heartbeat  (Ping every 15s)
ws_reader        (consumes WS stream until EOF)
   ↓
on disconnect: abort writer + heartbeat, return to outer loop
```

The reconnect loop never exits on errors — only a panic in `main` or a
`SIGTERM` brings the runner down. There is no internal supervision tree
for spawned agents: each `AgentHandle` owns a tokio task that proxies
session-daemon I/O to the WebSocket; if the WebSocket drops the I/O task
keeps running and continues writing into the outbound channel, which
will be drained by the *next* writer task once a new connection comes
up. Session daemons themselves are unaffected by runner reconnects
because they are detached and own their own PTY (see
[session-daemon.md](session-daemon.md)).

## Authenticated WebSocket handshake

The runner authenticates with a single API token at connect time. There
is no challenge/response, no per-message signing, and no token rotation
during a session — token theft means session theft until the token is
revoked in the dashboard.

1. **Token issuance.** A logged-in dashboard user calls
   `POST /api/runners/tokens` with `{"runner_name": "my-laptop"}`. The
   server generates a 256-bit random hex string, stores it in the
   `runner_tokens` table (currently unhashed; high-entropy random tokens
   are not bcrypt-worthy — see the comment on `sha256_hex` in
   `runner_ws.rs`) scoped to the caller's `org_id`, and returns the
   plaintext token **once**. The dashboard surfaces it in a copy-once
   modal.
2. **Connection URL.** The runner is invoked with
   `--saas-url wss://… --token <hex>` (or the matching env vars
   `BRANCHWORK_SAAS_URL` / `BRANCHWORK_RUNNER_TOKEN`). `build_ws_url`
   normalises the scheme (`http→ws`, `https→wss`), trims trailing
   slashes, and appends `/ws/runner?token=…`.
3. **Server-side validation.** `runner_ws_handler` looks the token up in
   `runner_tokens`. Hit → upgrade to WS and dispatch to
   `handle_runner_ws(socket, runner_name, org_id)`. Miss → `401
   invalid_token` and the upgrade is refused.
4. **Identification — second step.** The runner does **not** put its
   `runner_id` in the URL; the server learns it from the first
   `Envelope` it receives (every envelope carries `runner_id`). On that
   first frame, the server `INSERT … ON CONFLICT DO UPDATE` into the
   `runners` table (status `online`, refresh `last_seen_at`), registers
   the `command_tx` channel in the in-memory `RunnerRegistry`, and
   broadcasts `runner_connected` to dashboard browsers.
5. **Resume exchange.** Both sides immediately send a
   `Resume { last_seen_seq }` so the peer's outbox can flush anything
   that was unACKed at last disconnect. See [Outbox &
   replay](#outbox-and-replay-on-reconnect) below.

If the WS upgrade succeeds but the runner never sends a parseable
envelope, no `runner_id` is ever registered and the disconnect cleanup
path (which keys off `runner_id_write`) is a no-op — the only trace is
a stderr log line on the server.

## Runner ID persistence

The `runner_id` is the demultiplexing key on the SaaS side: it is what
ties an `inbox_pending` row to a specific runner, and what lets a runner
reconnect (possibly with a fresh WebSocket from a different IP) and pick
up where it left off. It must therefore survive process restart.

Persistence is intentionally low-tech: the runner stores the ID inside
its own `seq_tracker` table (the same table the outbox uses for peer
sequence tracking — reusing it avoids a second schema migration story).
On startup `load_or_generate_runner_id` runs

```sql
SELECT peer_id FROM seq_tracker WHERE peer_id LIKE 'runner-%' LIMIT 1
```

Hit → reuse it. Miss → generate `runner-{uuidv4}` and `INSERT OR
IGNORE`. The user can override the auto-generated value with
`--runner-id` or `BRANCHWORK_RUNNER_ID` (useful for fleets that want
hostnames or k8s pod names as IDs), in which case the DB lookup is
skipped entirely.

> **Gotcha.** If you delete `~/.branchwork-runner/runner.db` you also
> reset the `runner_id`. The server will see a fresh runner appear
> alongside the old one (now permanently `offline`); the
> `inbox_pending` rows for the old ID stay queued forever (or until a
> dashboard operator rotates the token). The fix is to either pass
> `--runner-id` explicitly or to deregister the old runner via the
> dashboard before deleting the DB.

## Driver discovery and auth reporting

Immediately after the WS handshake, the runner inspects its local
environment and reports what drivers it can run. The dashboard uses
this to populate the per-runner Drivers panel and to grey out Start
buttons when a driver is unauthenticated.

```rust
collect_driver_auth() → Vec<DriverAuthInfo>
```

For each known driver name (`claude`, `aider`, `codex`, `gemini`):

| Status            | Trigger                                                       |
|-------------------|---------------------------------------------------------------|
| `not_installed`   | Binary not on `$PATH` (cross-platform `which`)                |
| `api_key`         | Binary present **and** at least one expected env var set      |
| `unknown`         | Binary present but no env var set (likely OAuth, can't tell)  |

The env-var → driver mapping is hard-coded:

| Driver  | Env vars checked                                |
|---------|-------------------------------------------------|
| claude  | `ANTHROPIC_API_KEY`                             |
| aider   | `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`           |
| codex   | `OPENAI_API_KEY`                                |
| gemini  | `GEMINI_API_KEY`, `GOOGLE_API_KEY`              |

The list ships in **two** messages on every connect:

1. `RunnerHello { hostname, version, drivers }` — the canonical
   first frame, also used to update `runners.hostname` /
   `runners.version` in the server DB.
2. `DriverAuthReport { drivers }` — re-emitted as a separate message
   so the dashboard's `runner_drivers` event listener has a single
   handler for both initial state and later changes.

The wire protocol's `DriverAuthStatus` enum is richer than what the
runner currently emits (it can express `Oauth { account }` and
`CloudProvider { provider }` via the per-driver `AuthStatus`
introspection used by the in-process registry on a self-hosted server
— see [`server-rs/src/agents/driver.rs`](../../server-rs/src/agents/driver.rs)).
The runner's `collect_driver_auth` is the simpler "shell out + env
sniff" version because it has no driver crate compiled in. Bringing the
runner up to the same fidelity is tracked as a follow-up.

## Spawning agents (and reusing the session daemon)

When `StartAgent` arrives, the runner does **not** open a PTY itself.
It shells out to `branchwork-server session` — the same supervisor
binary self-hosted deployments run — and lets that daemon own the PTY,
the transcript log, and the local socket:

```
branchwork-server session \
    --socket <cwd>/.branchwork-runner-sessions/<agent_id>.sock \
    --cwd   <task working directory> \
    -- claude [--effort <effort>]
```

This is the central piece of code reuse in SaaS mode: PTY allocation,
log replay, fork+setsid (Unix), `DETACHED_PROCESS` (Windows), heartbeat
detection, and the typed `Message::Input/Output/Resize/Kill/Ping/Pong`
frame format are all shared with self-hosted mode. The only thing the
runner adds on top is the WS forwarding loop. See
[session-daemon.md](session-daemon.md) for the daemon internals.

Spawn sequence:

1. `mkdir -p <runner-cwd>/.branchwork-runner-sessions/`.
2. `Command::new(server_bin).args(["session", "--socket", …, "--cwd", …, "--", driver_bin, …])`.
   On Unix, `pre_exec` calls `setsid()` so the daemon outlives the
   runner. On Windows, `creation_flags(DETACHED_PROCESS |
   CREATE_NO_WINDOW)`.
3. Poll `<socket>` path for up to **10 s**, then sleep **200 ms** more
   for the daemon to start listening. Failure → `AgentStopped { status:
   "failed", stop_reason: "spawn failed: ..." }` reliable.
4. Connect a tokio task (`forward_agent_io`) to the socket. It reads
   `Message::Output` frames, base64-encodes the bytes, and pushes
   `AgentOutput` envelopes into the outbound channel as best-effort.
5. **Prompt injection.** The first 16 KiB of output is buffered while
   `is_ready()` watches for the readiness glyph (`❯` for Claude, or a
   generic `\n> ` REPL prompt). Once ready, the runner writes a single
   `Message::Input(prompt_bytes)` frame to the daemon and stops
   buffering. This is the equivalent of the self-hosted server typing
   the prompt into the agent's terminal — keeping it in the runner
   means the prompt itself never leaves customer infrastructure
   except as transient `agent_input` traffic the runner originated.
6. The `AgentHandle { pid, socket_path, io_task }` is stashed in
   `state.agents` keyed by `agent_id` so future `KillAgent`,
   `AgentInput`, and `ResizeTerminal` commands can find the right
   socket.

`KillAgent` is a SIGTERM to the daemon PID (Unix) or `taskkill /T`
(Windows). The `io_task` is also aborted, but the daemon itself decides
whether to cascade the kill to the underlying driver process — the
runner does not directly signal the driver.

## CWD handling

Three layers, in order of precedence:

| Source                       | Used for                                          |
|------------------------------|---------------------------------------------------|
| `StartAgent.cwd` (per-agent) | The agent's working directory if non-empty        |
| `--cwd` (process-wide)       | Fallback when `StartAgent.cwd` is empty           |
|  Default `.`                 | If neither is set, `--cwd` defaults to `.`        |

`std::fs::canonicalize(cwd)` runs once at startup and the canonical
path is what gets passed to every spawned daemon as `--cwd`. The
sockets directory `<canonical-cwd>/.branchwork-runner-sessions/` is
created with `create_dir_all` lazily on the first spawn.

A common deployment shape is one runner per repository checkout:
`branchwork-runner --cwd /home/me/code/foo` produces sockets in
`/home/me/code/foo/.branchwork-runner-sessions/`, and the dashboard
sends `StartAgent` envelopes with empty `cwd` fields. If the dashboard
issues per-task absolute paths (e.g. for monorepo subpackages) the
runner spawns the daemon with that exact path and the runner's own
`--cwd` is irrelevant for that agent.

## Outbox and replay on reconnect

This is the load-bearing reliability mechanism. The contract it offers
is **at-least-once delivery for reliable events; best-effort delivery
for terminal I/O and heartbeats**.

### What's reliable, what's best-effort

A message is reliable iff it is *not* in the best-effort set:

```
WireMessage::is_best_effort() → matches!(self,
    AgentOutput | AgentInput | Ping | Pong)
```

Concretely:

| Direction          | Reliable (outbox + ACK)                                         | Best-effort (no outbox, no ACK) |
|--------------------|------------------------------------------------------------------|--------------------------------|
| Runner → SaaS      | `RunnerHello`, `AgentStarted`, `AgentStopped`†, `TaskStatusChanged`, `DriverAuthReport`, `Resume`, `Ack` | `AgentOutput`, `Ping`, `Pong` |
| SaaS → Runner      | `StartAgent`, `KillAgent`, `TerminalReplay`, `Resume`, `Ack`     | `AgentInput`, `Ping`, `Pong`, `ResizeTerminal`‡ |

> † See [Failure modes](#failure-modes) — the *normal-exit* `AgentStopped`
> in `forward_agent_io` is currently sent best-effort, while the
> *spawn-failure* and `KillAgent` paths use `send_reliable`. This is a
> known wart, not a design choice; treat the wire protocol's "anything
> not best-effort is reliable" rule as the contract and assume the
> normal-exit path will be tightened.

> ‡ `ResizeTerminal` is technically reliable per the wire protocol but
> is sent through the same channel as `AgentInput` from the dashboard
> side; small terminal resizes that get lost across a reconnect are
> harmless because the dashboard re-emits them on the next render.

### Tables and seqs

Two SQLite tables, both ship in `outbox.rs`:

- **Runner side:** `runner_outbox(seq PK AUTOINC, event_type, payload,
  created_at, acked)`. One sender, no `runner_id` column needed.
- **Server side:** `inbox_pending(seq PK AUTOINC, runner_id,
  command_type, payload, created_at, acked)`. One row per
  (runner, command); the `idx_inbox_pending_runner` index covers the
  hot replay path.

A third helper table — `seq_tracker(peer_id PK, last_seq)` — exists on
both sides and stores the highest seq received from each peer. The
runner uses `peer_id = "server"`; the server uses `peer_id =
<runner_id>`.

### Send path (sender)

```
send_reliable(state, msg):
    payload = serde_json(msg)
    seq = enqueue_runner_event(conn, msg.event_type(), payload)   ← INSERT, AUTOINC
    env = Envelope::reliable(runner_id, seq, msg)
    ws_tx.send(serde_json(env))                                    ← may be queued only locally
```

The seq is allocated by SQLite and is **monotonic per sender** (the
runner's `runner_outbox.seq` is independent of the server's
`inbox_pending.seq`). The send into `ws_tx` is non-blocking; if the
WebSocket is currently down, the message stays in the channel and is
also durably in the outbox, so on reconnect either (a) the writer task
flushes the channel and the seq is acked, or (b) the channel was
abandoned by the previous writer task and the message is replayed from
the outbox.

### Receive path (receiver)

```
on Envelope { seq: Some(s), .. }:
    is_new = advance_peer_seq(conn, peer, s)   ← UPDATE seq_tracker
    send Ack { ack_seq: s }                    ← ALWAYS, even on dup
    if !is_new: skip                           ← idempotency check
    else: handle(envelope.message)
```

`advance_peer_seq` is the deduplication primitive. It refuses any seq
`<= last_seq` for that peer. **Always-ACK** means the sender can prune
its outbox even when the message was a retransmit — without this the
outbox would grow without bound across imperfect reconnects.

### ACK path (sender)

```
on WireMessage::Ack { ack_seq }:
    mark_runner_acked(conn, ack_seq)   ← UPDATE runner_outbox SET acked=1
```

ACKed rows are not deleted immediately; `prune_runner_outbox(keep)` /
`prune_server_inbox(runner_id, keep)` cull old rows lazily. (The
runner binary does not currently invoke `prune_runner_outbox` on a
schedule — long-lived runners with very high event volume will see
the table grow until the next manual vacuum. This is a follow-up.)

### Resume on reconnect

Both sides send `Resume { last_seen_seq }` immediately after
`RunnerHello` (runner side) or after they learn the runner_id (server
side). The receiver replays everything in its outbox with `seq >
last_seen_seq AND acked = 0`, in order, with the original seqs.

The order matters because:

- The receiver's `advance_peer_seq` enforces strict monotonicity.
  Replaying out of order would fail the `(seq as i64) <= current` check
  and silently drop messages.
- The dashboard's UI listens for the *agent_started* / *agent_output* /
  *agent_stopped* sequence in that order. Out-of-order delivery can
  briefly show a stopped agent that "starts" again.

### Delivery guarantees, summarised

- **Reliable messages:** at-least-once. The receiver's
  `advance_peer_seq` makes them effectively exactly-once **for
  state-machine transitions**, because the second copy is detected as
  a duplicate and the handler is not re-invoked. The first copy's
  side effects (DB row update, WS broadcast) are not transactional
  with the ACK, so a crash *between* `INSERT INTO agents …` and
  sending the ACK will cause a redelivery that the dedup correctly
  swallows. Net effect: callers can write side-effecting handlers
  assuming exactly-once.
- **Best-effort messages:** dropped on disconnect, never replayed.
  `AgentOutput` is the canonical example — replaying every byte the
  PTY emitted would dwarf the actual control plane and is unnecessary
  because the dashboard already has the *terminal replay* mechanism
  (`TerminalReplay { agent_id, from_offset }`) for catching up
  reconnecting browsers from the server-side log.

## Heartbeats

A dedicated tokio task ticks every 15 s and sends
`Envelope::best_effort(WireMessage::Ping {})`. The server replies with
`Pong {}` (also best-effort). Both sides drop the heartbeat task on
disconnect.

The runner does not currently kill the connection on a missing pong —
the WebSocket itself surfaces TCP-level death via a stream error,
which propagates out of `ws_reader` and into the reconnect loop. In
practice, network partitions that don't kill the TCP socket (e.g. a
silent middlebox dropping all traffic) take however long the OS's TCP
keepalive is set to detect.

## Failure modes

A bestiary of how things break and what the dashboard sees:

### Network loss (transient)

- **Symptom:** TCP RST or 30 s of no traffic; `ws_read.next()` returns
  `Some(Err(_))` and `connect_and_run` returns to the outer loop.
- **Effect:** Heartbeat and writer tasks are aborted. `state.agents`
  and the spawned session daemons keep running locally — they have no
  knowledge of the WS state. New `AgentOutput` envelopes pile up in the
  abandoned `ws_tx` channel and are dropped when the channel is
  garbage-collected.
- **Recovery:** Outer loop retries with jittered exponential backoff
  (1, 2, 4, 8, 16, 30, 30, … seconds, ±25 %). On reconnect: full
  `RunnerHello` + `DriverAuthReport` + `Resume` exchange. Any
  reliable events that were enqueued during the outage are replayed
  in seq order; best-effort `AgentOutput` for those seconds is lost,
  but the dashboard will pick up live output again as soon as the
  next frame arrives.
- **Server-side cleanup:** `runners.status` is updated to `offline`
  inside the disconnect handler; on reconnect the same row flips back
  to `online`.

### Token revocation

- **Symptom:** Operator deletes the row from `runner_tokens`. Existing
  WebSockets keep working until the next reconnect, at which point
  `validate_runner_token` returns `None` and the upgrade gets a
  `401 invalid_token`.
- **Effect:** `connect_async` returns
  `Err(Http(401))`. The runner prints the error and re-enters the
  backoff loop; backoff caps at 30 s, so the runner will retry every
  ~30 s indefinitely.
- **Recovery:** Manual — operator must either restore the token or
  stop the runner process. There is no in-band signal for "give up".
- **Side effect:** The unACKed events in `runner_outbox` accumulate
  forever. If a new token is later issued for the *same* runner_id
  (e.g. via `--runner-id`) the events flush on first connect.

### Binary version skew

- **Symptom:** Server is upgraded to a newer `WireMessage` schema and
  ships a variant the runner doesn't know.
- **Effect:** `serde_json::from_str::<Envelope>` returns `Err`. The
  runner's `ws_reader` logs `failed to parse envelope` and **does not
  ACK**. The server's `inbox_pending` row stays unACKed and replays
  forever on every subsequent reconnect, blocking nothing but
  growing the table.
- **Mitigation:** The wire protocol uses `#[serde(tag = "type")]` and
  no `#[serde(deny_unknown_fields)]`, so additive changes (new
  variants, new fields on existing variants with `#[serde(default)]`)
  are forward-compatible. The runner is intended to be upgrade-able
  independently — re-deploy the runner binary, restart, replay the
  backlog. **Removing or renaming an existing variant requires a
  coordinated rollout.**
- **Detection:** `RunnerHello { version }` makes runner versions
  visible in the dashboard so operators can spot stragglers.

### Session daemon crash

- **Symptom:** The driver process dies, segfaults, or is killed
  externally. The session daemon detects EOF on the PTY, writes the
  exit status into the on-disk log, and exits.
- **Effect on runner:** `forward_agent_io` returns from its `read_frame`
  loop and the spawned task fires the (currently best-effort) normal-exit
  `AgentStopped`. If the WS is up, the dashboard sees the stop in
  near-real-time; if the WS is down, the stop event is **lost** because
  it was sent best-effort, and the dashboard will continue showing the
  agent as "running" until the next event for that agent (which never
  comes) or until the runner is restarted and the underlying agent
  re-cleanup logic on the server kicks in. This is the wart called out
  in the [reliable/best-effort table](#whats-reliable-whats-best-effort).
- **Mitigation today:** spawn-failure and explicit `KillAgent` paths
  *do* use `send_reliable`, so the most common stop scenarios
  (immediate spawn failure, dashboard kill button) survive a reconnect
  fine.

### Runner crash / SIGKILL

- **Symptom:** `branchwork-runner` itself dies. `tokio::spawn`-ed I/O
  tasks evaporate; spawned **session daemons keep running** because
  they are detached (setsid on Unix, DETACHED_PROCESS on Windows).
- **Effect:** The dashboard sees `runner_disconnected`. The session
  daemons buffer PTY output to their on-disk log and wait for a client
  to attach. The agents themselves keep working.
- **Recovery:** Restart `branchwork-runner` with the same `--cwd` and
  the same `runner.db` (i.e. don't blow away `~/.branchwork-runner/`).
  The reconnect handshake re-registers the runner, the outbox replays
  pending events, but the runner has **no mechanism to re-attach to
  pre-existing session daemons** — the in-memory `state.agents` map is
  empty, so any future `AgentInput` / `KillAgent` for an old agent is
  silently dropped. The session daemons will keep running until their
  driver exits naturally or the operator kills them by hand. This is
  the runner-side analogue of the self-hosted server's
  `cleanup_and_reattach` and is currently a gap; it is the second
  wart in the runner that needs filling in.

### Customer machine reboot

- **Symptom:** Power off, OS reboot.
- **Effect:** Both the runner and every session daemon die. PTYs are
  gone, agents are gone. `runner.db` survives because it is on disk.
- **Recovery:** systemd unit (or equivalent) restarts
  `branchwork-runner` on boot. The outbox replays anything that hadn't
  been ACKed before shutdown. There is no agent-level recovery —
  in-flight agents are lost and must be restarted from the dashboard.

### SaaS server outage

- **Symptom:** Dashboard server is down. Every reconnect attempt fails
  fast with a connection-refused / 502 / 503.
- **Effect:** Backoff caps at 30 s; runner keeps retrying. Local
  agents continue to run and their events accumulate in `runner_outbox`.
- **Recovery:** Server returns; replay flushes the backlog. The only
  bound on event accumulation is local disk space.

## Glossary of source files

| File                                                                                          | Role                                                                |
|-----------------------------------------------------------------------------------------------|---------------------------------------------------------------------|
| [`server-rs/src/bin/branchwork_runner.rs`](../../server-rs/src/bin/branchwork_runner.rs)      | Binary entry, reconnect loop, agent spawning, I/O forwarding        |
| [`server-rs/src/saas/runner_protocol.rs`](../../server-rs/src/saas/runner_protocol.rs)        | `Envelope`, `WireMessage`, `DriverAuthInfo`, best-effort classification |
| [`server-rs/src/saas/outbox.rs`](../../server-rs/src/saas/outbox.rs)                          | `runner_outbox` / `inbox_pending` / `seq_tracker` schema + helpers  |
| [`server-rs/src/saas/runner_ws.rs`](../../server-rs/src/saas/runner_ws.rs)                    | Server-side WS handler, token validation, `RunnerRegistry`          |
| [`server-rs/src/agents/supervisor.rs`](../../server-rs/src/agents/supervisor.rs)              | The session daemon the runner shells out to (shared with self-hosted) |
| [`server-rs/src/agents/session_protocol.rs`](../../server-rs/src/agents/session_protocol.rs)  | Length-prefixed `Message` frames runner ↔ session daemon            |

The runner intentionally pulls the SaaS protocol and outbox modules in
via `#[path]` rather than depending on the `branchwork-server` crate,
so it can be built standalone and deployed without the rest of the
server's compiled code on the runner machine. The runtime dependency
on the supervisor is via `branchwork-server session` on `$PATH`, not
via Rust linkage.
