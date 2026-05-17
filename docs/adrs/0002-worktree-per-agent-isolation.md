# ADR 0002 — Worktree-per-agent isolation + at-merge conflict resolver

- **Status:** Accepted (2026-05-17)
- **Authors:** cpo
- **Decision driver(s):** parallel agents corrupting each other's working tree; merge-time conflicts going unhandled

## Context

Branchwork starts every agent in the **same** project working directory. `agents::mod::git_checkout_branch` (`server-rs/src/agents/mod.rs:169`) runs `git checkout <branch>` in `cwd` — and `cwd` is the project root, shared across every concurrently-running agent.

Git can only have one `HEAD` per working tree. So two agents on different branches in the same `cwd` collide deterministically:

- Agent A is on branch X with uncommitted edits in the working tree.
- Agent B starts on branch Y and runs `git checkout Y`. Either git refuses (dirty tree), or it succeeds and clobbers A's working tree, or it stashes A's edits behind A's back into a stash A doesn't know about.
- A's next file read returns Y-state files. A's next write lands on Y. The merge of A's branch contains Y-shaped commits. Compromised silently.

The user-visible symptoms are exactly what the field reports describe: stashed work disappearing, files reverting between writes, two agents producing nonsense merges, "telescoping" of edits across branches.

The cause is **not** insufficient agent coordination — it's that no amount of cooperation between agents can let them share a working tree. The OS + git already solved this with `git worktree`: independent on-disk checkouts of the same repo, sharing the `.git/` object database.

## Decision

**Each agent runs in its own git worktree.** The worktree is the agent's `cwd`. Branchwork creates the worktree on agent start, runs the agent against it, and removes the worktree on agent completion (or after merge resolution, on conflict).

### Worktree location

Worktrees live **outside** the project, under `~/.branchwork/worktrees/<project-slug>/<plan>/<task>-<agent-id>/`. Reasons:

- No `.gitignore` footgun — branchwork's per-agent state never accidentally lands inside a project commit.
- IDE / editor projects opened against the project root are not polluted with agent-state directories.
- Cleanup on uninstall is one `rm -rf ~/.branchwork/worktrees/`.
- Per-project subdirectory keeps the structure greppable.

### Conflict resolution at merge time

With worktrees, conflicts no longer happen during work — they surface at merge. Two agents that touched the same file in their respective branches will hit a real `git merge` conflict when branchwork merges them into the trunk.

