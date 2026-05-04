import { create } from "zustand";
import { usePlanStore } from "./plan-store.js";
import { useAgentStore } from "./agent-store.js";

const MAX_RECONNECT_DELAY = 30_000;
const INITIAL_RECONNECT_DELAY = 2_000;

function notificationsSupported(): boolean {
  return typeof window !== "undefined" && "Notification" in window;
}

function requestNotificationPermission() {
  if (!notificationsSupported()) return;
  if (Notification.permission === "default") {
    Notification.requestPermission().catch(() => {
      // ignore — user may dismiss or browser may block
    });
  }
}

function notify(title: string, body: string, tag?: string) {
  if (!notificationsSupported()) return;
  if (Notification.permission !== "granted") return;
  try {
    new Notification(title, { body, tag, icon: "/favicon.ico" });
  } catch {
    // some browsers (mobile Safari, etc.) throw on direct construction
  }
}

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
  connect: () => void;
  disconnect: () => void;
}

export const useWsStore = create<WsStore>((set, get) => ({
  connected: false,
  socket: null,
  reconnectAttempt: 0,

  connect: () => {
    const { socket } = get();
    if (socket) return;

    requestNotificationPermission();

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
      set({ connected: true, reconnectAttempt: 0 });
      if (wasReconnect) {
        // Refetch all data — events during the disconnect were lost
        usePlanStore.getState().fetchPlans();
        useAgentStore.getState().fetchAgents();
      }
    };

    ws.onerror = (ev) => {
      console.error("[ws] WebSocket error:", ev);
    };

    ws.onclose = () => {
      set({ connected: false, socket: null });
      scheduleReconnect(get);
    };

    ws.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        handleWsMessage(msg);
      } catch {
        // ignore non-JSON
      }
    };

    set({ socket: ws });
  },

  disconnect: () => {
    const { socket } = get();
    if (socket) {
      socket.close();
      set({ socket: null, connected: false, reconnectAttempt: 0 });
    }
  },
}));

function scheduleReconnect(get: () => WsStore) {
  const attempt = get().reconnectAttempt;
  const delay = Math.min(
    INITIAL_RECONNECT_DELAY * Math.pow(2, attempt),
    MAX_RECONNECT_DELAY
  );
  useWsStore.setState({ reconnectAttempt: attempt + 1 });
  setTimeout(() => get().connect(), delay);
}

let planRefreshTimer: ReturnType<typeof setTimeout> | null = null;

