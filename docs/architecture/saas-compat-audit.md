# SaaS compatibility audit

**Scope.** Every module under `server-rs/src/` was grepped for the
SaaS-relevant markers (`std::fs`, `tokio::fs`, `dirs::home_dir`,
`Path::exists`, `Command::new`, `tokio::process`, `env::var`) and each
hit was classified against the existing wire protocol in
`server-rs/src/saas/runner_protocol.rs`.

In **standalone** mode the server *is* the user's machine — its
`$HOME`, its `git`, its `gh`, its `~/.claude/.credentials.json`. In
**SaaS** mode the server is a multi-tenant control plane and the
filesystem / CLI tools live on the customer-side **runner** binary. A
hit is _routed_ when there's a `WireMessage` variant the server
dispatches (locally or via the runner) so the same call site works in
both modes; _broken_ when the server still touches its own filesystem
or shells locally even though the data lives on the runner; _partial_
when there's a deliberate compromise (e.g. 503 in SaaS) flagged in
code.

**Status legend:**

- ✅ **routed** — works in both modes.
- ⚠️ **partial** — works but with caveats explained in the row.
- ❌ **broken** — silently fails or behaves wrongly in SaaS today.

**Plan column:** points at the task that addresses the gap (this
plan's task numbers, an existing plan, or a stub plan filed under
`~/.claude/plans/`).

## Findings by category

### 1. Already routed (works in both modes)

Every dispatcher pair below shares the same shape: the call site asks
`saas::dispatch::org_has_runner(db, org_id)` and either calls a local
helper (standalone) or sends a `WireMessage` and awaits the runner's
reply (SaaS). All response-side wire variants are best-effort because
they're tied to a live HTTP caller (or the CI poller, where retry is
cheaper than outbox replay).

| Module:fn / line | What it touches | Wire variant |
|------------------|-----------------|--------------|
| `api/settings.rs:list_folders` (l. 132) | `dirs::home_dir()` + `read_dir` | `ListFolders` / `FoldersListed` |
| `api/plans.rs:create_plan` folder branch (l. 1748–1828) | `~/…` resolution + `create_dir_all` | `CreateFolder` / `FolderCreated` |
| `api/agents.rs:list_merge_targets` (l. 345) | `git symbolic-ref`, `git rev-parse`, `git branch --list` | `GetDefaultBranch` + `ListBranches` (via `git_ops::default_branch` + `list_branches`) |
| `api/agents.rs:merge_agent_branch` (l. 394) | five-step `git merge` sequence | `MergeBranch` / `MergeResult` (via `git_ops::merge_branch`) |
| `ci.rs:trigger_after_merge` push step (l. 147) | `git push origin <branch>` | `PushBranch` / `PushResult` (via `git_ops::push_branch`) |
| `ci.rs:fetch_run` (l. 247) | `gh run list --commit <sha>` | `GhRunList` / `GhRunListed` |
| `ci.rs:fetch_failure_log` (l. 513) | `gh run view <id> --log-failed` | `GhFailureLog` / `GhFailureLogFetched` |
| `api/ci.rs:failure_log` (l. 75) | passes through to `ci::fetch_failure_log` | (inherits routing) |

### 2. Broken but covered by this plan

Every row here is a gap that the auto-mode loop needs to be SaaS-clean
to ship. Existing tasks 0.3 / 0.4 / 0.5 cover the merge / CI / failure-log
routing the loop drives; the StartAgent + KillAgent dispatchers needed
for spawning and cancelling fix agents are added as new tasks 0.8 and
0.9 in this plan.

