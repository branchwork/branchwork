import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { fetchJson } from "../api.js";

/// Self-check report shape mirrors `server-rs/src/api/health.rs::HealthStateResponse`.
/// camelCase on the wire via `#[serde(rename_all = "camelCase")]`.
interface HealthState {
  status: "ok" | "warnings";
  checkedAt: string;
  uptimeSeconds: number;
  categories: {
    agents: {
      byStatus: Record<string, number>;
      unknownStatusCount: number;
    };
    taskStatus: {
      byStatus: Record<string, number>;
      unknownStatusCount: number;
      stuckCheckingCount: number;
    };
    branches: {
      lastSweepAt: string | null;
      secondsSinceLastSweep: number | null;
      lastSweepClearedCount: number;
    };
    ciRuns: {
      dismissedCount: number;
      totalCount: number;
    };
  };
  warnings: HealthWarning[];
}

interface HealthWarning {
  category: "agents" | "taskStatus" | "branches" | "ciRuns";
  kind: string;
  count: number;
  message: string;
  affectedPlans: string[];
  recovery: "reset_task_status" | "clean_stale_branches" | "reset_ci_badge" | "boot_required";
}

const POLL_MS = 30_000;

/// Sidebar-footer self-check widget.
///
/// Polls `/api/health/state` every 30 s. Renders a compact pill:
/// emerald "State OK" when the server reports zero warnings, amber
/// "State warnings" otherwise. Click to expand a popover that lists
/// affected plans, each linked to its plan board so the user can run
/// the Phase-1 recovery flows (per-task Reset, Clean Branches, Reset
/// CI badge) directly.
///
/// Failure mode: when the fetch throws (network blip, server restart),
/// we keep showing the last-known state and surface a small `error`
/// indicator. We do NOT pop a toast — this widget polls in the
/// background and a transient blip should not yell at the user.
export function HealthStateIndicator() {
  const [state, setState] = useState<HealthState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const data = await fetchJson<HealthState>("/api/health/state");
      setState(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    refresh();
    const id = window.setInterval(refresh, POLL_MS);
    return () => window.clearInterval(id);
  }, [refresh]);

  // Close popover on click outside.
  const containerRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (!open) return;
    function onDocClick(e: MouseEvent) {
      if (!containerRef.current) return;
      if (!containerRef.current.contains(e.target as Node)) setOpen(false);
    }
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, [open]);

  // Loading: render the pill in a neutral pending state so the footer
  // height is stable across the first poll round-trip.
  if (!state && !error) {
    return (
      <div className="px-2 pb-2" data-testid="health-state-indicator">
        <button
          disabled
          aria-disabled="true"
          className="w-full flex items-center gap-2 px-2 py-1 text-[10px] rounded text-gray-500 bg-gray-900/50"
        >
          <span aria-hidden="true" className="w-1.5 h-1.5 rounded-full bg-gray-600" />
          Checking state…
        </button>
      </div>
    );
  }

  const hasWarnings = !!state && state.warnings.length > 0;
  const tone = error
    ? { dot: "bg-gray-500", text: "text-gray-400", label: "State unknown" }
    : hasWarnings
      ? { dot: "bg-amber-400", text: "text-amber-300", label: `State warnings (${state!.warnings.length})` }
      : { dot: "bg-emerald-500", text: "text-emerald-300", label: "State OK" };

  return (
    <div className="px-2 pb-2 relative" data-testid="health-state-indicator" ref={containerRef}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-label={tone.label}
        data-state-warnings={hasWarnings ? "true" : "false"}
        data-testid="health-state-button"
        className={`w-full flex items-center gap-2 px-2 py-1 text-[10px] rounded transition ${
          hasWarnings
            ? "bg-amber-900/30 hover:bg-amber-900/50 border border-amber-700/40"
            : "hover:bg-gray-800"
        } ${tone.text}`}
        title={
          state?.categories.branches.lastSweepAt
            ? `Last sweep: ${formatRelativeSeconds(state.categories.branches.secondsSinceLastSweep)}`
            : "Sweep telemetry unavailable"
        }
      >
        <span aria-hidden="true" className={`w-1.5 h-1.5 rounded-full ${tone.dot}`} />
        <span className="flex-1 text-left truncate">{tone.label}</span>
        {state && (
          <span aria-hidden="true" className="text-gray-600 text-[8px]">
            {open ? "▾" : "▸"}
          </span>
        )}
      </button>
      {open && state && (
        <HealthStatePopover state={state} error={error} onPlanClick={() => setOpen(false)} />
      )}
    </div>
  );
}

