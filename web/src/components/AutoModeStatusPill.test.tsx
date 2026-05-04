import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AutoModeStatusPill } from "./PlanBoard.js";
import {
  usePlanStore,
  type AutoModeRuntime,
  type PlanConfig,
} from "../stores/plan-store.js";

const PLAN = "p1";

function defaultConfig(overrides: Partial<PlanConfig> = {}): PlanConfig {
  return {
    autoAdvance: false,
    autoMode: true,
    maxFixAttempts: 3,
    pausedReason: null,
    ...overrides,
  };
}

function seed(
  config: PlanConfig | null,
  runtime: AutoModeRuntime | null,
): void {
  usePlanStore.setState({
    planConfigs: config ? { [PLAN]: config } : {},
    autoModeRuntimes: { [PLAN]: runtime },
  });
}

afterEach(() => {
  cleanup();
  // Reset store between tests so seeded state doesn't leak.
  usePlanStore.setState({ planConfigs: {}, autoModeRuntimes: {} });
});

describe("AutoModeStatusPill", () => {
  it("hides when there is no PlanConfig", () => {
    usePlanStore.setState({ planConfigs: {}, autoModeRuntimes: {} });
    const { container } = render(<AutoModeStatusPill planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("hides when auto-mode is off and no runtime is set", () => {
    seed(defaultConfig({ autoMode: false }), null);
    const { container } = render(<AutoModeStatusPill planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders the merging pill with the task number", () => {
    seed(
      defaultConfig(),
      { state: "merging", task: "1.1" },
    );
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: merging task 1\.1/i)).toBeTruthy();
  });

  it("renders the awaiting-CI pill", () => {
    seed(
      defaultConfig(),
      { state: "awaiting_ci", task: "1.1", sha: "abc" },
    );
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: waiting on CI/i)).toBeTruthy();
  });

  it("renders the fixing-CI pill with attempt/cap", () => {
    seed(
      defaultConfig({ maxFixAttempts: 5 }),
      { state: "fixing_ci", task: "1.1", attempt: 2 },
    );
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: fixing CI \(attempt 2\/5\)/i)).toBeTruthy();
  });

  it("renders paused with merge-conflict label and Resume button", () => {
    seed(defaultConfig({ pausedReason: "merge_conflict" }), null);
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: paused — merge conflict/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Resume/i })).toBeTruthy();
  });

  it("renders paused with fix-cap-reached label", () => {
    seed(defaultConfig({ pausedReason: "fix_cap_reached" }), null);
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: paused — fix cap reached/i)).toBeTruthy();
  });

  it("renders paused with raw reason for unknown values", () => {
    seed(defaultConfig({ pausedReason: "weird_new_reason" }), null);
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: paused — weird_new_reason/i)).toBeTruthy();
  });

  it("renders idle when auto-mode on and no runtime", () => {
    seed(defaultConfig(), null);
    render(<AutoModeStatusPill planName={PLAN} />);
    expect(screen.getByText(/auto: idle/i)).toBeTruthy();
  });

  it("PUTs pausedReason: null when Resume is clicked", async () => {
    const fetchSpy = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify(
          defaultConfig({ pausedReason: null }),
        ),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchSpy);

    seed(defaultConfig({ pausedReason: "merge_conflict" }), null);
    render(<AutoModeStatusPill planName={PLAN} />);
    fireEvent.click(screen.getByRole("button", { name: /Resume/i }));

    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalled();
    });
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe(`/api/plans/${PLAN}/config`);
    expect(init.method).toBe("PUT");
    const bodyJson = JSON.parse(init.body as string) as Record<string, unknown>;
    // pausedReason must be present AND explicitly null on the wire.
    expect("pausedReason" in bodyJson).toBe(true);
    expect(bodyJson.pausedReason).toBeNull();

    vi.unstubAllGlobals();
  });
});
