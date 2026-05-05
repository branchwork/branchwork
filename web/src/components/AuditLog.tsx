import { useCallback, useEffect, useState } from "react";
import * as v from "valibot";
import { fetchJson, HttpError } from "../api.js";
import { useAuthStore } from "../stores/auth-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { formatTimestamp } from "../lib/time.js";
import { errorMessage } from "../lib/error.js";
import { parseWsMessage } from "../schemas/ws-events.js";

/// Audit-row diff bodies are server-generated JSON whose shape varies
/// per action. Schema coverage for every action's diff is a v2 concern
/// (audit §12 — `Agent` / `PlanConfig` / `AuditEntry` redeclarations).
/// At this layer we only assert the wire is a JSON object so the
/// renderer's `parsed.<key>` lookups don't blow up on a primitive or
/// array.
const DiffObjectSchema = v.record(v.string(), v.unknown());

interface AuditEntry {
  id: number;
  orgId: string;
  userId: string | null;
  userEmail: string | null;
  action: string;
  resourceType: string;
  resourceId: string | null;
  diff: string | null;
  createdAt: string;
  /// Set by the server for actions that snapshot before mutating
  /// (`plan.delete` and the rearrange-plan primitives that land in
  /// `project-plan-rearrange.yaml`). Drives the Undo affordance below.
  snapshotId?: number | null;
  /// `true` iff `snapshotId` resolves to a `plan_snapshots` row whose
  /// `restored_at IS NULL` AND `expires_at > datetime('now')`. Always
  /// present on the wire (the server defaults to `false`); checked
  /// alongside `snapshotId` because `false` collapses three different
  /// conditions (no snapshot, expired, already restored).
  recoverable: boolean;
  /// Wall-clock from `plan_snapshots.restored_at`. Set after the
  /// snapshot has been replayed via `POST /api/snapshots/:id/restore`.
  restoredAt?: string | null;
}

interface SnapshotRestoreResponse {
  ok: true;
  plan: string;
  snapshotId: number;
  restoredAt: string;
  warning?: string;
}

interface AuditResponse {
  entries: AuditEntry[];
  total: number;
  limit: number;
  offset: number;
}

const PAGE_SIZE = 50;

const ACTION_LABELS: Record<string, string> = {
  "agent.start": "Started agent",
  "agent.kill": "Killed agent",
  "agent.finish": "Finished agent",
  "agent.auto_finish": "Auto-finished agent",
  "task.status_change": "Changed task status",
  "branch.merge": "Merged branch",
  "branch.discard": "Discarded branch",
  "config.effort_change": "Changed effort level",
  "config.budget_change": "Changed budget",
  "config.auto_advance": "Toggled auto-advance",
  "config.auto_mode": "Configured auto-mode",
  "config.project_change": "Changed project",
  "config.kill_switch": "Toggled kill switch",
  "org.member_add": "Added member",
  "org.member_remove": "Removed member",
  "org.member_role_change": "Changed member role",
  "plan.create": "Created plan",
  "plan.update": "Updated plan",
  "plan.delete": "Deleted plan",
  "plan.restore": "Restored plan",
  "auth.signup": "Signed up",
  "auth.login": "Logged in",
  "auto_mode.merged": "Auto-merged task",
  "auto_mode.paused": "Auto-mode paused",
  "auto_mode.fix_spawned": "Spawned fix agent",
  "auto_mode.ci_passed": "CI passed (advanced)",
  "auto_mode.ci_failed": "CI failed",
  "auto_mode.resumed": "Resumed auto-mode",
};

const ACTION_COLORS: Record<string, string> = {
  "agent.start": "text-emerald-400",
  "agent.kill": "text-red-400",
  "agent.finish": "text-blue-400",
  "agent.auto_finish": "text-sky-400",
  "task.status_change": "text-amber-400",
  "branch.merge": "text-indigo-400",
  "branch.discard": "text-orange-400",
  "config.budget_change": "text-yellow-400",
  "config.kill_switch": "text-red-400",
  "org.member_add": "text-emerald-400",
  "org.member_remove": "text-red-400",
  "plan.create": "text-indigo-400",
  "plan.update": "text-blue-400",
  "plan.delete": "text-red-400",
  "plan.restore": "text-emerald-400",
  "auth.signup": "text-emerald-400",
  "auth.login": "text-gray-400",
  "auto_mode.merged": "text-emerald-400",
  "auto_mode.paused": "text-orange-400",
  "auto_mode.fix_spawned": "text-amber-400",
  "auto_mode.ci_passed": "text-sky-400",
  "auto_mode.ci_failed": "text-red-400",
  "auto_mode.resumed": "text-emerald-400",
};

