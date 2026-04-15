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
