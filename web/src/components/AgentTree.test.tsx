import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import axe from "axe-core";
import { AgentTree } from "./AgentTree.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-tree-1",
    session_id: "sess-1",
    pid: null,
    parent_agent_id: null,
    plan_name: "alpha",
    task_id: "1.1",
    cwd: "/tmp/wd",
    status: "running",
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
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
});

describe("AgentTree accessibility", () => {
  it("animated status dot exposes its state to screen readers via sr-only", () => {
    useAgentStore.setState({
      agents: [agent({ status: "running" })],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    render(
      <MemoryRouter>
        <AgentTree />
      </MemoryRouter>
    );

    expect(screen.getByText(/Status:\s*running/i)).toBeTruthy();
  });

  it("axe-core reports zero icon-button-name violations on the rendered tree", async () => {
    useAgentStore.setState({
      agents: [agent({ status: "running" }), agent({ id: "b", status: "completed" })],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    const { container } = render(
      <MemoryRouter>
        <AgentTree />
      </MemoryRouter>
    );

    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });

  it("axe-core reports zero violations on the empty-state hero", async () => {
    useAgentStore.setState({
      agents: [],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    const { container } = render(
      <MemoryRouter>
        <AgentTree />
      </MemoryRouter>
    );

    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