// Single-glyph icon per auto-mode action. Unicode escapes (no emoji
// presentation) — same convention as the Check Plan verdict badge in
// PlanBoard.tsx. Anything not in this map renders an empty placeholder
// so the column still aligns.
const ACTION_ICONS: Record<string, string> = {
  "auto_mode.merged": "✓", // CHECK MARK
  "auto_mode.paused": "■", // BLACK SQUARE
  "auto_mode.fix_spawned": "↺", // ANTICLOCKWISE OPEN CIRCLE ARROW
  "auto_mode.ci_passed": "→", // RIGHTWARDS ARROW
  "auto_mode.ci_failed": "✗", // BALLOT X
  "auto_mode.resumed": "▸", // BLACK RIGHT-POINTING SMALL TRIANGLE
};

function parseDiff(diff: string | null): Record<string, unknown> | null {
  if (!diff) return null;
  let value: unknown;
  try {
    value = JSON.parse(diff);
  } catch {
    return null;
  }
  const result = v.safeParse(DiffObjectSchema, value);
  return result.success ? result.output : null;
}

function autoFinishTriggerLabel(diff: string | null): string {
  const parsed = parseDiff(diff);
  if (!parsed) return "auto";
  const trigger = typeof parsed.trigger === "string" ? parsed.trigger : null;
  if (trigger === "stop_hook") return "Stop hook";
  if (trigger === "idle_timeout") return "idle timeout";
  return "auto";
}

/// Audit actions that snapshot before mutating, so a `snapshotId` on
/// the row points at a `plan_snapshots` entry that can be replayed via
/// `POST /api/snapshots/:id/restore`. `plan.merge`, `plan.archive`,
/// `plan.rename`, `plan.context_rewritten` ship with
/// `project-plan-rearrange.yaml`; including them now means the Undo
/// column lights up the moment those primitives start writing
/// snapshots, with no code change here.
const UNDOABLE_ACTIONS: ReadonlySet<string> = new Set([
  "plan.delete",
  "plan.merge",
  "plan.archive",
  "plan.rename",
  "plan.context_rewritten",
]);

function formatRestoredAt(iso: string): string {
  // The server sends `YYYY-MM-DD HH:MM:SS` (datetime('now')) in UTC.
  // Append Z so JS parses it as UTC and the `formatTimestamp` relative
  // form ("Just now", "5m ago") stays accurate after a restore.
  return formatTimestamp(iso);
}

