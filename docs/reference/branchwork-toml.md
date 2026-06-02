# `branchwork.toml` reference

A `branchwork.toml` file at the **root of a project directory** lets you
set repo-wide defaults for the two pieces of plan configuration that are
project-shaped rather than plan-shaped:

- **Which CI workflows block** auto-mode advancement (vs. which are
  informative-only — `Docker Publish`, `release`, `bench`, …).
- **The phase-end verification command** (e.g. `bash scripts/verify.sh`)
  run by the Check agent at every phase merge.

The file is loaded by
[`server-rs/src/repo_config.rs`](../../server-rs/src/repo_config.rs) on
the project-resolution path and cached in-process with mtime
invalidation, so calling it from a hot loop is cheap. The file is
**optional**: when absent, Branchwork falls back to its smart-default
classifier (CI filter) and treats phase verification as a no-op.

The values feed two consumers:

- **CI aggregate filter** —
  [`server-rs/src/ci/aggregate.rs::compute_with_filter`](../../server-rs/src/ci/aggregate.rs)
  partitions a SHA's runs into blocking and informational, and only the
  blocking subset can poison the verdict.
- **Phase-end Check agent** —
  [`server-rs/src/agents/phase_check.rs`](../../server-rs/src/agents/phase_check.rs)
  spawns a Check agent on the phase-merge SHA, runs `[phase] verification`
  in a fresh worktree, and pauses the plan on non-zero exit.

The precedence rules (per-phase < per-plan < `branchwork.toml` < smart
default) are owned by
[`server-rs/src/ci/resolution.rs`](../../server-rs/src/ci/resolution.rs)
and described in detail in
[adrs/0006-phase-verify-and-ci-filter.md](../adrs/0006-phase-verify-and-ci-filter.md).

---

## Location

`~/<project>/branchwork.toml` — i.e. the same directory the plan's
`project:` field points at. The project root is the only place
Branchwork looks; per-subdirectory `branchwork.toml` files are not
discovered.

