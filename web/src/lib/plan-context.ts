import { createContext, useContext } from "react";

/// Plan-wide set of task numbers whose status is `completed` or `skipped`.
/// Computed once per `selectedPlan` change at PlanBoard via `useMemo` and
/// passed via context so per-task dependency-gate checks don't rebuild a
/// fresh `Set` on every TaskCard render. Audit §10 minor: with 100+ tasks
/// per plan the per-card O(N) Set build dominated re-renders.
const EMPTY_COMPLETED_SET: ReadonlySet<string> = new Set<string>();

export const CompletedTasksContext =
  createContext<ReadonlySet<string>>(EMPTY_COMPLETED_SET);

export function useCompletedTasks(): ReadonlySet<string> {
  return useContext(CompletedTasksContext);
}
