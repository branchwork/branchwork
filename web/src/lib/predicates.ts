import type { PlanSummary } from "../stores/plan-store.js";

/// A plan is considered done when every task it advertises is counted
/// as completed or skipped server-side. Mirrors the gate the sidebar
/// and project dashboard use to fold a plan into the "done" group.
///
/// `taskCount > 0` is required because a 0-of-0 plan would collapse to
/// `0 >= 0 = true` and silently appear as done before it has any
/// tasks — surfaced in the audit as a duplicate predicate at
/// `Sidebar.tsx:19` and `ProjectDashboard.tsx:17` where the guard had
/// drifted out of sync once already.
export function isPlanDone(p: PlanSummary): boolean {
  return p.taskCount > 0 && p.doneCount >= p.taskCount;
}
