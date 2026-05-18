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
| PR against master                    | `task-tests`          | tests (nested) — Pipeline does not fire on PRs in this split today\* |
| `workflow_dispatch` on `pipeline.yml`| `Pipeline`            | classify + tests + docker + deploy            |
| Tag push (`v*`)                      | `tag-build`           | release artifact build                        |

\* `pipeline.yml`'s `on: pull_request: master` trigger is retained but
the `tests` job inside it is gated on `should_test` and the
classifier sets `should_test=true` for PRs, so it does fire on PRs.
The Phase 4.3 smoke test will clarify the post-split PR behaviour;
this matrix tracks what is currently shipped.

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

This file is the canonical place to document follow-up changes to
either workflow's trigger or gating logic — keep the per-event table
above honest as the split evolves.