| Module:fn / line | What it touches | SaaS today | Plan |
|------------------|-----------------|------------|------|
| `ci.rs:has_github_actions` (l. 36) | `read_dir(.github/workflows)` on cwd | trigger_after_merge skips local check in SaaS today; CI gate path needs runner-side detection | task 0.3 / 0.4 (`HasGithubActions` variant) |
| `ci.rs::poll_once` aggregation (l. 286–411) | one `ci_runs` row per merge | per-SHA aggregate + upstream-skip detection missing — Reglyze bug | task 0.3 / 0.4 (`GetCiRunStatus` + `CiAggregate`) |
| `ci.rs:fetch_failure_log` root-cause re-resolve (l. 513) | takes a `run_id`, can't re-resolve after a `Red` | re-shape so `run_id: None` re-finds failing run via runner-side cache | task 0.3 / 0.4 (`CiFailureLog { run_id: Option<String> }`) |
| `agents/pty_agent.rs:start_pty_agent` (l. 52) | `git_head_sha` + `git_default_branch` + `git_checkout_branch` + `supervisor::spawn_session_daemon` LOCALLY | `WireMessage::StartAgent` exists with a runner-side handler in `bin/branchwork_runner.rs:373`, but the server never sends it — every agent spawn happens on the SaaS server's filesystem | **task 0.8 (new)** — server-side StartAgent dispatcher |
| `api/ci.rs:fix_ci` (l. 232–280) | `git checkout master/main`, `git checkout -b <fix> <sha>` on server cwd, then local `start_pty_agent` | full chain runs on the SaaS server's filesystem instead of the runner | **task 0.8 (new)** — fix-CI's spawn path inherits the StartAgent dispatcher |
| `api/agents.rs:kill_agent` (l. 173) → `agents/mod.rs:kill_agent` (l. 620) | local `process_terminate` + `SessionMessage::Kill` over local socket | runner-spawned agents are not in the local registry; kill is silently a no-op | **task 0.9 (new)** — server-side KillAgent dispatcher (auto-mode 3.3 needs it for the cancel-on-toggle-off path) |
| `agents/mod.rs:ensure_git_initialized` (l. 69) | `git init`, `git config`, `git add -A`, `git commit` on cwd | runs against the SaaS server's filesystem; agent never sees a repo | covered indirectly by task 0.8 — once StartAgent routes through the runner, the runner-side handler does the init |

### 3. Broken and NOT covered by this plan (stub plans filed)

These rows are pre-existing SaaS gaps that don't block auto-mode but
*are* visible when a SaaS user tries to use the relevant feature. Each
row has a stub plan under `~/.claude/plans/saas-compat-<area>.yaml`
with a context block describing the gap; execution happens later.

