# Repro: Navbar plan completion — false positives and false negatives

## Task 0.1 — Query task_status DB for both plans

### Summary

The `infer_status` heuristic in `auto_status.rs` marks tasks as "completed"
based solely on file existence (>=80% of referenced files present). This
produces two classes of bugs in the navbar's Done/Active grouping:

1. **False positive**: Plans whose tasks reference pre-existing files get
   auto-inferred as "completed" even when no agent or human worked on them.
2. **False negative**: Once `infer_status` writes a "pending" row (files
   don't exist yet), it never re-evaluates — the task stays "pending"
   even after the work is genuinely completed.

---

### Evidence: scheduler (false positive)

**API output** — `GET /api/plans` extract:
```json
{
  "name": "scheduler",
  "title": "Task scheduler — dep graph + file-conflict avoidance",
  "taskCount": 7,
  "doneCount": 7
}
```

**DB dump** — all 7 tasks completed at the *exact same timestamp*, no agents:
```
task_number  status       updated_at            source
1.1          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
1.2          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
1.3          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
2.1          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
2.2          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
3.1          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
3.2          completed    2026-04-16 10:41:26   AUTO-INFERRED (0 agents)
```

**Why it's false**: Every referenced file is a core file that predates the
scheduler plan (plans.rs, TaskCard.tsx, PlanBoard.tsx, etc.):
```
Task 1.1: 2/2 files exist → infer_status = "completed"
  ✓ server-rs/src/api/plans.rs          (created months earlier)
  ✓ server-rs/src/agents/mod.rs         (created months earlier)
Task 1.2: 2/2 files exist → infer_status = "completed"
  ✓ web/src/components/TaskCard.tsx      (created months earlier)
  ✓ web/src/stores/agent-store.ts       (created months earlier)
Task 1.3: 4/4 files exist → infer_status = "completed"
Task 2.1: 1/1 files exist → infer_status = "completed"
Task 2.2: 2/2 files exist → infer_status = "completed"
Task 3.1: 3/3 files exist → infer_status = "completed"
Task 3.2: 2/2 files exist → infer_status = "completed"
```

The scheduler work WAS done (by external Claude Code sessions), but
`infer_status` would produce the identical result even if NO scheduler
work had ever been performed. File existence proves nothing about task
completion.

**Navbar impact**: `isPlanDone(p)` at `Sidebar.tsx:19` returns `true`
(7 >= 7), so the plan is grouped under "Done" and collapsed by default.

---

### Evidence: portable-agents-and-mcp

**API output**:
```json
{
  "name": "portable-agents-and-mcp",
  "taskCount": 23,
  "doneCount": 23
}
```

**DB dump** — timestamps spread across 2 days, 34 agents ran:
```
task_number  status       updated_at            agents
0.1          completed    2026-04-14 15:35:19   1 agent
0.2          completed    2026-04-14 16:23:24   1 agent
...
3.7          completed    2026-04-16 13:22:50   1 agent
```

This plan is **genuinely complete** (real agents worked each task).
However, `infer_status` would produce the same "completed" result for
a brand-new plan with the same file references — all 23 tasks reference
files that exist today.

---

### Evidence: false negative mechanism

`infer_status` (auto_status.rs:95-126) has three failure modes that
produce false negatives:

1. **No file_paths** → always returns "pending" (line 102-104), even if
   the task is a design review, investigation, or config change that
   doesn't touch listed files.

2. **Files created after first inference** → auto-status only runs on
   tasks without a DB row (`api/plans.rs:775`). Once it writes "pending",
   subsequent sync-all calls skip re-evaluation.

3. **File paths don't match** → typos, renames, or bare filenames that
   `find_file_in_project` can't resolve keep the ratio below 0.8.

Plans affected today:
- `whimsical-wobbling-ember`: 15 tasks, all referencing Java/Vue files
  that don't exist in the project → would stay "pending" forever
- `e2e-container-persistence`: 12 tasks, 11 reference files not yet
  created → auto-status would write "pending" and never update

---

### Additional false positive: declarative-marinating-charm

```
task_number  status       updated_at            agents
0.1          completed    2026-04-12 22:14:36   0 agents
```
Single-task plan, auto-inferred as "completed" because `nfa.rs` exists.

---

### Root cause summary

| Location | Issue |
|----------|-------|
| `auto_status.rs:113` | `infer_status` returns "completed" when ≥80% files exist |
| `api/plans.rs:775` | Auto-status skips tasks with existing DB rows — no re-evaluation |
| `Sidebar.tsx:19` | `isPlanDone` uses `doneCount >= taskCount` — consumes false positives |
| No `source` column | Cannot distinguish auto-inferred vs agent/manual completions |

### Fix direction (Phase 2)

1. Cap `infer_status` at "in_progress" — never auto-complete
2. Add `source` column to `task_status` (auto vs manual vs agent)
3. Clean up stale auto-inferred "completed" rows

---

## Task 0.2 — Trigger auto-status and observe the heuristic

### Method

Cleared the `task_status` DB rows for both plans (scheduler: all 7,
portable-agents-and-mcp: 3 sample tasks), then called
`POST /api/plans/{name}/auto-status` to force `infer_status` to run
fresh instead of returning `"existing (kept)"`.

### scheduler auto-status response (all 7 tasks re-inferred)

```json
{
    "plan": "scheduler",
    "project": "Branchwork",
    "projectDir": "/home/cpo/Branchwork",
    "results": [
        {"taskNumber": "1.1", "status": "completed", "reason": "2/2 files exist",
         "title": "Conflict check in `start_task`"},
        {"taskNumber": "1.2", "status": "completed", "reason": "2/2 files exist",
         "title": "Frontend surfaces the 409 clearly"},
        {"taskNumber": "1.3", "status": "completed", "reason": "4/4 files exist",
         "title": "Opt-in auto-queue instead of hard 409"},
        {"taskNumber": "2.1", "status": "completed", "reason": "1/1 files exist",
         "title": "Task card shows \"blocked by\" reasons"},
        {"taskNumber": "2.2", "status": "completed", "reason": "2/2 files exist",
         "title": "Plan-level \"Ready to start\" summary"},
        {"taskNumber": "3.1", "status": "completed", "reason": "3/3 files exist",
         "title": "Per-plan auto-advance flag"},
        {"taskNumber": "3.2", "status": "completed", "reason": "2/2 files exist",
         "title": "\"Run Plan\" button"}
    ],
    "summary": {"completed": 7, "in_progress": 0, "pending": 0, "total": 7}
}
```

**Every task marked "completed" solely because files exist.** None has a
single agent run or manual status update — the heuristic is the only signal.

### portable-agents-and-mcp auto-status (3 tasks re-inferred)

Cleared tasks 0.5, 2.1, 3.7 and re-triggered:

```json
{"taskNumber": "0.5", "status": "completed", "reason": "1/1 files exist",
 "title": "Remove tmux as a dependency"}
{"taskNumber": "2.1", "status": "completed", "reason": "1/1 files exist",
 "title": "Choose MCP implementation — Rust SDK"}
{"taskNumber": "3.7", "status": "completed", "reason": "3/3 files exist",
 "title": "Deployment artifacts"}
```

These tasks happen to be genuinely complete (agents ran), but
`infer_status` produces the identical result for any plan whose tasks
reference existing files.

### The heuristic code path

```
POST /api/plans/:name/auto-status
  → api/plans.rs:771-827  (auto_status_plan handler)
    → for each task: check if task_status row exists
      → YES: return "existing (kept)", skip
      → NO:  call auto_status::infer_status(project_dir, file_paths, _)
        → auto_status.rs:100-126
          → count files found via find_file_in_project
          → ≥80% exist → "completed", "N/N files exist"
          → INSERT INTO task_status
```

### Confirmed root cause

The file-existence heuristic (`auto_status.rs:113`, `ratio >= 0.8`)
is the sole source of false positives. It:

1. Treats file *existence* as proof of task *completion*
2. Has no concept of file creation time vs plan creation time
3. Has no concept of agent work — tasks are "completed" without
   any agent ever running
4. Persists the inferred status to DB, where it is never re-evaluated
5. Once persisted, `isPlanDone` in `Sidebar.tsx:19` consumes the
   false `doneCount` and misclassifies the plan as Done

---

## Task 1.1 — Trace the `infer_status` heuristic

### The function under inspection

`infer_status` lives at `server-rs/src/auto_status.rs:95-126`. The body
is small enough to quote in full:

```rust
pub fn infer_status(
    project_dir: &Path,
    file_paths: &[String],
    _title_words: &[&str],
) -> (&'static str, String) {
    let total_checked = file_paths.len();

    if total_checked == 0 {
        return ("pending", "no file paths to check".into());   // ← branch A
    }

    let found_count = file_paths
        .iter()
        .filter(|fp| find_file_in_project(project_dir, fp))
        .count();

    let ratio = found_count as f64 / total_checked as f64;
    if ratio >= 0.8 {
        ("completed", format!("{found_count}/{total_checked} files exist"))   // ← branch B
    } else if found_count > 0 {
        ("in_progress", format!("{found_count}/{total_checked} files exist"))
    } else {
        ("pending", format!("0/{total_checked} files exist"))
    }
}
```

The third argument (`_title_words`) is unused — git-history grep was
disabled because common keywords (`server`, `driver`, `agent`) match
too many existing commits. So the only signal is file existence.

### Decision table

| `file_paths.len()` | `found_count` | `ratio` | Result |
|--------------------|---------------|---------|--------|
| 0                  | n/a           | n/a     | `pending` ("no file paths to check") |
| N ≥ 1              | ≥ 0.8·N       | ≥ 0.80  | **`completed`** (`N/N files exist`) |
| N ≥ 1              | 1..0.8·N      | 0..0.80 | `in_progress` |
| N ≥ 1              | 0             | 0.00    | `pending` |

The pivotal row is the second: **branch B at line 113 returns
`"completed"`** based purely on the ratio of files-on-disk to
files-listed. This is the root cause.

### Why branch B is wrong

`find_file_in_project` (auto_status.rs:5-45) returns `true` if a path
exists *right now*, with no regard for:

- **When the file was created.** A pre-existing `server-rs/src/api/plans.rs`
  satisfies a brand-new task whose description is "add feature X to plans.rs".
- **What changed inside it.** The check is `path.exists()`, not a content
  diff or a git-blame on the listed lines.
- **Whether any agent ran.** A plan can be marked "completed" with zero
  rows in the `agents` table.
- **The plan's own creation time.** A plan whose YAML was written this
  morning will be auto-completed if its referenced files happen to exist.

This is why the scheduler plan (Task 0.1 evidence) flips to "completed"
instantly: every referenced file (`api/plans.rs`, `agents/mod.rs`,
`TaskCard.tsx`, `agent-store.ts`, …) is a long-lived core file that
predates the plan by months. The 80 % threshold, plus the fact that
listing 1–4 files makes `0.8` trivially reachable (`1/1 = 1.0`,
`2/2 = 1.0`, `4/4 = 1.0`), makes branch B the *expected* outcome for
any plan that anchors its tasks to existing modules.

### Code path that bakes the false positive into the DB

```
caller                                    file:line
──────────────────────────────────────    ───────────────────────────────
POST /api/plans/:name/auto-status   →     api/plans.rs:771-813
   (skipped if DB row exists,             api/plans.rs:775
    so the heuristic only writes
    once per task)
   ↓
auto_status::infer_status            →    auto_status.rs:95-126
   ratio ≥ 0.8 → "completed"              auto_status.rs:113   ← root cause
   ↓
INSERT INTO task_status              →    api/plans.rs:796-803
   ↓
GET /api/plans recomputes doneCount  →    api/plans.rs:99-105
   from task_status (status IN
   ('completed','skipped'))
   ↓
Sidebar isPlanDone(p)                →    Sidebar.tsx:19-21
   doneCount >= taskCount  →  Done fold
```

Each downstream step is correct in isolation: the API counts what the
DB says, the sidebar reads what the API ships. The fault is the *value
written* by line 113.

### Acceptance criteria for Task 1.1

> Root cause documented: `infer_status` ≥80 % threshold produces false
> `completed` for tasks whose files pre-exist.

✅ Confirmed. The ≥0.8 branch at `server-rs/src/auto_status.rs:113`
returns `"completed"` whenever the listed files exist on disk, with no
guard for file age, content, or agent activity. Phase 2 (Tasks 2.1-2.3)
will:

- Cap `infer_status` at `in_progress` so file existence can never
  produce `completed` (Task 2.1 — change line 113's branch).
- Add a `source` column to `task_status` so re-runs of auto-status can
  overwrite their own prior `auto` rows but never overwrite `manual`
  ones (Task 2.2).
- Provide a cleanup path for legacy bulk-inferred `completed` rows
  (Task 2.3).

## Task 1.2 — Confirm `done_count` computation in `list_plans` API

The read path that turns DB rows into the navbar's Done/Active split is
covered end-to-end by three short pieces of code. None of them are
buggy; both the false-positive and false-negative flows ride the same
correct pipeline, so the only thing that decides which side a plan
lands on is what `auto_status::infer_status` (Task 1.1) wrote into
`task_status` for it.

**1. `task_count` — comes straight from the YAML.**
`server-rs/src/api/plans.rs:139` sets `task_count: s.task_count` from
the `PlanSummary` returned by `plan_parser::list_plans`, which counts
tasks across phases at parse time.

**2. `done_count` — YAML × DB join.**
`api/plans.rs:99-128` re-parses the plan file, loads every
`task_status` row for that plan into a `HashMap<task_number, status>`,
and counts tasks whose looked-up status is `"completed"` or
`"skipped"`. Tasks with no DB row default to `"pending"` and are
excluded. The two terminal statuses are explicit on line 124:

```rust
.filter(|t| {
    let status = status_map
        .get(&t.number)
        .map(|s| s.as_str())
        .unwrap_or("pending");
    status == "completed" || status == "skipped"
})
.count()
```

**3. `isPlanDone` — pure arithmetic.**
`web/src/components/Sidebar.tsx:19-21` is the entire predicate:

```ts
function isPlanDone(p: PlanSummary): boolean {
  return p.taskCount > 0 && p.doneCount >= p.taskCount;
}
```

The `> 0` guard is the only embellishment — empty plans never count as
done. There is no second predicate, no cross-plan rollup, no inspection
of `PlanTask.status`; the same function decides the fold for every
plan in the navbar (`Sidebar.tsx:80`) and the project dashboard
(`ProjectDashboard.tsx:17-19` is byte-identical).

### Why both bug directions trace back to `auto_status`

| Symptom | DB state | Read path output |
|---|---|---|
| False positive (e.g. `scheduler` plan flipped to Done) | All 7 tasks have `task_status.status = "completed"`, written by `infer_status` ≥0.8 branch | `done_count = 7`, `task_count = 7`, `7 >= 7` → Done fold |
| False negative (real-completed plan stays in Active) | One or more tasks lack `completed`/`skipped` rows, or hold `in_progress` / `failed` | `done_count < task_count` → Active fold |

The sidebar arithmetic is symmetric: it cannot distinguish a heuristic
"completed" from a manual one, and it cannot distinguish a missing row
from an in-progress one. Whatever `task_status` says, it shows.

### Acceptance criteria for Task 1.2

> Confirmed that the sidebar logic is correct; the bug is in the DB
> data produced by `auto_status`.

✅ Confirmed. `task_count` reflects the YAML, `done_count` reflects the
DB, and `isPlanDone` is a single `doneCount >= taskCount` comparison.
The fix surface for Phase 2 is upstream of all three: the value
`auto_status::infer_status` writes at `auto_status.rs:113`, and the
write-once gate at `api/plans.rs:775` that prevents corrected runs
from overwriting bad rows.
