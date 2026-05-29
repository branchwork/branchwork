# Worktree-per-agent isolation — cwd / git-checkout / project-root audit

**Plan:** Worktree-per-agent isolation + at-merge conflict resolver — Phase 0,
Task 0.2.
**Status:** audit only. **No code is changed by this task.** This file is the
change-list a reviewer hands to Phase 2 (agent-spawn integration) and Phase 3
(at-merge conflict resolver).

## The model change this audit is sizing

Today an agent's **`cwd` is the project root itself**, and the agent works on a
task branch by *checking that branch out in the single shared working tree*
(`git checkout -b branchwork/<plan>/<task>`). Merges and discards then operate
in that same single tree: `git checkout master && git merge <task_branch>` /
`git branch -D <task_branch>`.

After Phase 2, each agent gets its **own `git worktree`** (a sibling directory
backed by the same `.git`), so:

- `agents.cwd` becomes a **worktree path** (e.g. `<project>/.worktrees/<id>` or
  a `/tmp` path), not the project root. Every reader of `agents.cwd` now sees
  that path.
- `git checkout <branch>` inside the worktree is **unnecessary** — the worktree
  *is* the branch. Worse, `git checkout master` inside a worktree pinned to the
  task branch will fail or move the worktree off its branch.
- **Branch refs are shared** across all worktrees of a repo (one `.git`/ref
  store). So `git branch`, `git show-ref`, `git rev-list <a>..<b>`,
  default-branch resolution, and `git diff <base>` all behave identically from
  any worktree → those reads are **path-agnostic** and stay correct.
- **Merge cannot run in the agent's worktree** (it's pinned to the task
  branch). It must run at the project root or in a dedicated merge worktree —
  Phase 3's domain.
- **`git branch -D <task_branch>` fails** while the branch is checked out in a
  worktree → discard must `git worktree remove` first, then delete the branch.

### Classification key

- **must-change** — the site will see worktree paths now, and its current
  behaviour breaks or becomes wrong under worktree isolation.
- **no-change** — already path-agnostic: the operation works identically whether
  `cwd` is the project root or a worktree of it (typically because it only
  touches the shared ref store or diffs against a recorded SHA).
- **test-only** — a test fixture that bootstraps a repo / seeds `agents.cwd`;
  needs a fixture adjustment but no production behaviour change.

### Two worktree consumers already exist (precedent for Phase 2)

The codebase already creates `git worktree`s in two read-only flows — Phase 2
should mirror their shape rather than invent a new one:

- `server-rs/src/agents/worktree.rs` — `TempWorktree::create(project_dir,
  agent_id, branch)` → `git worktree add --detach /tmp/bw-gate-<id> <branch>`
  with a `Drop` guard running `git worktree remove --force`. Used by the
  pre-merge gate (`auto_mode.rs::run_pre_merge_gate`).
- `server-rs/src/agents/phase_check.rs:744` — `create_worktree(project_dir,
  phase_sha)` → `git worktree add --detach <tmp> <sha>`, used by the phase-end
  Check agent. `cleanup_worktree` at `phase_check.rs:774`.

Both run `git worktree add` **from the project root** (`current_dir(project_dir)`)
and detach so they don't contend with the branch the agent has checked out.

---

## A. Reads of `agents.cwd`

### A.1 DB `SELECT cwd` (production)

