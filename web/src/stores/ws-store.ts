import { create } from "zustand";
import { getInFlightPlansFetch, usePlanStore } from "./plan-store.js";
import { getInFlightAgentsFetch, useAgentStore } from "./agent-store.js";
import { useSettingsStore } from "./settings-store.js";
import { useRunnerStore, type RunnerDriverInfo } from "./runner-store.js";
import { parseWsMessage, type WsMessage } from "../schemas/ws-events.js";
import { notify } from "../lib/notifications.js";

const MAX_RECONNECT_DELAY = 30_000;
const INITIAL_RECONNECT_DELAY = 2_000;

/// Skip a WS-triggered refetch when one resolved within this window. The
/// in-flight fetch (or the just-resolved fetch) already captured server
/// state; a second round trip would just race itself. Tuned to be longer
/// than typical /api/plans latency under load (~hundreds of ms) so back-
/// to-back WS events collapse to a single fetch.
const WS_REFETCH_DEBOUNCE_MS = 1_500;

function lookupTaskTitle(planName: string | null, taskNumber: string | null): string {
  if (!planName || !taskNumber) return taskNumber ?? "task";
  const plan = usePlanStore.getState().selectedPlan;
  if (plan?.name !== planName) return `Task ${taskNumber}`;
  for (const phase of plan.phases) {
    const t = phase.tasks.find((x) => x.number === taskNumber);
    if (t) return `Task ${taskNumber}: ${t.title}`;
  }
  return `Task ${taskNumber}`;
}

interface WsStore {
  connected: boolean;
  socket: WebSocket | null;
  reconnectAttempt: number;
  /// ms-since-epoch when the WS dropped from `connected:true` to closed.
  /// Cleared on reconnect (back to `null`). Read by `<ConnectionBanner/>`
  /// to gate the warning banner behind a 2s grace window so a momentary
  /// blip (network blip, server restart of <2s) doesn't flash a banner
  /// at the user. Also read by `<StaleDataChip/>` to decide whether to
  /// surface "this is stale" alongside cached data: chip shows when WS
  /// is disconnected AND `lastFetchedAt` is >60s old.
  ///
  /// Note this is intentionally module-level rather than per-fetch:
  /// the moment WS drops we lose authoritative live updates for every
  /// slice, so a single timestamp is the right granularity.
  disconnectedSince: number | null;
  connect: () => void;
  disconnect: () => void;
  /// Disconnect the socket, clear all subscriptions, and reset state.
  /// Driven by `lib/reset-all.ts::resetAllStores()` on logout so the
  /// previous user's session can't keep mutating stores via late
  /// broadcasts. Distinct from `disconnect()` (which leaves
  /// subscriptions intact for a future reconnect): on a real logout we
  /// want components that re-mount under user B to re-subscribe with
  /// fresh closures rather than fire stale handlers from user A's
  /// component tree.
  reset: () => void;
}

type WsEventName = WsMessage["type"];

interface WsSubscription {
  eventNames: ReadonlySet<WsEventName>;
  handler: (msg: WsMessage) => void;
}

/// Module-level subscriber registry. Lives outside the WsStore so its
/// lifetime is decoupled from the socket: a reconnect tears down + re-
/// creates the WebSocket, but the subscriber Set is untouched, so
/// handlers keep firing for the new socket without re-registration.
///
/// Audit §4 motivation: components that grabbed `useWsStore.getState()
/// .socket` and called `addEventListener("message", …)` directly were
/// bound to a specific socket instance and silently dropped events
/// after every reconnect — the new socket had no listener until the
/// component remounted. Routing dispatch through this registry fixes
/// that without exposing the socket to consumers.
const wsSubscriptions = new Set<WsSubscription>();

/// Subscribe a handler to one or more WS event types. Returns an
/// unsubscribe function — call from a `useEffect` cleanup so the
/// handler is dropped on unmount and unblocks GC of any captured state.
///
/// The handler runs AFTER the built-in dispatch switch updates every
/// store, so subscribers see the same event the rest of the dashboard
/// just reacted to. Handler exceptions are caught and logged (one
/// misbehaving subscriber must not break delivery to others).
export function subscribeToWsEvents<T extends WsEventName>(
  eventNames: readonly T[],
  handler: (msg: Extract<WsMessage, { type: T }>) => void,
): () => void {
  const sub: WsSubscription = {
    eventNames: new Set(eventNames),
    handler: handler as (msg: WsMessage) => void,
  };
  wsSubscriptions.add(sub);
  return () => {
    wsSubscriptions.delete(sub);
  };
}

