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

export interface ParsedPlan {
  name: string;
  filePath: string;
  title: string;
  context: string;
  project: string | null;
  createdAt: string;
  modifiedAt: string;
  phases: PlanPhase[];
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

interface PlanStore {
  plans: PlanSummary[];
  selectedPlan: ParsedPlan | null;
  loading: boolean;
  warnings: PlanWarning[];
  fetchPlans: () => Promise<void>;
  selectPlan: (name: string) => Promise<void>;
  clearSelectedPlan: () => void;
  updatePlan: (plan: ParsedPlan) => void;
  patchTaskStatus: (planName: string, taskNumber: string, status: string) => void;
  patchTaskCi: (planName: string, taskNumber: string, ci: CiStatus) => void;
  savePlan: (plan: ParsedPlan) => Promise<void>;
  addWarning: (w: PlanWarning) => void;
  dismissWarning: (name: string) => void;
}

export const usePlanStore = create<PlanStore>((set, get) => ({
  plans: [],
  selectedPlan: null,
  loading: false,
  warnings: [],

  fetchPlans: async () => {
    set({ loading: true });
    const plans = await fetchJson<PlanSummary[]>("/api/plans");
    set({ plans, loading: false });
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

  patchTaskStatus: (planName, taskNumber, status) => {
    const { selectedPlan, plans } = get();

    // Patch the selected plan in-place (no refetch)
    if (selectedPlan?.name === planName) {
      const patched = {
        ...selectedPlan,
        phases: selectedPlan.phases.map((p) => ({
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
    const updatedPlans = plans.map((p) => {
      if (p.name !== planName) return p;
      const delta =
        status === "completed" || status === "skipped" ? 1 : 0;
      // We don't know the previous status precisely, so just refetch later
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
}));