| Site | What it does | Classification | Note |
| --- | --- | --- | --- |
| `api/agents.rs:94` (`list_agents`, col 6) → JSON at `api/agents.rs:120` | Serialises `cwd` into the `/api/agents` response | **no-change** (cosmetic) | Dashboard will display worktree paths in the agent list/diff panel. No behaviour break, but UX worth knowing — the path the user sees is no longer the project root. |
| `api/agents.rs:538` (`get_agent_diff`: `SELECT cwd, base_commit`) → `git diff <base> --no-color` `.current_dir(&cwd)` (also `--stat`, `--name-only` at :600/:613) | Computes the agent's diff vs `base_commit` | **no-change** | The worktree *is* where the agent worked; diffing against the recorded base SHA inside it is exactly right. Arguably *more* correct than today (no sibling-agent edits to bleed in). |
| `api/agents.rs:653` (`list_merge_targets`: `SELECT cwd, branch, org_id`) → `git_ops::default_branch` / `list_branches` (cwd) | Populates the merge-target dropdown | **no-change** | Branch listing + default-branch resolution read the shared ref store; identical from any worktree. |
| `api/agents.rs:769` (`merge_agent_branch_inner`: `SELECT cwd, branch, plan_name, task_id, org_id`) → `git_ops::merge_branch(cwd, target, task_branch)` and `TriggerArgs.cwd` | Merges the task branch into the default branch, then pushes | **must-change** | `merge_branch_local` does `git checkout <target>` **in cwd**. If cwd is the agent's worktree (pinned to the task branch) this fails / corrupts the worktree. Merge must run at the project root or a dedicated merge worktree. **Core Phase 3 site.** |
| `api/agents.rs:963` (`TriggerArgs { cwd: PathBuf::from(&cwd), .. }`) → `ci::trigger_after_merge` | Pushes merged trunk + records `ci_runs` row | **must-change** | `trigger_after_merge` runs `has_github_actions(&cwd)`, `has_remote(&cwd,"origin")`, `default_branch(..&cwd)`, and `push_branch(..&cwd)` — it needs a working tree that holds the merged default branch, which the agent's task-branch worktree is not. Must point at the merge location (project root). |
| `api/agents.rs:1099` (`discard_agent_branch`: `SELECT cwd, branch, org_id`) → `git checkout <target>` (`:1210`) + `git branch -D <task_branch>` (`:1228`), both `.current_dir(&cwd)` | Discards an agent's task branch | **must-change** | `git branch -D` **refuses while the branch is checked out in a worktree**. Discard must `git worktree remove` the agent's worktree first, then delete the branch from the project root. |
| `auto_mode.rs:1101` (`resume_after_clean_tree` / gate-fix path: `SELECT cwd, branch`) → spawns gate-fix agent in `cwd` | Re-spawns a fix agent on the task branch after a pre-merge-gate failure | **must-change** | Fix agents continue the same task branch; under worktrees they need a worktree-derived cwd, not the (now-stale) project-root cwd. |
| `auto_mode.rs:2332` (`spawn_fix_agent`: `SELECT cwd … WHERE task_id NOT LIKE '%-fix-%'`) → spawns CI-fix agent in `cwd` | Re-spawns a fix agent after CI red | **must-change** | Same as above. Note the original task agent's worktree may already be cleaned up by merge time — the fix-agent spawn path must (re)create a worktree, not reuse a dead `cwd` string. |
| `agents/mod.rs:625` (`reconcile_orphaned_branches`: `SELECT id, branch, cwd … mode != 'remote'`) → `Path::is_dir(cwd)` guard + `git show-ref --verify refs/heads/<branch>` `.current_dir(&cwd)` | Boot sweep: clears `branch` when the local ref is gone | **must-change** | Two problems: (1) the `is_dir(cwd)` guard skips the row when the worktree has been removed (so the branch is never reconciled); (2) `git show-ref` should read the **shared ref store** (project root), not a disposable worktree. Resolve the project root for the ref probe instead of trusting `cwd`. |
| `saas/dispatch.rs:161` (`has_github_actions_dispatch`, standalone branch: `SELECT cwd`) → `has_github_actions_local(cwd)` | Checks `.github/workflows/*.yml` exists | **no-change** | A worktree has `.github/workflows` checked out too, so the probe still succeeds; could be retargeted to the project root for clarity but is not load-bearing. |
| `agents/pty_agent.rs:648` (`on_agent_exit`: `SELECT cwd`) → `branch_has_no_commits_ahead_of_trunk(cwd, branch)` (`git rev-list --count default..branch`) | Unattended-contract diagnostic (`eprintln` only) on clean exit | **no-change** | Reads the shared ref store; `rev-list` is correct from any worktree. Diagnostic only — no pause/state change here. |

