import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  MAX_OUTPUT_LINES,
  getInFlightAgentsFetch,
  useAgentStore,
  type Agent,
  type AgentOutputLine,
} from "./agent-store.js";
import { handleWsMessage } from "./ws-store.js";
import { useAuthStore } from "./auth-store.js";

interface FetchResult {
  status: number;
  body?: unknown;
  statusText?: string;
}

type FetchHandler = (url: string, method: string, body: unknown) => FetchResult;

interface CallLog {
  url: string;
  method: string;
  body: unknown;
}

// Same-shape helper as src/api.test.ts: each call is logged so tests can
// assert URL+method+body, and the handler picks the response. Restored by
// `vi.unstubAllGlobals` in afterEach.
function installFetchMock(handler: FetchHandler) {
  const calls: CallLog[] = [];
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const method = init?.method ?? "GET";
    const rawBody = init?.body;
    const body =
      typeof rawBody === "string" && rawBody.length > 0 ? JSON.parse(rawBody) : undefined;
    calls.push({ url, method, body });
    const result = handler(url, method, body);
    return new Response(result.body !== undefined ? JSON.stringify(result.body) : null, {
      status: result.status,
      statusText: result.statusText,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

beforeEach(() => {
  // Seed a non-null user so the api.ts 401 interceptor doesn't try to call
  // logout() during tests that simulate failures (we never test 401 paths
  // here — auth-store covers that). Keeps fetch failures from cascading
  // into resetAllStores.
  useAuthStore.setState({
    user: { id: "u1", email: "u@example.com" },
    error: null,
    loading: false,
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  useAgentStore.getState().reset();
  useAuthStore.setState({ user: null, error: null, loading: false });
});

function makeLine(seq: number): AgentOutputLine {
  return {
    id: seq,
    agent_id: "a1",
    message_type: "stdout",
    content: `line ${seq}`,
    timestamp: "2026-05-06T00:00:00Z",
  };
}

function makeAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-1",
    session_id: "session-1",
    pid: null,
    parent_agent_id: null,
    plan_name: null,
    task_id: null,
    cwd: "/tmp",
    status: "running",
    mode: "pty",
    prompt: null,
    started_at: "2026-04-12T00:00:00Z",
    finished_at: null,
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: null,
    source_branch: null,
    cost_usd: null,
    driver: null,
    merge_status: null,
    spawn_error: null,
    ...overrides,
  };
}

describe("agent-store initial state", () => {
  it("starts empty with the expected slice shape", () => {
    const s = useAgentStore.getState();
    expect(s.agents).toEqual([]);
    expect(s.selectedAgentId).toBeNull();
    expect(s.agentOutput).toEqual({});
    expect(s.agentDiffs).toEqual({});
    expect(s.agentsFetched).toBe(false);
    expect(s.lastAgentsFetchedAt).toBeNull();
    expect(getInFlightAgentsFetch()).toBeNull();
  });
});

describe("agent-store mutations", () => {
  it("selectAgent flips selectedAgentId (and clears with null)", () => {
    const { selectAgent } = useAgentStore.getState();
    selectAgent("agent-X");
    expect(useAgentStore.getState().selectedAgentId).toBe("agent-X");
    selectAgent(null);
    expect(useAgentStore.getState().selectedAgentId).toBeNull();
  });

  it("addAgent prepends the row so newest is first", () => {
    const { addAgent } = useAgentStore.getState();
    useAgentStore.setState({ agents: [makeAgent({ id: "older" })] });
    addAgent(makeAgent({ id: "newer" }));
    const ids = useAgentStore.getState().agents.map((a) => a.id);
    expect(ids).toEqual(["newer", "older"]);
  });

  it("updateAgentStatus patches only the matching row", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a", status: "running" }),
        makeAgent({ id: "b", status: "running" }),
      ],
    });
    useAgentStore.getState().updateAgentStatus("a", "completed");
    const rows = useAgentStore.getState().agents;
    expect(rows.find((r) => r.id === "a")?.status).toBe("completed");
    expect(rows.find((r) => r.id === "b")?.status).toBe("running");
  });

  it("clearAgentBranch by agentId nulls only that row's branch", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a", branch: "branchwork/p/1.1" }),
        makeAgent({ id: "b", branch: "branchwork/p/1.1" }),
      ],
    });
    useAgentStore.getState().clearAgentBranch({ agentId: "a" });
    const rows = useAgentStore.getState().agents;
    expect(rows.find((r) => r.id === "a")?.branch).toBeNull();
    expect(rows.find((r) => r.id === "b")?.branch).toBe("branchwork/p/1.1");
  });

  it("clearAgentBranch by branch (no agent_id, boot-sweep path) nulls every matching row", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a", branch: "branchwork/p/1.1" }),
        makeAgent({ id: "b", branch: "branchwork/p/1.1" }),
        makeAgent({ id: "c", branch: "branchwork/p/2.1" }),
      ],
    });
    useAgentStore.getState().clearAgentBranch({ branch: "branchwork/p/1.1" });
    const rows = useAgentStore.getState().agents;
    expect(rows.find((r) => r.id === "a")?.branch).toBeNull();
    expect(rows.find((r) => r.id === "b")?.branch).toBeNull();
    // Untouched: still on its own branch.
    expect(rows.find((r) => r.id === "c")?.branch).toBe("branchwork/p/2.1");
  });

  it("clearAgentBranch with no match is a no-op", () => {
    const seed = [makeAgent({ id: "a", branch: "branchwork/p/1.1" })];
    useAgentStore.setState({ agents: seed });
    useAgentStore.getState().clearAgentBranch({ agentId: "nope" });
    expect(useAgentStore.getState().agents[0].branch).toBe("branchwork/p/1.1");
  });
});

