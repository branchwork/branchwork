import { useCallback, useEffect, useRef, useState } from "react";
import { fetchJson } from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { formatRelative } from "../../lib/time.js";
import { Banner } from "../ui/Banner.js";

/// Wire shape of `GET /api/health` after Phase 9.2.
///
/// `status` and `timestamp` are the original fields used by Docker /
/// Helm liveness probes — additive `version`, `uptimeSeconds`,
/// `wsConnections` are consumed only by this tab. All numeric fields
/// fall back to 0 / "unknown" when missing so an older server that
/// hasn't shipped 9.2 still renders without throwing.
interface HealthResponse {
  status?: string;
  version?: string;
  uptimeSeconds?: number;
  wsConnections?: number;
  timestamp?: string;
}

/// Wire shape of `GET /api/runners` (saas/runner_ws.rs::list_runners).
/// We don't reuse the runner-store typing here because the diagnostics
/// fetch is intentionally independent of the cross-app runner cache:
/// the tab is a debug surface that should report what `/api/runners`
/// returns *right now*, not whatever ws-store events have replayed.
interface RunnerRow {
  id: string;
  name: string | null;
  status: string | null;
  hostname: string | null;
  version: string | null;
  lastSeenAt: string | null;
  createdAt: string | null;
}

interface RunnersResponse {
  runners: RunnerRow[];
  mode?: "standalone" | "saas";
}

/// Refresh cadence while the tab is the active document. Pauses
/// automatically when `document.visibilityState !== "visible"` (the
/// poller skips its tick) and resumes via the `visibilitychange`
/// listener — matches the brief's "every 10s while visible; pauses
/// when hidden" contract.
const REFRESH_MS = 10_000;