| Module:fn / line | What it touches | SaaS today | Stub plan |
|------------------|-----------------|------------|-----------|
| `agents/terminal_ws.rs:terminal_ws_handler` | subscribes to local `registry.agents[id].output_tx` + writes `SessionMessage::Input/Resize` to local socket | runner-spawned agents have no entry in the local registry — terminal pane shows the historical buffer then "session ended" even while the agent is alive on the runner | `saas-compat-pty-bridge.yaml` |
| `api/agents.rs:get_agent_diff` (l. 230) | `git diff <base>`, `git diff --stat`, `git diff --name-only` in agent's cwd | shells against the SaaS server's filesystem; cwd path doesn't exist there | `saas-compat-agent-diff.yaml` |
| `api/agents.rs:discard_agent_branch` (l. 609) | `git checkout`, `git branch -D` in agent cwd | already returns HTTP 503 `discard_not_supported_for_saas_runners` (intentional placeholder, see `agents.rs:650`) — so it's loud-broken not silent-broken | `saas-compat-discard-branch.yaml` |
| `agents/check_agent.rs:start_check_agent` (l. 42) | spawns `claude` directly in cwd via `Command::new("claude")` | spawn happens on the SaaS server, not the runner; check agent's `Read` tool can't see the project files | `saas-compat-check-agents.yaml` |
| `api/plans.rs:check_task` (l. 1495) / `check_all` (l. 2658) / `check_plan` (l. 1632) | resolve `home.join(project)` then call `check_agent::start_check_agent` | inherits the `check_agent` brokenness above | `saas-compat-check-agents.yaml` |
| `auto_status.rs:find_file_in_project` (l. 22) + `infer_status` (l. 92) | `Path::exists` and `find` against `project_dir` | always reports "0/N files exist" because `project_dir` lives on the runner | `saas-compat-check-agents.yaml` |
| `api/plans.rs:auto_status` handler (l. 747) / `sync_all` (l. 844) | call `auto_status::infer_status` against `home.join(project)` on the server | inherits the brokenness above | `saas-compat-check-agents.yaml` |
| `api/plans.rs:list_stale_branches` (l. 2369) | `git for-each-ref`, `git rev-parse`, `git rev-list` in plan's cwd | shells against SaaS server filesystem — returns empty list | `saas-compat-stale-branches.yaml` |
| `api/plans.rs:purge_stale_branches` (l. 2496) | `git rev-parse`, `git rev-list`, `git branch -D` in plan's cwd | silently no-ops on the SaaS server's local repo | `saas-compat-stale-branches.yaml` |
| `agents/mod.rs:check_tree_clean_for_completion` (l. 742) | `git status --porcelain --untracked-files=no` in plan's cwd | always returns `Unknown` on the SaaS server (cwd doesn't resolve), so the dirty-tree gate that protects `completed` transitions never fires | `saas-compat-tree-clean-completion.yaml` |
| `agents/mod.rs:reconcile_orphaned_branches` (l. 454) | `git show-ref --verify --quiet refs/heads/<branch>` in agent.cwd | runs at server boot — for runner-spawned agents the cwd doesn't exist on the server, the probe fails, and the server clears the `agents.branch` column even though the branch is alive on the runner | `saas-compat-orphan-branch-cleanup.yaml` |
| `agents/driver.rs:ClaudeDriver::auth_status` (l. 307) | reads `ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`, `~/.claude/.credentials.json` on the server | reports the SaaS server's auth, not the runner's; runner sends its own `DriverAuthReport` so the dashboard has the right answer when a runner is connected — but local `auth_status()` is still called by `GET /api/drivers` and gives a misleading row when there's no runner-yet attached | `saas-compat-driver-auth.yaml` |
| `agents/driver.rs:AiderDriver::auth_status` / `CodexDriver::auth_status` / `GeminiDriver::auth_status` (l. 459, 542, 603) | reads `OPENAI_API_KEY` / `GEMINI_API_KEY` / `GOOGLE_API_KEY` / `DEEPSEEK_API_KEY` on the server | same as above | `saas-compat-driver-auth.yaml` |
| `agents/driver.rs:binary_on_path` (l. 200) | reads server `$PATH` to decide `NotInstalled` | wrong host — the runner is the one that needs the CLI, not the server | `saas-compat-driver-auth.yaml` |
| `plan_parser.rs:get_project_dirs` (l. 142) + `infer_project` (l. 164) | scans server `$HOME` for sibling project dirs | project suggestions on the New Plan form are wrong in SaaS — they list the server's directories, not the runner's | `saas-compat-project-resolution.yaml` |
| `ci.rs:project_dir_for` (l. 483) / `resolve_project_dirs` (l. 611) | `home.join(project)` on the server | per-plan cwd resolution returns SaaS-server paths; everything that consumes the cwd (CI poller, fix-CI, check agents) inherits this | `saas-compat-project-resolution.yaml` |
| `agents/mod.rs:build_cross_plan_context` (l. 791) | reads `home.join(project)` and probes sibling plans for predecessor task notes | predecessor context is built against a non-existent path; cross-plan notes don't surface in SaaS | `saas-compat-project-resolution.yaml` |

## New tasks added to this plan

The audit surfaced two auto-mode-relevant gaps not covered by the
existing tasks 0.3 / 0.4 / 0.5. Both are dispatchers for wire variants
that already exist in `runner_protocol.rs` but have no server-side
sender today.

- **0.8 — server-side StartAgent dispatcher.** Refactor
  `pty_agent::start_pty_agent` (and its callers in `api/plans.rs`,
  `api/ci.rs::fix_ci`, and `auto_mode::spawn_fix_agent`) so the
  spawn path branches on `org_has_runner`: standalone keeps the
  current local supervisor + git-init flow; SaaS sends
  `WireMessage::StartAgent { agent_id, plan_name, task_id, prompt,
  cwd, driver, effort, max_budget_usd }` to the runner and lets the
  runner-side handler in `bin/branchwork_runner.rs:373` (already
  implemented) do the spawn. Auto-mode 1.1 + 3.1 + the existing
  fix-CI handler all flow through this dispatcher, so once it
  lands every fix-agent spawn works in both modes without further
  per-call-site work. Acceptance: SaaS integration test that spawns
  a fix agent via `auto_mode::spawn_fix_agent` and asserts a
  `StartAgent` envelope reaches the stub runner.

