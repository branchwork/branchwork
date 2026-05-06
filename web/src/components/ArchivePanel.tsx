import { useCallback, useEffect, useMemo, useState } from "react";
import { fetchJson, HttpError } from "../api.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { formatTimestamp } from "../lib/time.js";
import { errorMessage } from "../lib/error.js";
import { Modal } from "./ui/Modal.js";
import { Button } from "./ui/Button.js";

/// One row from `GET /api/snapshots`. Mirrors the Rust
/// `SnapshotListEntry` (camelCased on the wire).
interface SnapshotEntry {
  id: number;
  planName: string;
  kind: string;
  createdAt: string;
  expiresAt: string;
  archivePath: string | null;
  restoredAt: string | null;
}

interface SnapshotListResponse {
  snapshots: SnapshotEntry[];
}

interface SnapshotRestoreResponse {
  ok: true;
  plan: string;
  snapshotId: number;
  restoredAt: string;
  warning?: string;
}

interface SnapshotPurgeResponse {
  ok: true;
  snapshotId: number;
  plan: string;
  warning?: string;
}

const KIND_LABELS: Record<string, string> = {
  delete: "Soft delete",
  merge: "Merge",
  rename: "Rename",
  archive: "Archive",
  rewrite_context: "Context rewrite",
};

/// Ordered for the kind filter dropdown. Includes every kind written
/// by `plan_curate::SnapshotKind` so future primitives surface here
/// without code changes.
const KIND_OPTIONS: string[] = [
  "delete",
  "merge",
  "rename",
  "archive",
  "rewrite_context",
];

/// Render an `expires_at` UTC timestamp ("YYYY-MM-DD HH:MM:SS") as a
/// short countdown chip. Returns `expired` when the deadline has
/// passed (the retention purger will free the row on its next tick).
function formatCountdown(iso: string): { label: string; tone: string } {
  const d = new Date(iso + (iso.endsWith("Z") ? "" : "Z"));
  const now = new Date();
  const diffMs = d.getTime() - now.getTime();
  if (diffMs <= 0) return { label: "expired", tone: "text-gray-600" };
  const days = Math.floor(diffMs / 86_400_000);
  if (days >= 1) {
    return {
      label: `expires in ${days}d`,
      tone: days <= 1 ? "text-amber-400" : "text-gray-400",
    };
  }
  const hours = Math.floor(diffMs / 3_600_000);
  if (hours >= 1) {
    return { label: `expires in ${hours}h`, tone: "text-amber-400" };
  }
  const minutes = Math.max(1, Math.floor(diffMs / 60_000));
  return { label: `expires in ${minutes}m`, tone: "text-red-400" };
}

