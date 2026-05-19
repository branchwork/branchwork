import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { AgentRail } from "./AgentRail.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

const AGENT_ID = "agent-rail-1";

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: AGENT_ID,
    session_id: "sess-1",
    pid: null,
    parent_agent_id: null,
    plan_name: null,
    task_id: null,
    cwd: "/tmp/wd",
    status: "completed",
    // Non-pty avoids mounting xterm in jsdom (PtyTerminal is exercised
    // separately by manual UI verification — this test is about the
    // wrapper's responsive class set, not the terminal).
    mode: "stream-json",
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
    merge_status: null,
    spawn_error: null,
    ...overrides,
  };
}

function seedStores() {
  useAgentStore.setState({
    agents: [agent()],
    agentsFetched: true,
    fetchAgents: vi.fn().mockResolvedValue(undefined),
    fetchAgentOutput: vi.fn().mockResolvedValue(undefined),
    agentOutput: { [AGENT_ID]: [] },
  });
  usePlanStore.setState({ plans: [] });
  // AgentPanel reads driverCapabilities; the default in the store
  // already returns { supports_cost: false }-style fallbacks for
  // unknown driver names, so no override is required as long as the
  // store is initialised.
  useSettingsStore.setState({ drivers: [] });
}

afterEach(() => {
  cleanup();
  useAgentStore.setState({
    agents: [],
    agentsFetched: false,
    agentOutput: {},
    fetchAgents: useAgentStore.getInitialState().fetchAgents,
    fetchAgentOutput: useAgentStore.getInitialState().fetchAgentOutput,
  });
});

describe("AgentRail", () => {
  it("renders nothing when the URL has no agent id", () => {
    seedStores();
    render(
      <MemoryRouter initialEntries={["/plans/foo"]}>
        <AgentRail />
      </MemoryRouter>,
    );
    expect(screen.queryByTestId("agent-rail")).toBeNull();
  });

  it("renders the wrapper for the deep-link `/agents/:agentId` shape with responsive drawer + rail classes", () => {
    seedStores();
    render(
      <MemoryRouter initialEntries={[`/agents/${AGENT_ID}`]}>
        <AgentRail />
      </MemoryRouter>,
    );

    const rail = screen.getByTestId("agent-rail");
    // <lg: full-screen modal drawer so the panel doesn't overflow on
    // phone/tablet widths.
    expect(rail.className).toMatch(/\bfixed\b/);
    expect(rail.className).toMatch(/\binset-0\b/);
    expect(rail.className).toMatch(/\bz-50\b/);
    expect(rail.className).toMatch(/\bbg-gray-950\b/);
    // ≥lg: snap back into the historical 600px right rail.
    // `\b` doesn't match between `]` and ` ` (both non-word), so the
    // 600px assertion uses a lookahead-or-end check instead.
    expect(rail.className).toMatch(/\blg:static\b/);
    expect(rail.className).toMatch(/\blg:w-\[600px\](?=\s|$)/);
    expect(rail.className).toMatch(/\blg:border-l\b/);
  });

  it("also renders for the `?agent=<id>` cross-route overlay shape", () => {
    seedStores();
    render(
      <MemoryRouter initialEntries={[`/plans/foo?agent=${AGENT_ID}`]}>
        <AgentRail />
      </MemoryRouter>,
    );

    expect(screen.getByTestId("agent-rail")).toBeTruthy();
  });
});
