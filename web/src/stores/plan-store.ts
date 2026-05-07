import { create } from "zustand";
import { fetchJson, putJson } from "../api.js";

export type CiStatusValue = "pending" | "running" | "success" | "failure" | "cancelled" | "unknown";

export interface CiStatus {
  /// Row id in the server's `ci_runs` table — passed to the fix-CI endpoint
  /// so the server knows which specific run to recover from.
  id: number;
  status: CiStatusValue;
  conclusion?: string | null;
  runUrl?: string | null;
  commitSha?: string | null;
  updatedAt: string;
  /// Set when the surfaced row belongs to a fix attempt (`<task>-fix-<N>`)
  /// rather than the canonical task itself. Lets the badge tooltip make
  /// "this task is green via fix attempt N" explicit instead of silently
  /// claiming the original task passed.
  viaFixAttempt?: number | null;
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
  /// Per-plan opt-in for fan-out spawn (3.5.2). Toggling to true is rejected
  /// at the API layer with 412 until worktree-per-agent isolation ships
  /// (3.5.3) — the UI renders the switch disabled until then.
  parallel: boolean;
  /// Per-plan runner pin (T11.4). `null` = "any online runner" (today's
  /// behaviour); set = pin every spawn for this plan to that runner. The
  /// dispatcher pauses the plan with `paused_reason='runner_offline'` if
  /// the pinned runner is offline at spawn time.
  runnerId: string | null;
}

export interface PlanConfigPatch {
  autoAdvance?: boolean;
  autoMode?: boolean;
  maxFixAttempts?: number;
  parallel?: boolean;
  /// Explicit `null` clears the loop's self-pause and re-evaluates from the
  /// last completed task. Only the loop sets non-null values; the wire
  /// silently ignores non-null patches here.
  pausedReason?: string | null;
  /// Per-plan runner pin (T11.4). Three states: `undefined` (don't touch),
  /// explicit `null` (clear pin = "any online runner"), or a runner id
  /// string (pin to that runner).
  runnerId?: string | null;
}

/// Live status of the auto-mode loop for a single plan, driven by the
/// `auto_mode_state` / `auto_mode_paused` / `auto_mode_merged` /
/// `auto_mode_fix_spawned` WS events. The pill in PlanBoard renders from
/// this map plus the persistent `PlanConfig` (autoMode / pausedReason) so
/// it survives reconnects: the WS-derived runtime fills in *transient*
/// info (which task is mid-merge, which fix attempt is in flight); the
/// config fills in *persistent* info (paused or not, and why).
export interface AutoModeRuntime {
  state: "auto_finishing" | "merging" | "awaiting_ci" | "fixing_ci" | "advancing" | "paused";
  task?: string | null;
  sha?: string | null;
  reason?: string | null;
  attempt?: number;
}

export type ToastKind = "info" | "error" | "success";

/// Optional inline action attached to a toast. When `snapshotId` is set
/// the renderer wires the button to `POST /api/snapshots/{snapshotId}/restore`
/// (the Undo affordance for soft-deleted plans). Kept generic so future
/// destructive primitives that snapshot can reuse the same shape.
export interface ToastAction {
  label: string;
  snapshotId?: string;
}

export interface Toast {
  id: string;
  kind: ToastKind;
  message: string;
  action?: ToastAction;
}

export interface PushToastInput {
  id?: string;
  kind: ToastKind;
  message: string;
  action?: ToastAction;
  /// Auto-dismiss after this many ms. Omit (or 0) to keep the toast
  /// pinned until `dismissToast` is called.
  ttlMs?: number;
}

/// Body returned by `DELETE /api/plans/:name`. Mirrors the camelCase
/// shape from `api/plans.rs::delete_plan`. The Undo affordance lives
/// on the WS event (toast push) — this body is mostly for the caller
/// that wants to surface the snapshot id or warning inline.
export interface DeletePlanResponse {
  ok: true;
  name: string;
  snapshotId: number | null;
  archivePath: string | null;
  hard: boolean;
  cascadedRows?: Record<string, number>;
  warning?: string;
}

