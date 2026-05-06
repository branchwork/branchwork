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
  }),
});

const AutoModeResumed = v.object({
  type: v.literal("auto_mode_resumed"),
  data: v.object({
    plan: v.string(),
    last_completed_task: NullishStr,
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

export const WsMessageSchema = v.variant("type", [
  Connected,
  PlanUpdated,
  PlanDeleted,
  AgentStarted,
  AgentOutput,
  AgentStopped,
  AgentBranchMerged,
  AgentBranchDiscarded,
  AgentBranchCleared,
  AutoModeMerged,
  AutoFinishTriggered,
  AutoModeState,
  AutoModeFixSpawned,
  AutoModePaused,
  AutoModeResumed,
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
]);

export type WsMessage = v.InferOutput<typeof WsMessageSchema>;

export type ParseResult<T> =
  | { ok: true; value: T }
  | { ok: false; error: string };

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