// Exported for unit testing. The function is otherwise reached only via
// the `ws.onmessage` handler installed in `connect()`.
export function handleWsMessage(msg: { type: string; data: unknown }) {
  const planStore = usePlanStore.getState();
  const agentStore = useAgentStore.getState();

  switch (msg.type) {
    case "plan_updated": {
      const d = msg.data as { action: string; plan?: unknown };
      if (d.plan) {
        planStore.updatePlan(d.plan as Parameters<typeof planStore.updatePlan>[0]);
      }
      // Debounce plan list refresh to avoid flickering
      if (planRefreshTimer) clearTimeout(planRefreshTimer);
      planRefreshTimer = setTimeout(() => {
        planStore.fetchPlans();
        planRefreshTimer = null;
      }, 2000);
      break;
    }
    case "agent_started": {
      agentStore.fetchAgents();
      break;
    }
    case "agent_output": {
      const d = msg.data as { agent_id: string; message_type: string; content: unknown };
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
      const d = msg.data as { id: string; status: string };
      const agent = agentStore.agents.find((a) => a.id === d.id);
      const taskLabel = agent
        ? lookupTaskTitle(agent.plan_name, agent.task_id)
        : `Agent ${d.id.slice(0, 8)}`;
      notify(`${taskLabel} — ${d.status}`, "Agent finished", `agent-${d.id}`);
      agentStore.updateAgentStatus(d.id, d.status);
      // Refetch to pick up fields that may have been updated after initial
      // insert (branch, cost, base_commit) — ensures the task card's Merge
      // button shows up immediately when the agent finishes
      agentStore.fetchAgents();
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
      const d = msg.data as {
        plan: string;
        task: string;
        sha?: string | null;
        target?: string | null;
      };
      const taskLabel = lookupTaskTitle(d.plan, d.task);
      const targetSuffix = d.target ? ` → ${d.target}` : "";
      notify(
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
    case "auto_mode_state": {
      // Live pill feed: every transition from the auto-mode state machine
      // (merging → awaiting_ci → advancing|paused) carries a `state` label
      // here. The pill reads this map; transient labels overwrite each
      // other. Advancing is treated as idle and clears the runtime so the
      // pill disappears between tasks.
      const d = msg.data as {
        plan: string;
        task?: string | null;
        state: string;
        sha?: string | null;
        reason?: string | null;
      };
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
      const d = msg.data as {
        plan: string;
        task: string;
        fix_task: string;
        fix_agent_id: string;
        attempt: number;
        ci_run_id?: string | null;
      };
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
      const d = msg.data as {
        plan: string;
        task?: string | null;
        reason: string;
        target?: string | null;
      };
      planStore.patchPlanConfig(d.plan, { pausedReason: d.reason });
      planStore.setAutoModeRuntime(d.plan, {
        state: "paused",
        task: d.task ?? null,
        reason: d.reason,
      });
      const taskLabel = d.task ? lookupTaskTitle(d.plan, d.task) : "plan";
      notify(
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
      const d = msg.data as { plan: string };
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
      const d2 = msg.data as { plan_name: string; task_number: string; status: string };
      planStore.patchTaskStatus(d2.plan_name, d2.task_number, d2.status);
      break;
    }
    case "plan_checked": {
      const d = msg.data as {
        plan_name: string;
        verdict: string;
        reason?: string;
        agent_id?: string;
      };
      planStore.patchPlanVerdict(d.plan_name, {
        verdict: d.verdict,
        reason: d.reason ?? null,
        agentId: d.agent_id ?? null,
        checkedAt: new Date().toISOString(),
      });
      notify(
        `Plan check: ${d.verdict}`,
        d.reason ? `${d.plan_name} — ${d.reason}` : d.plan_name,
        `plan-checked-${d.plan_name}`,
      );
      break;
    }
    case "task_status_changed": {
      const d2 = msg.data as { plan_name: string; task_number: string; status: string };
      if (d2.status === "completed" || d2.status === "failed") {
        notify(
          `${lookupTaskTitle(d2.plan_name, d2.task_number)} — ${d2.status}`,
          d2.plan_name,
          `task-${d2.plan_name}-${d2.task_number}`
        );
      }
      planStore.patchTaskStatus(d2.plan_name, d2.task_number, d2.status);
      // Debounced authoritative refetch — guarantees convergence to server
      // truth (doneCount, statuses for non-selected plans, MCP/agent-driven
      // transitions) even when the signed-delta in patchTaskStatus can't see
      // the prior value. Shares planRefreshTimer so bursts collapse with
      // plan_updated events.
      if (planRefreshTimer) clearTimeout(planRefreshTimer);
      planRefreshTimer = setTimeout(() => {
        planStore.fetchPlans();
        planRefreshTimer = null;
      }, 2000);
      break;
    }
    case "ci_status_changed": {
      const d = msg.data as {
        id: number;
        plan_name: string;
        task_number: string;
        status: string;
        conclusion?: string | null;
        run_url?: string | null;
        commit_sha?: string | null;
      };
      planStore.patchTaskCi(d.plan_name, d.task_number, {
        id: d.id,
        status: d.status as
          | "pending" | "running" | "success" | "failure" | "cancelled" | "unknown",
        conclusion: d.conclusion ?? null,
        runUrl: d.run_url ?? null,
        commitSha: d.commit_sha ?? null,
        updatedAt: new Date().toISOString(),
      });
      if (d.status === "success" || d.status === "failure") {
        notify(
          `CI ${d.status}: ${lookupTaskTitle(d.plan_name, d.task_number)}`,
          d.run_url ?? d.plan_name,
          `ci-${d.plan_name}-${d.task_number}`
        );
      }
      break;
    }
    case "plan_warning": {
      const d = msg.data as { name: string; file: string; error: string };
      notify(`Plan error: ${d.name}`, d.error, `plan-warning-${d.name}`);
      planStore.addWarning({
        name: d.name,
        file: d.file,
        error: d.error,
        timestamp: Date.now(),
      });
      break;
    }
    case "hook_event":
      // Could display in an activity feed
      break;
  }
}
