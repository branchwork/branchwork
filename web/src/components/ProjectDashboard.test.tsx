import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, within } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import {
  ProjectDashboard,
  STALE_PLAN_NAME_RE,
  isAutoNamedPlan,
  isStalePlan,
} from "./ProjectDashboard.js";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

function plan(over: Partial<PlanSummary>): PlanSummary {
  return {
    name: "test-plan",
    title: "Test plan",
    project: "test",
    phaseCount: 1,
    taskCount: 1,
    doneCount: 1,
    createdAt: new Date(Date.now() - 60 * 86400000).toISOString(),
    modifiedAt: new Date(Date.now() - 60 * 86400000).toISOString(),
    ...over,
  };
}

beforeEach(() => {
  usePlanStore.setState({
    plans: [],
    loading: false,
    selectedPlan: null,
    selectPlan: vi.fn().mockResolvedValue(undefined),
  });
  useAgentStore.setState({ agents: [] });
  useSettingsStore.setState({ planArchiveRetentionDays: 30 });
});

afterEach(() => {
  cleanup();
});

describe("STALE_PLAN_NAME_RE", () => {
  // Acceptance examples are pinned by the plan task brief; loosening the
  // regex risks flagging hand-named plans as stale and silently deleting
  // active work, so any change here MUST keep these green.
  it.each(["enumerated-crafting-puffin", "cosmic-toasting-lagoon", "steady-prancing-squid"])(
    "matches CLI auto-named slug %s",
    (name) => {
      expect(STALE_PLAN_NAME_RE.test(name)).toBe(true);
      expect(isAutoNamedPlan(name)).toBe(true);
    },
  );

  it.each([
    "unify-check-prompts",
    "auto-mode-merge-ci-fix-loop",
    "plan-deletion",
    "fix-ci",
    "Cosmic-Toasting-Lagoon",
    "cosmic-toasting-lagoon-extra",
    "ing-ing-ing",
    "",
  ])("rejects non-auto slug %s", (name) => {
    expect(STALE_PLAN_NAME_RE.test(name)).toBe(false);
    expect(isAutoNamedPlan(name)).toBe(false);
  });

  it("requires the middle token to end in -ing", () => {
    expect(STALE_PLAN_NAME_RE.test("alpha-bravo-charlie")).toBe(false);
    expect(STALE_PLAN_NAME_RE.test("alpha-bring-charlie")).toBe(true);
  });
});

describe("isStalePlan", () => {
  // Anchor "now" so the matrix is deterministic.
  const NOW = new Date("2026-04-12T00:00:00Z").getTime();
  const FORTY_DAYS_AGO = new Date(NOW - 40 * 86400000).toISOString();
  const TEN_DAYS_AGO = new Date(NOW - 10 * 86400000).toISOString();

  it("flags a 40d old, all-done, auto-named plan", () => {
    const p = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 5,
      doneCount: 5,
      createdAt: FORTY_DAYS_AGO,
    });
    expect(isStalePlan(p, NOW)).toBe(true);
  });

  it("rejects a recent plan even if it is auto-named and done", () => {
    const p = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 5,
      doneCount: 5,
      createdAt: TEN_DAYS_AGO,
    });
    expect(isStalePlan(p, NOW)).toBe(false);
  });

  it("rejects a hand-named plan even if it is old and done", () => {
    const p = plan({
      name: "unify-check-prompts",
      taskCount: 5,
      doneCount: 5,
      createdAt: FORTY_DAYS_AGO,
    });
    expect(isStalePlan(p, NOW)).toBe(false);
  });

  it("rejects an auto-named plan with pending work", () => {
    const p = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 5,
      doneCount: 4,
      createdAt: FORTY_DAYS_AGO,
    });
    expect(isStalePlan(p, NOW)).toBe(false);
  });

  it("rejects a plan with zero tasks (isPlanDone semantics)", () => {
    const p = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 0,
      doneCount: 0,
      createdAt: FORTY_DAYS_AGO,
    });
    expect(isStalePlan(p, NOW)).toBe(false);
  });

  it("rejects a plan with a missing or unparseable createdAt", () => {
    const blank = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 1,
      doneCount: 1,
      createdAt: "",
    });
    const garbage = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 1,
      doneCount: 1,
      createdAt: "not-a-date",
    });
    expect(isStalePlan(blank, NOW)).toBe(false);
    expect(isStalePlan(garbage, NOW)).toBe(false);
  });

  it("rejects a plan exactly at the 30-day boundary (strictly greater)", () => {
    const exactlyThirty = plan({
      name: "cosmic-toasting-lagoon",
      taskCount: 1,
      doneCount: 1,
      createdAt: new Date(NOW - 30 * 86400000).toISOString(),
    });
    expect(isStalePlan(exactlyThirty, NOW)).toBe(false);
  });
});