### A.2 `cwd` struct-field reads (production)

| Site | Field | Classification | Note |
| --- | --- | --- | --- |
| `agents/pty_agent.rs:62` (`StartPtyOpts.cwd`, destructured at `:59`) | spawn cwd → `git_head_sha(cwd)` `:88`, `git_default_branch(cwd)` `:96`, `git_checkout_branch(cwd,…)` `:100`, INSERT `:113` | **must-change** | The pivot. `cwd` must become a freshly-created worktree path; the `git_checkout_branch` call disappears (worktree add does the branch placement). |
| `agents/spawn_ops.rs:170,185` (`opts.cwd` → `cwd_str`) | SaaS dispatch: INSERT `:269` + `StartAgent { cwd }` wire `:492` | **must-change** | Server hands the cwd to the runner; the runner (or server) must create the worktree. SaaS worktree handling is downstream of standalone but the wire field is the same string. |
| `bin/branchwork_runner.rs:884` (`AgentHandle.cwd`) read at `:2379` (`MergeAgentBranch`), `:2431` (`HasGithubActions`) | runner-side per-agent cwd | **must-change** (SaaS) | Same merge-in-worktree problem on the runner side. `merge_agent_branch_on_runner(cwd)` checks out `target` in the agent's cwd. |
| `bin/branchwork_runner.rs:1466` (`agent_cwd = cwd or state.cwd`) → `checkout_task_branch(&agent_cwd,…)` `:1492`, `spawn_agent(…&agent_cwd…)` `:1511` | runner spawn cwd | **must-change** (SaaS) | Runner's equivalent of `start_pty_agent`'s checkout dance. |

### A.3 `agents.cwd` is write-once — there is **no** `UPDATE agents SET cwd`

Confirmed by grep: `cwd` is only ever set at INSERT time. A worktree path
assigned at spawn lives for the agent's lifetime. (Relevant for Phase 2: no
"migrate cwd" UPDATE path exists or is needed.)

---

## B. Writes to `agents.cwd` (INSERT)

### B.1 Production INSERTs

| Site | Context | cwd value today | Classification |
| --- | --- | --- | --- |
| `agents/pty_agent.rs:107` | `start_pty_agent` (standalone spawn) | project root (`opts.cwd`) | **must-change** → write the worktree path |
| `agents/spawn_ops.rs:263` | `start_agent_via_runner` (SaaS pre-insert) | project root (`cwd_str`) | **must-change** |
| `saas/runner_ws.rs:848` | `AgentStarted` upsert (SaaS confirm) | from wire `cwd` | **must-change** (mirror of the value spawn_ops sent) |
| `agents/check_agent.rs:27` | `start_check_agent` (standalone Check) | project root | **no-change / review** — Check agents are read-only; today they run at the project root and diff/inspect. Phase 2 may move them into a detached worktree like `phase_check` already does, but nothing breaks if they stay at the root. |
| `agents/check_agent.rs:425` | `start_check_agent_remote` (SaaS Check) | from caller | **no-change / review** (same as above) |
| `agents/phase_check.rs:361` | `spawn_and_await_check_agent` (phase-end verify) | **already a worktree** (`worktree_dir`) | **no-change** — this flow is already worktree-based; it's the template to copy. |
| `mcp/tools/status.rs:291` | `report_cost` synthetic row | empty string `''` | **no-change** — cost-only row, never used as a working dir. |

### B.2 Test-fixture INSERTs (test-only)

All of these seed `agents.cwd` for tests; they live in `#[cfg(test)]` modules
(verified: `agents/mod.rs` tests begin at line 1987, `db.rs` tests at 2495).

