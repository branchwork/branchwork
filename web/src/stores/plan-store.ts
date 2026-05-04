import { create } from "zustand";
import { fetchJson, putJson } from "../api.js";

export type CiStatusValue =
  | "pending"
  | "running"
  | "success"
  | "failure"
  | "cancelled"
  | "unknown";

export interface CiStatus {
  /// Row id in the server's `ci_runs` table — passed to the fix-CI endpoint
  /// so the server knows which specific run to recover from.
  id: number;
  status: CiStatusValue;
  conclusion?: string | null;
  runUrl?: string | null;
  commitSha?: string | null;
  updatedAt: string;
}

export interface PlanTask {
  number: string;
  title: string;
  description: string;
  filePaths: string[];
  acceptance: string;
  dependencies?: string[];
  producesCommit?: boolean;
  status?: string;
  statusUpdatedAt?: string;
  agentId?: string;
  costUsd?: number;
  ci?: CiStatus | null;
}

export interface PlanPhase {
  number: number;
  title: string;
  description: string;
  tasks: PlanTask[];
}

export interface PlanVerdict {
  /// Status from the Check Plan agent: completed | in_progress | pending.
  verdict: string;
  reason?: string | null;
  agentId?: string | null;
  checkedAt: string;
}

export interface ParsedPlan {
  name: string;
  filePath: string;
  title: string;
  context: string;
  project: string | null;
  createdAt: string;
  modifiedAt: string;
  phases: PlanPhase[];
  verification?: string | null;
  verdict?: PlanVerdict | null;
  totalCostUsd?: number;
  maxBudgetUsd?: number | null;
}

export interface PlanSummary {
  name: string;
  title: string;
  project: string | null;
  phaseCount: number;
  taskCount: number;
  doneCount: number;
  createdAt: string;
  modifiedAt: string;
  totalCostUsd?: number;
  maxBudgetUsd?: number | null;
}

export interface PlanWarning {
  name: string;
  file: string;
  error: string;
  timestamp: number;
}

export interface PlanConfig {
  autoAdvance: boolean;
  autoMode: boolean;
  maxFixAttempts: number;
  pausedReason: string | null;
}

export interface PlanConfigPatch {
  autoAdvance?: boolean;
  autoMode?: boolean;
  maxFixAttempts?: number;
  /// Explicit `null` clears the loop's self-pause and re-evaluates from the
  /// last completed task. Only the loop sets non-null values; the wire
  /// silently ignores non-null patches here.
  pausedReason?: string | null;
}

/// Live status of the auto-mode loop for a single plan, driven by the
/// `auto_mode_state` / `auto_mode_paused` / `auto_mode_merged` /
/// `auto_mode_fix_spawned` WS events. The pill in PlanBoard renders from
/// this map plus the persistent `PlanConfig` (autoMode / pausedReason) so
/// it survives reconnects: the WS-derived runtime fills in *transient*
/// info (which task is mid-merge, which fix attempt is in flight); the
/// config fills in *persistent* info (paused or not, and why).
export interface AutoModeRuntime {
  state: "merging" | "awaiting_ci" | "fixing_ci" | "advancing" | "paused";
  task?: string | null;
  sha?: string | null;
  reason?: string | null;
  attempt?: number;
}

interface PlanStore {
  plans: PlanSummary[];
  selectedPlan: ParsedPlan | null;
  loading: boolean;
  warnings: PlanWarning[];
  /// Per-plan PlanConfig. Populated by `fetchPlanConfig` on plan open and
  /// updated by PUT responses + WS events that carry pause-state changes.
  /// Read by AutoModeControls (toggles) and AutoModeStatusPill (render).
  planConfigs: Record<string, PlanConfig>;
  /// Per-plan transient runtime state for the auto-mode pill. WS-driven;
  /// not persisted across page reloads. The persistent slice (paused vs
  /// not) lives in `planConfigs[plan].pausedReason`.
  autoModeRuntimes: Record<string, AutoModeRuntime | null>;
  fetchPlans: () => Promise<void>;
  selectPlan: (name: string) => Promise<void>;
  clearSelectedPlan: () => void;
  updatePlan: (plan: ParsedPlan) => void;
  patchTaskStatus: (planName: string, taskNumber: string, status: string) => void;
  patchTaskCi: (planName: string, taskNumber: string, ci: CiStatus) => void;
  patchPlanVerdict: (planName: string, verdict: PlanVerdict) => void;
  savePlan: (plan: ParsedPlan) => Promise<void>;
  addWarning: (w: PlanWarning) => void;
  dismissWarning: (name: string) => void;
  fetchPlanConfig: (planName: string) => Promise<PlanConfig>;
  setPlanConfig: (planName: string, config: PlanConfig) => void;
  patchPlanConfig: (planName: string, patch: Partial<PlanConfig>) => void;
  setAutoModeRuntime: (planName: string, runtime: AutoModeRuntime | null) => void;
}