export function ArchivePanel() {
  const connected = useWsStore((s) => s.connected);
  const pushToast = usePlanStore((s) => s.pushToast);
  const removePlan = usePlanStore((s) => s.removePlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);

  const [entries, setEntries] = useState<SnapshotEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [search, setSearch] = useState("");
  const [kindFilter, setKindFilter] = useState("");

  const [restoring, setRestoring] = useState<Record<number, boolean>>({});
  const [purging, setPurging] = useState<Record<number, boolean>>({});
  /// Snapshot id awaiting second-confirm purge. The modal renders only
  /// while this is set; cancel resets it back to null.
  const [confirmPurge, setConfirmPurge] = useState<SnapshotEntry | null>(null);

  const fetchSnapshots = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchJson<SnapshotListResponse>("/api/snapshots");
      setEntries(data.snapshots);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchSnapshots();
  }, [fetchSnapshots]);

  // Live updates: refetch on any plan_deleted (new soft delete adds a
  // row) or snapshot_purged (auto-purge or another tab purged) event.
  // Restored snapshots (`plan_restored`) also flip a row's restoredAt
  // so we refresh on those too.
  useEffect(() => {
    if (!connected) return;
    const socket = useWsStore.getState().socket;
    if (!socket) return;
    const handler = (ev: MessageEvent) => {
      try {
        const msg = JSON.parse(ev.data);
        if (
          msg.type === "plan_deleted" ||
          msg.type === "snapshot_purged" ||
          msg.type === "plan_restored"
        ) {
          fetchSnapshots();
        }
      } catch {
        // ignore non-JSON
      }
    };
    socket.addEventListener("message", handler);
    return () => socket.removeEventListener("message", handler);
  }, [connected, fetchSnapshots]);

  const filtered = useMemo(() => {
    const q = search.toLowerCase().trim();
    return entries.filter((e) => {
      if (q && !e.planName.toLowerCase().includes(q)) return false;
      if (kindFilter && e.kind !== kindFilter) return false;
      return true;
    });
  }, [entries, search, kindFilter]);

  const restoreSnapshot = useCallback(
    async (entry: SnapshotEntry) => {
      setRestoring((r) => ({ ...r, [entry.id]: true }));
      try {
        const res = await fetchJson<SnapshotRestoreResponse>(
          `/api/snapshots/${entry.id}/restore`,
          { method: "POST" },
        );
        // Optimistically mark the row restored so the Restore button
        // hides without waiting for the next refetch. The next
        // plan_restored / WS refetch will reconcile authoritatively.
        setEntries((prev) =>
          prev.map((p) =>
            p.id === entry.id ? { ...p, restoredAt: res.restoredAt } : p,
          ),
        );
        // Resurrect the plan in the active list so navigating back to
        // Plans surfaces it without a manual reload.
        fetchPlans().catch(() => {});
        pushToast({
          kind: "success",
          message: `Restored plan ${res.plan}`,
          ttlMs: 5_000,
        });
      } catch (err) {
        let message = errorMessage(err);
        if (err instanceof HttpError) {
          if (err.status === 410) {
            message = "snapshot already restored";
            const body = (err.body ?? {}) as { restored_at?: string | null };
            const restoredAt = body.restored_at ?? null;
            setEntries((prev) =>
              prev.map((p) =>
                p.id === entry.id ? { ...p, restoredAt } : p,
              ),
            );
          } else if (err.status === 409) {
            const body = (err.body ?? {}) as { current?: string };
            message = `name in use${body.current ? ` (${body.current})` : ""}`;
          } else if (err.status === 404) {
            message = "snapshot expired";
            setEntries((prev) => prev.filter((p) => p.id !== entry.id));
          }
        }
        pushToast({
          kind: "error",
          message: `Failed to restore ${entry.planName}: ${message}`,
          ttlMs: 7_000,
        });
      } finally {
        setRestoring((r) => {
          const { [entry.id]: _omit, ...rest } = r;
          return rest;
        });
      }
    },
    [pushToast, fetchPlans],
  );

  const purgeSnapshot = useCallback(
    async (entry: SnapshotEntry) => {
      setPurging((p) => ({ ...p, [entry.id]: true }));
      try {
        const res = await fetchJson<SnapshotPurgeResponse>(
          `/api/snapshots/${entry.id}`,
          { method: "DELETE" },
        );
        // Optimistic removal — the snapshot_purged WS broadcast will
        // refetch authoritatively, but this keeps the row from
        // lingering with a stuck spinner during the round-trip.
        setEntries((prev) => prev.filter((p) => p.id !== entry.id));
        // The plan was already gone from the active list (the snapshot
        // existed because it was deleted); but if a parallel tab
        // restored and re-deleted, removePlan is a noop on a missing
        // entry, so call it defensively.
        removePlan(entry.planName);
        pushToast({
          kind: "info",
          message: `Purged snapshot for ${res.plan}`,
          ttlMs: 5_000,
        });
        if (res.warning) {
          pushToast({
            kind: "info",
            message: res.warning,
            ttlMs: 7_000,
          });
        }
      } catch (err) {
        let message = errorMessage(err);
        if (err instanceof HttpError) {
          if (err.status === 404) {
            message = "already purged";
            setEntries((prev) => prev.filter((p) => p.id !== entry.id));
          } else if (err.status === 403) {
            message = "permission denied";
          }
        }
        pushToast({
          kind: "error",
          message: `Failed to purge ${entry.planName}: ${message}`,
          ttlMs: 7_000,
        });
      } finally {
        setPurging((p) => {
          const { [entry.id]: _omit, ...rest } = p;
          return rest;
        });
        setConfirmPurge((current) =>
          current?.id === entry.id ? null : current,
        );
      }
    },
    [pushToast, removePlan],
  );

  return (
    <div className="h-full flex flex-col">
      {/* Header */}
      <div className="px-6 py-4 border-b border-gray-800 flex items-center justify-between gap-4 flex-shrink-0">
        <div>
          <h2 className="text-lg font-semibold text-gray-100">Archive</h2>
          <p className="text-xs text-gray-500 mt-0.5">
            {entries.length} snapshot{entries.length !== 1 ? "s" : ""} pending
            retention.{" "}
            <span className="text-gray-600">
              Restore brings a plan back; Purge removes the snapshot
              immediately and cannot be undone.
            </span>
          </p>
        </div>

        <div className="flex items-center gap-2">
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search plan name..."
            className="text-xs bg-gray-800 border border-gray-700 rounded px-2 py-1 text-gray-300 placeholder-gray-600 outline-none focus:border-indigo-600"
          />
          <select
            value={kindFilter}
            onChange={(e) => setKindFilter(e.target.value)}
            className="text-xs bg-gray-800 border border-gray-700 rounded px-2 py-1 text-gray-300 outline-none focus:border-indigo-600"
          >
            <option value="">All kinds</option>
            {KIND_OPTIONS.map((k) => (
              <option key={k} value={k}>
                {KIND_LABELS[k] ?? k}
              </option>
            ))}
          </select>
          <button
            onClick={fetchSnapshots}
            disabled={loading}
            className="text-xs px-3 py-1 bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-400 rounded transition disabled:opacity-50"
          >
            {loading ? "Refreshing…" : "Refresh"}
          </button>
        </div>
      </div>

      {error && (
        <div className="px-6 py-2 bg-red-900/20 border-b border-red-800/30 text-xs text-red-400 flex items-center justify-between">
          <span>{error}</span>
          <button
            onClick={() => setError(null)}
            className="text-red-600 hover:text-red-400"
          >
            x
          </button>
        </div>
      )}

      {/* Body */}
      <div className="flex-1 overflow-auto">
        {loading && entries.length === 0 ? (
          <div className="flex items-center justify-center h-32 text-gray-600 text-sm">
            Loading…
          </div>
        ) : filtered.length === 0 ? (
          <div className="flex items-center justify-center h-32 text-gray-600 text-sm">
            {entries.length === 0
              ? "No snapshots in retention. Soft-deleted plans show up here."
              : "No snapshots match the current filter."}
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="sticky top-0 bg-gray-900 border-b border-gray-800">
              <tr className="text-left text-[10px] uppercase tracking-wider text-gray-500">
                <th className="px-6 py-2 font-medium">Plan</th>
                <th className="px-3 py-2 font-medium">Kind</th>
                <th className="px-3 py-2 font-medium">Snapshotted</th>
                <th className="px-3 py-2 font-medium">Retention</th>
                <th className="px-3 py-2 font-medium text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-800/50">
              {filtered.map((entry) => {
                const restored = entry.restoredAt != null;
                const countdown = formatCountdown(entry.expiresAt);
                const restoreBusy = restoring[entry.id] === true;
                const purgeBusy = purging[entry.id] === true;
                return (
                  <tr
                    key={entry.id}
                    className={`transition-colors ${
                      restored
                        ? "bg-gray-900/50 text-gray-600"
                        : "hover:bg-gray-800/30"
                    }`}
                  >
                    <td className="px-6 py-2 text-xs">
                      <div
                        className={`font-medium truncate ${
                          restored ? "text-gray-500" : "text-gray-200"
                        }`}
                      >
                        {entry.planName}
                      </div>
                      {entry.archivePath && (
                        <div
                          className="text-[10px] text-gray-600 font-mono truncate max-w-[280px]"
                          title={entry.archivePath}
                        >
                          {entry.archivePath}
                        </div>
                      )}
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-400 whitespace-nowrap">
                      {KIND_LABELS[entry.kind] ?? entry.kind}
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-500 whitespace-nowrap">
                      {formatTimestamp(entry.createdAt)}
                    </td>
                    <td className="px-3 py-2 text-xs whitespace-nowrap">
                      {restored ? (
                        <span className="text-emerald-400/80">
                          Restored {formatTimestamp(entry.restoredAt!)}
                        </span>
                      ) : (
                        <span className={countdown.tone}>
                          {countdown.label}
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2 text-xs whitespace-nowrap text-right">
                      <div className="inline-flex items-center gap-2 justify-end">
                        <button
                          onClick={() => restoreSnapshot(entry)}
                          disabled={restoreBusy || restored || purgeBusy}
                          className="text-[11px] px-2 py-0.5 rounded border border-indigo-700/60 bg-indigo-900/20 text-indigo-200 hover:border-indigo-500 hover:bg-indigo-900/40 disabled:opacity-40 disabled:cursor-not-allowed transition"
                        >
                          {restoreBusy ? "Restoring…" : "Restore"}
                        </button>
                        <button
                          onClick={() => setConfirmPurge(entry)}
                          disabled={purgeBusy || restoreBusy}
                          className="text-[11px] px-2 py-0.5 rounded border border-red-800/60 bg-red-900/20 text-red-300 hover:border-red-600 hover:bg-red-900/40 disabled:opacity-40 disabled:cursor-not-allowed transition"
                        >
                          {purgeBusy ? "Purging…" : "Purge now"}
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      <ConfirmPurgeModal
        entry={confirmPurge}
        busy={confirmPurge ? purging[confirmPurge.id] === true : false}
        onCancel={() => setConfirmPurge(null)}
        onConfirm={() => confirmPurge && purgeSnapshot(confirmPurge)}
      />
    </div>
  );
}

interface ConfirmPurgeModalProps {
  entry: SnapshotEntry | null;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}

function ConfirmPurgeModal({
  entry,
  busy,
  onCancel,
  onConfirm,
}: ConfirmPurgeModalProps) {
  return (
    <Modal
      open={entry !== null}
      onClose={() => {
        if (!busy) onCancel();
      }}
      title={entry ? `Purge snapshot for ${entry.planName}?` : ""}
      description="This will immediately remove the snapshot row and its archived YAML. The plan cannot be restored after this. Use this only to clean up scratch plans you do not want to wait out the retention window for."
    >
      {entry?.archivePath && (
        <p className="mt-2 text-[10px] text-gray-600 font-mono break-all">
          {entry.archivePath}
        </p>
      )}
      <div className="mt-4 flex items-center justify-end gap-2">
        <Button
          variant="ghost"
          size="sm"
          onClick={onCancel}
          disabled={busy}
        >
          Cancel
        </Button>
        <Button
          variant="danger"
          size="sm"
          onClick={onConfirm}
          disabled={busy}
          loading={busy}
        >
          {busy ? "Purging…" : "Purge permanently"}
        </Button>
      </div>
    </Modal>
  );
}
