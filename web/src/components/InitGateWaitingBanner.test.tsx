import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { InitGateWaitingBanner } from "./PlanBoard.js";
import { usePlanStore, type InitGatePending } from "../stores/plan-store.js";

const PLAN = "p1";

function defaultGate(overrides: Partial<InitGatePending> = {}): InitGatePending {
  return {
    nodeId: "init",
    planTitle: "My DAG Plan",
    context: "ctx",
    checks: ["git_repo"],
    dismissed: false,
    ...overrides,
  };
}

function seed(gate: InitGatePending | null): void {
  usePlanStore.setState({ initGates: gate ? { [PLAN]: gate } : {} });
}

afterEach(() => {
  cleanup();
  seed(null);
});

describe("InitGateWaitingBanner", () => {
  it("renders nothing when no init gate is pending for the plan", () => {
    seed(null);
    const { container } = render(<InitGateWaitingBanner planName={PLAN} />);
    expect(container.textContent).toBe("");
  });

  it("renders the 'Waiting for confirmation' banner while a gate is pending", () => {
    seed(defaultGate());
    render(<InitGateWaitingBanner planName={PLAN} />);
    expect(screen.getByText("Waiting for confirmation")).toBeTruthy();
    expect(screen.getByRole("status")).toBeTruthy();
  });

  it("hides the 'Review & start' button while the modal is still open (not dismissed)", () => {
    seed(defaultGate({ dismissed: false }));
    render(<InitGateWaitingBanner planName={PLAN} />);
    expect(screen.queryByTestId("init-gate-review")).toBeNull();
  });

  it("shows 'Review & start' after dismissal and re-opens the modal (dismissed=false)", () => {
    seed(defaultGate({ dismissed: true }));
    render(<InitGateWaitingBanner planName={PLAN} />);
    const review = screen.getByTestId("init-gate-review");
    fireEvent.click(review);
    expect(usePlanStore.getState().initGates[PLAN]?.dismissed).toBe(false);
  });

  it("scopes to its own plan name", () => {
    usePlanStore.setState({ initGates: { other: defaultGate() } });
    const { container } = render(<InitGateWaitingBanner planName={PLAN} />);
    expect(container.textContent).toBe("");
  });
});