describe("agent-store appendOutput cap", () => {
  it("caps at MAX_OUTPUT_LINES and evicts the oldest on overflow (audit §10)", () => {
    const { appendOutput } = useAgentStore.getState();
    const total = 10_000;
    for (let i = 1; i <= total; i++) {
      appendOutput("a1", makeLine(i));
    }
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf).toBeDefined();
    expect(buf.length).toBe(MAX_OUTPUT_LINES);
    expect(buf[0].id).toBe(total - MAX_OUTPUT_LINES + 1);
    expect(buf[buf.length - 1].id).toBe(total);
  });

  it("leaves the buffer unchanged below the cap", () => {
    const { appendOutput } = useAgentStore.getState();
    for (let i = 1; i <= 100; i++) {
      appendOutput("a1", makeLine(i));
    }
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf.length).toBe(100);
    expect(buf[0].id).toBe(1);
    expect(buf[99].id).toBe(100);
  });

  it("trims from existing oversize buffers (e.g. seeded by fetchAgentOutput)", () => {
    const seeded: AgentOutputLine[] = [];
    for (let i = 1; i <= MAX_OUTPUT_LINES + 200; i++) {
      seeded.push(makeLine(i));
    }
    useAgentStore.setState({ agentOutput: { a1: seeded } });
    useAgentStore.getState().appendOutput("a1", makeLine(99_999));
    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf.length).toBe(MAX_OUTPUT_LINES);
    expect(buf[buf.length - 1].id).toBe(99_999);
  });
});