export const usePlanStore = create<PlanStore>((set, get) => ({
  plans: [],
  selectedPlan: null,
  loading: false,
  warnings: [],
  planConfigs: {},
  autoModeRuntimes: {},

  fetchPlans: async () => {
    // Only flicker the global loading flag on the first load — refetches
    // (ws-driven, visibility-change, etc.) update silently to avoid
    // unmounting the active view while the network round-trip is in flight.
    const wasInitial = get().plans.length === 0;
    if (wasInitial) set({ loading: true });
    try {
      const plans = await fetchJson<PlanSummary[]>("/api/plans");
      // Defensive: a refetch that returns an empty list while we already
      // have populated state is almost always transient (server momentarily
      // can't enumerate, auth blip, race with file watcher, etc.). Keep the
      // last-known-good list and let the next event-driven refetch reconcile.
      // The narrow legitimate case ("user deleted every plan") loses one
      // refresh cycle, which is fine — they'll see the empty state on the
      // next event or page reload.
      if (plans.length === 0 && !wasInitial) {
        console.warn(
          "[plan-store] /api/plans returned empty during refetch; keeping current list",
        );
        set({ loading: false });
        return;
      }
      set({ plans, loading: false });
    } catch (e) {
      // Surface the failure but ensure loading is reset, otherwise the
      // dashboard would render the spinner indefinitely.
      set({ loading: false });
      throw e;
    }
  },

  selectPlan: async (name: string) => {
    // Only show loading state when switching to a different plan — refreshing
    // the current plan updates silently to avoid unmount/scroll reset.
    const { selectedPlan } = get();
    const isRefresh = selectedPlan?.name === name;
    if (!isRefresh) set({ loading: true });
    try {
      const plan = await fetchJson<ParsedPlan>(`/api/plans/${name}`);
      set({ selectedPlan: plan, loading: false });
    } catch (e) {
      set({ loading: false });
      throw e;
    }
  },

  clearSelectedPlan: () => set({ selectedPlan: null }),

  updatePlan: (plan: ParsedPlan) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name === plan.name) {
      set({ selectedPlan: plan });
    }
  },

  patchTaskCi: (planName, taskNumber, ci) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name !== planName) return;
    const patched = {
      ...selectedPlan,
      phases: selectedPlan.phases.map((p) => ({
        ...p,
        tasks: p.tasks.map((t) =>
          t.number === taskNumber ? { ...t, ci } : t
        ),
      })),
    };
    set({ selectedPlan: patched });
  },

  patchPlanVerdict: (planName, verdict) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name !== planName) return;
    set({ selectedPlan: { ...selectedPlan, verdict } });
  },

  patchTaskStatus: (planName, taskNumber, status) => {
    const { selectedPlan, plans } = get();

    // Look up the prior status BEFORE mutating so we can compute a signed delta.
    // Only the selected plan has per-task data in the store; for other plans we
    // fall back to the unsigned +1/0 heuristic and rely on the server refetch
    // in ws-store (task 2.2) to reconcile.
    const isSelected = selectedPlan?.name === planName;
    let prevStatus: string | undefined;
    if (isSelected) {
      for (const phase of selectedPlan!.phases) {
        const task = phase.tasks.find((t) => t.number === taskNumber);
        if (task) {
          prevStatus = task.status;
          break;
        }
      }
    }

    // Patch the selected plan in-place (no refetch)
    if (isSelected) {
      const patched = {
        ...selectedPlan!,
        phases: selectedPlan!.phases.map((p) => ({
          ...p,
          tasks: p.tasks.map((t) =>
            t.number === taskNumber
              ? { ...t, status, statusUpdatedAt: new Date().toISOString() }
              : t
          ),
        })),
      };
      set({ selectedPlan: patched });
    }

    // Patch doneCount in the plan list
    const isDone = status === "completed" || status === "skipped";
    const updatedPlans = plans.map((p) => {
      if (p.name !== planName) return p;
      let delta: number;
      if (isSelected) {
        // Signed delta handles all 4 transitions: pending→done (+1),
        // done→pending/in_progress/failed (-1), completed↔skipped (0),
        // repeated done→done (0).
        const wasDone =
          prevStatus === "completed" || prevStatus === "skipped";
        delta = (isDone ? 1 : 0) - (wasDone ? 1 : 0);
      } else {
        // Non-selected plan: store has no per-task data, so fall back to the
        // unsigned heuristic. Task 2.2 reconciles via a server refetch on
        // task_status_changed events.
        delta = isDone ? 1 : 0;
      }
      return { ...p, doneCount: p.doneCount + delta };
    });
    set({ plans: updatedPlans });
  },

  savePlan: async (plan: ParsedPlan) => {
    await putJson(`/api/plans/${plan.name}`, {
      title: plan.title,
      context: plan.context,
      project: plan.project,
      phases: plan.phases.map((p) => ({
        number: p.number,
        title: p.title,
        description: p.description,
        tasks: p.tasks.map((t) => ({
          number: t.number,
          title: t.title,
          description: t.description,
          filePaths: t.filePaths,
          acceptance: t.acceptance,
          dependencies: t.dependencies ?? [],
          ...(t.producesCommit === false && { producesCommit: false }),
        })),
      })),
    });
    set({ selectedPlan: plan });
  },

  addWarning: (w: PlanWarning) => {
    set((s) => ({
      warnings: [
        ...s.warnings.filter((x) => x.name !== w.name),
        w,
      ],
    }));
  },

  dismissWarning: (name: string) => {
    set((s) => ({
      warnings: s.warnings.filter((w) => w.name !== name),
    }));
  },

  fetchPlanConfig: async (planName: string) => {
    const cfg = await fetchJson<PlanConfig>(`/api/plans/${planName}/config`);
    set((s) => ({ planConfigs: { ...s.planConfigs, [planName]: cfg } }));
    return cfg;
  },

  setPlanConfig: (planName: string, config: PlanConfig) => {
    set((s) => ({ planConfigs: { ...s.planConfigs, [planName]: config } }));
  },

  patchPlanConfig: (planName: string, patch: Partial<PlanConfig>) => {
    set((s) => {
      const prev = s.planConfigs[planName];
      if (!prev) return s;
      return {
        planConfigs: { ...s.planConfigs, [planName]: { ...prev, ...patch } },
      };
    });
  },

  setAutoModeRuntime: (planName, runtime) => {
    set((s) => ({
      autoModeRuntimes: { ...s.autoModeRuntimes, [planName]: runtime },
    }));
  },
}));
