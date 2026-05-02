# Trace: source_branch capture, merge resolution, CI insert

Phase 1 task 1.1 of `merge-target-canonical-default-branch`. This is the
PR-description source for the four call sites that compose the bug.

## The four call sites

### 1. `start_pty_agent` — captures `source_branch` (server-rs/src/agents/pty_agent.rs:70-80)

```rust
let base_commit = git_head_sha(cwd);
let source_branch = git_current_branch(cwd).filter(|cur| match branch {
    Some(target) => cur != target,
    None => true,
});
```

The agent's `source_branch` comes from `git_current_branch(cwd)` at spawn
time. The only filter is "not equal to the new task branch" (a guard added
to avoid `<task>..<task>` = 0 commits in the merge guard). Whatever branch
the working tree happens to sit on at spawn — including a stale branch
left behind by a previous agent (e.g. `architecture-docs/3.4`) — is
recorded into the DB. Inserted at lines 92-104 along with the rest of the
agent row. There is no notion of "default branch" anywhere in this path.

### 2. `merge_agent_branch` — feeds it back to the resolver (server-rs/src/api/agents.rs:287-337)

```rust
// :302
"SELECT cwd, branch, source_branch, plan_name, task_id FROM agents WHERE id = ?",
// ...
// :337
let target = resolve_merge_target(source_branch.as_deref(), std::path::Path::new(&cwd));
```

Reads the row that `start_pty_agent` wrote and hands `source_branch`
verbatim to `resolve_merge_target`. The endpoint never consults
`origin/HEAD`, the symbolic-ref of the upstream default, or any other
canonical signal. Whatever was captured at spawn is what gets merged into.

### 3. `resolve_merge_target` — stored value beats master/main (server-rs/src/api/agents.rs:18-40)

```rust
fn resolve_merge_target(source_branch: Option<&str>, cwd: &std::path::Path) -> String {
    let resolves = |name: &str| -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            // ...
    };
    if let Some(c) = source_branch
        && resolves(c)
    {
        return c.to_string();              // <-- bug: stale-but-resolvable wins
    }
    if resolves("master") { return "master".to_string(); }
    if resolves("main")   { return "main".to_string();   }
    source_branch.unwrap_or("main").to_string()
}
```

The fallback to `master`/`main` only fires when the stored value fails
`git rev-parse --verify --quiet`. A *resolvable* stale branch passes the
check and short-circuits the function. Branches deleted out-of-band fall
through to master/main; branches that simply happen to still exist locally
do not. This is the root cause: every agent that inherits a stale working
tree position will silently fast-forward its task branch onto that stale
branch instead of the canonical default.

### 4. `trigger_after_merge` — unconditional CI insert (server-rs/src/api/agents.rs:484-495 → server-rs/src/ci.rs:80)

```rust
// agents.rs:484
tokio::spawn(crate::ci::trigger_after_merge(crate::ci::TriggerArgs {
    db: state.db.clone(),
    // ...
    source_branch: target.clone(),  // whatever resolve_merge_target returned
    task_branch: task_branch.clone(),
    merged_sha: sha,
}));

// ci.rs:80 → checks: has_github_actions(&cwd) && has_remote(&cwd, "origin")
// then unconditionally pushes + INSERTs ci_runs (status='pending')
```

The merge endpoint kicks off `trigger_after_merge` for every successful
merge with a known plan/task pair. `trigger_after_merge` gates only on
"workflows directory exists" and "origin remote exists" — never on
whether the merge target is actually a CI-watched branch. Result: when
site #3 has chosen a non-default branch, the push goes to a branch
GitHub Actions doesn't watch, no workflow run materialises, and the
`ci_runs` row sits `pending` for 30 min before the poller ages it out
to `unknown` (`MAX_RUN_AGE_SECS` = 1800, `ci.rs:27`).

## One-paragraph summary (PR description)

`start_pty_agent` (`pty_agent.rs:70-80`) records whatever branch the
working tree is on at spawn into `agents.source_branch`, only filtering
out the task branch itself; this lets a stale branch from a previous
agent leak into the DB. `merge_agent_branch` (`agents.rs:287-337`)
reads that value back and passes it to `resolve_merge_target`
(`agents.rs:18-40`), which prefers the stored value over master/main
whenever `git rev-parse --verify --quiet` succeeds — so a resolvable
stale branch silently wins. `trigger_after_merge` (`agents.rs:484-495`
→ `ci.rs:80`) then pushes and inserts a `ci_runs` row regardless of
whether that target is a CI-watched branch, leaving an orphaned
"pending" row that ages out to "unknown" 30 minutes later.
