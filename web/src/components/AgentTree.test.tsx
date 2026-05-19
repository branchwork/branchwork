import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import axe from "axe-core";
import { AgentTree } from "./AgentTree.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

function renderTree() {
  return render(
    <MemoryRouter>
      <AgentTree />
    </MemoryRouter>,
  );
}

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
    merge_status: null,
  spawn_error: null,
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
      </MemoryRouter>,
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
      </MemoryRouter>,
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
      </MemoryRouter>,
    );

    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});

describe("AgentTree empty state", () => {
  it("renders the 'No agents yet' hero when no agents exist", () => {
    useAgentStore.setState({
      agents: [],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    renderTree();

    expect(screen.getByText("No agents yet")).toBeTruthy();
    expect(
      screen.getByText(/Start a task from the Plan Board or wait for hook events/i),
    ).toBeTruthy();
    // Filter strip is hidden in the empty state — there's nothing to filter.
    expect(screen.queryByText("Status")).toBeNull();
  });

  it("calls fetchAgents on mount (loading-state seed)", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ agents: [], fetchAgents });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    renderTree();
    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("shows the active-/total-counts header with zero agents", () => {
    useAgentStore.setState({
      agents: [],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    renderTree();
    // "0 active / 0 total" sits in the heading. Match loosely so a future
    // visual tweak doesn't break the test.
    expect(screen.getByText(/0 active\s*\/\s*0 total/i)).toBeTruthy();
  });
});

describe("AgentTree depth indentation", () => {
  it("indents a child agent under its parent via paddingLeft", () => {
    useAgentStore.setState({
      agents: [
        agent({ id: "root", task_id: "1.1", parent_agent_id: null }),
        agent({ id: "child", task_id: "1.2", parent_agent_id: "root" }),
      ],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    renderTree();

    // Both rows render. The depth-tagged DFS walk wraps each row in a
    // <div style="padding-left: depth * 20"> — root=0, child=20.
    const rootLink = screen.getByText("Task 1.1").closest("a");
    const childLink = screen.getByText("Task 1.2").closest("a");
    expect(rootLink).not.toBeNull();
    expect(childLink).not.toBeNull();
    const rootWrapper = rootLink!.parentElement as HTMLElement;
    const childWrapper = childLink!.parentElement as HTMLElement;
    expect(rootWrapper.style.paddingLeft).toBe("0px");
    expect(childWrapper.style.paddingLeft).toBe("20px");
  });

  it("renders an orphaned child as a root when its parent is filtered out", () => {
    useAgentStore.setState({
      // Child references a parent_agent_id that does not appear in the
      // visible filtered set — the tree treats it as a fresh root.
      agents: [agent({ id: "orphan", task_id: "9.9", parent_agent_id: "missing" })],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    renderTree();
    const link = screen.getByText("Task 9.9").closest("a");
    expect(link).not.toBeNull();
    const wrapper = link!.parentElement as HTMLElement;
    expect(wrapper.style.paddingLeft).toBe("0px");
  });
});

describe("AgentTree status filter", () => {
  function setupMixed() {
    useAgentStore.setState({
      agents: [
        agent({ id: "a", task_id: "1.1", status: "running" }),
        agent({ id: "b", task_id: "1.2", status: "completed" }),
        agent({ id: "c", task_id: "1.3", status: "failed" }),
      ],
      fetchAgents: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });
  }

  it("renders all four filter buttons (All/Running/Done/Failed)", () => {
    setupMixed();
    renderTree();
    for (const label of ["All", "Running", "Done", "Failed"]) {
      expect(screen.getByText(label)).toBeTruthy();
    }
  });

  it("clicking Running narrows the visible rows to running agents only", () => {
    setupMixed();
    renderTree();
    fireEvent.click(screen.getByText("Running"));
    expect(screen.getByText("Task 1.1")).toBeTruthy();
    expect(screen.queryByText("Task 1.2")).toBeNull();
    expect(screen.queryByText("Task 1.3")).toBeNull();
  });

  it("clicking Done narrows to completed agents and clicking All restores the full list", () => {
    setupMixed();
    renderTree();
    fireEvent.click(screen.getByText("Done"));
    expect(screen.queryByText("Task 1.1")).toBeNull();
    expect(screen.getByText("Task 1.2")).toBeTruthy();
    expect(screen.queryByText("Task 1.3")).toBeNull();
    fireEvent.click(screen.getByText("All"));
    expect(screen.getByText("Task 1.1")).toBeTruthy();
    expect(screen.getByText("Task 1.2")).toBeTruthy();
    expect(screen.getByText("Task 1.3")).toBeTruthy();
  });

  it("active-button styling reflects the current filter", () => {
    setupMixed();
    renderTree();
    const filterStrip = screen.getByText("Running").parentElement as HTMLElement;
    fireEvent.click(within(filterStrip).getByText("Running"));
    const running = within(filterStrip).getByText("Running");
    expect(running.className).toMatch(/font-semibold/);
    const all = within(filterStrip).getByText("All");
    expect(all.className).not.toMatch(/font-semibold/);
  });
});
