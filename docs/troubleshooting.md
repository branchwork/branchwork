# Troubleshooting

FAQ-shaped, grouped by symptom. Each entry: what you see → likely
cause → fix → where to read more. The deep references live elsewhere
in this docs tree; this page is the index you reach for when something
visible is wrong.

If your symptom is not listed, the four most useful things to check
are usually:

1. The **dashboard server log** — every `eprintln!` and audit row
   passes through `branchwork-server`'s stderr.
2. The **per-agent transcript** — `<sockets_dir>/<agent_id>.log` (the
   on-disk log the [session daemon](architecture/session-daemon.md)
   writes; never truncated).
3. **`GET /api/agents`** and **`GET /api/plans/<name>`** — the
   authoritative state. WebSocket events are derived; the API is the
   source of truth.
4. The **WS event stream** in your browser's devtools (filter by
   "ws"). Most dashboard surprises are a missing handler, not a
   missing event.

## Plans and tasks

### Task shows "completed" but no agent ever ran

**Symptom.** A task is grouped under Done with a green status pill, but
`agents` is empty for `(plan, task)` and the task body looks untouched.

**Cause.** The legacy file-existence heuristic (`auto_status::infer_status`)
seeded `task_status` rows whenever ≥ 80 % of `file_paths` already
existed on disk. Branches that referenced long-lived core files
(e.g. `server-rs/src/api/plans.rs`) flipped to `completed` without
any work happening.

**Fix.** This was repaired in two steps and should not recur on a
fresh DB:

1. The heuristic is now capped at `in_progress` — it can no longer
   write `completed`. Only an explicit user/agent action transitions a
   task to `completed`.
2. Inferred rows are stored with `source = 'auto'`. The boot-time
   `cleanup_stale_auto_completed` sweep deletes any legacy
   `(status='completed', source='auto')` rows from older databases.

If you upgraded from before the cap and still see false-positive
"completed" tasks, restart the server once: `db::migrate` runs
`cleanup_stale_auto_completed` on every boot.