describe("agent-store fetchers (stubbed fetch)", () => {
  it("fetchAgents fills agents, sets agentsFetched, stamps lastAgentsFetchedAt", async () => {
    const before = Date.now();
    installFetchMock(() => ({
      status: 200,
      body: [makeAgent({ id: "x" }), makeAgent({ id: "y" })],
    }));

    await useAgentStore.getState().fetchAgents();

    const s = useAgentStore.getState();
    expect(s.agents.map((a) => a.id)).toEqual(["x", "y"]);
    expect(s.agentsFetched).toBe(true);
    expect(s.lastAgentsFetchedAt).not.toBeNull();
    expect(s.lastAgentsFetchedAt!).toBeGreaterThanOrEqual(before);
  });

  it("fetchAgents coalesces concurrent callers onto a single round trip", async () => {
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: [makeAgent({ id: "x" })],
    }));

    const a = useAgentStore.getState().fetchAgents();
    const b = useAgentStore.getState().fetchAgents();
    // Same in-flight handle is shared (audit §4).
    expect(a).toBe(b);
    expect(getInFlightAgentsFetch()).toBe(a);
    await Promise.all([a, b]);
    expect(calls.filter((c) => c.url === "/api/agents")).toHaveLength(1);
    // Handle is cleared after resolution so a future call starts fresh.
    expect(getInFlightAgentsFetch()).toBeNull();
  });

  it("fetchAgentOutput populates agentOutput[id]", async () => {
    const lines = [makeLine(1), makeLine(2)];
    const { calls } = installFetchMock(() => ({ status: 200, body: lines }));

    await useAgentStore.getState().fetchAgentOutput("a1");

    expect(useAgentStore.getState().agentOutput["a1"]).toEqual(lines);
    expect(calls[0]).toEqual({
      url: "/api/agents/a1/output",
      method: "GET",
      body: undefined,
    });
  });

  it("fetchAgentDiff populates agentDiffs[id]", async () => {
    const diff = {
      diff: "+ added\n",
      stat: " 1 file changed",
      files: ["src/x.ts"],
      base_commit: "abc",
    };
    const { calls } = installFetchMock(() => ({ status: 200, body: diff }));

    await useAgentStore.getState().fetchAgentDiff("a1");

    expect(useAgentStore.getState().agentDiffs["a1"]).toEqual(diff);
    expect(calls[0].url).toBe("/api/agents/a1/diff");
  });

  it("sendMessage POSTs /api/agents/{id}/message with the body", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useAgentStore.getState().sendMessage("a1", "hello");

    expect(calls).toEqual([
      { url: "/api/agents/a1/message", method: "POST", body: { message: "hello" } },
    ]);
  });

  it("killAgent DELETEs /api/agents/{id}", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useAgentStore.getState().killAgent("a1");

    expect(calls).toEqual([{ url: "/api/agents/a1", method: "DELETE", body: undefined }]);
  });

  it("finishAgent POSTs /api/agents/{id}/finish", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useAgentStore.getState().finishAgent("a1");

    expect(calls).toEqual([{ url: "/api/agents/a1/finish", method: "POST", body: {} }]);
  });

  it("mergeAgentBranch (success) clears branch locally and returns ok:true", async () => {
    useAgentStore.setState({
      agents: [makeAgent({ id: "a1", branch: "branchwork/p/1.1" })],
    });
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: { ok: true },
    }));

    const result = await useAgentStore.getState().mergeAgentBranch("a1", "main");

    expect(result).toEqual({ ok: true });
    expect(useAgentStore.getState().agents[0].branch).toBeNull();
    expect(calls[0]).toEqual({
      url: "/api/agents/a1/merge",
      method: "POST",
      body: { into: "main" },
    });
  });

  it("mergeAgentBranch (HttpError) returns the server's error string without throwing", async () => {
    useAgentStore.setState({
      agents: [makeAgent({ id: "a1", branch: "branchwork/p/1.1" })],
    });
    installFetchMock(() => ({
      status: 409,
      body: { error: "merge_conflict" },
      statusText: "Conflict",
    }));

    const result = await useAgentStore.getState().mergeAgentBranch("a1");

    expect(result).toEqual({ error: "merge_conflict" });
    // Branch is NOT cleared on failure — only success clears it.
    expect(useAgentStore.getState().agents[0].branch).toBe("branchwork/p/1.1");
  });

  it("fetchMergeTargets returns the server's {default, available} response", async () => {
    const payload = { default: "main", available: ["main", "develop"] };
    const { calls } = installFetchMock(() => ({ status: 200, body: payload }));

    const result = await useAgentStore.getState().fetchMergeTargets("a1");

    expect(result).toEqual(payload);
    expect(calls[0].url).toBe("/api/agents/a1/merge-targets");
  });

  it("discardAgentBranch (success) clears branch locally and returns ok:true", async () => {
    useAgentStore.setState({
      agents: [makeAgent({ id: "a1", branch: "branchwork/p/1.1" })],
    });
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: { ok: true },
    }));

    const result = await useAgentStore.getState().discardAgentBranch("a1");

    expect(result).toEqual({ ok: true });
    expect(useAgentStore.getState().agents[0].branch).toBeNull();
    expect(calls[0]).toEqual({
      url: "/api/agents/a1/discard",
      method: "POST",
      body: {},
    });
  });
});

