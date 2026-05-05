import { create } from "zustand";
import { fetchJson, postJson, deleteJson, HttpError } from "../api.js";
import { httpErrorBody } from "../lib/error.js";

export interface Agent {
  id: string;
  session_id: string;
  pid: number | null;
  parent_agent_id: string | null;
  plan_name: string | null;
  task_id: string | null;
  cwd: string;
  status: string;
  mode: "pty" | "stream-json";
  prompt: string | null;
  started_at: string;
  finished_at: string | null;
  last_tool: string | null;
  last_activity_at: string | null;
  base_commit: string | null;
  branch: string | null;
  source_branch: string | null;
  cost_usd: number | null;
  driver: string | null;
}

export interface AgentDiff {
  diff: string;
  stat: string;
  files: string[];
  base_commit: string;
  error?: string;
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
  agentDiffs: Record<string, AgentDiff>;
  fetchAgents: () => Promise<void>;
  fetchAgentOutput: (agentId: string) => Promise<void>;
  fetchAgentDiff: (agentId: string) => Promise<void>;
  selectAgent: (agentId: string | null) => void;
  sendMessage: (agentId: string, message: string) => Promise<void>;
  killAgent: (agentId: string) => Promise<void>;
  finishAgent: (agentId: string) => Promise<void>;
  mergeAgentBranch: (
    agentId: string,
    target?: string
  ) => Promise<{ ok?: boolean; error?: string }>;
  fetchMergeTargets: (
    agentId: string
  ) => Promise<{ default: string | null; available: string[] }>;
  discardAgentBranch: (agentId: string) => Promise<{ ok?: boolean; error?: string }>;
  addAgent: (agent: Agent) => void;
  updateAgentStatus: (agentId: string, status: string) => void;
  appendOutput: (agentId: string, line: AgentOutputLine) => void;
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  selectedAgentId: null,
  agentOutput: {},
  agentDiffs: {},

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

  fetchAgentDiff: async (agentId: string) => {
    const diff = await fetchJson<AgentDiff>(`/api/agents/${agentId}/diff`);
    set((s) => ({
      agentDiffs: { ...s.agentDiffs, [agentId]: diff },
    }));
  },

  selectAgent: (agentId) => set({ selectedAgentId: agentId }),

  sendMessage: async (agentId, message) => {
    await postJson(`/api/agents/${agentId}/message`, { message });
  },

  killAgent: async (agentId) => {
    await deleteJson(`/api/agents/${agentId}`);
  },

  finishAgent: async (agentId) => {
    await postJson(`/api/agents/${agentId}/finish`, {});
  },

  mergeAgentBranch: async (agentId, target) => {
    try {
      const body = target ? { into: target } : {};
      const result = await postJson<{ ok?: boolean; error?: string }>(
        `/api/agents/${agentId}/merge`,
        body
      );
      if (result.ok) {
        // Clear branch from local state after merge
        set((s) => ({
          agents: s.agents.map((a) =>
            a.id === agentId ? { ...a, branch: null } : a
          ),
        }));
      }
      return result;
    } catch (e) {
      if (e instanceof HttpError) {
        const body = httpErrorBody<{ error?: string }>(e);
        return { error: body?.error ?? `${e.status} ${e.statusText}` };
      }
      return { error: String(e) };
    }
  },

  fetchMergeTargets: async (agentId) => {
    return fetchJson<{ default: string | null; available: string[] }>(
      `/api/agents/${agentId}/merge-targets`
    );
  },

  discardAgentBranch: async (agentId) => {
    try {
      const result = await postJson<{ ok?: boolean; error?: string }>(
        `/api/agents/${agentId}/discard`,
        {}
      );
      if (result.ok) {
        set((s) => ({
          agents: s.agents.map((a) =>
            a.id === agentId ? { ...a, branch: null } : a
          ),
        }));
      }
      return result;
    } catch (e) {
      return { error: String(e) };
    }
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