/// Pretty-print server uptime for the diagnostics card. Renders as
/// `Nd Nh Nm` / `Nh Nm` / `Nm Ns` / `Ns` with whichever pair has
/// non-zero leading magnitude — avoids surfacing leading zeros that
/// the operator has to mentally skip past.
export function formatUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  const s = Math.floor(seconds);
  const days = Math.floor(s / 86_400);
  const hours = Math.floor((s % 86_400) / 3_600);
  const minutes = Math.floor((s % 3_600) / 60);
  const secs = s % 60;
  if (days > 0) return `${days}d ${hours}h ${minutes}m`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${secs}s`;
  return `${secs}s`;
}

interface DiagnosticsTabProps {
  /// Override the refresh interval (used by tests so a 10s timer
  /// doesn't slow the suite). Production callers omit this and get
  /// `REFRESH_MS`.
  refreshMs?: number;
}

/// Read-only triage view: server version, uptime, dashboard WS
/// subscriber count, and the registered-runner table. Refreshes every
/// 10s while the tab is visible; pauses when hidden so a backgrounded
/// browser tab doesn't keep firing fetches.
export function DiagnosticsTab({ refreshMs = REFRESH_MS }: DiagnosticsTabProps = {}) {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [runners, setRunners] = useState<RunnerRow[] | null>(null);
  const [mode, setMode] = useState<"standalone" | "saas" | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const cancelRef = useRef(false);

  const refresh = useCallback(async () => {
    setError(null);
    try {
      const [h, r] = await Promise.all([
        fetchJson<HealthResponse>("/api/health"),
        fetchJson<RunnersResponse>("/api/runners"),
      ]);
      if (cancelRef.current) return;
      setHealth(h);
      setRunners(r.runners ?? []);
      setMode(r.mode ?? null);
    } catch (e) {
      if (cancelRef.current) return;
      setError(errorMessage(e));
    } finally {
      if (!cancelRef.current) setLoading(false);
    }
  }, []);

  // Initial load + visibility-aware polling. The `visibilitychange`
  // listener fires `refresh()` immediately on resume so a tab that's
  // been hidden for hours doesn't surface a stale uptime number for
  // 10s; the interval predicate then carries on while visible.
  useEffect(() => {
    cancelRef.current = false;
    void refresh();

    const tick = () => {
      if (typeof document !== "undefined" && document.visibilityState !== "visible") {
        return;
      }
      void refresh();
    };
    const interval = setInterval(tick, refreshMs);

    const onVisibility = () => {
      if (typeof document !== "undefined" && document.visibilityState === "visible") {
        void refresh();
      }
    };
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibility);
    }

    return () => {
      cancelRef.current = true;
      clearInterval(interval);
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibility);
      }
    };
  }, [refresh, refreshMs]);

  return (
    <div className="max-w-3xl">
      <p className="text-[11px] text-gray-500 mb-4 leading-relaxed">
        Read-only triage view. Refreshes every 10 seconds while this tab is visible; pauses when the
        browser backgrounds it.
      </p>

      {error && <Banner className="mb-4">{error}</Banner>}

      <section
        className="rounded border border-gray-800 bg-gray-900/40 p-4 mb-4"
        aria-labelledby="diagnostics-server-heading"
      >
        <h2 id="diagnostics-server-heading" className="text-sm font-semibold text-gray-200 mb-3">
          Server
        </h2>

        {loading && health === null ? (
          <p className="text-xs text-gray-500" data-testid="diagnostics-loading">
            Loading…
          </p>
        ) : (
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-xs">
            <dt className="text-gray-500">Version</dt>
            <dd className="text-gray-200 font-mono" data-testid="diagnostics-version">
              {health?.version ?? "unknown"}
            </dd>

            <dt className="text-gray-500">Uptime</dt>
            <dd className="text-gray-200 font-mono" data-testid="diagnostics-uptime">
              {formatUptime(health?.uptimeSeconds ?? Number.NaN)}
            </dd>

            <dt className="text-gray-500">WS connections</dt>
            <dd className="text-gray-200 font-mono" data-testid="diagnostics-ws-connections">
              {health?.wsConnections ?? 0}
            </dd>

            {mode && (
              <>
                <dt className="text-gray-500">Deployment</dt>
                <dd className="text-gray-200 font-mono" data-testid="diagnostics-mode">
                  {mode}
                </dd>
              </>
            )}
          </dl>
        )}
      </section>

      <section
        className="rounded border border-gray-800 bg-gray-900/40 p-4"
        aria-labelledby="diagnostics-runners-heading"
      >
        <h2 id="diagnostics-runners-heading" className="text-sm font-semibold text-gray-200 mb-3">
          Runners
        </h2>

        {runners === null ? (
          <p className="text-xs text-gray-500">Loading…</p>
        ) : runners.length === 0 ? (
          <p className="text-xs text-gray-500" data-testid="diagnostics-no-runners">
            {mode === "standalone"
              ? "Standalone deployment — agents run on this server. No remote runners are registered."
              : "No runners registered yet."}
          </p>
        ) : (
          <table className="w-full text-xs" data-testid="diagnostics-runners-table">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-800">
                <th className="font-medium pb-1.5 pr-3">Name</th>
                <th className="font-medium pb-1.5 pr-3">Status</th>
                <th className="font-medium pb-1.5 pr-3">Hostname</th>
                <th className="font-medium pb-1.5 pr-3">Version</th>
                <th className="font-medium pb-1.5">Last seen</th>
              </tr>
            </thead>
            <tbody>
              {runners.map((r) => (
                <tr
                  key={r.id}
                  className="border-b border-gray-900 last:border-b-0"
                  data-testid={`diagnostics-runner-row-${r.id}`}
                >
                  <td className="py-1.5 pr-3 text-gray-200 font-mono">{r.name ?? r.id}</td>
                  <td className="py-1.5 pr-3">
                    <RunnerStatusBadge status={r.status} />
                  </td>
                  <td className="py-1.5 pr-3 text-gray-300 font-mono">{r.hostname ?? "—"}</td>
                  <td className="py-1.5 pr-3 text-gray-300 font-mono">{r.version ?? "—"}</td>
                  <td className="py-1.5 text-gray-400">
                    {r.lastSeenAt ? formatRelative(r.lastSeenAt) : "never"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}

/// One-line status pill that mirrors the runner-status semantics used
/// elsewhere in the dashboard (RunnerStatus chrome indicator). Online
/// is emerald, offline is gray, anything else (rejected / unknown) is
/// amber so the operator can spot it.
function RunnerStatusBadge({ status }: { status: string | null }) {
  const s = (status ?? "unknown").toLowerCase();
  let cls = "inline-flex items-center gap-1.5 text-[11px] px-2 py-0.5 rounded border";
  if (s === "online") {
    cls += " border-emerald-700/50 bg-emerald-900/30 text-emerald-200";
  } else if (s === "offline") {
    cls += " border-gray-700 bg-gray-800 text-gray-400";
  } else {
    cls += " border-amber-700/50 bg-amber-900/30 text-amber-200";
  }
  return (
    <span className={cls}>
      <span
        className={`inline-block w-1.5 h-1.5 rounded-full ${
          s === "online" ? "bg-emerald-400" : s === "offline" ? "bg-gray-500" : "bg-amber-400"
        }`}
        aria-hidden="true"
      />
      {s}
    </span>
  );
}
