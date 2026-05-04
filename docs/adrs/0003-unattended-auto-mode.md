# ADR 0003 — Unattended auto-mode end-to-end

- **Status:** Proposed (2026-05-04)
- **Authors:** cpo
- **Decision driver(s):** auto-mode that still requires a human Finish click; intra-phase task chains stalling after the first task completes

## Context

Branchwork's "auto-mode" promises hands-off execution of a plan from the first task to the last. Two concrete gaps break that promise today:

### Gap 1 — PTY doesn't auto-exit

The merge-on-completion hook lives at `server-rs/src/agents/pty_agent.rs:584`:

```rust
if marked
    && !supervisor_crashed
    && let Some((Some(plan), Some(task), _)) = meta.as_ref()
    && let Some(state) = registry.app_state.get()
{
    crate::auto_mode::on_task_agent_completed(state, agent_id, plan, task).await;
}
```

This branch only runs when the PTY has actually closed. But Claude Code (and every other CLI driver we ship) finishes its turn and sits at the prompt waiting for the next instruction — the PTY never closes on its own. Today the PTY closes only when:

- The human clicks **Finish** in the dashboard, which calls `AgentRegistry::graceful_exit` (`server-rs/src/agents/mod.rs:605`) and sends the driver's exit sequence (`/exit\r` for Claude); or
- The human clicks **Kill**, which goes through `kill_agent` and SIGKILLs the supervisor.

So today's "auto-mode" is really *auto-merge-after-you-click-Finish-mode*. Without a human at the keyboard to click Finish, the merge-on-completion path at `pty_agent.rs:584` never fires and the plan never advances.

### Gap 2 — `try_auto_advance` only advances across phase boundaries

The early-return at `server-rs/src/agents/mod.rs:1089-1138`:

```rust
if !auto_advance_enabled(&registry.db, &plan_name)
    && !crate::db::auto_mode_enabled(&registry.db, &plan_name)
{
    return;
}
// … snapshot status_map …
let phase_done = current_phase.tasks.iter().all(|t| {
    matches!(
        status_map.get(&t.number).map(String::as_str),
        Some("completed") | Some("skipped")
    )
});
if !phase_done {
    return;
}
```

When a task within a phase finishes, this returns early unless the *entire phase* is done. Sequential intra-phase chains (5.1 → 5.2 → 5.3 …) stall after the first task even when CI is green and the next task's deps are fully satisfied.

Confirmed live on plan `saas-folder-listing-via-runner` task **5.1** on 2026-05-04: 5.1 merged, CI green, 5.2's only dependency was 5.1 — and 5.2 never spawned. The repro is the canonical motivating case for this ADR; the same plan's task 5.1 is the dependency closure being unblocked here.

### What we're solving

Both gaps must close for unattended operation, and they have to close in a way that:

