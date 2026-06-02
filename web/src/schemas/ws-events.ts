import * as v from "valibot";

/// Schemas for every dashboard WebSocket event the server broadcasts.
/// `ws-store.ts` parses incoming frames through `parseWsMessage` so a
/// malformed payload logs and is dropped, instead of being silently
/// `as`-cast and partially applied to a store. Events broadcast by the
/// server but not yet handled by the dashboard (Phase 2 of the
/// dashboard-ui-overhaul plan) still need a schema here so the
/// validator does not flag them as malformed.

const NullishStr = v.nullish(v.string());
const NullishNum = v.nullish(v.number());

const PlanUpdated = v.object({
  type: v.literal("plan_updated"),
  data: v.object({
    action: v.string(),
    /// `plan` is the full ParsedPlan body the server sends. Schema
    /// coverage for server response types is intentionally hand-rolled
    /// for now (audit §12) — keep the inner payload as `unknown` so
    /// the validator gates the wire envelope without claiming to
    /// validate the inner Plan object.
    plan: v.optional(v.unknown()),
  }),
});

const PlanDeleted = v.object({
  type: v.literal("plan_deleted"),
  data: v.object({
    plan: v.string(),
    snapshot_id: NullishStr,
    hard: v.optional(v.boolean()),
  }),
});

const AgentStarted = v.object({
  type: v.literal("agent_started"),
  data: v.unknown(),
});

const AgentOutput = v.object({
  type: v.literal("agent_output"),
  data: v.object({
    agent_id: v.string(),
    message_type: v.string(),
    content: v.unknown(),
  }),
});

const AgentStopped = v.object({
  type: v.literal("agent_stopped"),
  data: v.object({
    id: v.string(),
    status: v.string(),
  }),
});

/// Runner reported `Command::spawn` Err for the session daemon (Task 1.1,
/// runner-install-and-spawn-reliability plan). The server already wrote the
/// pre-rendered message to `agents.spawn_error`; this broadcast triggers a
/// refetch so the dashboard surfaces the new column inline on the task
/// card. The matching `agent_stopped` envelope (status="failed") follows
/// immediately and is what actually flips the row off "starting".
const AgentSpawnFailed = v.object({
  type: v.literal("agent_spawn_failed"),
  data: v.object({
    id: v.string(),
    command: v.string(),
    errno: v.optional(v.number()),
    errno_str: v.string(),
    message: v.string(),
  }),
});

const AgentBranchMerged = v.object({
  type: v.literal("agent_branch_merged"),
  data: v.unknown(),
});

const AgentBranchDiscarded = v.object({
  type: v.literal("agent_branch_discarded"),
  data: v.unknown(),
});

/// Two emitters carry different payload shapes:
/// - `agents/mod.rs::boot_sweep` sends `{agent_id, branch, reason}` when
///   an orphaned task branch is cleared from the registry on startup.
/// - `api/plans.rs::clear_stale_branches` sends `{branch}` only when the
///   user clears a stale branch from the dashboard. No agent row is
///   associated, so `agent_id` is absent.
/// Schema accepts both: `branch` is the only invariant.
const AgentBranchCleared = v.object({
  type: v.literal("agent_branch_cleared"),
  data: v.object({
    agent_id: v.optional(v.string()),
    branch: v.string(),
    reason: v.optional(v.string()),
  }),
});

const AutoModeMerged = v.object({
  type: v.literal("auto_mode_merged"),
  data: v.object({
    plan: v.string(),
    task: v.string(),
    sha: NullishStr,
    target: NullishStr,
  }),
});

/// A task completed cleanly under a `phase` or `plan` merge cadence
/// but the cadence boundary hasn't been crossed yet. The agent's branch
/// stays intact; the dashboard can show a "deferred" pip on the task
/// card and the next boundary completion will drain everything in one
/// push. `cadence` carries the effective cadence ('task' | 'phase' |
/// 'plan') so the UI can render an explanatory tooltip.
const AutoModeMergeDeferred = v.object({
  type: v.literal("auto_mode_merge_deferred"),
  data: v.object({
    plan: v.string(),
    task: v.string(),
    agent_id: v.string(),
    cadence: v.string(),
  }),
});

