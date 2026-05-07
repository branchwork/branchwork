import { beforeEach, describe, expect, it } from "vitest";
import { usePlanStore, type ParsedPlan, type PlanSummary } from "./plan-store.js";

// Inlined copy of the gate used by Sidebar/ProjectDashboard. The sole signal
// for "plan is done" is doneCount >= taskCount, so any upward drift on
// doneCount mis-classifies the plan as done.
function isPlanDone(p: PlanSummary): boolean {
  return p.taskCount > 0 && p.doneCount >= p.taskCount;
}

const PLAN_NAME = "drift-repro";

function makePendingTask(number: string) {
  return {
    number,
    title: `Task ${number}`,
    description: "",
    filePaths: [],
    acceptance: "",
    status: "pending",
  };
}

function seedStore() {
  const selectedPlan: ParsedPlan = {
    name: PLAN_NAME,
    filePath: `${PLAN_NAME}.md`,
    title: "Drift repro",
    context: "",
    project: null,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
    phases: [
      {
        number: 1,
        title: "Phase 1",
        description: "",
        tasks: [makePendingTask("1.1"), makePendingTask("1.2")],
      },
      {
        number: 2,
        title: "Phase 2",
        description: "",
        tasks: [makePendingTask("2.1"), makePendingTask("2.2")],
      },
    ],
  };

  const summary: PlanSummary = {
    name: PLAN_NAME,
    title: "Drift repro",
    project: null,
    phaseCount: 2,
    taskCount: 4,
    doneCount: 0,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
  };

  usePlanStore.setState({
    plans: [summary],
    selectedPlan,
    loading: false,
    warnings: [],
  });
}

describe("plan-store patchTaskStatus", () => {
  beforeEach(() => {
    seedStore();
  });

  it("does not double-count when last task flips completed → in_progress, then earlier tasks finish", () => {
    const { patchTaskStatus } = usePlanStore.getState();

    // Buggy sequence from task 0.2: last task is briefly marked completed,
    // then reverted to in_progress, then the three earlier tasks complete.
    patchTaskStatus(PLAN_NAME, "2.2", "completed");
    patchTaskStatus(PLAN_NAME, "2.2", "in_progress");
    patchTaskStatus(PLAN_NAME, "1.1", "completed");
    patchTaskStatus(PLAN_NAME, "1.2", "completed");
    patchTaskStatus(PLAN_NAME, "2.1", "completed");

    const plan = usePlanStore.getState().plans[0];
    // 3 tasks are completed, 1 is in_progress — must NOT show as done.
    // Pre-fix code reports doneCount=4 (never decrements), tripping isPlanDone.
    expect(plan.doneCount).toBe(3);
    expect(isPlanDone(plan)).toBe(false);
  });
});
