# Plan schema reference

A **plan** is a YAML or Markdown file in `<claude-dir>/plans/` (default
`~/.claude/plans/`) that describes a unit of work as a tree of phases and
tasks. Every field below is parsed by
[`server-rs/src/plan_parser.rs`](../../server-rs/src/plan_parser.rs); a
minimal in-repo template lives at the root [`plan.yaml`](../../plan.yaml)
and points back to this page rather than duplicating the schema.

The dashboard's file watcher monitors the plans directory non-recursively,
so dropping a new YAML or Markdown file in is enough — no restart needed.

## Formats

Branchwork accepts three file extensions, looked up by
[`find_plan_file`](../../server-rs/src/plan_parser.rs):

| Extension | Parser | Notes |
|---|---|---|
| `.yaml`, `.yml` | `parse_plan_yaml` | Strict — invalid YAML returns an error. **Preferred for new plans.** |
| `.md` | `parse_plan_markdown` | Best-effort heading + bullet parsing. |

When both `<name>.yaml` and `<name>.md` exist for the same plan name, the
YAML wins (priority order in `list_plans`). Plans created via the
dashboard always emit YAML.

YAML and Markdown produce the **same** `ParsedPlan` shape, so once on
the wire there is no difference downstream.

> **Wire format note.** YAML keys are `snake_case`. The JSON returned by
> `GET /api/plans/:name` uses `camelCase` (the `ParsedPlan` struct
> declares `#[serde(rename_all = "camelCase")]`). So `produces_commit`
> on disk is `producesCommit` over HTTP, `file_paths` is `filePaths`,
> and so on. The tables below give the on-disk YAML key.

---

## YAML schema