function HealthStatePopover({
  state,
  error,
  onPlanClick,
}: {
  state: HealthState;
  error: string | null;
  onPlanClick: () => void;
}) {
  const sweepAt = state.categories.branches.lastSweepAt;
  const sweepSecs = state.categories.branches.secondsSinceLastSweep;
  return (
    <div
      role="dialog"
      aria-label="State warnings"
      data-testid="health-state-popover"
      className="absolute bottom-full mb-1 left-2 right-2 z-30 bg-gray-900 border border-gray-700 rounded shadow-lg p-2 text-[10px] space-y-1.5"
    >
      {error && (
        <div className="text-amber-400">
          Couldn’t reach the health endpoint — showing last-known data.
        </div>
      )}
      {state.warnings.length === 0 ? (
        <div className="text-emerald-400">All checks green.</div>
      ) : (
        state.warnings.map((w) => <WarningRow key={`${w.category}.${w.kind}`} w={w} onPlanClick={onPlanClick} />)
      )}
      <div className="pt-1.5 border-t border-gray-800 text-gray-600 space-y-0.5">
        <div>
          Agents:{" "}
          <span className="text-gray-400">
            {Object.values(state.categories.agents.byStatus).reduce((a, b) => a + b, 0)}
          </span>{" "}
          · Tasks:{" "}
          <span className="text-gray-400">
            {Object.values(state.categories.taskStatus.byStatus).reduce((a, b) => a + b, 0)}
          </span>{" "}
          · CI runs:{" "}
          <span className="text-gray-400">{state.categories.ciRuns.totalCount}</span>{" "}
          (<span className="text-gray-500">{state.categories.ciRuns.dismissedCount} dismissed</span>)
        </div>
        <div>
          Last orphan sweep:{" "}
          <span className="text-gray-400">{sweepAt ? formatRelativeSeconds(sweepSecs) : "—"}</span>
          {state.categories.branches.lastSweepClearedCount > 0 && (
            <>
              {" · cleared "}
              <span className="text-gray-400">
                {state.categories.branches.lastSweepClearedCount}
              </span>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function WarningRow({ w, onPlanClick }: { w: HealthWarning; onPlanClick: () => void }) {
  return (
    <div className="space-y-0.5">
      <div className="text-amber-300">{w.message}</div>
      {w.affectedPlans.length > 0 && (
        <div className="flex flex-wrap gap-1 pl-1">
          {w.affectedPlans.slice(0, 6).map((name) => (
            <Link
              key={name}
              to={`/plans/${name}`}
              onClick={onPlanClick}
              className="text-indigo-400 hover:text-indigo-300 underline truncate"
              data-testid={`health-warning-plan-${name}`}
            >
              {name}
            </Link>
          ))}
          {w.affectedPlans.length > 6 && (
            <span className="text-gray-500">+{w.affectedPlans.length - 6} more</span>
          )}
        </div>
      )}
      <div className="text-gray-500 italic">
        {recoveryHint(w.recovery)}
      </div>
    </div>
  );
}

function recoveryHint(recovery: HealthWarning["recovery"]): string {
  switch (recovery) {
    case "reset_task_status":
      return "Recovery: open the affected plan and use the kebab → Reset on the wedged task.";
    case "clean_stale_branches":
      return "Recovery: open the plan and use Clean Branches.";
    case "reset_ci_badge":
      return "Recovery: open the plan and dismiss the CI badge with the ✕ button.";
    case "boot_required":
      return "Recovery: this clears once the server completes its boot sweep.";
    default:
      return "";
  }
}

function formatRelativeSeconds(secs: number | null | undefined): string {
  if (secs == null) return "—";
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}
