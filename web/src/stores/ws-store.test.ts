import { afterEach, describe, expect, it, vi } from "vitest";
import { useAgentStore } from "./agent-store.js";
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
});

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
});
