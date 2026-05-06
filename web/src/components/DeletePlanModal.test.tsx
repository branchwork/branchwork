import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  screen,
  waitFor,
} from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import axe from "axe-core";
import {
  DeletePlanModal,
  formatCascadeSummary,
} from "./DeletePlanModal.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { HttpError } from "../api.js";

const PLAN = "scratch-plan";

/// Default dry-run preview shape — clean plan (not blocked, modest
/// row counts). Tests that exercise the blocked path override this.
function defaultPreview() {
  return {
    ok: true as const,
    dryRun: true as const,
    name: PLAN,
    filePath: `/plans/${PLAN}.yaml`,
    hard: false,
    cascadeTables: [
      "task_status",
      "ci_runs",
      "plan_auto_mode",
      "plan_auto_advance",
      "task_fix_attempts",
      "plan_project",
      "plan_verdicts",
      "plan_budget",
      "task_learnings",
      "plan_org",
    ],
    wouldDelete: {
      task_status: 12,
      ci_runs: 8,
      plan_auto_mode: 1,
      plan_auto_advance: 1,
      task_fix_attempts: 3,
      plan_project: 1,
      plan_verdicts: 0,
      plan_budget: 0,
      task_learnings: 0,
      plan_org: 1,
    },
    blockedBy: null,
  };
}

beforeEach(() => {
  // Replace store actions with spies; tests assert on these.
  usePlanStore.setState({
    deletePlan: vi.fn().mockResolvedValue({
      ok: true,
      name: PLAN,
      snapshotId: 42,
      archivePath: "/p/archive/x.yaml",
      hard: false,
    }),
    previewDeletePlan: vi.fn().mockResolvedValue(defaultPreview()),
  });
  useAgentStore.setState({ selectAgent: vi.fn() });
});

afterEach(() => {
  cleanup();
});

