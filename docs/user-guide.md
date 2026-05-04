# User guide

This guide walks the dashboard end to end, organized by what you do —
not by which file it lives in. If you just want to get something
working, start with [quickstart.md](quickstart.md). If you want the
internals, see [architecture/overview.md](architecture/overview.md).

Branchwork ships as one binary (`branchwork-server`) that serves both
the SPA and the HTTP/WebSocket API on `http://localhost:3100`. Every
plan is a YAML file under `~/.claude/plans/`; every task runs on its
own git branch under a per-agent supervisor daemon that survives
server restarts.

## Contents

- [Plans](#plans) — YAML schema, creating, editing, migrating from .md, project inference
- [Tasks](#tasks) — dependencies, statuses, auto-status, `produces_commit`
- [Agents](#agents) — starting, attaching, types (PTY vs stream-JSON), stopping, check agents
- [Drivers](#drivers) — Claude, Aider, Codex, Gemini — auth and how to pick
- [Git flow](#git-flow) — branch naming, diff review, merge, stale branch cleanup
- [Cost tracking & budgets](#cost-tracking--budgets)
- [CI integration](#ci-integration)
- [Auto-mode](#auto-mode) — auto-advance, auto-merge, the status pill, and the disabled Parallel toggle
- [Unattended auto-mode](#unattended-auto-mode) — Stop-hook → tree-clean gate → merge → CI → next-task pipeline
- [Notifications](#notifications)
- [Settings](#settings) — effort level, `--claude-dir`, port, webhook URL
- [Audit log](#audit-log)
- [Authentication](#authentication)

---

## Plans

A **plan** is a YAML file describing an entire piece of work — phases,
tasks, dependencies, and (optionally) a verification block. The plan
lives at `~/.claude/plans/<slug>.yaml` and is the source of truth.
Branchwork never stores plan structure in the database; it parses the
file on every read and writes back when you edit through the UI.

### Where plans appear

The **sidebar** ([screenshots/01-sidebar.png](../screenshots/01-sidebar.png))
groups plans by inferred project, splits each project into **Active**
and **Completed**, and shows progress (`done/total`, percentage,
"last modified" age) for each plan. Completed plans collapse by
default. Use the search box at the top of the sidebar to filter by
title, slug, or project.

### Creating a plan

Click **+ New Plan** in the sidebar. The dialog asks for:

- **Folder** — any directory on the host. Auto-completes from
  recently used folders. If the folder doesn't exist, Branchwork asks
  before creating it. If it isn't a git repo yet, it gets `git init`-ed
  the first time a task spawns an agent.
- **Template** (optional) — pick "From scratch" or a built-in template.
  Templates pre-fill the description with prompts that nudge the
  design agent toward a useful structure.
- **Description** — one or two sentences describing what you want
  built. A design agent reads this and writes the actual plan YAML.

Click **Create Plan**. The design agent spawns, writes the YAML to
`~/.claude/plans/<slug>.yaml`, and the file watcher pushes the new
plan into the sidebar as soon as it lands on disk. (`Cmd+Enter` /
`Ctrl+Enter` submits the form.)

See [screenshots/05-new-plan.png](../screenshots/05-new-plan.png).

### YAML schema

The canonical schema lives at the repo root in
[`plan.yaml`](../plan.yaml). The shape is:

```yaml
title: "My plan"
context: |
  Optional background that every agent sees in its prompt.
project: my-project          # optional; inferred if omitted (see below)
created_at: 2026-04-12T...   # optional; auto-set on first save
verification: |              # optional; markdown shell script for Check Plan
  cargo test --workspace
  pnpm --filter @branchwork/web test

phases:
  - number: 1
    title: "Discovery"
    description: "..."
    tasks:
      - number: "1.1"
        title: "Reproduce the bug"
        description: |
          Investigate `server-rs/src/api/plans.rs` and capture evidence.
        file_paths:                # optional; auto-extracted from description if omitted
          - server-rs/src/api/plans.rs
        acceptance: "Repro doc lands in docs/"
        dependencies: []           # task numbers that must be done first
        produces_commit: false     # optional; defaults to true
```

Field notes:

- **`title`, `phases[].number`, `phases[].title`, tasks[].number`,
  `tasks[].title`** are required. Everything else is optional with
  sensible defaults.
- **`file_paths`** are auto-extracted from `description` (regex over
  backtick-quoted and absolute paths). Provide them explicitly when
  the description is too prose-y for the regex.
- **`dependencies`** lock the **Start** button until the listed tasks
  reach `completed` or `skipped`. The button tooltip lists which
  dependencies are still unmet.
- **`produces_commit`** defaults to `true`. Set it to `false` for
  investigation-only tasks (writing repro docs, design notes, audit
  reports) so the **Merge** button is hidden and you don't get false
  "stale merge" prompts. **Discard** is still available — empty
  branches still need cleanup. See
  [design-produces-commit.md](design-produces-commit.md) for the
  rationale.
- **`verification`** is a free-form markdown block. The plan-level
  Check agent uses it as its source of truth (see
  [check agents](#check-agents)).

The full schema is enforced in [`plan_parser.rs`](../server-rs/src/plan_parser.rs).
Missing fields produce a parse warning in the sidebar (dismissable);
the rest of the plan keeps working.

### Editing a plan

The plan board ([screenshots/02-plan-board.png](../screenshots/02-plan-board.png))
edits in place:

- The plan title at the top of the board is click-to-edit.
- Each phase title is click-to-edit.
- Each task's title, description, acceptance criteria, and file paths
  are click-to-edit on the task card. Multi-line fields (description,
  acceptance) expand into a textarea on focus and save on blur or
  `Cmd+Enter`.

Edits round-trip through `PUT /api/plans/<slug>` which re-serializes
the YAML. Comments survive (serde ignores them), but unknown
top-level keys are dropped. If you've hand-edited the YAML and
something looks off after a UI save, run `git diff ~/.claude/plans/`
and re-add anything that didn't make it.

### Project inference

A plan with no explicit `project:` gets one inferred at parse time:

1. Scan `context` and task `description`s for absolute paths.
2. Match each path against directories under `$HOME`.
3. The most-frequent match wins; ties break in scan order.

Inferred projects let the sidebar group related plans and unlock the
plan-level **Check Plan** button. If inference fails the plan lists
under "Unassigned" and Check Plan is disabled with a tooltip
explaining why. Set `project:` explicitly in the YAML to override.

### Migrating from `.md` plans

Older plans were Markdown files (`## Phase 1` / `### Task 1.1` /
bullet lists). The parser still reads them, but every UI feature
that reserializes a plan (title edits, description edits, status
changes via MCP) writes YAML.

When `~/.claude/plans/` contains any `.md` files, the sidebar shows a
**Convert All to YAML** banner. Per-plan **Convert to YAML** buttons
also appear on each `.md` plan board. Both call
`POST /api/plans/<slug>/convert` (and `/api/plans/convert-all`),
which parses the Markdown, writes a sibling `.yaml`, and removes the
`.md` file. Phase/task structure is preserved; manual prose between
sections is folded into `context:` or `description:` fields as
appropriate.

Convert before hand-editing — round-tripping a `.md` plan through
the UI without converting first will leave you with both formats
on disk and the YAML wins on next reload.

---

## Tasks

A **task** is one work item under a phase. Each task has a status,
optional dependencies, optional file paths, and a one-click **Start**
button that spawns an agent on a dedicated git branch.

### Statuses

| Status | Meaning |
|--------|---------|
| `pending` | Not started. **Start** is available unless dependencies are unmet. |
| `in_progress` | An agent is (or was) working on it. **Continue** resumes; **Retry** restarts. |
| `checking` | A check agent is verifying the task. Set/cleared automatically. |
| `completed` | Done. Counts toward `doneCount` in the sidebar. |
| `skipped` | Intentionally not doing it. Counts toward `doneCount`. |
| `failed` | Agent exited unsuccessfully. **Retry** restarts. |

The status badge on each task card is click-to-cycle and right-click
opens a full menu including **Reset** (admin-only escape hatch when a
task is wedged). Status changes also fire from agents (via MCP
`update_task_status` or hooks) and from check agents (verdict).

### Dependencies

`dependencies: ["1.1", "2.3"]` on a task disables **Start** until both
listed tasks reach `completed` or `skipped`. The disabled button
shows an amber "Dependency blocked" banner inside the card listing
every unmet dependency by number. Cyclic dependencies are not
detected — the parser will accept them and the UI will simply leave
all tasks in the cycle un-startable.

### Auto-status

The file watcher checks each task's `file_paths` whenever the project
tree changes. The heuristic is intentionally narrow:

- 0 paths declared, or **none** of them exist → `pending`.
- **Any** declared path exists → `in_progress`.
- **Auto-status never marks a task `completed`.** That requires an
  agent or a human.

This caps over-eager promotion (see
[repro-navbar-false-completion.md](repro-navbar-false-completion.md)
for the bug this rule fixes). Auto writes also tag rows with
`source = "auto"`; subsequent manual changes flip `source = "manual"`
and auto-status will never overwrite them.

### `produces_commit`

Investigation tasks that only produce documentation (repro notes,
design docs, audit reports) shouldn't trigger the **Merge** banner —
their work lives in `docs/`, not on a task branch. Set
`produces_commit: false` on those tasks. The card hides **Merge**,
keeps **Discard**, and swaps the banner heading to "Review artifacts".
The server's empty-branch merge guard (HTTP 409) is still in place
as defense in depth.

### Resetting a stuck task

`Reset` from the status menu clears the task back to `pending` and
unblocks dependants. It does **not** delete the task branch — use
**Stale Branches** (see [git flow](#git-flow)) for that.

---

## Agents

An **agent** is one CLI session (claude / aider / codex / gemini)
running against one task on its own git branch. Each agent lives
inside a detached supervisor daemon (`branchwork-server session`) so
killing the dashboard server doesn't kill the agent — see
[architecture/overview.md](architecture/overview.md).

### Starting an agent

On a task card:

- **Start** (indigo) — pending tasks. Spawns a new agent on
  `branchwork/<plan>/<task>`.
- **Continue** (amber) — in-progress tasks. Resumes the previous
  session if the driver supports session IDs (Claude does), otherwise
  starts a fresh agent on the same branch.
- **Retry** (red) — failed tasks. Behaves like Continue.
- **Check** — read-only verification agent (see
  [check agents](#check-agents) below).
- **Fix CI** (red, inline with the CI badge) — only when CI failed.
  Spawns an agent on `branchwork/fix/<plan>/<task>/<run-id>` with
  the failure log baked into its prompt.

The **driver selector** dropdown above these buttons appears when
more than one driver is registered. While an agent is running it
stays visible but disabled, so you can see which driver is in flight
without it being changeable.

### Attaching to a running agent

Click any task with a live agent (or any row in the **Agents** view)
to open the **Agent Panel** ([screenshots/03-agent-terminal.png](../screenshots/03-agent-terminal.png))
on the right. The panel header shows status, mode (`pty` vs
`stream-json`), short agent ID, plan, branch, and accumulated cost.

Two tabs:

- **Output** — for `pty` agents, a live xterm.js terminal you can
  type into. For `stream-json` agents (check agents), a structured
  render of the JSON event stream — tool calls as blue `[tool-name]`
  labels, assistant text inline, a footer with duration / turn count
  / cost, and a verdict banner at the top once the agent finishes.
- **Diff** — unified diff against the base commit, file-by-file. The
  footer carries the **Discard** and **Merge** buttons (two-stage
  confirm on Discard).

### Stopping an agent

Three buttons in the panel header:

- **Finish** (emerald) — polite shutdown. For Claude this sends
  `/exit` so the model can flush state cleanly. Use this when the
  agent has produced its commit but is waiting on input.
- **Kill** (red) — `SIGKILL` on the supervisor. The on-disk PTY
  transcript and any committed work survive; uncommitted edits in
  the agent's working tree are lost.
- **Close** (×) — just deselects the panel. Doesn't touch the agent.

### Survival across server restarts

A running agent doesn't depend on `branchwork-server`. Restart the
server and reopen the page; the agent panel reattaches to the same
supervisor daemon and replays the PTY transcript from
`~/.claude/sessions/<agent-id>.log`. This is the property exercised
in step 5 of the [quickstart](quickstart.md).

If the supervisor itself died (host reboot, OOM kill), the server
detects this on startup via `cleanup_and_reattach` and marks the
agent `failed` with `stop_reason = orphaned`, broadcasting
`agent_stopped` so the card unlocks.

### Check agents

Check agents are read-only verification runs that produce a
**verdict** (`completed` / `in_progress` / `pending` plus a reason
string). They never commit. Three entry points:

- **Check** on a task card — verifies one task against its
  description, file paths, and acceptance criteria.
- **Check Phase** (on each phase) — fans out one check agent per
  unfinished task in that phase, in parallel.
- **Check All** (on the plan header) — fans out across the entire
  plan. Asks for confirmation before spawning.
- **Check Plan** (on the plan header) — runs the plan's
  `verification:` block. Disabled when the plan has no
  `verification` or no inferred project. The verdict badge
  (`✓` / `◐` / `✗`) sits inside the button and persists to
  `plan_verdicts` in the DB so it survives reload.

All three per-task entry points (**Check**, **Check Phase**,
**Check All**) build the prompt the same way — one builder, one
shape, one source of truth — so picking one over another never
flips a task's verdict for reasons unrelated to its state. The
verdict is purely a function of the working-tree file content at
`project_dir`: `completed` iff the changes described in the
acceptance criteria are visible in the tree at check time. Git
history (whether the per-task ref still resolves, what's in `git
log`, whether the merge has happened) is not consulted by the
prompt. The "did the agent actually commit anything?" signal lives
server-side at the merge gate (the empty-branch guard described in
[Merging](#merging) above) and in the auto-mode
[pause-on-uncommitted-work path](#pause-on-uncommitted-work), not
inside the LLM prompt. See
[ADR 0004](adrs/0004-unify-check-prompts.md) for the rationale.

The verdict's `status` writes back to `task_status` —
`completed` flips the task into the done-section gate,
`in_progress` keeps it active, `pending` parks it. The **Merge**
banner is gated by [`produces_commit`](#produces_commit) plus the
server-side empty-branch guard, not by the verdict reason string.

### check-agents query

The **Agents** view in the sidebar (with active count badge) shows
the agent tree across all plans. Filter by status (All / Running /
Done / Failed) or by plan. Each row shows status dot, task ID, cost,
plan name, last tool used, and current working directory. Parent
agents (e.g., a design agent that spawned a check agent) nest their
children indented under them.

---

## Drivers

A **driver** wraps one external CLI (`claude`, `aider`, `codex`,
`gemini`) so Branchwork can spawn it, parse its output, and pass it
the right flags. The set of installed drivers and their auth status
shows up in the sidebar as a compact "Driver auth status" panel.

| Driver | Binary | Cost | Verdict | Session resume | MCP auto-inject |
|--------|--------|------|---------|----------------|------------------|
| `claude` | `claude` | ✓ | ✓ | ✓ | ✓ |
| `aider`  | `aider`  | — | partial | — | — |
| `codex`  | `codex`  | — | partial | — | — |
| `gemini` | `gemini` | — | partial | — | — |

Capability flags map to UI behaviour: cost columns hide when the
driver doesn't `supports_cost`; verdict UI defers to whatever signal
the driver can parse; session resume turns **Continue** into a true
resume rather than a fresh spawn on the same branch.

### Auth flows

Each driver reports an auth state on startup:

- **`oauth`** — Claude after `claude` login (account email shown).
- **`api_key`** — env-var based (`ANTHROPIC_API_KEY` etc.).
- **`cloud_provider`** — Codex with an OpenAI org / Gemini with a
  GCP project.
- **`unauthenticated`** — installed but not logged in. Sidebar shows
  a help string telling you what to run.
- **`not_installed`** — binary not on `PATH`. Help string points at
  the install docs.
- **`unknown`** — status check is best-effort; an unknown reading is
  treated as "probably ready" so a driver isn't permanently hidden
  when its CLI changes its `--version` output.

### Picking a driver

Defaults to `claude`. Override per task in the driver dropdown on the
task card (visible whenever more than one driver is registered).
There's no "global default driver" knob — Branchwork picks the first
ready driver in registration order, which is `claude`.

Use Claude when you can — the MCP integration gives the agent
read/write access to the plan, task statuses, blocker reports, and
cost reports while it's running, which closes the loop on a lot of
otherwise-manual coordination.

### Authoring a new driver

`AgentDriver` is a small Rust trait — `binary()`, `spawn_args()`,
`format_prompt()`, `is_ready()`, `parse_cost()`, `parse_verdict()`,
`mcp_config_json()`. Register it in `DriverRegistry::with_defaults`.
A reference page for this lives at
[reference/drivers.md](reference/drivers.md) and the trait
itself is in [`server-rs/src/agents/driver.rs`](../server-rs/src/agents/driver.rs).

---

## Git flow

Every task runs on its own branch. Merge happens in the dashboard.

### Branch naming

- **Task branches**: `branchwork/<plan-slug>/<task-number>` — e.g.,
  `branchwork/architecture-docs/1.2`. Created at agent start, merged
  into the source branch (the branch you were on when the agent
  spawned, usually `main` / `master`) when you click **Merge**.
- **Fix branches**: `branchwork/fix/<plan-slug>/<task-number>/<run-id>`
  — created by the **Fix CI** button, off the failing commit SHA so
  the agent has the same tree the workflow saw.

### Reviewing the diff

The **Diff** tab in the agent panel shows a unified diff per file
with sticky file headers, hunk headers, and per-line +/- coloring.
The "All (N files)" tab concatenates everything; per-file tabs let
you focus on one. The footer carries the merge controls and the base
commit SHA so you know what you're diffing against.

### Merging

The **Merge** button on the task card (and inside the diff footer)
runs `git checkout <source>` then `git merge --no-ff <task-branch>`.
A server-side guard runs `git rev-list --count <source>..<branch>`
first; zero commits returns HTTP 409 with the literal message
`task branch has no commits — agent exited without committing`,
which the card surfaces as an inline error. This is what catches an
agent that exited cleanly without committing — the guard fires
before the checkout so your working tree isn't touched.

If the task is `produces_commit: false` the **Merge** button is
hidden entirely. If a task that should commit ended with no commits,
prefer **Continue** or **Retry** over a manual workaround — the agent
needs to actually do the work.

### Discarding

**Discard** deletes the task branch (`git branch -D`). Two-stage
confirm. Use it for failed attempts you'll redo from scratch, or for
investigation tasks where the artifact landed in `docs/` rather than
on the branch.

### Stale branches

The plan board's **Stale Branches** button opens a modal listing
every `branchwork/<plan>/*` ref it can find, with commit count, age,
and originating agent ID. Branches with zero unique commits ahead of
`main` are pre-checked. A **force** checkbox unlocks the rest. Bulk
deletion happens on the server via `git branch -D`. This is the
recommended cleanup path; manual `git branch -D branchwork/...`
works too but you'll have to figure out which branches are safe.

---

## Cost tracking & budgets

When a driver `supports_cost`, Branchwork sums the cost it reports
into per-agent, per-task, per-plan, and per-org buckets.

- **Per-task cost** shows on the task card (small yellow mono number
  next to the task ID) when greater than zero.
- **Per-plan cost** shows in the plan header alongside `done/total`.
- **Per-org cost** drives the SaaS billing path (see
  [`server-rs/src/saas/billing.rs`](../server-rs/src/saas/billing.rs)).

Plan-level **budgets** are editable inline in the plan header.
Format: `Budget: $X / $Y`. Color-codes at 80% (amber) and 100% (red).
Setting a budget to `0` clears it.

In SaaS mode, organization-level budgets and per-user quotas are
enforced as a **kill switch** — once an org exceeds its monthly cap,
new agent spawns are rejected with `budget_exceeded` until the next
billing period or until an admin raises the cap.

The `BRANCHWORK_WEBHOOK_URL` env can deliver "80% reached" / "100%
reached" notifications (see [notifications](#notifications)).

---

## CI integration

When a task branch merges and the project has `.github/workflows/`
plus a working `gh` CLI, Branchwork pushes the source branch to
`origin` to kick off the workflow, then polls every 30 seconds via
`gh run list` / `gh run view` for up to 30 minutes.

The CI badge on the task card cycles through:

- `pending` → `running` → `success` (green ✓)
- → `failure` (red ✗)
- → `cancelled` (gray) / `unknown` (gray)

Failed and cancelled badges have a small **×** that dismisses the
badge (the CI run record stays in the DB, the UI just stops showing
it).

When CI fails, the **Fix CI** button appears inline with the badge.
It calls `POST /api/actions/fix-ci`, which:

1. Fetches the failure log tail (cached at ~8 KB on first hit).
2. Creates `branchwork/fix/<plan>/<task>/<run-id>` off the failing
   commit SHA — not off `main`, so the agent sees the broken tree.
3. Spawns an agent with the task prompt **plus** the failure log
   **plus** an explicit "run `cargo fmt && cargo clippy && cargo
   test` (or the project equivalent) before committing" instruction.

When the fix agent commits, the task card's merge banner reactivates
on the fix branch.

CI is best-effort: missing `gh`, no `origin` remote, no workflows, or
permission errors all silently degrade — the merge still succeeds,
just without CI tracking.

---

## Auto-mode

Two opt-in toggles on the plan header turn a plan from "click Start,
click Finish, click Merge, click Start on the next task" into a
hands-off pipeline.

- **Auto-advance** — when one task completes, automatically start the
  next ready task in the plan. One agent at a time.
- **Auto-mode** — auto-advance **plus** auto-merge each task on
  completion, wait for CI, and spawn a fix agent on failure (up to
  `max_fix_attempts` times before pausing).

Flip them per plan via the toggles on the plan header (or
`PUT /api/plans/:name/config`).

### The auto-mode pill

When auto-mode is on, the plan header shows a small status pill that
mirrors the loop's state:

- `auto: idle` (green) — armed, no task in flight.
- `auto: merging task N` (amber) — merging the completed task into
  the source branch.
- `auto: waiting on CI` (indigo) — polling `gh run list` for the
  merged commit.
- `auto: fixing CI (attempt N/cap)` (orange) — a fix agent is running
  against the failing run.
- `auto: paused — <reason>` (red) + **Resume** — the loop hit a
  failure it can't recover from (`merge_conflict`,
  `agent_left_uncommitted_work`, `budget_exceeded`, …). Click
  **Resume** to clear the pause and re-evaluate from the last
  completed task. The reason is persisted on the plan config so a
  fresh page load still shows the paused pill.

A third **Parallel** switch sits next to the Auto-advance / Auto-mode
toggles but is currently disabled — see
[Parallel auto-advance](#parallel-auto-advance) for why.

### Parallel auto-advance

Auto-advance is **sequential by default** — one task agent per plan
at a time. Parallel mode would let independent sibling tasks run
concurrently, but it requires worktree-per-agent isolation (each
agent on its own checkout) to avoid interleaved diffs on a shared
working tree. Until that lands, the parallel toggle on the plan
board is disabled and any API attempt to set `parallel=true` is
rejected with `412 worktrees_required`.

---

## Unattended auto-mode

[Auto-mode](#auto-mode) above describes the toggle and the pill;
this section describes what actually happens between
"agent finishes its turn" and "next task spawns" once the toggle is
on. The end-to-end loop runs without a human at the keyboard — no
Finish click, no Merge click, no Start click on the next task — for
any plan whose driver supports a Stop hook (today: `claude`).

### End-to-end flow

When a Claude agent under auto-mode finishes its turn:

1. **Agent finishes its turn.** The model emits its final message
   and would otherwise sit at the prompt waiting for input.
2. **Stop hook fires.** A per-session settings file written at
   spawn time (`~/.claude/sessions/<agent-id>.settings.json`,
   passed via `claude --settings`) tells Claude Code to POST every
   `Stop` event to `http://localhost:<port>/hooks`. The receiver
   is `server-rs/src/hooks.rs::receive_hook`.
3. **Tree-clean gate.** Branchwork runs `git status --porcelain`
   in the agent's working directory. Tracked-modified, deleted, or
   unmerged paths trip the gate; untracked files (the agent's
   scratchpad) are tolerated. A dirty tree pauses the plan with
   reason `agent_left_uncommitted_work` (see
   [Pause on uncommitted work](#pause-on-uncommitted-work)) and
   leaves the agent at the prompt for human review. A clean (or
   unknown) tree falls through to step 4.
4. **`graceful_exit`.** Branchwork sends the driver's polite exit
   sequence (`/exit\r` for Claude) to the PTY. The agent flushes
   and the PTY closes — the same path a human Finish click takes,
   but with the audit row written as
   `agent.auto_finish` + `{"trigger": "stop_hook"}` instead of
   `agent.finish`.
5. **`on_agent_exit`.** The merge-on-completion hook in
   `server-rs/src/agents/pty_agent.rs` notifies the auto-mode
   loop that the task is ready for merge.
6. **Auto-mode merges.** The loop checks out the source branch
   (usually `main`/`master`), runs `git merge --no-ff
   <task-branch>`, and broadcasts `auto_mode_merged`. The empty-
   branch merge guard (HTTP 409) and conflict detection (pause
   with `merge_conflict`) are the same as for a manual Merge
   click.
7. **CI gate.** When the project has GitHub Actions and a working
   `gh` CLI, the source branch is pushed and the loop polls
   `gh run list` for up to 30 minutes. On failure, a Fix CI agent
   is spawned automatically (up to `max_fix_attempts`); on
   exhaustion, the loop pauses with `ci_failed`. CI tooling is
   best-effort — projects without `gh`/workflows fall through and
   the loop advances.
8. **`try_auto_advance`.** With the merge committed (and CI green
   if applicable), the loop scans the plan for the next ready task.
   It scans the **current phase first**, broadcasting
   `task_advanced` if a sibling task is now ready in the same phase.
   When the current phase is fully done, it falls through to the
   next-phase scan and broadcasts `phase_advanced`. Either path
   spawns one new agent and the cycle restarts at step 1.

The audit log records every system-driven Finish as
`agent.auto_finish` with a trigger badge (`stop_hook` or
`idle_timeout`) so a post-mortem can tell unattended runs apart
from the manual `agent.finish` rows.

### Enabling unattended auto-mode

Flip the **Auto-mode** toggle on the plan header. That single
switch enables the full end-to-end loop for any task whose driver
returns a non-`None` `stop_hook_config` — today only `claude`. The
setting is per-plan and persists in the database, so you can leave
it on for plans you trust to run unattended and off for plans where
you want to review each task before it merges.

There is no global "default to auto-mode" knob. Opting in is an
explicit per-plan decision that survives server restarts and
shows in `plan_updated` WS events.

For non-Claude drivers, see
[Drivers without a Stop hook](#drivers-without-a-stop-hook) below —
unattended operation needs an extra opt-in.

### Failure modes

#### Pause on uncommitted work

If the Stop hook fires while the agent's working tree has tracked
changes, Branchwork pauses the plan instead of merging an
incomplete branch:

- The auto-mode pill flips to **paused — agent left uncommitted
  work** (red) with a **Resume** button.
- A red banner above the phase board lists the dirty paths and
  exposes an **Inspect agent** button that opens the agent's
  terminal panel. The task agent is still `running` (the dirty-
  tree branch never calls `graceful_exit`), so you can pick up
  exactly where the agent left off.

To resolve:

1. Click **Inspect agent** to open the agent terminal.
2. Either **commit** the work (drive the agent to commit, or
   `git add` + `git commit` from your own shell against the
   agent's working directory) **or discard** it (`git restore .`,
   or delete the offending files). Untracked scratchpad files
   never trip the gate, so you don't need to clean those up.
3. Click **Resume** on the auto-mode pill. Branchwork clears
   `pausedReason`, re-evaluates from the last completed task, and
   the loop picks up at step 4 of the [end-to-end flow](#end-to-end-flow).

If you'd rather take the task over by hand: kill the agent, finish
the changes manually, and click the regular **Merge** button.
Auto-mode stays paused until you click Resume — it won't grab the
next task while you're driving.

#### Drivers without a Stop hook

Only the `claude` driver returns a non-`None` `stop_hook_config`
today. `aider`, `codex`, and `gemini` have no settings-driven Stop
hook, so the PTY never closes on its own and step 2 of the flow
above never fires.

| Driver | `stop_hook_config` | Auto-finish path |
|---|---|---|
| `claude` | `Some(...)` | Stop hook (deterministic) |
| `aider`  | `None`      | Idle timer (opt in) |
| `codex`  | `None`      | Idle timer (opt in) |
| `gemini` | `None`      | Idle timer (opt in) |

Two ways to run those drivers unattended:

- **Idle-timer fallback (opt in).** Set
  `BRANCHWORK_AUTO_FINISH_IDLE=1` on the server before launch (and
  optionally `BRANCHWORK_AUTO_FINISH_IDLE_SECS=<seconds>`, default
  `300`). A background poller scans every running auto-mode agent
  whose driver returned `None` from `stop_hook_config` and treats
  any agent idle past the threshold as done — same tree-clean
  gate, same `graceful_exit`, same merge / CI / advance loop. The
  audit trigger reads `idle_timeout` instead of `stop_hook`. Both
  variables are documented in
  [reference/configuration.md](reference/configuration.md#auto-mode-idle-finish).
- **Stay manual.** Leave the env var unset and click **Finish**
  yourself when the driver finishes its turn. Auto-mode still
  merges, watches CI, and advances after that — only the Finish
  click is manual.

The fallback is **off by default** because a driver with no
activity telemetry can fire prematurely. Driver-specific Stop-hook
support is the long-term fix; per-driver follow-up backlog plans
live at `~/.claude/plans/backlog/auto-mode-stop-hook-<driver>.yaml`.
See
[reference/drivers.md](reference/drivers.md#stop-hooks-and-unattended-auto-mode)
for the trait-level contract.

### Manual escape hatch

Clicking **Finish** in the agent panel under auto-mode works
**exactly the same as before**. The Stop-hook path and the manual
Finish click both call into `AgentRegistry::graceful_exit`; the
only observable difference is the audit-log action constant
(`agent.auto_finish` with a `trigger` field, vs. plain
`agent.finish`). If you'd rather drive a task by hand while
auto-mode stays armed for the next one, Finish it yourself — the
loop picks up from step 5 onward and merges, watches CI, and
advances as usual.

The same applies to **Kill**: a SIGKILL'd agent leaves the plan
in whatever state the half-merged tree implies (auto-mode will
typically pause on the next scan), and **Resume** is the way back
in once you've cleaned things up by hand.

---

## Notifications

`--webhook-url <url>` (or the `BRANCHWORK_WEBHOOK_URL` env) accepts a
Slack incoming webhook or any JSON-accepting endpoint. The payload
is `{"text": "..."}` so Slack works out of the box; other endpoints
will see the same shape.

Triggers:

- Agent completion (per task: status, cost, branch).
- Phase advance (when a phase fully completes and the next opens).

Email notifications use SMTP env vars (`SMTP_HOST`, `SMTP_PORT`,
`SMTP_USER`, `SMTP_PASSWORD`, `SMTP_FROM`) and are reserved for
budget alerts in SaaS mode. The reference page at
[reference/configuration.md](reference/configuration.md) _(stub)_
will enumerate them all.

If neither is set, notifications silently no-op. Webhook calls have a
5-second timeout and errors are logged but never surface in the UI —
they're best-effort by design.

---

## Settings

There is no "settings page" in the app — every setting is either a
CLI flag, an environment variable, or a one-knob control on the
sidebar.

### Sidebar controls

- **Effort selector** (Low / Medium / High / Max) — sent to every
  newly spawned agent and recorded as the user's default. Stored via
  `PUT /api/settings`.
- **Driver auth status** — read-only; expanding a driver shows the
  help text for fixing its auth state.

### CLI flags (`branchwork-server`)

| Flag | Default | Notes |
|------|---------|-------|
| `--port` | `3100` | HTTP/WS listen port. |
| `--effort` | `high` | Default effort for new agents (overridable per task). |
| `--claude-dir` | `~/.claude` | Holds `plans/`, `branchwork.db`, `sessions/`. |
| `--webhook-url` | unset | Slack-compatible webhook for completion / phase events. Also reads `BRANCHWORK_WEBHOOK_URL`. |

Subcommands (`branchwork-server <subcommand>`):

- `session` — internal supervisor daemon, spawned by the server
  itself. Not for end users.
- `mcp` — serve the Branchwork MCP tools over stdio (for Claude Code
  configured to spawn it as a child process). The same MCP handler
  is also mounted at `/mcp` on the HTTP listener.

A full per-flag reference is in
[reference/cli.md](reference/cli.md) _(stub)_; environment variables
in [reference/configuration.md](reference/configuration.md) _(stub)_.

### `~/.claude/` layout

```
~/.claude/
├── plans/                # YAML (and legacy .md) plan files — source of truth
│   └── <slug>.yaml
├── branchwork.db         # SQLite: agents, task_status, cost, audit, outbox
├── sessions/             # one socket + log + pid per agent
│   ├── <agent-id>.sock
│   ├── <agent-id>.log    # PTY transcript, replayed on reattach
│   ├── <agent-id>.pid
│   └── <agent-id>.mcp.json   # auto-injected for Claude agents
└── settings.json         # Claude Code's own settings (Branchwork doesn't write here)
```

Branchwork writes `plans/`, `branchwork.db`, and `sessions/`. It
reads `settings.json` only to confirm Claude Code's hook config (see
the historical Phase 4.1 work on hooks).

---

## Audit log

The **Activity** view in the sidebar (icon: clipboard) opens an
audit log of every state-changing action: agent starts and stops,
task status transitions, branch merges and discards, plan creates
and edits, budget changes, member roles (in SaaS mode).

Filter by action type or resource type. Export to CSV. The table is
paginated (50 per page) and updates live via `audit_log` WS events.
Each row shows the human-readable diff (`pending → completed`,
`branch merged into main`, etc.) so you don't have to dig into IDs
to see what happened.

---

## Authentication

Self-hosted Branchwork runs unauthenticated by default — anyone who
can reach the port has full access. SaaS mode (`branchwork-runner`
plus the hosted dashboard) requires login.

The login page handles email + password sign-up / sign-in plus SSO
discovery: when you type an email whose domain has an SSO provider
configured, the form swaps to "Continue with <IdP>" and SAML / OIDC
takes over. JIT provisioning creates the user on first SSO login.
Errors come back as URL params (`?sso_error=...`) and render with
humanized text on the login page.

Cookies are HttpOnly + SameSite=Lax. Sessions rotate on login. Org /
member / SSO / runner-token endpoints all live under `/api/orgs/...`
— see the operations docs for runner token issuance and SSO
configuration.

---

## See also

- [quickstart.md](quickstart.md) — five-minute install + first plan.
- [architecture/overview.md](architecture/overview.md) — the three
  binaries, protocols, and what survives a restart.
- [architecture/persistence.md](architecture/persistence.md) — SQLite
  schema, on-disk artifacts, and the restart matrix.
- [reference/plan-schema.md](reference/plan-schema.md) _(stub)_ —
  field-by-field plan YAML reference, supersedes the inline schema
  in this guide.
- [reference/drivers.md](reference/drivers.md) — per-driver reference
  for `claude`/`aider`/`codex`/`gemini`, the `AgentDriver` trait,
  `DriverCapabilities`, and MCP auto-injection.
- [troubleshooting.md](troubleshooting.md) _(stub)_ — common
  failures and how to fix them.
- [design-produces-commit.md](design-produces-commit.md) — why the
  Merge button is gated on `produces_commit`.
