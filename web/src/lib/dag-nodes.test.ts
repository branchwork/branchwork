import { describe, expect, it } from "vitest";
import { collectTaskNodes, subPlanProgress } from "./dag-nodes.js";
import { type PlanNode } from "../stores/plan-store.js";

function task(id: string, status?: string): PlanNode {
  return { id, type: "task", title: id, status };
}
function gate(id: string): PlanNode {
  return { id, type: "gate", title: id, gateKind: "init" };
}
function subPlan(id: string, nodes: PlanNode[]): PlanNode {
  return { id, type: "sub_plan", title: id, nodes };
}

describe("collectTaskNodes", () => {
  it("returns only task nodes at the top level", () => {
    const nodes = [task("a"), gate("g"), task("b")];
    expect(collectTaskNodes(nodes).map((n) => n.id)).toEqual(["a", "b"]);
  });

  it("descends into sub-plans (and nested sub-plans), skipping gates", () => {
    const nodes = [task("a"), subPlan("s1", [task("b"), gate("g1"), subPlan("s2", [task("c")])])];
    expect(collectTaskNodes(nodes).map((n) => n.id)).toEqual(["a", "b", "c"]);
  });

  it("returns an empty array for an empty tree", () => {
    expect(collectTaskNodes([])).toEqual([]);
  });
});

describe("subPlanProgress", () => {
  it("counts completed and skipped task descendants as done", () => {
    const node = subPlan("s", [
      task("a", "completed"),
      task("b", "skipped"),
      task("c", "in_progress"),
      task("d", "pending"),
    ]);
    expect(subPlanProgress(node)).toEqual({ done: 2, total: 4 });
  });

  it("counts task descendants of nested sub-plans and ignores gates", () => {
    const node = subPlan("outer", [
      task("a", "completed"),
      gate("g"),
      subPlan("inner", [task("x", "pending"), task("y", "completed")]),
    ]);
    expect(subPlanProgress(node)).toEqual({ done: 2, total: 3 });
  });

  it("reports 0/0 for a sub-plan with no task descendants", () => {
    expect(subPlanProgress(subPlan("s", [gate("g")]))).toEqual({ done: 0, total: 0 });
    expect(subPlanProgress({ id: "s", type: "sub_plan", title: "s" })).toEqual({
      done: 0,
      total: 0,
    });
  });
});