function DiffSummary({ diff, action }: { diff: string | null; action: string }) {
  const parsed = parseDiff(diff);
  if (!parsed) return null;

  if (action === "task.status_change") {
    return (
      <span className="text-gray-500">
        {String(parsed.from || "pending")} &rarr; {String(parsed.to || "?")}
      </span>
    );
  }
  if (action === "branch.merge") {
    return (
      <span className="text-gray-500 truncate">
        {String(parsed.branch || "")} &rarr; {String(parsed.into || "")}
      </span>
    );
  }
  if (action === "config.budget_change") {
    const val = parsed.maxBudgetUsd;
    return (
      <span className="text-gray-500">
        {val != null ? `$${Number(val).toFixed(2)}` : "cleared"}
      </span>
    );
  }
  if (action === "agent.start") {
    const parts: string[] = [];
    if (parsed.plan) parts.push(String(parsed.plan));
    if (parsed.task) parts.push(`T${parsed.task}`);
    if (parsed.driver && parsed.driver !== "claude")
      parts.push(String(parsed.driver));
    return <span className="text-gray-500 truncate">{parts.join(" / ")}</span>;
  }
  if (action === "org.member_add") {
    return (
      <span className="text-gray-500 truncate">
        {String(parsed.email || "")} as {String(parsed.role || "member")}
      </span>
    );
  }
  if (action === "org.member_role_change") {
    return (
      <span className="text-gray-500 truncate">
        &rarr; {String(parsed.newRole || "?")}
      </span>
    );
  }
  if (action === "config.auto_advance") {
    return (
      <span className="text-gray-500">
        {parsed.enabled ? "enabled" : "disabled"}
      </span>
    );
  }
  if (action === "config.kill_switch") {
    return (
      <span className="text-gray-500">
        {parsed.active ? "activated" : "deactivated"}
        {parsed.reason ? ` — ${String(parsed.reason)}` : ""}
      </span>
    );
  }
  if (action === "config.auto_mode") {
    const parts: string[] = [];
    if (typeof parsed.enabled === "boolean") {
      parts.push(parsed.enabled ? "enabled" : "disabled");
    }
    if (parsed.maxFixAttempts != null) {
      parts.push(`max fix attempts ${String(parsed.maxFixAttempts)}`);
    }
    return <span className="text-gray-500 truncate">{parts.join(", ")}</span>;
  }
  if (action === "auto_mode.merged") {
    const sha = typeof parsed.sha === "string" ? parsed.sha.slice(0, 7) : "";
    const target = parsed.target ? String(parsed.target) : "";
    return (
      <span className="text-gray-500 truncate">
        T{String(parsed.task ?? "?")}{" "}
        {sha && (
          <>
            <span className="font-mono">{sha}</span>{" "}
          </>
        )}
        &rarr; {target || "default"}
      </span>
    );
  }
  if (action === "auto_mode.paused") {
    return (
      <span className="text-gray-500 truncate">
        T{String(parsed.task ?? "?")} — {String(parsed.reason ?? "paused")}
      </span>
    );
  }
  if (action === "auto_mode.fix_spawned") {
    const parts: string[] = [`T${String(parsed.task ?? "?")}`];
    if (parsed.attempt != null) parts.push(`attempt ${String(parsed.attempt)}`);
    if (parsed.ci_run_id) parts.push(`run ${String(parsed.ci_run_id)}`);
    return <span className="text-gray-500 truncate">{parts.join(" / ")}</span>;
  }
  if (action === "auto_mode.ci_passed") {
    const sha = typeof parsed.sha === "string" ? parsed.sha.slice(0, 7) : "";
    const outcome = parsed.outcome ? String(parsed.outcome) : "green";
    return (
      <span className="text-gray-500 truncate">
        T{String(parsed.task ?? "?")} {sha && <span className="font-mono">{sha}</span>}{" "}
        ({outcome})
      </span>
    );
  }
  if (action === "auto_mode.ci_failed") {
    const sha = typeof parsed.sha === "string" ? parsed.sha.slice(0, 7) : "";
    return (
      <span className="text-gray-500 truncate">
        T{String(parsed.task ?? "?")}{" "}
        {sha && <span className="font-mono">{sha}</span>}
        {parsed.ci_run_id ? ` — run ${String(parsed.ci_run_id)}` : ""}
      </span>
    );
  }
  if (action === "auto_mode.resumed") {
    const last = parsed.last_completed_task;
    return (
      <span className="text-gray-500 truncate">
        {last ? `from T${String(last)}` : "from start"}
      </span>
    );
  }
  return null;
}

