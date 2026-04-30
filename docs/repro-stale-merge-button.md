# Repro — stale "Merge" button on no-commit tasks

Bug: the TaskCard renders an indigo merge/discard banner whenever a
finished agent for that task still has a task branch, with no regard for
whether the agent produced any commits. Clicking **Merge** on a branch
that's 0 commits ahead of its source returns 409 from the backend —
user-visible only after the click, not up-front.

## Code path under repro

- Banner gate, `web/src/components/TaskCard.tsx:89-97` and `:561`:
  the `branchAgent` selector matches any agent for the current task
  whose `status !== "running"` and `status !== "starting"` *and* has a
  non-empty `branch`. The JSX at `:561` renders the indigo banner
  whenever `branchAgent?.branch` is truthy. There is no per-task signal
  that the work wasn't supposed to produce a commit.
- Server-side guard, `server-rs/src/api/agents.rs:302-353`: before any
  checkout, `merge_agent_branch` runs
  `git rev-list --count <target>..<task_branch>`. When the count is `0`
  it returns **409 Conflict** with
  `error: "task branch has no commits — agent exited without
  committing"`. The guard is defense-in-depth, not the UX: by the time
  the user sees the error, they've already clicked Merge.

## Reproduction

The stale-banner scenario is "any completed agent row whose branch is
at the same SHA as its source." Any finished investigation/repro task
whose agent didn't commit lands in this state. In the current dev DB,
the agent for
`fix-plan-done-in-progress` task `1.2` happens to be in that shape —
branch `branchwork/fix-plan-done-in-progress/1.2` points at the same
commit as `master`, and the agent row is `status=completed` with a
`branch` set. That's sufficient to trigger both the banner (frontend)
and the guard (backend) without spinning up a brand-new agent.

### Setup check

```bash
# Confirm the agent row shape
curl -s http://localhost:3100/api/agents | \
  python3 -c "import json,sys; \
    a=[x for x in json.load(sys.stdin) \
       if x['id']=='06c8ffa1-662e-4f6e-8ee7-43f919b119ef'][0]; \
    print(a['status'], a['branch'], a['source_branch'])"
# → completed branchwork/fix-plan-done-in-progress/1.2 master

# Confirm branch has no commits ahead of source
git rev-list --count master..branchwork/fix-plan-done-in-progress/1.2
# → 0
```

### Step 1 — banner shows up

Open the Plan Board for plan `fix-plan-done-in-progress`, phase 1,
task 1.2. The indigo merge/discard banner renders:

```
⁋ branchwork/fix-plan-done-in-progress/1.2
  → master                          [Discard] [Merge]
```

Nothing in the UI signals that this branch has no commits. A user
looking at this card reasonably assumes the work is ready to merge.

### Step 2 — click Merge → 409

```bash
curl -s -i -X POST \
  http://localhost:3100/api/agents/06c8ffa1-662e-4f6e-8ee7-43f919b119ef/merge
```

Observed response:

```
HTTP/1.1 409 Conflict
content-type: application/json
content-length: 144

{"branch":"branchwork/fix-plan-done-in-progress/1.2",
 "error":"task branch has no commits — agent exited without committing",
 "target":"master"}
```

The 409 matches exactly the string in
`server-rs/src/api/agents.rs:327`. The working tree is untouched — the
guard returns before `git checkout <target>` runs
(`agents.rs:355` is never reached).

### Step 3 — UI surfaces the error *after* the click

`mergeAgentBranch` in `web/src/stores/agent-store.ts` unwraps
`HttpError.body.error` and returns it; `TaskCard.tsx:620` feeds it into
`setError(...)`, so the error pane at the bottom of the card shows the
same message. The banner itself does not disappear — the button
remains clickable and continues to return 409 on every retry until the
user reaches for **Discard** instead.

## Summary of observed behavior

| step | expected for a "won't-commit" task | actual today |
|---|---|---|
| agent exits without committing | no merge button (nothing to merge) | indigo banner with green **Merge** button |
| user clicks Merge | no-op, or button was never there | 409 `task branch has no commits …` |
| user retries Merge | — | same 409, indefinitely |
| up-front UI signal | present (task declares it won't commit) | **none** — condition is just `branchAgent.branch` |

## What to change (for reference — out of scope for 0.1)

- Plan YAML / `PlanTask`: add a per-task `produces_commit: bool`
  (default `true` to preserve current behavior).
- `TaskCard.tsx:561`: gate the **Merge** button on
  `task.producesCommit !== false`. Keep **Discard** available — an
  empty branch still needs cleanup.
- Leave the server 409 guard in place as defense-in-depth.

## Acceptance

- [x] Repro steps documented.
- [x] 409 observed on merge click for a no-commit task
      (`fix-plan-done-in-progress/1.2` agent, 2026-04-15, body matches
      `agents.rs:327` verbatim).
