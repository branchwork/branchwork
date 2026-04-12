import { create } from "zustand";
import { fetchJson } from "../api.js";

export interface PlanTask {
  number: string;
  title: string;
  description: string;
  filePaths: string[];
  acceptance: string;
  status?: string;
  statusUpdatedAt?: string;
  agentId?: string;
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
}

interface PlanStore {
  plans: PlanSummary[];
  selectedPlan: ParsedPlan | null;
  loading: boolean;
  fetchPlans: () => Promise<void>;
  selectPlan: (name: string) => Promise<void>;
  updatePlan: (plan: ParsedPlan) => void;
}

export const usePlanStore = create<PlanStore>((set, get) => ({
  plans: [],
  selectedPlan: null,
  loading: false,

  fetchPlans: async () => {
    set({ loading: true });
    const plans = await fetchJson<PlanSummary[]>("/api/plans");
    set({ plans, loading: false });
  },

  selectPlan: async (name: string) => {
    set({ loading: true });
    const plan = await fetchJson<ParsedPlan>(`/api/plans/${name}`);
    set({ selectedPlan: plan, loading: false });
  },

  updatePlan: (plan: ParsedPlan) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name === plan.name) {
      set({ selectedPlan: plan });
    }
  },
}));