/// Operator clicked Flush deferred merges on the dashboard. Fires once
/// at the end of the flush regardless of how many merges landed (the
/// per-merge `auto_mode_merged` events still fire inside the batch).
/// `count` is how many landed before the loop stopped; `paused` is
/// true when a merge failure aborted the flush mid-batch.
const AutoModeFlushedDeferred = v.object({
  type: v.literal("auto_mode_flushed_deferred"),
  data: v.object({
    plan: v.string(),
    count: v.number(),
    paused: v.boolean(),
  }),
});

const AutoFinishTriggered = v.object({
  type: v.literal("auto_finish_triggered"),
  data: v.object({
    agent_id: v.string(),
    plan: v.string(),
    task: v.string(),
    trigger: v.string(),
  }),
});

const AutoModeState = v.object({
  type: v.literal("auto_mode_state"),
  data: v.object({
    plan: v.string(),
    task: NullishStr,
    state: v.string(),
    sha: NullishStr,
    reason: NullishStr,
  }),
});

const AutoModeFixSpawned = v.object({
  type: v.literal("auto_mode_fix_spawned"),
  data: v.object({
    plan: v.string(),
    task: v.string(),
    fix_task: v.string(),
    fix_agent_id: v.string(),
    attempt: v.number(),
    ci_run_id: NullishStr,
  }),
});

const AutoModePaused = v.object({
  type: v.literal("auto_mode_paused"),
  data: v.object({
    plan: v.string(),
    task: NullishStr,
    reason: v.string(),
    target: NullishStr,
    runner_id: v.optional(v.string()),
    /// `auto_push_rebase_conflict` pauses carry the branch that failed
    /// to push + the conflicting file list captured from
    /// `git diff --name-only --diff-filter=U` before the rebase abort.
    /// Wire is capped to the first 10 paths so a pathological N-file
    /// conflict doesn't blow the WS frame; the audit row carries the
    /// full list, and `file_count` exposes the real total.
    branch: v.optional(v.string()),
    files: v.optional(v.array(v.string())),
    file_count: v.optional(v.number()),
  }),
});

const AutoModeResumed = v.object({
  type: v.literal("auto_mode_resumed"),
  data: v.object({
    plan: v.string(),
    last_completed_task: NullishStr,
    reason: v.optional(v.string()),
  }),
});

/// Pre-merge gate (T1.3 of the `pre-merge-gate` plan): a configured
/// `[auto_mode.pre_merge_checks]` entry exited non-zero (or the
/// whole-gate ceiling fired) before the merge could land. Plan is
/// paused with the literal `pausedReason = "pre_merge_check_failed"`
/// concurrently; the auto-mode pill listener reads that via the
/// PlanConfig snapshot. This event carries the structured detail the
/// dashboard banner renders: which check tripped, its exit code, and a
/// 4 KB middle-truncated snippet of the captured combined stdout+stderr.
const AutoModePreMergeCheckFailed = v.object({
  type: v.literal("auto_mode_pre_merge_check_failed"),
  data: v.object({
    plan: v.string(),
    task: NullishStr,
    agent_id: NullishStr,
    check_name: v.string(),
    /// `null` when the gate killed the check on per-check timeout or
    /// when the process died from a signal (no exit code surface).
    exit_code: v.nullish(v.number()),
    output_snippet: v.string(),
  }),
});

/// The operator toggled auto-mode OFF while an at-merge conflict resolver
/// was in flight (Task 3.4 of the worktree-per-agent plan). The resolver
/// agent was killed, but its worktree is preserved on disk (conflict markers
/// intact) so the human can resolve the conflict manually. `task` is the
/// ORIGINAL task id (not the `-resolve-N` resolver task) so the dashboard can
/// surface the conflict under the original task card; `conflicted_paths` is
/// the still-unmerged file list. Re-enabling auto-mode does NOT re-spawn the
/// resolver — the human must intervene.
const ResolverCancelled = v.object({
  type: v.literal("resolver_cancelled"),
  data: v.object({
    plan: v.string(),
    task: v.string(),
    agent_id: v.string(),
    parent_agent_id: NullishStr,
    conflicted_paths: v.array(v.string()),
  }),
});

