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
