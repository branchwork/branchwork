import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { AwaitingCadenceChip } from "./PlanBoard.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";

const PLAN = "p1";

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-default",
    session_id: "sess-default",
    pid: null,
    parent_agent_id: null,
    plan_name: PLAN,
    task_id: "1.1",
    cwd: "/tmp/wd",
    status: "completed",
    mode: "pty",
    prompt: null,
    started_at: new Date().toISOString(),
    finished_at: new Date().toISOString(),
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: `branchwork/${PLAN}/1.1`,
    source_branch: "master",
    cost_usd: null,
    driver: "claude",
    merge_status: "deferred_for_cadence",
    spawn_error: null,
    ...overrides,
  };
}

interface FlushCall {
  path: string;
  method: string;
}

function installFlushMock(): {
  calls: FlushCall[];
  setResponse: (body: Record<string, unknown>, status?: number) => void;
} {
  const calls: FlushCall[] = [];
  let responseBody: Record<string, unknown> = {
    ok: true,
    plan: PLAN,
    merged: [{ agentId: "a-1", taskId: "1.1" }],
    count: 1,
    paused: false,
    ciTriggered: true,
    message: "Flushed 1 deferred task to master.",
  };
  let status = 200;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      calls.push({ path, method: init?.method ?? "GET" });
      return new Response(JSON.stringify(responseBody), {
        status,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
  return {
    calls,
    setResponse: (body, s = 200) => {
      responseBody = body;
      status = s;
    },
  };
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn());
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  useAgentStore.setState({ agents: [], selectedAgentId: null });
});

describe("AwaitingCadenceChip", () => {
  it("does not render when no agents are deferred", () => {
    useAgentStore.setState({ agents: [] });
    const { container } = render(<AwaitingCadenceChip planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when deferred agents belong to a different plan", () => {
    useAgentStore.setState({
      agents: [agent({ id: "a-other", plan_name: "other-plan" })],
    });
    const { container } = render(<AwaitingCadenceChip planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when matching agents have null merge_status", () => {
    useAgentStore.setState({
      agents: [agent({ id: "a-cleared", merge_status: null })],
    });
    const { container } = render(<AwaitingCadenceChip planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders count (singular) and clickable Flush now button when one task is deferred", () => {
    useAgentStore.setState({ agents: [agent()] });
    render(<AwaitingCadenceChip planName={PLAN} />);
    expect(screen.getByText(/1 task awaiting cadence/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Flush now/i })).toBeTruthy();
  });

  it("renders count (plural) when three tasks are deferred", () => {
    useAgentStore.setState({
      agents: [
        agent({ id: "a-1", task_id: "1.1" }),
        agent({ id: "a-2", task_id: "1.2" }),
        agent({ id: "a-3", task_id: "1.3" }),
      ],
    });
    render(<AwaitingCadenceChip planName={PLAN} />);
    expect(screen.getByText(/3 tasks awaiting cadence/i)).toBeTruthy();
  });

  it("only counts agents belonging to this plan", () => {
    useAgentStore.setState({
      agents: [
        agent({ id: "a-1", task_id: "1.1" }),
        agent({ id: "a-2", task_id: "1.2" }),
        agent({ id: "a-other", plan_name: "other-plan", task_id: "1.1" }),
      ],
    });
    render(<AwaitingCadenceChip planName={PLAN} />);
    expect(screen.getByText(/2 tasks awaiting cadence/i)).toBeTruthy();
  });

  it("opens confirm dialog on Flush now click and POSTs to flush-merges on confirm", async () => {
    const { calls } = installFlushMock();
    useAgentStore.setState({ agents: [agent()] });
    render(<AwaitingCadenceChip planName={PLAN} />);

    fireEvent.click(screen.getByRole("button", { name: /Flush now/i }));
    // ConfirmDialog opens with the deferred-merge title.
    expect(screen.getByText(/Flush deferred merges/i)).toBeTruthy();

    // Confirm button label includes the count.
    const confirm = screen.getByRole("button", { name: /Merge 1 now/i });
    fireEvent.click(confirm);

    await waitFor(() => {
      const flushCall = calls.find((c) => c.path.endsWith("/flush-merges"));
      expect(flushCall).toBeTruthy();
      expect(flushCall?.method).toBe("POST");
    });
  });

  it("disappears once agent merge_status flips off deferred (banner fates with agent list)", async () => {
    const { setResponse } = installFlushMock();
    setResponse({
      ok: true,
      plan: PLAN,
      merged: [{ agentId: "agent-default", taskId: "1.1" }],
      count: 1,
      paused: false,
      ciTriggered: true,
      message: "Flushed 1 deferred task to master.",
    });
    useAgentStore.setState({ agents: [agent()] });
    render(<AwaitingCadenceChip planName={PLAN} />);
    expect(screen.getByText(/1 task awaiting cadence/i)).toBeTruthy();

    // The real flow drives merge_status -> null via per-merge `auto_mode_merged`
    // WS events that refresh the agent list. Here we simulate the post-flush
    // state directly: agent-store update with merge_status=null.
    act(() => {
      useAgentStore.setState({ agents: [agent({ merge_status: null })] });
    });
    expect(screen.queryByText(/awaiting cadence/i)).toBeNull();
  });

  it("cancel dismisses the confirm dialog without POSTing", () => {
    const { calls } = installFlushMock();
    useAgentStore.setState({ agents: [agent()] });
    render(<AwaitingCadenceChip planName={PLAN} />);

    fireEvent.click(screen.getByRole("button", { name: /Flush now/i }));
    expect(screen.getByText(/Flush deferred merges/i)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/i }));
    expect(screen.queryByText(/Flush deferred merges/i)).toBeNull();
    expect(calls.find((c) => c.path.endsWith("/flush-merges"))).toBeUndefined();
  });
});
