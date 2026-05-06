import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  screen,
  waitFor,
} from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import axe from "axe-core";
import { BulkDeleteModal } from "./BulkDeleteModal.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { HttpError } from "../api.js";

const PLANS = ["alpha-plan", "beta-plan", "gamma-plan"];

function okResponse(name: string) {
  return {
    ok: true as const,
    name,
    snapshotId: 7,
    archivePath: `/p/archive/${name}.yaml`,
    hard: false,
  };
}

beforeEach(() => {
  usePlanStore.setState({
    deletePlan: vi.fn().mockImplementation((name: string) =>
      Promise.resolve(okResponse(name)),
    ),
  });
  useAgentStore.setState({ selectAgent: vi.fn() });
});

afterEach(() => {
  cleanup();
});

describe("BulkDeleteModal", () => {
  it("lists every selected plan name and pluralizes the heading", () => {
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    expect(
      screen.getByRole("heading", { name: /Delete 3 plans\?/i }),
    ).toBeTruthy();
    const list = screen.getByTestId("bulk-delete-plan-list");
    expect(list.textContent).toContain("alpha-plan");
    expect(list.textContent).toContain("beta-plan");
    expect(list.textContent).toContain("gamma-plan");
  });

  it("uses singular heading when only one plan is selected", () => {
    render(
      <BulkDeleteModal
        planNames={["only-one"]}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    expect(
      screen.getByRole("heading", { name: /Delete 1 plan\?/i }),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: /^Delete 1$/ }),
    ).toBeTruthy();
  });

  it("opens directly in hard-confirm mode when retentionDays is 0", () => {
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={0}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    expect(
      screen.getByRole("heading", { name: /Permanently delete 3 plans\?/i }),
    ).toBeTruthy();
    expect(screen.queryByText(/Hold/)).toBeNull();
    expect(
      screen.getByRole("button", { name: /Permanently delete 3/i }),
    ).toBeTruthy();
  });

  it("Cancel does not call deletePlan and invokes onClose", () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn();
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/ }));
    expect(deletePlan).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("happy path: deletes all 3 plans serially and closes the modal", async () => {
    const onClose = vi.fn();
    const onPlanDeleted = vi.fn();
    const deletePlan = vi.fn().mockImplementation((name: string) =>
      Promise.resolve(okResponse(name)),
    );
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={onPlanDeleted}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 3$/ }));
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(3));
    // Serial: each deletePlan call landed in the brief's order, with
    // soft-delete opts (undefined) since retention > 0 and Shift was
    // not pressed.
    expect(deletePlan.mock.calls[0]).toEqual(["alpha-plan", undefined]);
    expect(deletePlan.mock.calls[1]).toEqual(["beta-plan", undefined]);
    expect(deletePlan.mock.calls[2]).toEqual(["gamma-plan", undefined]);
    // onPlanDeleted fires once per success so the parent can drop the
    // name from its selection set in real time.
    expect(onPlanDeleted).toHaveBeenCalledTimes(3);
    expect(onPlanDeleted).toHaveBeenNthCalledWith(1, "alpha-plan");
    expect(onPlanDeleted).toHaveBeenNthCalledWith(2, "beta-plan");
    expect(onPlanDeleted).toHaveBeenNthCalledWith(3, "gamma-plan");
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("Shift+click flips to hard-confirm without deleting; second click commits hard", async () => {
    const deletePlan = vi.fn().mockImplementation((name: string) =>
      Promise.resolve({ ...okResponse(name), hard: true }),
    );
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 3$/ }), {
      shiftKey: true,
    });
    expect(deletePlan).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(
        screen.getByRole("heading", { name: /Permanently delete 3 plans\?/i }),
      ).toBeTruthy(),
    );
    fireEvent.click(
      screen.getByRole("button", { name: /Permanently delete 3/i }),
    );
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(3));
    expect(deletePlan.mock.calls[0]).toEqual(["alpha-plan", { hard: true }]);
    expect(deletePlan.mock.calls[2]).toEqual(["gamma-plan", { hard: true }]);
  });

  it("409 mid-stream halts the loop, reports the blocker, leaves remaining plans untouched", async () => {
    const onClose = vi.fn();
    const onPlanDeleted = vi.fn();
    const deletePlan = vi.fn().mockImplementation((name: string) => {
      if (name === "beta-plan") {
        return Promise.reject(
          new HttpError(409, "Conflict", {
            error: "plan_has_running_agents",
            agents: ["agent-x", "agent-y"],
          }),
        );
      }
      return Promise.resolve(okResponse(name));
    });
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={onPlanDeleted}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 3$/ }));
    await waitFor(() =>
      expect(
        screen.getByText(/Cannot delete "beta-plan"/i),
      ).toBeTruthy(),
    );
    // Two attempts (alpha succeeded, beta blocked); gamma is NEVER
    // attempted — that is the load-bearing serial-halt invariant.
    expect(deletePlan).toHaveBeenCalledTimes(2);
    expect(deletePlan.mock.calls[0][0]).toBe("alpha-plan");
    expect(deletePlan.mock.calls[1][0]).toBe("beta-plan");
    // onPlanDeleted only fires for the success — the parent's selection
    // set keeps beta + gamma after this run, ready for retry.
    expect(onPlanDeleted).toHaveBeenCalledTimes(1);
    expect(onPlanDeleted).toHaveBeenCalledWith("alpha-plan");
    // Modal stays open on a halt so the user can act on the banner.
    expect(onClose).not.toHaveBeenCalled();
    // Agent IDs are clickable so the user can open the offending
    // terminal directly from the modal.
    expect(screen.getByRole("button", { name: "agent-x" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "agent-y" })).toBeTruthy();
  });

  it("clicking an agent link calls selectAgent and closes the modal", async () => {
    const onClose = vi.fn();
    const selectAgent = vi.fn();
    const deletePlan = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", {
        error: "plan_has_running_agents",
        agents: ["agent-zzz"],
      }),
    );
    usePlanStore.setState({ deletePlan });
    useAgentStore.setState({ selectAgent });
    render(
      <BulkDeleteModal
        planNames={["alpha-plan"]}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 1$/ }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "agent-zzz" })).toBeTruthy(),
    );
    fireEvent.click(screen.getByRole("button", { name: "agent-zzz" }));
    expect(selectAgent).toHaveBeenCalledWith("agent-zzz");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("409 auto_mode_in_flight surfaces an explanatory banner without agent list", async () => {
    const deletePlan = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", { error: "auto_mode_in_flight" }),
    );
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={["alpha-plan"]}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 1$/ }));
    await waitFor(() =>
      expect(
        screen.getByText(/auto-mode is mid-flight/i),
      ).toBeTruthy(),
    );
  });

  it("404 mid-stream is treated as success (raced delete) and the loop continues", async () => {
    const onPlanDeleted = vi.fn();
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockImplementation((name: string) => {
      if (name === "beta-plan") {
        return Promise.reject(
          new HttpError(404, "Not Found", { error: "plan_not_found" }),
        );
      }
      return Promise.resolve(okResponse(name));
    });
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={onPlanDeleted}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 3$/ }));
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(3));
    // All three names dropped from selection — the 404 is silently
    // treated as already-gone so it counts as success.
    expect(onPlanDeleted).toHaveBeenCalledTimes(3);
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("non-409 error halts the loop and surfaces the message", async () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockImplementation((name: string) => {
      if (name === "beta-plan") {
        return Promise.reject(
          new HttpError(500, "Server Error", { error: "boom" }),
        );
      }
      return Promise.resolve(okResponse(name));
    });
    usePlanStore.setState({ deletePlan });
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete 3$/ }));
    await waitFor(() =>
      expect(screen.getByText(/Delete failed on "beta-plan"/i)).toBeTruthy(),
    );
    // Exactly two attempts: alpha succeeded, beta blew up. Gamma was
    // never attempted because the serial loop halted.
    expect(deletePlan).toHaveBeenCalledTimes(2);
    expect(onClose).not.toHaveBeenCalled();
  });

  it("ESC closes the modal", () => {
    const onClose = vi.fn();
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={onClose}
        onPlanDeleted={() => {}}
      />,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("returns focus to the trigger element on unmount (focus-trap return)", () => {
    const trigger = document.createElement("button");
    trigger.id = "open-bulk-modal";
    document.body.appendChild(trigger);
    trigger.focus();
    const { unmount } = render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    expect(document.activeElement).not.toBe(trigger);
    unmount();
    expect(document.activeElement).toBe(trigger);
    document.body.removeChild(trigger);
  });

  it("Tab cycles focus inside the dialog (focus trap)", () => {
    render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    const dialog = screen.getByRole("dialog");
    const focusables = Array.from(
      dialog.querySelectorAll<HTMLElement>("button:not([disabled])"),
    );
    expect(focusables.length).toBeGreaterThan(0);
    const last = focusables[focusables.length - 1];
    last.focus();
    expect(document.activeElement).toBe(last);
    fireEvent.keyDown(document, { key: "Tab" });
    // Tab from the last focusable wraps back to the first.
    expect(document.activeElement).toBe(focusables[0]);
  });

  it("axe-core reports zero structural a11y violations on the rendered dialog", async () => {
    const { container } = render(
      <BulkDeleteModal
        planNames={PLANS}
        retentionDays={30}
        onClose={() => {}}
        onPlanDeleted={() => {}}
      />,
    );
    const dialog = container.querySelector('[role="dialog"]');
    expect(dialog).toBeTruthy();
    const results = await axe.run(dialog as Element, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