/// One of three states per row:
/// - Already restored → "Restored at <relative ts>"
/// - Still recoverable → small Undo button + any inline diagnostic
///   from a prior 409/410
/// - Snapshot is gone (expired or hard delete) → faint "no longer
///   recoverable" hint
/// Rows whose action is not in `UNDOABLE_ACTIONS` render nothing
/// (the column is left blank for unrelated activity).
function UndoCell({
  entry,
  busy,
  diagnostic,
  onUndo,
}: {
  entry: AuditEntry;
  busy: boolean;
  diagnostic: string | undefined;
  onUndo: () => void;
}) {
  if (!UNDOABLE_ACTIONS.has(entry.action)) return null;
  if (entry.restoredAt) {
    return (
      <span className="text-emerald-400/80 text-[11px]">
        Restored {formatRestoredAt(entry.restoredAt)}
      </span>
    );
  }
  if (entry.snapshotId == null || !entry.recoverable) {
    return (
      <span className="text-gray-600 text-[11px] italic">
        no longer recoverable
      </span>
    );
  }
  return (
    <span className="inline-flex items-center gap-2">
      <button
        onClick={onUndo}
        disabled={busy}
        className="text-[11px] px-2 py-0.5 rounded border border-indigo-700/60 bg-indigo-900/20 text-indigo-200 hover:border-indigo-500 hover:bg-indigo-900/40 disabled:opacity-50 transition"
      >
        {busy ? "Restoring..." : "Undo"}
      </button>
      {diagnostic && (
        <span className="text-[11px] text-amber-400">{diagnostic}</span>
      )}
    </span>
  );
}

