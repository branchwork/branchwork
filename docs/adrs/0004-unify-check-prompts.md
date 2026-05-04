# ADR 0004 — Unify single-task and multi-task check prompts

- **Status:** Accepted (2026-05-04)
- **Authors:** cpo
- **Decision driver(s):** "Check phase" / "Check all" reporting `pending` for tasks that the single-task "Check" button reports `completed`; LLM-side branch-existence detection encoding a contract that doesn't match the real branch lifecycle

## Context

Branchwork's dashboard exposes three buttons that all spawn a "verify this task is done" check agent:

- The **Check** button on each `TaskCard` calls `POST /api/plans/:name/tasks/:num/check`, handled by `check_task` at `server-rs/src/api/plans.rs:1590`. The prompt is built **inline** at `server-rs/src/api/plans.rs:1664-1687`.
- The **Check phase** button on each `PhaseCard` and the **Check all** button on `PlanBoard` both call `POST /api/plans/:name/check-all`, handled by `check_all` at `server-rs/src/api/plans.rs:2770`. That handler loops over the phase's tasks and builds each prompt via `build_check_prompt` at `server-rs/src/api/plans.rs:2890-2970` (called at `server-rs/src/api/plans.rs:2842`).

Read side-by-side, the only material divergence between the two prompts is:

- `build_check_prompt` adds a **CRITICAL — verify the work is committed on the task branch:** block at `server-rs/src/api/plans.rs:2947-2957`,
- supported by the `task_branch`, `quoted_files`, `git_log_cmd`, and `git_log_unique_cmd` locals at `server-rs/src/api/plans.rs:2914-2936` that bake `git log branchwork/<plan>/<task>` invocations directly into the prompt text.

Everything else — the system framing, the JSON output contract, the project / plan / phase / task / file-paths / acceptance-criteria block — is byte-for-byte the same as `check_task`'s inline prompt.

### Live mis-verdict (2026-05-04)

Same task (`portable-agents-and-mcp/0.1`), two paths, two verdicts:

- Single-task check (inline prompt, `server-rs/src/api/plans.rs:1664`):

  ```json
  {"status": "completed", "reason": "Cargo.toml includes interprocess v2 + postcard v1, …"}
  ```

- Multi-task check (`build_check_prompt`, `server-rs/src/api/plans.rs:2890`):

  ```json
  {"status": "pending", "reason": "branch branchwork/portable-agents-and-mcp/0.1 does not exist; no evidence of task being started"}
  ```

The acceptance-criteria files are present in the working tree on master with the expected content. Both verdicts are run from the same `project_dir` against the same `(plan, phase, task)` inputs. The only thing the multi-task path "sees differently" is the strict `git log branchwork/portable-agents-and-mcp/0.1` block — and that branch was pruned at merge time, so the command errors and the agent dutifully reports "pending".

### Root cause

The strict block encodes a contract — *"committed work for a finished task lives on its per-task branch"* — that does not match the real lifecycle. After merge to master the per-task branch is pruned (standard Branchwork merge cleanup), so `git log branchwork/<plan>/<task>` fails. The LLM follows the prompt instructions, can't find the branch, and reports `pending` regardless of whether the working tree contains the work.

The "did the agent commit anything?" signal that the strict block was trying to enforce already exists server-side, in two more reliable places:

- `server-rs/src/agents/pty_agent.rs:613` — *"exited clean but left no commits — likely violated the unattended contract"* (introduced by `auto-mode-merge-ci-fix-loop.yaml` task 0.7), fired from `branch_has_no_commits_ahead_of_trunk` against the agent's actual `cwd` at the moment the PTY closes.
- `server-rs/src/api/agents.rs:584` — `merge_agent_branch_inner`'s `WireMergeOutcome::EmptyBranch` arm: *"task branch has no commits — agent exited without committing"*, returned as a hard merge refusal.

Both observe the agent's actual exit. Neither depends on the branch still existing at check time.

## Decision

**Collapse to one prompt — the simple `check_task` shape — used by both `check_task` and `check_all`.**

`build_check_prompt` keeps its current signature `(plan_name, plan, phase, task, project_dir) -> String` so the existing call site at `server-rs/src/api/plans.rs:2842` compiles unchanged. Its body is rewritten to produce the inline `check_task` prompt verbatim. `check_task` is then refactored to call `build_check_prompt` instead of holding its own `format!` — one builder, one shape, one source of truth.

Lifecycle-related detection (uncommitted work, branch existence, "did the agent actually commit anything?") **belongs server-side**, where it can observe the agent's actual exit and the actual state of the merge target. It does not belong inside an LLM prompt that runs at an arbitrary later time and has to re-derive the same signal from a working tree whose history has already been rewritten by the merge.

## Consequences

### Positive