const PlanRunnerAffinityChanged = v.object({
  type: v.literal("plan_runner_affinity_changed"),
  data: v.object({
    plan: v.string(),
    runner_id: NullishStr,
  }),
});

const TaskAdvanced = v.object({
  type: v.literal("task_advanced"),
  data: v.object({
    plan: v.string(),
    from_task: NullishStr,
    to_tasks: v.optional(v.array(v.string())),
  }),
});

const TaskChecked = v.object({
  type: v.literal("task_checked"),
  data: v.object({
    plan_name: v.string(),
    task_number: v.string(),
    status: v.string(),
  }),
});

const PlanChecked = v.object({
  type: v.literal("plan_checked"),
  data: v.object({
    plan_name: v.string(),
    verdict: v.string(),
    reason: NullishStr,
    agent_id: NullishStr,
  }),
});

const TaskStatusChanged = v.object({
  type: v.literal("task_status_changed"),
  data: v.object({
    plan_name: v.string(),
    task_number: v.string(),
    status: v.string(),
  }),
});

const CiStatusChanged = v.object({
  type: v.literal("ci_status_changed"),
  data: v.object({
    id: v.number(),
    plan_name: v.string(),
    task_number: v.string(),
    status: v.string(),
    conclusion: NullishStr,
    run_url: NullishStr,
    commit_sha: NullishStr,
  }),
});

const PlanWarning = v.object({
  type: v.literal("plan_warning"),
  data: v.object({
    name: v.string(),
    file: v.string(),
    error: v.string(),
  }),
});

const HookEvent = v.object({
  type: v.literal("hook_event"),
  data: v.unknown(),
});

const AuditLog = v.object({
  type: v.literal("audit_log"),
  data: v.object({
    org_id: v.string(),
    user_email: NullishStr,
    action: v.string(),
    resource_type: v.string(),
    resource_id: NullishStr,
  }),
});

/// --- Events broadcast today but not yet handled by the dashboard ---
/// (audit §4 — Phase 2 of dashboard-ui-overhaul wires the handlers).
/// Schemas are listed up front so a future `case "phase_advanced":` in
/// `handleWsMessage` can reach for `WsMessage` typing without growing
/// this file.

const PhaseAdvanced = v.object({
  type: v.literal("phase_advanced"),
  data: v.object({
    plan_name: v.string(),
    from_phase: v.number(),
    to_phase: v.number(),
  }),
});

const TaskCostReported = v.object({
  type: v.literal("task_cost_reported"),
  data: v.object({
    plan_name: v.string(),
    task_number: v.string(),
    amount_usd: v.number(),
  }),
});

const PlanReset = v.object({
  type: v.literal("plan_reset"),
  data: v.object({
    plan_name: v.string(),
    cleared: v.unknown(),
  }),
});

const CiRunDismissed = v.object({
  type: v.literal("ci_run_dismissed"),
  data: v.object({
    id: NullishNum,
    plan_name: v.string(),
    task_number: v.string(),
  }),
});

const RunnerConnected = v.object({
  type: v.literal("runner_connected"),
  data: v.object({
    runner_id: v.string(),
    runner_name: NullishStr,
  }),
});

const RunnerDisconnected = v.object({
  type: v.literal("runner_disconnected"),
  data: v.object({
    runner_id: v.string(),
  }),
});

const RunnerDrivers = v.object({
  type: v.literal("runner_drivers"),
  data: v.object({
    runner_id: v.string(),
    // Loose-typed: the runner protocol may add `DriverAuthStatus`
    // variants we haven't modelled yet (future drivers / cloud
    // providers). The handler casts to `RunnerDriverInfo[]` and the
    // RunnersPage chip ignores variants it doesn't recognize.
    drivers: v.unknown(),
  }),
});

/// One line of the runner host's stdout/stderr — the `[runner] connecting…`
/// breadcrumbs the runner prints to its own terminal, forwarded straight to
/// the dashboard's `/runners/{id}` tail panel. Best-effort fan-out from the
/// `RunnerLogLine` wire variant; no DB persistence on the server side.
const RunnerLog = v.object({
  type: v.literal("runner_log"),
  data: v.object({
    runner_id: v.string(),
    /// RFC3339 / ISO 8601 timestamp captured at emit time on the runner.
    ts: v.string(),
    /// One of `info` / `warn` / `error`. Free-form so future levels flow
    /// through without a wire change.
    level: v.string(),
    line: v.string(),
  }),
});

