import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { StaleDataChip, STALE_DATA_THRESHOLD_MS } from "./StaleDataChip.js";
import { useWsStore } from "../stores/ws-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";

afterEach(() => {
  cleanup();
  useWsStore.setState({ connected: false, disconnectedSince: null });
  usePlanStore.setState({ lastPlansFetchedAt: null });
  useAgentStore.setState({ lastAgentsFetchedAt: null });
});

describe("StaleDataChip", () => {
  it("does not render when WS is connected (even if data is old)", () => {
    useWsStore.setState({ connected: true, disconnectedSince: null });
    usePlanStore.setState({
      lastPlansFetchedAt: Date.now() - STALE_DATA_THRESHOLD_MS * 5,
    });
    render(<StaleDataChip slice="plans" />);
    expect(screen.queryByTestId("stale-chip-plans")).toBeNull();
  });

  it("does not render when data is fresh (even if WS is disconnected)", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now() - 5_000,
    });
    usePlanStore.setState({ lastPlansFetchedAt: Date.now() - 1_000 });
    render(<StaleDataChip slice="plans" />);
    expect(screen.queryByTestId("stale-chip-plans")).toBeNull();
  });

  it("does not render when lastFetchedAt is null", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now(),
    });
    usePlanStore.setState({ lastPlansFetchedAt: null });
    render(<StaleDataChip slice="plans" />);
    expect(screen.queryByTestId("stale-chip-plans")).toBeNull();
  });

  it("renders when WS is disconnected AND data is older than the threshold", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now() - STALE_DATA_THRESHOLD_MS,
    });
    usePlanStore.setState({
      lastPlansFetchedAt: Date.now() - (STALE_DATA_THRESHOLD_MS + 5_000),
    });
    render(<StaleDataChip slice="plans" />);
    const chip = screen.getByTestId("stale-chip-plans");
    expect(chip).toBeTruthy();
    expect(chip.textContent ?? "").toMatch(/stale/);
    expect(chip.getAttribute("title") ?? "").toMatch(/Disconnected from server/);
  });

  it("reads from the agents slice when slice='agents'", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now() - STALE_DATA_THRESHOLD_MS,
    });
    // Plan slice is fresh, agent slice is stale — only the agent chip
    // should render.
    usePlanStore.setState({ lastPlansFetchedAt: Date.now() });
    useAgentStore.setState({
      lastAgentsFetchedAt: Date.now() - (STALE_DATA_THRESHOLD_MS + 1_000),
    });
    const { container } = render(
      <>
        <StaleDataChip slice="plans" />
        <StaleDataChip slice="agents" />
      </>,
    );
    expect(container.querySelector('[data-testid="stale-chip-plans"]')).toBeNull();
    expect(
      container.querySelector('[data-testid="stale-chip-agents"]'),
    ).toBeTruthy();
  });

  it("includes a relative-age label in the tooltip", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now() - STALE_DATA_THRESHOLD_MS,
    });
    // 90 seconds old — formats as "1m ago".
    usePlanStore.setState({ lastPlansFetchedAt: Date.now() - 90_000 });
    render(<StaleDataChip slice="plans" />);
    const chip = screen.getByTestId("stale-chip-plans");
    expect(chip.getAttribute("title") ?? "").toMatch(/Last refreshed 1m ago/);
  });
});
