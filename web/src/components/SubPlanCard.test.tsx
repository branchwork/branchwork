import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { SubPlanCard } from "./SubPlanCard.js";
import { type PlanNode } from "../stores/plan-store.js";

function task(id: string, status?: string): PlanNode {
  return { id, type: "task", title: `Task ${id}`, status };
}

function gate(id: string): PlanNode {
  return { id, type: "gate", title: `Gate ${id}`, gateKind: "init" };
}

function subPlan(id: string, nodes: PlanNode[], title?: string): PlanNode {
  return { id, type: "sub_plan", title: title ?? `Sub ${id}`, nodes };
}

/// Render a recognizable marker per child so we can assert (a) which children
/// rendered and (b) the scoped id each one received — without mounting real
/// TaskCard/GateCard (which need router + stores). This is the delegation seam
/// the production board fills with DagNodeRenderer.
function renderChildStub(node: PlanNode, scopedId: string) {
  return (
    <div data-testid={`child-${node.id}`} data-scoped={scopedId} data-type={node.type}>
      {node.title}
    </div>
  );
}

function renderCard(node: PlanNode, scopedId = node.id) {
  return render(<SubPlanCard node={node} scopedId={scopedId} renderChild={renderChildStub} />);
}

afterEach(() => cleanup());

describe("SubPlanCard progress summary", () => {
  it("shows aggregate task progress, counting completed and skipped", () => {
    renderCard(
      subPlan("auth", [
        task("a", "completed"),
        task("b", "skipped"),
        task("c", "completed"),
        task("d", "in_progress"),
        task("e", "pending"),
      ]),
    );
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("3/5 tasks done");
  });

  it("excludes gate nodes from the task count", () => {
    renderCard(subPlan("auth", [task("a", "completed"), gate("g"), task("b", "pending")]));
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("1/2 tasks done");
  });

  it("counts task descendants of nested sub-plans", () => {
    renderCard(
      subPlan("outer", [
        task("a", "completed"),
        subPlan("inner", [task("x", "pending"), task("y", "completed")]),
      ]),
    );
    // 3 tasks total (a, inner.x, inner.y); 2 done (a, y).
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("2/3 tasks done");
  });

  it("singularizes the label for a single task", () => {
    renderCard(subPlan("auth", [task("a", "completed")]));
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("1/1 task done");
  });
});

describe("SubPlanCard completed state (Task 5.2)", () => {
  it("shows the completed (emerald) badge when every task is done", () => {
    renderCard(subPlan("auth", [task("a", "completed"), task("b", "skipped")]));
    expect(screen.getByTestId("sub-plan-progress").className).toContain("emerald");
  });

  it("shows the in-progress (indigo) badge while a task is outstanding", () => {
    renderCard(subPlan("auth", [task("a", "completed"), task("b", "pending")]));
    const badge = screen.getByTestId("sub-plan-progress");
    expect(badge.className).toContain("indigo");
    expect(badge.className).not.toContain("emerald");
  });

  it("flips to completed from the propagated node status even with a non-task child", () => {
    // A sub-plan whose own node.status is `completed` (set by the
    // `sub_plan_completed` WS event → patchNodeStatus) reads as done even
    // though the task-only progress count can't see the completed gate child.
    const node: PlanNode = {
      id: "auth",
      type: "sub_plan",
      title: "Auth",
      status: "completed",
      nodes: [task("login", "completed"), gate("approve")],
    };
    renderCard(node);
    // Task-only progress is 1/1 (the gate isn't counted), but the badge is
    // emerald because node.status === "completed" is authoritative.
    expect(screen.getByTestId("sub-plan-progress").className).toContain("emerald");
  });
});

describe("SubPlanCard collapse / expand", () => {
  it("defaults expanded while work is outstanding and renders children", () => {
    renderCard(subPlan("auth", [task("a", "completed"), task("b", "pending")]));
    expect(screen.getByTestId("sub-plan-card").getAttribute("data-expanded")).toBe("true");
    // Children are visible (acceptance: expanding shows child tasks).
    expect(screen.getByTestId("child-a")).toBeTruthy();
    expect(screen.getByTestId("child-b")).toBeTruthy();
  });

  it("collapses to the summary when toggled (children hidden, progress kept)", () => {
    renderCard(subPlan("auth", [task("a", "completed"), task("b", "pending")]));
    // Exactly one button per card (the disclosure toggle).
    fireEvent.click(screen.getByRole("button"));
    const card = screen.getByTestId("sub-plan-card");
    expect(card.getAttribute("data-expanded")).toBe("false");
    expect(screen.queryByTestId("child-a")).toBeNull();
    expect(screen.queryByTestId("child-b")).toBeNull();
    // Acceptance: collapsing still shows the summary.
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("1/2 tasks done");
  });

  it("defaults collapsed once every task is done, and expands on click", () => {
    renderCard(subPlan("auth", [task("a", "completed"), task("b", "skipped")]));
    expect(screen.getByTestId("sub-plan-card").getAttribute("data-expanded")).toBe("false");
    expect(screen.queryByTestId("child-a")).toBeNull();
    fireEvent.click(screen.getByRole("button"));
    expect(screen.getByTestId("sub-plan-card").getAttribute("data-expanded")).toBe("true");
    expect(screen.getByTestId("child-a")).toBeTruthy();
  });
});

describe("SubPlanCard child delegation", () => {
  it("delegates each child with its scoped id (parent.child)", () => {
    renderCard(subPlan("auth", [task("login", "pending"), gate("end")]), "auth");
    expect(screen.getByTestId("child-login").getAttribute("data-scoped")).toBe("auth.login");
    expect(screen.getByTestId("child-end").getAttribute("data-scoped")).toBe("auth.end");
    // Child type is preserved so the dispatcher can pick TaskCard vs GateCard.
    expect(screen.getByTestId("child-end").getAttribute("data-type")).toBe("gate");
  });

  it("scopes children under a nested sub-plan's own scoped id", () => {
    renderCard(subPlan("outer", [subPlan("inner", [task("x", "pending")])], "Outer"), "outer");
    // The inner sub-plan is delegated with scope `outer.inner`; the production
    // dispatcher then renders another SubPlanCard whose own children scope to
    // `outer.inner.x`. Here the stub renders the inner node directly.
    const inner = screen.getByTestId("child-inner");
    expect(inner.getAttribute("data-scoped")).toBe("outer.inner");
    expect(inner.getAttribute("data-type")).toBe("sub_plan");
  });
});

describe("SubPlanCard edge cases", () => {
  it("falls back to the scoped id when the node has no title", () => {
    renderCard({ id: "s", type: "sub_plan", title: "", nodes: [task("a", "pending")] }, "parent.s");
    const card = screen.getByTestId("sub-plan-card");
    expect(within(card).getByRole("button").textContent).toContain("parent.s");
  });

  it("shows an empty-state line for a sub-plan with no children", () => {
    renderCard(subPlan("empty", []));
    // total === 0 → collapsed by default; expand to reveal the empty-state.
    fireEvent.click(screen.getByRole("button"));
    expect(screen.getByTestId("sub-plan-children").textContent).toContain("No nodes");
    expect(screen.getByTestId("sub-plan-progress").textContent).toBe("0/0 tasks done");
  });
});
