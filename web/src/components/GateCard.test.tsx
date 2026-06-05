import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { GateCard, gateVisualState } from "./GateCard.js";
import { usePlanStore, type ParsedPlan, type PlanNode } from "../stores/plan-store.js";

const PLAN = "dag-plan";

function gateNode(overrides: Partial<PlanNode> = {}): PlanNode {
  return {
    id: "init",
    type: "gate",
    title: "Precondition gate",
    gateKind: "init",
    checks: ["git_repo", "clean_tree"],
    ...overrides,
  };
}

/// Seed `selectedPlan` so the optimistic `patchNodeStatus` in approve/retry
/// has a tree to patch (and so we can assert the patched status).
function seedPlan(nodes: PlanNode[]) {
  usePlanStore.setState({
    selectedPlan: {
      name: PLAN,
      filePath: `${PLAN}.yaml`,
      title: "DAG plan",
      context: "",
      project: "p",
      createdAt: new Date().toISOString(),
      modifiedAt: new Date().toISOString(),
      phases: [],
      nodes,
    } as ParsedPlan,
  });
}

interface PostCall {
  path: string;
  method: string;
}

function installFetchMock(): PostCall[] {
  const calls: PostCall[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({
        path: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
      });
      return new Response(JSON.stringify({ ok: true }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
  return calls;
}

beforeEach(() => {
  usePlanStore.setState({ selectedPlan: null });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  usePlanStore.getState().reset();
});

describe("gateVisualState", () => {
  it("maps completed / skipped to passed", () => {
    expect(gateVisualState(gateNode({ status: "completed" }))).toBe("passed");
    expect(gateVisualState(gateNode({ status: "skipped" }))).toBe("passed");
  });

  it("maps failed to failed", () => {
    expect(gateVisualState(gateNode({ status: "failed" }))).toBe("failed");
  });

  it("maps in_progress init/approval gates to awaiting_approval", () => {
    expect(gateVisualState(gateNode({ gateKind: "init", status: "in_progress" }))).toBe(
      "awaiting_approval",
    );
    expect(gateVisualState(gateNode({ gateKind: "approval", status: "in_progress" }))).toBe(
      "awaiting_approval",
    );
  });

  it("maps in_progress ci/end gates to checking", () => {
    expect(gateVisualState(gateNode({ gateKind: "ci", status: "in_progress" }))).toBe("checking");
    expect(gateVisualState(gateNode({ gateKind: "end", status: "in_progress" }))).toBe("checking");
  });

  it("maps pending / missing status to pending", () => {
    expect(gateVisualState(gateNode({ status: "pending" }))).toBe("pending");
    expect(gateVisualState(gateNode({ status: undefined }))).toBe("pending");
  });
});

describe("GateCard rendering", () => {
  it("renders a pending gate gray with no action button", () => {
    render(<GateCard planName={PLAN} nodeId="init" node={gateNode({ status: "pending" })} />);
    const card = screen.getByTestId("gate-card");
    expect(card.getAttribute("data-gate-state")).toBe("pending");
    expect(within(card).queryByRole("button")).toBeNull();
    // The defining diamond + kind label are present.
    expect(card.textContent).toContain("◇");
    expect(card.textContent).toContain("Init gate");
    expect(card.textContent).toContain("Pending");
  });

  it("renders an awaiting-approval init gate with an Approve button", () => {
    render(<GateCard planName={PLAN} nodeId="init" node={gateNode({ status: "in_progress" })} />);
    const card = screen.getByTestId("gate-card");
    expect(card.getAttribute("data-gate-state")).toBe("awaiting_approval");
    expect(within(card).getByRole("button", { name: /approve/i })).toBeTruthy();
    expect(card.textContent).toContain("Awaiting approval");
  });

  it("renders a checking CI gate with a spinner and no action button", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="ci"
        node={gateNode({ id: "ci", gateKind: "ci", status: "in_progress" })}
      />,
    );
    const card = screen.getByTestId("gate-card");
    expect(card.getAttribute("data-gate-state")).toBe("checking");
    expect(within(card).queryByRole("button")).toBeNull();
    expect(card.querySelector(".animate-spin")).toBeTruthy();
    expect(card.textContent).toContain("Checking");
  });

  it("renders a passed gate green with a check and no action button", () => {
    render(<GateCard planName={PLAN} nodeId="init" node={gateNode({ status: "completed" })} />);
    const card = screen.getByTestId("gate-card");
    expect(card.getAttribute("data-gate-state")).toBe("passed");
    expect(within(card).queryByRole("button")).toBeNull();
    expect(card.textContent).toContain("✓"); // ✓
    expect(card.textContent).toContain("Passed");
  });

  it("renders a failed gate red with a Retry button", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={gateNode({ id: "end", gateKind: "end", status: "failed" })}
      />,
    );
    const card = screen.getByTestId("gate-card");
    expect(card.getAttribute("data-gate-state")).toBe("failed");
    expect(within(card).getByRole("button", { name: /retry/i })).toBeTruthy();
    expect(card.textContent).toContain("✗"); // ✗
    expect(card.textContent).toContain("Failed");
  });

  it("falls back to the scoped id when the node has no title", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="sub.gate"
        node={gateNode({ id: "gate", title: "", status: "pending" })}
      />,
    );
    expect(screen.getByTestId("gate-card").textContent).toContain("sub.gate");
  });
});

