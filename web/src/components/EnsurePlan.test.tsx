import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
  type RenderResult,
} from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { EnsurePlan } from "./EnsurePlan.js";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";

function summary(name: string): PlanSummary {
  return {
    name,
    title: name,
    project: null,
    phaseCount: 1,
    taskCount: 1,
    doneCount: 0,
    createdAt: "2026-01-01T00:00:00Z",
    modifiedAt: "2026-01-01T00:00:00Z",
  };
}

function renderAt(initialPath: string): RenderResult {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Routes>
        <Route
          path="/plans/:planName"
          element={
            <EnsurePlan>
              <div>plan-board-marker</div>
            </EnsurePlan>
          }
        />
        <Route path="/" element={<div>dashboard-marker</div>} />
      </Routes>
    </MemoryRouter>,
  );
}

afterEach(() => {
  cleanup();
  usePlanStore.setState({
    plans: [],
    plansFetched: false,
    fetchPlans: usePlanStore.getInitialState().fetchPlans,
  });
});

describe("EnsurePlan", () => {
  it("renders loading state and calls fetchPlans when not yet fetched", async () => {
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({ plans: [], plansFetched: false, fetchPlans });

    renderAt("/plans/foo");

    expect(screen.getByText(/Loading/i)).toBeTruthy();
    await waitFor(() => {
      expect(fetchPlans).toHaveBeenCalledTimes(1);
    });
  });

  it("renders 'plan not found' with a back-to-dashboard link when name is absent", () => {
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({
      plans: [summary("alpha")],
      plansFetched: true,
      fetchPlans,
    });

    renderAt("/plans/does-not-exist");

    expect(screen.getByText(/Plan not found/i)).toBeTruthy();
    expect(screen.getByText("does-not-exist")).toBeTruthy();
    const back = screen.getByRole("link", { name: /Back to dashboard/i });
    expect((back as HTMLAnchorElement).getAttribute("href")).toBe("/");
    expect(fetchPlans).not.toHaveBeenCalled();
  });

  it("renders children when the plan name is in the fetched list", () => {
    const fetchPlans = vi.fn().mockResolvedValue(undefined);
    usePlanStore.setState({
      plans: [summary("alpha"), summary("beta")],
      plansFetched: true,
      fetchPlans,
    });

    renderAt("/plans/beta");

    expect(screen.getByText("plan-board-marker")).toBeTruthy();
    expect(screen.queryByText(/Plan not found/i)).toBeNull();
  });
});