- **Bug fix.** `check_task` and `check_all` return identical verdicts for the same `(plan, phase, task, project_dir)`. Picking "Check phase" or "Check all" no longer flips a `completed` task to `pending` for reasons unrelated to the task's actual state.
- **Smaller test surface.** Two unit tests in `server-rs/src/api/plans.rs` (`includes_git_log_verification_with_branch_and_files` at line 3008 and `falls_back_to_plain_git_log_when_no_files` at line 3053) get retired or rewritten by Phase 1.3 of the implementation plan. Phase 2 adds three regression guards (prompt-shape forbids the deleted vocabulary, byte-identity between both call sites, integration test of the pruned-branch case) — net replacement, not net growth.
- **One mental model for the prompt.** Future contributors don't have to ask which path their change should affect; there is one builder.

### Negative / preserved gaps

- The "agent exited without committing" signal disappears from the LLM's reasoning. **It is preserved server-side**, in two places that observe the actual exit rather than a re-derived after-the-fact signal:
  - `server-rs/src/agents/pty_agent.rs:613` (auto-mode unattended-contract diagnostic, log-only).
  - `server-rs/src/api/agents.rs:584` (`merge_agent_branch_inner` `EmptyBranch` arm, hard merge refusal).
  Future readers tempted to reintroduce the prompt-side check should reach for these instead — they fire at the canonical observation point and don't depend on branch lifetime.
- The prompt no longer asks the LLM to "verify the work is committed". Verification reduces to *"the working tree at `project_dir` contains the changes described in the acceptance criteria"*. That's the right contract for `check_*` — the merge-time guard already enforces commit existence at the integration point.

### Migration

- No DB schema change.
- No request/response shape change for any of the three endpoints (`check_task`, `check_all`, `check_plan`).
- No driver behaviour change. Only the prompt string sent to the LLM is altered.
- `start_check_agent` (`server-rs/src/agents/check_agent.rs:12`) and `build_plan_check_prompt` (`server-rs/src/api/plans.rs:1815`) are untouched — different concerns, called out as out-of-scope in the plan context.

## Rejected alternatives

### Keep the strict prompt + add "fall back to master if branch pruned" logic

Patch `build_check_prompt` to detect that the per-task branch has been pruned (e.g. by checking `git rev-parse --verify branchwork/<plan>/<task>` first and falling back to `git log master -- <files>` if it fails). Rejected because:

- It re-encodes branch lifetime inside the prompt, doubling the failure surface — the LLM now has to perform the fall-back correctly, on top of the original branch check.
- The "did the agent commit anything?" signal the strict block was trying to enforce is fundamentally an *exit-time* observation, not a *check-time* one. Re-deriving it from a working tree whose history has already been rewritten by merge gives the wrong answer in normal cases (pruned branch ≠ no commits) and the right answer only by coincidence.
- The server already owns this signal in two places that observe the actual exit. Adding a third (worse) version inside an LLM prompt is duplication, not safety in depth.

### Make the strict block opt-in via a request flag

Add a `?strict=true` query parameter (or a `strict: bool` request body field) so callers can toggle the branch-asserting block on demand. Rejected per **non-negotiable #4** in the plan context: *"No new prompt knobs. Don't add a 'strict' mode toggle. One prompt, one behaviour."* If a stricter "did anyone commit?" check is later wanted, it's a separate signal computed server-side alongside the LLM verdict, not a prompt-shape switch.

## References

- Implementation plan: `~/.claude/plans/unify-check-prompts.yaml` (Phase 0 — this ADR; Phases 1-3 — refactor, regression guards, hygiene).
- Inline `check_task` prompt: `server-rs/src/api/plans.rs:1664-1687`.
- `build_check_prompt`: `server-rs/src/api/plans.rs:2890-2970`; CRITICAL block at lines 2947-2957; `task_branch` / `quoted_files` / `git_log_cmd` / `git_log_unique_cmd` locals at lines 2914-2936.
- Unit tests retired / rewritten by Phase 1.3: `server-rs/src/api/plans.rs:3008` (`includes_git_log_verification_with_branch_and_files`), `server-rs/src/api/plans.rs:3053` (`falls_back_to_plain_git_log_when_no_files`).
- Server-side no-commit signals preserved: `server-rs/src/agents/pty_agent.rs:613`, `server-rs/src/api/agents.rs:584` (`merge_agent_branch_inner` `EmptyBranch` arm).
- Out of scope: `build_plan_check_prompt` (`server-rs/src/api/plans.rs:1815`), `start_check_agent` (`server-rs/src/agents/check_agent.rs:12`).
- ADR template reference: ADR 0002 (`docs/adrs/0002-worktree-per-agent-isolation.md`), ADR 0003 (`docs/adrs/0003-unattended-auto-mode.md`).