export const useWsStore = create<WsStore>((set, get) => ({
  connected: false,
  socket: null,
  reconnectAttempt: 0,
  /// Seeded as `null` (the "we never connected yet" signal) so the
  /// banner stays hidden during the initial bootstrap — the user sees
  /// "Disconnected. Reconnecting…" only once we've actually been
  /// connected and lost it. App.tsx flips us to a real timestamp via
  /// the close handler below.
  disconnectedSince: null,

  connect: () => {
    const { socket } = get();
    if (socket) return;

    // Audit §16: do NOT prompt for browser-notification permission on
    // first connect. The user opts in via the AdminPage "Enable
    // notifications" button — see web/src/lib/notifications.ts.

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    let ws: WebSocket;
    try {
      ws = new WebSocket(`${protocol}//${window.location.host}/ws`);
    } catch (e) {
      console.error("[ws] Failed to create WebSocket:", e);
      scheduleReconnect(get);
      return;
    }

    ws.onopen = () => {
      const wasReconnect = get().reconnectAttempt > 0;
      // Clear `disconnectedSince` on every successful open (initial *and*
      // reconnect) so the banner / stale chip disappear the moment the
      // socket recovers. The wasReconnect flag below still drives the
      // refetch sweep — that's a separate signal.
      set({ connected: true, reconnectAttempt: 0, disconnectedSince: null });
      if (wasReconnect) {
        // Refetch every cached slice — events during the disconnect were
        // lost, so without this the dashboard could show a stale agent
        // status, an outdated webhook URL, or a since-removed driver until
        // the user navigated.
        // Audit §4 (minor): settings + drivers + per-plan auto-mode-config
        // were missed by the original two-call refetch.
        const planStore = usePlanStore.getState();
        const settingsStore = useSettingsStore.getState();
        planStore.fetchPlans().catch(() => {});
        useAgentStore
          .getState()
          .fetchAgents()
          .catch(() => {});
        settingsStore.fetchSettings().catch(() => {});
        settingsStore.fetchDrivers().catch(() => {});
        // Runner inventory: any `runner_connected/_disconnected` events that
        // fired during the disconnect were lost, so the cached `online/offline`
        // status could be wrong. Refetching gives us a clean baseline.
        useRunnerStore
          .getState()
          .fetchRunners()
          .catch(() => {});
        // Per-plan auto-mode-config: only refetch the plans we already
        // know about (planConfigs is keyed by plans the user has opened
        // in this session). New plans surface via plan_updated /
        // plan_created events, which have their own fetch path.
        for (const planName of Object.keys(planStore.planConfigs)) {
          planStore.fetchPlanConfig(planName).catch(() => {});
        }
      }
    };

    ws.onerror = (ev) => {
      console.error("[ws] WebSocket error:", ev);
    };

    ws.onclose = () => {
      // Stamp `disconnectedSince` only on the FIRST close in a streak.
      // A retry that fails synchronously fires another onclose; we want
      // ConnectionBanner's 2s grace measured from the original drop, not
      // from each retry, otherwise long disconnects keep resetting the
      // grace window and the banner never appears.
      const prev = get().disconnectedSince;
      set({
        connected: false,
        socket: null,
        disconnectedSince: prev ?? Date.now(),
      });
      scheduleReconnect(get);
    };

    ws.onmessage = (ev) => {
      handleWsMessage(ev.data);
    };

    set({ socket: ws });
  },

  disconnect: () => {
    const { socket } = get();
    if (socket) {
      socket.close();
      set({
        socket: null,
        connected: false,
        reconnectAttempt: 0,
        disconnectedSince: null,
      });
    }
  },

  reset: () => {
    const { socket } = get();
    if (socket) {
      // Some browsers throw when close() is called on a socket that's
      // already mid-close. We don't care about the error here — the
      // state assignment below makes us forget the handle either way.
      try {
        socket.close();
      } catch {
        /* already closed */
      }
    }
    // Drop every external subscriber. Components mounted under user A
    // will unmount when the login screen renders (user becomes null),
    // which fires their useEffect cleanups; clearing here is belt-and-
    // braces for any subscriber whose cleanup didn't reach the Set
    // (e.g. an old test that registered without unmounting).
    wsSubscriptions.clear();
    set({
      socket: null,
      connected: false,
      reconnectAttempt: 0,
      disconnectedSince: null,
    });
  },
}));

