import { create } from "zustand";
import { usePlanStore } from "./plan-store.js";
import { useAgentStore } from "./agent-store.js";

interface WsStore {
  connected: boolean;
  socket: WebSocket | null;
  connect: () => void;
  disconnect: () => void;
}

export const useWsStore = create<WsStore>((set, get) => ({
  connected: false,
  socket: null,

  connect: () => {
    const { socket } = get();
    if (socket) return;

    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${protocol}//${window.location.host}/ws`);

    ws.onopen = () => set({ connected: true });
    ws.onclose = () => {
      set({ connected: false, socket: null });
      // Auto-reconnect after 2s
      setTimeout(() => get().connect(), 2000);
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
      set({ socket: null, connected: false });
    }
  },
}));

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
      break;
    }
    case "task_checked": {
      // Agent finished checking a task — refresh the plan
      planStore.fetchPlans();
      const d2 = msg.data as { plan_name: string };
      const sel = planStore.selectedPlan;
      if (sel?.name === d2.plan_name) {
        planStore.selectPlan(d2.plan_name);
      }
      break;
    }
    case "hook_event":
      // Could display in an activity feed
      break;
  }
}
