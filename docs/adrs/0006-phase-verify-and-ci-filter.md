# ADR 0006 — Phase-end verify + per-plan CI workflow filter

- **Status:** Accepted (2026-05-08)
- **Authors:** cpo
- **Decision driver(s):** unrelated workflows poisoning the CI verdict on every merged SHA; phase-end verify only firing at plan-end-merge instead of per-phase; users wanting an explicit, UI-editable surface rather than relying on auto-detection alone

## Context

Two pieces of CI-aware behaviour in Branchwork were too coarse for
real-world repos.

### Single-verdict CI aggregation poisoning unrelated jobs

Before this change, `ci/aggregate.rs::compute` treated every workflow on
a merged SHA equally: any failure flipped the aggregate `conclusion` to
`failure`. For repos like `cep` that run a monolithic `CI` workflow
alongside `Docker`, `Docker Publish`, `release`, `bench`, and `fuzz`, a
slow Docker build, a flaky bench, or a release-only job blocked merge
advancement just as hard as a real test or clippy failure. Auto-mode
paused, the dashboard turned red, and the user had to chase an unrelated
red badge before they could land work that was passing the workflows
they actually cared about.

The user's framing on 2026-05-07: **"the real CI workflows should block,
and packaging/deploy/release should be informative-only."** The fix is
to partition runs into a *blocking* subset and an *informational* subset,
and only let the blocking subset poison the verdict.

### Phase-end verify only at plan-end-merge

The pre-existing top-level `verification:` field on a plan fires a
plan-level Check agent at **plan-end-merge** — i.e. when every phase is
done. That's the right granularity for "does the plan match the spec?"
but it leaves a coarser gap: most users want to run a project-defined
verify suite (cep's `bash scripts/verify.sh` doing fmt + clippy + audit
+ deny) at **every phase merge**, so a phase can't quietly advance with
broken hygiene.

