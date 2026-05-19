# CI pipelines

How GitHub Actions workflows fan out across master, task branches, and
PRs. The actual job logic (server tests, web typecheck, e2e) lives in
**one** place — [`tests.yml`](../../.github/workflows/tests.yml) — and
two callers invoke it via `workflow_call` for two different cadences:

- [`task-tests.yml`](../../.github/workflows/task-tests.yml) — the
  per-task merge gate, fires on every `branchwork/**` push and every
  PR against master. Just the tests. No bump, no build, no deploy.
- [`pipeline.yml`](../../.github/workflows/pipeline.yml) — the
  master-side workflow. Runs the same tests via `workflow_call`, then
  bumps (on human master pushes) or builds + deploys (on the resulting
  auto-bump commit).

The image build + Hetzner deploy live inside `pipeline.yml`'s `docker`
and `deploy` jobs; see [`deploy.md`](deploy.md) for the publish-and-roll
side.

## Per-event matrix

| Event                                | Workflows fired       | Jobs run                                      |
| ------------------------------------ | --------------------- | --------------------------------------------- |
| Human commit pushed to master        | `Pipeline`            | classify + bump (tests / docker / deploy skipped) |
| Bump commit pushed to master         | `Pipeline`            | classify + tests (nested) + docker + deploy   |
| Push to `branchwork/**` task branch  | `task-tests`          | tests (nested)                                |
| PR against master                    | `task-tests`          | tests (nested)                                |
| `workflow_dispatch` on `pipeline.yml`| `Pipeline`            | classify + tests + docker + deploy            |
| Tag push (`v*`)                      | `tag-build`           | release artifact build                        |

PRs fire `task-tests` only — `pipeline.yml`'s `on:` block lists
`push: [master]` + `workflow_dispatch`, no `pull_request:` entry, so
opening or pushing to a PR cannot reach its trigger surface. This is
the design intent of the split (Phase 4.3): each event maps to one
workflow run, the PR Checks tab shows a single check, and master-side
build/deploy stays out of the PR review surface.

## Why a separate `task-tests` workflow

A single `pipeline.yml` listening on `branchwork/**` AND `master`
worked fine, but coupled the test cadence to the build cadence: any
future change to "test more often than we build" or "ship docker on a
different ref" would force unwinding `pipeline.yml`'s `needs:` graph.
Splitting now keeps the test cost equal across both cadences while
freeing the master-side graph to evolve independently. The plan that
introduced the split is `ci-split-task-tests-from-master-pipeline`
(structural-only, no behaviour change at merge time).

The dashboard's per-plan `ciBlockingWorkflows` setting is also why the
name `task-tests` matters: the merge-gate poller blocks on **that**
workflow's run for a given task branch, not on `Pipeline` (which does
not fire on task branches in the post-split world). Plans created
before the split carried `ciBlockingWorkflows: ["Pipeline"]` and the
one-shot migration (Phase 3.1) flipped them to `["task-tests"]`.

## Nested `workflow_call` and the Actions tab

`pipeline.yml::tests` and `task-tests.yml::tests` both invoke
`tests.yml` via `uses: ./.github/workflows/tests.yml`. GitHub renders
the called workflow's jobs **nested under the caller's run** in the
Actions tab rather than as a sibling top-level row, so the per-event
count stays at one Pipeline row per master push (plus one task-tests
row per branchwork push). That nesting is the reason the Phase 4
smoke tests can assert "exactly two Pipeline runs per master push
cycle" without having to count tests as a third.

## Smoke-test evidence (Phase 4.1)

Verified against the post-Phase-2.2 run history on master
(2026-05-18). The cleanest paired example:

- HUMAN master push: `46f60b8` "chore(web): bump bundle budget
  160KB → 168KB for banner work" → Pipeline run **26066590263**:
  `Classify event (success)`, `Auto-bump patch version (success)`,
  `Tests (skipped)`, `Docker (skipped)`, `Deploy to Hetzner (skipped)`.
- BUMP commit master push: `54e4bd9` "chore: auto-bump to v0.5.46" →
  Pipeline run **26066677810**: `Classify event (success)`,
  `Tests / Web — typecheck, build (success)`,
  `Tests / E2E tests (gh + Claude) (success)`,
  `Tests / Rust (success)`, `Auto-bump patch version (skipped)`, then
  `Docker` + `Deploy to Hetzner` gated on Tests passing.

