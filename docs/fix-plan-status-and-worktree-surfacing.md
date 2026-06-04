# Fix plan — task status correctness + worktree surfacing

**Date:** 2026-06-04
**Trigger:** `dag-based-plan-model` plan showed nonsensical state — tasks
stuck `in_progress` with finished agents and ready branches, killed agents
appearing as running, no worktree visibility.

## Diagnosis (verified against live server :3100 + `~/.claude/branchwork.db`)

Observed state for `dag-based-plan-model`:

| Task | task_status | agent.status | merge_status        | branch/worktree |
|------|-------------|--------------|---------------------|-----------------|
| 2.1  | completed   | killed       | –                   | gone (merged)   |
| 2.2  | completed   | killed       | –                   | gone (merged)   |
| 2.3  | in_progress | completed    | deferred_for_cadence| live worktree   |
| 2.4  | in_progress | completed    | deferred_for_cadence| live worktree   |
| 3.1  | completed   | completed    | deferred_for_cadence| live worktree   |
| 3.2  | in_progress | completed    | deferred_for_cadence| live worktree   |

Three independent bugs:

### Bug 1 — task completion is authored by the agent, not the server (root of "makes no sense")

3.1 and 2.3/2.4/3.2 are in the **identical** underlying state (agent
finished clean, branch ready, merge deferred for CI cadence) yet render
differently. Cause: task completion is written by the **agent**
self-reporting via the MCP `update_task_status` tool (`source='manual'`,
timestamps flip ~20s *before* the agent process dies). It's best-effort —
agents that called it → `completed` (2.1/2.2/3.1); agents that didn't →
stuck `in_progress` (2.3/2.4/3.2).

The server's own pipeline should make this deterministic but doesn't:
`auto_mode.rs::defer_for_cadence` (`server-rs/src/auto_mode.rs:1598-1647`)
marks the agent `deferred_for_cadence` and advances scheduling but **never
writes `task_status`**. So a finished, ready-to-merge task is left at
`in_progress` unless the LLM happened to self-report.

UI consequence: the "Awaiting cadence" badge only renders when
`task.status === "completed"` (`web/src/components/TaskCard.tsx:104`,
derived at the `isAwaitingCadence` check). The stuck-`in_progress` tasks
don't even get that hint — they show a pulsing amber "In Progress" as if
an agent were live.

### Bug 2 — "killed agents still running" = stale store + a restamp race

- Their DB status genuinely *is* `killed` (set by
  `AgentRegistry::mark_supervisor_died`, `server-rs/src/agents/mod.rs:1424`).
  A fresh `/api/agents` load renders them correctly. The `agent_stopped`
  WS broadcast flips the store (`web/src/stores/ws-store.ts:389-408`) — but
  those kills fired ~half a day before viewing. There is **no
  reconcile-on-reconnect**, so a browser that missed the event keeps
  showing the spawn-time `running`. Hard refresh fixes the display.
- Deeper: 2.1/2.2 were stamped `killed` even though their task completed
  and branch merged. Their clean-completion recording was lost (process
  gone before `mark_agent_finished`), then a liveness/boot sweep stamped
  `killed` over the still-`running` row. Live variant of the v0.5.126
  supervisor-died race.
- Side issue: ~7 orphaned `branchwork-server session` processes from old
  `plan-fix-merge` worktrees linger in `ps` (May 30–Jun 2), never reaped.

### Bug 3 — worktree info exists but is never surfaced

The 4 ready branches each hold a live git worktree under
`~/.branchwork/worktrees/branchwork/dag-based-plan-model/...` (verified on
disk). The data is in `agent.cwd`, and `TaskCard` already has an
`isWorktreeCwd` helper (`web/src/components/TaskCard.tsx:176`) — but the
"running in:" row only renders for the **running** agent, so a
finished/deferred agent's worktree is invisible. Worktree path/size/orphan
status otherwise lives only in admin-only endpoints
(`server-rs/src/api/orphan_worktrees.rs`,
`server-rs/src/api/worktree_disk_usage.rs`), never joined into the plan
board. (DB is already 2.8 GB.)

---

## Fix plan

### Phase 1 — Deterministic task completion (Bug 1) — highest leverage