| Site | cwd seeded today |
| --- | --- |
| `db.rs:3079, 3094, 3120, 3135, 3154, 3174, 3180, 3264` (db unit-test helpers) | `/tmp`-ish / sentinel |
| `agents/mod.rs:2089, 2131, 2150, 2526, 2776, 2955, 2983, 2989` (try_auto_advance / cleanup / spawn_ready_tasks tests) | sentinel or `branchwork-no-such-dir-*` |
| `agents/mod.rs:3062` (`insert_branched_agent`, used by `reconcile_orphaned_branches` tests) | a real git repo dir (needs `show-ref` to work) |
| `hooks.rs:350, 363` (`seed_running_agent` / `seed_running_session_agent`) | `dir/project` |
| `tests/merge_guard.rs:26` (`seed_agent`) | `d.project` (project root) |
| `tests/recovery.rs:85, 229` (inline INSERTs) | `/tmp/scratch` sentinel / `d.project` |

**Classification:** test-only. Phase 2 changes the spawn path to write worktree
paths; any test that asserts on `agents.cwd == project root`, or that seeds a
`cwd` and then expects merge/discard/checkout to operate on the project root,
needs the fixture adjusted to model a worktree (see Section E).

---

## C. `git checkout` `Command` invocations

### C.1 Production `git checkout` sites

| Site | Command | Runs in | Classification | Note |
| --- | --- | --- | --- | --- |
| `agents/mod.rs:198` | `git checkout <branch>` (continue) | agent `cwd` | **must-change** | Inside `git_checkout_branch`. With worktrees the branch placement is done by `git worktree add <path> <branch>`; this whole helper should no longer run on the agent spawn path (it stays only for any non-worktree fallback). |
| `agents/mod.rs:215` | `git checkout -b <branch>` (create) | agent `cwd` | **must-change** | Same helper, create arm. |
| `agents/mod.rs:226` | `git checkout <branch>` (fallback) | agent `cwd` | **must-change** | Same helper, exists-already arm. |
| `api/agents.rs:1210` | `git checkout <target>` (discard step 1) | agent `cwd` | **must-change** | Part of discard; the checkout-then-`branch -D` dance must become `worktree remove` + branch delete at the project root. |
| `api/ci.rs:236` | `git checkout master`/`main` (Fix-CI: land on trunk before capturing source_branch) | project `cwd` (`project_dir_for`) | **no-change / review** | Fix-CI runs in the resolved project dir, **not** an agent worktree (it shells out before spawning). Stays valid; but note Phase 2 may want Fix-CI to spawn into a worktree like other agents (`api/ci.rs` then spawns via `start_pty_agent`). |
| `api/ci.rs:257` | `git checkout -b <fix_branch> <commit_sha>` | project `cwd` | **no-change / review** | Pre-creates the recovery branch in the project root; the spawned fix agent then `is_continue: true` checks it out. Under worktrees this becomes "create the branch ref, then `git worktree add` it". Phase 2 review item. |
| `git_helpers.rs:165` | `git checkout <target>` (inside `merge_branch_local`) | merge `cwd` | **must-change** | The merge sequence checks out the target in `cwd`; cannot be the agent's task-branch worktree. **Core Phase 3 site** (shared by standalone *and* runner via `#[path]`). |
| `git_helpers.rs:224` | `git checkout <target>` (inside `discard_branch_local`) | discard `cwd` | **must-change** | Runner-side / shared discard; same `branch -D`-while-checked-out problem. |
| `bin/branchwork_runner.rs:3281` | `git checkout <branch>` (existing) | agent `cwd` | **must-change** (SaaS) | Inside `checkout_task_branch`; runner's equivalent of `git_checkout_branch`. Replaced by `git worktree add` on the runner. |
| `bin/branchwork_runner.rs:3291` | `git checkout -b <branch>` (create) | agent `cwd` | **must-change** (SaaS) | Same helper, create arm. |

### C.2 Test-only `git checkout` sites

These are all inside `#[cfg(test)]` modules or test helper fns; they bootstrap
branches in scratch repos. Adjust as fixtures, not behaviour:

`git_helpers.rs:836, 840, 871, 876` · `agents/pty_agent.rs:1039` ·
`agents/worktree.rs:170, 174` · `auto_mode.rs:3556, 3562, 4098, 4106, 8486,
8490` · `bin/branchwork_runner.rs:3375, 3391, 4815, 4819, 4879, 4883, 5108,
5184, 5222, 5228, 5234`.