### Top level — `YamlPlan`

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `title` | string | yes | — | Human-readable plan title shown in the sidebar and PR descriptions. |
| `context` | string (block scalar OK) | no | `""` | Background paragraph displayed at the top of the plan board. |
| `project` | string | no | inferred — see [Project inference](#project-inference) | Slug of the project directory (`~/<project>`) the plan targets. Tasks in this plan spawn agents with `cwd = ~/<project>`. |
| `created_at` | string (RFC 3339) | no | filesystem creation time of the plan file | Sets the timestamp shown in the dashboard "created" column. Useful when a plan is checked in from elsewhere and the file's metadata doesn't reflect when the plan was authored. |
| `phases` | list of `YamlPlanPhase` | yes | — | Ordered phases. May be empty (`[]`) but the key must be present. |
| `verification` | string (block scalar OK) | no | omitted | Numbered list of post-implementation verification steps. Surfaces as a collapsible section on the plan board and feeds the plan-level Check agent. Empty / whitespace-only values are normalised to omitted. |
| `ci_blocking_workflows` | list of strings | no | omitted | Plan-wide allowlist of GitHub workflow names that block merges and auto-mode advancement. Every other workflow on the same SHA is treated as informational. See [CI workflow filter](#ci-workflow-filter). |
| `phase_verification` | string (block scalar OK) | no | omitted | Plan-wide phase-end verify command, run by the Check agent at every phase merge. Per-phase value below overrides this. See [Phase-end verification](#phase-end-verification). |

Unknown top-level keys are silently dropped (the parser uses `serde_yaml`
without `deny_unknown_fields`); typos in field names will not raise an
error, they will just be ignored.

### Phase — `YamlPlanPhase`

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `number` | u32 | yes | — | Phase number. Conventionally starts at 0 and is monotonic, but no uniqueness check is enforced. |
| `title` | string | yes | — | Phase heading. |
| `description` | string | no | `""` | Phase summary, rendered above the task list. |
| `tasks` | list of `YamlPlanTask` | yes | — | Tasks in this phase. May be empty. |
| `ci_blocking_workflows` | list of strings | no | omitted | Per-phase override of the plan-level `ci_blocking_workflows`. See [CI workflow filter](#ci-workflow-filter). |
| `phase_verification` | string (block scalar OK) | no | omitted | Per-phase override of the plan-level `phase_verification`. See [Phase-end verification](#phase-end-verification). |

### Task — `YamlPlanTask`

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `number` | string | yes | — | Stable task identifier (e.g. `"0.1"`, `"3.12"`). Quote it — YAML would otherwise coerce a bare `1.2` to a float. The dashboard, MCP tools, and `update_task_status` all key on this string. |
| `title` | string | yes | — | One-line task heading. |
| `description` | string (block scalar OK) | no | `""` | Multi-paragraph task body; goes verbatim into the agent's prompt. |
| `file_paths` | list of strings | no | `[]` | Files relevant to this task. Used by the auto-status file-existence heuristic and embedded in the agent prompt. |
| `acceptance` | string | no | `""` | Acceptance criteria, embedded in the agent prompt and the Check-agent verification prompt. |
| `dependencies` | list of strings | no | `[]` | Task numbers this task depends on (e.g. `["0.1", "1.2"]`). Advisory only — the dashboard does not block on them, but the agent and Check prompts cite them. Omitted from serialised YAML when empty. |
| `produces_commit` | bool | no | `true` | When `false`, the dashboard hides the **Merge** button on the task card. See [`produces_commit`](#produces_commit). Omitted from serialised YAML when `true`, so existing plans round-trip unchanged. |

---

## Markdown format

When a plan file ends in `.md`,
[`parse_plan_markdown`](../../server-rs/src/plan_parser.rs) maps the
same `ParsedPlan` shape onto heading + bullet structure. The Markdown
form is supported for legacy plans and AI-assisted draft authoring; new
plans created via the dashboard always emit YAML.

### Top level

| Plan field | Markdown source |
|---|---|
| `title` | First `# H1` line. Falls back to the filename stem. |
| `context` | Body of the `## Context` section (case-insensitive prefix). |
| `verification` | Body of the `## Verification` section, if present and non-empty. |
| `project` | Inferred from the whole file body — see [Project inference](#project-inference). Markdown plans cannot pin a project explicitly. |
| `created_at` | Always taken from filesystem `ctime` — the Markdown form has no surface for this field. |

### Phases

A `## H2` is treated as a phase if its heading matches one of these
patterns (in priority order):

| Pattern | Example | Phase number |
|---|---|---|
| `Phase N: Title` / `Step N: Title` | `## Phase 0: Scaffolding` | parsed `N` (trailing letter ignored) |
| `N) Title` / `N. Title` | `## 1) First phase` | parsed `N` |
| `Changes` / `Implementation` / `Approach` / `Design` / `The change` | `## Changes` | sequential index of phases-so-far |

Anything else under `##` is ignored as flavour text.

### Tasks

Within each phase body, tasks are extracted by trying three strategies
in order, falling through if the previous strategy produced nothing:

1. **`### H3` task headings** — preferred. Matches `### 1.1 Title`, `### Phase 1.1 — Title`, or any `### token Title`. Within each task body:
   - `acceptance` is pulled from `**Acceptance:** …`.
   - `dependencies` is pulled from `**Depends on:** 1.1, 1.2` (also accepts `**Dependencies:**`, `**Blocked by:**`, `**Requires:**`).
2. **Bold-bullet fallback** — `- **Title** — description` lines, numbered `<phase>.1`, `<phase>.2`, …
3. **Whole phase body** — if neither of the above produces tasks but the phase body has text, the entire body becomes one task numbered `<phase>.1`.

`file_paths` are extracted from the task description by regex over
backtick-quoted relative paths (`` `src/foo.rs` ``) and absolute paths
(`/home/me/project/src/foo.rs`); duplicates are removed.

`produces_commit` is **always `true`** on Markdown-parsed tasks — the
field has no Markdown surface form. To opt a task out of merge, port the
plan to YAML and set `produces_commit: false`.

`ci_blocking_workflows` and `phase_verification` are also **always
`None`** on Markdown-parsed plans — neither is expressible in the
Markdown form. Markdown plans inherit those values from
`branchwork.toml` (or fall through to the smart classifier) only.

---

## Field semantics

### `produces_commit`

Defaults to `true`. Set to `false` on tasks that don't change tracked
files — investigation, reproduction, manual verification — to hide the
**Merge** button on the task card. The **Discard** button still shows so
the empty branch can be cleaned up. The serializer omits
`produces_commit: true` from saved YAML
([`is_true` / `default_true`](../../server-rs/src/plan_parser.rs)), so
existing plans round-trip unchanged and only the explicit `false`
appears on disk.

The merge UI is the only consumer; the agent prompt and Check pipeline
ignore the field. There is also a server-side guard in
[`api/agents.rs`](../../server-rs/src/api/agents.rs) that rejects merges
of zero-commit branches with HTTP 409 — `produces_commit: false` is the
UI signal that a no-commit task is *expected*; the 409 is defence in
depth.

### Project inference

If `project:` is set explicitly in the YAML, that wins. Otherwise
[`infer_project`](../../server-rs/src/plan_parser.rs) scans the raw
text of the plan file and tries four strategies in order, returning the
first match against an existing `~/<project>/` directory:

1. **Absolute paths** — count occurrences of `/home/<user>/<dirname>/...` and pick the most-mentioned `<dirname>` that exists under `$HOME`.
2. **Cargo crate paths** — for any `crates/<name>` reference, find the project that owns `~/<project>/crates/<name>`.
3. **Module-name paths** — for any hyphenated `<name>/src/` reference, find the project that owns `~/<project>/<name>`.
4. **Title / context keyword match** — within the first 500 chars, lower-cased, find the longest project-directory name (≥ 4 chars) that appears as a substring. Length-descending order so `reglyze` beats `rust`.

If no strategy matches, `project` is `None` and tasks default to spawning
agents in the dashboard server's own working directory. The list of
candidate `~/<project>/` directories is scanned **once** per server
process (cached via `OnceLock`) — adding a new project directory after
the server starts requires a server restart for inference to see it.
Pinning `project:` explicitly is the supported escape hatch.

### `created_at`

Optional, RFC 3339 timestamp (e.g. `"2025-01-15T10:30:00Z"`). When
omitted the parser falls back to the plan file's filesystem creation
time ([`parse_plan_file`](../../server-rs/src/plan_parser.rs)). Useful
when a plan is checked in from elsewhere (a teammate's machine, a
backup, a `git clone` that resets metadata) and the on-disk creation
time doesn't reflect authorship. The field flows into the dashboard's
"created" column and into PR description templates.

### `verification`

Block-scalar string, typically a numbered list. When present, the plan
board renders it as a collapsible **Verification** section, and the
plan-level Check agent (the **Check Plan** button) reads it as the spec
to verify the as-built plan against. Empty / whitespace-only strings
are normalised to omitted. Round-trips through YAML and survives plan
edits via the dashboard.

### CI workflow filter

`ci_blocking_workflows` (top-level **and** per-phase) names the GitHub
workflows that block auto-mode advancement and feed the dashboard's
red/green CI verdict. Every other workflow that ran on the merged SHA is
captured in the aggregate as `informational: true` and never affects
`conclusion` ([`server-rs/src/ci/aggregate.rs`](../../server-rs/src/ci/aggregate.rs)).

This is the surface for "the project ships a `Docker Publish` /
`release` / `bench` workflow that fails or hangs for unrelated reasons —
don't let it block the merge gate." Names are matched **case-sensitively**
against the workflow's top-level `name:` field (with the `.github/workflows/<file>.yml`
filename stem as fallback when `name:` is absent).

Resolution is per-field, first match wins
([`server-rs/src/ci/resolution.rs`](../../server-rs/src/ci/resolution.rs)):

| Layer | Source | Wins for |
|---|---|---|
| 1. Phase | `YamlPlanPhase.ci_blocking_workflows` | that phase only |
| 2. Plan | `YamlPlan.ci_blocking_workflows` (top level) | every phase that doesn't override |
| 3. Repo | `branchwork.toml` `[ci] blocking_workflows` | every plan in that project |
| 4. Smart default | regex `(?i)docker\|deploy\|publish\|release\|bench\|fuzz` matches → informational, everything else → blocking | when no other layer is set |

An **explicit empty list** at any layer (`ci_blocking_workflows: []`) is
not "fall through to the next layer" — it means "nothing blocks", a
vacuous-success configuration. `None` (the field is absent) is what
triggers fall-through. The smart classifier in layer 4 is run **only**
when every higher layer is `None`; an explicit list at layers 1-3 is
returned verbatim, even if the classifier would have demoted some of
those names.

`branchwork.toml`'s sibling field `blocking_workflows_skip` is a deny-list
alternative used at the repo layer only — see
[branchwork-toml.md](branchwork-toml.md).

### Phase-end verification

`phase_verification` (top-level **and** per-phase) is a shell command run
by the Check agent at every **phase merge** — i.e. when the last task in
a phase merges into the project's default branch. The command runs in a
fresh git worktree at the merge SHA
([`server-rs/src/agents/phase_check.rs`](../../server-rs/src/agents/phase_check.rs));
zero exit advances to the next phase, non-zero pauses the plan with
`paused_reason: phase_verify_failed`.

Resolution is the same precedence pattern as `ci_blocking_workflows`,
minus the smart-default layer (verify is opt-in — there is no
auto-detected fallback):

| Layer | Source | Wins for |
|---|---|---|
| 1. Phase | `YamlPlanPhase.phase_verification` | that phase only |
| 2. Plan | `YamlPlan.phase_verification` (top level) | every phase that doesn't override |
| 3. Repo | `branchwork.toml` `[phase] verification` | every plan in that project |
| 4. — | (no smart default — `None` means no-op) | — |

The string is passed verbatim into the Check agent prompt and run via
the agent's `Bash` tool. The canonical pattern is to keep the entrypoint
trivial (`bash scripts/verify.sh`, `make verify`, `pnpm run ci`) and
keep the real logic in a checked-in script — see the example below.

### `dependencies`

Advisory only — the server does not block dependent tasks from starting
before their dependencies are done, but the agent prompt and Check-agent
prompt both quote the dependency list. Omitted from serialised YAML when
empty so the on-disk form stays terse.

### `file_paths`

In YAML these are explicit list entries; in Markdown they are extracted
by regex from the task body. The dashboard uses the merged list for two
things:

- **Auto-status file-existence heuristic** ([`auto_status::infer_status`](../../server-rs/src/auto_status.rs)) — caps at `in_progress`. It never auto-marks a task `completed`.
- **Prompt context** — the agent prompt enumerates `file_paths` so the agent knows which files the task touches.

Listing files that don't yet exist is fine — the heuristic flips a task
to `in_progress` once any of them appears.

---

## Server-only fields

`ParsedPlan` and `PlanTask` carry several fields that are **never**
written to disk — they are populated by the server when responding to
`GET /api/plans/:name` and ignored by the parser:

| JSON field | Source | Notes |
|---|---|---|
| `name` | filename stem | e.g. `<claude-dir>/plans/architecture-docs.yaml` → `"architecture-docs"`. |
| `filePath` | absolute path to the plan file | For UI links and Check-agent prompts. |
| `modifiedAt` | filesystem mtime | Refreshed live by the file watcher. |
| `totalCostUsd` | aggregated from `agents` rows | Sum of all per-task costs in this plan. |
| `maxBudgetUsd` | `/api/settings` | Server-wide budget cap surfaced for client-side checks. |
| `phases[].tasks[].status` | `task_status` table | One of `pending`, `in_progress`, `completed`, `skipped`, `failed`, `checking`. |
| `phases[].tasks[].statusUpdatedAt` | `task_status.updated_at` | RFC 3339. |
| `phases[].tasks[].costUsd` | `agents` rows for this task | Per-task cost rolled up from all agents that ran on it. |
| `phases[].tasks[].ci` | `ci_runs` join | Latest CI run for this task's branch. |

If you `PUT /api/plans/:name` (e.g. via the dashboard's Edit Plan
modal), only the on-disk fields are sent — server-only fields are
discarded.

> **Caveat.** The current `UpdateTaskBody` (`api/plans.rs`) does not yet
> plumb `producesCommit` or `verification`, so editing a plan via the UI
> clobbers explicit `produces_commit: false` back to `true` and wipes
> top-level `verification`. Hand-edit the YAML if you need either field
> to stick. Plumbing them through is tracked as future work.

---

## Listing & precedence

`GET /api/plans` returns a `PlanSummary` per file in
`<claude-dir>/plans/`. Files are deduplicated by **stem name** — when
both `foo.yaml` and `foo.md` exist, the YAML wins (sorted first in
`list_plans`). Both files stay on disk; only the YAML is reachable via
the dashboard until the Markdown is renamed or removed.

---

## Examples

### Minimal valid plan

```yaml
title: Minimal Plan
phases:
  - number: 0
    title: Setup
    tasks:
      - number: "0.1"
        title: Init
```

Everything else takes its default. The plan parses and lists in the
sidebar.

### Full feature surface

```yaml
title: Architecture Documentation
context: |
  Promote module-level //! comments and bug-investigation notes
  into a published docs/ tree.
project: branchwork
created_at: "2026-04-01T09:00:00Z"
phases:
  - number: 3
    title: Reference pages
    description: One page per surface — CLI, configuration, schemas, drivers.
    tasks:
      - number: "3.3"
        title: Write reference/plan-schema.md
        description: |
          Promote the schema content from the root plan.yaml into
          docs/reference/plan-schema.md. Update plan.yaml to point
          to the new canonical page.
        file_paths:
          - docs/reference/plan-schema.md
          - plan.yaml
        acceptance: Schema doc covers every field parsed by plan_parser.rs.
        dependencies: ["3.1", "3.2"]
verification: |-
  1. plan.yaml header points to docs/reference/plan-schema.md.
  2. New doc lists every field on YamlPlan, YamlPlanPhase, YamlPlanTask.
```

### Investigation task — no commit

```yaml
title: Investigate Flaky Test
phases:
  - number: 0
    title: Reproduce
    tasks:
      - number: "0.1"
        title: Reproduce locally
        description: Run `cargo test foo --release` 50 times; capture failures.
        produces_commit: false
        acceptance: |
          docs/repro-flaky-foo.md captures repro frequency and stack trace.
```

The Merge button is hidden on this task; Discard remains.

### CI filter + phase verification

A plan that overrides the repo defaults — narrows the blocking-workflow
allowlist plan-wide, then carves out one slow phase that wants a stricter
verify suite:

```yaml
title: Cep auto-mode hardening
project: cep
ci_blocking_workflows: [CI, lint, typecheck]
phase_verification: bash scripts/verify.sh
phases:
  - number: 0
    title: Repro
    tasks:
      - number: "0.1"
        title: Capture failing SHA
        produces_commit: false
  - number: 1
    title: Fix
    # This phase replaces the plan-level verify with a richer one.
    phase_verification: |
      bash scripts/verify.sh && cargo deny check && cargo audit
    tasks:
      - number: "1.1"
        title: Land the fix
```

Per-phase overrides win, then plan-level, then `branchwork.toml`, then
the smart classifier (CI filter only). See [CI workflow filter](#ci-workflow-filter)
and [Phase-end verification](#phase-end-verification) for the full
precedence chain.

---

## See also

- [`plan.yaml`](../../plan.yaml) — in-repo template you can copy into `<claude-dir>/plans/` and adapt.
- [user-guide.md](../user-guide.md) — authoring workflow, Edit Plan modal, plan board UI.
- [architecture/server.md](../architecture/server.md) — file watcher, plan listing API.
- [architecture/persistence.md](../architecture/persistence.md) — how `task_status`, `agents`, and `ci_runs` rows back the server-only fields.
- [reference/cli.md](cli.md) — `branchwork-server --claude-dir` and the `<claude-dir>/plans/` location.
- [reference/branchwork-toml.md](branchwork-toml.md) — repo-level defaults for `ci_blocking_workflows` and `phase_verification`.
- [adrs/0006-phase-verify-and-ci-filter.md](../adrs/0006-phase-verify-and-ci-filter.md) — design rationale for the four-layer precedence and the smart classifier.
- [design-produces-commit.md](../design-produces-commit.md) — historical design note for the `produces_commit` field.
