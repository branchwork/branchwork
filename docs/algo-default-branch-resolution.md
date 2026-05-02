# Algorithm: canonical default-branch resolution

Phase 1 task 1.2 of `merge-target-canonical-default-branch`. PR-description
source for the helper introduced in Phase 2.1.

## Helper signature (locked)

```rust
/// Resolve the canonical default branch for the repo at `cwd`.
/// Tries `origin/HEAD` first, then falls back to local `master` / `main`.
/// Returns `None` if nothing resolves. Local-only — never fetches.
pub fn git_default_branch(cwd: &std::path::Path) -> Option<String>
```

Style matches the existing `git_head_sha` / `git_current_branch` helpers in
`server-rs/src/agents/mod.rs:123-154`: every probe uses
`Command::output()`, gates on `status.success()`, trims stdout, and returns
`Option<String>`. No `git fetch`, no `--all`, no network.

## Algorithm

```text
1. probe `origin/HEAD`
   git symbolic-ref --short refs/remotes/origin/HEAD
   on success: trim stdout → strip leading "origin/" → return Some(name)
   on failure: continue

2. probe local `master`, then local `main`
   for name in ["master", "main"]:
     git rev-parse --verify --quiet <name>
     on success: return Some(name)
     on failure: continue

3. fallback
   return None
```

### Step 1 — `origin/HEAD`

Authoritative when the upstream remote has a `set-head` record. Typical
shape of `git symbolic-ref --short refs/remotes/origin/HEAD` on a freshly
cloned repo:

```
$ git symbolic-ref --short refs/remotes/origin/HEAD
origin/main
```

The leading `origin/` prefix must be stripped — that's the remote-tracking
namespace, not a branch name the local resolver understands.

`git symbolic-ref` exits **128** (with `fatal: ref … is not a symbolic ref`
on stderr) when the symref is absent, **not** 1. The implementation must
gate on `status.success()` rather than matching specific exit codes.

`set-head` is populated automatically by `git clone` and refreshed by
`git remote set-head origin --auto`; it can also be set explicitly. It is
**not** present on:
- repos created with `git init` (no remote at all)
- repos with an `origin` remote that has never been fetched
- repos where the user removed the symref deliberately

All three cases fall through to step 2 — exactly the situation the
existing `["master", "main"]` probes handle today.

### Step 2 — local `master`, then `main`

`git rev-parse --verify --quiet <name>` is the same probe used in:
- `server-rs/src/api/plans.rs:2336-2347` (stale-branch trunk detection)
- `server-rs/src/api/agents.rs:32-37` (`resolve_merge_target` fallback)

Order: `master` first, then `main`. This matches the precedence in both
existing call sites — projects that initialised before late-2020 default
to `master`, newer ones to `main`. Probing `master` first keeps existing
behaviour stable for repos that have both refs.

The `--quiet` flag suppresses the "fatal: Needed a single revision"
output that `git rev-parse` writes on failure. The probe also returns
failure (exit 1) on a freshly `git init -b master`'d repo with no commits
yet — `master` is the symbolic HEAD but no ref exists until the first
commit. That's by design: a repo with no commits has no canonical default,
so step 3 returns `None`.

### Step 3 — `None`

Reached when:
- no `origin/HEAD` symref is set, AND
- neither `master` nor `main` resolves (typically: empty repo, or a
  project using a non-conventional trunk like `trunk` / `develop` with
  no remote `set-head`).

Caller is responsible for the fallback. Phase 2.3's pure
`resolve_merge_target` translates `None` → `"main"` (today's behaviour
when nothing resolves), but only as the very last branch of the priority
chain — `explicit_into > default_branch > "main"`.

## Why this consolidation

Two probe sites today, neither aware of `origin/HEAD`:

| Site | Probe | Used for |
|------|-------|----------|
| `api/plans.rs:2336-2347` | `["master", "main"]` via `rev-parse --verify` | choose a "trunk" for the stale-branch sweep |
| `api/agents.rs:32-37` | `["master", "main"]` via `rev-parse --verify --quiet` | fallback inside `resolve_merge_target` when the stored `source_branch` doesn't resolve |

Both fail-open to "main" if neither ref exists, and both miss the case
where the repo's actual default is something else (per `origin/HEAD`).
The new helper is a strict superset: every input that resolved before
still resolves, plus repos with a non-trunk default get the right answer.

After Phase 2 lands, both call sites switch to `git_default_branch`. The
old `["master", "main"]` literals disappear from the resolution paths.

## SaaS hook

In the 3-binary deployment (Phase 5) the server has no access to the
customer's repo, so this helper's *call site* moves to the runner via a
`GetDefaultBranch` / `DefaultBranchResolved` round-trip
(`saas/runner_protocol.rs`, Phase 5.1). The algorithm is unchanged —
the runner copies the helper verbatim and invokes it on its own working
tree. The pure `resolve_merge_target` (Phase 2.3) takes the resolved
default as a parameter, so it doesn't care which side ran the probe.

This is why the helper signature is `(&Path) -> Option<String>` and not
`async fn (...)`: the local helper stays sync, and the SaaS dispatcher
wraps it in an async wrapper (`agents::git_ops::default_branch`,
Phase 5.6) that picks local vs runner-RPC based on whether the caller's
org has a registered runner.

## Test-harness implication

`server-rs/tests/support/mod.rs` builds scratch repos with
`git init -b master` and commits straight onto `master` (no remote, no
clone). For the unit tests in Phase 2.1:

- `git init -b master` + one commit → step 1 fails (no remote), step 2
  succeeds on the first probe → `Some("master")`.
- `git init -b main` + one commit → step 2 succeeds on the second probe
  → `Some("main")`.
- `git init -b master` with no commits → step 2 fails on both probes
  (the branch is the symbolic HEAD but no ref exists yet) → `None`.
- For the `set-head` case, the test explicitly creates the symref:
  `git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/<name>`
  after seeding a fake `origin` remote. No clone or push needed.

No network in any case — the algorithm only reads local refs.
