import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, type RenderResult } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { EnsureAgents } from "./EnsureAgents.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-id-default",
    session_id: "sess-default",
    pid: 1234,
    parent_agent_id: null,
    plan_name: null,
    task_id: null,
    cwd: "/tmp/wd",
    status: "running",
    mode: "pty",
    prompt: null,
    started_at: new Date().toISOString(),
    finished_at: null,
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: null,
    source_branch: null,
    cost_usd: null,
    driver: "claude",
    ...overrides,
  };
}

/// Mounts <EnsureAgents/> under both deep-link and overlay shapes. The
/// home route doubles as the host for `?agent=...` so we exercise the
/// search-param branch of `useRouteSelection`.
function renderAt(initialPath: string): RenderResult {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Routes>
        <Route
          path="/agents/:agentId"
          element={
            <EnsureAgents>
              <div>agent-panel-marker</div>
            </EnsureAgents>
          }
        />
        <Route
          path="/"
          element={
            <EnsureAgents>
              <div>agent-panel-marker</div>
            </EnsureAgents>
          }
        />
      </Routes>
    </MemoryRouter>,
  );
}

afterEach(() => {
  cleanup();
  useAgentStore.setState({
    agents: [],
    agentsFetched: false,
    fetchAgents: useAgentStore.getInitialState().fetchAgents,
  });
});

describe("EnsureAgents", () => {
  it("renders loading state and calls fetchAgents when not yet fetched", async () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ agents: [], agentsFetched: false, fetchAgents });

    renderAt("/agents/some-id");

    expect(screen.getByText(/Loading/i)).toBeTruthy();
    await waitFor(() => {
      expect(fetchAgents).toHaveBeenCalledTimes(1);
    });
  });

  it("renders 'Agent not found' on /agents/:unknown after fetch", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({
      agents: [agent({ id: "real-agent" })],
      agentsFetched: true,
      fetchAgents,
    });

    renderAt("/agents/missing-agent");

    expect(screen.getByText(/Agent not found/i)).toBeTruthy();
    // Truncated id matches the first 8 characters of the URL id.
    expect(screen.getByText("missing-")).toBeTruthy();
    expect(fetchAgents).not.toHaveBeenCalled();
  });

  it("also reports not-found for the ?agent= overlay shape", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({
      agents: [agent({ id: "real-agent" })],
      agentsFetched: true,
      fetchAgents,
    });

    renderAt("/?agent=overlay-missing");

    expect(screen.getByText(/Agent not found/i)).toBeTruthy();
    expect(screen.getByText("overlay-")).toBeTruthy();
  });

  it("renders children when the URL agent id matches a known agent", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({
      agents: [agent({ id: "real-agent" })],
      agentsFetched: true,
      fetchAgents,
    });

    renderAt("/agents/real-agent");

    expect(screen.getByText("agent-panel-marker")).toBeTruthy();
    expect(screen.queryByText(/Agent not found/i)).toBeNull();
  });
});
