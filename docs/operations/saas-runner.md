# SaaS runner runbook

Run `branchwork-runner` as a long-lived process on a customer
workstation, build box, or CI worker so a hosted Branchwork dashboard
(`branchwork.dev` or your own deploy) can spawn agents inside your
private filesystem and git remotes. The runner is the **only** piece
of customer infrastructure a SaaS deployment needs — driver CLIs, the
source tree, the agent's API key, and the resulting commits all stay
on the customer side; the dashboard sees only the events the runner
chooses to forward.

This page is the day-2 surface: how to mint a token from the
dashboard, where the runner persists its identity, the systemd /
launchd / NSSM units you paste in, what the network actually needs
(spoiler: outbound WSS, no inbound at all), how to rotate a token
without losing in-flight agents, and what to do when the dashboard
shows the runner red.

The architecture this rests on lives in
[architecture/runner.md](../architecture/runner.md) (lifecycle,
outbox, replay, failure modes); flag- and env-var-level reference is
in [reference/cli.md § branchwork-runner](../reference/cli.md#branchwork-runner)
and [reference/configuration.md](../reference/configuration.md). For
the single-host shape — server + agents on the same machine — see
[operations/self-hosted.md](self-hosted.md).

## Footprint

A runner install is one binary plus one tiny state directory:

| Path | Created by | Holds |
|---|---|---|
| `branchwork-runner` (~10 MB, no runtime deps beyond `git` + `gh` if you want CI integration) | release archive, `cargo build --release`, or `install-runner.sh` (drops it at `~/.local/bin/`) | The binary. Outbound WSS client; never opens a listening socket. |
| `~/.branchwork-runner/runner.db` (+ `-wal`, `-shm`) | `init_runner_outbox` + `init_seq_tracker` at first boot | SQLite — the [outbox](../architecture/runner.md#outbox-and-replay-on-reconnect) (`runner_outbox`), per-peer ACK cursors (`seq_tracker`), and the persisted `runner_id`. WAL mode. |
| `~/.branchwork-runner/config.toml` | `install-runner.sh` (only) | Operator-only record of the SaaS URL + token. The runner binary itself does **not** read this file — it consumes `--saas-url` / `--token` (or the matching env vars). Keep it `0600`; a fresh token here is enough to relaunch by hand. |
| `<cwd>/.branchwork-runner-sessions/<agent-id>.{sock,log,pid}` | the per-agent session daemon, lazily | One set per running agent. Sockets land under the runner's `--cwd`, not under `~/.branchwork-runner/`, because a single runner can host agents in multiple project trees and the sockets must be co-located with the worktree the agent edits. |
| `~/.branchwork-runner/runner.log`, `~/.branchwork-runner/runner.pid` | `install-runner.sh`'s `nohup &` background launcher | Only present if you bootstrapped via `install-runner.sh`. systemd / launchd / NSSM ignore these — they own the process and the log stream themselves. |

Override the DB path with `--db-path` (handy under systemd if you
want state under `/var/lib/`) and the working directory with `--cwd`.
Full layout in
[reference/configuration.md § Filesystem layout](../reference/configuration.md#filesystem-layout).

## Issue a runner token

The runner authenticates with a single API token at WebSocket
connect time. There is no challenge/response, no per-message signing,
and no token rotation during a session — token theft means session
theft until the token is revoked in the dashboard. Tokens are
**org-scoped**: a runner authenticated against one org cannot accept
commands intended for another.

There are three ways to get a token in front of a runner. Pick the
one that matches how you're rolling the runner out.

### One-liner from the dashboard

The dashboard's `/runners` page exposes an **Add runner** button that
mints a token, builds the install command, and renders it in a
copy-once modal. Under the hood:

```sh
# What the modal pastes (you don't run this by hand — copy from the modal).
curl -fsSL https://app.branchwork.dev/install-runner.sh \
  | sh -s -- bwr_live_xxxxxxxxxxxxxxxxxxxx
```

The script (sourced from
[`deploy/install-runner.sh`](../../deploy/install-runner.sh)):

1. Detects `uname -s / -m`. Supported triples: `linux-amd64`,
   `linux-arm64`, `darwin-arm64` (`darwin-amd64` falls through to a
   build-from-source hint).
2. Downloads the runner binary — first from the GitHub Release
   asset, falling back to extracting the multi-arch `:edge` GHCR
   image with `docker create + cp` when releases haven't shipped
   yet. `BRANCHWORK_BINARY_URL=…` overrides both.
3. Drops it at `~/.local/bin/branchwork-runner` (override:
   `BRANCHWORK_INSTALL_DIR=…`).
4. Writes `~/.branchwork-runner/config.toml` (mode `0600`) with the
   SaaS URL and token, for **your records** — the runner does not
   read this file.
5. Backgrounds the runner with `nohup … &`, writes the PID to
   `~/.branchwork-runner/runner.pid`, and tails the log to
   `~/.branchwork-runner/runner.log`.

If the runner connects within ~1 s the modal flips to **Connected**;
if it doesn't, the script tails the last 20 log lines to stderr and
exits non-zero.

This path is fastest, but the `nohup &` launcher does **not** survive
logout on macOS (LaunchAgents are user-scoped) and is not what you
want on a long-lived host. Once the runner is connected, follow it
up with one of the init-system units below; you can leave the same
`runner.db` in place so the runner reattaches to the same `runner_id`.

### `POST /api/runners/install-command` (programmatic)

The same modal hits this endpoint:

```sh
curl -fsSL https://app.branchwork.dev/api/runners/install-command \
  -H "Content-Type: application/json" \
  -H "Cookie: branchwork_session=<your-session-cookie>" \
  -d '{"runner_name":"build-box-1"}'
```

returns:

```json
{
  "token":       "bwr_live_xxxxxxxxxxxxxxxxxxxx",
  "command":     "curl -fsSL https://app.branchwork.dev/install-runner.sh | sh -s -- 'bwr_live_xxxxxxxxxxxxxxxxxxxx'",
  "runner_name": "build-box-1",
  "saas_url":    "https://app.branchwork.dev"
}
```

Same token row as the dashboard modal — `runner_tokens.org_id` comes
from your authenticated session, `created_by` from your user id,
`runner_name` is whatever you passed.

### `POST /api/runners/tokens` (token only, no install command)

If you want the bare token to bake into a config-management secret
(Ansible, k8s Secret, …) without the install-script wrapping:

```sh
curl -fsSL https://app.branchwork.dev/api/runners/tokens \
  -H "Content-Type: application/json" \
  -H "Cookie: branchwork_session=<your-session-cookie>" \
  -d '{"runner_name":"build-box-1"}'
```

returns just `{ "token": "...", "runner_name": "build-box-1" }`. The
token is the literal value the runner needs in `--token` /
`BRANCHWORK_RUNNER_TOKEN`.

> **One token, one runner.** The first time a runner connects with a
> given token, the server records its `runner_id` in
> `runner_tokens.claimed_runner_id`. From that point on, the same
> token only authenticates that same `runner_id`. If you copy the
> token to a second host (different `runner.db` ⇒ different
> `runner_id`) the second host's WS upgrade is refused with `401
> token_already_claimed`. Mint a fresh token per host.

## Persisted runner ID

The `runner_id` is the demultiplexing key on the SaaS side: it ties
`inbox_pending` rows to a specific runner and lets a reconnect
(possibly with a fresh WebSocket from a different IP) pick up where
the previous session left off. It must therefore survive process
restart, which is why it lives in `runner.db` rather than process
memory.

On startup, in
[`load_or_generate_runner_id`](../../server-rs/src/bin/branchwork_runner.rs):

```sql
SELECT peer_id FROM seq_tracker WHERE peer_id LIKE 'runner-%' LIMIT 1
```

Hit → reuse it. Miss → generate `runner-{uuidv4}` and `INSERT OR
IGNORE`. Override with `--runner-id` (or `BRANCHWORK_RUNNER_ID`) when
you want hostnames or k8s pod names as IDs; the DB lookup is skipped
entirely in that case.

> **Don't blow away `runner.db` casually.** Doing so resets the
> `runner_id` and the dashboard sees a fresh runner appear alongside
> the old one (now permanently `offline`). The old `inbox_pending`
> rows stay queued until the operator either reconnects with a
> matching `--runner-id` or revokes the runner via `DELETE
> /api/runners/{id}`. See
> [architecture/runner.md § Runner ID persistence](../architecture/runner.md#runner-id-persistence).

## Network requirements

The runner is **outbound-only**. There is no listening socket, no
inbound port to open, no reverse proxy in front of it. A typical
firewall rule looks like:

| Direction | Protocol / port | Destination | Why |
|---|---|---|---|
| Outbound | TCP 443 (`wss://`) or 80 (`ws://` lab only) | `app.branchwork.dev` (or your self-hosted SaaS URL) | The single authenticated WebSocket the runner keeps open. Carries every command and event. |
| Outbound | TCP 443 | `github.com`, `api.github.com`, `objects.githubusercontent.com` | Only if you use the GitHub Release asset path of `install-runner.sh`. Optional after install. |
| Outbound | TCP 443 | `ghcr.io` | Only when `install-runner.sh` falls back to extracting the `:edge` image via `docker create + cp`. Optional after install. |
| Outbound | TCP 443 | Driver vendor APIs (`api.anthropic.com`, `api.openai.com`, `generativelanguage.googleapis.com`) | The agent itself talks to its driver API directly from the runner host. Branchwork does not proxy LLM calls. |
| Outbound | TCP 443 | Your git remote (GitHub, GitLab, internal Bitbucket, …) | The runner pushes task branches with `git push origin <branch>` after a merge. |
| Outbound | DNS / NTP | as configured | Plain `getaddrinfo` and OS clock — needed for TLS cert validation and for deduplicating idle-poller ticks. |
| Inbound | _none_ | _none_ | The runner never accepts connections. |

The reconnect loop uses jittered exponential backoff (1, 2, 4, 8,
16, 30, 30, … seconds, ±25 %) and never gives up — only `SIGTERM` or
a panic in `main()` brings the runner down. If your network goes
through a captive portal, the WS will reconnect automatically once
TLS-to-the-SaaS-URL works again.

## Linux — systemd unit

Drop this at `/etc/systemd/system/branchwork-runner.service`. It runs
as a dedicated `branchwork-runner` user; create the user first if
needed (`useradd --system --create-home --shell /usr/sbin/nologin
branchwork-runner`). State lives under that user's `$HOME`, so
backups and upgrades don't need to know where it lives.

```ini
[Unit]
Description=Branchwork SaaS runner
Documentation=https://github.com/branchwork/branchwork
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=branchwork-runner
Group=branchwork-runner
# Secrets via EnvironmentFile so the unit text isn't a leaky place
# to store them. The file should be 0600 root:root and contain:
#   BRANCHWORK_SAAS_URL=wss://app.branchwork.dev
#   BRANCHWORK_RUNNER_TOKEN=bwr_live_xxxxxxxxxxxxxxxxxxxx
EnvironmentFile=/etc/branchwork-runner/env

ExecStart=/usr/local/bin/branchwork-runner \
    --cwd /home/branchwork-runner/work \
    --db-path /home/branchwork-runner/.branchwork-runner/runner.db

# Restart on every non-zero exit. The runner's reconnect loop already
# handles transient network failures; on-failure here only fires for
# panics, OOM, or operator SIGKILL.
Restart=on-failure
RestartSec=5s

# Hardening: the runner needs (a) outbound network, (b) read/write to
# its state dir, (c) read/write to the project worktrees it spawns
# agents in, (d) its session-socket dir under --cwd. Nothing else.
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/home/branchwork-runner
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Enable and start:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now branchwork-runner.service
sudo systemctl status branchwork-runner.service
```

Tail logs (`branchwork-runner` writes to stdout/stderr; systemd
captures both into the journal):

```sh
journalctl -u branchwork-runner -f
```

The very first `[runner] id=runner-xxxxxxxx cwd=… db=…` line is the
canonical "I came up cleanly" signal — until you see it, the binary
hasn't even reached its outer reconnect loop.

### Project trees and the systemd sandbox

Spawned session daemons run as the same `branchwork-runner` user and
inherit the unit's environment. If you want them to write into
project repos elsewhere on the filesystem, list every project root
under `ReadWritePaths=`. Without that, `ProtectHome=read-only` lets
the runner read git worktrees but blocks branch creation. Easiest
escape hatch: keep all project clones under
`/home/branchwork-runner/work/` so the default `ReadWritePaths`
covers them — and that's exactly what the `--cwd` in the unit above
points at.

`branchwork-runner` does not implement a SIGTERM handler — on stop
the OS just kills the process. The session daemons it spawned
[detach via `setsid`](../architecture/session-daemon.md#detach-unix-vs-windows)
and survive, so a `systemctl stop` loses the WebSocket but leaves
agents working; on restart the runner-side
`cleanup_and_reattach_runner` walks
`<cwd>/.branchwork-runner-sessions/`, re-binds to live socket files,
and the dashboard sees the same agents reappear after the next
reconnect. See
[architecture/runner.md § Runner crash / SIGKILL](../architecture/runner.md#runner-crash--sigkill).

## macOS — launchd plist

Run `branchwork-runner` as a per-user **LaunchAgent** (not a
LaunchDaemon — the supervisor's `setsid` is enough; LaunchAgent
gives spawned agents the same `HOME` and Keychain context the user
already configured `claude` / `aider` / `gh` under). Drop this at
`~/Library/LaunchAgents/dev.branchwork.runner.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.branchwork.runner</string>

  <key>ProgramArguments</key>
  <array>
    <string>/Users/USERNAME/.local/bin/branchwork-runner</string>
    <string>--cwd</string>
    <string>/Users/USERNAME/code</string>
  </array>

  <key>EnvironmentVariables</key>
  <dict>
    <key>BRANCHWORK_SAAS_URL</key>
    <string>wss://app.branchwork.dev</string>
    <key>BRANCHWORK_RUNNER_TOKEN</key>
    <string>bwr_live_xxxxxxxxxxxxxxxxxxxx</string>
    <!-- The runner shells out to `branchwork-server session …`; make
         sure that binary is reachable on PATH inside the LaunchAgent.
         GUI launchd does NOT inherit your shell PATH. -->
    <key>PATH</key>
    <string>/Users/USERNAME/.local/bin:/usr/local/bin:/usr/bin:/bin</string>
  </dict>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>

  <key>StandardOutPath</key>
  <string>/Users/USERNAME/Library/Logs/branchwork-runner.out.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/USERNAME/Library/Logs/branchwork-runner.err.log</string>

  <key>WorkingDirectory</key>
  <string>/Users/USERNAME</string>
</dict>
</plist>
```

Replace `USERNAME` with `$(whoami)`. Load and start:

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.branchwork.runner.plist
launchctl kickstart -k gui/$(id -u)/dev.branchwork.runner
launchctl print  gui/$(id -u)/dev.branchwork.runner | head
```

Tail logs:

```sh
tail -f ~/Library/Logs/branchwork-runner.err.log
```

`KeepAlive=true` is the launchd equivalent of `Restart=on-failure`.
Stop the agent:

```sh
launchctl bootout gui/$(id -u)/dev.branchwork.runner
```

Three macOS-only gotchas:

- **PATH does not inherit from your shell.** GUI launchd launches
  with a minimal PATH. The runner shells out to `branchwork-server
  session …` (and `git`, `gh`, `claude`, `aider`, …); pin every
  one of those into the plist `PATH` env var as shown above, or
  spawned agents will fail with `executable not found`.
- **Code-signing dialog on first launch.** The release binary is
  not notarized today. The first time launchd runs it, macOS may
  block it; one manual `open` (or `xattr -d com.apple.quarantine
  ~/.local/bin/branchwork-runner`) clears the quarantine bit.
- **Full Disk Access.** If your project repos live under
  `~/Documents`, `~/Desktop`, or another protected directory, grant
  the binary Full Disk Access in System Settings → Privacy &
  Security so spawned agents can write task branches there.

## Windows — service notes

Branchwork's runner on Windows does not ship a service wrapper. The
supervisor detach path uses
[`CREATE_NO_WINDOW | DETACHED_PROCESS`](../architecture/session-daemon.md#detach-unix-vs-windows),
which is enough for spawned agents to survive `branchwork-runner`
exits, but the runner process itself needs an external launcher if
you want it up after logout.

Pick one (same options as the [self-hosted Windows
notes](self-hosted.md#windows--service-notes)):

- **NSSM (recommended).** [nssm.cc](https://nssm.cc) wraps any
  console binary as a Windows service with auto-restart. Install
  NSSM, then:

  ```cmd
  nssm install BranchworkRunner "C:\Program Files\branchwork\branchwork-runner.exe"
  nssm set BranchworkRunner AppDirectory C:\Users\branchwork-runner\work
  nssm set BranchworkRunner AppParameters "--cwd C:\Users\branchwork-runner\work --db-path C:\Users\branchwork-runner\.branchwork-runner\runner.db"
  nssm set BranchworkRunner AppEnvironmentExtra BRANCHWORK_SAAS_URL=wss://app.branchwork.dev BRANCHWORK_RUNNER_TOKEN=bwr_live_xxxxxxxxxxxxxxxxxxxx
  nssm set BranchworkRunner Start SERVICE_AUTO_START
  nssm set BranchworkRunner AppStdout C:\ProgramData\branchwork\runner-stdout.log
  nssm set BranchworkRunner AppStderr C:\ProgramData\branchwork\runner-stderr.log
  nssm start BranchworkRunner
  ```

  The `AppDirectory` becomes the runner's working directory and
  decides where session sockets land
  (`<AppDirectory>\.branchwork-runner-sessions\`).

- **`sc.exe create` directly.** Built-in but you lose the
  auto-restart back-off and stdout capture. Workable for ad-hoc
  setups; not recommended for daily use.

- **Scheduled task with "At startup" trigger and "Run whether user
  is logged on or not."** Survives logout, no third-party install,
  but the task doesn't restart if the binary exits — pair with a
  monitoring script or accept the manual recovery step.

State paths on Windows resolve from `USERPROFILE` (typically
`C:\Users\<user>\.branchwork-runner\`). Per-agent IPC uses named
pipes (`\\.\pipe\<stem>`) instead of UDS, but the on-disk filenames
are identical to Unix.

## Logs

| Stream | Where it lands | Captured by |
|---|---|---|
| `branchwork-runner` stdout/stderr | systemd journal / launchd `StandardOutPath` / NSSM `AppStdout` | Init system. Boot lines (`[runner] id=…`), reconnect events, `[runner] shutdown requested by SaaS:` on operator-initiated drains. |
| Per-agent PTY transcript | `<cwd>/.branchwork-runner-sessions/<agent-id>.log` | The session daemon writes every byte the PTY emits. Replayed on `/terminal` reconnect via the dashboard's terminal-replay path; the runner does not forward `<.log>` content — it only forwards live `Message::Output` frames. |
| Per-agent supervisor PID | `<cwd>/.branchwork-runner-sessions/<agent-id>.pid` | Daemon's own PID. Removed on clean exit; presence after process death is the canonical "supervisor crashed" signal that `cleanup_and_reattach_runner` keys off when reaping orphans. |
| Local outbox state | `~/.branchwork-runner/runner.db` (`runner_outbox`, `seq_tracker`) | Not human-readable, but `sqlite3 runner.db 'SELECT COUNT(*) FROM runner_outbox WHERE acked = 0;'` is the runner-side answer to "is anything backlogged?". The dashboard's `/runners` page surfaces the same number under `health.outboxDepth`. |

The runner has no log-rotation policy. The journal / launchd /
NSSM owners rotate stdout/stderr per their own policy; the
session-daemon `.log` files exist for the lifetime of the agent
(bounded — sockets and logs are unlinked when the agent exits, see
[`agents/pty_agent.rs::on_agent_exit`](../../server-rs/src/agents/pty_agent.rs)).

## Token rotation

Tokens never auto-rotate. Plan for rotation as a deliberate operator
action:

1. **Mint a new token for the same runner.** From the dashboard's
   `/runners` page, **Re-issue token** on the row, or
   `POST /api/runners/tokens` with the same `runner_name`. The
   server inserts a new `runner_tokens` row; the existing connection
   is unaffected.
2. **Roll the new token into the runner's environment.** Update
   `/etc/branchwork-runner/env` (systemd), the
   `EnvironmentVariables` block in the launchd plist, or the
   `AppEnvironmentExtra` of the NSSM service. **Do not restart
   yet.**
3. **Restart the runner.** `systemctl restart branchwork-runner`,
   `launchctl kickstart -k …`, `nssm restart BranchworkRunner`.
   The runner reconnects on the new token; because its `runner_id`
   is persisted in `runner.db`, the SaaS side `INSERT … ON CONFLICT
   DO UPDATE`s the same `runners` row and existing
   `inbox_pending` rows replay normally.
4. **Revoke the old token.** Either delete the row from
   `runner_tokens` directly, or use the dashboard's revoke action.
   The old token can no longer authenticate, but the
   `runners.claimed_runner_id` binding survives — a future
   re-issued token for the same `runner_name` re-binds to the same
   `runner_id`.

If you skip step 2 and restart with the old token still in place,
the runner happily reconnects on the old token until you revoke it.
If you revoke the old token first, the runner's existing WebSocket
keeps working until the next disconnect; from that point on every
reconnect attempt fails with `401 invalid_token` and the reconnect
backoff caps at 30 s. See
[architecture/runner.md § Token revocation](../architecture/runner.md#token-revocation).

> **Hard reset.** `DELETE /api/runners/{id}` (the `/runners` page's
> **Revoke** button) deletes every `runner_tokens` row whose
> `claimed_runner_id` matches and soft-deletes the `runners` row
> (`removed_at = datetime('now')`). The next reconnect attempt is
> refused with `401 invalid_token`; the runner's outbox and
> `runner.db` survive locally so a future re-issuance can reattach.
> The current WS connection (if any) stays alive until the runner
> closes it or TCP keepalive times out — there is no kill -9.

## Multi-runner setups

Multiple runners are first-class. They are all keyed by
`runner_id` server-side, so the dashboard distinguishes them
naturally; the choice of how to slice your fleet is purely
operational.

Three patterns we see in practice:

- **One runner per project tree.** Mint a token per runner, set
  `--cwd /home/me/code/foo` on each. The dashboard sends `StartAgent`
  envelopes with empty `cwd` fields and the runner spawns the
  daemon under that `--cwd`. Sockets land in
  `/home/me/code/foo/.branchwork-runner-sessions/`. This is the
  cleanest separation but the most processes to operate.

- **One runner per host, with per-task absolute `cwd`.** Mint one
  token, run one runner with `--cwd /home/me/code` (the parent of
  every project). Plan `project:` fields like
  `/home/me/code/foo` resolve to absolute paths the dashboard
  forwards in `StartAgent.cwd`; the runner spawns the daemon there.
  The runner's own `--cwd` is irrelevant for those agents
  (canonicalisation still runs at startup so the WS handshake has a
  hostname-stable identity).

- **Fleet of build-box runners with stable IDs.** For k8s pods,
  CI runners, or anything ephemeral, set
  `--runner-id $POD_NAME` (or `BRANCHWORK_RUNNER_ID`) and avoid
  persisting `runner.db`. The dashboard tracks the pod by its
  Kubernetes name; on restart the same pod reuses its identity
  even though the local DB was discarded. (Outbox events are
  lost in this mode; only events emitted while the WS was
  actually connected reach the dashboard.)

There is no per-runner "owner project" today — every connected
runner in an org is eligible to receive any `StartAgent` from that
org. The dashboard's runner picker chooses the runner whose
`StartAgent.cwd` resolves locally.

## Troubleshooting connect / reconnect

A short triage list — the deeper failure-mode bestiary lives in
[architecture/runner.md § Failure modes](../architecture/runner.md#failure-modes).

### The dashboard never flips to "Connected"

- Tail the runner log — the very first line should be `[runner]
  id=runner-… cwd=… db=…`. If it's missing, the binary didn't reach
  the outer reconnect loop. Common causes: missing `branchwork-server`
  on `$PATH` (the runner spawns it lazily, but a missing binary
  becomes visible the first time you spawn an agent — pre-flight
  with `branchwork-server --version`); insufficient permissions on
  `~/.branchwork-runner/`; SQLite refusing `PRAGMA journal_mode =
  WAL` on a noexec / network mount.
- Look for `failed to connect: …` lines. The reconnect loop logs
  every failed `connect_async`. `Http(401)` means token rejected
  (see next item). `Connection refused` / DNS errors mean the
  network path is broken.

### Reconnects every ~30 s with `401`

- The token is wrong, revoked, or claimed by a different runner.
  `runner_tokens.claimed_runner_id` binds the token to the *first*
  runner that successfully connects on it, so copying a token to a
  second host gets `401 token_already_claimed`. Mint a fresh token
  per host, or pre-claim by setting `--runner-id` to the same
  identity on both runs (the second host then takes over the slot
  and the first one starts getting `401`s — this is generally not
  what you want).

### Connected, but no agents will spawn

- The dashboard's runner row shows online but `StartAgent` lands
  with `agent_failed: spawn failed: …`. Either (a) `branchwork-server`
  isn't on `$PATH` of the runner's process (systemd / launchd /
  NSSM environments do **not** inherit the operator's shell PATH —
  pin it explicitly in the unit or use `--server-bin
  /usr/local/bin/branchwork-server`), or (b) the driver CLI isn't
  installed (`claude` / `aider` / `gh` not on the runner host's
  PATH). The runner's `RunnerHello.drivers` list is the
  ground-truth signal; if the dashboard's per-row driver chip shows
  the driver as `not_installed`, install it on the runner and
  restart. See
  [architecture/runner.md § Driver discovery](../architecture/runner.md#driver-discovery-and-auth-reporting).

### Backlog growing on `runner_outbox`

- `sqlite3 ~/.branchwork-runner/runner.db 'SELECT COUNT(*) FROM
  runner_outbox WHERE acked = 0;'` returning a number that climbs
  monotonically means the SaaS side either is not ACKing (server
  outage, server restarted with a stale `runner_id` mapping) or
  cannot keep up. Same number is surfaced as `health.outboxDepth`
  on `/api/runners`. Steady state on a healthy connection is
  single digits; a few thousand after a multi-hour outage is
  normal. Many tens of thousands indicates a stuck server-side
  consumer — escalate to the dashboard operator.

### Runner restarts but agents are gone

- A clean `systemctl restart` should reattach to live session
  daemons via `cleanup_and_reattach_runner` (sweeping
  `<cwd>/.branchwork-runner-sessions/` for live socket files).
  If agents come back as `failed` instead of resuming, the
  signature is a `<agent-id>.pid` file present with no live
  process matching that PID — the supervisor crashed sometime
  between stop and start. Clean it up by killing the agent from
  the dashboard and re-spawning. Detail in
  [architecture/runner.md § Runner crash / SIGKILL](../architecture/runner.md#runner-crash--sigkill).

### "Runner disconnected" mid-task

- Transient WS drops are expected and handled — the dashboard
  shows a brief red dot, the reconnect succeeds within seconds,
  and reliable events replay from the outbox. If the disconnect
  persists, check the runner's network path
  ([Network requirements](#network-requirements) above). Spawned
  session daemons keep running locally regardless; agents
  themselves only stop if the customer machine reboots or the
  driver process dies.

## See also

- [architecture/runner.md](../architecture/runner.md) — what the
  binary actually does after `ExecStart`: lifecycle, outbox, replay,
  and every failure mode in detail.
- [architecture/session-daemon.md](../architecture/session-daemon.md)
  — why `systemctl stop branchwork-runner` does not kill spawned
  agents.
- [architecture/protocols.md](../architecture/protocols.md) — the
  `WireMessage` reliable / best-effort split, request/response
  frames, and version-skew policy.
- [reference/cli.md § branchwork-runner](../reference/cli.md#branchwork-runner)
  — every flag on the runner binary.
- [reference/configuration.md](../reference/configuration.md) —
  every environment variable the runner actually reads, plus the
  shape of `~/.branchwork-runner/`.
- [operations/self-hosted.md](self-hosted.md) — the single-host
  shape (server + agents on the same machine) that this page is the
  SaaS counterpart to.
