import { create } from "zustand";
import { fetchJson, postJson } from "../api.js";

/// Deployment mode discriminator returned by `GET /api/runners`. Drives
/// whether the `RunnerStatus` indicator renders at all (audit §17 / 4.1):
///
/// - `standalone`: the dashboard owns the local filesystem; no runners are
///   expected. Indicator renders nothing.
/// - `saas`: this deployment expects a remote runner. Indicator surfaces
///   amber/red/emerald based on registration + connection state.
///
/// Server-side rule (`server-rs/src/saas/runner_ws.rs::deployment_mode`):
/// `saas` if either `BRANCHWORK_PUBLIC_URL` is set OR the org already has
/// any runner row; `standalone` otherwise. Server is the only component
/// that knows both signals so the dashboard reads a pre-computed answer
/// instead of duplicating the rule.
export type DeploymentMode = "standalone" | "saas" | "unknown";

export interface Runner {
  id: string;
  name: string | null;
  status: string | null;
  hostname: string | null;
  version: string | null;
  lastSeenAt: string | null;
  createdAt: string | null;
  /// Last-known driver inventory the runner pushed. Persisted server-side
  /// in `runners.drivers_json` so this is set even when `status==offline`.
  /// Empty array means the runner has not reported yet (or the deserialize
  /// failed — server falls back to `[]` rather than failing the row).
  drivers?: RunnerDriverInfo[];
}

interface RunnersResponse {
  runners: Runner[];
  mode?: "standalone" | "saas";
}

const SELECTED_RUNNER_STORAGE_KEY = "branchwork.selectedRunnerId";

function readPersistedSelectedRunnerId(): string | null {
  if (typeof window === "undefined") return null;
  try {
    const v = window.localStorage?.getItem(SELECTED_RUNNER_STORAGE_KEY);
    return v && v.length > 0 ? v : null;
  } catch {
    return null;
  }
}

function writePersistedSelectedRunnerId(id: string | null): void {
  if (typeof window === "undefined") return;
  try {
    if (id) window.localStorage?.setItem(SELECTED_RUNNER_STORAGE_KEY, id);
    else window.localStorage?.removeItem(SELECTED_RUNNER_STORAGE_KEY);
  } catch {
    // Storage may be disabled (private mode, quota, …); selection
    // simply won't persist.
  }
}

/// Server response for `POST /api/runners/tokens`. The 32-byte hex
/// token is shown to the operator exactly once — there is no `GET` to
/// re-read it later (only the SHA-256 hash is persisted server-side).
export interface RunnerTokenIssued {
  token: string;
  runner_name: string;
}

/// Snapshot of one driver's auth state on a runner. Mirrors the
/// server-side `DriverAuthInfo` wire shape (snake_case `state` tag) and
/// is surfaced verbatim by `GET /api/runners` per row, so each row can
/// render its inventory chip without an extra `/api/drivers?runner_id=`
/// round-trip. This is the runner-protocol shape, not the dashboard's
/// `AuthStatus` (which uses a `kind` tag and is built by the backend's
/// `list_drivers_dispatch` for `/api/drivers` callers).
export type RunnerDriverState =
  | { state: "not_installed" }
  | { state: "unauthenticated"; help?: string | null }
  | { state: "oauth"; account?: string | null }
  | { state: "api_key" }
  | { state: "cloud_provider"; provider: string }
  | { state: "unknown" };

export interface RunnerDriverInfo {
  name: string;
  status: RunnerDriverState;
}

