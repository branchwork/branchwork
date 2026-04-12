import { create } from "zustand";
import { fetchJson, postJson, deleteJson } from "../api.js";

export interface Agent {
  id: string;
  session_id: string;
  pid: number | null;
  parent_agent_id: string | null;
  plan_name: string | null;
  task_id: string | null;
  cwd: string;
  status: string;
  prompt: string | null;
  started_at: string;
  finished_at: string | null;
  last_tool: string | null;
  last_activity_at: string | null;
}

export interface AgentOutputLine {
  id: number;
  agent_id: string;
  message_type: string;
  content: string;
  timestamp: string;
}

interface AgentStore {
  agents: Agent[];
  selectedAgentId: string | null;
  agentOutput: Record<string, AgentOutputLine[]>;
  fetchAgents: () => Promise<void>;
  fetchAgentOutput: (agentId: string) => Promise<void>;
  selectAgent: (agentId: string | null) => void;
  sendMessage: (agentId: string, message: string) => Promise<void>;
  killAgent: (agentId: string) => Promise<void>;
  addAgent: (agent: Agent) => void;
  updateAgentStatus: (agentId: string, status: string) => void;
  appendOutput: (agentId: string, line: AgentOutputLine) => void;
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  selectedAgentId: null,
  agentOutput: {},

  fetchAgents: async () => {
    const agents = await fetchJson<Agent[]>("/api/agents");
    set({ agents });
  },

  fetchAgentOutput: async (agentId: string) => {
    const output = await fetchJson<AgentOutputLine[]>(
      `/api/agents/${agentId}/output`
    );
    set((s) => ({
      agentOutput: { ...s.agentOutput, [agentId]: output },
    }));
  },

  selectAgent: (agentId) => set({ selectedAgentId: agentId }),

  sendMessage: async (agentId, message) => {
    await postJson(`/api/agents/${agentId}/message`, { message });
  },

  killAgent: async (agentId) => {
    await deleteJson(`/api/agents/${agentId}`);
  },

  addAgent: (agent) =>
    set((s) => ({ agents: [agent, ...s.agents] })),

  updateAgentStatus: (agentId, status) =>
    set((s) => ({
      agents: s.agents.map((a) =>
        a.id === agentId ? { ...a, status } : a
      ),
    })),

  appendOutput: (agentId, line) =>
    set((s) => ({
      agentOutput: {
        ...s.agentOutput,
        [agentId]: [...(s.agentOutput[agentId] ?? []), line],
      },
    })),
}));
