# Auto-mode operations

Auto-mode is the per-plan toggle that turns a sequence of completed
tasks into an unattended pipeline: each task's branch merges into the
default branch, CI runs, and the next ready task spawns — without a
human at the keyboard. The end-to-end flow and the status pill live in
[user-guide.md § Unattended auto-mode](../user-guide.md#unattended-auto-mode);
this page covers the two cross-cutting operational knobs you tune
**before** turning auto-mode on:

- **Merge cadence** — `task`, `phase`, or `plan`. Decides *when* the
  default branch actually moves (and therefore when CI fires, when the
  patch version bumps, and when the deploy lands).
- **Dirty-tree allowlist** — which paths the
  `agent_left_uncommitted_work` pause should tolerate (build logs,
  scratchpads, …) so the loop doesn't pause on operational noise.

Both live under `[auto_mode]` in the project's `branchwork.toml`
([reference/branchwork-toml.md](../reference/branchwork-toml.md)) and
both can be overridden per plan. They interact: a `phase` or `plan`
cadence holds completed task branches longer, which means the
dirty-tree check protects more state per merge boundary.

---

## TL;DR — picking a cadence for your first plan

| You want… | Pick |
|---|---|
| Per-task feedback. CI runs on every task merge. Useful for spike work, exploratory plans, single-developer projects with cheap CI. | `task` |
| One CI run per phase. Tasks complete on their branches and batch-merge at the phase boundary. **Default for new plans.** Good fit for most real plans (5–15 tasks, 2–4 phases). | `phase` |
| One CI run + one version bump + one deploy per plan. Best when the plan ships as a single semantic unit (a feature flag rollout, a coordinated refactor). | `plan` |

The default is **`phase`** when you create a new plan under a project
without a `branchwork.toml`. Existing plans grandfathered before the
cadence work shipped stay on `task` until you change them — that
migration was deliberate so the day-1 rollout was behavioural no-op.

If you're not sure, leave the default. `phase` is the cadence the
plan brief was designed around and it's what every Branchwork project
runs in production.

---

## The three cadences

The cadence governs when auto-mode merges a completed task into the
plan's default branch. Implementation lives in
[`server-rs/src/auto_mode.rs::should_merge_now`](../../server-rs/src/auto_mode.rs);
the predicate is consulted on every task-completion event.

### `task` — merge on every completion

Legacy behaviour. The moment a task reaches `completed | skipped`,
auto-mode checks out the default branch, fast-forwards the task
branch in, pushes, and waits for CI. Then it spawns the next ready
task.

- One commit on master per task.
- One CI run per task.
- One patch-version bump per task.
- One deploy per task (in projects that gate deploy on a green CI).

Use it when **per-task feedback is the goal** — spike work, narrow
fixes, plans where the task structure exists only to break work down
and you want each step shipped immediately.

### `phase` — merge at the phase boundary

Tasks complete on their own branches. Auto-mode marks each completed
agent `merge_status='deferred_for_cadence'` and leaves the branch
intact. When **every** task in the phase has status
`completed | skipped`, the loop drains the deferred queue in
plan-declaration order and emits **one** master push at the end of
the batch (one commit per task, single push). CI then runs once
against the phase-boundary SHA.

- N commits on master per phase (one per task, but all in one push).
- One CI run per phase.
- One patch-version bump per phase.
- One deploy per phase.

Use it when **the phase is the natural unit of work**. Most plans
fit this shape — phases group related tasks that share a
verification step, and shipping them together keeps the deploy
history aligned with the plan's conceptual chunks.

A failed task **blocks** the phase boundary. The predicate returns
`false` until the operator either fixes the task and re-runs it, or
marks it skipped. Auto-mode does not move on by ignoring failures.

### `plan` — merge at the end of the plan

The strictest cadence. Auto-mode defers every completed task until
**every** task in **every** phase has status `completed | skipped`.
At that point the loop drains every deferred merge in order and the
default branch moves exactly once.

- N commits on master per plan (one per task, single push).
- One CI run per plan.
- One patch-version bump per plan.
- One deploy per plan.

Use it when **the plan ships as a single semantic unit** — a feature
flag rollout, a coordinated cross-cutting refactor, a release
bundling multiple changes that only make sense together. The version
field then reads "X shipped plans" instead of "X completed tasks",
which is closer to what the version number is meant to mean.

For single-phase plans, `plan` and `phase` collapse to the same
trigger. Accepted per the plan brief — no special-casing needed.

---

## Setting the cadence

Two layers, plan-level winning over project-level. The resolution
chain mirrors the CI-filter and phase-verify settings
([reference/branchwork-toml.md § Precedence](../reference/branchwork-toml.md#precedence)).

### 1. Per-plan (via the dashboard or `PUT /api/plans/:name/settings`)

The fastest path. Open the plan, click **Settings**, find the
**Merge cadence** section, and pick a radio. The active row shows
the resolution source as a chip — `plan` (you set it here),
`inherited` (came from the project default), or `default` (the
hard-coded fallback — `phase`). Click **Inherit** to remove the
plan-level pin and fall back to the project default.

Programmatically:

```sh
curl -X PUT http://localhost:3100/api/plans/<plan-name>/settings \
  -H 'content-type: application/json' \
  -d '{"mergeCadence": "phase"}'
```

Send `null` to clear the override (`{"mergeCadence": null}`). Omit
the field entirely to leave it untouched — the body is tristate per
field (mirrors the `deserialize_some` shim in
[`server-rs/src/api/plans.rs`](../../server-rs/src/api/plans.rs)).

### 2. Per-project (via `branchwork.toml`)

Set the default for every plan in the project. The file lives at
the project root — the same directory the plan's `project:` field
points at.

```toml
# ~/<project>/branchwork.toml
[auto_mode]
# When auto-mode should merge a task's branch into the default
# branch.
#   task  — every completed task (legacy / fastest feedback)
#   phase — at phase boundary (default)
#   plan  — only at end of plan (one shipped version per plan)
merge_cadence = "phase"
```

Plans in this project that don't pin a value at the plan level
inherit `phase`. Plans that **do** pin a value still override.

An invalid value (`merge_cadence = "weekly"`) fails the TOML parse
and the whole file is treated as malformed — Branchwork falls back
to its defaults, logs a one-line warning to stderr
(`[branchwork] warning: failed to parse …`), and caches the `None`
so subsequent calls don't re-warn. The narrow lesson: a typo in this
field is loud at parse time but silent at runtime unless you check
the server log. Double-check via the Settings tab — it surfaces
the resolved cadence and its source.

### 3. Hard-coded fallback

If neither the plan nor the project sets a value, Branchwork uses
[`MergeCadence::default()`](../../server-rs/src/repo_config.rs) — which
is `phase`. Mirrored on the frontend as `DEFAULT_MERGE_CADENCE` in
[`web/src/api/plans.ts`](../../web/src/api/plans.ts) so client-side
preview agrees with the server.

---

## Trade-offs

The three cadences are not just three speeds of the same thing — they
trade three different properties:

### Bisectability

Master CI on `phase` and `plan` reports green/red on **N tasks
combined**. A regression that only appears from the *combination* of
two task changes (a function added in task 1.1 and a caller added in
task 1.3, say) won't be caught by either task's individual
`task-tests` run on the task branch; the first place it surfaces is
the post-merge master CI, which now covers a larger blast radius.

`task` cadence preserves per-task bisectability: every master commit
maps to exactly one task, every CI run answers "did this task break
master?" deterministically.

For most plans this trade is fine — Branchwork's per-task
`task-tests` workflow ([architecture/ci-pipelines.md](../architecture/ci-pipelines.md))
runs the full test suite on every task branch before merge, so most
regressions get caught before the cadence boundary. The trade-off
bites on integration tests that only run on master, or on tests
that explicitly span multiple files updated by sibling tasks.

### Merge-time conflict surface

Multiple completed task branches all wait for the cadence boundary,
then merge into the default branch in plan-declaration order. If two
deferred tasks touched the same files, the second merge in the batch
conflicts. Auto-mode pauses the plan with `merge_conflict` and
surfaces the dirty paths via the same banner the dirty-tree path
uses (see [Dirty-tree allowlist](#dirty-tree-allowlist) below).

`task` cadence avoids batching entirely — every merge lands
immediately, so conflicts surface task by task and are easier to
isolate.

The branchwork conflict-resolution path exists and handles the
common case. It has not been stress-tested at N > 3 simultaneously
deferred merges with overlapping diffs; if you frequently work with
phases where 5+ tasks touch the same module, `task` cadence may
match your reality better than `phase`.

### Version semantics

The patch version (`v0.5.X`) bumps on every master push by the
bump workflow ([architecture/ci-pipelines.md](../architecture/ci-pipelines.md)).
Bump cadence therefore follows merge cadence by construction.

- `task` → 60+ patch bumps per heavy session. The version number
  reads as "tasks completed", which is technically correct but not
  what readers expect from a `vX.Y.Z` string.
- `phase` → one bump per phase. The version reads as "phases
  shipped", a coarser but more meaningful axis.
- `plan` → one bump per plan. The version reads as "plans
  delivered", which is closest to what a semantic-version-style
  patch number is meant to mean.

There is **no** independent `bump_cadence` knob distinct from
`merge_cadence`. Decoupling them would mean master could advance
without bumping; the spec for "when does a non-bump master commit
get deployed?" is more complex than the plan brief took on. By
construction, bump cadence = merge cadence.

---

## Operational signals

When you switch off `task` cadence, the dashboard surfaces three new
states so you can tell deferred work apart from broken work.

### `awaiting-cadence` pill on the task card

After a task completes under a non-`task` cadence, its card shows a
muted-purple **awaiting-cadence** pill with tooltip "Completed;
waiting for phase boundary to merge (X of N tasks done)." The agent
row stays in the dashboard with `merge_status='deferred_for_cadence'`
and its branch intact, ready for the boundary drain.

If you want to inspect the branch in the meantime, the agent panel
opens it the same way it always has — only the merge button is
suppressed until the cadence triggers (or the plan-level **Flush
now** button fires below).

### Plan-header chip: "N tasks awaiting cadence merge"

When at least one task is deferred, the plan title bar carries a
chip: `"3 tasks awaiting phase-end merge — [Flush now]"`. The chip
disappears as soon as the cadence boundary triggers and the batch
drains.

### Flush now — operator escape hatch

The chip and the **Deferred merges** banner under the plan header
both expose a **Flush now** button. It calls
`POST /api/plans/:name/flush-merges` and unconditionally drains
every `deferred_for_cadence` row in the plan, regardless of the
configured cadence, then triggers exactly one master push + CI
+ deploy at the end of the batch.

```sh
curl -X POST http://localhost:3100/api/plans/<plan-name>/flush-merges
```

Idempotent — running it with zero deferred rows returns
`{"merged": [], "count": 0, "message": "No deferred merges to flush."}`
and is a no-op. The button is wired through the same dialog pattern
the per-task **Merge** button uses, so the operator confirms before
the batch fires.

The audit log records the operator intent as one
`auto_mode.flushed_deferred` row even though each merge in the batch
emits its own `auto_mode.merged` row. Reading the audit log
top-down, "Flush now" reads as a single deliberate action; the
per-task merges read as the cadence boundary's work.

---

## Dirty-tree allowlist

Auto-mode's tree-clean gate sits between **agent finishes** and
**merge fires**. A tracked-modified path in the project tree pauses
the plan with `agent_left_uncommitted_work` instead of merging an
incomplete branch
([user-guide.md § Pause on uncommitted work](../user-guide.md#pause-on-uncommitted-work)).

The gate exists because half-committed agent work is the most common
unattended-failure mode, and silently merging it would damage master
in a way that's hard to undo. But it also fires on **operational
write-noise** — build logs an agent's project script appends to,
local `.mcp.json` overrides, scratch notes, the test runner's own
output files. Those shouldn't pause the plan.

The fix is a per-project allowlist:

```toml
# ~/<project>/branchwork.toml
[auto_mode.dirty_tree]
# Gitignore-style patterns. Matched against `git status --porcelain`
# paths; tracked-modified files matching ANY pattern are filtered
# before the dirty-tree verdict is computed. Patterns without `/`
# match the basename at any depth; `**` matches across directories.
ignore = [
  "*.log",          # build logs
  ".bob/**",        # operator scratch notes
  "scratch/**",     # local-only WIP dir
]
```

Matching semantics are gitignore-style, implemented inline in
[`server-rs/src/repo_config.rs`](../../server-rs/src/repo_config.rs)
(no `globset` dep). When **every** dirty path matches the allowlist
the verdict flips to Clean and the loop proceeds to merge.

The allowlist is a **filter on operational noise**, not a way to
suppress the gate entirely. Agent code paths (`server-rs/`,
`web/src/`, scripts the agent actually edits) should still trip the
pause — that's the whole reason the gate exists. Keep entries
narrow and named for the producer (e.g. `*.log` not `*`).

For a deeper walkthrough — including the per-plan resolver, the
on-pause file list in the banner, the auto-resume-on-tree-clean
watcher, and why this all needs to coexist with the cadence
batching above — see the
[dirty-tree-check plan write-up](../../README.md) at the root of
the repo and ADR 0003.

### Why cadence and dirty-tree interact

Under `phase` or `plan` cadence, completed-but-deferred branches
accumulate. Each one is **still subject to its own tree-clean check
at completion time** — the pause fires per-task, not per-batch. A
dirty tree on task 1.2 pauses the plan with the deferred queue
intact; resuming after the operator cleans the tree drains the
queue at the boundary as if 1.2 had completed cleanly the first
time.

This is intentional: defer-then-resume should look identical to
clean-completion-then-defer on the merge path. The dirty-tree
allowlist therefore matters **more** under non-`task` cadence,
because a noisy tracked file dirties the plan three or four times
per phase instead of once per task. Set the allowlist before you
flip the cadence away from `task`.

---

## Failure modes

| Symptom | Cadence-aware cause | Where to look |
|---|---|---|
| Task shows `completed` for hours but the merge banner never fires. | Working as designed under `phase` / `plan` — the merge is deferred until the cadence boundary. The pill should be **awaiting-cadence** (muted-purple). | Hover the pill for the tooltip; check the plan-header chip for "N tasks awaiting" with **Flush now**. |
| `Flush now` reports "No deferred merges to flush" but the cadence chip is showing. | Race between the chip's local state and the WS event that drained the batch. Refresh; the chip is reactive to the agent `merge_status` flips. | [`web/src/components/PlanBoard.tsx`](../../web/src/components/PlanBoard.tsx) — `AwaitingCadenceChip` reads from `useAgentStore`. |
| `phase` cadence never drains — boundary task is `failed`. | Failed tasks block the boundary by design. Either re-run the task, fix manually + mark it complete, or mark it `skipped` if it's no longer needed. | Open the task card; the failure reason is on the agent row. |
| `branchwork.toml` says `merge_cadence = "phase"` but the Settings tab shows `default`. | TOML parse failure — usually a typo elsewhere in the file (e.g. an unquoted string in `ci.blocking_workflows`). The whole file falls back. | `[branchwork] warning: failed to parse …` in the server log. Fix the typo and the cadence resolves to "repo default" again. |
| Plan paused with `agent_left_uncommitted_work` on a file the project always touches (e.g. `web-dev.log`). | The dirty-tree allowlist doesn't cover that path. Add it to `[auto_mode.dirty_tree].ignore`. | [Dirty-tree allowlist](#dirty-tree-allowlist) above; [reference/branchwork-toml.md](../reference/branchwork-toml.md) for the full schema. |

---

## See also

- [user-guide.md § Auto-mode](../user-guide.md#auto-mode) — the
  toggle, the status pill, the `Parallel` switch.
- [user-guide.md § Unattended auto-mode](../user-guide.md#unattended-auto-mode)
  — the end-to-end Stop-hook → tree-clean gate → merge → CI →
  next-task pipeline.
- [reference/branchwork-toml.md](../reference/branchwork-toml.md) —
  schema for `[ci]`, `[phase]`, and `[auto_mode]` sections of the
  project-level config file.
- [reference/plan-schema.md](../reference/plan-schema.md) — per-plan
  and per-phase fields, including the CI workflow filter and phase
  verification settings the cadence interacts with.
- [architecture/ci-pipelines.md](../architecture/ci-pipelines.md) —
  why `task-tests` and `Pipeline` are split, and how each cadence
  changes the firing pattern.
- [adrs/0003-unattended-auto-mode.md](../adrs/0003-unattended-auto-mode.md)
  — design rationale for the Stop-hook handler, the dirty-tree gate,
  and the merge-on-completion contract the cadence rides on top of.
- [troubleshooting.md § CI and auto-mode](../troubleshooting.md) —
  symptom-indexed FAQ covering pause reasons, fix-CI retries,
  workflow classification.
