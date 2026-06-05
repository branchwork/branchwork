import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { patchNodeInTree, usePlanStore, type ParsedPlan, type PlanNode } from "./plan-store.js";

function tree(): PlanNode[] {
  return [
    { id: "init", type: "gate", title: "Init", gateKind: "init", status: "completed" },
    { id: "a", type: "task", title: "A", status: "pending" },
    {
      id: "sub",
      type: "sub_plan",
      title: "Sub",
      nodes: [
        { id: "wire", type: "task", title: "Wire", status: "pending" },
        { id: "g", type: "gate", title: "G", gateKind: "approval", status: "in_progress" },
      ],
    },
  ];
}

describe("patchNodeInTree", () => {
  it("patches a top-level node by its bare (== scoped) id", () => {
    const t = tree();
    const out = patchNodeInTree(t, "a", "completed");
    expect(out).not.toBe(t);
    expect(out[1].status).toBe("completed");
    // Untouched sibling keeps its identity (no needless re-render).
    expect(out[0]).toBe(t[0]);
  });

  it("patches a nested node by its scoped (dotted) id", () => {
    const t = tree();
    const out = patchNodeInTree(t, "sub.g", "completed");
    expect(out[2].nodes?.[1].status).toBe("completed");
    // The scoped match did not touch the sibling task.
    expect(out[2].nodes?.[0].status).toBe("pending");
  });

  it("does NOT match a nested node by its bare id (must be scoped)", () => {
    const t = tree();
    // "wire" is "sub.wire" scoped — a bare "wire" must not match.
    expect(patchNodeInTree(t, "wire", "completed")).toBe(t);
  });

  it("returns the same reference when nothing matches", () => {
    const t = tree();
    expect(patchNodeInTree(t, "nope", "completed")).toBe(t);
  });
});

describe("patchNodeStatus store action", () => {
  beforeEach(() => {
    usePlanStore.setState({ selectedPlan: null });
  });
  afterEach(() => {
    usePlanStore.getState().reset();
  });

  function seed(nodes?: PlanNode[]) {
    usePlanStore.setState({
      selectedPlan: {
        name: "p",
        filePath: "p.yaml",
        title: "P",
        context: "",
        project: null,
        createdAt: "",
        modifiedAt: "",
        phases: [],
        nodes,
      } as ParsedPlan,
    });
  }

  it("patches the selected plan's node status", () => {
    seed([{ id: "init", type: "gate", title: "Init", gateKind: "init", status: "in_progress" }]);
    usePlanStore.getState().patchNodeStatus("p", "init", "completed");
    expect(usePlanStore.getState().selectedPlan?.nodes?.[0].status).toBe("completed");
  });

  it("is a no-op for a different selected plan", () => {
    seed([{ id: "init", type: "gate", title: "Init", gateKind: "init", status: "in_progress" }]);
    usePlanStore.getState().patchNodeStatus("other", "init", "completed");
    expect(usePlanStore.getState().selectedPlan?.nodes?.[0].status).toBe("in_progress");
  });

  it("is a no-op (no throw) for v1 plans with no nodes", () => {
    seed(undefined);
    usePlanStore.getState().patchNodeStatus("p", "init", "completed");
    expect(usePlanStore.getState().selectedPlan?.nodes).toBeUndefined();
  });
});