function scheduleReconnect(get: () => WsStore) {
  const attempt = get().reconnectAttempt;
  const delay = Math.min(INITIAL_RECONNECT_DELAY * Math.pow(2, attempt), MAX_RECONNECT_DELAY);
  useWsStore.setState({ reconnectAttempt: attempt + 1 });
  setTimeout(() => get().connect(), delay);
}

let planRefreshTimer: ReturnType<typeof setTimeout> | null = null;

/// Run `fn` after any in-flight `/api/plans` fetch resolves. With no fetch
/// in flight, `fn` runs synchronously so existing tests that assert state
/// immediately after `handleWsMessage(...)` keep working.
///
/// Audit §4 race this guards: a slow `/api/plans` response could land
/// AFTER a WS event arrived and clobber the optimistic patch the handler
/// applied. Deferring the patch until the response lands flips the order
/// so the patch is on top — `final state = fetch result + WS patch`.
function deferBehindPlansFetch(fn: () => void): void {
  const inFlight = getInFlightPlansFetch();
  if (!inFlight) {
    fn();
    return;
  }
  inFlight.then(fn, fn);
}

/// Same contract as `deferBehindPlansFetch`, but for `/api/agents`. Used
/// by handlers that mutate the local `agents[]` slice (`updateAgentStatus`,
/// `clearAgentBranch`) so an overlapping `fetchAgents()` can't clobber
/// the patch with a stale snapshot.
function deferBehindAgentsFetch(fn: () => void): void {
  const inFlight = getInFlightAgentsFetch();
  if (!inFlight) {
    fn();
    return;
  }
  inFlight.then(fn, fn);
}

/// Schedule the debounced `fetchPlans()` refetch shared by `plan_updated`
/// and `task_status_changed`. At fire time, skip the round-trip when a
/// fetch is in flight or one resolved within the debounce window — the
/// optimistic patches we already layered are on top of fetched state, so
/// a duplicate fetch is wasted bandwidth and another opportunity to race.
function schedulePlansRefetch(): void {
  if (planRefreshTimer) clearTimeout(planRefreshTimer);
  planRefreshTimer = setTimeout(() => {
    planRefreshTimer = null;
    if (getInFlightPlansFetch()) return;
    const last = usePlanStore.getState().lastPlansFetchedAt;
    if (last !== null && Date.now() - last < WS_REFETCH_DEBOUNCE_MS) return;
    usePlanStore
      .getState()
      .fetchPlans()
      .catch(() => {});
  }, 2000);
}

// Exported for unit testing. Accepts either a raw JSON string (from
// `ws.onmessage`) or an already-decoded message object (handy for tests).
// Validation runs through `parseWsMessage`; malformed payloads are
// logged once and dropped — they never reach the store.
export function handleWsMessage(input: unknown) {
  const parsed = parseWsMessage(input);
  if (!parsed.ok) {
    console.warn("[ws] dropped malformed message:", parsed.error);
    return;
  }
  dispatch(parsed.value);
}

