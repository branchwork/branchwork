# Quickstart

Five minutes from nothing to a Claude agent working on a real task in
your browser. Self-hosted, single machine, no account.

## 1. Install

Pick one. All three land the same `branchwork-server` binary on your
`PATH`.

**Shell installer (Linux / macOS).** Downloads the latest release
binary for your platform into `/usr/local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/branchwork/branchwork/master/install.sh | sh
```

**Direct binary.** Grab the archive that matches your platform from
[github.com/branchwork/branchwork/releases](https://github.com/branchwork/branchwork/releases/latest)
(`branchwork-server-{linux,macos}-{x64,arm64}.tar.gz`), extract it, and
move `branchwork-server` somewhere on your `PATH`.

**Build from source.** Requires Rust 1.88+, Node.js 20+, pnpm:

```sh
pnpm --filter @branchwork/web build && \
  cd server-rs && cargo build --release
```

Binary lands at `server-rs/target/release/branchwork-server`
(~15 MB, no runtime dependencies).

## 2. Check prerequisites

Branchwork drives an external AI CLI and works out of a git worktree.
Confirm both are reachable:

```sh
claude --version    # or: aider --version / codex --version / gemini --version
git --version
```

If `claude` isn't installed, follow the
[Claude Code install guide](https://docs.claude.com/en/docs/claude-code/setup)
and run `claude` once to complete OAuth login (or export
`ANTHROPIC_API_KEY`). Any driver Branchwork supports is fine — Claude
Code is the default because its MCP integration gives agents the
richest feedback loop.

You do not need to pre-create a git repo for your project: Branchwork
auto-inits one the first time it needs a task branch.

## 3. Run the server

```sh
branchwork-server
```

No flags needed. The server binds `http://localhost:3100`, creates
`~/.claude/plans/` and `~/.claude/branchwork.db` if they don't exist,
and embeds the dashboard SPA into the same binary.

Open <http://localhost:3100> in any browser. You should see an empty
plan board.

## 4. Create your first plan and start a task

1. Click **+ New Plan** in the left sidebar.
2. Describe what you want to build in one or two sentences
   (e.g. "Add a `/health` endpoint that returns `{status: \"ok\"}`").
3. Pick a folder. Any directory on the host machine works — if it
   isn't a git repo yet, Branchwork will init one.
4. Click **Create Plan**. A design agent spins up and writes
   `~/.claude/plans/<slug>.yaml` with phases and tasks. The plan
   appears in the sidebar as soon as the file is written.
5. On any task card, click **Start**. An agent spawns on the branch
   `branchwork/<plan>/<task>` and a live xterm.js terminal opens in
   the panel on the right.

The agent is now working. You can type at it like any other terminal.

## 5. Prove session persistence

The terminal you're watching lives inside a detached supervisor
daemon, not inside `branchwork-server` itself. Kill the server to
prove it:

1. In the terminal where `branchwork-server` is running, press
   `Ctrl-C`. The browser tab disconnects.
2. Start it again:

   ```sh
   branchwork-server
   ```

3. Reload the browser. Click back into the running agent. The same
   terminal reattaches, and you can see everything the agent produced
   while the server was down — the PTY transcript is persisted to
   `~/.claude/sessions/<agent-id>.log` and replayed on reconnect.

That's the core loop. Plans on disk, agents in isolated supervisor
daemons, git branches per task, review and merge in the browser.

## Where to go next

- [user-guide.md](user-guide.md) — full dashboard tour: plan
  authoring, check agents, diffs, merge/discard, cost tracking, CI
  integration, drivers.
- [architecture/overview.md](architecture/overview.md) — the three
  binaries (`branchwork-server`, `session_daemon`,
  `branchwork-runner`), the protocols between them, and where session
  state lives.
- [troubleshooting.md](troubleshooting.md) — common failures (empty
  merge branch, stuck status, driver auth).