The user's framing on 2026-05-07 (recorded in the plan context): the
verify suite should fire at the moment the last task in a phase merges,
running against the merge commit, with non-zero exit pausing the plan
the same way a CI failure would. Per-task verify is **explicitly out
of scope** — see [Rejected alternatives](#rejected-alternatives).

### Why a four-layer precedence (not a single global config)

Different projects in the same Branchwork install have different CI
shapes. cep wants `[CI, lint, typecheck]` blocking; another project
might run only `CI`; a third might have its own taxonomy. Plans within
a project mostly inherit the project default but occasionally need to
narrow it (a hardening phase that wants `CI && deny check` while every
other phase keeps the cheaper baseline). The four layers — per-phase
< per-plan < repo defaults < smart classifier — match the natural unit
the user is editing at the moment they reach for the field, without
forcing them to repeat themselves at every layer.

## Decision

### Two new YAML fields

`YamlPlan` and `YamlPlanPhase`
([`server-rs/src/plan_parser.rs`](../../server-rs/src/plan_parser.rs))
each gain two optional fields:

- `ci_blocking_workflows: Option<Vec<String>>` — allowlist of GitHub
  workflow names. Names match case-sensitively against the workflow's
  top-level `name:` field, with the `.github/workflows/<file>.yml`
  filename stem as fallback when `name:` is absent.
- `phase_verification: Option<String>` — shell command run by the
  phase-end Check agent, e.g. `bash scripts/verify.sh`.

Both fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`
so existing plans round-trip unchanged and only the explicit `Some`
appears on disk.

### Repo-level `branchwork.toml`

A new `branchwork.toml` at the project root
([`server-rs/src/repo_config.rs`](../../server-rs/src/repo_config.rs);
documented at [`docs/reference/branchwork-toml.md`](../reference/branchwork-toml.md))
holds the same two values as repo-wide defaults:

```toml
[ci]
blocking_workflows = ["CI", "lint", "typecheck"]
# OR (deny-list alternative):
# blocking_workflows_skip = ["Docker", "Deploy", "Publish"]

[phase]
verification = "bash scripts/verify.sh"
```

Parse errors log a stderr warning and cache `None` so the same broken
file doesn't re-warn on every poll. Parsed values are cached with
mtime invalidation so the loader is cheap on the hot path.

### Four-layer precedence

Resolution lives in
[`server-rs/src/ci/resolution.rs`](../../server-rs/src/ci/resolution.rs)
as two pure functions over already-parsed inputs. First match wins:

| Layer | `ci_blocking_workflows` | `phase_verification` |
|---|---|---|
| 1. Per-phase | `YamlPlanPhase.ci_blocking_workflows` | `YamlPlanPhase.phase_verification` |
| 2. Per-plan | `YamlPlan.ci_blocking_workflows` | `YamlPlan.phase_verification` |
| 3. Repo defaults | `branchwork.toml [ci] blocking_workflows` | `branchwork.toml [phase] verification` |
| 4. Smart default | regex `(?i)docker\|deploy\|publish\|release\|bench\|fuzz` matches → informational, everything else → blocking | `None` (verify is opt-in — no smart fallback) |

Each layer is independent per field. Setting `[ci] blocking_workflows`
in `branchwork.toml` does not suppress a plan-level `phase_verification`
and vice versa.

An **explicit empty list** (`ci_blocking_workflows: []`) at any layer is
a "nothing blocks" configuration, not a fall-through. `None` is what
triggers fall-through. The smart classifier in layer 4 runs **only**
when every higher layer is `None`; an explicit list at layers 1-3 is
returned verbatim, even if the classifier would have demoted some of
those names. This is what gives the user a way to say "I know the
classifier disagrees; here is the exact list I want."

### Filter applied at aggregation time

[`server-rs/src/ci/aggregate.rs::compute_with_filter`](../../server-rs/src/ci/aggregate.rs)
takes the resolved allowlist and partitions runs into a blocking and an
informational subset. `CiAggregate::conclusion` and `failing_run_id`
both derive from the blocking subset only; informational runs surface
as `informational: true` on the wire so the dashboard can still render
them. Every filter decision is logged at INFO with run id, workflow
name, classification (blocking / informational), and source
(via-config / via-classifier) so audit trails exist.

The runner side
([`server-rs/src/bin/branchwork_runner.rs`](../../server-rs/src/bin/branchwork_runner.rs))
re-uses the same aggregation entry point. The server resolves layers
1-3 (which require plan/repo state the runner doesn't carry), ships the
resolved `Option<Vec<String>>` to the runner via a new
`WireMessage::GetCiRunStatus.blocking_workflows` field, and the runner
applies layer 4's classifier locally over the workflow names it
discovers via `gh`. `None` on the wire = "no explicit list, classify
locally"; `Some(list)` = "use this allowlist verbatim."

### Phase-end verify

[`server-rs/src/agents/phase_check.rs`](../../server-rs/src/agents/phase_check.rs)
subscribes to a new `phase_completed` broadcast event emitted by
`agents::try_auto_advance` when the last task in a phase merges. On
each event the listener resolves `phase_verification` for that phase;
if `Some`, it spawns a Check agent on a fresh git worktree at the merge
SHA, with the verify command embedded in the prompt. Worktrees are
cleaned up on every exit path (success or failure).

On `passed` verdict the listener calls `advance_after_phase_verify`
which scans the next phase's ready tasks and spawns them — bypassing
the gate that `phase_completed` had set in `try_auto_advance` so the
chain doesn't double-broadcast. On `failed` verdict the plan is paused
with `paused_reason: phase_verify_failed: <short>`, surfaced in the
dashboard the same way a CI failure pause is.

### UI surface

Plan board's **Settings** tab
([`web/src/components/PlanSettings.tsx`](../../web/src/components/PlanSettings.tsx))
exposes the resolved allowlist + verify command, the source layer for
each, and an Override / Inherit toggle that writes back via
`PUT /api/plans/:name/settings`. The PUT path uses a line-based YAML
editor
([`server-rs/src/api/plans.rs::update_yaml_top_level_key`](../../server-rs/src/api/plans.rs))
that preserves comments — round-tripping through serde_yaml would
have dropped them, and comments on plans are common.

Per-phase overrides live in an accordion under each phase header
([`web/src/components/PhaseHeader.tsx`](../../web/src/components/PhaseHeader.tsx))
with the same Override / Inherit affordance.

## Consequences

### Positive

- **Real-CI failures still block; packaging failures don't.** The cep
  shape (`CI` failing alongside `Docker:success`) now writes
  `conclusion=failure` and a deep link to the CI run, instead of
  silently riding the most-recent-workflow's verdict — which was
  previously the headline bug for the dashboard CI poller.
- **Phase merges run a project-defined verify suite without a per-task
  cost.** A 30-second `cargo deny check && cargo audit` runs once per
  phase boundary instead of once per task, which means a 12-task phase
  costs the same as a 1-task phase.
- **Single source of truth for "is this workflow blocking?"** Both the
  auto-mode merge gate and the dashboard CI poller call the same
  `get_ci_run_status_dispatch` → `compute_with_filter` path, so they
  cannot disagree.
- **Explicit user-editable surface.** Users no longer rely solely on
  the smart classifier; they can pin `ci_blocking_workflows` per-plan
  or `branchwork.toml`-wide, and the Settings tab tells them which
  layer is winning.
- **Pure resolution function.** The four-layer logic is a pair of pure
  Rust functions over already-parsed inputs (no DB, no filesystem),
  exhaustively covered by unit tests.

### Negative / preserved gaps

- **Smart classifier is a heuristic.** A workflow named `release-tests`
  is classified as informational by default (matches `release`),
  even though the user might want it to block. The escape hatch is to
  set `ci_blocking_workflows` explicitly at any layer; the
  user-editable Settings tab is the discoverability surface for that
  hatch.
- **Names match case-sensitively.** `branchwork.toml` listing `CI`
  doesn't match a workflow whose `name:` is `ci`. Resolution.rs owns
  normalisation; the design choice is to leave names verbatim so
  `Docker Publish` (with the space) doesn't accidentally collide with
  `docker-publish`.
- **`branchwork.toml` is per-project, not per-org or per-install.**
  Two unrelated projects in the same `~/projects/` tree need their own
  `branchwork.toml`. Intentional — repo-wide defaults are the natural
  unit; per-install would force a single CI shape on every project.
- **Phase verify runs once at phase-end, not on every task merge.** A
  task that breaks the verify suite mid-phase will only surface at
  phase-end. The trade-off is documented in
  [Rejected alternatives](#rejected-alternatives) below; the auto-fix
  loop catches per-task CI failures via the existing aggregate path,
  which is the per-task signal users actually have.

### Migration

- No DB schema change for the resolution helpers themselves; the
  pre-existing `ci_runs` table picks up the corrected verdicts via
  the one-shot `ci::backfill_aggregates` startup routine
  ([`server-rs/src/ci.rs`](../../server-rs/src/ci.rs); gate
  `ci_backfill_v1_done` in the new `settings` key/value table).
- Existing plans without `ci_blocking_workflows` / `phase_verification`
  parse unchanged; the smart classifier is the layer-4 default for the
  CI filter, and verify is a no-op when no layer sets it.
- Markdown plans cannot express either field —
  [`parse_plan_markdown`](../../server-rs/src/plan_parser.rs) defaults
  both to `None`. To use either field, port the plan to YAML.
- The pre-existing top-level `verification:` field is unchanged — it
  still drives the **Check Plan** button's plan-end-merge agent. The
  new `phase_verification` is a separate field with a different
  trigger (phase-end-merge).

## Rejected alternatives

### Per-task verify

Wire the verify command to fire at every task merge instead of every
phase merge. **Rejected** by the user on 2026-05-07 on cost-vs-granularity
grounds:

- **Cost.** A typical hardening phase has ~10 tasks. cep's
  `scripts/verify.sh` is ~30s; running it 10× per phase adds 5 min
  of latency every phase, vs. ~30s once at phase-end.
- **Granularity.** Per-task verify catches a broken task one merge
  earlier, but the aggregate CI verdict already catches per-task
  regressions via the auto-fix loop. The thing per-task verify would
  uniquely catch is a regression that GitHub Actions CI does not
  cover but a `branchwork.toml`-defined script does — rare enough to
  be a phase-end check, not a per-task one.
- **User explicitly framed it.** The user said phase-level is the
  natural unit; per-task is an opt-in feature for a separate plan if
  it's ever wanted, gated behind a different field name.

### Single global config (no per-plan / per-phase override)

Have `branchwork.toml` be the only knob; drop the YAML fields. Rejected
because:

- Plans within a project genuinely diverge — a hardening phase wants a
  stricter verify than the baseline, and a fix-CI phase wants a wider
  blocking allowlist than the rest of the plan. Forcing them through
  `branchwork.toml` either pollutes the project default for every
  plan or pushes users toward a "current `branchwork.toml`, then swap
  it back" hack.
- The four layers cost almost nothing to implement: two small pure
  functions in `resolution.rs`, ~150 lines total. The expressivity is
  worth it.

### Inverse-only deny list (drop `blocking_workflows`, keep only
`blocking_workflows_skip`)

Express the allowlist exclusively as the things to *not* block —
"everything blocks except `Docker`, `release`, etc." Rejected because:

- New workflows added to a project would silently start blocking the
  pipeline. The allowlist form fails closed: a new workflow is
  informational by default until the user adds it explicitly. With
  the deny list, a new workflow is blocking by default — the *less*
  safe failure mode for a CI gate.
- We support `blocking_workflows_skip` as an alternative phrasing in
  `branchwork.toml`, but `blocking_workflows` (the allowlist) is the
  recommended form, and the smart classifier in layer 4 is itself a
  deny list — anyone who wants the deny shape gets it for free via
  the classifier.

### Smart classifier as the only fallback (no `branchwork.toml`)

Drop the file entirely; rely on the regex classifier when the plan
doesn't pin a list. Rejected because:

- The classifier's match set is a guess. It catches the common
  packaging / deploy / release shape but misses project-specific
  naming (`integration-test`, `acceptance-suite`) that a project knows
  is non-blocking. Without a repo-level override, the user has to
  copy the same allowlist into every plan.
- `branchwork.toml` is also the natural home for `phase_verification`
  — that field has no smart-default fallback, so without the file the
  feature would have to be plan-by-plan.

### Make verify a CI workflow

Have phase-end verify just be another GitHub Actions workflow that
runs on the merge commit; let the existing CI aggregate do the rest.
Rejected because:

- Most teams already have a CI workflow; phase verify is meant to be
  a project-local script the user invokes outside the GH Actions cost
  model (`cargo deny check` runs in 3s locally vs ~45s on the GHA
  free tier). Pushing it into a GHA workflow loses that.
- The Check agent affordance — running the script in a fresh worktree
  at the merge SHA, surfacing the failure log inline in the dashboard,
  optionally feeding into the auto-fix loop — is something a GHA
  workflow can't provide. The existing Check-agent pipeline is the
  right runtime for it.

## References

- Implementation plan: `~/.claude/plans/branchwork-phase-verify-and-ci-filter.yaml`
  (Phase 0 — schema + resolution; Phase 1 — CI aggregate filter; Phase
  2 — phase-end verify Check agent; Phase 3 — UI; Phase 4 — these docs).
- Resolution helpers: [`server-rs/src/ci/resolution.rs`](../../server-rs/src/ci/resolution.rs).
- CI aggregate filter: [`server-rs/src/ci/aggregate.rs`](../../server-rs/src/ci/aggregate.rs)
  (`compute_with_filter`, `BlockingFilter`, `is_workflow_blocking_by_default`).
- Phase-end Check agent listener: [`server-rs/src/agents/phase_check.rs`](../../server-rs/src/agents/phase_check.rs).
- `phase_completed` event emitter: [`server-rs/src/agents/mod.rs`](../../server-rs/src/agents/mod.rs)
  (`try_auto_advance`).
- Repo-level loader: [`server-rs/src/repo_config.rs`](../../server-rs/src/repo_config.rs);
  doc at [`docs/reference/branchwork-toml.md`](../reference/branchwork-toml.md).
- Plan-schema doc: [`docs/reference/plan-schema.md`](../reference/plan-schema.md)
  (the "CI workflow filter" and "Phase-end verification" subsections
  cover the user-facing surface).
- Settings UI: [`web/src/components/PlanSettings.tsx`](../../web/src/components/PlanSettings.tsx),
  [`web/src/components/PhaseHeader.tsx`](../../web/src/components/PhaseHeader.tsx).
- Plan API: [`server-rs/src/api/plans.rs`](../../server-rs/src/api/plans.rs)
  (`get_plan_settings`, `put_plan_settings`, `get_phase_settings`,
  `put_phase_settings`).
- Dashboard CI poller migrated to the aggregate path:
  [`server-rs/src/ci.rs`](../../server-rs/src/ci.rs) (`poll_once`,
  `backfill_aggregates`).
- Prior ADR pattern: [ADR 0002](0002-worktree-per-agent-isolation.md),
  [ADR 0004](0004-unify-check-prompts.md).

> **Note on ADR number.** The plan brief named this file
> `0003-phase-verify-and-ci-filter.md`, but ADR 0003 was already taken
> by `0003-unattended-auto-mode.md` at the time this plan was authored
> (2026-05-07). Filed at 0006 (next free index after 0001–0005); the
> plan's task description retains the original number for historical
> grep value.
