import { create } from "zustand";
import { fetchJson } from "../api.js";

/// One row in the "Learnings due" panel — mirrors
/// `server-rs/src/api/learnings.rs::PendingLearningRow` verbatim
/// (camelCase wire shape via serde rename_all). Backs the per-row
/// rendering of the dashboard panel plus the drilldown to the failure
/// log.
///
/// `id` is the `ci_failure_events.id` row primary key — used as both
/// the React key and the path parameter for the
/// `GET /api/learnings/pending/{id}/log` drilldown.
///
/// `agentStatus` is the live `agents.status` (running / starting /
/// completed / failed / killed). `null` when the agent row is missing
/// (defensive — the production path only inserts when a live agent
/// owns the branch). The panel renders this as a small chip so the
/// operator can spot a blocked-but-now-failed agent.
///
/// `taskNumber` follows the plan-schema convention (e.g. "1.4"). The
/// brief reserves null for ad-hoc / phase-level failures that aren't
/// tied to a specific task.
export interface PendingLearningRow {
  id: number;
  agentId: string;
  agentStatus: string | null;
  planName: string;
  taskNumber: string | null;
  branch: string;
  runId: string;
  runUrl: string | null;
  workflow: string | null;
  conclusion: string | null;
  failedJob: string | null;
  summary: string | null;
  observedAt: string;
}

interface PendingLearningsResponse {
  items: PendingLearningRow[];
}

/// Cache key for the per-row log drilldown. Avoids a separate fetch
/// when the operator collapses + re-expands the same row inside one
/// session.
type LogStatus = "loading" | "ok" | "error";

interface LogCacheEntry {
  status: LogStatus;
  text: string | null;
  error: string | null;
}

interface LearningsState {
  items: PendingLearningRow[];
  loading: boolean;
  loaded: boolean;
  /// Per-event log cache, keyed by `ci_failure_events.id`. Populated on
  /// first drilldown open; cleared by `reset()`.
  logsByEventId: Record<number, LogCacheEntry | undefined>;
  /// Module-deduped fetch — overlapping callers share one promise.
  fetchPending: () => Promise<void>;
  /// Per-event log fetch, lazy on first expand. Writes through to the
  /// `logsByEventId` cache so the component can render the entry
  /// synchronously on the next render.
  fetchLog: (eventId: number) => Promise<void>;
  /// Test escape hatch — resets every slice and clears in-flight
  /// markers. Called from `lib/reset-all.ts` on logout.
  reset: () => void;
}

let inFlightPendingFetch: Promise<void> | null = null;

/// Per-event in-flight log fetches. Concurrent expand+collapse-and-
/// re-expand should not double-fire the log fetch (which can shell out
/// to `gh` on the runner host and is slow).
const inFlightLogFetches: Map<number, Promise<void>> = new Map();

export const useLearningsStore = create<LearningsState>((set) => ({
  items: [],
  loading: false,
  loaded: false,
  logsByEventId: {},

  fetchPending: async () => {
    if (inFlightPendingFetch) return inFlightPendingFetch;
    set({ loading: true });
    // Capture-then-assign so the inner `.finally()` can compare against
    // the same handle (mirrors the credentials-store pattern).
    let handle: Promise<void> | null = null;
    const promise: Promise<void> = (async () => {
      try {
        const res = await fetchJson<PendingLearningsResponse>("/api/learnings/pending");
        set({ items: res.items ?? [], loading: false, loaded: true });
      } catch {
        // Leave `loaded` false so a subsequent retry re-fires; the
        // panel falls back to its previous items (or empty) until the
        // next attempt lands. `api.ts` already toasted on 401/403.
        set({ loading: false });
      } finally {
        if (inFlightPendingFetch === handle) {
          inFlightPendingFetch = null;
        }
      }
    })();
    handle = promise;
    inFlightPendingFetch = promise;
    return promise;
  },

  fetchLog: async (eventId: number) => {
    const existing = inFlightLogFetches.get(eventId);
    if (existing) return existing;
    set((s) => ({
      logsByEventId: {
        ...s.logsByEventId,
        [eventId]: { status: "loading", text: null, error: null },
      },
    }));
    const promise: Promise<void> = (async () => {
      try {
        const res = await fetch(`/api/learnings/pending/${eventId}/log`, {
          credentials: "same-origin",
        });
        if (!res.ok) {
          const body = await res.text();
          set((s) => ({
            logsByEventId: {
              ...s.logsByEventId,
              [eventId]: {
                status: "error",
                text: null,
                error: body || `HTTP ${res.status}`,
              },
            },
          }));
          return;
        }
        const text = await res.text();
        // Tail to the last 100 lines per the brief — `gh` already caps
        // at ~8 KB but a workflow that emits short lines could push
        // hundreds of lines through that budget. Slicing here keeps the
        // panel scroll height predictable.
        const trimmed = lastNLines(text, 100);
        set((s) => ({
          logsByEventId: {
            ...s.logsByEventId,
            [eventId]: { status: "ok", text: trimmed, error: null },
          },
        }));
      } catch (e) {
        set((s) => ({
          logsByEventId: {
            ...s.logsByEventId,
            [eventId]: {
              status: "error",
              text: null,
              error: e instanceof Error ? e.message : String(e),
            },
          },
        }));
      } finally {
        inFlightLogFetches.delete(eventId);
      }
    })();
    inFlightLogFetches.set(eventId, promise);
    return promise;
  },

  reset: () => {
    inFlightPendingFetch = null;
    inFlightLogFetches.clear();
    set({ items: [], loading: false, loaded: false, logsByEventId: {} });
  },
}));

/// Tail a string to the last `n` lines. Splits on `\n`, slices the
/// last `n`, joins back. Exported for direct unit testing.
export function lastNLines(text: string, n: number): string {
  if (n <= 0 || !text) return text;
  const lines = text.split("\n");
  if (lines.length <= n) return text;
  return lines.slice(lines.length - n).join("\n");
}

/// Read-only accessor used by tests / module consumers to peek the
/// in-flight promise without subscribing to the store. Mirrors the
/// `getInFlightPlansFetch` pattern in `plan-store.ts`.
export function getInFlightPendingFetch(): Promise<void> | null {
  return inFlightPendingFetch;
}

/// Used by tests to reset the module-private in-flight maps between
/// runs. The store's `reset()` already does this but it is also called
/// from `reset-all.ts` (which a unit test would not want to invoke).
export function clearInFlightForTests(): void {
  inFlightPendingFetch = null;
  inFlightLogFetches.clear();
}

// Re-export for downstream consumers that want a typed view of the
// cache entry without re-deriving it.
export type { LogCacheEntry };