**Classification:** test-only.

---

## D. `ensure_git_initialized` / `git_checkout_branch` / `git_head_sha` / `git_current_branch` (+ `git_default_branch` / `git_list_branches`) and the cwd they pass

Helper definitions: `agents/mod.rs:94` (`ensure_git_initialized`), `:151`
(`git_head_sha`), `:165` (`git_current_branch`), `:194` (`git_checkout_branch`),
`:189` (re-export of `git_helpers::git_default_branch`). Leaf copies for the
shared/runner path live in `git_helpers.rs:31` (`git_default_branch`), `:71`
(`git_list_branches`), `:98` (`git_current_branch`), `:117` (`git_head_sha`).

### D.1 `ensure_git_initialized(cwd)`

| Site | cwd passed | Classification | Note |
| --- | --- | --- | --- |
| `api/plans.rs:3751` (`start_task`) | `work_dir` = `home.join(project)` or `body.cwd` (project root) | **no-change** | The **project root** must still be a git repo — that's where worktrees are added *from*. `ensure_git_initialized` keeps targeting the project root, not the worktree. Phase 2 just needs to make sure init happens *before* the worktree is created. |
| `api/plans.rs:3981` (`start_phase_tasks`) | `work_dir` | **no-change** | Same. |
| `api/plans.rs:4213` (`start_plan_session`) | `work_dir` | **no-change** | Same. |
| `api/plans.rs:4795` (`create_plan`) | `resolved` (freshly-made project folder) | **no-change** | Same. |

All four are gated on `!org_has_runner` (standalone only). **Key insight:** these
target the *project root* (`work_dir`), which the worktree plan must keep as the
git-repo anchor. The thing that changes is what gets passed to
`start_agent_dispatch` as `cwd` (a worktree of `work_dir`, not `work_dir`
itself — see `api/plans.rs:3771, 4022, 4232, 4852`).

### D.2 `git_checkout_branch(cwd, branch, is_continue)`

| Site | cwd | Classification |
| --- | --- | --- |
| `agents/pty_agent.rs:100` (`start_pty_agent`) | agent spawn cwd | **must-change** — replaced by `git worktree add <path> <branch>` (or `--detach` then checkout). The single call that establishes the agent's branch today. |

(No other production callers; the runner's `checkout_task_branch` at
`branchwork_runner.rs:3258` is the SaaS analogue — see C.1.)

### D.3 `git_head_sha(cwd)`

| Site | cwd | Classification | Note |
| --- | --- | --- | --- |
| `agents/pty_agent.rs:88` (`start_pty_agent`, `base_commit`) | agent spawn cwd | **no-change** | Capturing HEAD; in a worktree, HEAD-before-work is the branch tip (= base_commit), still correct. Must be captured *after* `git worktree add` and *before* the agent edits. |
| `agents/check_agent.rs:22` (`start_check_agent`) | check cwd | **no-change** | Records base for diffing; correct in any worktree. |
| `agents/phase_check.rs:356` (`spawn_and_await_check_agent`) | worktree dir | **no-change** | Already worktree-based. |
| `git_helpers.rs:196, 534` (inside `merge_branch_local` / `rebase_against_origin`) | merge/push cwd | **must-change (by association)** | Not the cwd that's wrong — these run wherever the merge runs; see the merge sites in A.1/C.1. |
| `agents/pty_agent.rs:474` (`branch_has_no_commits_ahead_of_trunk`, calls `git_default_branch` + `git rev-list`) | agent `cwd` | **no-change** | `rev-list default..branch` reads the shared ref store; correct from any worktree. |

### D.4 `git_current_branch(cwd)`

