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
