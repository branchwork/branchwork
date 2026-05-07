import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { FailoverPicker } from "./PlanBoard.js";
import { useRunnerStore } from "../stores/runner-store.js";

afterEach(() => {
  cleanup();
  useRunnerStore.setState({ mode: "saas", runners: [] });
});

function seedSaasMode(): void {
  useRunnerStore.setState({ mode: "saas", runners: [] });
}

function seedStandaloneMode(): void {
  useRunnerStore.setState({ mode: "standalone", runners: [] });
}

describe("FailoverPicker (T11.5)", () => {
  it("hides entirely in standalone mode", () => {
    seedStandaloneMode();
    const { container } = render(
      <FailoverPicker runnerId="runner-x" runnerFailover="pause" onChange={() => {}} />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders disabled when no runner is pinned (failover meaningless without a pin)", () => {
    seedSaasMode();
    render(<FailoverPicker runnerId={null} runnerFailover="pause" onChange={() => {}} />);
    const select = screen.getByLabelText(
      "Runner failover policy for this plan",
    ) as HTMLSelectElement;
    expect(select.disabled).toBe(true);
  });

  it("renders enabled when a runner is pinned", () => {
    seedSaasMode();
    render(<FailoverPicker runnerId="runner-x" runnerFailover="pause" onChange={() => {}} />);
    const select = screen.getByLabelText(
      "Runner failover policy for this plan",
    ) as HTMLSelectElement;
    expect(select.disabled).toBe(false);
  });

  it("reflects runnerFailover='sibling' as the selected option", () => {
    seedSaasMode();
    render(<FailoverPicker runnerId="runner-x" runnerFailover="sibling" onChange={() => {}} />);
    const select = screen.getByLabelText(
      "Runner failover policy for this plan",
    ) as HTMLSelectElement;
    expect(select.value).toBe("sibling");
  });

  it("calls onChange with 'sibling' when the user picks it", () => {
    seedSaasMode();
    const onChange = vi.fn();
    render(<FailoverPicker runnerId="runner-x" runnerFailover="pause" onChange={onChange} />);
    const select = screen.getByLabelText("Runner failover policy for this plan");
    fireEvent.change(select, { target: { value: "sibling" } });
    expect(onChange).toHaveBeenCalledWith("sibling");
  });

  it("calls onChange with 'pause' when toggling back", () => {
    seedSaasMode();
    const onChange = vi.fn();
    render(<FailoverPicker runnerId="runner-x" runnerFailover="sibling" onChange={onChange} />);
    const select = screen.getByLabelText("Runner failover policy for this plan");
    fireEvent.change(select, { target: { value: "pause" } });
    expect(onChange).toHaveBeenCalledWith("pause");
  });

  it("respects the disabled prop even when a runner is pinned", () => {
    seedSaasMode();
    render(
      <FailoverPicker runnerId="runner-x" runnerFailover="pause" disabled onChange={() => {}} />,
    );
    const select = screen.getByLabelText(
      "Runner failover policy for this plan",
    ) as HTMLSelectElement;
    expect(select.disabled).toBe(true);
  });
});