describe("DeletePlanModal", () => {
  it("renders the soft-delete copy and shows the Shift hint when retention > 0", () => {
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    expect(
      screen.getByRole("heading", { name: /Delete plan scratch-plan\?/i }),
    ).toBeTruthy();
    expect(
      screen.getByText(/Recoverable for 30 days from the Activity tab/i),
    ).toBeTruthy();
    // Shift hint mentions Shift modifier verbatim.
    expect(screen.getByText(/Hold/)).toBeTruthy();
    expect(screen.getByText("Shift")).toBeTruthy();
  });

  it("uses singular 'day' when retention is exactly 1", () => {
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={1}
        onClose={() => {}}
      />,
    );
    expect(
      screen.getByText(/Recoverable for 1 day from the Activity tab/i),
    ).toBeTruthy();
  });

  it("opens directly in hard-confirm mode when retentionDays is 0", () => {
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={0}
        onClose={() => {}}
      />,
    );
    expect(
      screen.getByRole("heading", {
        name: /Permanently delete plan scratch-plan\?/i,
      }),
    ).toBeTruthy();
    expect(
      screen.getByText(/Permanently deletes the plan file/i),
    ).toBeTruthy();
    expect(screen.getByText(/This cannot be undone/i)).toBeTruthy();
    // No Shift hint — modifier is meaningless when retention is 0.
    expect(screen.queryByText(/Hold/)).toBeNull();
    // Primary button text is the permanent variant.
    expect(
      screen.getByRole("button", { name: /Permanently delete/i }),
    ).toBeTruthy();
  });

  it("Cancel does not call deletePlan and invokes onClose", () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn();
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/ }));
    expect(deletePlan).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("plain Delete click triggers a soft delete and closes the modal", async () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockResolvedValue({
      ok: true,
      name: PLAN,
      snapshotId: 7,
      archivePath: null,
      hard: false,
    });
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }));
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(1));
    // Soft delete: opts is undefined so the call site sends no `?hard=true`.
    expect(deletePlan).toHaveBeenCalledWith(PLAN, undefined);
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("Shift+click flips to hard-confirm without calling deletePlan; second click commits hard", async () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockResolvedValue({
      ok: true,
      name: PLAN,
      snapshotId: null,
      archivePath: null,
      hard: true,
    });
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    // First click holds Shift — should NOT issue the delete.
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }), {
      shiftKey: true,
    });
    expect(deletePlan).not.toHaveBeenCalled();
    // Modal re-renders into hard-confirm mode.
    await waitFor(() =>
      expect(
        screen.getByRole("heading", {
          name: /Permanently delete plan scratch-plan\?/i,
        }),
      ).toBeTruthy(),
    );
    // Second click on the new "Permanently delete" button issues hard delete.
    fireEvent.click(
      screen.getByRole("button", { name: /Permanently delete/i }),
    );
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(1));
    expect(deletePlan).toHaveBeenCalledWith(PLAN, { hard: true });
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("409 plan_has_running_agents keeps the modal open and lists agent IDs", async () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", {
        error: "plan_has_running_agents",
        agents: ["agent-aaa", "agent-bbb"],
      }),
    );
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }));
    await waitFor(() =>
      expect(
        screen.getByText(/this plan has running agents/i),
      ).toBeTruthy(),
    );
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "agent-aaa" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "agent-bbb" })).toBeTruthy();
  });

  it("clicking an agent link calls selectAgent and closes the modal", async () => {
    const onClose = vi.fn();
    const selectAgent = vi.fn();
    const deletePlan = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", {
        error: "plan_has_running_agents",
        agents: ["agent-xyz"],
      }),
    );
    usePlanStore.setState({ deletePlan });
    useAgentStore.setState({ selectAgent });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "agent-xyz" })).toBeTruthy(),
    );
    fireEvent.click(screen.getByRole("button", { name: "agent-xyz" }));
    expect(selectAgent).toHaveBeenCalledWith("agent-xyz");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("409 auto_mode_in_flight surfaces an explanatory banner", async () => {
    const onClose = vi.fn();
    const deletePlan = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", { error: "auto_mode_in_flight" }),
    );
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }));
    await waitFor(() =>
      expect(
        screen.getByText(/auto-mode is mid-flight/i),
      ).toBeTruthy(),
    );
    expect(onClose).not.toHaveBeenCalled();
  });

  it("404 (plan already gone) closes the modal silently", async () => {
    const onClose = vi.fn();
    const deletePlan = vi
      .fn()
      .mockRejectedValue(
        new HttpError(404, "Not Found", { error: "plan_not_found" }),
      );
    usePlanStore.setState({ deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /^Delete$/ }));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("ESC closes the modal", () => {
    const onClose = vi.fn();
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={onClose}
      />,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("returns focus to the trigger element on unmount", () => {
    const trigger = document.createElement("button");
    trigger.id = "open-modal";
    document.body.appendChild(trigger);
    trigger.focus();
    const { unmount } = render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    // While the modal is mounted, focus should have moved off the trigger.
    expect(document.activeElement).not.toBe(trigger);
    unmount();
    expect(document.activeElement).toBe(trigger);
    document.body.removeChild(trigger);
  });

  it("renders 'Computing cascade preview…' until the dry-run fetch resolves", async () => {
    // Don't resolve the preview promise during this assertion window.
    let resolvePreview: (v: ReturnType<typeof defaultPreview>) => void = () => {};
    const previewDeletePlan = vi
      .fn()
      .mockImplementation(
        () =>
          new Promise<ReturnType<typeof defaultPreview>>((res) => {
            resolvePreview = res;
          }),
      );
    usePlanStore.setState({ previewDeletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    expect(
      screen.getByText(/Computing cascade preview…/i),
    ).toBeTruthy();
    resolvePreview(defaultPreview());
    await waitFor(() =>
      expect(
        screen.queryByText(/Computing cascade preview…/i),
      ).toBeNull(),
    );
  });

  it("renders the per-table cascade preview line from wouldDelete", async () => {
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    // Wait for the preview fetch to resolve.
    await waitFor(() =>
      expect(
        screen.getByTestId("delete-plan-cascade-preview").textContent,
      ).toContain("12 task statuses"),
    );
    const previewText = screen.getByTestId(
      "delete-plan-cascade-preview",
    ).textContent;
    expect(previewText).toContain("12 task statuses");
    expect(previewText).toContain("8 CI runs");
    expect(previewText).toContain("3 fix attempts");
    // Skips zero counts.
    expect(previewText).not.toMatch(/0 (check verdicts|budget settings|learnings)/);
    // Tables with count 1 use the singular form.
    expect(previewText).toContain("1 auto-mode setting");
  });

  it("disables Delete and lists running agents when dry-run reports blockedBy", async () => {
    const previewDeletePlan = vi.fn().mockResolvedValue({
      ...defaultPreview(),
      blockedBy: {
        runningAgents: ["agent-aaa", "agent-bbb"],
        autoModeInFlight: false,
      },
    });
    const deletePlan = vi.fn();
    usePlanStore.setState({ previewDeletePlan, deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    // Banner copy from the blocked branch.
    await waitFor(() =>
      expect(
        screen.getByText(/this plan has running agents/i),
      ).toBeTruthy(),
    );
    // Agent IDs rendered as buttons (clickable to inspect).
    expect(screen.getByRole("button", { name: "agent-aaa" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "agent-bbb" })).toBeTruthy();
    // Delete button disabled — clicking it must NOT call deletePlan.
    const deleteBtn = screen.getByRole("button", { name: /^Delete$/ });
    expect((deleteBtn as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(deleteBtn);
    expect(deletePlan).not.toHaveBeenCalled();
  });

  it("disables Delete with auto-mode banner when dry-run reports autoModeInFlight", async () => {
    const previewDeletePlan = vi.fn().mockResolvedValue({
      ...defaultPreview(),
      blockedBy: { runningAgents: [], autoModeInFlight: true },
    });
    const deletePlan = vi.fn();
    usePlanStore.setState({ previewDeletePlan, deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    await waitFor(() =>
      expect(screen.getByText(/auto-mode is mid-flight/i)).toBeTruthy(),
    );
    const deleteBtn = screen.getByRole("button", { name: /^Delete$/ });
    expect((deleteBtn as HTMLButtonElement).disabled).toBe(true);
  });

  it("falls back to 'preview unavailable' if dry-run fetch errors and Delete stays clickable", async () => {
    const previewDeletePlan = vi
      .fn()
      .mockRejectedValue(new HttpError(500, "boom", { error: "x" }));
    const deletePlan = vi.fn().mockResolvedValue({
      ok: true,
      name: PLAN,
      snapshotId: 1,
      archivePath: null,
      hard: false,
    });
    usePlanStore.setState({ previewDeletePlan, deletePlan });
    render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    await waitFor(() =>
      expect(
        screen.getByText(/Cascade preview unavailable/i),
      ).toBeTruthy(),
    );
    // Delete button still clickable — preview is best-effort UX.
    const deleteBtn = screen.getByRole("button", { name: /^Delete$/ });
    expect((deleteBtn as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(deleteBtn);
    await waitFor(() => expect(deletePlan).toHaveBeenCalledTimes(1));
  });

  describe("formatCascadeSummary", () => {
    it("returns a fallback when nothing would be deleted", () => {
      expect(
        formatCascadeSummary({ task_status: 0, ci_runs: 0 }),
      ).toMatch(/No cascade rows to delete\./);
    });

    it("renders a single non-zero count with singular/plural", () => {
      expect(formatCascadeSummary({ task_status: 1 })).toBe(
        "1 task status will be deleted.",
      );
      expect(formatCascadeSummary({ task_status: 12 })).toBe(
        "12 task statuses will be deleted.",
      );
    });

    it("joins two entries with 'and'", () => {
      expect(
        formatCascadeSummary({ task_status: 12, ci_runs: 8 }),
      ).toBe("12 task statuses and 8 CI runs will be deleted.");
    });

    it("joins three or more entries with commas + Oxford and", () => {
      const out = formatCascadeSummary({
        task_status: 12,
        ci_runs: 8,
        task_fix_attempts: 3,
      });
      expect(out).toBe(
        "12 task statuses, 8 CI runs, and 3 fix attempts will be deleted.",
      );
    });

    it("skips zero-count entries silently", () => {
      const out = formatCascadeSummary({
        task_status: 12,
        ci_runs: 0,
        task_fix_attempts: 3,
      });
      expect(out).not.toContain("CI run");
      expect(out).toBe(
        "12 task statuses and 3 fix attempts will be deleted.",
      );
    });
  });

  it("axe-core reports zero violations on the rendered dialog", async () => {
    const { container } = render(
      <DeletePlanModal
        planName={PLAN}
        retentionDays={30}
        onClose={() => {}}
      />,
    );
    const dialog = container.querySelector('[role="dialog"]');
    expect(dialog).toBeTruthy();
    const results = await axe.run(dialog as Element, {
      // jsdom doesn't compute layout / colors so visual rules
      // (color-contrast in particular) can't be checked here. We
      // restrict to structural a11y rules — focus management, ARIA
      // semantics, keyboard reachability — which is exactly what
      // the task brief lists (focus trap, return-focus,
      // role="dialog").
      runOnly: {
        type: "tag",
        values: ["wcag2a", "wcag2aa"],
      },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
