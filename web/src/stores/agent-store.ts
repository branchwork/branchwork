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
  /// `false` until the first successful `fetchAgents()` resolves.
  /// Used by `<EnsureAgents/>` so it can show a loading state on first
  /// nav and only surface "agent not found" once the list is known.
  agentsFetched: boolean;
  /// ms-since-epoch when the last `fetchAgents()` resolved. ws-store reads
  /// this to debounce WS-triggered refetches (e.g. an `agent_started` burst
  /// during plan auto-advance) — refetches scheduled within
  /// `WS_REFETCH_DEBOUNCE_MS` of the last fetch are skipped because the
  /// in-flight or just-resolved fetch already carries the new row.
  lastAgentsFetchedAt: number | null;
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
  /// Set `branch = null` on an agent row matched by id (when the WS
  /// frame carries `agent_id`) or by branch name (when the boot-sweep
  /// path emits only `{branch}`). Driven by `agent_branch_cleared`.
  /// No-op if no row matches — the next `fetchAgents()` reconciles.
  clearAgentBranch: (match: { agentId?: string; branch?: string }) => void;
  appendOutput: (agentId: string, line: AgentOutputLine) => void;
}

/// Module-level handle to the single in-flight `fetchAgents()` round trip.
/// Coalesces concurrent callers so `agent_started` / `agent_stopped`
/// bursts during auto-advance don't fan out into N parallel /api/agents
/// requests racing each other (audit §4). ws-store reads this so it can
/// `await` the in-flight fetch before applying optimistic patches.
let inFlightAgentsFetch: Promise<void> | null = null;

/// Read-only accessor for the in-flight fetchAgents promise.
export function getInFlightAgentsFetch(): Promise<void> | null {
  return inFlightAgentsFetch;
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: [],
  selectedAgentId: null,
  agentOutput: {},
  agentDiffs: {},
  agentsFetched: false,
  lastAgentsFetchedAt: null,

  fetchAgents: () => {
    if (inFlightAgentsFetch) return inFlightAgentsFetch;
    const promise = (async () => {
      const agents = await fetchJson<Agent[]>("/api/agents");
      set({
        agents,
        agentsFetched: true,
        lastAgentsFetchedAt: Date.now(),
      });
    })();
    inFlightAgentsFetch = promise;
    promise.finally(() => {
      if (inFlightAgentsFetch === promise) inFlightAgentsFetch = null;
    });
    return promise;
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

  clearAgentBranch: ({ agentId, branch }) =>
    set((s) => ({
      agents: s.agents.map((a) => {
        if (agentId && a.id === agentId) return { ...a, branch: null };
        if (!agentId && branch && a.branch === branch) {
          return { ...a, branch: null };
        }
        return a;
      }),
    })),

  appendOutput: (agentId, line) =>
    set((s) => ({
      agentOutput: {
        ...s.agentOutput,
        [agentId]: [...(s.agentOutput[agentId] ?? []), line],
      },
    })),
}));
