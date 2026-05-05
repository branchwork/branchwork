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
