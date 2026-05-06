import { afterEach, describe, expect, it, vi } from "vitest";
import { useAgentStore, type Agent } from "./agent-store.js";
import {
  usePlanStore,
  type ParsedPlan,
  type PlanSummary,
} from "./plan-store.js";
import { handleWsMessage } from "./ws-store.js";

afterEach(() => {
  // Reset zustand stores so seeded state and spies don't leak between tests.
  usePlanStore.setState({
    autoModeRuntimes: {},
    toasts: [],
    plans: [],
    selectedPlan: null,
  });
  useAgentStore.setState({ agents: [] });
});

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
    ...overrides,
  };
}

function makePlan(overrides: Partial<ParsedPlan> = {}): ParsedPlan {
  return {
    name: "p",
    filePath: "p.yaml",
    title: "P",
    context: "",
    project: null,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
    phases: [],
    ...overrides,
  };
}

describe("ws-store handleWsMessage", () => {
  it("refreshes agents on task_advanced", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({
      type: "task_advanced",
      data: {
        plan: "fix-plan-done-in-progress",
        from_task: "1.1",
        to_tasks: ["1.2", "1.3"],
      },
    });

    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it("sets auto_finishing pill state on auto_finish_triggered", () => {
    const fetchAgents = vi.fn().mockResolvedValue(undefined);
    useAgentStore.setState({ fetchAgents });

    handleWsMessage({
      type: "auto_finish_triggered",
      data: {
        agent_id: "abc-123",
        plan: "unattended-auto-mode",
        task: "6.1",
        trigger: "stop_hook",
      },
    });

    const runtime = usePlanStore.getState()
      .autoModeRuntimes["unattended-auto-mode"];
    expect(runtime).toEqual({
      state: "auto_finishing",
      task: "6.1",
    });
    // Stop-hook path also defensively refreshes agents so the row's
    // stop_reason flips visibly before `agent_stopped` lands.
    expect(fetchAgents).toHaveBeenCalledTimes(1);
  });

  it(
    "soft plan_deleted drops the plan, clears selectedPlan, " +
      "and pushes an Undo toast",
    () => {
      const summary: PlanSummary = {
        name: "doomed",
        title: "Doomed",
        project: null,
        phaseCount: 1,
        taskCount: 1,
        doneCount: 0,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
      };
      const selected: ParsedPlan = {
        name: "doomed",
        filePath: "doomed.yaml",
        title: "Doomed",
        context: "",
        project: null,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
        phases: [],
      };
      usePlanStore.setState({ plans: [summary], selectedPlan: selected });

      handleWsMessage({
        type: "plan_deleted",
        data: { plan: "doomed", snapshot_id: "snap-123", hard: false },
      });

      const state = usePlanStore.getState();
      expect(state.plans.find((p) => p.name === "doomed")).toBeUndefined();
      // App.tsx routes back to ProjectDashboard when selectedPlan is null.
      expect(state.selectedPlan).toBeNull();
      expect(state.toasts).toHaveLength(1);
      expect(state.toasts[0]).toMatchObject({
        kind: "info",
        message: "Deleted plan doomed",
        action: { label: "Undo", snapshotId: "snap-123" },
      });
    },
  );

  it(
    "hard plan_deleted (no snapshot_id) pushes a toast without an Undo action",
    () => {
      const summary: PlanSummary = {
        name: "obsolete",
        title: "Obsolete",
        project: null,
        phaseCount: 0,
        taskCount: 0,
        doneCount: 0,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
      };
      usePlanStore.setState({ plans: [summary], selectedPlan: null });

      handleWsMessage({
        type: "plan_deleted",
        data: { plan: "obsolete", snapshot_id: null, hard: true },
      });

      const state = usePlanStore.getState();
      expect(state.plans.find((p) => p.name === "obsolete")).toBeUndefined();
      expect(state.toasts).toHaveLength(1);
      expect(state.toasts[0].action).toBeUndefined();
      expect(state.toasts[0]).toMatchObject({
        kind: "info",
        message: "Deleted plan obsolete",
      });
    },
  );

  it(
    "plan_deleted leaves selectedPlan alone when the user is viewing a different plan",
    () => {
      const other: ParsedPlan = {
        name: "still-here",
        filePath: "still-here.yaml",
        title: "Still here",
        context: "",
        project: null,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
        phases: [],
      };
      usePlanStore.setState({ plans: [], selectedPlan: other });

      handleWsMessage({
        type: "plan_deleted",
        data: { plan: "doomed", snapshot_id: "snap-1", hard: false },
      });

      expect(usePlanStore.getState().selectedPlan).toEqual(other);
    },
  );

  it(
    "drops a malformed task_status_changed: warns, does not throw, " +
      "does not partially apply",
    () => {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      const patchTaskStatus = vi.fn();
      // Replace the action with a spy so we can assert the malformed
      // payload never reached the store.
      usePlanStore.setState({ patchTaskStatus });

      // `plan_name` must be a string — sending a number is the canonical
      // shape mismatch the schema needs to reject.
      expect(() =>
        handleWsMessage({
          type: "task_status_changed",
          data: { plan_name: 123, task_number: "1.1", status: "completed" },
        }),
      ).not.toThrow();

      expect(patchTaskStatus).not.toHaveBeenCalled();
      expect(warn).toHaveBeenCalledTimes(1);
      const args = warn.mock.calls[0];
      expect(String(args[0])).toMatch(/dropped malformed/);

      warn.mockRestore();
    },
  );

  it(
    "drops a JSON string that fails the discriminator: warns and ignores",
    () => {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      const fetchAgents = vi.fn().mockResolvedValue(undefined);
      useAgentStore.setState({ fetchAgents });

      // Unknown event types fall outside the discriminated union and the
      // listener silently drops them. The on-the-wire shape is a string
      // (this is what `ws.onmessage` hands to `handleWsMessage`), so we
      // exercise the JSON-string branch here too.
      expect(() =>
        handleWsMessage(
          JSON.stringify({ type: "completely_unknown_event", data: {} }),
        ),
      ).not.toThrow();

      expect(fetchAgents).not.toHaveBeenCalled();
      expect(warn).toHaveBeenCalled();

      warn.mockRestore();
    },
  );

  it("drops non-JSON garbage from ws.onmessage without throwing", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(() => handleWsMessage("<<not even json>>")).not.toThrow();
    expect(warn).toHaveBeenCalled();
    expect(String(warn.mock.calls[0][0])).toMatch(/malformed/);

    warn.mockRestore();
  });

  it(
    "phase_advanced refetches the selected plan and pushes a toast",
    () => {
      const selected = makePlan({ name: "alpha", title: "Alpha" });
      const selectPlan = vi.fn().mockResolvedValue(undefined);
      usePlanStore.setState({ selectedPlan: selected, selectPlan });

      handleWsMessage({
        type: "phase_advanced",
        data: { plan_name: "alpha", from_phase: 1, to_phase: 2 },
      });

      expect(selectPlan).toHaveBeenCalledWith("alpha");
      const toasts = usePlanStore.getState().toasts;
      expect(toasts).toHaveLength(1);
      expect(toasts[0].message).toMatch(/alpha.*Phase 2/);
    },
  );

  it(
    "phase_advanced for a non-selected plan still pushes a toast " +
      "but does not refetch",
    () => {
      const other = makePlan({ name: "beta" });
      const selectPlan = vi.fn().mockResolvedValue(undefined);
      usePlanStore.setState({ selectedPlan: other, selectPlan });

      handleWsMessage({
        type: "phase_advanced",
        data: { plan_name: "alpha", from_phase: 1, to_phase: 2 },
      });

      expect(selectPlan).not.toHaveBeenCalled();
      expect(usePlanStore.getState().toasts).toHaveLength(1);
    },
  );

  it("task_cost_reported updates the per-task cost and the plan aggregate", () => {
    const selected = makePlan({
      name: "alpha",
      totalCostUsd: 0.5,
      phases: [
        {
          number: 1,
          title: "P1",
          description: "",
          tasks: [
            {
              number: "1.1",
              title: "T1",
              description: "",
              filePaths: [],
              acceptance: "",
              costUsd: 0.2,
            },
          ],
        },
      ],
    });
    const summary: PlanSummary = {
      name: "alpha",
      title: "Alpha",
      project: null,
      phaseCount: 1,
      taskCount: 1,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      totalCostUsd: 0.5,
    };
    usePlanStore.setState({ selectedPlan: selected, plans: [summary] });

    handleWsMessage({
      type: "task_cost_reported",
      data: { plan_name: "alpha", task_number: "1.1", amount_usd: 0.7 },
    });

    const next = usePlanStore.getState();
    expect(next.selectedPlan!.phases[0].tasks[0].costUsd).toBeCloseTo(0.7);
    // Aggregate is bumped by the signed delta (+0.5 = 0.7 - 0.2)
    expect(next.selectedPlan!.totalCostUsd).toBeCloseTo(1.0);
    expect(next.plans[0].totalCostUsd).toBeCloseTo(1.0);
  });

  it("task_cost_reported on a non-selected plan still bumps the summary", () => {
    const summary: PlanSummary = {
      name: "alpha",
      title: "Alpha",
      project: null,
      phaseCount: 1,
      taskCount: 1,
      doneCount: 0,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      totalCostUsd: 0.5,
    };
    usePlanStore.setState({ selectedPlan: null, plans: [summary] });

    handleWsMessage({
      type: "task_cost_reported",
      data: { plan_name: "alpha", task_number: "1.1", amount_usd: 0.3 },
    });

    // Non-selected: store has no per-task data, so the unsigned +amount
    // fallback applies. Next plan refetch reconciles drift.
    expect(usePlanStore.getState().plans[0].totalCostUsd).toBeCloseTo(0.8);
  });

  it("plan_reset refetches the selected plan, refreshes the list, and toasts", async () => {
    const selected = makePlan({ name: "alpha" });
    const selectPlan = vi.fn().mockResolvedValue(undefined);
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ selectedPlan: selected, selectPlan, fetchPlans });

    handleWsMessage({
      type: "plan_reset",
      data: { plan_name: "alpha", cleared: 4 },
    });

    expect(selectPlan).toHaveBeenCalledWith("alpha");
    expect(fetchPlans).toHaveBeenCalledTimes(1);
    const toasts = usePlanStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].message).toMatch(/alpha.*4 tasks/);
  });

  it("plan_reset with cleared=1 toasts in the singular", () => {
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ selectedPlan: null, fetchPlans });

    handleWsMessage({
      type: "plan_reset",
      data: { plan_name: "alpha", cleared: 1 },
    });

    expect(usePlanStore.getState().toasts[0].message).toMatch(/1 task\)/);
  });

  it("ci_run_dismissed clears the CI badge on the matching task", () => {
    const selected = makePlan({
      name: "alpha",
      phases: [
        {
          number: 1,
          title: "P1",
          description: "",
          tasks: [
            {
              number: "1.1",
              title: "T1",
              description: "",
              filePaths: [],
              acceptance: "",
              ci: {
                id: 7,
                status: "failure",
                conclusion: "failure",
                runUrl: null,
                commitSha: null,
                updatedAt: "2026-04-12T00:00:00Z",
              },
            },
          ],
        },
      ],
    });
    usePlanStore.setState({ selectedPlan: selected });

    handleWsMessage({
      type: "ci_run_dismissed",
      data: { id: 7, plan_name: "alpha", task_number: "1.1" },
    });

    const ci = usePlanStore.getState().selectedPlan!.phases[0].tasks[0].ci;
    expect(ci).toBeNull();
  });

  it(
    "agent_branch_cleared with agent_id patches branch on the matching agent",
    () => {
      useAgentStore.setState({
        agents: [
          makeAgent({ id: "a-1", branch: "feature/foo" }),
          makeAgent({ id: "a-2", branch: "feature/bar" }),
        ],
      });

      handleWsMessage({
        type: "agent_branch_cleared",
        data: {
          agent_id: "a-1",
          branch: "feature/foo",
          reason: "boot_sweep: branch not present in project git",
        },
      });

      const agents = useAgentStore.getState().agents;
      expect(agents.find((a) => a.id === "a-1")!.branch).toBeNull();
      expect(agents.find((a) => a.id === "a-2")!.branch).toBe("feature/bar");
    },
  );

  it(
    "agent_branch_cleared without agent_id falls back to matching by branch",
    () => {
      useAgentStore.setState({
        agents: [
          makeAgent({ id: "a-1", branch: "feature/foo" }),
          makeAgent({ id: "a-2", branch: "feature/foo" }),
          makeAgent({ id: "a-3", branch: "feature/bar" }),
        ],
      });

      handleWsMessage({
        type: "agent_branch_cleared",
        data: { branch: "feature/foo" },
      });

      const agents = useAgentStore.getState().agents;
      expect(agents.find((a) => a.id === "a-1")!.branch).toBeNull();
      expect(agents.find((a) => a.id === "a-2")!.branch).toBeNull();
      expect(agents.find((a) => a.id === "a-3")!.branch).toBe("feature/bar");
    },
  );

  it("hook_event is logged via the validator instead of swallowed", () => {
    const debug = vi.spyOn(console, "debug").mockImplementation(() => {});

    handleWsMessage({
      type: "hook_event",
      data: { session_id: "s-1", hook_type: "PreToolUse", tool_name: "bash" },
    });

    expect(debug).toHaveBeenCalled();
    expect(String(debug.mock.calls[0][0])).toMatch(/hook_event/);

    debug.mockRestore();
  });

  it("runner_connected validates and is accepted as a no-op (Phase 4)", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    expect(() =>
      handleWsMessage({
        type: "runner_connected",
        data: { runner_id: "r-1", runner_name: "alpha" },
      }),
    ).not.toThrow();
    // No `dropped malformed` warning — the schema accepted the payload.
    expect(warn).not.toHaveBeenCalled();

    warn.mockRestore();
  });
});
