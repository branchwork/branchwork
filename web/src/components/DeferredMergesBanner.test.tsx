import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { DeferredMergesBanner } from "./PlanBoard.js";
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

describe("DeferredMergesBanner", () => {
  it("does not render when no agents are deferred", () => {
    useAgentStore.setState({ agents: [] });
    const { container } = render(<DeferredMergesBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when deferred agents belong to a different plan", () => {
    useAgentStore.setState({
      agents: [agent({ id: "a-other", plan_name: "other-plan" })],
    });
    const { container } = render(<DeferredMergesBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when matching agents have null merge_status", () => {
    useAgentStore.setState({
      agents: [agent({ merge_status: null })],
    });
    const { container } = render(<DeferredMergesBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders a count and the Flush now button when one agent is deferred", () => {
    useAgentStore.setState({ agents: [agent()] });
    render(<DeferredMergesBanner planName={PLAN} />);
    expect(screen.getByText(/1 task deferred for cadence/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Flush now/ })).toBeTruthy();
  });

  it("pluralizes the count copy with three deferred agents", () => {
    useAgentStore.setState({
      agents: [
        agent({ id: "a1", task_id: "1.1" }),
        agent({ id: "a2", task_id: "1.2" }),
        agent({ id: "a3", task_id: "1.3" }),
      ],
    });
    render(<DeferredMergesBanner planName={PLAN} />);
    expect(screen.getByText(/3 tasks deferred for cadence/i)).toBeTruthy();
  });

  it("opens a confirm dialog that calls flush-merges on confirm", async () => {
    const fetchMock = installFlushMock();
    useAgentStore.setState({
      agents: [
        agent({ id: "a1", task_id: "1.1" }),
        agent({ id: "a2", task_id: "1.2" }),
        agent({ id: "a3", task_id: "1.3" }),
      ],
    });
    render(<DeferredMergesBanner planName={PLAN} />);

    fireEvent.click(screen.getByRole("button", { name: /Flush now/ }));

    // Confirm dialog should announce the count + the cadence-side-effect
    // warning copy verbatim from the brief.
    await waitFor(() => {
      expect(
        screen.getByText(
          /Merge 3 deferred tasks to master now\? This will trigger a build \+ deploy outside the configured cadence\./i,
        ),
      ).toBeTruthy();
    });
    const confirmBtn = screen.getByRole("button", { name: /Merge 3 now/ });
    await act(async () => {
      fireEvent.click(confirmBtn);
    });

    await waitFor(() => {
      expect(fetchMock.calls.some((c) => c.path === `/api/plans/${PLAN}/flush-merges`)).toBe(true);
    });
    const flushCall = fetchMock.calls.find((c) => c.path === `/api/plans/${PLAN}/flush-merges`);
    expect(flushCall?.method).toBe("POST");
  });

  it("closes the confirm dialog when Cancel is clicked without firing flush", async () => {
    const fetchMock = installFlushMock();
    useAgentStore.setState({ agents: [agent()] });
    render(<DeferredMergesBanner planName={PLAN} />);

    fireEvent.click(screen.getByRole("button", { name: /Flush now/ }));
    await waitFor(() => {
      expect(screen.getByRole("button", { name: /Cancel/ })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("button", { name: /Cancel/ }));

    expect(fetchMock.calls.some((c) => c.path.includes("flush-merges"))).toBe(false);
  });
});
