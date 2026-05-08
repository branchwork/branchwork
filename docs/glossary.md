# Glossary

Single-page definitions for the vocabulary the rest of the docs assume.
Each entry links to the canonical reference for the deep dive. Other
docs link here on first mention of a term.

If a term you're hunting for doesn't appear below, the
[user-guide table of contents](user-guide.md#contents) and the
[README](README.md) are the next places to look.

## Terms

### agent

One AI-CLI session running against one [task](#task) on its own git
branch. Each agent lives inside a detached [supervisor](#supervisor)
so killing the dashboard doesn't kill the agent. PTY agents
(Claude / Aider / Codex / Gemini) are interactive; [check
agents](#check-agent) are read-only stream-json runs that produce a
[verdict](#verdict). See
[user-guide.md § Agents](user-guide.md#agents).

### auto-mode

Per-plan opt-in that makes Branchwork advance through tasks without a
human click: merge on commit, run CI, spawn a fix-CI agent on red,
auto-advance to the next ready task on green. Driven by a server-side
supervisor loop. See
[user-guide.md § Auto-mode](user-guide.md#auto-mode) and
[ADR 0003](adrs/0003-unattended-auto-mode.md).

### auto-status

The file watcher's heuristic: when **any** of a task's `file_paths`
exist on disk, the task is bumped to `in_progress`. Auto-status
**never** writes `completed` — that requires an explicit user or
agent action. Auto-written rows carry `source = "auto"`; subsequent
manual changes flip `source = "manual"` and lock auto-status out. See
[user-guide.md § Auto-status](user-guide.md#auto-status) and
[repro-navbar-false-completion.md](repro-navbar-false-completion.md).

### check agent

A read-only verification run that produces a [verdict](#verdict)
without committing. Spawned per-task (**Check**), per-phase (**Check
Phase**), or per-plan (**Check All** / **Check Plan**). All entry
points share one prompt builder, so the verdict is purely a function
of working-tree content at check time — git history is not consulted.
See [user-guide.md § Check agents](user-guide.md#check-agents) and
[ADR 0004](adrs/0004-unify-check-prompts.md).

### driver

A small Rust trait that wraps one external AI CLI (`claude`, `aider`,
`codex`, `gemini`): `binary()`, `spawn_args()`, capability flags
(`supports_cost`, session resume, MCP injection), and an optional
Stop-hook config. Defaults to `claude`. Override per task via the
driver dropdown. See [reference/drivers.md](reference/drivers.md) and
[`server-rs/src/agents/driver.rs`](../server-rs/src/agents/driver.rs).

### effort

The `low` / `medium` / `high` / `max` knob threaded into every newly
spawned agent's prompt. The sidebar selector picks the user default;
the `--effort` flag on `branchwork-server` picks the server default;
each task may override either. Recorded in `audit_log` on change. See
[user-guide.md § Settings](user-guide.md#settings).

### MCP (Model Context Protocol)

The protocol Claude Code uses to give agents tool access. Branchwork
embeds an MCP server both at `/mcp` on the dashboard and as the
`branchwork-server mcp` stdio subcommand, exposing
`update_task_status`, `report_blocker`, `report_cost`, `list_plans`,
`get_task`, etc. Auto-injected into each Claude agent via a per-agent
`<agent>.mcp.json`. See [architecture/server.md](architecture/server.md)
and [bob-shell-integration.md](bob-shell-integration.md).

### outbox

Two SQLite tables in
[`saas/outbox.rs`](../server-rs/src/saas/outbox.rs) that give SaaS
reliable WireMessages at-least-once delivery across disconnects:
`runner_outbox` on the runner side (single sender, server ACKs by
`seq`) and `inbox_pending` on the server side (one row per runner,
runner ACKs by `seq`). Best-effort traffic (`AgentOutput`,
`Ping`/`Pong`) bypasses the outbox. See
[architecture/runner.md § Outbox and replay](architecture/runner.md#outbox-and-replay-on-reconnect).

### phase

A grouping of related tasks inside a [plan](#plan). Phases impose
order (phase 2 starts when phase 1 is done) and own per-phase verify
hooks (**Check Phase**, `phase_ci_blocking_workflows`,
`phase_verify`). See
[reference/plan-schema.md](reference/plan-schema.md).

### plan

A YAML file under `~/.claude/plans/` that describes a piece of work
as `phases` of `tasks`. The on-disk YAML is the source of truth —
SQLite stores runtime state (agents, task status, cost, audit) but
never the plan definition itself. Markdown plans are still parsed but
re-serialise to YAML on the next UI edit. See
[reference/plan-schema.md](reference/plan-schema.md) and the in-repo
sample [`plan.yaml`](../plan.yaml).

### produces_commit

Per-task boolean (default `true`). Set to `false` for investigation
tasks whose work lives in `docs/` rather than on a [task
branch](#task-branch) — the **Merge** banner is hidden, **Discard**
stays available, and the server's empty-branch merge guard returns
HTTP 409 as defense in depth. See
[user-guide.md § produces_commit](user-guide.md#produces_commit) and
[design-produces-commit.md](design-produces-commit.md).

### project

The directory the agent's work happens in — usually a sibling of the
plan file under `$HOME`. Set explicitly via `project:` in the YAML or
inferred at parse time from absolute paths in `context` and task
descriptions (most-frequent match wins). Plans without an inferred
project show under "Unassigned" and have **Check Plan** disabled. See
[user-guide.md § Project inference](user-guide.md#project-inference).

### runner

`branchwork-runner`, the SaaS-only customer-side bridge. Reaches
outbound to `wss://<host>/ws/runner?token=…`, translates SaaS
commands into local [supervisor](#supervisor) spawns, and ferries PTY
output back upstream. Reliable messages ride the [outbox](#outbox);
high-frequency I/O (`AgentOutput`, `AgentInput`, `Ping`/`Pong`) is
best-effort. Self-hosted Branchwork has no runner. See
[architecture/runner.md](architecture/runner.md) and
[operations/saas-runner.md](operations/saas-runner.md).

### server

`branchwork-server`, the dashboard backend — a single binary that
serves the SPA, `/api/*`, `/ws`, `/terminal`, `/mcp`, owns the SQLite
database, watches the plans directory, and embeds the [session
daemon](#session-daemon) under the `branchwork-server session`
subcommand. Runs on Linux, macOS, and Windows. See
[architecture/server.md](architecture/server.md).

### session daemon

The per-agent supervisor process. Forks + `setsid`s itself on Unix
(or is launched with `DETACHED_PROCESS` on Windows) so it outlives
the dashboard, owns the PTY master, runs the AI CLI inside it, and
listens on a local socket (Unix domain socket / named pipe) for
length-prefixed `postcard`-encoded
`Input`/`Output`/`Resize`/`Kill`/`Ping`/`Pong` frames. Reachable two
ways: the embedded `branchwork-server session` subcommand (normal
path) or the standalone `session_daemon` binary. See
[architecture/session-daemon.md](architecture/session-daemon.md).

### supervisor

Synonym for [session daemon](#session-daemon) — the per-agent process
that owns one PTY and one AI-CLI invocation. The two terms are
interchangeable in the docs and the source: "supervisor" emphasises
that it outlives its parent and handles crash recovery; "session
daemon" emphasises that it's one daemon per session.

### task

One work item under a [phase](#phase). Has a status, optional
`dependencies`, optional `file_paths`, optional
[`produces_commit`](#produces_commit), and a one-click **Start**
button that spawns an [agent](#agent) on a [task
branch](#task-branch). See [user-guide.md § Tasks](user-guide.md#tasks).

### task branch

The git branch a task agent works on, named
`branchwork/<plan>/<task>`. Created by the server on **Start**,
populated by the agent's commits, and gated for merge by the
empty-branch guard plus [`produces_commit`](#produces_commit). The
fix-CI variant is `branchwork/fix/<plan>/<task>/<run-id>`. See
[user-guide.md § Git flow](user-guide.md#git-flow).

### verdict

A [check agent](#check-agent)'s output: a status (`completed` /
`in_progress` / `pending`) plus a reason string. Persisted to
`task_status` and (for plan-level checks) `plan_verdicts`. The
verdict's status writes back into the task's status; its reason is
informational, not gating. The **Merge** banner is gated by
[`produces_commit`](#produces_commit) and the empty-branch guard, not
by the verdict reason. See
[user-guide.md § Check agents](user-guide.md#check-agents).

## See also

- [README.md](README.md) — the docs map.
- [troubleshooting.md](troubleshooting.md) — symptom-shaped index.
- [user-guide.md](user-guide.md) — the long-form walkthrough every
  term ultimately points back to.
- [architecture/overview.md](architecture/overview.md) — the
  three-binary diagram and end-to-end Start-task walkthrough.
