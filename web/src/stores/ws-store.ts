import { create } from "zustand";
import { usePlanStore } from "./plan-store.js";
import { useAgentStore } from "./agent-store.js";

const MAX_RECONNECT_DELAY = 30_000;
const INITIAL_RECONNECT_DELAY = 2_000;

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
      set({ connected: true, reconnectAttempt: 0 });
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

function handleWsMessage(msg: { type: string; data: unknown }) {
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
    case "task_checked": {
      const d2 = msg.data as { plan_name: string; task_number: string; status: string };
      planStore.patchTaskStatus(d2.plan_name, d2.task_number, d2.status);
      break;
    }
    case "task_status_changed": {
      const d2 = msg.data as { plan_name: string; task_number: string; status: string };
      planStore.patchTaskStatus(d2.plan_name, d2.task_number, d2.status);
      break;
    }
    case "plan_warning": {
      const d = msg.data as { name: string; file: string; error: string };
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
