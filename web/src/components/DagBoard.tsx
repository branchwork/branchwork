import { type PlanNode, type PlanTask } from "../stores/plan-store.js";
import { TaskCard } from "./TaskCard.js";
import { GateCard } from "./GateCard.js";
import { SubPlanCard } from "./SubPlanCard.js";
import { CompletedTasksContext } from "../lib/plan-context.js";

/// Map a DAG task node to the `PlanTask` shape `TaskCard` consumes. `number`
/// is the **scoped** node id so TaskCard's agent lookups (`a.task_id ===
/// task.number`) line up with what the DAG scheduler spawns agents with.
/// `filePaths` / `acceptance` / `description` are defaulted so TaskCard's
/// direct field accesses (e.g. `task.filePaths.length`) never throw.
function nodeToTask(node: PlanNode, scopedId: string): PlanTask {
  return {
    number: scopedId,
    title: node.title,
    description: node.description ?? "",
    filePaths: node.filePaths ?? [],
    acceptance: node.acceptance ?? "",
    dependencies: node.dependsOn ?? [],
    producesCommit: node.producesCommit,
    status: node.status,
    statusUpdatedAt: node.statusUpdatedAt,
    costUsd: node.costUsd,
    ci: null,
  };
}

/// Render one DAG node by its `type` discriminator: gate → diamond `GateCard`,
/// sub-plan → collapsible `SubPlanCard` (recursing through this same dispatcher
/// for its children), task → full-width `TaskCard`. Passing this back into
/// `SubPlanCard` as its `renderChild` keeps the dispatch in one place without
/// an import cycle.
function DagNodeRenderer({
  planName,
  node,
  scopedId,
}: {
  planName: string;
  node: PlanNode;
  scopedId: string;
}) {
  if (node.type === "gate") {
    return <GateCard planName={planName} nodeId={scopedId} node={node} />;
  }
  if (node.type === "sub_plan") {
    return (
      <SubPlanCard
        node={node}
        scopedId={scopedId}
        renderChild={(child, childScopedId) => (
          <DagNodeRenderer planName={planName} node={child} scopedId={childScopedId} />
        )}
      />
    );
  }
  // Task node — full-width TaskCard. phaseNumber=1 maps to the single phase
  // the v2→ParsedPlan conversion produces (used only for the cadence tooltip).
  return <TaskCard task={nodeToTask(node, scopedId)} planName={planName} phaseNumber={1} />;
}

/// The v2 (DAG) board: a top-to-bottom rendering of the node graph. Gate nodes
/// render as diamond `GateCard`s, task nodes as full-width `TaskCard`s, and
/// sub-plan nodes as collapsible `SubPlanCard` groups (recursing with scoped
/// ids). Shares the `CompletedTasksContext` so TaskCard dependency gating reads
/// the plan-wide done-node set.
///
/// Lazy-loaded by `PlanBoard` (see its `DagBoard` lazy import) so the
/// gate/sub-plan rendering code stays out of the first-paint entry chunk for
/// the v1 (phase) plan path — the entry chunk gzipped budget is enforced by
/// `scripts/check-bundle-size.ts`. Mirrors the MultiPlanBoard lazy boundary.
export function DagBoard({
  planName,
  nodes,
  completedSet,
}: {
  planName: string;
  nodes: PlanNode[];
  completedSet: ReadonlySet<string>;
}) {
  return (
    <CompletedTasksContext.Provider value={completedSet}>
      <div className="space-y-2 pb-4">
        {nodes.map((node) => (
          <DagNodeRenderer key={node.id} planName={planName} node={node} scopedId={node.id} />
        ))}
      </div>
    </CompletedTasksContext.Provider>
  );
}
