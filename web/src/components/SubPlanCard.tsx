import { useState, type ReactNode } from "react";
import { type PlanNode } from "../stores/plan-store.js";
import { subPlanProgress } from "../lib/dag-nodes.js";

interface SubPlanCardProps {
  /// The `sub_plan` node to render.
  node: PlanNode;
  /// This sub-plan's **scoped** id (`parent` at the top level, `grand.parent`
  /// when nested) — the prefix every child's scoped id is built from.
  scopedId: string;
  /// Render one child node. The caller (DagBoard's `DagNodeRenderer`) supplies
  /// this so `SubPlanCard` stays agnostic of the task/gate/sub-plan dispatch
  /// and no import cycle forms (`DagBoard` → `SubPlanCard` → … → `DagBoard`).
  /// Each child is delegated, transitively, to `TaskCard` / `GateCard` / a
  /// nested `SubPlanCard`.
  renderChild: (child: PlanNode, childScopedId: string) => ReactNode;
}

/// A DAG `sub_plan` node rendered as a collapsible group (Task 5.1 of
/// dag-based-plan-model).
///
/// - **Collapsed** shows the title plus aggregate task progress
///   ("3/5 tasks done") so a done sub-plan reads at a glance.
/// - **Expanded** renders each child node (via `renderChild`) inside an
///   indented, subtle-bordered box that visually groups them one level below
///   the surrounding flow.
///
/// Defaults open while work is outstanding and collapsed once every task
/// descendant is done (mirrors the done-section collapse convention); the
/// user's manual toggle wins from there.
export function SubPlanCard({ node, scopedId, renderChild }: SubPlanCardProps) {
  const children = node.nodes ?? [];
  const { done, total } = subPlanProgress(node);
  // useState initializer runs once on mount, so a later WS status change won't
  // yank the panel shut under the user.
  const [expanded, setExpanded] = useState(total === 0 ? false : done < total);

  // "Completed" reflects EITHER the server's propagated sub-plan node status
  // (the `sub_plan_completed` / `node_status_changed` WS events → `patchNodeStatus`
  // set `node.status = "completed"` once every direct child finishes — Task 5.2)
  // OR every task descendant being done. The node status is authoritative: it
  // accounts for non-task children (gates, nested sub-plans) the task-only
  // progress count ignores, so a sub-plan that contains gates still flips to
  // completed when its WS event arrives.
  const allDone = node.status === "completed" || (total > 0 && done === total);
  const progressLabel = `${done}/${total} task${total === 1 ? "" : "s"} done`;

  return (
    <div
      className="my-2 rounded-lg border border-indigo-800/40 bg-indigo-950/20"
      data-testid="sub-plan-card"
      data-expanded={expanded}
    >
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition hover:bg-indigo-900/20"
        title={expanded ? "Collapse sub-plan" : "Expand sub-plan"}
      >
        <span aria-hidden="true" className="text-indigo-400">
          {expanded ? "▾" : "▸"}
        </span>
        <span className="flex-1 truncate text-xs font-semibold uppercase tracking-wide text-indigo-300">
          {node.title || scopedId}
        </span>
        <span
          data-testid="sub-plan-progress"
          className={`flex-shrink-0 rounded-full border px-2 py-0.5 text-[10px] font-medium ${
            allDone
              ? "border-emerald-700/50 bg-emerald-900/40 text-emerald-300"
              : "border-indigo-700/50 bg-indigo-900/40 text-indigo-200"
          }`}
        >
          {progressLabel}
        </span>
      </button>
      {expanded && (
        <div
          className="space-y-2 border-t border-indigo-800/30 py-2 pl-5 pr-3"
          data-testid="sub-plan-children"
        >
          {children.length === 0 ? (
            <div className="text-[11px] italic text-gray-600">No nodes in this sub-plan.</div>
          ) : (
            children.map((child) => (
              <div key={child.id}>{renderChild(child, `${scopedId}.${child.id}`)}</div>
            ))
          )}
        </div>
      )}
    </div>
  );
}
