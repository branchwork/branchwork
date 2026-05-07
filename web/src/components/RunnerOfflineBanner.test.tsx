import { afterEach, describe, expect, it } from "vitest";
import { cleanup, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { RunnerOfflineBanner } from "./PlanBoard.js";
import { usePlanStore, type PlanConfig } from "../stores/plan-store.js";
import { useRunnerStore } from "../stores/runner-store.js";

const PLAN = "p1";

function defaultConfig(overrides: Partial<PlanConfig> = {}): PlanConfig {
  return {
    autoAdvance: false,
    autoMode: true,
    maxFixAttempts: 3,
    pausedReason: null,
    parallel: false,
    runnerId: null,
    ...overrides,
  };
}

function seed(
  config: PlanConfig | null,
  runners: Array<{
    id: string;
    name: string | null;
    status: "online" | "offline";
  }>,
): void {
  usePlanStore.setState({ planConfigs: config ? { [PLAN]: config } : {} });
  useRunnerStore.setState({
    runners: runners.map((r) => ({
      id: r.id,
      name: r.name,
      status: r.status,
      hostname: null,
      version: null,
      lastSeenAt: null,
      createdAt: null,
      health: null,
      versionMismatch: null,
      drivers: undefined,
    })) as never,
  });
}

afterEach(() => {
  cleanup();
  usePlanStore.setState({ planConfigs: {} });
  useRunnerStore.setState({ runners: [] });
});

describe("RunnerOfflineBanner", () => {
  it("does not render when pausedReason is null", () => {
    seed(defaultConfig({ pausedReason: null }), []);
    const { container } = render(<RunnerOfflineBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render for unrelated pause reasons", () => {
    seed(defaultConfig({ pausedReason: "merge_conflict" }), []);
    const { container } = render(<RunnerOfflineBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders banner copy when pausedReason is runner_offline", () => {
    seed(
      defaultConfig({ pausedReason: "runner_offline", runnerId: "runner-1" }),
      [{ id: "runner-1", name: "alpha", status: "offline" }],
    );
    render(<RunnerOfflineBanner planName={PLAN} />);
    expect(screen.getByText(/Auto-mode paused: pinned runner/i)).toBeTruthy();
    // The runner's name (not its id) is what we show by preference.
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText(/Wait for the runner to reconnect/i)).toBeTruthy();
  });

  it("falls back to the runner id when the row is not in the store", () => {
    seed(
      defaultConfig({ pausedReason: "runner_offline", runnerId: "runner-stale" }),
      [],
    );
    render(<RunnerOfflineBanner planName={PLAN} />);
    expect(screen.getByText("runner-stale")).toBeTruthy();
  });
});
