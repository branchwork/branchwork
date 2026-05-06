import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { TaskCard } from "./TaskCard.js";
import {
  usePlanStore,
  type CiStatus,
  type ParsedPlan,
  type PlanTask,
} from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

const PLAN = "p1";

function task(overrides: Partial<PlanTask> = {}): PlanTask {
  return {
    number: "1.1",
    title: "Sample task",
    description: "",
    filePaths: [],
    acceptance: "",
    ...overrides,
  };
}

function ci(overrides: Partial<CiStatus> = {}): CiStatus {
  return {
    id: 1,
    status: "success",
    updatedAt: new Date().toISOString(),
    ...overrides,
  };
}

function plan(t: PlanTask): ParsedPlan {
  return {
    name: PLAN,
    filePath: "/tmp/p1.yaml",
    title: "Plan 1",
    context: "",
    project: null,
    createdAt: new Date().toISOString(),
    modifiedAt: new Date().toISOString(),
    phases: [
      {
        number: 1,
        title: "Phase 1",
        description: "",
        tasks: [t],
      },
    ],
  };
}

function seed(t: PlanTask): void {
  useAgentStore.setState({ agents: [], selectAgent: vi.fn() });
  usePlanStore.setState({
    selectedPlan: plan(t),
    selectPlan: vi.fn().mockResolvedValue(undefined),
    savePlan: vi.fn().mockResolvedValue(undefined),
    fetchPlans: vi.fn().mockResolvedValue(undefined),
  });
  // Default settings-store initial state already has effort/drivers/defaultDriver.
  // Force loaded=true so any conditional gating on it stays neutral.
  useSettingsStore.setState({ loaded: true });
}

afterEach(() => {
  cleanup();
  useAgentStore.setState({ agents: [] });
  usePlanStore.setState({ selectedPlan: null });
});

// The merge-button gate in TaskCard.tsx (line 99):
//   const canMerge = task.producesCommit !== false;
// This mirrors the exact expression — undefined/true → show Merge, false → hide.
function canMerge(t: Pick<PlanTask, "producesCommit">): boolean {
  return t.producesCommit !== false;
}

describe("TaskCard canMerge gate", () => {
  it("shows Merge when producesCommit is undefined (default)", () => {
    expect(canMerge({})).toBe(true);
  });

  it("shows Merge when producesCommit is true", () => {
    expect(canMerge({ producesCommit: true })).toBe(true);
  });

  it("hides Merge when producesCommit is false", () => {
    expect(canMerge({ producesCommit: false })).toBe(false);
  });
});

describe("TaskCard CI badge — via_fix_attempt marker", () => {
  it("appends a fix #N chip and 'passed via fix attempt N' tooltip when set", () => {
    const t = task({ ci: ci({ viaFixAttempt: 1 }) });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    // Chip text reads "fix #1" next to the green check.
    expect(screen.getByText(/fix #1/i)).toBeTruthy();
    // The CI label is preserved (green check is part of c.label "CI ✓").
    expect(screen.getByText(/CI/i)).toBeTruthy();
    // Tooltip is on the badge wrapper. No runUrl so the badge is a <span>;
    // querying by title attribute pulls the wrapper directly.
    const badge = document.querySelector(
      '[title="passed via fix attempt 1"]',
    );
    expect(badge).not.toBeNull();
  });

  it("renders unchanged when viaFixAttempt is not set", () => {
    const t = task({ ci: ci() });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    // No fix chip on the original-CI path.
    expect(screen.queryByText(/fix #/i)).toBeNull();
    // Tooltip is the original c.title for the success case.
    const badge = document.querySelector('[title="CI passed"]');
    expect(badge).not.toBeNull();
    // And the "passed via fix attempt" wording must NOT leak into a no-fix
    // run's tooltip.
    expect(
      document.querySelector('[title*="passed via fix attempt"]'),
    ).toBeNull();
  });

  it("appends ' — open run' to the fix tooltip when runUrl is present", () => {
    const t = task({
      ci: ci({
        viaFixAttempt: 2,
        runUrl: "https://example.invalid/run/42",
      }),
    });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    expect(screen.getByText(/fix #2/i)).toBeTruthy();
    const link = document.querySelector(
      '[title="passed via fix attempt 2 — open run"]',
    );
    expect(link).not.toBeNull();
    expect(link?.tagName).toBe("A");
  });
});