`task-tests` filtered to `headBranch=master`: zero runs. The
`branchwork/**` push filter on `task-tests.yml` plus the bare
`branches: [master]` push filter on `pipeline.yml` partitions the
trigger surface cleanly.

## Smoke-test evidence (Phase 4.2)

The task-branch counterpart to Phase 4.1. Acceptance asks that a push
to `branchwork/foo/1.1` produce exactly **one** `task-tests` run and
**zero** `Pipeline` runs. Verified on 2026-05-19 via a combination of
structural evidence (the workflow files themselves) and an empirical
baseline (no `task-tests` runs exist, so no false-positive Pipeline
runs can be hiding behind them).

### Structural evidence — trigger partition

`pipeline.yml::on` (post-Phase-2.2):

```yaml
on:
  push:
    branches: [master]
  pull_request:
    branches: [master]
  workflow_dispatch:
```

`task-tests.yml::on`:

```yaml
on:
  push:
    branches:
      - 'branchwork/**'
  pull_request:
    branches: [master]
```

`branchwork/**` appears in exactly one workflow's push filter. A push
to any `branchwork/*` ref cannot reach `pipeline.yml`'s trigger
surface — its push filter is the bare `[master]` literal, no glob, no
`branchwork/**` entry.

### Empirical baseline — `task-tests` run history

`gh run list --workflow=279130905` (the `task-tests` workflow id from
`gh workflow list`) returns an empty array as of 2026-05-19 23:42
UTC. The workflow exists, GitHub recognises it (it appears in
`gh workflow list`), but no event has fired it yet — confirming that
no task branch has been pushed to origin since the workflow shipped
in Phase 2.1 (commit `e7ba5f4`, 2026-05-18).

The wider history is consistent: `gh run list --limit 200` returns
only `master` pushes (Pipeline) and tag pushes (Release / Docker on
`v0.5.*` tags). Zero `branchwork/*` `headBranch` values across the
last 200 runs.

### Why the merge of this task does not itself fire `task-tests`

The standard Branchwork auto-mode merge flow (`merge_agent_branch_inner`
in `server-rs/src/api/agents.rs`) merges the task branch into the
canonical default **locally** on the runner host, then pushes only
the target ref (master) when `should_record_ci_run(target, default)`
returns true. The task branch itself stays a local ref — it never
crosses the wire to GitHub under normal merge.

So this Task-4.2 merge will produce one more **Pipeline** run on
master (the bump-cycle Phase-4.1 evidence already pins), but it will
NOT produce a `task-tests` run, because the `branchwork/ci-split-task-tests-from-master-pipeline/4.2`
ref it lives on is never published to origin. That is by design —
Branchwork's auto-mode loop is explicit that local task branches stay
local until an operator pushes them manually.

### Forward-looking verification recipe

The first time any operator (or future task) pushes a `branchwork/**`
ref to origin — for PR review, debugging, or a deliberate one-off
smoke run — the trigger split will be exercised live. Recipe:

```sh
git push -u origin branchwork/<plan>/<task>
gh run list --limit 5 --json databaseId,name,headBranch,event,status
```

Expected output: one row, `name: task-tests`, `event: push`,
`headBranch: branchwork/<plan>/<task>`. Zero `Pipeline` rows on that
branch. Inside the run, `gh run view <id> --json jobs` shows the
single `Tests` job with three nested children (`Tests / Rust`,
`Tests / Web — typecheck, build`, `Tests / E2E tests (gh + Claude)`)
via the `workflow_call` nesting documented in the previous section.

If both halves hold — one task-tests row, zero pipeline rows, three
nested Tests children — the split is verified live. Update this
section with the run id when that happens so the post-split smoke
test stops relying on absence-of-evidence and starts citing a real
run.

## Smoke-test evidence (Phase 4.3)

The PR-side counterpart to Phase 4.1 (human master push) and Phase
4.2 (task-branch push). Acceptance asks that opening a PR against
master produce exactly **one** `task-tests` run and **zero**
`Pipeline` runs, with the PR Checks tab showing `task-tests` as the
only check, and that merging the PR fires `Pipeline` as a separate
top-level master-push run.

