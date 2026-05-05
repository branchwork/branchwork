# Docker build perf — baseline (2026-05-05)

Status quo for `.github/workflows/docker.yml` before any of the
optimizations in `plans/docker-build-perf.yaml`. Cold and warm wall
clock numbers, per-step breakdown, longest substep, cache-hit rate.
The numbers below are referenced by the acceptance criteria of phases
1 and 2.

## Setup

Both runs are `build-and-push` of `deploy/Dockerfile` on
`linux/amd64,linux/arm64`, on `ubuntu-latest`, with
`cache-from/to: type=gha,mode=max`. The Dockerfile has 3 stages
(`web` → `server` → `runtime`); stages 1 and 2 build twice (once per
arch), stage 3 produces the published images.

| Run | ID | Trigger | SHA | Note |
|---|---|---|---|---|
| Cold | [25371587513](https://github.com/branchwork/branchwork/actions/runs/25371587513) | `workflow_run` (after CI green) | 79dbd2b | First-ever run of `docker.yml`; GHA cache empty |
| Warm | [25376229417](https://github.com/branchwork/branchwork/actions/runs/25376229417) | `workflow_dispatch` | 79dbd2b | Same SHA, full GHA cache from cold run |

The warm run was triggered via `gh workflow run docker.yml --ref
master` instead of pushing a no-op commit. Branchwork's unattended-
execution contract forbids agents from pushing to master directly. A
no-op commit (whitespace in a Dockerfile comment) would have given an
equivalent measurement: BuildKit hashes per-instruction inputs, so a
comment-only change does not propagate to any `RUN` / `COPY` layer's
cache key. Treat the warm number as the upper bound of cache
effectiveness — a Cargo.lock change would invalidate stage 2's deps
layer and re-pay most of the Rust build cost.

## Total wall clock

From `gh run view --json updatedAt,createdAt` (run level — includes
queueing). Job- and step-level numbers come from
`/repos/.../actions/runs/<id>/jobs`.

|        | Cold       | Warm   | Speedup |
|--------|-----------:|-------:|--------:|
| Run (createdAt → updatedAt) | **1h 21m 09s** (4869s) | **56s** | **87×** |
| Job (started_at → completed_at) | 1h 21m 02s (4862s) | 49s | 99× |
| `Build and push` step | 1h 20m 36s (4836s) | 19s | 254× |
| Job overhead (job − step) | 26s | 30s | — |

The cold run is *entirely* dominated by the `Build and push` step
(99.4% of run wall clock). The warm run's overhead is mostly fixed
runner / Buildx setup (~30s); the actual rebuild fits in 19s of
which most is GHCR push + GHA cache export, not compute.

## Per-step breakdown

GitHub-Actions step durations (from the REST `…/jobs` payload).
"Post X" steps are GHA's automatic teardown for action `X`.

| Step                          | Cold       | Warm |
|-------------------------------|-----------:|-----:|
| Set up job                    | 6s         | 5s   |
| Run `actions/checkout@v4`     | 2s         | 1s   |
| Set up QEMU                   | 5s         | 8s   |
| Set up Docker Buildx          | 4s         | 5s   |
| Log in to GHCR                | 1s         | 1s   |
| Extract metadata              | 1s         | 1s   |
| **Build and push**            | **4836s**  | **19s** |
| Post Build and push           | 2s         | 3s   |
| Post Log in to GHCR           | 0s         | 0s   |
| Post Set up Docker Buildx     | 2s         | 0s   |
| Post Set up QEMU              | 3s         | 1s   |
| Post Run `actions/checkout@v4`| 0s         | 0s   |
| Complete job                  | 0s         | 0s   |

## Build-and-push internals (cold) — longest substeps

BuildKit DAG nodes inside the cold `Build and push`. The arm64 Rust
build is the single biggest line item by a 5× margin over anything
else. Numbers from the buildx `#NN DONE Xs` lines in the run log.

| #  | Step                                                                | Duration |
|----|---------------------------------------------------------------------|---------:|
| 41 | `[linux/arm64 server 6/9]` `cargo build --release` (deps prebuild)  | **58m 01s** (3481.6s) |
| 56 | `[linux/arm64 server 9/9]` `cargo build --release --bin …`          | 18m 18s (1098.1s) |
| 33 | `[linux/amd64 server 6/9]` `cargo build --release` (deps prebuild)  | 9m 36s (575.5s) |
| 50 | `[linux/amd64 server 9/9]` `cargo build --release --bin …`          | 4m 03s (242.6s) |
| 47 | `[linux/arm64 web 9/9]` `pnpm --filter @branchwork/web build`       | 4m 00s (240.0s) |
| 62 | `exporting to GitHub Actions Cache`                                 | 3m 50s (229.9s) |

The arm64 stage-2 chain (#41 → #56) alone is **1h 16m 20s = 94.7% of
`Build and push`**. The amd64 stage-2 chain (#33 → #50) is **13m
39s**, a 5.6× ratio against arm64 for the same source — the QEMU
emulation tax. The web stage runs faster on amd64 (~30s) than on arm64
(4m, also QEMU-bound), but at this scale it's a rounding error against
the Rust build.

**The single longest substep is #41, `[linux/arm64 server 6/9]`
running the dummy-source `cargo build --release` to prime the deps
layer, at 58m 01s.** This is what `plans/docker-build-perf.yaml`
phase 1 (zigbuild) targets directly: replacing QEMU-emulated arm64
with native cross-compile collapses #41 + #56 from ~76m to ~5m at
amd64-native speed.

## Cache-hit rate

From the buildx output (`CACHED` markers).

|        | Buildable layers | CACHED | Hit rate |
|--------|-----------------:|-------:|---------:|
| Cold   | 40               | 0      | 0%       |
| Warm   | 40               | 40     | 100%     |

40 here counts the `RUN` / `COPY` / `FROM`-resolution layers across
both arches (the BuildKit DAG has 63 nodes total; the other 23 are
metadata loads, base-image auth, image export, GHCR push, and GHA
cache export — they re-run regardless of cache state).

In the warm run, the only non-trivial wall-clock contributors were
**#61 (`exporting to image` + GHCR push) at 16.8s** and **#62 (GHA
cache export) at 7.4s** — both pure I/O, not compute. Any future
"warm" measurement that does not invalidate stage 2's deps layer
should land in the same 15-25s range for `Build and push`.

## What this means for the plan

- Phase 1 acceptance ("post-zigbuild cold + warm timings"): the cold
  number to beat is **81 minutes** (1h 21m 09s); the warm number is
  already at the cache-pipeline floor (~56s run total) and cannot
  drop materially without changing the trigger surface.
- Phase 2 acceptance ("post-parallel cold + warm timings"): the cold
  number after both optimizations should approach
  **max(amd64-native, arm64-native) ≈ amd64-native**, expected ~5-7
  minutes per arch in parallel jobs; warm stays in the seconds
  range.
- The `cargo build` deps prebuild (#41 / #33) being its own layer is
  load-bearing for warm-cache behavior — a Cargo.lock change still
  invalidates it. Plan phase 1.1 should preserve the deps-vs-binaries
  split when rewriting stage 2 for zigbuild.

## Phase 1 results

Re-measurement after T1.1 (`deploy/Dockerfile` rewritten with
`cargo-zigbuild` stage 2 pinned to `$BUILDPLATFORM=amd64`,
cross-compiling to both `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`) and T1.2 (`Set up QEMU` step dropped
from `docker.yml`, buildx cache namespaced to `scope=zigbuild-deps`).

| Run | ID | Trigger | SHA | Note |
|---|---|---|---|---|
| Cold | [25381236706](https://github.com/branchwork/branchwork/actions/runs/25381236706) | `workflow_run` (after CI green) | 708c689 | First run with both T1.1 and T1.2 on master; new `zigbuild-deps` cache scope empty |
| Warm | [25382135087](https://github.com/branchwork/branchwork/actions/runs/25382135087) | `workflow_dispatch` | 708c689 | Same SHA, full `zigbuild-deps` cache from cold run |

Same workflow-dispatch protocol as 0.1; the unattended-execution
contract still forbids no-op pushes to master.

### Total wall clock

|        | Cold (Phase 1) | vs baseline | Warm (Phase 1) | vs baseline |
|--------|---------------:|------------:|---------------:|------------:|
| Run (`createdAt → updatedAt`)        | **15m 50s** (950s) | **19.5%** (5.1× speedup) | **1m 14s** (74s)  | 132% (1.3× slower) |
| Job (`started_at → completed_at`)    | 15m 35s (935s)     | 19.2% (5.2×)             | 1m 04s (64s)      | 131% |
| `Build and push` step                | 15m 07s (907s)     | 18.8% (5.3×)             | 43s               | 226% |
| Job overhead (job − step)            | 28s                | —                        | 21s               | — |

**Cold acceptance — PASS** (≤50% of baseline cold). The 65-minute
absolute drop comes entirely from collapsing the QEMU-emulated arm64
chain: native `cargo zigbuild` with the `zigcc` linker produces both
`x86_64` and `aarch64` musl binaries on the amd64 build host, and the
build no longer waits on any user-mode emulation.

**Warm acceptance — FAIL** (≤70% of baseline warm). Warm rose from
56s to 74s (132%, 18s slower). Root cause is structural to the new
stage 2 layout, not a regression we can patch in isolation — see
"Why warm got slower" below. A second warm dispatch
([25382275630](https://github.com/branchwork/branchwork/actions/runs/25382275630))
landed at 102s with `Build and push` = 68s, confirming the cost is in
the cached-layer materialization path (variance ~18-25s across runs)
rather than a one-time priming effect.

### Per-step breakdown

| Step                          | Cold       | Warm |
|-------------------------------|-----------:|-----:|
| Set up job                    | 5s         | 2s   |
| Run `actions/checkout@v4`     | 2s         | 1s   |
| Set up Docker Buildx          | 8s         | 9s   |
| Log in to GHCR                | 7s         | 0s   |
| Extract metadata              | 1s         | 1s   |
| **Build and push**            | **907s**   | **43s** |
| Post Build and push           | 2s         | 2s   |
| Post Log in to GHCR           | 0s         | 0s   |
| Post Set up Docker Buildx     | 3s         | 3s   |
| Post Run `actions/checkout@v4`| 0s         | 0s   |
| Complete job                  | 0s         | 0s   |

`Set up QEMU` and `Post Set up QEMU` are gone (T1.2). On the cold run
that saves ~8s; on the warm run the same saving is offset by the
larger cached-layer extraction below.

### Build-and-push internals (cold) — longest substeps

| #  | Step                                                                | Duration |
|----|---------------------------------------------------------------------|---------:|
| 33 | `[linux/amd64 server  9/13]` `cargo zigbuild --release` (deps prebuild) | **7m 14s** (433.7s) |
| 36 | `[linux/amd64 server 12/13]` `cargo zigbuild --bin … --release`     | 3m 30s (210.2s) |
| 46 | `exporting to GitHub Actions Cache`                                 | 2m 44s (163.9s) |
| 28 | `[linux/amd64 server  4/13]` `cargo install cargo-zigbuild`         | 1m 09s (69.2s) |
| 44 | `exporting to image` (multi-arch manifest + GHCR push)              | 21s   (20.6s) |
| 27 | `[linux/amd64 web 9/9]` `pnpm --filter @branchwork/web build`       | 11s   (11.0s) |

**Both arch-2 chains (the 76-minute arm64 stack from baseline #41 +
#56) are gone.** Cross-compilation runs once on the amd64 host:
`cargo zigbuild --target x86_64-unknown-linux-musl --target
aarch64-unknown-linux-musl` produces both binary sets in a single
invocation, so step 9/13 (deps) and step 12/13 (bins) each cover both
arches inside one DAG node. The amd64 deps-prebuild itself (433s)
sits below baseline's amd64-only deps (575s) despite carrying the
arm64 compile in the same step — zigbuild's two-target invocation
shares LLVM IR work between the targets, which is cheaper than two
back-to-back single-target cargo invocations would be.

`cargo install cargo-zigbuild` (#28, 69s) is new overhead introduced
by the toolchain. It cuts to ~0s on warm because the deps + binary
output of the install live inside the cached stage 2 layer.

### Why warm got slower

Baseline warm `Build and push` = 19s; Phase 1 warm = 43s. The 24s
regression is concentrated in DAG node #39, "extracting" cached
layers ahead of the runtime image build:

- **Baseline warm** (run 25376229417): #61 (`exporting to image`) =
  16.8s, #62 (GHA cache export) = 7.4s. No measurable layer
  extraction step — each arch's stage 2 layer was its own thin
  cached blob and the runtime stage's `COPY --from=server` only
  pulled that arch's binaries.
- **Phase 1 warm** (run 25382135087): #39 (`extracting` cached
  zigbuild stage 2 layers) = 29s, #44 (`exporting to image`) =
  12.7s, #46 (GHA cache export) = 2.3s. The cached stage 2 layer
  now carries **both arches' compile output** in a single DAG node
  (because the zigbuild stage is `--platform=$BUILDPLATFORM` and
  emits to `/out/amd64/` + `/out/arm64/` in the same `RUN`), and
  buildx materializes that whole blob to disk before slicing it
  into the per-arch runtime stage.

The export side of the same tradeoff is favourable: cold cache export
dropped from 230s (baseline #62) to 164s (Phase 1 #46). The warm
penalty is the import/extract side, and there is no straightforward
way to shrink it under the current single-stage-2 design — the
binaries for both arches have to live in the same cached layer for
the cross-compile to be coherent. Phase 2's per-arch matrix splits
stage 2 into two parallel jobs, which would partition the cache by
arch and put each warm extract in the same ballpark as baseline.

### Cache-hit rate

|        | Buildable layers | CACHED | Hit rate |
|--------|-----------------:|-------:|---------:|
| Cold   | 27               | 0      | 0%       |
| Warm   | 27               | 27     | 100%     |

The DAG is smaller than baseline (40 → 27 buildable) because the
arm64 stage 2 chain collapsed into shared steps with amd64 — only
the per-arch runtime slices split out per platform. Total DAG node
count fell 63 → 47.

### Decision: Phase 2 is still worth doing

Cold acceptance is hit dramatically (5.1× wall-clock speedup, 65
minutes saved per cold run). Warm acceptance fails by 18s on the
median run, but the loss is structural (cache layout, not compute)
and the absolute cost is small relative to cold's gain — even the
slow-tail warm run (102s) is a rounding error against the old
81-minute cold.

Phase 2 (per-arch matrix) addresses both remaining levers in one
move:

- **Cold further**: today the cold cost is `amd64-deps + amd64-bin
  + cache-export ≈ 7m + 3.5m + 2.7m = ~13m of wall clock`. A
  per-arch matrix runs amd64 and arm64 in parallel jobs, dropping
  cold to `max(amd64, arm64) ≈ amd64-native ~ 6-7m` (zigbuild's
  arm64 build is bound by the same amd64-host CPU but the *job*
  parallelizes, hiding it).
- **Warm restored**: each arch gets its own cache scope (`zigbuild-
  amd64` / `zigbuild-arm64`), so the warm extract cost halves per
  job and the manifest-merge job stays trivial. Expected warm: low
  20s, in line with baseline.

The zigbuild lift did **not** absorb most of the available gain —
it left ~10 minutes of cold and the entire warm regression on the
table, both of which a per-arch matrix recovers.

## Phase 2 results

Re-measurement after T2.1 (`docker.yml` split into a 2-entry build
matrix on `amd64`/`arm64` with `push-by-digest=true`, plus a
`manifest` job that fans the per-arch digests back into the
human-readable tags via `docker buildx imagetools create`).
Per-arch GHA cache scopes (`amd64` / `arm64`) replace the previous
single `zigbuild-deps` scope, so the two arches no longer invalidate
each other.

The Dockerfile is unchanged from Phase 1 — stage 2 still does a
single dual-target `cargo zigbuild --target x86_64-unknown-linux-musl
--target aarch64-unknown-linux-musl` invocation. Both matrix arches
therefore execute the full dual-target Rust compile; only stage 3
(`COPY --from=server /out/${TARGETARCH}/...`) is per-arch. Splitting
stage 2 by `$TARGETARCH` was deferred — see "Why the cold gap
remains" below.

| Run | ID | Trigger | SHA | Note |
|---|---|---|---|---|
| Cold | [25383180962](https://github.com/branchwork/branchwork/actions/runs/25383180962) | `workflow_run` (after CI green) | 310afbe | First run on master with both arches' GHA cache scopes empty |
| Warm | [25384334395](https://github.com/branchwork/branchwork/actions/runs/25384334395) | `workflow_dispatch` | 310afbe | Same SHA, full per-arch GHA cache from cold run |

A second `workflow_run` on the same SHA fired concurrently with the
cold above (run [25383536100](https://github.com/branchwork/branchwork/actions/runs/25383536100),
created 7 minutes after cold, finished 6 minutes after cold). It
contended with the cold run for cache writes — relevant context for
the amd64-vs-arm64 wall-clock asymmetry below; see "Variance from
cache contention".

### Total wall clock

The metric of record is run-level `createdAt → updatedAt`, which
spans the parallel build matrix and the sequential manifest job.

|        | Cold (Phase 2) | vs 0.1 baseline | Warm (Phase 2) | vs 0.1 baseline |
|--------|---------------:|----------------:|---------------:|----------------:|
| Run (`createdAt → updatedAt`)         | **20m 28s** (1228s) | **25.2%** (4.0× speedup) | **1m 44s** (104s) | 186% (1.9× slower) |
| `build (arm64)` job (started → completed) | 13m 14s (794s)      | —                       | 30s               | — |
| `build (amd64)` job (started → completed) | 19m 32s (1172s)     | —                       | 22s               | — |
| `manifest` job (started → completed)      | 36s                 | —                       | 57s               | — |

**Cold acceptance — PASS** (≤30% of the 0.1 baseline 4869s, i.e.
≤1460s). Cold landed at 25.2% of baseline, comfortably under the
target. Compared to Phase 1 cold (950s, 19.5%), Phase 2 cold
regressed by 278s / 29% — see "Why the cold gap remains".

**Warm acceptance — FAIL** (≤40% of the 0.1 baseline 56s, i.e.
≤22s). Warm landed at 186% of baseline (104s), a 4.6× miss against
the target and a 41% regression vs Phase 1 warm (74s). The miss is
structural — see "Why warm got slower again".

### Per-job breakdown

The build matrix runs `amd64` and `arm64` in parallel, each emitting
a tagless image keyed by digest. The `manifest` job downloads both
digests via `actions/download-artifact@v4` and runs `docker buildx
imagetools create` to attach the human-readable tags pointing at
both per-arch images.

#### Cold

| Job             | Build and push | Set up Docker Buildx | Misc | Total |
|-----------------|---------------:|---------------------:|-----:|------:|
| build (arm64)   | 12m 44s (764s) | 17s                  | 13s  | 13m 14s |
| build (amd64)   | 19m 10s (1150s)| 6s                   | 16s  | 19m 32s |
| manifest        | —              | 9s                   | 27s* | 36s   |

\* `manifest` misc = Set up job 4s + Download digests <1s + Login 1s + Extract metadata 1s + **Create manifest 17s** + post-steps 4s.

#### Warm

| Job             | Build and push | Set up Docker Buildx | Misc | Total |
|-----------------|---------------:|---------------------:|-----:|------:|
| build (amd64)   | 5s             | 7s                   | 10s  | 22s   |
| build (arm64)   | 10s            | 6s                   | 14s  | 30s   |
| manifest        | —              | 7s                   | 50s* | 57s   |

\* `manifest` misc = Set up job 2s + Download digests <1s + Login 1s + Extract metadata 1s + **Create manifest 43s** + post-steps 3s.

The manifest job is wholly sequential against the build matrix
(`needs: build`). Even on a perfect cache hit, it adds ~30-60s of
fixed overhead on top of `max(amd64-job, arm64-job)`, dominated by
the `docker buildx imagetools create` call attaching tags to the
per-arch digests in GHCR.

### Build-and-push internals (cold) — longest substeps

Aggregating both matrix jobs' BuildKit DAG nodes (each job's stage 2
runs the full dual-target zigbuild, so identical step labels appear
in both jobs).

| #  | Job   | Step                                                                | Duration |
|----|-------|---------------------------------------------------------------------|---------:|
| 30 | amd64 | `[server  9/13]` `cargo zigbuild --release` (deps prebuild, dual-target) | **7m 49s** (469.4s) |
| 40 | amd64 | `exporting to GitHub Actions Cache`                                 | **5m 55s** (354.5s) |
| 30 | arm64 | `[server  9/13]` `cargo zigbuild --release` (deps prebuild, dual-target) | 5m 49s (348.5s) |
| 33 | amd64 | `[server 12/13]` `cargo zigbuild --bin … --release` (dual-target)   | 3m 45s (224.7s) |
| 33 | arm64 | `[server 12/13]` `cargo zigbuild --bin … --release` (dual-target)   | 2m 49s (168.8s) |
| 40 | arm64 | `exporting to GitHub Actions Cache`                                 | 2m 33s (152.7s) |
| 25 | amd64 | `[server  4/13]` `cargo install cargo-zigbuild`                     | 1m 13s (73.1s) |
| 23 | arm64 | `[server  4/13]` `cargo install cargo-zigbuild`                     | 1m 02s (61.7s) |

The Phase 1 longest substep was a single `[amd64 server 9/13]
cargo zigbuild` deps prebuild at 433.7s. Phase 2 cold contains *two*
of those steps — one per matrix job — at 348.5s and 469.4s. The
zigbuild + install + rust-target overhead is paid twice instead of
once, and that duplication is the main reason cold regressed against
Phase 1.

#### Why the cold gap remains

The Phase 1 prediction was `cold ≈ max(amd64-native, arm64-native)
≈ 6-7m per arch`. Realised cold is 13-20m per arch. The factor-2
gap traces to the Dockerfile still cross-compiling **both** targets
in stage 2, even when the parent matrix job only needs one:

```dockerfile
# stage 2 RUN (deploy/Dockerfile:51-54 + 63-69), executed in BOTH
# matrix jobs:
RUN cargo zigbuild \
      --target x86_64-unknown-linux-musl \
      --target aarch64-unknown-linux-musl \
      --release …
```

Buildx does not prune unreferenced stage 2 outputs based on the
single `COPY --from=server /out/${TARGETARCH}/…` in stage 3. The
arm64 matrix job builds both binaries, then copies only the arm64
slice into runtime; amd64 does the same in mirror. Each parallel
job thus carries the full dual-target compile cost, and the
parallelism only saves the stage-3 layer + per-arch GHA cache
export (which is now non-trivially smaller per scope).

To realise the predicted ~7m cold, stage 2 would need to be
split — e.g. take a `TARGET_TRIPLE` `ARG` from `$TARGETARCH` and
invoke `cargo zigbuild --target $TARGET_TRIPLE` once. That work is
out of scope for this plan; tracked as a follow-up.

#### Variance from cache contention

A second cold run on the same SHA ([25383536100](https://github.com/branchwork/branchwork/actions/runs/25383536100))
fired 7 minutes after our canonical cold and ran in parallel until
shortly after our warm dispatch. Both cold runs wrote to the same
per-arch cache scopes simultaneously. In our canonical cold:

- arm64 finished its `Build and push` at 14:54:02Z, before the
  second cold's amd64 had even started exporting cache. arm64 was
  uncontested.
- amd64 finished its `Build and push` at 15:00:25Z, with 354.5s of
  that being `exporting to GitHub Actions Cache` — overlapping
  exactly the window during which the second cold's amd64 was
  reading the scope.

The second cold's per-arch wall clock was actually closer to
symmetric: arm64 14m 41s, amd64 16m 25s. Our cold's amd64 (19m 32s)
appears anomalously slow as a result of the export contention.
Treat the cold number as an upper bound; a clean re-run with no
parallel cold should land closer to 16-17m, still above the Phase 1
combined 16m but with the asymmetry reduced.

### Why warm got slower again

Phase 1 warm = 74s; Phase 2 warm = 104s. The 30s regression splits
across two structural costs:

- **Manifest job overhead (~57s)**: Phase 1 had a single build job
  whose `Post Build and push` step exported the cached image
  directly to GHCR with all tags attached in one shot. Phase 2 has
  to re-attach tags via a separate manifest job — `docker buildx
  imagetools create` against the per-arch digests took 43s on warm
  (it round-trips through GHCR to read each digest's manifest, then
  writes a new manifest list per tag). Add Buildx setup + login on
  the new job (~10s) and the manifest contributes ~57s of
  unavoidable sequential overhead.
- **Per-job setup duplication (~5-10s)**: each parallel build job
  pays its own `Set up Docker Buildx` (6-7s) and Buildx daemon
  cold-start. The matrix amortises compute but multiplies setup.
  In the warm case the build itself is `5-10s` per arch, so setup
  + post-steps dominate; the parallel structure then runs at
  `max(amd64, arm64) ≈ 30s` for the build phase.

Net warm budget on a perfect cache hit:
`max(amd64-job, arm64-job) + manifest-job ≈ 30s + 57s ≈ 90s`,
which is consistent with the observed 104s once you allow ~14s of
runner overhead between job boundaries (queueing, runner
provisioning).

The ≤22s warm target is unreachable under the current matrix +
manifest design — the manifest job alone exceeds it. Reaching the
target would require collapsing back to a single build job (losing
parallel arch builds) or moving the tag attachment into the build
jobs themselves (losing the per-arch digest publishing model).
Neither is in scope for this plan.

### Cache-hit rate

|        | Buildable layers (per arch job) | CACHED | Hit rate |
|--------|--------------------------------:|-------:|---------:|
| Cold   | 24                              | 0      | 0%       |
| Warm   | 24                              | 24     | 100%     |

Same shape as Phase 1: zero hits cold, 100% hits warm. The DAG
node count per arch job (~38) is below the Phase 1 combined-job DAG
(47) because each matrix job only materialises one runtime stage
slice instead of both.

### Final verdict against documented targets

| Metric | Target (vs 0.1 baseline) | Phase 1 | Phase 2 | Verdict |
|--------|--------------------------|--------:|--------:|---------|
| Cold   | ≤30% of 4869s = ≤1460s   | 950s (19.5%) | **1228s (25.2%)** | **PASS** |
| Warm   | ≤40% of 56s   = ≤22s     | 74s (132%)   | **104s (186%)**   | **FAIL** (gap explained) |

Cold passes on absolute targets but regresses against Phase 1, due
to the Dockerfile dual-target zigbuild being unchanged in T2.1 (the
matrix split saves stage-3 + cache-export, not stage-2 compute). A
follow-up that splits stage 2 by `$TARGETARCH` would recover the
predicted ~7m cold.

Warm fails by 4.6× on the target. The miss is structural to the
matrix + manifest design: the sequential manifest job adds ~57s of
fixed overhead that the ≤22s warm target cannot absorb. This is a
documented trade-off — per-arch digest publishing requires a fan-in
step — and the absolute warm cost (104s) is still trivial against
cold's 1228s.

The plan-level wall-clock objective stands: cold went from 81m
(baseline) to 20.5m (Phase 2), a 60-minute saving per cold run.