describe("agent-store reset()", () => {
  it("drops every slice back to the initial shape and clears the in-flight handle", () => {
    useAgentStore.setState({
      agents: [makeAgent({ id: "x" })],
      selectedAgentId: "x",
      agentOutput: { x: [makeLine(1)] },
      agentDiffs: { x: { diff: "d", stat: "s", files: [], base_commit: "c" } },
      agentsFetched: true,
      lastAgentsFetchedAt: 12345,
    });

    useAgentStore.getState().reset();

    const s = useAgentStore.getState();
    expect(s.agents).toEqual([]);
    expect(s.selectedAgentId).toBeNull();
    expect(s.agentOutput).toEqual({});
    expect(s.agentDiffs).toEqual({});
    expect(s.agentsFetched).toBe(false);
    expect(s.lastAgentsFetchedAt).toBeNull();
    expect(getInFlightAgentsFetch()).toBeNull();
  });
});

describe("agent-store WS-driven patches (via handleWsMessage)", () => {
  it("agent_started -> calls fetchAgents", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({ type: "agent_started", data: {} });

    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("agent_output -> appends a line to agentOutput[agent_id]", () => {
    handleWsMessage({
      type: "agent_output",
      data: { agent_id: "a1", message_type: "stdout", content: "tick" },
    });

    const buf = useAgentStore.getState().agentOutput["a1"];
    expect(buf).toBeDefined();
    expect(buf.length).toBe(1);
    expect(buf[0].agent_id).toBe("a1");
    expect(buf[0].content).toBe("tick");
    expect(buf[0].message_type).toBe("stdout");
  });

  it("agent_branch_cleared (boot-sweep shape: branch only) nulls every matching agent's branch", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a", branch: "branchwork/p/1.1" }),
        makeAgent({ id: "b", branch: "branchwork/p/2.1" }),
      ],
    });

    handleWsMessage({
      type: "agent_branch_cleared",
      data: { branch: "branchwork/p/1.1", reason: "stale" },
    });

    const rows = useAgentStore.getState().agents;
    expect(rows.find((r) => r.id === "a")?.branch).toBeNull();
    expect(rows.find((r) => r.id === "b")?.branch).toBe("branchwork/p/2.1");
  });

  it("agent_branch_cleared (api/plans clear-stale path with agent_id) targets exactly that row", () => {
    useAgentStore.setState({
      agents: [
        makeAgent({ id: "a", branch: "branchwork/p/1.1" }),
        makeAgent({ id: "b", branch: "branchwork/p/1.1" }),
      ],
    });

    handleWsMessage({
      type: "agent_branch_cleared",
      data: { agent_id: "a", branch: "branchwork/p/1.1" },
    });

    const rows = useAgentStore.getState().agents;
    expect(rows.find((r) => r.id === "a")?.branch).toBeNull();
    expect(rows.find((r) => r.id === "b")?.branch).toBe("branchwork/p/1.1");
  });
});