**1.1 — `defer_for_cadence` marks the task completed (server-authored).**
In `server-rs/src/auto_mode.rs::defer_for_cadence` (~1606), after setting
`merge_status='deferred_for_cadence'`, also upsert `task_status` to
`completed` with `source='auto'` and broadcast `task_status_changed` —
reusing the exact pattern already at `auto_mode.rs:3019-3041`
(`on_fix_ci_passed`). Rationale: the agent finished cleanly and the branch
is captured for the cadence drain; the task IS done from the plan's POV,
it's only the *merge* that's pending — which the existing "Awaiting
cadence" badge is designed to express. This makes 2.3/2.4/3.2 behave
identically to 3.1 and removes the dependency on agent self-reporting.
  - Keep `source='auto'` so a manual user edit still wins and a future
    re-sync can overwrite (matches the navbar-plan T2.3 convention noted
    in the existing comment).
  - Do **not** clear the agent `branch` here — the drain still needs it
    (existing invariant, see `defer_for_cadence` doc comment).

**1.2 — Backfill / reconcile sweep.** Add a one-shot reconcile (boot-time,
alongside the existing orphan sweep) that finds tasks where
`task_status != 'completed'` but a non-failed agent for that task has
`merge_status='deferred_for_cadence'` (branch intact, clean finish) and
upgrades them to `completed/auto`. Fixes the already-stuck 2.3/2.4/3.2
without waiting for the next drain. Idempotent.

**1.3 — Tests.** Unit test `defer_for_cadence` writes
`task_status=completed/auto` + emits `task_status_changed`. Unit test the
reconcile sweep upgrades a stuck deferred task and is a no-op on a
genuinely-in-progress one (live agent, no branch). Run the existing
auto-mode state-machine tests to confirm no regression in the drain path
(the drain reads agent `branch`, not `task_status`, so completion-marking
must not interfere).

### Phase 2 — Surface worktrees per task (Bug 3)

**2.1 — Render the worktree row for the branch (deferred) agent too.** In
`TaskCard.tsx`, show the existing `isWorktreeCwd`-gated "running in:" row
for `branchAgent` (finished-with-branch), relabeled e.g. "worktree:" when
not running. Pure frontend; data (`cwd`) is already present.

**2.2 — Join worktree disk size into the plan/agent payload.** Surface the
per-worktree size already computed by `worktree_disk_usage.rs` so the card
can show "worktree: … (N MB)". If a per-agent join is too heavy, expose a
lightweight `GET` that returns `{agent_id → {path, exists, size_bytes}}`
for a plan and have `PlanBoard` fetch it. Show an "exists/orphaned" flag.

**2.3 — Per-task "discard worktree" affordance** for a ready/deferred task
whose branch the user does not want to merge — wired to the existing
orphan-cleanup path (`orphan_worktrees.rs` cleanup endpoint), scoped to the
single path. (Optional; gate behind confirmation.)

### Phase 3 — Liveness/store hardening (Bug 2)

**3.1 — Reconcile agents on WS reconnect.** In `web/src/stores/ws-store.ts`,
on socket (re)open call `fetchAgents()` so a store that missed
`agent_stopped` while disconnected re-syncs. Kills the "killed shows as
running until refresh" class.

**3.2 — Stop restamping completed/merged agents as killed.** Audit the
supervisor-died / boot-reconcile path
(`agents/mod.rs::mark_supervisor_died`, the boot sweep, and the v0.5.126
pidfile check) so an agent that already produced+merged a branch isn't
re-marked `killed`. Likely: tighten the `WHERE status IN
('running','starting')` guard with a "and no merged branch / no clean exit
recorded" condition, or record clean exit before the process can be reaped.
Needs a focused repro first — treat as investigation, not a blind patch.

**3.3 — Reap orphaned session supervisors.** Confirm the boot orphan sweep
covers leaked `branchwork-server session` PIDs from dead worktrees (the
May 30–Jun 2 leftovers), not just worktree dirs. **Must obey CLAUDE.md** —
no `pgrep -f "branchwork-server"` / `killall`; scope by recorded PID
(`.test.pids`-style) or the session's own tracked pidfile. This is the
exact footgun ADR 0005 exists for.

---

## Ordering & rollout

1. **Phase 1 first** — it's the actual "makes no sense" bug, server-only,
   small, high-confidence. Ship 1.1 + 1.2 + 1.3 together.
2. **Phase 2** — independent, frontend-heavy, low risk.
3. **Phase 3** — 3.1 is trivial and safe; 3.2 needs investigation; 3.3
   must respect ADR 0005.

Per CLAUDE.md memory: run `cargo fmt + clippy + test` locally before every
push; any e2e/process test goes through Docker Compose, never host-process
patterns.

## Out of scope / open questions

- Whether to introduce a distinct `task_status` value for "done, pending
  merge" instead of overloading `completed` + `merge_status`. Current plan
  reuses `completed` because the UI already models it via the
  awaiting-cadence derived state — a new enum value touches DB, API,
  frontend, and every status consumer. Flag for decision if you'd rather
  model it explicitly.
- The 2.8 GB `branchwork.db` size is a separate concern (likely agent
  output / audit retention) — not addressed here.
