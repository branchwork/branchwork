import type { PlanNode } from "../stores/plan-store.js";

/// Pure helpers over a DAG (`schema_version: 2`) node tree. Kept free of JSX so
/// they are cheap to unit-test and can be shared between the entry chunk
/// (`PlanBoard` header stats) and the lazy DAG board chunk (`DagBoard` /
/// `SubPlanCard`).

/// Recursively collect every task node from a DAG node tree, descending into
/// sub-plans. Used for "N/M tasks done" progress in the plan header and the
/// `SubPlanCard` summary — gate and sub-plan nodes are structural and excluded.
export function collectTaskNodes(nodes: PlanNode[]): PlanNode[] {
  const out: PlanNode[] = [];
  const walk = (ns: PlanNode[]) => {
    for (const n of ns) {
      if (n.type === "task") out.push(n);
      if (n.nodes && n.nodes.length > 0) walk(n.nodes);
    }
  };
  walk(nodes);
  return out;
}

/// Aggregate task progress for a sub-plan node: how many of its (recursively
/// flattened) task descendants are completed/skipped, over the total. Gates
/// don't count. A sub-plan with no task descendants reports `0/0`.
export function subPlanProgress(node: PlanNode): { done: number; total: number } {
  const tasks = collectTaskNodes(node.nodes ?? []);
  const done = tasks.filter((t) => t.status === "completed" || t.status === "skipped").length;
  return { done, total: tasks.length };
}