/// Body returned by `DELETE /api/plans/:name?dry_run=true`. The dry-run
/// path runs every safety gate without touching the FS or DB; the
/// response is the cascade preview the modal renders before the user
/// commits. `blockedBy` carries the gate state — when populated, the
/// modal disables the Delete button proactively (instead of waiting for
/// the real DELETE to 409).
export interface DeletePlanPreview {
  ok: true;
  dryRun: true;
  name: string;
  filePath: string | null;
  hard: boolean;
  cascadeTables: string[];
  /// Per-cascade-table row count. Keys are SQL table names (snake_case
  /// because they ARE the SQL identifiers). Every cascade table is
  /// always present, including those at zero, so a future schema add
  /// shows up in the preview without a UI release.
  wouldDelete: Record<string, number>;
  /// Null when the plan is not blocked. When populated, the object
  /// carries both gate slots — `runningAgents` is always an array
  /// (possibly empty) and `autoModeInFlight` is always a bool.
  blockedBy: {
    runningAgents: string[];
    autoModeInFlight: boolean;
  } | null;
}

interface PlanStore {
  plans: PlanSummary[];
  selectedPlan: ParsedPlan | null;
  loading: boolean;
  /// `false` until the first successful `fetchPlans()` resolves. Distinct
  /// from `loading` (which toggles per fetch) and from `plans.length === 0`
  /// (which can mean "fetched, none exist" *or* "never fetched"). Read by
  /// `<EnsurePlan/>` to decide between rendering a loading state and
  /// surfacing "plan not found" — without it the wrapper can't tell those
  /// two states apart on first nav.
  plansFetched: boolean;
  /// ms-since-epoch when the last `fetchPlans()` resolved (success or empty
  /// guard). Read by ws-store to debounce WS-triggered refetches: a refetch
  /// scheduled within `WS_REFETCH_DEBOUNCE_MS` of the last fetch is skipped
  /// because the in-flight or just-resolved fetch already carries the
  /// server's view. Null until the first successful fetch.
  lastPlansFetchedAt: number | null;
  warnings: PlanWarning[];
  /// Per-plan PlanConfig. Populated by `fetchPlanConfig` on plan open and
  /// updated by PUT responses + WS events that carry pause-state changes.
  /// Read by AutoModeControls (toggles) and AutoModeStatusPill (render).
  planConfigs: Record<string, PlanConfig>;
  /// Per-plan transient runtime state for the auto-mode pill. WS-driven;
  /// not persisted across page reloads. The persistent slice (paused vs
  /// not) lives in `planConfigs[plan].pausedReason`.
  autoModeRuntimes: Record<string, AutoModeRuntime | null>;
  /// Transient toast queue. Driven by ws-store on destructive
  /// operations (e.g. `plan_deleted` pushes an "Undo" toast). The
  /// renderer reads this slice; auto-dismiss is wired into `pushToast`
  /// via `ttlMs`.
  toasts: Toast[];
  fetchPlans: () => Promise<void>;
  selectPlan: (name: string) => Promise<void>;
  clearSelectedPlan: () => void;
  updatePlan: (plan: ParsedPlan) => void;
  /// Drop a plan from the summary list and clear `selectedPlan` if it
  /// matches the gone name (so App.tsx routes back to ProjectDashboard).
  /// Driven by the `plan_deleted` WS event.
  removePlan: (planName: string) => void;
  patchTaskStatus: (planName: string, taskNumber: string, status: string) => void;
  patchTaskCi: (planName: string, taskNumber: string, ci: CiStatus) => void;
  /// Drop the CI badge for a single task on the selected plan. Driven by
  /// `ci_run_dismissed` — the server already wrote `dismissed_at` on the
  /// row; the badge here is the local UI state the dashboard reads from.
  clearTaskCi: (planName: string, taskNumber: string) => void;
  /// Patch a task's reported cost (and bump the plan-list aggregate by
  /// the signed delta). Driven by `task_cost_reported` — agents call this
  /// via the MCP cost-report tool, so the row may not yet exist locally;
  /// missing tasks no-op silently and the next plan refetch reconciles.
  patchTaskCost: (planName: string, taskNumber: string, amountUsd: number) => void;
  patchPlanVerdict: (planName: string, verdict: PlanVerdict) => void;
  savePlan: (plan: ParsedPlan) => Promise<void>;
  addWarning: (w: PlanWarning) => void;
  dismissWarning: (name: string) => void;
  fetchPlanConfig: (planName: string) => Promise<PlanConfig>;
  setPlanConfig: (planName: string, config: PlanConfig) => void;
  patchPlanConfig: (planName: string, patch: Partial<PlanConfig>) => void;
  setAutoModeRuntime: (planName: string, runtime: AutoModeRuntime | null) => void;
  pushToast: (toast: PushToastInput) => string;
  dismissToast: (id: string) => void;
  /// DELETE /api/plans/:name (with `?hard=true` when `opts.hard`).
  /// Throws `HttpError` on non-2xx so callers can branch on 409
  /// (running agents, auto-mode in flight). Does NOT remove the plan
  /// from the local list — that is driven by the `plan_deleted` WS
  /// event (see ws-store.ts) so the sidebar converges identically
  /// whether the delete was triggered from this tab or another.
  deletePlan: (name: string, opts?: { hard?: boolean }) => Promise<DeletePlanResponse>;
  /// DELETE /api/plans/:name?dry_run=true — read-only cascade preview.
  /// Used by `DeletePlanModal` on open so the user sees how many rows
  /// will be cleared and whether the plan is currently blocked. Throws
  /// `HttpError` on non-2xx; the dry-run path itself never 409s, so a
  /// 4xx here means the plan is gone (404) or the request was malformed.
  previewDeletePlan: (name: string) => Promise<DeletePlanPreview>;
  /// Drop every slice back to its initial shape. Driven by
  /// `lib/reset-all.ts::resetAllStores()` after `auth-store.logout()`
  /// completes so user A's plans don't bleed into user B's session in
  /// the same tab. Also clears the in-flight fetch handle so user B's
  /// bootstrap is not coalesced onto user A's still-pending request.
  reset: () => void;
}