| Site | cwd | Classification | Note |
| --- | --- | --- | --- |
| `api/ci.rs:245` (Fix-CI, capture `original_branch`) | project `cwd` | **no-change / review** | Runs in the project dir before spawning; valid. |
| `bin/branchwork_runner.rs:2903` (`merge_agent_branch_on_runner`, resolve task_branch from HEAD) | agent worktree cwd | **review** | Reads "what branch is HEAD" — in a worktree that's the task branch, which is exactly what's wanted. But it's read in order to *merge in the same cwd*, which is the must-change merge problem (A.1). The read itself is fine. |

### D.5 `git_default_branch(cwd)` / `git_list_branches(cwd)`

All of these read the **shared ref store**, so they are correct from any
worktree → **no-change**:

| Site | Helper | Classification |
| --- | --- | --- |
| `agents/pty_agent.rs:96` (`source_branch`) | `git_default_branch` | no-change |
| `agents/pty_agent.rs:475` | `git_default_branch` | no-change |
| `api/agents.rs:1205` (discard target resolve, standalone) | `git_default_branch` | no-change (the *read*; the subsequent checkout/delete is must-change) |
| `api/agents.rs:672/678, 820/843, 1143` | `git_ops::default_branch` / `list_branches` (dispatcher) | no-change |
| `bin/branchwork_runner.rs:2183, 2912` | `git_default_branch` | no-change |
| `bin/branchwork_runner.rs:2208, 2915` | `git_list_branches` | no-change |

---

## E. Test fixtures that bootstrap a bare repo / project root an agent runs inside

The shared assumption across every fixture: **one scratch `project/` repo, and
`agents.cwd` == that project root.** Under worktree isolation the spawn path
writes a worktree path; fixtures that seed `cwd = project root` and then exercise
merge/discard/checkout will need a worktree-aware shape (or the worktree manager
will create the worktree and the fixture must seed accordingly).

| Fixture | What it bootstraps | Classification | Note |
| --- | --- | --- | --- |
| `tests/support/mod.rs:51` (`TestDashboard::new_with_env`) | scratch `project/` git repo on `master` + 1 commit; `HOME=tempdir` so `project_dir_for` resolves `project` | **test-only** | Canonical fixture. The repo it inits is the project root that worktrees would be added from — Phase 2 may add a helper that `git worktree add`s here. |
| `tests/support/mod.rs:177` (`create_task_branch`) | `git checkout -q -b <branch>` **in `self.project`**, optional commit, then `git checkout -q master` | **test-only (must-adjust)** | Models the *old* single-tree branch flow. To exercise worktree merge/discard, this needs to create the branch in a worktree (or the helper stays for ref-only setup and a new `create_task_worktree` helper is added). |
| `tests/support/mod.rs:197` (`setup_github_actions`) | commits `.github/workflows/ci.yml` on the current branch of `self.project` | **test-only** | Project-root fixture; fine, but commit it *before* creating task worktrees so they inherit it. |
| `tests/support/mod.rs:215` (`setup_origin_remote`) | `git init --bare` in tempdir, adds as `origin` | **test-only** | Bare-repo origin for push tests. Worktrees share the remote — no change to the bare repo itself. |
| `tests/support/mod.rs:234` (`local_branches`) | `git branch` in `self.project` | **test-only** | Reads shared refs; valid for worktrees. |
| `tests/merge_guard.rs:15` (`seed_agent`) | INSERT `agents` with `cwd = d.project` | **test-only (must-adjust)** | Every merge-guard test seeds the project root as cwd and then merges. Under worktrees the seeded cwd should be the agent's worktree, and the merge target lands at the project root. This is the file the conflict-resolver tests (Phase 3) extend. |
| `tests/merge_guard.rs:44` (`minimal_plan`) | YAML with absolute `project:` path | **test-only** | No cwd; fine. |
| `tests/merge_guard.rs:514` (CI-trigger helper: `.github/workflows/ci.yml` + bare `origin`) | project-root fixture | **test-only** | Same as `setup_github_actions`/`setup_origin_remote`. |
| `tests/recovery.rs:85` (inline INSERT, `cwd='/tmp/scratch'` sentinel) | reset/agent-running guard tests | **test-only** | cwd is a sentinel, never used as a working dir. |
| `tests/recovery.rs:229` (inline INSERT, `cwd` + `branch`) | stale-branch / live-agent purge tests | **test-only (must-adjust)** | If the cwd needs to host a real worktree for the show-ref reconcile path, model a worktree. |
| `tests/recovery.rs:470, 560` (`git` `.current_dir(&d.project)`) | Fix-CI recovery tests | **test-only** | Project-root git ops; valid. |
| `tests/unattended_auto_mode_e2e.rs` | bash `claude` stub commits on the task branch in the spawned agent's cwd | **test-only (must-adjust)** | End-to-end; the stub's `git commit` happens in whatever cwd the agent was spawned with — once that's a worktree, the stub keeps working but the test's branch/SHA assertions key off the worktree. |
| `agents/mod.rs:3062` (`insert_branched_agent`) + `git_init_with_commit` test helpers | inline scratch repos for `reconcile_orphaned_branches` unit tests | **test-only (must-adjust)** | These probe `git show-ref` in `cwd`; if the reconcile path moves to the project root (per A.1), the fixture follows. |
| `hooks.rs:317` (`git_init_with_clean_tree`) + `seed_running_agent` | Stop-hook tree-clean tests; `cwd = dir/project` | **test-only** | The tree-clean check (`check_tree_clean_for_completion`) runs `git status` in the agent's cwd; under worktrees that's the worktree (which is what should be clean). Adjust the fixture to point at a worktree if the production path moves. |