Verified on 2026-05-19 via the same structural-evidence +
empirical-baseline pattern as Phase 4.2, because no PR has yet been
opened against this repository (`gh pr list --state all` returns
empty) and a sandbox-agent on a task branch cannot push to origin or
open one. The forward-recipe at the end of this section is the
load-bearing piece for whichever operator opens the first PR.

### Structural evidence — trigger partition

Phase 4.3 finalised the split by removing `pipeline.yml`'s
`pull_request:` trigger. Post-Phase-4.3 `on:` blocks:

`pipeline.yml::on`:

```yaml
on:
  push:
    branches: [master]
  workflow_dispatch:
```

`task-tests.yml::on`:

```yaml
on:
  push:
    branches:
      - 'branchwork/**'
  pull_request:
    branches: [master]
```

`pull_request:` appears in exactly one workflow's trigger block —
`task-tests.yml`. A PR against master cannot reach `pipeline.yml`'s
trigger surface because no `pull_request` key exists in its `on:`
block. The `classify` step inside `pipeline.yml` lost its `is_pr`
variable and the corresponding `should_test=true` arm at the same
time — same surgical edit Phase 2.2 applied for `is_task_branch_push`.

The earlier doc note that "Pipeline retains its `pull_request:
master` trigger but the test job is gated on `should_test`" no longer
applies — both halves were dropped together to keep the workflow
honest at the `on:` level rather than relying on classifier output to
suppress the run.

### Empirical baseline — `pull_request` run history

`gh run list --workflow=279031709 --event=pull_request` (Pipeline
workflow id) returns an empty array as of 2026-05-19 — Pipeline has
NEVER fired on a `pull_request` event, even though its trigger surface
included one until this Phase 4.3 change. The same query against
`task-tests` (workflow id 279130905) also returns empty: no PR has
yet been opened against this repo at all.

`gh run list --limit 200 --json event` confirms the wider history is
event=push (102) + event=workflow_run (97) + event=workflow_dispatch
(1), with zero `pull_request` events across the full run window. No
historical PR run can be hiding behind the empty list — the absence
is honest, not a query mistake.

### Why this task-branch merge does not itself fire `task-tests` on a PR

The standard Branchwork auto-mode merge flow merges the task branch
into the canonical default **locally** on the runner host and pushes
only the target ref (master), exactly as documented in the Phase 4.2
section above. The task branch ref never crosses the wire to GitHub,
so it cannot be the head of a PR. The merge of this Phase 4.3 task
will produce one more **Pipeline** run on master (the bump-cycle
Phase 4.1 evidence already pins) and zero PR-side artefacts.

### Forward-looking verification recipe

The first time any operator opens a PR against master — for review,
external contribution, or a deliberate smoke run — the PR-side trigger
partition will be exercised live. Recipe:

```sh
# Open the PR (assumes a feature branch already pushed to origin)
gh pr create --base master --head <branch> --title <title> --body <body>

# Wait for the workflow runs to appear (~2s)
gh run list --limit 5 --json databaseId,name,headBranch,event,status
```

Expected output: one row, `name: task-tests`, `event: pull_request`,
`headBranch: <branch>`. Zero `Pipeline` rows associated with the PR
(filterable via `--event=pull_request --workflow=279031709`).

In the GitHub PR Checks tab: one entry titled "Tests" (the reusable
workflow's job name) with three nested children — `Tests / Rust`,
`Tests / Web — typecheck, build`, `Tests / E2E tests (gh + Claude)`.
No `Classify event`, no `Auto-bump patch version`, no `Docker`, no
`Deploy to Hetzner` checks; those only exist inside `pipeline.yml`,
which the PR cannot trigger.

When the PR merges, the resulting master push fires `Pipeline` as a
fresh top-level run — the human-master-push path from Phase 4.1
takes over (`classify` + `bump`, then a follow-up bump-commit run
with `classify` + nested `Tests` + `docker` + `deploy`).

If all three halves hold — one task-tests row on the PR, zero
Pipeline rows on the PR, and one Pipeline run fires on merge — the
PR-side split is verified live. Update this section with the run ids
when that happens so the post-split smoke test stops relying on
absence-of-evidence and starts citing real runs.

This file is the canonical place to document follow-up changes to
either workflow's trigger or gating logic — keep the per-event table
above honest as the split evolves.
