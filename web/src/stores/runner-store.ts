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
}

interface RunnersResponse {
  runners: Runner[];
  mode?: "standalone" | "saas";
}

/// Server response for `POST /api/runners/tokens`. The 32-byte hex
/// token is shown to the operator exactly once — there is no `GET` to
/// re-read it later (only the SHA-256 hash is persisted server-side).
export interface RunnerTokenIssued {
  token: string;
  runner_name: string;
}

interface RunnerStore {
  /// Resolved deployment mode. Starts as `unknown` so the indicator stays
  /// hidden until we know the answer (avoids flashing the amber "register
  /// a runner" prompt on a standalone dashboard during boot).
  mode: DeploymentMode;
  runners: Runner[];
  /// `false` until the first `fetchRunners()` resolves. Indicator subscribes
  /// to this so it can decide whether `mode === "unknown"` is "still loading"
  /// vs "fetch errored".
  loaded: boolean;
  /// ms-since-epoch of the last successful fetch. Mirrors the debounce
  /// contract used by plan/agent/settings stores.
  lastRunnersFetchedAt: number | null;

  fetchRunners: () => Promise<void>;
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
  /// Wired from `ws-store.ts` for `runner_drivers`. Today this is a touch-
  /// only signal — `4.5` will surface per-runner driver inventory in the UI;
  /// for `4.1` we only update `lastSeenAt` so the indicator's tooltip stays
  /// fresh.
  applyDriversTouch: (payload: { runner_id: string }) => void;
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
  "mode" | "runners" | "loaded" | "lastRunnersFetchedAt"
> = {
  mode: "unknown",
  runners: [],
  loaded: false,
  lastRunnersFetchedAt: null,
};

export const useRunnerStore = create<RunnerStore>((set, get) => ({
  ...INITIAL_STATE,

  fetchRunners: () => {
    if (inFlightRunnersFetch) return inFlightRunnersFetch;
    const promise = (async () => {
      try {
        const data = await fetchJson<RunnersResponse>("/api/runners");
        set({
          mode: data.mode ?? "unknown",
          runners: data.runners ?? [],
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
      if (idx >= 0) {
        const next = s.runners.slice();
        next[idx] = {
          ...next[idx],
          status: "online",
          name: payload.runner_name ?? next[idx].name ?? null,
          lastSeenAt: nowIso,
        };
        return { runners: next };
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
      if (idx < 0) return {};
      const next = s.runners.slice();
      next[idx] = { ...next[idx], lastSeenAt: nowIso };
      return { runners: next };
    });
  },

  reset: () => {
    inFlightRunnersFetch = null;
    set({ ...INITIAL_STATE });
  },
}));