Other integration tests seed `agents.cwd` indirectly through `TestDashboard` /
`seed_agent` and inherit the same adjustment; the canonical fixtures above are
the ones to fix first.

---

## F. Runner `state.cwd` and the `validated_cwd` sandbox check

The task brief references "the sandbox check at line 1027 (`validated_cwd`)" —
that line has drifted. The current location is **`bin/branchwork_runner.rs:2642`**
(`fn validated_cwd`).

### F.1 What `state.cwd` is

- `RunnerState.cwd` — `bin/branchwork_runner.rs:814`. Set once at startup from
  `--cwd` (`:997` `std::fs::canonicalize(&cli.cwd)`), default `"."` (`:787`).
  This is the runner's **canonical root** — every project the runner can serve
  lives under it.
- Uses of `state.cwd`:
  - `:1467` — `agent_cwd = state.cwd.clone()` fallback when `StartAgent.cwd` is
    empty.
  - `:2485, :2556` — fallback cwd for CI handlers when `plan_cwd` has no entry.
  - `:3504, :3889, :3964, :3980` — `state.cwd.join(".branchwork-runner-sessions")`
    (socket/log dir; **not** the agent working dir).

### F.2 `validated_cwd(state, requested)` — the sandbox

```
fn validated_cwd(state, requested) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(requested)?;     // also an existence check
    if !canonical.starts_with(&state.cwd) {                // ← the sandbox gate
        return Err("cwd <x> outside runner root <state.cwd>");
    }
    Ok(canonical)
}
```

Callers (all the read/merge/push/gh RPC handlers): `:1811` (start_check_agent),
`:2179` (GetDefaultBranch), `:2205` (ListBranches), `:2234` (MergeBranch),
`:2259` (DiscardBranch), `:2287` (PushBranch), `:2331` (GhRunList), `:2352`
(GhFailureLog).

### F.3 Interaction with paths outside the project root — the key finding

`validated_cwd` rejects any path that does **not** start with `state.cwd`.
Whether worktrees are accepted depends entirely on **where the worktree lives**:

- **Worktree *inside* the project root** (e.g. `<project>/.worktrees/<id>`, and
  `<project>` is under `state.cwd`) → `canonical.starts_with(state.cwd)` holds →
  **passes the sandbox unchanged**. ✅ No `validated_cwd` change needed.
- **Worktree under a sibling/temp dir** (e.g. `/tmp/bw-…/<id>`, the location the
  existing `worktree.rs::TempWorktree` and `phase_check::create_worktree` use) →
  `/tmp/...` does **not** start with `state.cwd` → **`validated_cwd` rejects it
  with `cwd … outside runner root`**, and the RPC degrades (read handlers return
  empty/`None`, merge returns `MergeOutcome::Other { stderr }`). ⚠️ **must-change
  consideration.**

