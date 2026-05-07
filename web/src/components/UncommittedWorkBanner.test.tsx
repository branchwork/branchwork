import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { UncommittedWorkBanner } from "./PlanBoard.js";
import { usePlanStore, type PlanConfig } from "../stores/plan-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";

const PLAN = "p1";

function defaultConfig(overrides: Partial<PlanConfig> = {}): PlanConfig {
  return {
    autoAdvance: false,
    autoMode: true,
    maxFixAttempts: 3,
    pausedReason: null,
    parallel: false,
    runnerId: null,
    runnerFailover: "pause",
    ...overrides,
  };
}

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-id-default",
    session_id: "sess-default",
    pid: 1234,
    parent_agent_id: null,
    plan_name: PLAN,
    task_id: "1.1",
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

function seed(config: PlanConfig | null, agents: Agent[]): void {
  usePlanStore.setState({
    planConfigs: config ? { [PLAN]: config } : {},
  });
  useAgentStore.setState({ agents });
}

afterEach(() => {
  cleanup();
  usePlanStore.setState({ planConfigs: {} });
  useAgentStore.setState({ agents: [], selectedAgentId: null });
});

describe("UncommittedWorkBanner", () => {
  it("does not render when pausedReason is null", () => {
    seed(defaultConfig({ pausedReason: null }), []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render for other pause reasons", () => {
    seed(defaultConfig({ pausedReason: "merge_conflict" }), []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when there is no PlanConfig", () => {
    seed(null, []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders the banner copy when pausedReason is agent_left_uncommitted_work", () => {
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({ id: "running-agent-1" }),
    ]);
    render(<UncommittedWorkBanner planName={PLAN} />);
    expect(screen.getByText(/Auto-mode paused: agent left uncommitted work\./i)).toBeTruthy();
    expect(screen.getByText(/Inspect and either commit, discard, or click Resume\./i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Inspect agent/i })).toBeTruthy();
  });

  it("clicking Inspect agent selects the still-running task agent for this plan", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({
        id: "other-plan-running",
        plan_name: "p2",
        status: "running",
      }),
      agent({
        id: "this-plan-completed",
        plan_name: PLAN,
        status: "completed",
      }),
      agent({
        id: "this-plan-running",
        plan_name: PLAN,
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    fireEvent.click(screen.getByRole("button", { name: /Inspect agent/i }));

    expect(selectAgentSpy).toHaveBeenCalledTimes(1);
    expect(selectAgentSpy).toHaveBeenCalledWith("this-plan-running");
  });

  it("disables Inspect agent when no running agent exists for this plan", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({ id: "completed-1", status: "completed" }),
      agent({
        id: "other-plan",
        plan_name: "p2",
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    const button = screen.getByRole("button", {
      name: /Inspect agent/i,
    }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);

    fireEvent.click(button);
    expect(selectAgentSpy).not.toHaveBeenCalled();
  });

  it("ignores non-task (check-plan) agents when picking the inspect target", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({
        id: "check-plan-agent",
        task_id: null,
        status: "running",
      }),
      agent({
        id: "task-agent",
        task_id: "2.1",
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    fireEvent.click(screen.getByRole("button", { name: /Inspect agent/i }));

    expect(selectAgentSpy).toHaveBeenCalledWith("task-agent");
  });
});
