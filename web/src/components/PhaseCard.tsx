import { useState } from "react";
import type { PlanPhase } from "../stores/plan-store.js";
import { postJson } from "../api.js";
import { TaskCard } from "./TaskCard.js";

interface Props {
  phase: PlanPhase;
  planName: string;
  statusFilter?: string | null;
}

export function PhaseCard({ phase, planName, statusFilter }: Props) {
  const total = phase.tasks.length;
  const done = phase.tasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  ).length;
  const inProgress = phase.tasks.filter(
    (t) => t.status === "in_progress"
  ).length;
  const pending = total - done - inProgress;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const allDone = total > 0 && done === total;

  // Done phases start collapsed, active phases start expanded
  const [expanded, setExpanded] = useState(!allDone);
  const [checking, setChecking] = useState(false);

  const filteredTasks = statusFilter
    ? phase.tasks.filter((t) => (t.status ?? "pending") === statusFilter)
    : phase.tasks;

  async function handleCheckPhase(e: React.MouseEvent) {
    e.stopPropagation();
    const eligible = phase.tasks.filter(
      (t) => !["completed", "skipped", "checking"].includes(t.status ?? "pending")
    ).length;
    if (eligible === 0) return;
    if (!confirm(`Spawn ${eligible} check agents for Phase ${phase.number}? This will use API credits.`))
      return;
    setChecking(true);
    try {
      await postJson(`/api/plans/${planName}/check-all`, { phase: phase.number });
    } catch (e) {
      console.error("Check phase failed:", e);
    } finally {
      setChecking(false);
    }
  }

  return (
    <div
      className={`rounded-lg border transition ${
        allDone
          ? "border-gray-800/50 bg-gray-900/50"
          : "border-gray-800 bg-gray-900"
      }`}
    >
      {/* Phase header — always visible, clickable */}
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full text-left p-3 hover:bg-gray-800/30 transition rounded-lg"
      >
        <div className="flex items-center gap-3">
          {/* Expand arrow */}
          <span className={`text-[10px] text-gray-600 transition-transform ${expanded ? "rotate-90" : ""}`}>
            &#9654;
          </span>

          {/* Phase badge */}
          <span
            className={`text-xs font-mono px-1.5 py-0.5 rounded flex-shrink-0 ${
              allDone
                ? "bg-emerald-600/20 text-emerald-400"
                : inProgress > 0
                ? "bg-amber-600/20 text-amber-400"
                : "bg-indigo-600/20 text-indigo-400"
            }`}
          >
            Phase {phase.number}
          </span>

          {/* Title */}
          <span className={`text-sm font-semibold truncate ${allDone ? "text-gray-500" : "text-gray-200"}`}>
            {phase.title}
          </span>

          {/* Stats */}
          <div className="ml-auto flex items-center gap-3 flex-shrink-0">
            {/* Status counts */}
            <div className="flex items-center gap-2 text-[10px]">
              {done > 0 && (
                <span className="text-emerald-400">{done} done</span>
              )}
              {inProgress > 0 && (
                <span className="text-amber-400">{inProgress} active</span>
              )}
              {pending > 0 && (
                <span className="text-gray-500">{pending} pending</span>
              )}
            </div>

            {/* Progress bar */}
            {total > 0 && (
              <div className="w-24 h-1.5 bg-gray-800 rounded-full overflow-hidden flex-shrink-0">
                <div
                  className={`h-full rounded-full transition-all duration-300 ${
                    allDone ? "bg-emerald-500" : "bg-indigo-500"
                  }`}
                  style={{ width: `${pct}%` }}
                />
              </div>
            )}

            {/* Check phase button (only when there's something to check) */}
            {!allDone && (
              <button
                onClick={handleCheckPhase}
                disabled={checking}
                className="flex-shrink-0 px-2 py-0.5 text-[10px] bg-gray-800 border border-gray-700 hover:border-emerald-600 hover:text-emerald-400 disabled:opacity-50 text-gray-400 rounded transition"
                title="Spawn a check agent for every unfinished task in this phase"
              >
                {checking ? "..." : "Check"}
              </button>
            )}

            {/* Percentage */}
            <span className={`text-xs font-mono w-8 text-right ${allDone ? "text-emerald-400" : "text-gray-500"}`}>
              {pct}%
            </span>
          </div>
        </div>
      </button>

      {/* Expanded: show tasks */}
      {expanded && filteredTasks.length > 0 && (
        <div className="px-3 pb-3 grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-2">
          {filteredTasks.map((task) => (
            <TaskCard
              key={task.number}
              task={task}
              planName={planName}
              phaseNumber={phase.number}
            />
          ))}
        </div>
      )}

      {expanded && filteredTasks.length === 0 && phase.tasks.length > 0 && (
        <div className="px-3 pb-3">
          <p className="text-xs text-gray-600">No matching tasks</p>
        </div>
      )}
    </div>
  );
}