**Implication for Phase 2/3:** if the worktree manager places worktrees outside
the project subtree (the precedent set by the pre-merge gate and phase-check,
both `/tmp`-based), the runner's `validated_cwd` must learn to also accept the
worktree root(s) — e.g. allow a configured worktree base dir, or canonicalise
the worktree path against an allow-list that includes `state.cwd` **and** the
worktree base. Conversely, if the plan decides worktrees live under the project
(`<project>/.worktrees/...`), `validated_cwd` needs no change. **This is a
Phase-0 decision input** (Task 0.1's "where do worktrees live" choice directly
gates whether `validated_cwd` is must-change or no-change).

Note also: `validated_cwd` does `std::fs::canonicalize`, which **fails on a
non-existent path**. A worktree must already exist on disk before any RPC
references it — fine for the normal flow (worktree created at spawn), but the
discard/cleanup ordering matters (don't reference a removed worktree).

---

## Phase 2 change-list (the actionable subset)

Ordered by where the work concentrates. Every item below is **must-change**;
everything classified no-change/test-only above is excluded.

1. **Spawn path writes a worktree, not the project root.**
   - `agents/pty_agent.rs:59-127` (`start_pty_agent`): create a worktree from
     `work_dir`, set `cwd` to it, drop the `git_checkout_branch` call
     (`:100`), capture `base_commit` (`:88`) inside the worktree, INSERT the
     worktree path (`:113`).
   - `agents/spawn_ops.rs:167-279` (`start_agent_via_runner`): same, server-side
     decision of the worktree path before the `StartAgent` wire send + INSERT.
   - `bin/branchwork_runner.rs:1466-1493` + `checkout_task_branch:3258`: runner
     creates the worktree instead of `git checkout`.
   - `api/plans.rs` callers (`:3771, :4022, :4232, :4852`) pass the worktree to
     `start_agent_dispatch` (the project-root `ensure_git_initialized` calls at
     `:3751, :3981, :4213, :4795` stay put).

2. **Merge runs at the project root / merge worktree, not the agent's cwd.**
   - `api/agents.rs:761-979` (`merge_agent_branch_inner`) + `TriggerArgs.cwd`
     (`:963`).
   - `git_helpers.rs:143-213` (`merge_branch_local`, shared by runner via
     `#[path]`) + the runner's `merge_agent_branch_on_runner`
     (`branchwork_runner.rs:2902`).
   - `ci.rs:100` (`trigger_after_merge`) push location. **This is Phase 3's
     core.**

3. **Discard removes the worktree before deleting the branch.**
   - `api/agents.rs:1093-…` (`discard_agent_branch`, standalone checkout+delete
     at `:1210/:1228`).
   - `git_helpers.rs:222` (`discard_branch_local`, shared/runner).

4. **Fix-agent re-spawn (re)creates a worktree.**
   - `auto_mode.rs:1097` (gate-fix) and `auto_mode.rs:2329` (`spawn_fix_agent`):
     the seeded `cwd` may be a dead worktree by re-spawn time.

5. **Boot reconcile reads the shared ref store, not a disposable worktree.**
   - `agents/mod.rs:621` (`reconcile_orphaned_branches`): the `is_dir(cwd)` guard
     + `git show-ref` in `cwd` (`:625-659`).

6. **Runner sandbox accepts the worktree location.**
   - `bin/branchwork_runner.rs:2642` (`validated_cwd`) — **only if** worktrees
     live outside `state.cwd` (gated by Task 0.1's location decision).

7. **Test fixtures** (Section E): `tests/support/mod.rs` (`create_task_branch`),
   `tests/merge_guard.rs` (`seed_agent`), `tests/recovery.rs`,
   `tests/unattended_auto_mode_e2e.rs`, and the `reconcile_orphaned_branches` /
   Stop-hook unit-test repos.