describe("ProjectDashboard stale filter", () => {
  function setupPlans() {
    const old = new Date(Date.now() - 60 * 86400000).toISOString();
    const recent = new Date(Date.now() - 5 * 86400000).toISOString();
    usePlanStore.setState({
      plans: [
        // Stale: auto-named, done, old.
        plan({
          name: "cosmic-toasting-lagoon",
          title: "Cosmic Toasting Lagoon",
          project: "alpha",
          taskCount: 3,
          doneCount: 3,
          createdAt: old,
          modifiedAt: old,
        }),
        // Stale: auto-named, done, old, different project.
        plan({
          name: "steady-prancing-squid",
          title: "Steady Prancing Squid",
          project: "beta",
          taskCount: 2,
          doneCount: 2,
          createdAt: old,
          modifiedAt: old,
        }),
        // Hand-named, done, old — not stale (regex miss).
        plan({
          name: "unify-check-prompts",
          title: "Unify check prompts",
          project: "alpha",
          taskCount: 4,
          doneCount: 4,
          createdAt: old,
          modifiedAt: old,
        }),
        // Auto-named, done, but recent — not stale.
        plan({
          name: "enumerated-crafting-puffin",
          title: "Enumerated Crafting Puffin",
          project: "alpha",
          taskCount: 2,
          doneCount: 2,
          createdAt: recent,
          modifiedAt: recent,
        }),
        // Auto-named, old, but tasks not all done — not stale.
        plan({
          name: "lively-running-otter",
          title: "Lively running otter",
          project: "alpha",
          taskCount: 3,
          doneCount: 1,
          createdAt: old,
          modifiedAt: old,
        }),
      ],
    });
  }

  it("default view leaves the filter off and renders no banner", () => {
    setupPlans();
    render(<ProjectDashboard />);
    const toggle = screen.getByTestId("show-stale-toggle");
    expect(toggle.getAttribute("aria-pressed")).toBe("false");
    expect(toggle.textContent).toMatch(/Show stale plans/);
    expect(screen.queryByTestId("stale-banner")).toBeNull();
    // The one plan with pending work is in the active list and
    // therefore visible without expanding the done section, which
    // confirms the dashboard is unfiltered.
    expect(screen.getByText("Lively running otter")).toBeTruthy();
  });

  it("filters down to stale plans when toggled on", () => {
    setupPlans();
    render(<ProjectDashboard />);
    fireEvent.click(screen.getByTestId("show-stale-toggle"));
    const banner = screen.getByTestId("stale-banner");
    expect(banner.textContent).toMatch(/Showing\s+2\s+plans/);
    // Stale plans visible:
    expect(screen.getByText("Cosmic Toasting Lagoon")).toBeTruthy();
    expect(screen.getByText("Steady Prancing Squid")).toBeTruthy();
    // Non-stale plans gone:
    expect(screen.queryByText("Unify check prompts")).toBeNull();
    expect(screen.queryByText("Enumerated Crafting Puffin")).toBeNull();
    expect(screen.queryByText("Lively running otter")).toBeNull();
  });

  it("shows a friendly empty state when no plan is stale", () => {
    const recent = new Date(Date.now() - 5 * 86400000).toISOString();
    usePlanStore.setState({
      plans: [
        plan({
          name: "enumerated-crafting-puffin",
          title: "Enumerated Crafting Puffin",
          taskCount: 1,
          doneCount: 1,
          createdAt: recent,
          modifiedAt: recent,
        }),
      ],
    });
    render(<ProjectDashboard />);
    fireEvent.click(screen.getByTestId("show-stale-toggle"));
    const banner = screen.getByTestId("stale-banner");
    expect(banner.textContent).toMatch(/No stale plans/i);
    // No "Select all" button appears when there is nothing to select.
    expect(screen.queryByTestId("select-all-stale")).toBeNull();
  });

  it("Select all enters selection mode and ticks every visible stale plan", () => {
    setupPlans();
    render(<ProjectDashboard />);
    fireEvent.click(screen.getByTestId("show-stale-toggle"));
    fireEvent.click(screen.getByTestId("select-all-stale"));
    // Bulk-delete footer fires once selection is non-empty.
    const footer = screen.getByTestId("bulk-delete-footer");
    expect(footer.textContent).toMatch(/2\s+plans selected/);
    // Both stale rows are now checked. Each plan title sits inside a
    // <label> wrapping the checkbox once selection mode kicks in.
    const cosmicLabel = screen.getByText("Cosmic Toasting Lagoon").closest("label");
    expect(cosmicLabel).not.toBeNull();
    const cosmicBox = within(cosmicLabel as HTMLElement).getByRole("checkbox") as HTMLInputElement;
    expect(cosmicBox.checked).toBe(true);
    const squidLabel = screen.getByText("Steady Prancing Squid").closest("label");
    const squidBox = within(squidLabel as HTMLElement).getByRole("checkbox") as HTMLInputElement;
    expect(squidBox.checked).toBe(true);
  });
});

