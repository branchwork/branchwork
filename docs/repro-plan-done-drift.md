# Repro — plan shown as completed while last task is in_progress

Bug: the frontend's `doneCount` in `PlanSummary` drifts upward because
`patchTaskStatus` (`web/src/stores/plan-store.ts:161-189`) only ever adds
`+1` for transitions to `completed`/`skipped` and never subtracts. Once
`doneCount >= taskCount`, the plan is shown as done in the sidebar's
"project groups" and the Project Dashboard card, even if a task is
visibly `in_progress` on the plan board.

## Reproduction sequence

Driven via `PUT /api/plans/fix-plan-done-in-progress/tasks/:num/status`
on the `fix-plan-done-in-progress` plan (11 tasks total).

Starting point: 0.1 `completed`, 0.2 `in_progress`, all others `pending`.
Server `doneCount = 1/11`.

1. `3.3 → completed` — server `doneCount = 2/11`; frontend `doneCount = 2`.
2. `3.3 → in_progress` — server `doneCount = 1/11`; frontend `doneCount = 2`.
   Drift +1 (no decrement applied on the transition out of `completed`).
3. Mark the other 10 tasks `completed` in any order. Each event increments
   the frontend `doneCount` by `+1`, including `0.1` which was already
   `completed` server-side.

## Before / after

| | server doneCount | frontend doneCount | taskCount | isPlanDone |
|---|---|---|---|---|
| before | 1 | 1 | 11 | false |
| after  | 10 | 12 | 11 | **true** |

After the sequence, task 3.3 is still `in_progress` but the plan is moved
into the "Done" section because `doneCount (12) >= taskCount (11)`.
Server-side is correct: `GET /api/plans` returns `doneCount=10/11` and
`GET /api/plans/fix-plan-done-in-progress` shows 3.3 as `in_progress`.

## Contributing factors

- `patchTaskStatus` delta is `+1` or `0`, never `-1`
  (`web/src/stores/plan-store.ts:183-186`).
- `task_status_changed` in `web/src/stores/ws-store.ts:195-206` does not
  trigger a debounced `fetchPlans`, so the drift is never reconciled
  against the server.
- `isPlanDone` uses `p.doneCount >= p.taskCount`
  (`web/src/components/Sidebar.tsx:19-21`,
  `web/src/components/ProjectDashboard.tsx:17-19`), so any upward drift
  immediately flips the plan into the done group.

## Task 1.1 — root decision point confirmed

`isPlanDone` is the sole gate that moves a plan into the "Done" section,
in both the sidebar and the project dashboard. Both copies are
byte-identical:

```ts
function isPlanDone(p: PlanSummary): boolean {
  return p.taskCount > 0 && p.doneCount >= p.taskCount;
}
```

(`web/src/components/Sidebar.tsx:19-21`,
`web/src/components/ProjectDashboard.tsx:17-19`.)

Decision is purely arithmetic on `PlanSummary`:

- `Sidebar.tsx:80` — `if (isPlanDone(p)) g.done.push(p) else g.active.push(p)`.
  No other status comparison exists in the grouping loop.
- `ProjectDashboard.tsx:77-78` —
  `activePlans: sortedPlans.filter((p) => !isPlanDone(p))` /
  `donePlans: sortedPlans.filter(isPlanDone)`. All downstream uses of
  `donePlans` (lines 215, 269, 275-276) consume that filtered array
  without re-checking task statuses.

`PlanSummary` (`web/src/stores/plan-store.ts:67-78`) carries
`doneCount` and `taskCount` as flat numbers; neither file inspects
`PlanTask.status` (e.g. `in_progress`, `failed`) when deciding
done-ness. So once `doneCount` drifts above `taskCount` (per Task 0.2),
the plan flips into "Done" regardless of any task being visibly
`in_progress`.

Confirms the acceptance criterion: `isPlanDone(p)` depends solely on
`p.doneCount >= p.taskCount` (with the `taskCount > 0` guard); there is
no per-status check.

## Task 0.3 — server-side confirmation

With `0.1 completed`, `0.2 completed`, `0.3 in_progress`, and all other
tasks `skipped` (10 effective done out of 11), `curl /api/plans` returns:

```json
{ "name": "fix-plan-done-in-progress", "taskCount": 11, "doneCount": 10 }
```

`doneCount < taskCount` holds — `GET /api/plans` is authoritative and
correctly reports the in_progress task as not done. The bug is isolated
to the frontend's optimistic `patchTaskStatus`; the backend never claims
the plan is complete in this state.

## Task 1.2 — drifting mutation pinpointed

Root cause confirmed in `patchTaskStatus`
(`web/src/stores/plan-store.ts:161-189`):

```ts
const delta =
  status === "completed" || status === "skipped" ? 1 : 0;
// We don't know the previous status precisely, so just refetch later
return { ...p, doneCount: p.doneCount + delta };
```

Three independent failure modes follow from this delta:

1. **One-way / never decrements.** `delta` is `+1` or `0` only. A
   transition out of `completed`/`skipped` (e.g. `completed → in_progress`
   in step 2 of the repro) leaves `doneCount` untouched. The cached
   number can only grow.
2. **Double-counts re-entry.** No comparison against the prior task
   status, so any duplicate `task_status_changed` for an already-done
   task (e.g. `completed → completed`, or `skipped → completed`) adds
   another `+1`. The repro hits this on step 3 when `0.1` (already
   completed server-side) is re-marked.
3. **No refetch safety net.** The inline comment promises a "refetch
   later" but no caller schedules one. The `task_status_changed` handler
   (`web/src/stores/ws-store.ts:195-206`) only invokes
   `planStore.patchTaskStatus(...)`; the debounced `fetchPlans()` in
   `ws-store.ts:128-132` is wired to `plan_updated` exclusively, so the
   drift is never reconciled against the server's authoritative
   `doneCount`.

These three effects compose exactly into the table at the top of this
doc (server `doneCount = 10/11`, frontend `doneCount = 12/11`).

**Acceptance**: `patchTaskStatus` applies an unsigned `+1` delta, never
decrements, and `task_status_changed` has no refetch safety net.

## Task 1.3 — backend ruled out

`list_plans` in `server-rs/src/api/plans.rs:76-127` recomputes
`done_count` from authoritative state on every `GET /api/plans`:

```rust
parsed.phases.iter().flat_map(|p| &p.tasks)
    .filter(|t| {
        let status = status_map.get(&t.number)
            .map(|s| s.as_str()).unwrap_or("pending");
        status == "completed" || status == "skipped"
    })
    .count()
```

`status_map` is loaded fresh from `task_status` (line 84-93); tasks
without a row default to `pending`. The filter only counts `completed`
and `skipped`, matching the same semantics used by `set_task_status`
when persisting transitions. A hard refresh therefore always converges
to truth.

Grep across `server-rs/src` for `done_count|doneCount` returns only:

- `api/plans.rs:27` — `PlanListEntry.done_count` field on the
  `GET /api/plans` response.
- `api/plans.rs:80,120` — the recomputation above.
- `mcp/tools/plans.rs:45,136,156` — the parallel MCP tool path
  (`list_plans` over MCP) which mirrors the same DB-derived
  recomputation.

No WebSocket event ships a precomputed `doneCount` (no matches in
`ws.rs`, `hooks.rs`, or anywhere else); WS only emits granular events
like `task_status_changed`, `plan_updated`, etc. The server never
hands the client a stale or precomputed `doneCount` that could be
trusted blindly.

**Acceptance**: server code verified correct; the fix must be
frontend-only.