interface RunnerStore {
  /// Resolved deployment mode. Starts as `unknown` so the indicator stays
  /// hidden until we know the answer (avoids flashing the amber "register
  /// a runner" prompt on a standalone dashboard during boot).
  mode: DeploymentMode;
  runners: Runner[];
  /// Per-runner driver inventory, keyed by `runner.id`. Populated from the
  /// `runner_drivers` WS event AND from `/api/runners` rows (each row
  /// carries its last-known inventory). The RunnersPage chip reads from
  /// this map; the sidebar's DriverStatusList prefers the typed map for
  /// the selected runner's row but still calls /api/drivers for the
  /// dashboard-shaped `AuthStatus.kind` payload it needs.
  driversByRunnerId: Record<string, RunnerDriverInfo[]>;
  /// Currently-active runner the dashboard targets for driver lookups
  /// and per-runner views. Defaults to `null` (no selection) until the
  /// first runner row is observed; `applyConnected` and `fetchRunners`
  /// auto-select the first runner if none is selected. Persisted to
  /// localStorage so a returning user lands on the same runner they
  /// left on (audit §17 multi-runner UX).
  selectedRunnerId: string | null;
  /// `false` until the first `fetchRunners()` resolves. Indicator subscribes
  /// to this so it can decide whether `mode === "unknown"` is "still loading"
  /// vs "fetch errored".
  loaded: boolean;
  /// ms-since-epoch of the last successful fetch. Mirrors the debounce
  /// contract used by plan/agent/settings stores.
  lastRunnersFetchedAt: number | null;

  fetchRunners: () => Promise<void>;
  /// User-selected runner the rest of the dashboard targets. Persists to
  /// localStorage so the choice survives reloads. Pass `null` to clear.
  setSelectedRunnerId: (id: string | null) => void;
  /// Create a runner enrolment token. The full token is only present
  /// in this response — the dashboard MUST surface it to the operator
  /// immediately (the install-command modal in `RunnersPage`) because
  /// nothing stores it. On success we refetch `/api/runners` so a row
  /// appears once the runner connects via `branchwork-runner --token`.
  createRunnerToken: (runnerName: string) => Promise<RunnerTokenIssued>;
  /// Wired from `ws-store.ts` for `runner_connected`. Optimistically inserts
  /// or updates the row so the indicator flips to emerald the moment the
  /// runner registers, without waiting for a refetch.
  applyConnected: (payload: { runner_id: string; runner_name?: string | null }) => void;
  /// Wired from `ws-store.ts` for `runner_disconnected`. Marks the matching
  /// row offline; the indicator flips amber/red without a refetch.
  applyDisconnected: (payload: { runner_id: string }) => void;
  /// Wired from `ws-store.ts` for `runner_drivers`. Updates the runner's
  /// `lastSeenAt` AND its entry in `driversByRunnerId` so the RunnersPage
  /// chip reflects the latest auth state without a refetch. Drivers are
  /// optional: 4.1's call site passes only `runner_id`; 4.5+ passes the
  /// payload's `drivers` array as well.
  applyDriversTouch: (payload: {
    runner_id: string;
    drivers?: RunnerDriverInfo[];
  }) => void;
  /// Drop everything back to its initial shape. Driven by `reset-all.ts` on
  /// logout so user A's runner inventory doesn't bleed into user B's tab.
  reset: () => void;
}

let inFlightRunnersFetch: Promise<void> | null = null;

export function getInFlightRunnersFetch(): Promise<void> | null {
  return inFlightRunnersFetch;
}

const INITIAL_STATE: Pick<
  RunnerStore,
  | "mode"
  | "runners"
  | "loaded"
  | "lastRunnersFetchedAt"
  | "driversByRunnerId"
  | "selectedRunnerId"
> = {
  mode: "unknown",
  runners: [],
  loaded: false,
  lastRunnersFetchedAt: null,
  driversByRunnerId: {},
  selectedRunnerId: null,
};