- **0.9 — server-side KillAgent dispatcher.** Refactor
  `agents::AgentRegistry::kill_agent` (and `api/agents.rs::kill_agent`)
  so the kill path branches on `org_has_runner`: standalone keeps
  the local SIGTERM + `SessionMessage::Kill` flow; SaaS sends
  `WireMessage::KillAgent { agent_id }` to the runner and lets the
  runner-side handler in `bin/branchwork_runner.rs:431` (already
  implemented) tear down the session daemon. Auto-mode 3.3's
  cancel-on-toggle-off path requires this. Acceptance: SaaS
  integration test that spawns a fix agent then toggles auto-mode
  off mid-flight and asserts a `KillAgent` envelope reaches the
  stub runner. (`api/agents.rs::finish_agent` and
  `agents::AgentRegistry::graceful_exit` need the same treatment for
  feature parity but are out of scope for auto-mode — they're
  user-driven, not loop-driven.)

Both tasks slot into the existing dependency graph as siblings of 0.5
(dispatch shims), and 1.1 / 3.1 / 3.3 should add `["0.8", "0.9"]` to
their `dependencies` once the new tasks land.

## SaaS-safe modules (no further work needed)

The following touch the filesystem or shell out but are correctly
server-side in both modes — they don't need routing because the data
they touch genuinely lives on the SaaS server (plan files, server
config, audit log, supervisor sockets for agents that ARE local).

| Module | Why it's SaaS-safe |
|--------|--------------------|
| `db.rs:init` (l. 43) | `~/.claude/branchwork.db` is the server-owned SQLite store in both modes |
| `persisted_settings.rs` (read/write `branchwork-settings.json`) | server config sidecar — meaningful per-deployment, not per-runner |
| `file_watcher.rs:start` | watches `~/.claude/plans/` on the server; plans always live on the server |
| `plan_parser.rs:list_plans` / `parse_plan_file` / `find_plan_file` | reads plan YAML/markdown from `state.plans_dir` (server-owned) |
| `api/plans.rs:update_plan` / `convert_plan` / `convert_all` | writes plan files in `state.plans_dir` (server-owned) |
| `agents/supervisor.rs:run_session` + `agents/session_protocol.rs` | local IPC abstraction; in SaaS the SaaS server never invokes the supervisor for runner-spawned agents (the runner has its own copy of the supervisor for its local-spawned agents) |
| `agents/pty_agent.rs:reattach_agent` (l. 511 cleanup) | reads/writes `<sockets_dir>/<id>.{sock,pid}` — only triggered for local-spawned agents that survived a server restart |
| `main.rs:cleanup_and_reattach` PTY branch | same — only acts on agents whose supervisor lives next to the server process |
| `saas/billing.rs:SmtpConfig::from_env` (l. 377–386) | reads `SMTP_HOST` / `SMTP_PORT` / `SMTP_FROM` / `SMTP_USERNAME` / `SMTP_PASSWORD` on the server — SaaS-correct, the server is the one that sends emails |
| `audit.rs` | DB-only |
| `notifications.rs:notify` | HTTP POST to webhook — agnostic |
| `hooks.rs:receive_hook` | DB-only on the server side; the hook sender (Claude Code) lives wherever the agent runs |
| `mcp/` (transport, tools/plans, tools/status) | DB + plan file reads only, all of which are server-owned |
| `auth/` (mod, sessions, orgs, sso) | DB-only |
| `static_files.rs` / `templates/` | embedded assets / static templates |
| `bin/branchwork_runner.rs` | the runner binary — runs on the customer's machine, every fs/process touch is correct by definition |
| `bin/session_daemon.rs` | thin wrapper that includes `agents/supervisor.rs` and `agents/session_protocol.rs` — same supervisor used by the runner |