/// `auto_mode` rebased the local branch onto origin after a non-FF
/// rejection and retried the push (see Phase 1 of the auto-push-rebase-
/// on-non-fast-forward plan). One event per rebase retry — operator
/// visibility for the race between sibling-agent / auto-bump pushes
/// landing on the canonical default branch. The dashboard surfaces a
/// transient "rebased on origin (n)" pill next to the auto-mode
/// indicator for ~10 s and then auto-clears.
const AutoPushRebased = v.object({
  type: v.literal("auto_push_rebased"),
  data: v.object({
    plan: v.string(),
    task: v.string(),
    branch: v.string(),
    attempt: v.number(),
    /// 40-char SHA — empty string when git rev-parse failed (the
    /// retry helper falls back to "" rather than failing the push).
    last_rebase_sha: v.string(),
    prior_remote_sha: v.string(),
  }),
});

/// Per-runner health snapshot — outbox depth, 24-hour reconnect count,
/// CI-poll latency percentiles, version-mismatch verdict. Pushed on a
/// ~30 s tick by the runner via the `RunnerHealth` wire variant; the
/// server forwards (with `version_mismatch` computed against
/// `CARGO_PKG_VERSION`) so the dashboard chip + Health panel update
/// without a refetch.
const RunnerHealth = v.object({
  type: v.literal("runner_health"),
  data: v.object({
    runner_id: v.string(),
    outbox_depth: v.number(),
    ws_reconnects_24h: v.number(),
    ci_poll_ms_p50: NullishNum,
    ci_poll_ms_p99: NullishNum,
    /// Runner-reported version (mirrors `runners.version` in the DB).
    version: NullishStr,
    /// Server's `env!("CARGO_PKG_VERSION")` — same value the API surfaces
    /// at the response root so the chip can render `0.3.x → 0.4.x` style
    /// labels.
    server_version: NullishStr,
    /// `ok | patch | minor | major` — same vocabulary as `versionMismatch`
    /// on `/api/runners` rows.
    version_mismatch: v.string(),
    /// T1.3: coarser color verdict (`green | amber | red`). Optional for
    /// back-compat with older server builds that haven't shipped the
    /// field yet.
    version_severity: v.optional(v.string()),
    /// T1.3: operator-set override that lifts the dispatch block. Optional
    /// for back-compat (older servers always send false implicitly).
    version_mismatch_override: v.optional(v.boolean()),
  }),
});

/// Transport-level hello sent once by the server immediately after
/// the WS upgrade succeeds (see `server-rs/src/ws.rs:30-42`). Not a
/// domain event — no broadcast catalogue entry, no DB row, no UI
/// reaction beyond optionally reading `timestamp` for clock-skew
/// telemetry. Listed here so the validator stops flagging it as
/// malformed; ws-store treats it as a no-op.
const Connected = v.object({
  type: v.literal("connected"),
  timestamp: v.string(),
});

/// Runner has accepted a `CloneProject` request and started the clone.
/// Fire-and-forget progress breadcrumb — the terminal `clone_done` or
/// `clone_failed` event is what unblocks the HTTP caller (and updates the
/// projects.workspace_path column). Surfaced to the NewProjectModal so it
/// can show a `Cloning …` indicator between submit and done.
const CloneStarted = v.object({
  type: v.literal("clone_started"),
  data: v.object({
    runner_id: v.string(),
    req_id: v.string(),
  }),
});

/// Runner finished cloning into `resolved_path`. The HTTP response
/// arrives shortly after this fires; the modal can use either as the
/// success signal but listening to the WS event lets it react before
/// the HTTP response lands.
const CloneDone = v.object({
  type: v.literal("clone_done"),
  data: v.object({
    runner_id: v.string(),
    req_id: v.string(),
    resolved_path: v.string(),
  }),
});