describe("GateCard end-gate check results (Task 3.6)", () => {
  function endGate(overrides: Partial<PlanNode> = {}): PlanNode {
    return gateNode({
      id: "end",
      gateKind: "end",
      title: "Final verification",
      checks: ["all_merged", "compiles", "ci_green"],
      ...overrides,
    });
  }

  it("renders each check's verdict inline with its detail", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={endGate({
          status: "completed",
          gateChecks: [
            { name: "all_merged", status: "passed", detail: "3/3 branches merged" },
            { name: "compiles", status: "passed", detail: "2 check(s) passed" },
            { name: "ci_green", status: "passed", detail: "CI green" },
          ],
        })}
      />,
    );
    const checks = screen.getByTestId("gate-checks");
    // All three named checks render with their human label + detail.
    expect(within(checks).getByTestId("gate-check-all_merged").textContent).toContain("All merged");
    expect(within(checks).getByTestId("gate-check-all_merged").textContent).toContain(
      "3/3 branches merged",
    );
    expect(within(checks).getByTestId("gate-check-compiles").textContent).toContain("Compiles");
    expect(within(checks).getByTestId("gate-check-ci_green").textContent).toContain("CI green");
    // Passed checks carry the passed status marker.
    expect(
      within(checks).getByTestId("gate-check-all_merged").getAttribute("data-check-status"),
    ).toBe("passed");
  });

  it("renders the ci_green detail as a link when a run URL is present", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={endGate({
          status: "failed",
          gateChecks: [
            { name: "all_merged", status: "passed", detail: "3/3 branches merged" },
            { name: "compiles", status: "passed", detail: "1 check(s) passed" },
            {
              name: "ci_green",
              status: "failed",
              detail: "CI failed",
              url: "https://github.com/o/r/actions/runs/42",
            },
          ],
        })}
      />,
    );
    const link = within(screen.getByTestId("gate-check-ci_green")).getByRole("link");
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/actions/runs/42");
    expect(link.textContent).toContain("CI failed");
  });

  it("shows the failing compile output in a collapsible block (the failure is visible)", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={endGate({
          status: "failed",
          gateChecks: [
            { name: "all_merged", status: "passed", detail: "1/1 branches merged" },
            {
              name: "compiles",
              status: "failed",
              detail: "check 'build' failed (exit 7)",
              output: "error[E0599]: COMPILE_FAILED_MARKER\nerror: aborting",
            },
            { name: "ci_green", status: "skipped", detail: "not run — an earlier check failed" },
          ],
        })}
      />,
    );
    const compiles = screen.getByTestId("gate-check-compiles");
    expect(compiles.getAttribute("data-check-status")).toBe("failed");
    expect(compiles.textContent).toContain("check 'build' failed (exit 7)");
    // Acceptance: if compile fails, the failure output is visible.
    const out = within(compiles).getByTestId("gate-check-output-compiles");
    expect(out.textContent).toContain("COMPILE_FAILED_MARKER");
    // ci_green was skipped (short-circuited) — shown with the skipped marker.
    expect(screen.getByTestId("gate-check-ci_green").getAttribute("data-check-status")).toBe(
      "skipped",
    );
  });

  it("falls back to the declared check names when the gate hasn't run yet", () => {
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={endGate({ status: "pending" })} // no gateChecks
      />,
    );
    expect(screen.queryByTestId("gate-checks")).toBeNull();
    // The static check-names line is still shown.
    expect(screen.getByTestId("gate-card").textContent).toContain(
      "all_merged · compiles · ci_green",
    );
  });
});

describe("GateCard actions", () => {
  it("Approve POSTs to the approve endpoint and optimistically completes the node", async () => {
    const calls = installFetchMock();
    seedPlan([gateNode({ status: "in_progress" })]);
    render(<GateCard planName={PLAN} nodeId="init" node={gateNode({ status: "in_progress" })} />);

    fireEvent.click(screen.getByRole("button", { name: /approve/i }));

    await waitFor(() =>
      expect(
        calls.some(
          (c) => c.method === "POST" && c.path === `/api/plans/${PLAN}/gates/init/approve`,
        ),
      ).toBe(true),
    );
    await waitFor(() =>
      expect(usePlanStore.getState().selectedPlan?.nodes?.[0].status).toBe("completed"),
    );
  });

  it("Retry POSTs to the retry endpoint and optimistically resets the node to pending", async () => {
    const calls = installFetchMock();
    seedPlan([gateNode({ id: "end", gateKind: "end", status: "failed" })]);
    render(
      <GateCard
        planName={PLAN}
        nodeId="end"
        node={gateNode({ id: "end", gateKind: "end", status: "failed" })}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /retry/i }));

    await waitFor(() =>
      expect(
        calls.some((c) => c.method === "POST" && c.path === `/api/plans/${PLAN}/gates/end/retry`),
      ).toBe(true),
    );
    await waitFor(() =>
      expect(usePlanStore.getState().selectedPlan?.nodes?.[0].status).toBe("pending"),
    );
  });
});
