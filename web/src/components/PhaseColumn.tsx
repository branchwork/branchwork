import { useState } from "react";
import type { PlanPhase } from "../stores/plan-store.js";
import { TaskCard } from "./TaskCard.js";

interface Props {
  phase: PlanPhase;
  planName: string;
}

export function PhaseColumn({ phase, planName }: Props) {
  const total = phase.tasks.length;
  const done = phase.tasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  ).length;
  const inProgress = phase.tasks.filter(
    (t) => t.status === "in_progress"
  ).length;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const allDone = total > 0 && done === total;

  const [collapsed, setCollapsed] = useState(allDone);
  const [showDoneTasks, setShowDoneTasks] = useState(false);

  const activeTasks = phase.tasks.filter(
    (t) => t.status !== "completed" && t.status !== "skipped"
  );
  const doneTasks = phase.tasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  );

  return (
    <div
      className={`flex-shrink-0 w-80 bg-gray-900 rounded-lg border ${
        allDone ? "border-gray-800/50 opacity-75" : "border-gray-800"
      }`}
    >
      {/* Phase header — clickable to collapse */}
      <button
        onClick={() => setCollapsed(!collapsed)}
        className="w-full text-left p-3 border-b border-gray-800 hover:bg-gray-800/30 transition"
      >
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-gray-600">
            {collapsed ? "\u25B6" : "\u25BC"}
          </span>
          <span
            className={`text-xs font-mono px-1.5 py-0.5 rounded ${
              allDone
                ? "bg-emerald-600/20 text-emerald-400"
                : "bg-indigo-600/20 text-indigo-400"
            }`}
          >
            Phase {phase.number}
          </span>
          <span className="text-xs text-gray-500">
            {done}/{total}
            {inProgress > 0 && (
              <span className="text-amber-400 ml-1">({inProgress} active)</span>
            )}
          </span>
        </div>
        <h3 className="text-sm font-semibold mt-1 truncate" title={phase.title}>
          {phase.title}
        </h3>
        {/* Progress bar */}
        {total > 0 && (
          <div className="mt-2 h-1 bg-gray-800 rounded-full overflow-hidden">
            <div
              className={`h-full rounded-full transition-all duration-300 ${
                allDone ? "bg-emerald-500" : "bg-indigo-500"
              }`}
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
      </button>

      {/* Task cards */}
      {!collapsed && (
        <div className="p-2 space-y-2 max-h-[calc(100vh-280px)] overflow-y-auto">
          {/* Active tasks first */}
          {activeTasks.map((task) => (
            <TaskCard
              key={task.number}
              task={task}
              planName={planName}
              phaseNumber={phase.number}
            />
          ))}

          {/* Done tasks — collapsible */}
          {doneTasks.length > 0 && (
            <div>
              <button
                onClick={() => setShowDoneTasks(!showDoneTasks)}
                className="w-full text-left px-2 py-1.5 text-[11px] text-gray-500 hover:text-gray-400 transition flex items-center gap-1"
              >
                <span className="text-[9px]">{showDoneTasks ? "\u25BC" : "\u25B6"}</span>
                {doneTasks.length} completed task{doneTasks.length !== 1 ? "s" : ""}
              </button>
              {showDoneTasks &&
                doneTasks.map((task) => (
                  <TaskCard
                    key={task.number}
                    task={task}
                    planName={planName}
                    phaseNumber={phase.number}
                  />
                ))}
            </div>
          )}

          {phase.tasks.length === 0 && (
            <p className="text-xs text-gray-600 p-2">No tasks parsed</p>
          )}
        </div>
      )}
    </div>
  );
}
