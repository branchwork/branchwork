# Self-hosted runbook

Run `branchwork-server` as a long-lived process on a single host —
laptop, home server, or VM — backed by SQLite and spawning local
agents. This is the deploy shape the
[quickstart](../quickstart.md) walks through; that page gets you
running in five minutes, this page covers the day-2 surface: an
init-system unit you paste into `/etc/systemd/system/` (or
`~/Library/LaunchAgents/`, or NSSM), where the logs go, what to back
up, and the upgrade procedure.

The architecture this rests on lives in
[architecture/server.md](../architecture/server.md) and
[architecture/session-daemon.md](../architecture/session-daemon.md);
flag and env-var detail in [reference/cli.md](../reference/cli.md)
and [reference/configuration.md](../reference/configuration.md). For
the SaaS shape — hosted dashboard, customer-side runner — see
[operations/saas-runner.md](saas-runner.md).

## Footprint

A self-hosted Branchwork install is one binary plus one state
directory:

| Path | Created by | Holds |
|---|---|---|
| `branchwork-server` (~15 MB, no runtime deps) | release archive or `cargo build --release` | The binary. Drop it on `PATH`; the install script lands it at `/usr/local/bin/branchwork-server`. |
| `~/.claude/branchwork.db` (+ `-wal`, `-shm`) | `db::init` at first boot | SQLite — agents, plan task status, audit log, runner outbox. WAL mode. |
| `~/.claude/plans/*.{yaml,yml,md}` | `POST /api/plans` and the file watcher | Plan source of truth. |
| `~/.claude/sessions/<agent-id>.{sock,log,pid,mcp.json}` | the per-agent session daemon | One set per running agent. The `.log` file is the authoritative PTY transcript. |