- Doesn't lose work — if the agent finishes its turn with uncommitted tracked changes, we **must** pause the plan rather than auto-merging an incomplete branch.
- Doesn't touch the user's global `~/.claude/settings.json` — a side-effect on a global config file is not acceptable for a per-session feature.
- Distinguishes a system-driven Finish from a user-driven Finish in the audit log.
- Stays driver-agnostic: Claude is the first driver to grow Stop-hook support, but the design must accommodate Aider/Codex/Gemini as those drivers gain (or don't gain) equivalent hooks.
- Preserves manual semantics: a human clicking Finish under auto-mode still works exactly as today.

## Decision

Two coordinated changes in the runtime, plus a small frontend piece for observability:

### 1. Per-session Stop hook injected at spawn

**Per-session settings file (locked in by Task 0.2):**

| | Value |
| --- | --- |
| Path template | `<sockets_dir>/<agent_id>.settings.json` — concretely `~/.claude/sessions/<agent_id>.settings.json` for the default `claude_dir`. |
| CLI flag | `--settings <file-or-json>` — verified against `claude --help` 2026-05-04: *"Path to a settings JSON file or a JSON string to load additional settings from"*. |
| Naming source | `agent_id` (Branchwork's per-spawn UUID), matching the existing sibling files `<agent_id>.sock` / `<agent_id>.mcp.json` / `<agent_id>.log` / `<agent_id>.pid` written into the same directory by `AgentRegistry::socket_for` / `mcp_config_for` (`server-rs/src/agents/mod.rs:308-319`). |
| Cleanup contract | `pty_agent::on_agent_exit` (currently `server-rs/src/agents/pty_agent.rs:529-531`) gains one more `let _ = std::fs::remove_file(...)` line for the settings path, alongside the existing socket + pidfile removals. Same best-effort semantics: silently ignore `NotFound` and IO errors so cleanup can never fail an exit. |
| Crash leak sweep | `AgentRegistry::cleanup_and_reattach` (`server-rs/src/agents/mod.rs:335`) sweeps stale settings files for orphaned agent rows on startup, same code path that already handles stale sockets. |

**Hook URL contract (locked in by Task 0.3):**

| | Value |
| --- | --- |
| URL template | `http://localhost:<port>/hooks` |
| Port source | `AgentRegistry.port` (`server-rs/src/agents/mod.rs:245`) — set at startup from the same `--port` flag the dashboard binds to and threaded through to drivers at spawn time (`server-rs/src/agents/pty_agent.rs:116`). |
| Why this wire path | The Branchwork driver already builds `http://127.0.0.1:<port>/mcp` from this exact field via `AgentDriver::mcp_config_json(port)` (`server-rs/src/agents/driver.rs:280`). The Stop-hook URL reuses the same `<port>` so anything that reaches the dashboard's MCP endpoint already reaches `/hooks` — no new reachability assumption beyond what MCP injection already requires. |
| Scope | **Standalone only.** SaaS reachability across runner ↔ server is explicitly out of scope — see Consequences §*Scope: standalone only*. |

Branchwork writes the settings file at agent-spawn time containing a `Stop` hook that POSTs to `http://localhost:<port>/hooks` (the existing receiver at `server-rs/src/hooks.rs:18`, mounted at `POST /hooks` on the dashboard's HTTP listener). The Claude PTY is launched with `--settings <path>`. Branchwork deliberately does **not** pass `--setting-sources` — that flag would replace the layered `user,project,local` sources, but we want our settings to be purely additive on top of the user's global config (the user's `~/.claude/settings.json` keeps loading from the `user` source).

The path lives under the same `claude_dir.join("sessions")` directory used for sockets / MCP configs / PTY logs (`server-rs/src/main.rs:74`). That directory is Branchwork-owned in practice — no Claude-internal feature writes there, so the `<agent_id>.settings.json` namespace is collision-free.

The Stop handler in `hooks.rs::receive_hook` learns one new branch:

- If `hook_event_name == "Stop"` and the session maps to an agent on a plan with `auto_mode_enabled`, **and** the worktree has no uncommitted tracked changes, call `AgentRegistry::graceful_exit(agent_id)`. That fires the existing merge-on-completion path at `pty_agent.rs:584` exactly as if the human had clicked Finish — except the audit log records the system-level constant `audit::actions::AGENT_AUTO_FINISH` with `{"trigger": "stop_hook"}`.
- If the tree is dirty, the plan **pauses** with reason `agent_left_uncommitted_work` and broadcasts; the agent stays at the prompt so a human can decide what to do.

The Stop handler is idempotent + debounced on `agents.status == 'running'` so a duplicate Stop event (Claude's hook system fires it multiple times in some flows) doesn't double-trigger or race against an already-exiting agent.

### 2. Tree-committable gate before `graceful_exit`

The auto-finish path **always** checks the worktree before exiting. Even with the per-session Stop hook firing reliably, the gate is what guarantees we never silently auto-merge a half-finished branch. The check uses `git status --porcelain` against the agent's `cwd` and rejects on any tracked-modified, tracked-deleted, or unmerged path. Untracked files are tolerated — those are the agent's scratchpad, not its deliverable.

### 3. Intra-phase `try_auto_advance` rework

Refactor `agents::mod::try_auto_advance` (currently `server-rs/src/agents/mod.rs:1089-1138`) so the logic is:

1. Gate on auto-advance OR auto-mode (unchanged).
2. **First**, scan the *current* phase for newly-ready tasks (status `pending` or `failed`, all dependencies in the done-set). If any are ready, spawn them and broadcast a new `task_advanced` WS event. Done.
3. Only if the current phase is now fully done — fall through to the existing next-phase scan, which keeps broadcasting `phase_advanced` exactly as today.

Eligibility is the same `(status pending|failed) && deps in done_set` predicate already used for the cross-phase scan. The shared helper is extracted as `spawn_ready_tasks(phase, done_set, …)` so both call sites use one source of truth.

#### WS event shapes (locked in by Task 0.4)

Two distinct events with two distinct payload shapes — the UI must be able to tell "advancing within phase" apart from "phase boundary crossed" without inspecting plan state.

**`phase_advanced`** — kept verbatim from today (`server-rs/src/agents/mod.rs:1251-1259`). Meaning: a phase is fully done and the auto-advance scan has just spawned the first batch of tasks in the next phase.

```json
{
  "plan_name": "saas-folder-listing-via-runner",
  "from_phase": 4,
  "to_phase": 5
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `plan_name` | string | Plan slug. Field name preserved for backward compatibility with the existing pill UI and any external listeners. |
| `from_phase` | integer | The phase that just finished. |
| `to_phase` | integer | The phase whose tasks were just spawned. |

**`task_advanced`** — new event introduced by Phase 3. Meaning: a task within a phase has finished and the auto-advance scan has just spawned one or more newly-ready tasks **within the same phase** (no phase boundary crossed).

```json
{
  "plan": "saas-folder-listing-via-runner",
  "from_task": "5.1",
  "to_tasks": ["5.2"]
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `plan` | string | Plan slug. Field name follows the `auto_mode.rs` broadcast convention (`auto_mode_merged`, `auto_mode_paused`, `auto_mode_state`, …) — `task_advanced` is wired alongside that loop, so the field name aligns there rather than mirroring `phase_advanced`'s legacy `plan_name`. |
| `from_task` | string | The task that just completed and triggered this scan (e.g. `"5.1"`). Always set — `task_advanced` is only emitted in response to a completion. |
| `to_tasks` | array of strings | Task numbers of every task that became ready *and* was successfully spawned in this scan (i.e. that won the `claim_task` race). Always non-empty — if the scan finds no ready tasks the event is **not** broadcast. |

The two events are deliberately non-overlapping:

- A single auto-advance scan emits **either** `task_advanced` (intra-phase) **or** `phase_advanced` (cross-phase), never both.
- `phase_advanced` keeps its current `plan_name`/`from_phase`/`to_phase` shape so existing dashboards and the phase-pill UI (`web/src/components/PlanBoard.tsx`) keep working without changes.
- `task_advanced` carries no phase numbers — within-phase advances don't change the phase pill, so the pill UI ignores the event. The plan board re-renders task pills off `task_status_changed` (already broadcast at `agents/mod.rs:1207-1215`); `task_advanced` is the *trigger* the frontend uses to refetch plan-level config (e.g. paused-reason, fix-attempt counters) without polling.

Phase 3 broadcasts both events from the rework site. Phase 6 (Frontend / UX surfacing — see §5 below) handles `task_advanced` on the dashboard side.

### 4. Per-driver fallback to idle timer (off by default)

The Stop hook is Claude-specific. For other drivers a new trait method:

```rust
trait Driver {
    fn stop_hook_config(&self, session_id: &str, hook_url: &str) -> Option<SessionSettings>;
}
```

returns `Some(...)` for Claude and `None` for Aider/Codex/Gemini. When `None` is returned, branchwork falls back to a tokio idle-poller (`now() - last_activity_at > IDLE_THRESHOLD`) that triggers the same auto-finish path. The fallback is gated behind an env var (`BRANCHWORK_AUTO_FINISH_IDLE=1`, threshold `BRANCHWORK_AUTO_FINISH_IDLE_SECS` defaulting to 300s) and disabled by default — driver-specific instrumentation is the right long-term fix; the timer is a stopgap that only opt-in users see. Both vars are documented in [docs/reference/configuration.md](../reference/configuration.md#auto-mode-idle-finish).

### 5. UX surfacing

Phase 6 (Frontend / UX surfacing) wires these:

- Frontend handles the new `task_advanced` event (shape pinned in §3 above) by refetching plan-level config — no banner, no pill change, since per-task pills already animate off `task_status_changed`.
- Frontend handles a new `auto_finish_triggered` broadcast event with a small pill on the agent row (`auto-finished`) distinct from the manual `finished` pill.
- Audit log UI renders a trigger badge (`stop_hook` / `idle_timeout`) on `agent.auto_finish` rows.
- Plan-detail banner shows `Paused: agent left uncommitted work` for plans paused by the tree gate, with a one-click "Inspect" link to the agent's worktree.

This ADR moves to **Accepted** once Phases 1–3 of the implementation plan are merged (tracked by task `6.4`).

## Consequences

### Positive

- **Truly hands-off operation.** The user sets `auto_mode_enabled=1` on a plan and walks away. Each task spawn → finish → merge → next-task spawn happens without input.
- **No global-config pollution.** The per-session settings file is scoped to one agent spawn and removed on exit. The user's `~/.claude/settings.json` is read-only from branchwork's perspective.
- **Driver-agnostic by construction.** Adding the Stop hook is one trait method on `Driver`. Drivers that can't return one degrade to the idle-poller fallback (or stay manual if the operator hasn't opted in).
- **Phase-boundary semantics preserved.** Existing dashboards and scripts that listen for `phase_advanced` see no change; intra-phase chains get a new `task_advanced` event they can ignore or surface.
- **Auto-mode and auto-advance opt-ins remain independent.** Either one is enough to drive the new intra-phase advance — the rework respects both gates.

### Negative

- **One new per-session file written per spawn.** Cleanup happens on agent exit, but a server crash mid-task leaks the file. Mitigation: a startup sweep that removes session files for agent rows whose `status` is no longer `running`.
- **Tighter coupling between auto-mode and auto-advance internals.** The Stop handler reads `auto_mode_enabled` and calls into `agent_registry::graceful_exit` from inside `hooks.rs` — a path that previously only logged events. Risk surface widens; mitigated by idempotency + debounce + the tree-committable gate.
- **Stop hook event volume.** Every Claude turn emits a Stop event, not just the final one. The handler's gate (`status == 'running'` + auto-mode + clean tree) is the filter; expect a small uptick in `hook_events` row inserts proportional to turns-per-task.
- **Idle-poller is best-effort.** A driver that emits no telemetry has no `last_activity_at`, so the fallback can fire prematurely. Defaulted off, opt-in only, env-flag documented.

### Scope: standalone only

The hook URL contract pinned in Decision §1 (`http://localhost:<port>/hooks` with `<port>` taken from `AgentRegistry.port`) assumes the spawned CLI driver can reach the dashboard's HTTP listener directly on `localhost`. This holds for the standalone deployment where the dashboard, the supervisor daemon, and the agent PTY all live on the same host — exactly the same assumption the existing MCP injection at `http://127.0.0.1:<port>/mcp` already relies on (`server-rs/src/agents/driver.rs:280`).

**SaaS deployments are explicitly out of scope for this ADR.** In SaaS, the agent runs on a customer-side runner process while `/hooks` is served by the SaaS server across a WebSocket back-channel — `localhost:<port>` from the runner does not reach the server. The fix is to hand the hook URL through the runner's existing back-channel (the same WS the runner already uses for `StartAgent` / `AgentOutput` / etc.) rather than embedding a server-side URL in the per-session settings file. That work is tracked under the `saas-compat-*` backlog plans (`~/.claude/plans/backlog/saas-compat-*.yaml`, audit at `docs/architecture/saas-compat-audit.md`); this ADR does not block on it, and any standalone-mode-only plan landing first is fine.

## Failure modes (explicit)

1. **Agent leaves uncommitted work.** Tree gate trips, plan pauses with `agent_left_uncommitted_work`, agent stays at the prompt for human review. No silent auto-merge.
2. **Stop hook not delivered.** Network blip between Claude and the local hook URL, or Claude's hook subsystem suppresses the event. Falls through to whatever the driver's idle timer is configured for; if the operator hasn't opted into the idle-poller, the agent sits at the prompt and a human Finish click is the recovery.
3. **Stop event arrives after the agent already finished from another path.** Idempotency guard (`agents.status != 'running'`) makes the second invocation a no-op. The handler also debounces inside a short window so a same-turn duplicate doesn't double-fire even before the status update has landed.
4. **Orphan-phase tasks** (a task whose dependencies are met but its phase is otherwise complete and the next phase has already been scanned). Out of scope for this ADR; current behaviour preserved — those tasks wait for a human nudge or the next plan-level scan trigger.
5. **Per-session settings file collides with another file.** The naming source is `agent_id`, a fresh per-spawn UUID; collision probability is zero in practice. The directory (`claude_dir.join("sessions")`) is Branchwork-owned — no Claude-internal feature writes there. Path template + cleanup contract are pinned in Decision §1 (Task 0.2), and the same `path_for(agent_id)` helper is used at write-time and exit-time so write/cleanup can't drift.

(SaaS deployments where the runner can't reach the server's `/hooks` URL are not a failure mode of this ADR — they're an explicit non-goal. See Consequences §*Scope: standalone only*.)

## Out of scope

- Streaming / MCP-based completion detection (richer than the Stop hook, but a much larger lift).
- Auto-commit of the agent's uncommitted work before auto-finish — pause-and-notify is the chosen UX.
- Time-bounded execution caps (separate concern from "did the agent finish its turn").
- Distinguishing Claude paused-on-prompt from Claude finished-cleanly — Stop fires on both; the tree-committable gate is the discriminator.
- Aider/Codex/Gemini Stop-hook investigation beyond the trait-stub work in Phase 5. Filed as backlog plans at `~/.claude/plans/backlog/auto-mode-stop-hook-{aider,codex,gemini}.yaml` — each will reach a real `stop_hook_config` impl or an explicit "no portable hook — idle timer is canonical" decision before being closed.
- SaaS reachability for the hook URL (runner ↔ server back-channel). Tracked under the `saas-compat-*` backlog plans; see Consequences §*Scope: standalone only*.

## References

- ADR 0001 (GitHub App auth) and ADR 0002 (Worktree-per-agent isolation) — orthogonal, complementary work; this ADR makes no assumptions about either being landed.
- Repro plan + task: `~/.claude/plans/saas-folder-listing-via-runner.yaml`, task **5.1** — the live failure case that motivated this ADR (2026-05-04: 5.1 merged + CI green → 5.2 never spawned).
- Existing merge-on-completion hook site: `server-rs/src/agents/pty_agent.rs:584`.
- Existing intra-phase early-return: `server-rs/src/agents/mod.rs:1089-1138`.
- Existing hook receiver: `server-rs/src/hooks.rs:18`.
- Existing graceful exit (the call we re-enter from the Stop handler): `server-rs/src/agents/mod.rs:605`.
- Existing `phase_advanced` broadcast (whose shape Decision §3 pins verbatim): `server-rs/src/agents/mod.rs:1251-1259`.
- Existing `auto_mode.rs` broadcast convention that `task_advanced` follows on the `plan` field name: `server-rs/src/auto_mode.rs:152` (`auto_mode_merged`), `:189` (`auto_mode_paused`), `:222` (`auto_mode_state`).
- Existing per-agent file siblings already living in the chosen directory: `AgentRegistry::socket_for` / `mcp_config_for` (`server-rs/src/agents/mod.rs:308-319`), pidfile via `supervisor::pidfile_path` (`server-rs/src/agents/supervisor.rs:174`), `sockets_dir` initialized at `server-rs/src/main.rs:74`.
- `claude --settings <file-or-json>` flag verified via `claude --help` on 2026-05-04 (Claude Code CLI installed locally). Decision §1 also notes why we do **not** pair it with `--setting-sources`.
- Hook URL contract sources: `AgentRegistry.port` field (`server-rs/src/agents/mod.rs:245`), threaded into drivers at `pty_agent.rs:116` (`driver.mcp_config_json(registry.port)`), and reused by the Claude driver to build `http://127.0.0.1:<port>/mcp` at `server-rs/src/agents/driver.rs:280` — same `<port>` the Stop hook URL targets.
- SaaS-compat audit (the back-channel work that would carry the hook URL through the runner): `docs/architecture/saas-compat-audit.md`; backlog plans at `~/.claude/plans/backlog/saas-compat-*.yaml`.
- Implementation plan tracking this ADR: `~/.claude/plans/unattended-auto-mode.yaml` (this plan).
- Per-driver Stop-hook follow-up backlog plans (filed by task 5.1): `~/.claude/plans/backlog/auto-mode-stop-hook-aider.yaml`, `~/.claude/plans/backlog/auto-mode-stop-hook-codex.yaml`, `~/.claude/plans/backlog/auto-mode-stop-hook-gemini.yaml`.