/// Runner could not clone — captured stderr lives in `error`. Fires
/// before the HTTP response (which surfaces a 500 with the same body).
const CloneFailed = v.object({
  type: v.literal("clone_failed"),
  data: v.object({
    runner_id: v.string(),
    req_id: v.string(),
    error: v.string(),
  }),
});

/// A typed CI failure event was observed on a live agent's branch
/// (Phase 1, Task 1.1). Fires from `ci.rs::record_ci_failure_observed`
/// at most once per `(agent_id, run_id)` pair. The "Learnings due"
/// panel listens for this to refetch its queue without polling.
const CiFailureObserved = v.object({
  type: v.literal("ci_failure_observed"),
  data: v.object({
    agent_id: v.string(),
    plan_name: v.string(),
    task_number: NullishStr,
    branch: v.string(),
    run_id: v.string(),
    run_url: NullishStr,
    workflow: NullishStr,
    conclusion: NullishStr,
    failed_job: v.nullish(v.unknown()),
    summary: NullishStr,
    org_id: v.string(),
  }),
});

/// A learning crossed the promotion threshold (Phase 4, Task 4.1).
/// Fires from `spawn_ops::record_learning_hits_for_spawn` at most once
/// per learning. The Promotions admin tab listens for this to refetch
/// its candidate list without polling.
const PromotionCandidate = v.object({
  type: v.literal("promotion_candidate"),
  data: v.object({
    learning_id: v.string(),
    org_id: v.string(),
    hit_count: v.number(),
    threshold: v.number(),
    window_days: v.number(),
  }),
});

/// A promotion candidate was approved and a PR was opened against the
/// project repo (Phase 4, Task 4.2). Fires from
/// `api/learnings.rs::promote_learning` on a successfully-opened PR. The
/// Promotions tab refetches so the row flips to "promoted → <PR link>".
const LearningPromoted = v.object({
  type: v.literal("learning_promoted"),
  data: v.object({
    learning_id: v.string(),
    org_id: v.string(),
    pr_url: v.string(),
  }),
});

export const WsMessageSchema = v.variant("type", [
  Connected,
  PlanUpdated,
  PlanDeleted,
  AgentStarted,
  AgentOutput,
  AgentStopped,
  AgentSpawnFailed,
  AgentBranchMerged,
  AgentBranchDiscarded,
  AgentBranchCleared,
  AutoModeMerged,
  AutoModeMergeDeferred,
  AutoModeFlushedDeferred,
  AutoFinishTriggered,
  AutoModeState,
  AutoModeFixSpawned,
  AutoModePaused,
  AutoModeResumed,
  AutoModePreMergeCheckFailed,
  ResolverCancelled,
  TaskAdvanced,
  TaskChecked,
  PlanChecked,
  TaskStatusChanged,
  CiStatusChanged,
  PlanWarning,
  HookEvent,
  AuditLog,
  PhaseAdvanced,
  TaskCostReported,
  PlanReset,
  CiRunDismissed,
  RunnerConnected,
  RunnerDisconnected,
  RunnerDrivers,
  RunnerLog,
  RunnerHealth,
  PlanRunnerAffinityChanged,
  AutoPushRebased,
  CloneStarted,
  CloneDone,
  CloneFailed,
  CiFailureObserved,
  PromotionCandidate,
  LearningPromoted,
]);

export type WsMessage = v.InferOutput<typeof WsMessageSchema>;

export type ParseResult<T> = { ok: true; value: T } | { ok: false; error: string };

/// Parse a raw WS frame body into a typed `WsMessage`. Accepts either a
/// JSON string (the on-the-wire form delivered to `ws.onmessage`) or an
/// already-decoded object (handy for unit tests). Returns
/// `{ ok: false, error }` for both invalid JSON and a payload that
/// fails schema validation; the caller logs and drops it.
export function parseWsMessage(input: unknown): ParseResult<WsMessage> {
  let value: unknown = input;
  if (typeof input === "string") {
    try {
      value = JSON.parse(input);
    } catch (e) {
      return {
        ok: false,
        error: `invalid JSON: ${e instanceof Error ? e.message : String(e)}`,
      };
    }
  }
  const result = v.safeParse(WsMessageSchema, value);
  if (result.success) return { ok: true, value: result.output };
  return { ok: false, error: v.summarize(result.issues) };
}