export function AuditLog() {
  const user = useAuthStore((s) => s.user);
  const orgSlug = user?.orgId ?? "default-org";
  const connected = useWsStore((s) => s.connected);

  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [actionFilter, setActionFilter] = useState("");
  const [resourceFilter, setResourceFilter] = useState("");

  const [exporting, setExporting] = useState(false);
  /// Per-entry state for in-flight Undo POSTs and any diagnostic the
  /// server returned (409 slug_collision, 410 already_restored). Keyed
  /// by audit entry id so a row's spinner / error survives unrelated
  /// rerenders. Cleared when the entry transitions to `restoredAt`.
  const [restoring, setRestoring] = useState<Record<number, boolean>>({});
  const [undoErrors, setUndoErrors] = useState<Record<number, string>>({});

  const pushToast = usePlanStore((s) => s.pushToast);

  const undoSnapshot = useCallback(
    async (entryId: number, snapshotId: number, planName: string | null) => {
      setRestoring((r) => ({ ...r, [entryId]: true }));
      setUndoErrors((e) => {
        if (!(entryId in e)) return e;
        const { [entryId]: _omit, ...rest } = e;
        return rest;
      });
      try {
        const res = await fetchJson<SnapshotRestoreResponse>(
          `/api/snapshots/${snapshotId}/restore`,
          { method: "POST" },
        );
        // Optimistic patch: flip this row's `restoredAt` so the Undo
        // button hides without waiting for a refetch. The server has
        // already audited a `plan.restore` row that the next refresh
        // (or a future audit_log broadcast) will surface separately.
        setEntries((prev) =>
          prev.map((p) =>
            p.id === entryId
              ? { ...p, recoverable: false, restoredAt: res.restoredAt }
              : p,
          ),
        );
        pushToast({
          kind: "success",
          message: `Restored plan ${res.plan}`,
          ttlMs: 5_000,
        });
      } catch (err) {
        if (err instanceof HttpError) {
          if (err.status === 410) {
            // Snapshot was already replayed (concurrent Undo, or the
            // user reloaded a stale tab). Pull the new restoredAt out
            // of the body so the row updates in place rather than
            // showing a stale Undo button.
            const body = (err.body ?? {}) as {
              restored_at?: string | null;
              error?: string;
            };
            const restoredAt = body.restored_at ?? null;
            setEntries((prev) =>
              prev.map((p) =>
                p.id === entryId
                  ? { ...p, recoverable: false, restoredAt }
                  : p,
              ),
            );
            setUndoErrors((e) => ({
              ...e,
              [entryId]: "already restored",
            }));
          } else if (err.status === 409) {
            // Slug collision — another plan owns the same name now,
            // and the user must rename/delete it before retrying.
            const body = (err.body ?? {}) as { current?: string };
            const current = body.current ? ` (${body.current})` : "";
            setUndoErrors((e) => ({
              ...e,
              [entryId]: `name in use${current}`,
            }));
          } else if (err.status === 404) {
            setUndoErrors((e) => ({
              ...e,
              [entryId]: "snapshot expired",
            }));
            setEntries((prev) =>
              prev.map((p) =>
                p.id === entryId ? { ...p, recoverable: false } : p,
              ),
            );
          } else {
            setUndoErrors((e) => ({
              ...e,
              [entryId]: err.message,
            }));
          }
        } else {
          setUndoErrors((e) => ({
            ...e,
            [entryId]: errorMessage(err),
          }));
        }
        pushToast({
          kind: "error",
          message: planName
            ? `Failed to restore ${planName}`
            : "Failed to restore plan",
          ttlMs: 5_000,
        });
      } finally {
        setRestoring((r) => {
          const { [entryId]: _omit, ...rest } = r;
          return rest;
        });
      }
    },
    [pushToast],
  );

  const fetchLogs = useCallback(
    async (newOffset = 0) => {
      setLoading(true);
      setError(null);
      try {
        const params = new URLSearchParams();
        params.set("limit", String(PAGE_SIZE));
        params.set("offset", String(newOffset));
        if (actionFilter) params.set("action", actionFilter);
        if (resourceFilter) params.set("resource_type", resourceFilter);

        const data = await fetchJson<AuditResponse>(
          `/api/orgs/${orgSlug}/audit-log?${params}`
        );
        setEntries(data.entries);
        setTotal(data.total);
        setOffset(newOffset);
      } catch (e) {
        setError(errorMessage(e));
      } finally {
        setLoading(false);
      }
    },
    [orgSlug, actionFilter, resourceFilter]
  );

  useEffect(() => {
    fetchLogs(0);
  }, [fetchLogs]);

  // Live refresh: listen for audit_log WS events
  useEffect(() => {
    if (!connected) return;
    const socket = useWsStore.getState().socket;
    if (!socket) return;

    const handler = (ev: MessageEvent) => {
      const parsed = parseWsMessage(ev.data);
      if (parsed.ok && parsed.value.type === "audit_log") {
        // Refresh the current page to show the new entry
        fetchLogs(offset);
      }
    };
    socket.addEventListener("message", handler);
    return () => socket.removeEventListener("message", handler);
  }, [connected, offset, fetchLogs]);

  async function handleExport() {
    setExporting(true);
    try {
      const params = new URLSearchParams();
      if (actionFilter) params.set("action", actionFilter);
      if (resourceFilter) params.set("resource_type", resourceFilter);

      const res = await fetch(
        `/api/orgs/${orgSlug}/audit-log/export?${params}`,
        { credentials: "same-origin" }
      );
      if (!res.ok) throw new Error(`Export failed: ${res.status}`);
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `audit-log-${orgSlug}-${new Date().toISOString().slice(0, 10)}.csv`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setExporting(false);
    }
  }

  const totalPages = Math.ceil(total / PAGE_SIZE);
  const currentPage = Math.floor(offset / PAGE_SIZE) + 1;

  // Unique action types for the filter dropdown
  const actionTypes = Object.keys(ACTION_LABELS);
  const resourceTypes = ["agent", "task", "plan", "org", "user", "config"];

  return (
    <div className="h-full flex flex-col">
      {/* Header */}
      <div className="px-6 py-4 border-b border-gray-800 flex items-center justify-between gap-4 flex-shrink-0">
        <div>
          <h2 className="text-lg font-semibold text-gray-100">Audit Log</h2>
          <p className="text-xs text-gray-500 mt-0.5">
            {total} event{total !== 1 ? "s" : ""} recorded
          </p>
        </div>

        <div className="flex items-center gap-2">
          {/* Filters */}
          <select
            value={actionFilter}
            onChange={(e) => setActionFilter(e.target.value)}
            className="text-xs bg-gray-800 border border-gray-700 rounded px-2 py-1 text-gray-300 outline-none focus:border-indigo-600"
          >
            <option value="">All actions</option>
            {actionTypes.map((a) => (
              <option key={a} value={a}>
                {ACTION_LABELS[a] ?? a}
              </option>
            ))}
          </select>
          <select
            value={resourceFilter}
            onChange={(e) => setResourceFilter(e.target.value)}
            className="text-xs bg-gray-800 border border-gray-700 rounded px-2 py-1 text-gray-300 outline-none focus:border-indigo-600"
          >
            <option value="">All resources</option>
            {resourceTypes.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>

          <button
            onClick={handleExport}
            disabled={exporting || total === 0}
            className="text-xs px-3 py-1 bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-400 rounded transition disabled:opacity-50"
          >
            {exporting ? "Exporting..." : "Export CSV"}
          </button>
        </div>
      </div>

      {/* Error */}
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

      {/* Table */}
      <div className="flex-1 overflow-auto">
        {loading && entries.length === 0 ? (
          <div className="flex items-center justify-center h-32 text-gray-600 text-sm">
            Loading...
          </div>
        ) : entries.length === 0 ? (
          <div className="flex items-center justify-center h-32 text-gray-600 text-sm">
            No audit entries found.
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead className="sticky top-0 bg-gray-900 border-b border-gray-800">
              <tr className="text-left text-[10px] uppercase tracking-wider text-gray-500">
                <th className="px-6 py-2 font-medium">When</th>
                <th className="px-3 py-2 font-medium">Who</th>
                <th className="px-3 py-2 font-medium">Action</th>
                <th className="px-3 py-2 font-medium">Resource</th>
                <th className="px-3 py-2 font-medium">Details</th>
                <th className="px-3 py-2 font-medium">Undo</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-800/50">
              {entries.map((e) => (
                <tr
                  key={e.id}
                  className="hover:bg-gray-800/30 transition-colors"
                >
                  <td className="px-6 py-2 text-xs text-gray-500 whitespace-nowrap">
                    {formatTimestamp(e.createdAt)}
                  </td>
                  <td className="px-3 py-2 text-xs text-gray-400 truncate max-w-[160px]">
                    {e.userEmail ?? (
                      <span className="italic text-gray-600">system</span>
                    )}
                  </td>
                  <td className="px-3 py-2 text-xs whitespace-nowrap">
                    <span
                      className={
                        ACTION_COLORS[e.action] ?? "text-gray-400"
                      }
                    >
                      {ACTION_ICONS[e.action] && (
                        <span
                          aria-hidden="true"
                          className="inline-block w-3 mr-1 text-center"
                        >
                          {ACTION_ICONS[e.action]}
                        </span>
                      )}
                      {ACTION_LABELS[e.action] ?? e.action}
                    </span>
                    {e.action === "agent.auto_finish" && (
                      <span className="ml-2 inline-flex items-center px-1.5 py-0.5 rounded border border-sky-700/50 bg-sky-900/30 text-sky-200 text-[10px] uppercase tracking-wide">
                        {autoFinishTriggerLabel(e.diff)}
                      </span>
                    )}
                  </td>
                  <td className="px-3 py-2 text-xs text-gray-500 whitespace-nowrap">
                    <span className="text-gray-600">{e.resourceType}</span>
                    {e.resourceId && (
                      <span className="ml-1 font-mono text-gray-500 truncate max-w-[120px] inline-block align-bottom">
                        {e.resourceId.length > 12
                          ? `${e.resourceId.slice(0, 12)}...`
                          : e.resourceId}
                      </span>
                    )}
                  </td>
                  <td className="px-3 py-2 text-xs truncate max-w-[200px]">
                    <DiffSummary diff={e.diff} action={e.action} />
                  </td>
                  <td className="px-3 py-2 text-xs whitespace-nowrap">
                    <UndoCell
                      entry={e}
                      busy={restoring[e.id] === true}
                      diagnostic={undoErrors[e.id]}
                      onUndo={() =>
                        e.snapshotId != null &&
                        undoSnapshot(e.id, e.snapshotId, e.resourceId)
                      }
                    />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="px-6 py-2 border-t border-gray-800 flex items-center justify-between text-xs text-gray-500 flex-shrink-0">
          <span>
            Page {currentPage} of {totalPages}
          </span>
          <div className="flex gap-1">
            <button
              onClick={() => fetchLogs(offset - PAGE_SIZE)}
              disabled={offset === 0}
              className="px-2 py-1 bg-gray-800 rounded hover:bg-gray-700 disabled:opacity-30 transition"
            >
              Prev
            </button>
            <button
              onClick={() => fetchLogs(offset + PAGE_SIZE)}
              disabled={offset + PAGE_SIZE >= total}
              className="px-2 py-1 bg-gray-800 rounded hover:bg-gray-700 disabled:opacity-30 transition"
            >
              Next
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