function dispatch(msg: WsMessage) {
  const planStore = usePlanStore.getState();
  const agentStore = useAgentStore.getState();

  switch (msg.type) {
    case "plan_updated": {
      const { plan } = msg.data;
      deferBehindPlansFetch(() => {
        const planStoreNow = usePlanStore.getState();
        if (plan) {
          planStoreNow.updatePlan(plan as Parameters<typeof planStoreNow.updatePlan>[0]);
        }
        schedulePlansRefetch();
      });
      break;
    }
    case "plan_deleted": {
      // The server emits this right after the cascade commits in
      // delete_plan (api/plans.rs). Drop the plan from the summary list
      // and clear `selectedPlan` if the user was viewing it — App.tsx
      // routes back to ProjectDashboard the moment selectedPlan is null.
      // Soft delete carries `snapshot_id`; we surface it as an Undo
      // action so the renderer can POST /api/snapshots/{id}/restore.
      // Hard delete (`hard: true`) has no snapshot_id and no Undo.
      const d = msg.data;
      planStore.removePlan(d.plan);
      const snapshotId = d.snapshot_id ?? undefined;
      planStore.pushToast({
        kind: "info",
        message: `Deleted plan ${d.plan}`,
        action: snapshotId ? { label: "Undo", snapshotId } : undefined,
        ttlMs: 30_000,
      });
      break;
    }
    case "agent_started": {
      agentStore.fetchAgents();
      break;
    }
    case "agent_output": {
      const d = msg.data;
      agentStore.appendOutput(d.agent_id, {
        id: Date.now(),
        agent_id: d.agent_id,
        message_type: d.message_type,
        content: typeof d.content === "string" ? d.content : JSON.stringify(d.content),
        timestamp: new Date().toISOString(),
      });
      break;
    }
    case "agent_stopped": {
      const d = msg.data;
      const agent = agentStore.agents.find((a) => a.id === d.id);
      const taskLabel = agent
        ? lookupTaskTitle(agent.plan_name, agent.task_id)
        : `Agent ${d.id.slice(0, 8)}`;
      notify("agent_stopped", `${taskLabel} — ${d.status}`, "Agent finished", `agent-${d.id}`);
      // Defer the patch behind any in-flight fetchAgents — otherwise a slow
      // /api/agents response could clobber `status` back to running. The
      // refetch we follow with is coalesced via `getInFlightAgentsFetch`,
      // so concurrent callers share the same round-trip.
      deferBehindAgentsFetch(() => {
        const agentStoreNow = useAgentStore.getState();
        agentStoreNow.updateAgentStatus(d.id, d.status);
        // Refetch to pick up fields that may have been updated after initial
        // insert (branch, cost, base_commit) — ensures the task card's Merge
        // button shows up immediately when the agent finishes.
        agentStoreNow.fetchAgents().catch(() => {});
      });
      break;
    }
    case "agent_branch_merged":
    case "agent_branch_discarded": {
      // Branch was merged/discarded — refetch so the Merge button disappears
      agentStore.fetchAgents();
      break;
    }
    case "auto_mode_merged": {
      // The auto-mode loop merged a task branch. The lower-level
      // `agent_branch_merged` event also fires from the merge inner and
      // already triggers fetchAgents; this case adds the user-facing
      // notification with the plan/task context that's only visible at the
      // auto-mode layer.
      const d = msg.data;
      const taskLabel = lookupTaskTitle(d.plan, d.task);
      const targetSuffix = d.target ? ` → ${d.target}` : "";
      notify(
        "auto_mode_merged",
        `Auto-merged: ${taskLabel}${targetSuffix}`,
        d.plan,
        `auto-mode-merged-${d.plan}-${d.task}`,
      );
      // Defensive refetch: agent_branch_merged already triggers this on the
      // local merge path, but the SaaS path goes through a runner round-trip
      // and may race the broadcast — refetching here keeps the UI converged
      // regardless of which event arrives first.
      agentStore.fetchAgents();
      break;
    }
    case "auto_finish_triggered": {
      // The unattended Stop-hook (or idle-poller fallback) decided to
      // finalize the agent. The auto-mode loop will run the
      // merge → CI → advance state machine next; this transient pill
      // label sits between `agent_stopped` and the first
      // `auto_mode_state` event (state=merging) so the user sees the
      // loop start work without a visible gap. Overwritten by the
      // following `auto_mode_state` broadcast.
      const d = msg.data;
      planStore.setAutoModeRuntime(d.plan, {
        state: "auto_finishing",
        task: d.task,
      });
      // Refresh agents so the row's stop_reason / status flips visibly
      // even before `agent_stopped` lands (graceful_exit is async).
      agentStore.fetchAgents();
      break;
    }
    case "auto_mode_state": {
      // Live pill feed: every transition from the auto-mode state machine
      // (merging → awaiting_ci → advancing|paused) carries a `state` label
      // here. The pill reads this map; transient labels overwrite each
      // other. Advancing is treated as idle and clears the runtime so the
      // pill disappears between tasks.
      const d = msg.data;
      if (d.state === "advancing") {
        planStore.setAutoModeRuntime(d.plan, null);
      } else if (
        d.state === "merging" ||
        d.state === "awaiting_ci" ||
        d.state === "fixing_ci" ||
        d.state === "paused"
      ) {
        planStore.setAutoModeRuntime(d.plan, {
          state: d.state,
          task: d.task ?? null,
          sha: d.sha ?? null,
          reason: d.reason ?? null,
        });
      }
      break;
    }
    case "auto_mode_fix_spawned": {
      // A fix agent was spawned for a Red CI outcome. The state-machine
      // doesn't broadcast a `fixing_ci` auto_mode_state event today, so we
      // synthesise one here using the attempt count from the payload + the
      // cap from the per-plan PlanConfig (read at render time in the pill).
      const d = msg.data;
      planStore.setAutoModeRuntime(d.plan, {
        state: "fixing_ci",
        task: d.task,
        attempt: d.attempt,
      });
      break;
    }
    case "auto_mode_paused": {
      // The auto-mode loop paused itself for a plan. The pill reads
      // `pausedReason` from per-plan PlanConfig (persistent across reloads),
      // so we patch the local config here from the event payload to avoid a
      // separate refetch — the server already wrote the column.
      const d = msg.data;
      planStore.patchPlanConfig(d.plan, { pausedReason: d.reason });
      planStore.setAutoModeRuntime(d.plan, {
        state: "paused",
        task: d.task ?? null,
        reason: d.reason,
      });
      const taskLabel = d.task ? lookupTaskTitle(d.plan, d.task) : "plan";
      notify(
        "auto_mode_paused",
        `Auto-mode paused: ${d.plan}`,
        `${taskLabel} — ${d.reason}`,
        `auto-mode-paused-${d.plan}`,
      );
      break;
    }
    case "auto_mode_resumed": {
      // User clicked Resume; the server cleared `paused_reason` and re-
      // evaluated auto-advance. Update local config + clear runtime so the
      // pill disappears immediately.
      const d = msg.data;
      planStore.patchPlanConfig(d.plan, { pausedReason: null });
      planStore.setAutoModeRuntime(d.plan, null);
      break;
    }
    case "task_advanced": {
      // Intra-phase advance: a task finished and one or more sibling tasks
      // in the same phase are being spawned. Refresh agents so the new rows
      // appear immediately. The accompanying `task_status_changed` events
      // already drive task-pill state, so no plan refetch is needed here.
      // No pill update — the existing phase_advanced + auto_mode_state
      // events drive the auto-mode pill. task_advanced is purely a refresh
      // trigger today (UI pill enrichment lands in 6.1).
      agentStore.fetchAgents();
      break;
    }
    case "task_checked": {
      const d = msg.data;
      deferBehindPlansFetch(() => {
        usePlanStore.getState().patchTaskStatus(d.plan_name, d.task_number, d.status);
      });
      break;
    }
    case "plan_checked": {
      const d = msg.data;
      // Notification reads only event payload, no store state — fire it
      // before the deferred patch so the user sees it without waiting on a
      // possibly-slow in-flight fetch.
      notify(
        "plan_checked",
        `Plan check: ${d.verdict}`,
        d.reason ? `${d.plan_name} — ${d.reason}` : d.plan_name,
        `plan-checked-${d.plan_name}`,
      );
      deferBehindPlansFetch(() => {
        usePlanStore.getState().patchPlanVerdict(d.plan_name, {
          verdict: d.verdict,
          reason: d.reason ?? null,
          agentId: d.agent_id ?? null,
          checkedAt: new Date().toISOString(),
        });
      });
      break;
    }
    case "task_status_changed": {
      const d = msg.data;
      if (d.status === "completed" || d.status === "failed") {
        notify(
          "task_status_changed",
          `${lookupTaskTitle(d.plan_name, d.task_number)} — ${d.status}`,
          d.plan_name,
          `task-${d.plan_name}-${d.task_number}`,
        );
      }
      // Defer both the optimistic patch AND the debounced refetch behind
      // any in-flight fetchPlans. Without this, a /api/plans response that
      // landed AFTER this WS event would clobber the patched `doneCount`
      // and `selectedPlan.task.status` with the pre-event server snapshot.
      // See the audit-§4 acceptance test in ws-store.test.ts.
      deferBehindPlansFetch(() => {
        usePlanStore.getState().patchTaskStatus(d.plan_name, d.task_number, d.status);
        // Authoritative refetch — guarantees convergence to server truth
        // (doneCount drift on non-selected plans, MCP/agent-driven
        // transitions) even when the signed-delta in patchTaskStatus
        // can't see the prior value. Shares planRefreshTimer so bursts
        // collapse with plan_updated events.
        schedulePlansRefetch();
      });
      break;
    }
    case "ci_status_changed": {
      const d = msg.data;
      if (d.status === "success" || d.status === "failure") {
        notify(
          "ci_status_changed",
          `CI ${d.status}: ${lookupTaskTitle(d.plan_name, d.task_number)}`,
          d.run_url ?? d.plan_name,
          `ci-${d.plan_name}-${d.task_number}`,
        );
      }
      deferBehindPlansFetch(() => {
        usePlanStore.getState().patchTaskCi(d.plan_name, d.task_number, {
          id: d.id,
          status: d.status as
            | "pending"
            | "running"
            | "success"
            | "failure"
            | "cancelled"
            | "unknown",
          conclusion: d.conclusion ?? null,
          runUrl: d.run_url ?? null,
          commitSha: d.commit_sha ?? null,
          updatedAt: new Date().toISOString(),
        });
      });
      break;
    }
    case "plan_warning": {
      const d = msg.data;
      notify("plan_warning", `Plan error: ${d.name}`, d.error, `plan-warning-${d.name}`);
      planStore.addWarning({
        name: d.name,
        file: d.file,
        error: d.error,
        timestamp: Date.now(),
      });
      break;
    }
    case "phase_advanced": {
      // Cross-phase advance from `try_auto_advance`. Refresh the selected
      // plan so the new phase's tasks render with their current statuses
      // (intra-phase advances ride `task_status_changed` instead — those
      // already debounce-refetch the plan list).
      const d = msg.data;
      const planStoreNow = usePlanStore.getState();
      if (planStoreNow.selectedPlan?.name === d.plan_name) {
        planStoreNow.selectPlan(d.plan_name).catch(() => {
          // Swallow: the next plan_updated / task_status_changed will retry.
        });
      }
      const taskLabel = `Phase ${d.to_phase}`;
      planStoreNow.pushToast({
        kind: "info",
        message: `${d.plan_name}: advanced to ${taskLabel}`,
        ttlMs: 8_000,
      });
      notify(
        "phase_advanced",
        `Phase ${d.to_phase} advanced`,
        d.plan_name,
        `phase-advanced-${d.plan_name}-${d.to_phase}`,
      );
      break;
    }
    case "task_cost_reported": {
      // Agents report their cost via the MCP cost-report tool; the server
      // already wrote the row, this just keeps the per-task pill + the
      // ProjectDashboard aggregate in sync without a refetch.
      const d = msg.data;
      deferBehindPlansFetch(() => {
        usePlanStore.getState().patchTaskCost(d.plan_name, d.task_number, d.amount_usd);
      });
      break;
    }
    case "plan_reset": {
      // The user wiped `task_status` for the plan via the reset endpoint.
      // Refresh the selected plan (so per-task statuses go back to derived)
      // and the summary list (so doneCount drops to zero) — the server
      // already deleted the rows. Toast surfaces the count cleared so a
      // mistaken reset is recoverable via undo (future work).
      const d = msg.data;
      const planStoreNow = usePlanStore.getState();
      if (planStoreNow.selectedPlan?.name === d.plan_name) {
        planStoreNow.selectPlan(d.plan_name).catch(() => {
          // Swallow — next event will reconcile.
        });
      }
      planStoreNow.fetchPlans().catch(() => {});
      const cleared = typeof d.cleared === "number" ? d.cleared : null;
      const suffix = cleared !== null ? ` (${cleared} task${cleared === 1 ? "" : "s"})` : "";
      planStoreNow.pushToast({
        kind: "info",
        message: `Reset task statuses for ${d.plan_name}${suffix}`,
        ttlMs: 6_000,
      });
      break;
    }
    case "ci_run_dismissed": {
      // CI badge in TaskCard is mirrored from PlanTask.ci. The server
      // marked `dismissed_at` on the row; clear the badge locally so the
      // user sees it disappear immediately instead of waiting for the
      // next plan refetch.
      const d = msg.data;
      deferBehindPlansFetch(() => {
        usePlanStore.getState().clearTaskCi(d.plan_name, d.task_number);
      });
      break;
    }
    case "agent_branch_cleared": {
      // Two emitters with different shapes — see the schema comment.
      // When `agent_id` is present, patch the row in-place; when absent
      // (clear-stale-branches admin path), fall back to matching every
      // agent row that points at that branch.
      const d = msg.data;
      deferBehindAgentsFetch(() => {
        useAgentStore.getState().clearAgentBranch({
          agentId: d.agent_id ?? undefined,
          branch: d.branch,
        });
      });
      break;
    }
    case "hook_event": {
      // Phase 2 reserves a real consumer — today the dashboard doesn't
      // surface raw hook frames anywhere. Logging through the validator
      // is at least a breadcrumb so future bug reports can confirm the
      // event arrived, instead of the silent `break;` that hid drops.
      console.debug("[ws] hook_event accepted (no consumer)", msg.data);
      break;
    }
    case "audit_log":
      // No built-in store mutation — AuditLog refetches its own page
      // via the `subscribeToWsEvents(["audit_log"], …)` API below.
      // Keeping the case explicit so the switch still discriminates
      // every WsMessage variant.
      break;
    case "runner_connected": {
      const d = msg.data;
      useRunnerStore.getState().applyConnected({
        runner_id: d.runner_id,
        runner_name: d.runner_name ?? null,
      });
      break;
    }
    case "runner_disconnected": {
      const d = msg.data;
      useRunnerStore.getState().applyDisconnected({ runner_id: d.runner_id });
      break;
    }
    case "runner_drivers": {
      // Forwards the typed driver inventory to the runner-store so the
      // RunnersPage chip and DriverStatusList reflect the runner's
      // latest auth state without a refetch. The `drivers` field is
      // schema-typed as `unknown` (4.1) — cast at the boundary; the
      // store handler tolerates `undefined` and unfamiliar variants.
      const d = msg.data;
      const drivers = Array.isArray(d.drivers) ? (d.drivers as RunnerDriverInfo[]) : undefined;
      useRunnerStore.getState().applyDriversTouch({ runner_id: d.runner_id, drivers });
      break;
    }
    case "runner_log": {
      // Live tail of one runner's stdout/stderr — pushed into the per-
      // runner ring so `RunnerLogPanel` (on `/runners/{id}`) can render
      // the last N lines without an extra fetch. Best-effort: lines
      // dropped during a reconnect are NOT replayed (the runner has no
      // outbox for this variant).
      const d = msg.data;
      useRunnerStore
        .getState()
        .pushRunnerLog(d.runner_id, { ts: d.ts, level: d.level, line: d.line });
      break;
    }
    case "runner_health": {
      // Periodic per-runner health snapshot (T11.3). Best-effort fan-out
      // from the `RunnerHealth` wire variant; the server has already
      // computed the version-mismatch verdict against `CARGO_PKG_VERSION`,
      // so the store handler just patches the row and lets the chip /
      // panel re-render.
      const d = msg.data;
      useRunnerStore.getState().applyHealth({
        runner_id: d.runner_id,
        outbox_depth: d.outbox_depth,
        ws_reconnects_24h: d.ws_reconnects_24h,
        ci_poll_ms_p50: d.ci_poll_ms_p50 ?? null,
        ci_poll_ms_p99: d.ci_poll_ms_p99 ?? null,
        version_mismatch: d.version_mismatch,
      });
      break;
    }
  }

  // Notify external subscribers AFTER the built-in switch so they see
  // the same event the rest of the dashboard just reacted to.
  // Subscribers are decoupled from the socket lifecycle (audit §4), so
  // they survive reconnects without re-registration.
  for (const sub of wsSubscriptions) {
    if (sub.eventNames.has(msg.type)) {
      try {
        sub.handler(msg);
      } catch (e) {
        console.error("[ws] subscriber threw:", e);
      }
    }
  }
}