describe("ProjectDashboard render with plans", () => {
  function setupSeveral() {
    const recent = new Date(Date.now() - 5 * 86400000).toISOString();
    usePlanStore.setState({
      plans: [
        plan({
          name: "alpha-plan",
          title: "Alpha plan",
          project: "alpha",
          taskCount: 4,
          doneCount: 2,
          createdAt: recent,
          modifiedAt: recent,
        }),
        plan({
          name: "alpha-other",
          title: "Alpha other",
          project: "alpha",
          taskCount: 3,
          doneCount: 3,
          createdAt: recent,
          modifiedAt: recent,
        }),
        plan({
          name: "beta-plan",
          title: "Beta plan",
          project: "beta",
          taskCount: 2,
          doneCount: 1,
          createdAt: recent,
          modifiedAt: recent,
        }),
      ],
    });
  }

  it("groups plans by project and renders each project header", () => {
    setupSeveral();
    render(<ProjectDashboard />);
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText("beta")).toBeTruthy();
    // The summary line names the totals — 2 projects / 3 plans.
    const heading = screen.getByText(/^Projects$/i).closest("h2") as HTMLElement;
    const headerBlock = heading.parentElement as HTMLElement;
    expect(headerBlock.textContent).toMatch(/2\s*projects/);
    expect(headerBlock.textContent).toMatch(/3\s*plans/);
  });

  it("renders a Loading state when loading is true and plans are empty", () => {
    usePlanStore.setState({ plans: [], loading: true });
    render(<ProjectDashboard />);
    expect(screen.getByText(/Loading\.\.\./i)).toBeTruthy();
  });

  it("renders the 'No plans yet' empty state when neither loading nor any plans", () => {
    usePlanStore.setState({ plans: [], loading: false });
    render(<ProjectDashboard />);
    expect(screen.getByText("No plans yet")).toBeTruthy();
    expect(screen.getByText(/Plans are loaded from ~\/.claude\/plans\//i)).toBeTruthy();
  });
});

describe("ProjectDashboard plan navigation", () => {
  it("plan rows render as <Link> targeting /plans/:name", () => {
    const recent = new Date(Date.now() - 1 * 86400000).toISOString();
    usePlanStore.setState({
      plans: [
        plan({
          name: "navigate-me",
          title: "Navigate me",
          project: "alpha",
          taskCount: 2,
          doneCount: 0,
          createdAt: recent,
          modifiedAt: recent,
        }),
      ],
    });
    render(<ProjectDashboard />);
    const link = screen.getByText("Navigate me").closest("a") as HTMLAnchorElement;
    expect(link).not.toBeNull();
    // Memory router renders react-router <Link> as <a href> internally.
    expect(link.getAttribute("href")).toBe("/plans/navigate-me");
  });

  it("clicking a plan calls usePlanStore.selectPlan with the plan name", () => {
    const selectPlan = vi.fn().mockResolvedValue(undefined);
    const recent = new Date(Date.now() - 1 * 86400000).toISOString();
    usePlanStore.setState({
      selectPlan,
      plans: [
        plan({
          name: "navigate-me",
          title: "Navigate me",
          project: "alpha",
          taskCount: 2,
          doneCount: 0,
          createdAt: recent,
          modifiedAt: recent,
        }),
      ],
    });
    render(<ProjectDashboard />);
    const link = screen.getByText("Navigate me").closest("a") as HTMLAnchorElement;
    fireEvent.click(link);
    expect(selectPlan).toHaveBeenCalledWith("navigate-me");
  });

  it("does NOT call selectPlan when selection mode is on (clicking the row toggles the checkbox)", () => {
    const selectPlan = vi.fn().mockResolvedValue(undefined);
    const recent = new Date(Date.now() - 1 * 86400000).toISOString();
    usePlanStore.setState({
      selectPlan,
      plans: [
        plan({
          name: "navigate-me",
          title: "Navigate me",
          project: "alpha",
          taskCount: 2,
          doneCount: 0,
          createdAt: recent,
          modifiedAt: recent,
        }),
      ],
    });
    render(<ProjectDashboard />);
    fireEvent.click(screen.getByText(/^Select$/));
    // After Select, the row becomes a <label> wrapping a checkbox.
    const cb = screen.getByRole("checkbox") as HTMLInputElement;
    fireEvent.click(cb);
    expect(cb.checked).toBe(true);
    expect(selectPlan).not.toHaveBeenCalled();
  });
});