/// Module-level handle to the single in-flight `fetchPlans()` round trip.
/// Coalesces concurrent callers (App.tsx bootstrap, ws-store reconnect,
/// visibility-change refetch) onto one network request, and lets ws-store
/// `await` the in-flight fetch before applying optimistic patches so a
/// stale fetch response can't clobber a newer WS event's mutation.
let inFlightPlansFetch: Promise<void> | null = null;

/// Read-only accessor for the in-flight fetchPlans promise. Returns null
/// when no fetch is in flight. Exported so ws-store can defer optimistic
/// patches behind it without poking at module-internal state.
export function getInFlightPlansFetch(): Promise<void> | null {
  return inFlightPlansFetch;
}

export const usePlanStore = create<PlanStore>((set, get) => ({
  plans: [],
  selectedPlan: null,
  loading: false,
  plansFetched: false,
  lastPlansFetchedAt: null,
  warnings: [],
  planConfigs: {},
  autoModeRuntimes: {},
  toasts: [],

  fetchPlans: () => {
    // Coalesce: any caller that arrives while a fetch is in flight gets the
    // same Promise back, so we never have two `/api/plans` requests racing
    // each other (audit §4: previously WS-triggered refetches could overlap
    // App.tsx's bootstrap fetch and the slower one would clobber the faster).
    if (inFlightPlansFetch) return inFlightPlansFetch;

    // Only flicker the global loading flag on the first load — refetches
    // (ws-driven, visibility-change, etc.) update silently to avoid
    // unmounting the active view while the network round-trip is in flight.
    const wasInitial = get().plans.length === 0;
    if (wasInitial) set({ loading: true });

    const promise = (async () => {
      try {
        const plans = await fetchJson<PlanSummary[]>("/api/plans");
        // Defensive: a refetch that returns an empty list while we already
        // have populated state is almost always transient (server momentarily
        // can't enumerate, auth blip, race with file watcher, etc.). Keep the
        // last-known-good list and let the next event-driven refetch
        // reconcile. The narrow legitimate case ("user deleted every plan")
        // loses one refresh cycle, which is fine — they'll see the empty
        // state on the next event or page reload.
        if (plans.length === 0 && !wasInitial) {
          console.warn(
            "[plan-store] /api/plans returned empty during refetch; keeping current list",
          );
          set({
            loading: false,
            plansFetched: true,
            lastPlansFetchedAt: Date.now(),
          });
          return;
        }
        set({
          plans,
          loading: false,
          plansFetched: true,
          lastPlansFetchedAt: Date.now(),
        });
      } catch (e) {
        // Surface the failure but ensure loading is reset, otherwise the
        // dashboard would render the spinner indefinitely. `plansFetched`
        // stays false so EnsurePlan can retry on the next mount; in the
        // worst case the user sees the loading state until the network
        // recovers, which is preferable to a misleading "plan not found".
        set({ loading: false });
        throw e;
      }
    })();

    inFlightPlansFetch = promise;
    promise.finally(() => {
      if (inFlightPlansFetch === promise) inFlightPlansFetch = null;
    });
    return promise;
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

  removePlan: (planName: string) => {
    set((s) => ({
      plans: s.plans.filter((p) => p.name !== planName),
      selectedPlan: s.selectedPlan?.name === planName ? null : s.selectedPlan,
    }));
  },

  patchTaskCi: (planName, taskNumber, ci) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name !== planName) return;
    const patched = {
      ...selectedPlan,
      phases: selectedPlan.phases.map((p) => ({
        ...p,
        tasks: p.tasks.map((t) => (t.number === taskNumber ? { ...t, ci } : t)),
      })),
    };
    set({ selectedPlan: patched });
  },

  patchPlanVerdict: (planName, verdict) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name !== planName) return;
    set({ selectedPlan: { ...selectedPlan, verdict } });
  },

  clearTaskCi: (planName, taskNumber) => {
    const { selectedPlan } = get();
    if (selectedPlan?.name !== planName) return;
    const patched = {
      ...selectedPlan,
      phases: selectedPlan.phases.map((p) => ({
        ...p,
        tasks: p.tasks.map((t) => (t.number === taskNumber ? { ...t, ci: null } : t)),
      })),
    };
    set({ selectedPlan: patched });
  },

  patchTaskCost: (planName, taskNumber, amountUsd) => {
    const { selectedPlan, plans } = get();
    let prevCost: number | undefined;
    let touched = false;

    if (selectedPlan?.name === planName) {
      const patchedPhases = selectedPlan.phases.map((p) => ({
        ...p,
        tasks: p.tasks.map((t) => {
          if (t.number !== taskNumber) return t;
          touched = true;
          prevCost = t.costUsd;
          return { ...t, costUsd: amountUsd };
        }),
      }));
      const aggregateDelta = amountUsd - (prevCost ?? 0);
      const patched: ParsedPlan = {
        ...selectedPlan,
        phases: patchedPhases,
        totalCostUsd: (selectedPlan.totalCostUsd ?? 0) + aggregateDelta,
      };
      set({ selectedPlan: patched });
    }

    // Mirror the aggregate delta on the summary list so ProjectDashboard
    // does not need a second refetch. When `prevCost` is unknown (the
    // task lives on a non-selected plan), fall back to additive +N — the
    // debounced fetchPlans on `task_status_changed` reconciles drift.
    const delta = touched ? amountUsd - (prevCost ?? 0) : amountUsd;
    const updatedPlans = plans.map((p) =>
      p.name === planName ? { ...p, totalCostUsd: (p.totalCostUsd ?? 0) + delta } : p,
    );
    set({ plans: updatedPlans });
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
              : t,
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
        const wasDone = prevStatus === "completed" || prevStatus === "skipped";
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
      warnings: [...s.warnings.filter((x) => x.name !== w.name), w],
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

  pushToast: ({ id, kind, message, action, ttlMs }) => {
    const toastId = id ?? `toast-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    set((s) => ({
      toasts: [...s.toasts.filter((t) => t.id !== toastId), { id: toastId, kind, message, action }],
    }));
    if (ttlMs && ttlMs > 0) {
      // Auto-dismiss. If the user already dismissed manually (or the
      // toast was pre-empted by an id collision), the filter in
      // dismissToast becomes a no-op.
      setTimeout(() => {
        get().dismissToast(toastId);
      }, ttlMs);
    }
    return toastId;
  },

  dismissToast: (id: string) => {
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  },

  deletePlan: async (name: string, opts) => {
    const qs = opts?.hard ? "?hard=true" : "";
    return await fetchJson<DeletePlanResponse>(`/api/plans/${name}${qs}`, {
      method: "DELETE",
    });
  },

  previewDeletePlan: async (name: string) => {
    return await fetchJson<DeletePlanPreview>(`/api/plans/${name}?dry_run=true`, {
      method: "DELETE",
    });
  },

  reset: () => {
    // Clear the module-level in-flight handle so a stale user-A fetch
    // does not coalesce user-B's bootstrap onto a now-401-bound promise.
    // The `.finally()` guard in `fetchPlans` checks `=== promise` before
    // nulling, so this assignment is safe even if the prior fetch is
    // still pending.
    inFlightPlansFetch = null;
    set({
      plans: [],
      selectedPlan: null,
      loading: false,
      plansFetched: false,
      lastPlansFetchedAt: null,
      warnings: [],
      planConfigs: {},
      autoModeRuntimes: {},
      toasts: [],
    });
  },
}));