When that happens, branchwork **auto-spawns a "resolve-conflict" sub-agent** (mirroring the existing fix-CI flow), seeded with:
- The conflict markers from `git status` / per-file `<<<<<<<` blocks,
- Both diffs (the original agent's change, the trunk-side change it conflicts with),
- The original task's description as context.

If the resolver agent succeeds (clean tree, all conflicts resolved, tests still green), the merge proceeds. If it **fails** — exits unclean, hits the fix-attempt cap, or produces a non-resolving diff — branchwork **falls back to pausing the plan and notifying the human**, identical to the existing "merge_conflict" pause path.

This is the (c) behaviour: try once automatically, fall back to human on failure.

### Build cache sharing

Naïve worktree-per-agent multiplies disk usage by the number of concurrent agents. Mitigation per language:

- **Rust:** set `CARGO_TARGET_DIR=~/.branchwork/cache/<project-slug>/cargo-target` in every agent's environment. Cargo's own file lock serializes concurrent compiles against the shared target dir, which is the correct behaviour anyway (running `cargo build` in two worktrees at once would otherwise compile the same crates twice).
- **pnpm/npm:** pnpm's content-addressable store at `~/.local/share/pnpm/store` already handles this. Running `pnpm install` per worktree is cheap (symlinks). For workspaces with hundreds of packages, this is the only viable approach.
- **Other (Go, Python, etc.):** their respective module/wheel caches handle it; document per-tool as needed.

Per-language cache config is part of branchwork's project setup, not hardcoded. New projects pick it up via a `branchwork.toml` (or similar) override.

### What stays in scope

Worktree creation, agent cwd, branch checkout/creation, merge, cleanup, conflict-resolver spawn, fallback-to-human, cache directory configuration.

### What is explicitly NOT in scope (separate concerns)

- **Dev DB and port-bound services.** Two agents both running migrations against `localhost:5432/myapp_dev` still collide; same for two `next dev` processes both binding port 3000. Solving this requires per-agent ephemeral DB schemas, port offset, or a "max concurrent agents per project" cap. None of those belong to the worktree change.
- **Cross-project agents.** Each project still has one trunk; agents within a project use this isolation. Coordinating across projects (when an agent in project A needs to edit project B) is unrelated.
- **Filesystem locks within the same worktree.** A single agent inside one worktree using multiple drivers/threads still has whatever locking the driver provides. Not changed.

## Consequences

### Positive

- **Concurrent agents physically cannot corrupt each other.** Different `cwd`, different working tree, different `HEAD`. Git enforces it.
- **No per-tool cooperation needed.** Every existing driver (claude / aider / codex / gemini) sees a coherent worktree exactly as if it were the only agent. Zero driver changes.
- **Conflicts surface at the canonical integration point** (merge), not as silent data corruption mid-work. Standard git tooling applies.
- **Easier to debug.** "Where is agent X's work?" becomes a single directory path. Inspect, diff, rerun.
- **Branchwork SaaS gets cleaner.** Per-customer per-agent worktrees in the runner host's filesystem; the SaaS server doesn't need to know.

### Negative

- **Disk usage.** N concurrent agents → N worktrees, each holding a checkout of the source. Mitigated by build-cache sharing (above) and the fact that working-tree files are usually a tiny fraction of total project size compared to `target/`, `node_modules/`, `.next/`, etc. — those are *not* in the worktree.
- **Worktree leak on crash.** If branchwork-server crashes mid-task, the worktree stays on disk. Mitigation: a startup sweep that lists `git worktree list`, cross-references against active agent rows, and removes orphans (or at least surfaces them). Implementation detail in the plan.
- **Merge gets harder operationally.** Today's merges happen in the project root with the agent's branch already checked out. Worktree-based merges happen against the trunk worktree (or a transient one created just for the merge). Code path in `agents/git_ops.rs` needs adjustment.
- **Resolver agent can produce nonsense.** Conflict resolution by an LLM is best-effort; for non-trivial conflicts (semantic ones, not just textual overlap) it may produce a "looks-clean but wrong" resolution. The fallback-to-human path is critical, and the resolver's output should always be a *commit*, never a `git merge --continue` from a stash, so a human can review.

### Migration

- New agents started after this lands use worktrees from day one.
- In-flight agents at deploy time still live in the shared cwd; they finish out under the old model. A startup check refuses to start *new* concurrent agents in the same cwd if any pre-migration agent is still running there.
- The existing `agents.cwd` column is already populated; no schema change.
- A short rollback path: a feature flag `BRANCHWORK_USE_WORKTREES=0` falls back to the old shared-cwd behaviour for one minor version, with a startup warning that parallel agents are unsafe in that mode.

## Failure modes (explicit)

1. **Concurrent compile against shared `CARGO_TARGET_DIR`.** Cargo's per-target file lock makes this safe but serializes builds across agents. That's correct — two agents building the same crate simultaneously is wasteful regardless. Document the latency expectation.
2. **Worktree path collision** (rare). Branchwork generates `<task>-<agent-id>` paths; agent IDs are UUIDs, collision probability is zero in practice. But: don't use derived names that could collide on retry of the same task — always include the agent_id.
3. **Worktree on a different filesystem than the project.** `git worktree add` works across filesystems but operations are slower (no hardlinks for blob refs in some configs). Branchwork should verify the worktree base dir is on the same filesystem as the project's `.git`, and warn (not fail) if not.
4. **Resolver agent infinite loop on the same conflict.** The fix-attempt cap from auto-mode applies here too (default 3). Past the cap → fall back to human, mark the plan paused with reason `merge_conflict`.
5. **`git worktree remove` failures.** A removed worktree dir with an in-flight file write can fail. Cleanup uses `--force` and falls back to a manual `rm -rf` of the path + `git worktree prune` to remove the dangling reference. Best-effort; surfaces a warning if it fails.

## Rejected alternatives

### Cooperative locking via agent IPC (the "discuss with each other" option)

Have agents send/receive lock-request messages before editing files; pause if another agent holds the lock. Rejected because:

- Every existing driver writes files via its own primitives (Claude Code's `Edit`/`Write`, Aider's edit blocks, Codex's apply-patch, etc.). None of them know about a lock service. Adding cooperation requires intercepting every file write — a deep, brittle, per-driver instrumentation.
- "Pause until peer finishes" is unbounded; could be hours. Bad UX, hard to model in the dashboard.
- Even if implemented, it doesn't solve the underlying problem (shared HEAD): two agents on different branches in the same cwd is *still* impossible regardless of file locks. You'd still need worktrees underneath.

### File-level mutex at task start (the "second agent blocks" option)

When task B starts, look at its declared `file_paths` against currently-open task A's; if overlap, queue B until A finishes. Rejected because:

- `file_paths` in plans is a hint, not a contract. Agents wander outside what's declared. False negatives lead to corruption (the bug we're trying to solve).
- False positives serialize tasks that could safely run in parallel (touching different sections of a shared file). Defeats the parallelism the user designed for.
- Requires plan authors to keep `file_paths` accurate as code evolves — a maintenance burden for marginal benefit.

### Bare-clone-per-agent (separate `.git/` per worktree)

Like worktrees but each agent gets a full `git clone` instead of a worktree. Rejected because:

- ~Doubles network I/O on agent start (full clone vs. worktree add, which is local).
- Loses the shared object database — each clone holds its own copy of every blob. Disk usage explodes for repos with large histories.
- Worktrees give 95% of the isolation benefit at 5% of the cost.

## Implementation pointer

The implementation plan that derives from this ADR lives at `~/.claude/plans/worktree-per-agent-isolation.yaml`. Phase 0 of that plan covers decisions, audit, and fixtures (including this status flip); Phases 1–4 cover the worktree manager, agent-spawn integration, merge-time conflict resolver, and rollout.

## References

- ADR 0001 (GitHub App auth) — separate trust-boundary work; complementary, not dependent.
- `git-worktree(1)` — official semantics: <https://git-scm.com/docs/git-worktree>
- Existing single-cwd code path: `server-rs/src/agents/mod.rs:169` (`git_checkout_branch`), `server-rs/src/agents/pty_agent.rs:83` (caller).
- Existing fix-CI flow (template for the resolver agent): `server-rs/src/ci.rs::fetch_failure_log` + the auto-mode plan's Phase 3 design (`~/.claude/plans/auto-mode-merge-ci-fix-loop.yaml`, tasks 3.1–3.3).