export const useRunnerStore = create<RunnerStore>((set, get) => ({
  ...INITIAL_STATE,
  selectedRunnerId: readPersistedSelectedRunnerId(),

  fetchRunners: () => {
    if (inFlightRunnersFetch) return inFlightRunnersFetch;
    const promise = (async () => {
      try {
        const data = await fetchJson<RunnersResponse>("/api/runners");
        const runners = data.runners ?? [];
        // Build (or rebuild) the per-runner drivers map from the row
        // payload. /api/runners is authoritative for "drivers a runner
        // ever reported" — we drop entries for runners that were removed
        // and refresh entries for runners that just reported.
        const driversByRunnerId: Record<string, RunnerDriverInfo[]> = {};
        for (const r of runners) {
          if (r.drivers && r.drivers.length > 0) {
            driversByRunnerId[r.id] = r.drivers;
          }
        }
        // Seed `selectedRunnerId` to the first runner if the user hasn't
        // chosen one OR the persisted id no longer exists. The
        // most-recent-first server ordering means the freshest runner
        // wins by default.
        const currentSelected = get().selectedRunnerId;
        const selectedStillValid =
          currentSelected !== null &&
          runners.some((r) => r.id === currentSelected);
        const selectedRunnerId = selectedStillValid
          ? currentSelected
          : (runners[0]?.id ?? null);
        if (selectedRunnerId !== currentSelected) {
          writePersistedSelectedRunnerId(selectedRunnerId);
        }
        set({
          mode: data.mode ?? "unknown",
          runners,
          driversByRunnerId,
          selectedRunnerId,
          loaded: true,
          lastRunnersFetchedAt: Date.now(),
        });
      } catch {
        // Leave `loaded` false on error so the indicator stays hidden until
        // a future reconnect/refetch succeeds. A failed fetch is not a SaaS
        // signal; surfacing red on a transient 5xx would be misleading.
        set({ loaded: false });
      }
    })();
    inFlightRunnersFetch = promise;
    promise.finally(() => {
      if (inFlightRunnersFetch === promise) inFlightRunnersFetch = null;
    });
    return promise;
  },

  setSelectedRunnerId: (id) => {
    set({ selectedRunnerId: id });
    writePersistedSelectedRunnerId(id);
  },

  createRunnerToken: async (runnerName) => {
    const issued = await postJson<RunnerTokenIssued>(
      "/api/runners/tokens",
      { runner_name: runnerName },
    );
    // Don't await — the modal needs the token NOW; the runner row only
    // appears once the operator runs `branchwork-runner --token` and the
    // WS handshake lands. The refetch is a best-effort warmup so a row
    // shows up faster if the runner is already running. WS
    // `runner_connected` will reconcile in any case.
    void get().fetchRunners();
    return issued;
  },

  applyConnected: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      // Auto-select the freshly-connected runner if no selection is
      // active yet. This makes the dashboard's first runner the default
      // target without any extra user click.
      const autoSelected =
        s.selectedRunnerId === null ? payload.runner_id : s.selectedRunnerId;
      if (autoSelected !== s.selectedRunnerId) {
        writePersistedSelectedRunnerId(autoSelected);
      }
      if (idx >= 0) {
        const next = s.runners.slice();
        next[idx] = {
          ...next[idx],
          status: "online",
          name: payload.runner_name ?? next[idx].name ?? null,
          lastSeenAt: nowIso,
        };
        return { runners: next, selectedRunnerId: autoSelected };
      }
      // New runner — server's broadcast arrived before any `/api/runners`
      // refetch carried the row. Synthesize a row so the indicator can flip
      // emerald immediately; later refetches reconcile the missing fields.
      return {
        runners: [
          {
            id: payload.runner_id,
            name: payload.runner_name ?? null,
            status: "online",
            hostname: null,
            version: null,
            lastSeenAt: nowIso,
            createdAt: null,
          },
          ...s.runners,
        ],
        // Once any runner is connected, mode is unambiguously SaaS — patch
        // up front so the indicator flips even if `mode` was still `unknown`
        // (e.g. a runner connected mid-bootstrap before fetchRunners landed).
        mode: s.mode === "unknown" ? "saas" : s.mode,
        selectedRunnerId: autoSelected,
      };
    });
  },

  applyDisconnected: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      if (idx < 0) return {};
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        status: "offline",
        lastSeenAt: nowIso,
      };
      return { runners: next };
    });
  },

  applyDriversTouch: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      const nextDriversByRunnerId = payload.drivers
        ? { ...s.driversByRunnerId, [payload.runner_id]: payload.drivers }
        : s.driversByRunnerId;
      if (idx < 0) {
        // Driver report can land before the runner row exists — still
        // cache the inventory so the RunnersPage chip is ready when the
        // row arrives via fetchRunners().
        return payload.drivers
          ? { driversByRunnerId: nextDriversByRunnerId }
          : {};
      }
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        lastSeenAt: nowIso,
        drivers: payload.drivers ?? next[idx].drivers,
      };
      return { runners: next, driversByRunnerId: nextDriversByRunnerId };
    });
  },

  reset: () => {
    inFlightRunnersFetch = null;
    writePersistedSelectedRunnerId(null);
    set({ ...INITIAL_STATE });
  },
}));
