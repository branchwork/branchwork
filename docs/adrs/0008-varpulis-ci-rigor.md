# ADR 0008 — Apply Varpulis-level CI rigor to Branchwork

- **Status:** Proposed (2026-05-17)
- **Authors:** cpo
- **Decision driver(s):** Branchwork's CI catches compile/test/clippy
  regressions but is silent on license violations, unused deps, stale
  intra-doc links, MSRV drift, and coverage trends. Varpulis (`cep`)
  has accumulated those levers and they pay off — Branchwork should
  adopt the same set, one PR per lever, so a single bad rule doesn't
  block the others.

## Context

[`cep`](https://github.com/branchwork/cep) — internally referred to
as Varpulis — and Branchwork share the same author conventions, the
same crate ecosystem, and the same review cadence. Their CI shapes
have diverged because cep's CI accreted rigor levers over months while
Branchwork's stayed at the baseline. The gap shows up as silent
failure modes Branchwork has no signal for today (a GPL-licensed
transitive dep landing in `Cargo.lock`, a `cargo machete`-detectable
unused crate lingering after a refactor, a `[FunctionName]`-style
intra-doc link rotting after a rename, an MSRV-incompatible language
feature creeping in).

### cep vs Branchwork CI shapes (verified 2026-05-07)

| Concern | cep (`.github/workflows/ci.yml`, **359 lines, ~12 jobs**) | Branchwork (`.github/workflows/ci.yml`, **118 lines, 3 jobs**) |
|---|---|---|
| `cargo fmt --check` | ✓ (dedicated `fmt` job, nightly) | ✓ (inside matrix, stable) |
| `cargo clippy -- -D warnings` | ✓ (dedicated `clippy` job) | ✓ (inside matrix) |
| `cargo test` | ✓ (`test` + `cross-platform` jobs) | ✓ (inside matrix, ubuntu + windows) |
| `cargo check` on MSRV | ✓ (`msrv` job, Rust 1.93) | ✗ |
| `cargo deny check` (advisories + licenses + bans + sources) | ✓ (`deny` job, `deny.toml` at root) | ✗ |
| `cargo audit` (advisories only) | ✓ (`audit` job, kept alongside deny) | ✓ (advisory-only) |
| `cargo machete` (unused deps) | ✓ (`machete` job) | ✗ |
| `cargo doc -D warnings` | ✓ (`doc` job, `RUSTDOCFLAGS="-D warnings"`) | ✗ |
| `cargo llvm-cov` → Codecov | ✓ (`coverage` job, `codecov.yml` project 70% / patch 60%) | ✗ |
| `rustfmt.toml` (`imports_granularity`, `group_imports`) | ✓ (nightly-only options; nightly fmt job) | ✗ (default rustfmt config) |
| `cargo semver-checks` (public-API library checks) | ✓ (`semver` job) | n/a (binary repo — see below) |
| WASM build (`wasm32-unknown-unknown`) | ✓ (`wasm` job for `varpulis-wasm`) | n/a (no WASM crate) |
| Chaos / fuzz / bench harnesses | ✓ (`chaos-tests` + parser fuzz + bench jobs) | n/a (no parser/protocol surface today) |
| Feature-flag matrix | ✓ (`feature-flags` job, several feature axes) | n/a (small feature surface) |
| Docker image build | separate workflow | separate workflow |

The `n/a` rows are deliberate — see [Rejected alternatives](#rejected-alternatives).
The `✗` rows are the gap this ADR closes.

### Non-negotiables for the rollout

1. **One lever per PR.** Each phase ships independently. A failed
   `cargo machete` rule must not block `rustfmt.toml` adoption.
2. **No silenced rules without a reason.** Every entry in
   `deny.toml`'s `[advisories.ignore]` carries a `reason` string
   (matches cep convention). Every `[package.metadata.cargo-machete]
   ignored = [...]` entry has a one-line comment explaining why the
   dep looks unused but isn't.
3. **No coverage gate before baseline measurement.** Phase 6 enables
   coverage *reporting* first; the project/patch thresholds in
   `codecov.yml` start permissive (50% / 50%) and can be raised once
   real numbers are visible. Failing builds on coverage from day one
   is how teams disable coverage entirely.
4. **MSRV pin is descriptive, not aspirational.** Pick the version
   `cargo +stable build` succeeds on today, minus one stable release.
   Not an old version contributors are *hoped* to respect.
5. **No new dev dependencies in production targets.** `cargo machete`,
   `cargo deny`, `cargo llvm-cov` are CI-only — installed via
   `taiki-e/install-action` or `cargo install --locked`, not added to
   `Cargo.toml`. Same pattern cep uses.

## Decision

Adopt six rigor levers, **in this order, one PR per lever**:

1. **`cargo deny`** — replace `cargo audit` with `cargo deny check`
   (advisories + licenses + sources + bans). Strictly broader than
   audit; `deny.toml` lives at repo root with a `[advisories.ignore]`
   bootstrap list that carries `reason` strings.
2. **`cargo machete`** — catch unused deps. Each tolerated
   false-positive carries a `# why` comment above the
   `[package.metadata.cargo-machete] ignored = [...]` entry.
3. **`rustfmt.toml` with import grouping** — adopt cep's two-line
   config (`imports_granularity = "Module"`,
   `group_imports = "StdExternalCrate"`). Requires nightly rustfmt;
   the `cargo fmt --check` step in CI switches to nightly.
4. **`cargo doc -D warnings`** — catch stale `///` intra-doc links.
   Fix existing breakage in the same PR.
5. **MSRV pin** — set `rust-version = "1.X"` in `server-rs/Cargo.toml`
   (and any other workspace member with its own `[package]`). Detect
   the binding lower bound by trying `cargo +1.91 build`, `1.90`,
   `1.89`, …, then bump one stable release up. Add a dedicated `msrv`
   job that runs `cargo check` (not `cargo test`).
6. **`cargo llvm-cov` + Codecov** — coverage *reporting* (not gating)
   first. `codecov.yml` starts at project 50% / patch 50%, tightened
   in a follow-up PR after the first three uploads stabilise.

Each lever lives in its own phase of
[`branchwork-ci-rigor.yaml`](../../../.claude/plans/branchwork-ci-rigor.yaml).
Phase ordering reflects payoff per unit of contributor disruption
(deny + machete catch real bugs immediately; coverage is informational
until thresholds tighten). Skipping or reordering individual levers is
fine — the only ordering constraint is that `cargo audit` is removed
in lever 1 (deny supersedes it), so any later lever that mentions
`cargo audit` is wrong.

### Skipped rigor levers (one-line rationale each)

- **`cargo semver-checks`** — applies to public-API libraries.
  Branchwork ships binaries (server, runner, session daemon), not
  crates.io artefacts. Skip.
- **WASM build target** — cep's `varpulis-wasm` crate has no
  Branchwork analogue. Skip.
- **Chaos / fuzz / bench workflows** — cep's parser, message bus, and
  chaos harness are protocol-level concerns; Branchwork is an
  application and has no parser surface worth fuzzing today. Skip —
  revisit if `PlanParser` or the supervisor IPC frame format ever
  warrants it.
- **Feature-flag matrix** — Branchwork's feature surface is small
  enough that the existing `cargo build --release` exercises it.
  Skip.

## Consequences

- **CI fans out from 3 jobs to ~9.** Push-to-PR latency grows by
  ~3 min on the slow path (the coverage job, which runs unoptimised
  builds with instrumentation). The fast path (matrix Rust + Web)
  stays roughly the same because the new jobs run in parallel.
- **First-time contributors see new failure modes** — license
  violations, unused deps, stale intra-doc links, MSRV regressions,
  Codecov status checks. Each phase's acceptance criteria require a
  passing local run **before** the PR opens, so contributors can
  reproduce CI failures locally instead of treating CI as the only
  signal. The phase task descriptions enumerate the exact local
  invocation (`cargo deny check`, `cargo machete`,
  `cargo +nightly fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc
  --no-deps --workspace`, `cargo +1.X build --release --workspace`,
  `cargo llvm-cov --workspace --lcov`).
- **The `cargo audit` step is removed in phase 1.2** (`deny` covers
  advisories). Anyone reading old CI history — green checkmarks on
  PRs from before 2026-05-17 carrying an `audit` step that no longer
  exists — should be pointed at this ADR. The `audit:` job in cep is
  kept *alongside* deny because cep's history of finding real
  advisories there is longer; Branchwork has no such history and
  letting deny own advisories avoids the cost of running both.
- **`branchwork.toml` is not affected.** This ADR is about CI; ADR
  0006 governs per-plan/per-phase CI workflow filtering and verify
  commands. The two surfaces are independent — a stricter Branchwork
  CI doesn't change what `ci_blocking_workflows` resolves to for any
  plan.
- **No new runtime deps.** All six levers are CI-only tools installed
  in workflow steps. `Cargo.toml` is unchanged outside of the MSRV
  pin in phase 5.

## Rejected alternatives

### Adopt all six levers in one PR

**Rejected.** A single bad rule (e.g. a license entry that
inadvertently bans a transitive dep, or an `[advisories.ignore]`
bootstrap that's incomplete and turns the deny job red on a
real-but-low-priority advisory) would block every other lever from
landing. The plan structure of one phase per lever lets the rigor
effort make forward progress even if one phase needs a follow-up
fix. The cost of six PRs over six days versus one PR over one day is
six review cycles — acceptable for permanent infrastructure changes.

### Match cep's CI set verbatim

**Rejected.** WASM, chaos, fuzz, bench, and `cargo semver-checks`
don't apply to an application binary repo (justified per-lever in the
[Skipped rigor levers](#skipped-rigor-levers-one-line-rationale-each)
list above). Forcing them in would either produce always-vacuous
jobs (no `wasm32` crate exists, so the wasm job would have nothing
to build) or block PRs on inapplicable concerns (semver-checks would
flag every internal refactor as a public-API break, because the
"public API" is `branchwork-server`'s entire crate surface today).

## References

- Implementation plan:
  [`~/.claude/plans/branchwork-ci-rigor.yaml`](../../../.claude/plans/branchwork-ci-rigor.yaml)
  (Phase 0 — this ADR; Phase 1 — `cargo deny`; Phase 2 —
  `cargo machete`; Phase 3 — `rustfmt.toml`; Phase 4 —
  `cargo doc -D warnings`; Phase 5 — MSRV pin; Phase 6 — coverage).
- Branchwork CI today: [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml).
- cep CI reference: `/home/cpo/cep/.github/workflows/ci.yml`
  (off-repo, snapshot 2026-05-07).
- cep `deny.toml` reference: `/home/cpo/cep/deny.toml` (off-repo).
- cep `rustfmt.toml` reference: `/home/cpo/cep/rustfmt.toml` (off-repo).
- cep `codecov.yml` reference: `/home/cpo/cep/codecov.yml` (off-repo).
- Adjacent ADR on per-plan CI workflow filtering (orthogonal — not a
  CI-rigor lever, but informs `ci_blocking_workflows`):
  [ADR 0006](0006-phase-verify-and-ci-filter.md).
- Prior ADR style references: [ADR 0002](0002-worktree-per-agent-isolation.md),
  [ADR 0005](0005-e2e-tests-must-be-containerized.md),
  [ADR 0006](0006-phase-verify-and-ci-filter.md).

> **Note on ADR number.** The plan brief named this file
> `0006-varpulis-ci-rigor.md`, but ADR 0006 was already taken by
> `0006-phase-verify-and-ci-filter.md` at the time this plan was
> authored (2026-05-07). Filed at 0008 (next free index after
> 0001–0007); the plan's task description retains the original number
> for historical grep value. This mirrors the same situation ADR 0006
> itself documents (originally drafted as 0003).