Override the state root with `--claude-dir <path>` (or set `HOME`
explicitly under the unit, since `~` resolves through
`dirs::home_dir()`). Full layout in
[reference/configuration.md § Filesystem layout](../reference/configuration.md#filesystem-layout).

## Linux — systemd unit

Drop this at `/etc/systemd/system/branchwork.service`. It runs as a
dedicated `branchwork` user; create the user first if it doesn't
exist (`useradd --system --create-home --shell /usr/sbin/nologin
branchwork`). The state directory is the user's `$HOME`, so backup
and upgrade procedures don't need to know where it lives.

```ini
[Unit]
Description=Branchwork dashboard server
Documentation=https://github.com/branchwork/branchwork
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=branchwork
Group=branchwork
# branchwork-server does not daemonize; systemd owns the process directly.
# Override the listen port or effort here if needed; see reference/cli.md.
ExecStart=/usr/local/bin/branchwork-server
# Optional: webhook for agent-finished / phase-advanced notifications.
# Environment=BRANCHWORK_WEBHOOK_URL=https://hooks.slack.com/services/...
# Environment=BRANCHWORK_AUTO_FINISH_IDLE=1
Restart=on-failure
RestartSec=5s
# The dashboard binds 0.0.0.0:3100 by default. If you want it
# loopback-only, run behind a local reverse proxy and add:
# Environment=BRANCHWORK_BIND=127.0.0.1   (not yet wired — see issue tracker)

# Hardening: branchwork-server only needs its own home directory and
# the project repos it spawns agents in. Adjust ReadWritePaths to
# include every project root you intend to plan against.
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/home/branchwork
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

Enable and start:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now branchwork.service
sudo systemctl status branchwork.service
```

Tail logs (`branchwork-server` writes to stdout/stderr; systemd
captures both into the journal):

```sh
journalctl -u branchwork -f
```

The default port is `3100`. Open it in the browser, or front the
service with Caddy / nginx if you want TLS — the binary speaks plain
HTTP and does not terminate TLS itself. (The Hetzner production
deploy uses Caddy; see [`docs/ops/hetzner.md`](../ops/hetzner.md) for
the site block.)

### Local agents and the systemd sandbox

Spawned agents run as the same `branchwork` user and inherit the
unit's environment. If you want them to write into project repos
elsewhere on the filesystem, list every project root under
`ReadWritePaths=`. Without that, `ProtectHome=read-only` will let
the dashboard read git worktrees but block branch creation. Easiest
escape hatch: keep all project clones under `/home/branchwork/` so
the default `ReadWritePaths` covers them.

`branchwork-server` does not implement a SIGTERM handler — on stop
the OS just kills the process. The session daemons it spawned
[detach via `setsid` and survive](../architecture/session-daemon.md#detach-unix-vs-windows),
so a unit stop loses the dashboard but leaves agents working; on
restart, `cleanup_and_reattach` ([`agents/mod.rs`](../../server-rs/src/agents/mod.rs))
walks `~/.claude/sessions/` and re-binds to every live socket.

## macOS — launchd plist

Run `branchwork-server` as a per-user **LaunchAgent** (not a
LaunchDaemon — the supervisor's `setsid` is enough; LaunchAgent
gives the dashboard the same `HOME` and Keychain context the user
already configured `claude` under). Drop this at
`~/Library/LaunchAgents/dev.branchwork.server.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.branchwork.server</string>

  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/branchwork-server</string>
  </array>

  <!-- Optional environment. Uncomment to enable. -->
  <!--
  <key>EnvironmentVariables</key>
  <dict>
    <key>BRANCHWORK_WEBHOOK_URL</key>
    <string>https://hooks.slack.com/services/...</string>
  </dict>
  -->

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>

  <key>StandardOutPath</key>
  <string>/Users/USERNAME/Library/Logs/branchwork.out.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/USERNAME/Library/Logs/branchwork.err.log</string>

  <key>WorkingDirectory</key>
  <string>/Users/USERNAME</string>
</dict>
</plist>
```

Replace `USERNAME` with `$(whoami)`. Load and start:

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.branchwork.server.plist
launchctl kickstart -k gui/$(id -u)/dev.branchwork.server
launchctl print  gui/$(id -u)/dev.branchwork.server | head
```

Tail logs:

```sh
tail -f ~/Library/Logs/branchwork.err.log
```

`KeepAlive=true` is the launchd equivalent of `Restart=on-failure` —
launchd respawns the process if it exits non-zero. To stop the
service:

```sh
launchctl bootout gui/$(id -u)/dev.branchwork.server
```

Two macOS-only gotchas:

- **Code-signing dialog on first launch.** The release binary is
  not notarized today. The first time launchd runs it, macOS may
  block the binary; one manual `open` (or `xattr -d
  com.apple.quarantine /usr/local/bin/branchwork-server`) clears
  the quarantine bit.
- **Full Disk Access.** If you keep project repos under
  `~/Documents` or another protected directory, grant the binary
  Full Disk Access in System Settings → Privacy & Security so
  spawned agents can write task branches there.

## Windows — service notes

Branchwork on Windows does not ship a service wrapper. The supervisor
detach path uses
[`CREATE_NO_WINDOW | DETACHED_PROCESS`](../architecture/session-daemon.md#detach-unix-vs-windows),
which is enough for spawned agents to survive `branchwork-server`
exits, but the dashboard process itself needs an external runner if
you want it up after logout.

Pick one:

- **NSSM (recommended).** [nssm.cc](https://nssm.cc) wraps any
  console binary as a Windows service with auto-restart. Install
  NSSM, then:

  ```cmd
  nssm install Branchwork "C:\Program Files\branchwork\branchwork-server.exe"
  nssm set Branchwork AppDirectory C:\Users\branchwork
  nssm set Branchwork Start SERVICE_AUTO_START
  nssm set Branchwork AppStdout C:\ProgramData\branchwork\stdout.log
  nssm set Branchwork AppStderr C:\ProgramData\branchwork\stderr.log
  nssm start Branchwork
  ```

  The `AppDirectory` becomes `branchwork-server`'s working directory
  and (with `dirs::home_dir()` resolving from `USERPROFILE`) decides
  where `branchwork.db` and `plans/` land.

- **`sc.exe create` directly.** Built-in but less ergonomic — you
  lose the auto-restart back-off and stdout capture NSSM gives for
  free. Workable for ad-hoc setups; not recommended for daily use.

- **Scheduled task with "At startup" trigger and "Run whether user
  is logged on or not."** Survives logout, no third-party install,
  but the task doesn't restart if the binary exits — pair with
  `--restart` semantics elsewhere or just accept the manual recovery
  step.

State paths on Windows resolve from `USERPROFILE` (typically
`C:\Users\<user>\.claude\`). Per-agent IPC uses named pipes
(`\\.\pipe\<stem>`) instead of UDS, but the on-disk filenames are
identical to Unix.

## Logs

| Stream | Where it lands | Captured by |
|---|---|---|
| `branchwork-server` stdout/stderr | systemd journal / launchd `StandardOutPath` / NSSM `AppStdout` | Init system. Boot lines, broadcast envelopes, panics. |
| Per-agent PTY transcript | `~/.claude/sessions/<agent-id>.log` | The session daemon writes every byte the PTY emits. Replayed on `/terminal` reconnect. |
| Per-agent supervisor PID | `~/.claude/sessions/<agent-id>.pid` | Daemon's own PID. Removed on clean exit; presence after process death is the canonical "supervisor crashed" signal (see [`agents/pty_agent.rs`](../../server-rs/src/agents/pty_agent.rs)). |
| Per-session settings | `~/.claude/sessions/<agent-id>.settings.json` | Stop-hook configuration written at spawn for `claude` agents only ([`agents/session_settings.rs`](../../server-rs/src/agents/session_settings.rs)). Cleaned up on agent exit. |

Branchwork itself has no log-rotation policy — the server is a
single binary writing to stdout, and the daemons append to their
log files for the lifetime of the agent (which is bounded — sockets
and logs are unlinked when the agent exits). If you keep many
long-running agents and want to bound disk use per agent, kill them
from the dashboard (the daemon cleans up its sibling files on exit
via the path in [`agents/pty_agent.rs::on_agent_exit`](../../server-rs/src/agents/pty_agent.rs)).

## Backup

The entire persistent state is under `~/.claude/` (per the layout
table above). To capture a consistent snapshot:

```sh
# Stop the server briefly to flush WAL into the main DB file. This is
# not strictly required (sqlite3 .backup works on a live WAL DB) but
# makes the resulting tarball trivial to inspect.
sudo systemctl stop branchwork
tar -C /home/branchwork -czf branchwork-$(date +%F).tgz .claude/
sudo systemctl start branchwork
```

Hot backup without stopping the server:

```sh
sqlite3 ~/.claude/branchwork.db ".backup '/tmp/branchwork-hot.db'"
tar -C /home/branchwork \
  --exclude='.claude/branchwork.db*' \
  -czf branchwork-$(date +%F).tgz .claude/
mv /tmp/branchwork-hot.db /backup/branchwork-$(date +%F).db
```

The `.backup` builtin gets a consistent SQLite file even with
concurrent writers; the rest of `~/.claude/` (plans, session
sockets, logs) tolerates a plain `tar` because the only files that
mutate during a snapshot are the per-agent transcripts, and losing
the most recent few bytes of an `.log` file does not corrupt
anything Branchwork reads back.

What's worth backing up vs. what isn't:

| Path | Worth backing up? |
|---|---|
| `~/.claude/branchwork.db` | **Yes.** Plan task status, audit log, agent history, cost ledger, runner outbox. |
| `~/.claude/plans/` | **Yes.** Plan source of truth — the dashboard reconstructs nothing if these go away. |
| `~/.claude/sessions/` | **No.** Only useful for live agents; on restart they reattach if the daemons are still alive, otherwise the rows in `branchwork.db` carry the history. |
| Project git worktrees | **Yes**, but with your normal repo backup strategy — Branchwork doesn't manage them. Task branches are pushed to your remote when you click Merge. |

There is no Postgres backend in the standalone binary
([architecture/server.md § Persistence](../architecture/server.md#persistence));
the Helm chart's `postgres` mode is not wired into the Rust source
yet. For self-hosted, SQLite is the only path.

## Upgrade procedure

The release sequence is **stop, swap binary, start**. There is no
in-place migration step — `db::migrate` is idempotent and runs at
every boot.

```sh
# 1. Fetch the new binary somewhere temporary.
curl -fsSL -o /tmp/branchwork-server.tgz \
  https://github.com/branchwork/branchwork/releases/latest/download/branchwork-server-linux-x64.tar.gz
tar -xzf /tmp/branchwork-server.tgz -C /tmp

# 2. Stop the dashboard. Spawned session daemons keep running.
sudo systemctl stop branchwork

# 3. Swap the binary atomically. install(1) writes to a temporary
# inode and renames into place.
sudo install -m 0755 /tmp/branchwork-server /usr/local/bin/branchwork-server

# 4. Start. db::migrate runs first; the file watcher and CI poller
# spawn next; cleanup_and_reattach walks ~/.claude/sessions/ and
# rebinds to every live agent socket.
sudo systemctl start branchwork
```

Why this is safe in steady state:

- **Spawned agents survive.** Session daemons forked off via
  [`setsid` (Unix)](../architecture/session-daemon.md#detach-unix-vs-windows)
  or `DETACHED_PROCESS` (Windows) are not in the dashboard's process
  group, so stopping `branchwork-server` does not signal them. The
  PTY keeps running, the on-disk transcript keeps growing, and the
  client (browser) sees a transient WebSocket disconnect.
- **Reattach on the way back up.** `cleanup_and_reattach`
  ([`agents/mod.rs`](../../server-rs/src/agents/mod.rs)) inspects
  every agent row in the DB whose status is `running` or `starting`,
  checks if the supervisor socket is still listening, and either
  re-binds the in-memory registry or marks the row `failed`. Browser
  reload picks the live ones back up; orphaned rows show up as
  stopped and can be retried.
- **Migrations are idempotent.** Every `migrate()` step is
  `CREATE TABLE IF NOT EXISTS` or `ALTER TABLE … ADD COLUMN`
  guarded by an `.ok()` so a re-run is a no-op
  ([architecture/persistence.md § Migration model](../architecture/persistence.md)).
  No manual schema-bump step is ever required.

If something does go wrong on start, the journal (or launchd log)
shows the error and the previous binary is still on disk under
`/tmp` — `install` to roll back, restart.

### When sessions don't reattach

If after a restart an agent shows in the dashboard but the terminal
panel is empty, the supervisor died sometime between stop and start.
The signature is a `<agent-id>.pid` file present in
`~/.claude/sessions/` with no live process matching that PID. Clean
it up by killing the agent from the dashboard (the row gets marked
`failed`) and re-spawning. The detail of how this is detected is in
[`agents/mod.rs::cleanup_and_reattach`](../../server-rs/src/agents/mod.rs).

## See also

- [quickstart.md](../quickstart.md) — five-minute happy path before
  you wire any of this up.
- [architecture/server.md](../architecture/server.md) — what the
  process actually does after `ExecStart`.
- [architecture/session-daemon.md](../architecture/session-daemon.md)
  — why `systemctl stop branchwork` does not kill spawned agents.
- [reference/cli.md](../reference/cli.md) — every flag on
  `branchwork-server` and its subcommands.
- [reference/configuration.md](../reference/configuration.md) —
  every environment variable the source actually reads.
- [ops/hetzner.md](../ops/hetzner.md) — production runbook for the
  containerized deploy at `branchwork.dev` (Caddy + Docker compose +
  GHCR).