**See also.**
[Repro: navbar plan completion](repro-navbar-false-completion.md) ·
[architecture/server.md § Auto-status](architecture/server.md#auto-status).

### Navbar groups a plan under "Done" while a task is still in progress

**Symptom.** The plan is in the Done section of the sidebar (or
collapsed under the project dashboard's Done fold), but opening it
shows one or more tasks visibly `in_progress` / `failed` / `pending`.

**Cause.** Frontend `doneCount` drift in `patchTaskStatus`. The
optimistic delta used to be unsigned (`+1` for transitions to
`completed` / `skipped`, `0` for everything else, never `-1`), so
toggling a task back out of done left the cached count stuck. Repeated
events also double-counted re-entries.

**Fix.** Already shipped:

- `patchTaskStatus` ([`web/src/stores/plan-store.ts`](../web/src/stores/plan-store.ts))
  now computes a signed delta `(isDone ? 1 : 0) - (wasDone ? 1 : 0)`
  by looking up the previous status on `selectedPlan`.
- `task_status_changed` events in
  [`web/src/stores/ws-store.ts`](../web/src/stores/ws-store.ts) share
  the 2 s debounce timer with `plan_updated` and trigger a server-
  authoritative `fetchPlans()` so any residual drift converges within
  the debounce window.

If the issue reappears, hard-reload the dashboard: `GET /api/plans`
recomputes `doneCount` from the DB on every call and is the only
source of truth.

**See also.** [Repro: plan-done drift](repro-plan-done-drift.md).

### Plan file edited on disk but the dashboard does not pick it up

**Symptom.** You edited a YAML in `~/.claude/plans/` (added a task,
fixed a typo) but the dashboard still shows the old content even
after a hard refresh.

**Likely causes.**

1. **You edited under a subdirectory.** The file watcher
   ([`file_watcher.rs`](../server-rs/src/file_watcher.rs)) is
   non-recursive and filters events by
   `path.parent() == plans_dir`. The archive subdir
   (`plans/archive/`) is intentionally ignored. If your edit landed
   in any other subdir, move it back to the top of `plans_dir`.
2. **Wrong extension.** Only `*.yaml`, `*.yml`, and `*.md` are
   watched; a save-as-`.txt` or editor backup like `*.yaml~` is
   ignored.
3. **Soft-deleted by a concurrent UI delete.** Look in
   `plans/archive/<name>.<utc>.yaml`; the SPA's delete moves files
   into that subdir, where the watcher does not pick them up.
4. **Atomic-write rename trips the debounce.** Editors that write to
   a temp file and `rename(2)` it over the target produce a single
   modify event after a short coalesce window; this is normal. Wait
   ~1 s after save, then refresh.

**Fix.** When in doubt, restart `branchwork-server`. The plan list is
re-parsed from disk on every `GET /api/plans`, and `db::migrate` is
idempotent so a restart is safe.

**See also.**
[architecture/server.md § Plan files: parser and watcher](architecture/server.md#plan-files-parser-and-watcher).

## Merge button and branches

### Merge button shown on a task that didn't commit anything

**Symptom.** A finished agent left an indigo merge banner on the task
card. Clicking **Merge** returns 409 with body
`task branch has no commits — agent exited without committing`. The
button does not disappear; clicking again returns the same 409.

**Cause.** The banner gate in
[`web/src/components/TaskCard.tsx`](../web/src/components/TaskCard.tsx)
fires whenever the task has a non-running agent with a `branch`. For
genuine no-commit tasks (investigation, repro, design-only work),
this is exactly the wrong UX — the button can never succeed.

**Fix.** Set `produces_commit: false` on the task in the plan YAML.
The default is `true`, so existing plans keep today's behaviour; only
explicit `false` hides the Merge button and swaps the banner copy to
"Review artifacts". The Discard button stays — an empty branch still
needs cleanup.

```yaml
- number: "0.1"
  title: Reproduce the bug
  produces_commit: false
  description: |
    Investigation only — no commit expected.
```

The server-side 409 guard at
[`server-rs/src/api/agents.rs::merge_agent_branch`](../server-rs/src/api/agents.rs)
stays in place as defense-in-depth: a `produces_commit: true` task
whose agent happens to exit clean still gets the same 409.

**See also.**
[Design: produces_commit](design-produces-commit.md) ·
[Repro: stale merge button](repro-stale-merge-button.md) ·
[reference/plan-schema.md](reference/plan-schema.md).

### Merge picked the wrong target branch

**Symptom.** Merge succeeded but landed on a stale branch (e.g. an
older feature branch left checked out by a previous agent) instead of
`master` / `main`.

**Cause.** `start_pty_agent` records `source_branch` from
`git_current_branch(cwd)` at spawn time. If the working tree was
sitting on a stale branch, that branch becomes the merge target.

**Fix.** Three improvements compose into the modern resolver, all
already shipped:

1. `git_default_branch(cwd)` probes `origin/HEAD` first, then falls
   back to local `master` / `main`. Local-only — never fetches.
2. The merge resolver prefers an *explicit* `merge_target` on the
   plan/task if it resolves; otherwise it picks the canonical default
   from above; the agent's `source_branch` is the third fallback.
3. `branchwork-server` checks the working tree before spawn and
   restores the canonical default branch when the previous agent left
   it elsewhere.

If you still see a stale target after a clean restart, set an
explicit `merge_target:` on the task or run `git remote set-head
origin --auto` so `origin/HEAD` resolves.

**See also.**
[Algorithm: default-branch resolution](algo-default-branch-resolution.md) ·
[Trace: source_branch capture and merge resolution](trace-merge-target-resolution.md).

### Discard button does nothing / leaves a stale branch

**Symptom.** You click Discard but the local branch stays around.

**Cause.** Discard deletes the *local* branch via `git branch -D` and
clears `agents.branch` to `NULL`. It does not push the deletion or
prune remote tracking refs.

**Fix.** Run `git fetch --prune origin` (or whatever your remote is)
to clean up any remote-tracking copy. If you want every stale local
`branchwork/*` branch swept at once, use the dashboard's
**Stale branches** modal (PlanBoard header) or
`POST /api/plans/<name>/clear-stale-branches`.

## Agents and drivers

### Driver auth fails / "unauthenticated"

**Symptom.** The driver chip on the task card shows a red
`unauthenticated` badge, or `GET /api/drivers` returns
`auth_status.kind = "unauthenticated"`. Start is disabled.

**Likely causes (per driver).**

- **Claude.** The probe order is: `ANTHROPIC_API_KEY` →
  `CLAUDE_CODE_USE_BEDROCK` → `CLAUDE_CODE_USE_VERTEX` →
  `~/.claude/.credentials.json`. The env vars short-circuit the file,
  so a stale env var shadows valid OAuth credentials. Run `claude` in
  a terminal once to refresh the credentials file, or unset the env
  var and restart the server (driver auth is probed at startup, not
  per request).
- **Aider.** Needs `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` (and
  optionally `DEEPSEEK_API_KEY` / `GEMINI_API_KEY` /
  `GOOGLE_API_KEY`). The runner-side probe in
  [`bin/branchwork_runner.rs`](../server-rs/src/bin/branchwork_runner.rs)
  only looks at `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`; the others
  are read by the driver itself when it spawns. Cosmetic gap on the
  /runners chip, not a runtime issue.
- **Codex / Gemini.** Need their respective vendor CLIs to have
  authenticated separately (`codex login`, `gemini login`). Branchwork
  does not store these credentials.

**Fix.** Authenticate the underlying CLI directly, then restart
`branchwork-server`. In SaaS, restart the runner — it caches driver
auth in its `RunnerHello` payload at connect time and only re-reports
on connect. **Do not** put your provider key in a project `.env` and
expect Branchwork to read it; the server reads from its own process
environment.

**See also.**
[reference/drivers.md § Auth](reference/drivers.md) ·
[reference/configuration.md](reference/configuration.md) for the
canonical env-var list.

### Agent stuck in "starting" / "supervisor_unreachable"

**Symptom.** Agent row shows `status='starting'` and never advances,
or shows `status='failed', stop_reason='supervisor_unreachable'`.

**Likely causes.**

- The CLI binary is not on `PATH`. The supervisor invokes it directly
  (no shell), so `which claude` (or `aider` / `codex` / `gemini`)
  must succeed in the shell that started `branchwork-server`. A
  per-shell `nvm` / `pyenv` shim that only loads in interactive zsh
  will NOT be visible to a launchd service.
- The session daemon was SIGKILLed (OOM, panic) and left
  `<socket>.pid` behind without unlinking the socket. The next
  `pty_agent::on_agent_exit` reads the orphaned pidfile and writes
  `failed / supervisor_unreachable` — that is exactly what is
  supposed to happen, the agent is gone.
- A 45 s heartbeat lapsed. Self-healing — the next heartbeat tick
  marks the row failed; just re-Start the task.

**Fix.** Re-Start the task. The agent row is recreated on a fresh
branch (`branchwork/<plan>/<task>`); the old row stays for audit. If
this happens repeatedly, check `<sockets_dir>/<agent_id>.log` for
the underlying CLI error.

**See also.**
[architecture/session-daemon.md § Exit and cleanup](architecture/session-daemon.md#exit-and-cleanup) ·
[architecture/persistence.md § Restart matrix](architecture/persistence.md).

### Session terminal shows blank after reconnect

**Symptom.** Closing and re-opening the agent terminal panel renders
nothing. The agent is still running; clicks/keystrokes work but
historical output is missing.

**Likely causes.**

- The agent is **stream-JSON mode** (a check agent or driver that
  doesn't use a PTY). Those rows never reattach across server
  restarts and have no `<socket>.log` to replay; the panel is blank
  by design once the source process disconnects.
- A yellow banner reading
  `--- terminal detached (server restarted while agent was running) ---`
  was shown. That marks the gap between the server crash and the
  reattach: the agent kept running and wrote to `<socket>.log` on
  disk, but those bytes never made it into the `agent_output` table
  the dashboard replays from. The live broadcast continues from the
  reattach point.
- Slow client → the broadcast lagged the subscriber and dropped
  frames. Reconnect — the next `/terminal` WS handshake replays from
  `agent_output` (server-captured) before resubscribing.

**Fix.** For PTY agents, the on-disk `<socket>.log` always has the
full transcript (the daemon never truncates it). If you need to see
what the agent printed during a crash gap, `cat` it directly — it
sits next to `<socket>` and `<socket>.pid` under
`<sockets_dir>` (default `~/.claude/sessions/`). For stream-JSON
agents, kill and re-Start.

**See also.**
[architecture/session-daemon.md § Reconnect / replay across server restarts](architecture/session-daemon.md#reconnect--replay-across-server-restarts) ·
[architecture/persistence.md § Per-agent sibling files](architecture/persistence.md).

### Session terminal shows garbled output / characters at wrong x-positions

**Symptom.** The terminal pane renders characters in the wrong
columns, frozen spinner frames stick in scrollback, or a cascade of
`▀` glyphs duplicates across line breaks. Often appears after
collapsing the sidebar or docking devtools mid-session.

**Cause.** PTY-vs-viewport geometry mismatch + mid-stream join into
a DEC 2026 (Synchronized Output) frame. The dedicated page below
has the verbatim log evidence, the two-layer fix (server-side
spawn-time grace + client-side reset+Ctrl+L on resize), the repro
recipe, and the regression-checklist.

**Fix.** Already mitigated by Tasks 4.1 + 4.2 in the
`dashboard-stability` plan. If the symptom comes back, walk the
"What to check first if it regresses" list in the deep-dive page.

**See also.**
[troubleshooting/terminal-rendering.md](troubleshooting/terminal-rendering.md) ·
[architecture/session-daemon.md](architecture/session-daemon.md).

### Agent committed to the wrong branch (or to the source branch)

**Symptom.** The agent left commits on `master` (or whatever
`source_branch` was) instead of its task branch.

**Cause.** The agent's first action was a commit before checking out
its task branch. This is rare with the unattended-execution contract
(the agent prompt explicitly tells it which branch it is on) but
possible with custom prompts.

**Fix.** Use `git log master..branchwork/<plan>/<task>` to see what
landed where. If commits ended up on `source_branch` itself, the
recovery is `git reset --hard <pre-agent-sha>` on `source_branch`
followed by `git cherry-pick` onto the task branch. Branchwork does
not auto-recover from this — the merge guard's empty-branch rejection
is the only signal.

## SaaS runner

### Runner won't connect

**Symptom.** The dashboard's /runners page shows the runner offline
(or never connected). The runner host's process logs show repeated WS
errors.

**Likely causes (in order of frequency).**

1. **`401 token_already_claimed`.** The first runner that connected
   with this token is recorded in `runner_tokens.claimed_runner_id`,
   and a different `runner_id` cannot reuse it. Symptoms: you copied
   the install command to a second host, or rebuilt the first host
   without preserving `~/.branchwork-runner/runner.db`. Fix: mint a
   fresh token from the dashboard's enrollment modal.
2. **Network egress blocked.** The runner only needs *outbound* WSS
   to the dashboard URL. No inbound, no extra ports. Test with
   `curl -i https://app.branchwork.dev` from the runner host. If a
   corporate proxy intercepts WSS, set `HTTPS_PROXY` /
   `https_proxy`.
3. **Wrong URL.** The runner's `--saas-url` /
   `BRANCHWORK_SAAS_URL` must be the *dashboard* URL (the same one
   you log into in a browser), not an internal hostname. The runner
   upgrades that URL to `wss://…/ws/runner` automatically.
4. **Token revoked.** The dashboard's `Revoke` button or
   `DELETE /api/runners/{id}` deletes every `runner_tokens` row for
   that runner. Mint a fresh token.

**Fix.** Read the runner stderr — the WS upgrade error includes the
status code (`401 token_already_claimed`, `403`, `404 not found`,
network errors). For the most common cases, mint a fresh token from
the **+ Add runner** modal on `/runners` and re-run the install
command on a clean host.

**See also.**
[operations/saas-runner.md § Issue a runner token](operations/saas-runner.md#issue-a-runner-token) ·
[architecture/runner.md § Reconnect and ID claiming](architecture/runner.md).

### Runner connected but agents never spawn

**Symptom.** The runner shows online on /runners but clicking Start
produces no PTY output and the agent row stays in `starting`.

**Likely causes.**

- The runner host can't find the underlying CLI (`claude`, `aider`,
  …) on `PATH`. `branchwork-server session` is a child process, so
  the runner's PATH at startup is what matters — not your interactive
  shell. For systemd, set `Environment="PATH=…"` on the unit; for
  launchd, set `EnvironmentVariables.PATH` in the plist.
- The runner's working directory does not contain a git repo. The
  spawn pipeline shells `git rev-parse` — verify with
  `cd <project> && git status` on the runner host.
- The driver is not authenticated on the runner host (see
  [Driver auth fails](#driver-auth-fails--unauthenticated) above —
  the same probe runs on the runner).
- A `WireMessage::StartAgent` was queued in the outbox while the
  runner was disconnected and the dashboard has not yet flushed it.
  The runner replays on reconnect; wait ~1 s.

**Fix.** SSH to the runner host, run the CLI by hand from the
project's `cwd`, and confirm it starts cleanly. Then restart the
runner — driver auth is re-probed on each WS connect.

**See also.**
[operations/saas-runner.md](operations/saas-runner.md) ·
[architecture/runner.md § Outbox and replay on reconnect](architecture/runner.md).

### Auto-mode plan is paused on the runner side

**Symptom.** Auto-mode pill shows "Paused — runner offline". Re-pinning
to a different runner has no effect.

**Cause.** The plan is pinned to a specific runner via
`plan_runner_affinity`, that runner went offline, and the failover
policy is `pause` (the default). Silent fan-out to another runner
would risk running the next task against a different filesystem
state.

**Fix.** Either (a) bring the original runner back online (the pause
clears automatically when re-pinning), or (b) repin the plan to a
different runner from the plan board's runner picker. If your plan
genuinely can fail over to siblings (shared `cwd` over NFS, or a
`git remote` is enough), set `runnerFailover: 'sibling'` via
`PUT /api/plans/{name}/config`.

**See also.**
[operations/saas-runner.md](operations/saas-runner.md) ·
[user-guide.md § Auto-mode](user-guide.md#auto-mode).

## CI and auto-mode

### Auto-mode paused with `agent_left_uncommitted_work`

**Symptom.** The plan board shows an amber banner: "Paused: agent
left uncommitted work". Auto-advance stops.

**Cause.** The Stop hook fired on a clean agent exit, but
`git status --porcelain` reported tracked-but-modified files in the
project working tree. Auto-mode refuses to merge a half-done branch.

**Fix.** Click **Inspect agent** in the banner; the right-rail
opens to the agent that triggered the pause. Either commit / discard
the dirty files manually, then click **Resume** in the auto-mode pill.
Untracked files are tolerated; tracked-modified are not.

**See also.**
[user-guide.md § Unattended auto-mode](user-guide.md#unattended-auto-mode) ·
[ADR 0003](adrs/0003-unattended-auto-mode.md).

### Auto-mode keeps spawning fix agents but CI stays red

**Symptom.** The pill cycles `fixing_ci → awaiting_ci → fixing_ci` for
attempts 1 / 2 / 3, then the plan pauses on `ci_failed: <run_id>`.

**Cause.** Working as intended. The retry cap defaults to 3 (settable
per plan via `maxFixAttempts` in the auto-mode panel). The third red
CI does not spawn a fourth fix agent — the loop pauses so a human
can read the failure log and intervene.

**Fix.** Open the failing CI run from the task card, fix manually,
and click **Resume** on the pill. Or raise `maxFixAttempts` if you
trust the model to keep trying — but at that point a fresh approach
is usually faster than another retry.

### CI workflow is "blocking" or "informational" but I expected the opposite

**Symptom.** Auto-mode advances despite a red CI run, or pauses on a
deploy-only failure that should not gate.

**Cause.** Workflow classification follows a four-layer precedence:

1. Per-phase `phase_ci_blocking_workflows`.
2. Per-plan `ci_blocking_workflows` (top-level YAML).
3. Repo-level `branchwork.toml` `[ci] blocking_workflows`.
4. Smart classifier — `(?i)docker|deploy|publish|release|bench|fuzz`
   match → informational; everything else → blocking.

**Fix.** Set an explicit allowlist at the most specific layer that
matches your intent. Empty list (`ci_blocking_workflows: []`) is the
"opt out completely" form — every workflow becomes informational and
the gate is vacuously green.

**See also.**
[reference/plan-schema.md § CI workflow filter](reference/plan-schema.md) ·
[reference/branchwork-toml.md](reference/branchwork-toml.md) ·
[ADR 0006](adrs/0006-phase-verify-and-ci-filter.md).

## MCP and external clients

### `claude` doesn't see the Branchwork MCP tools

**Symptom.** Inside an agent session, `/tools` does not list
`update_task_status` / `list_plans` / etc., or `claude` warns "MCP
config rejected".

**Likely causes.**

- The agent was spawned with `mcp_config_json()` returning empty
  (non-Claude driver). Only the Claude driver auto-injects MCP
  config today.
- The per-agent `<agent_id>.mcp.json` file was not written (server
  was OOM-killed during spawn). Re-Start the task.
- The agent is not Claude. Aider / Codex / Gemini have no MCP
  integration in Branchwork — see
  [reference/drivers.md](reference/drivers.md).

**Fix.** Confirm the file is at
`<sockets_dir>/<agent_id>.mcp.json` and contains
`{"mcpServers": {"branchwork": {"url": "http://127.0.0.1:<port>/mcp"}}}`
(plus the runner variant if SaaS). If you want to test the MCP
handler directly, point a stdio-capable client at
`branchwork-server mcp` and call `list_plans`.

**See also.**
[architecture/server.md § MCP](architecture/server.md) ·
[bob-shell-integration.md](bob-shell-integration.md) for one
external-client setup that works.

## Historical investigations

These notes were written to drive specific bug fixes. They live next
to the architecture docs because the file-line citations are still
useful when a regression rears its head.

| Note | Symptom it documents |
|------|----------------------|
| [design-produces-commit.md](design-produces-commit.md) | Per-task `produces_commit` field that gates the Merge button. |
| [repro-navbar-false-completion.md](repro-navbar-false-completion.md) | Auto-status file-existence heuristic produced false-positive `completed` rows. |
| [repro-plan-done-drift.md](repro-plan-done-drift.md) | Frontend `doneCount` drift in `patchTaskStatus` (one-way delta, no refetch). |
| [repro-stale-merge-button.md](repro-stale-merge-button.md) | Merge banner firing on task branches with zero commits. |
| [algo-default-branch-resolution.md](algo-default-branch-resolution.md) | The `git_default_branch(cwd)` helper algorithm — `origin/HEAD`, then local `master` / `main`. |
| [trace-merge-target-resolution.md](trace-merge-target-resolution.md) | Four call sites that compose merge-target resolution: `start_pty_agent` → `merge_agent_branch` → resolver → CI insert. |

The remaining files in `docs/` (`build-perf-2026-05-05-baseline.md`,
`bob-shell-integration.md`) are not bug-fix artifacts and stay outside
this index — the build-perf doc is a baseline for an optimisation
plan, and the Bob Shell guide is linked from
[`README.md`](README.md) under Integrations.
