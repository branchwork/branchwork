import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { HealthStateIndicator } from "./HealthStateIndicator.js";

type FetchHandler = (path: string) => { status: number; body: unknown };

function installFetchMock(handler: FetchHandler) {
  const fn = vi.fn(async (input: string | URL | Request) => {
    const path =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const { status, body } = handler(path);
    return new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

const OK_REPORT = {
  status: "ok",
  checkedAt: "2026-05-08T12:00:00Z",
  uptimeSeconds: 1234,
  categories: {
    agents: { byStatus: { running: 1, completed: 4 }, unknownStatusCount: 0 },
    taskStatus: {
      byStatus: { pending: 3, completed: 7 },
      unknownStatusCount: 0,
      stuckCheckingCount: 0,
    },
    branches: {
      lastSweepAt: "2026-05-08T11:55:00Z",
      secondsSinceLastSweep: 300,
      lastSweepClearedCount: 0,
    },
    ciRuns: { dismissedCount: 0, totalCount: 12 },
  },
  warnings: [],
};

const WARN_REPORT = {
  ...OK_REPORT,
  status: "warnings",
  categories: {
    ...OK_REPORT.categories,
    agents: { byStatus: { haunted: 1, running: 1 }, unknownStatusCount: 1 },
  },
  warnings: [
    {
      category: "agents",
      kind: "unknown_status",
      count: 1,
      message: '1 agent row(s) have an unrecognised status value: "haunted"',
      affectedPlans: ["plan-alpha", "plan-bravo"],
      recovery: "reset_task_status",
    },
  ],
};

describe("HealthStateIndicator", () => {
  beforeEach(() => {
    // Default to a test environment where the fetch stub is the only
    // network access. Each test installs its own handler.
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("renders 'State OK' when warnings is empty", async () => {
    installFetchMock(() => ({ status: 200, body: OK_REPORT }));
    render(<HealthStateIndicator />);
    await waitFor(() => {
      expect(screen.getByText("State OK")).toBeTruthy();
    });
    const button = screen.getByTestId("health-state-button");
    expect(button.getAttribute("data-state-warnings")).toBe("false");
  });

  it("flips to amber 'State warnings' when warnings is non-empty", async () => {
    installFetchMock(() => ({ status: 200, body: WARN_REPORT }));
    render(<HealthStateIndicator />);
    await waitFor(() => {
      expect(screen.getByText(/State warnings/)).toBeTruthy();
    });
    const button = screen.getByTestId("health-state-button");
    expect(button.getAttribute("data-state-warnings")).toBe("true");
  });

  it("expands a popover with affected plan deep-links on click", async () => {
    installFetchMock(() => ({ status: 200, body: WARN_REPORT }));
    render(<HealthStateIndicator />);
    await waitFor(() => screen.getByText(/State warnings/));

    fireEvent.click(screen.getByTestId("health-state-button"));
    expect(screen.getByTestId("health-state-popover")).toBeTruthy();

    const linkAlpha = screen.getByTestId("health-warning-plan-plan-alpha");
    const linkBravo = screen.getByTestId("health-warning-plan-plan-bravo");
    expect(linkAlpha.getAttribute("href")).toBe("/plans/plan-alpha");
    expect(linkBravo.getAttribute("href")).toBe("/plans/plan-bravo");
    expect(screen.getByText(/Recovery: open the affected plan/)).toBeTruthy();
  });

  it("toggles the popover closed on a second click", async () => {
    installFetchMock(() => ({ status: 200, body: WARN_REPORT }));
    render(<HealthStateIndicator />);
    await waitFor(() => screen.getByText(/State warnings/));
    const button = screen.getByTestId("health-state-button");

    fireEvent.click(button);
    expect(screen.queryByTestId("health-state-popover")).toBeTruthy();
    fireEvent.click(button);
    expect(screen.queryByTestId("health-state-popover")).toBeNull();
  });

  it("falls back to a 'State unknown' pill when fetch errors", async () => {
    installFetchMock(() => ({ status: 500, body: { error: "boom" } }));
    render(<HealthStateIndicator />);
    await waitFor(() => {
      expect(screen.getByText("State unknown")).toBeTruthy();
    });
  });
});
