import { create } from "zustand";
import { fetchJson, postJson } from "../api.js";

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

/// One promotion candidate — a learning that crossed the promotion
/// threshold (rendered into >= N distinct agent sessions within the
/// window). Mirrors `api/learnings.rs::PromotionCandidateRow` (the
/// flattened `PromotionCandidate` plus `appendPreview`).
///
/// `appendPreview` is the exact markdown block the promote flow would
/// append under `## Other rules` in `CLAUDE.md` — the dashboard renders
/// it as the diff preview without any GitHub round trip.
///
/// `promotedPrUrl` is non-null once the candidate has been approved and a
/// PR opened; the panel then renders the PR link instead of a Promote
/// button.
export interface PromotionCandidate {
  learningId: string;
  orgId: string;
  kind: string;
  category: string;
  slug: string;
  bodyMd: string;
  hitCount: number;
  threshold: number;
  windowDays: number;
  createdAt: string;
  promotedPrUrl: string | null;
  promotedAt: string | null;
  appendPreview: string;
}

interface PromotionCandidatesResponse {
  items: PromotionCandidate[];
}

/// Result of a successful `POST /api/learnings/{id}/promote`.
export interface PromoteResult {
  ok: boolean;
  prUrl: string;
  prNumber: number;
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

  /// Promotion candidates (Phase 4.2). Learnings that crossed the
  /// promotion threshold, surfaced in the Promotions admin tab.
  candidates: PromotionCandidate[];
  candidatesLoaded: boolean;
  candidatesLoading: boolean;
  /// Module-deduped fetch — overlapping callers share one promise.
  fetchCandidates: () => Promise<void>;
  /// Approve a candidate: POST the promote request, which opens the PR
  /// server-side and stamps the candidate. Returns the PR url/number on
  /// success; rejects (HttpError) on 4xx/5xx so the caller can surface
  /// the message. Refetches the candidate list on success so the row
  /// flips to "promoted".
  promote: (learningId: string, projectId: string, credentialId?: string) => Promise<PromoteResult>;

  /// Test escape hatch — resets every slice and clears in-flight
  /// markers. Called from `lib/reset-all.ts` on logout.
  reset: () => void;
}

let inFlightPendingFetch: Promise<void> | null = null;
let inFlightCandidatesFetch: Promise<void> | null = null;

/// Per-event in-flight log fetches. Concurrent expand+collapse-and-
/// re-expand should not double-fire the log fetch (which can shell out
/// to `gh` on the runner host and is slow).
const inFlightLogFetches: Map<number, Promise<void>> = new Map();

export const useLearningsStore = create<LearningsState>((set, get) => ({
  items: [],
  loading: false,
  loaded: false,
  logsByEventId: {},
  candidates: [],
  candidatesLoaded: false,
  candidatesLoading: false,

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

  fetchCandidates: async () => {
    if (inFlightCandidatesFetch) return inFlightCandidatesFetch;
    set({ candidatesLoading: true });
    let handle: Promise<void> | null = null;
    const promise: Promise<void> = (async () => {
      try {
        const res = await fetchJson<PromotionCandidatesResponse>(
          "/api/learnings/promotion-candidates",
        );
        set({
          candidates: res.items ?? [],
          candidatesLoading: false,
          candidatesLoaded: true,
        });
      } catch {
        // Leave `candidatesLoaded` false so a retry re-fires; `api.ts`
        // already toasted on 401/403.
        set({ candidatesLoading: false });
      } finally {
        if (inFlightCandidatesFetch === handle) {
          inFlightCandidatesFetch = null;
        }
      }
    })();
    handle = promise;
    inFlightCandidatesFetch = promise;
    return promise;
  },

  promote: async (learningId, projectId, credentialId) => {
    const body: { projectId: string; credentialId?: string } = { projectId };
    if (credentialId) body.credentialId = credentialId;
    const res = await postJson<PromoteResult>(
      `/api/learnings/${encodeURIComponent(learningId)}/promote`,
      body,
    );
    // Optimistically stamp the candidate so the row flips before the
    // WS `learning_promoted` event lands; the next fetch reconciles.
    set((s) => ({
      candidates: s.candidates.map((c) =>
        c.learningId === learningId ? { ...c, promotedPrUrl: res.prUrl } : c,
      ),
    }));
    // Refetch authoritatively (picks up promotedAt + any concurrent
    // changes). Fire-and-forget — the optimistic patch already updated
    // the row.
    get()
      .fetchCandidates()
      .catch(() => {});
    return res;
  },

  reset: () => {
    inFlightPendingFetch = null;
    inFlightCandidatesFetch = null;
    inFlightLogFetches.clear();
    set({
      items: [],
      loading: false,
      loaded: false,
      logsByEventId: {},
      candidates: [],
      candidatesLoaded: false,
      candidatesLoading: false,
    });
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
  inFlightCandidatesFetch = null;
  inFlightLogFetches.clear();
}

// Re-export for downstream consumers that want a typed view of the
// cache entry without re-deriving it.
export type { LogCacheEntry };