If your project lives outside `~`, it is still discovered the same way
the rest of Branchwork resolves projects: via the `plan_project` table
override (see [plan-schema.md → Project inference](plan-schema.md#project-inference)).

---

## Schema

```toml
[ci]
# Allowlist of workflow names that block merges and auto-mode
# advancement. Names are matched case-sensitively against the
# `name:` field of each GitHub workflow.
blocking_workflows = ["CI"]

# OR, instead of an allowlist, name the workflows that should NOT block.
# Useful when you want everything to block by default and only carve
# out a handful of slow/flaky jobs:
# blocking_workflows_skip = ["Docker", "Deploy", "Publish"]

[phase]
# Shell command run by the phase-end Check agent. Executed in the
# project root. Any non-zero exit blocks the phase from being
# marked done.
verification = "bash scripts/verify.sh"

[cache]
# Build-cache directory overrides for worktree-isolated agents. Point
# the shared cache at a bigger/faster disk than the default
# `<worktree-base>/../cache/<project>/…`.
cargo_target_dir = "/mnt/big-disk/branchwork-cache/cargo"
pnpm_store_dir   = "/mnt/big-disk/branchwork-cache/pnpm-store"
```

All sections are optional. An empty file is valid (and is parsed to a
fully-default config). Unknown top-level keys and unknown keys inside
`[ci]` / `[phase]` / `[cache]` are silently dropped, so typos won't crash
the parser — but they also won't produce a warning, so double-check
spelling against this page.

### `[ci]`

| Key | Type | Default | Description |
|---|---|---|---|
| `blocking_workflows` | list of strings | unset (smart-default) | Names of workflows that block merges and auto-mode advancement. When set, every other workflow on the same SHA is informative-only. |
| `blocking_workflows_skip` | list of strings | unset (smart-default) | Names of workflows that are explicitly informative-only. Conceptually the inverse of `blocking_workflows`. |

If neither is set, Branchwork uses a regex classifier that marks any
workflow matching `(?i)docker|deploy|publish|release|bench|fuzz` as
non-blocking and everything else as blocking
([`is_workflow_blocking_by_default`](../../server-rs/src/ci/aggregate.rs)).

When **both** `blocking_workflows` and `blocking_workflows_skip` are
set on the same `branchwork.toml`, `blocking_workflows` wins — the
allowlist is treated as authoritative and the skip list is ignored.
The two are alternative phrasings of the same rule, and a project that
sets both is almost certainly mid-edit; the safer behaviour is to use
the explicit allowlist rather than try to intersect them.

### `[phase]`

| Key | Type | Default | Description |
|---|---|---|---|
| `verification` | string | unset | Shell command run by the phase-end Check agent. Whitespace-trimmed before execution. Empty / whitespace-only values behave as if the key were absent. |

The string is passed verbatim into the agent prompt; it is **not**
interpreted by a shell directly, so things like environment variable
expansion or `&&`-chained commands are at the agent's discretion. The
canonical pattern is to keep the entrypoint trivial (`bash
scripts/verify.sh`, `make verify`, `pnpm run ci`) and put the real
logic in a checked-in script.

### `[cache]`

| Key | Type | Default | Description |
|---|---|---|---|
| `cargo_target_dir` | path | unset | Override for `CARGO_TARGET_DIR`. When set, every worktree-isolated agent's cargo build writes here instead of the computed default. Used verbatim — no project-slug suffix is appended. |
| `pnpm_store_dir` | path | unset | Reserved for a future pnpm store-dir override. Parsed and round-tripped, but not yet consumed (pnpm already shares one global store per host). |

The `[cache]` table only affects **worktree-isolated** agents (the
`worktree-per-agent-isolation` model). An agent running directly in the
project root shares the project's own `target/` and ignores it. When
unset, the cargo cache defaults to
`<worktree-base>/../cache/<project-slug>/cargo-target` — a sibling of the
worktree base so every agent on the project shares one build cache. See
[user-guide.md → Project configuration](../user-guide.md#project-configuration)
for the operator-facing walkthrough.

---

## Precedence

The full precedence chain (owned by
[`server-rs/src/ci/resolution.rs`](../../server-rs/src/ci/resolution.rs))
is:

1. **Per-phase** — `ci_blocking_workflows` / `phase_verification` set on
   a single `YamlPlanPhase` in the plan YAML. Wins for that phase only.
2. **Per-plan** — the same fields set at the top level of the plan
   YAML. Wins for every phase in that plan that doesn't override them.
3. **Repo defaults** — this file (`branchwork.toml`). Wins when neither
   the plan nor the phase sets the field.
4. **Smart-default classifier** — the regex fallback described above.
   Applies to `ci_blocking_workflows` only — `phase_verification` has
   **no** layer-4 fallback (verify is opt-in).

Each layer is **independent per field**: setting only
`blocking_workflows` in `branchwork.toml` doesn't suppress the
plan-level `phase_verification`, and vice versa.

An **explicit empty list** at any layer (`ci_blocking_workflows: []` in
plan YAML, or `blocking_workflows = []` in `branchwork.toml`) is a
"nothing blocks" configuration, **not** a fall-through. Layers below it
are skipped. To fall through, omit the key entirely.

---

## Example

A typical layout for a Rust + TypeScript project with a slow Docker
build and a fuzzing job:

```toml
# ~/cep/branchwork.toml
[ci]
blocking_workflows = ["CI", "lint", "typecheck"]

[phase]
verification = "bash scripts/verify.sh"
```

…with `scripts/verify.sh` running the project's full check suite:

```bash
#!/usr/bin/env bash
set -euo pipefail
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo deny check
cargo audit
pnpm --filter ./web run lint
```

Now any plan in `~/cep/` will:

- treat only `CI`, `lint`, and `typecheck` workflows as blocking on
  every CI poll (`Docker Publish`, `release`, `bench` etc. show up in
  the dashboard but never gate a merge);
- spawn a Check agent at every phase merge that runs
  `bash scripts/verify.sh` in `~/cep/` and blocks the phase from being
  marked done until the script exits 0.

---

## Failure modes

- **File absent** → Branchwork falls back to the smart-default
  classifier silently. The common case.
- **File present but malformed** → a one-line warning is logged to
  stderr (`[branchwork] warning: failed to parse …`), the cache stores
  a `None` so subsequent calls don't re-warn, and Branchwork falls back
  to the smart-default classifier as if the file were absent.
- **Unknown keys** → silently dropped. `serde` is configured without
  `deny_unknown_fields`, so a typo in `blocking_workflows` will look
  identical to an absent field. Verify your config by checking the
  resolved values surface on the plan board's **Settings** tab — it
  reads back through the same resolution helper and shows you the
  effective allowlist + the layer it came from.

---

## See also

- [reference/plan-schema.md](plan-schema.md) — per-plan and per-phase
  override fields (`ci_blocking_workflows`, `phase_verification`).
- [reference/configuration.md](configuration.md) — env vars and CLI
  flags. `branchwork.toml` is intentionally **not** a global config
  surface; it's a per-project file with a narrow schema.
- [adrs/0006-phase-verify-and-ci-filter.md](../adrs/0006-phase-verify-and-ci-filter.md)
  — design rationale for the four-layer precedence and the
  classifier-as-fallback choice.
