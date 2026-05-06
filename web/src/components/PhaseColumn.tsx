import { useState } from "react";
import { postJson } from "../api.js";
import { usePlanStore, type PlanPhase } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { TaskCard } from "./TaskCard.js";
import { Button } from "./ui/Button.js";
import { toastError, toastWarn } from "../lib/toast.js";

interface Props {
  phase: PlanPhase;
  planName: string;
  statusFilter?: string | null;
}

interface StartPhaseResponse {
  started: Array<{ taskId: string; agentId: string; branch: string }>;
}

export function PhaseColumn({ phase, planName, statusFilter }: Props) {
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
  const [starting, setStarting] = useState(false);
  const selectedPlan = usePlanStore((s) => s.selectedPlan);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const effort = useSettingsStore((s) => s.effort);

  const filteredTasks = statusFilter
    ? phase.tasks.filter((t) => (t.status ?? "pending") === statusFilter)
    : phase.tasks;

  const activeTasks = filteredTasks.filter(
    (t) => t.status !== "completed" && t.status !== "skipped"
  );
  const doneTasks = filteredTasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  );

  // Ready = pending/failed AND all deps satisfied. Deps can cross phases,
  // so consult the whole plan's completed set.
  const depSet = new Set<string>(
    (selectedPlan?.phases ?? [])
      .flatMap((p) => p.tasks)
      .filter((t) => t.status === "completed" || t.status === "skipped")
      .map((t) => t.number)
  );
  const readyTasks = phase.tasks.filter((t) => {
    const status = t.status ?? "pending";
    if (status !== "pending" && status !== "failed") return false;
    return (t.dependencies ?? []).every((d) => depSet.has(d));
  });
  const readyCount = readyTasks.length;

  async function handleStartPhase() {
    if (readyCount === 0) return;
    setStarting(true);
    try {
      const res = await postJson<StartPhaseResponse>(
        `/api/plans/${planName}/phases/${phase.number}/start`,
        { effort }
      );
      await selectPlan(planName);
      if (res.started.length === 0) {
        toastWarn("No tasks were started");
      }
    } catch (e) {
      toastError(e, "Start phase failed");
    } finally {
      setStarting(false);
    }
  }

  return (
    <div
      className={`flex-shrink-0 w-80 bg-gray-900 rounded-lg border ${
        allDone ? "border-gray-800/50 opacity-75" : "border-gray-800"
      }`}
    >
      {/* Phase header — clickable to collapse */}
      <div
        onClick={() => setCollapsed(!collapsed)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setCollapsed(!collapsed);
          }
        }}
        className="w-full text-left p-3 border-b border-gray-800 hover:bg-gray-800/30 transition cursor-pointer"
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
          {/* Start Phase — spawn agents for all ready tasks in parallel */}
          {readyCount > 0 && (
            <Button
              variant="primary"
              size="sm"
              onClick={(e) => {
                e.stopPropagation();
                handleStartPhase();
              }}
              disabled={starting}
              title={`Spawn ${readyCount} agent${readyCount !== 1 ? "s" : ""} in parallel, each on its own branch`}
              className="ml-auto py-0.5 text-[10px] font-medium"
            >
              {starting ? "Starting..." : `Start Phase (${readyCount})`}
            </Button>
          )}
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
      </div>

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

          {filteredTasks.length === 0 && phase.tasks.length > 0 && (
            <p className="text-xs text-gray-600 p-2">No matching tasks</p>
          )}
          {phase.tasks.length === 0 && (
            <p className="text-xs text-gray-600 p-2">No tasks parsed</p>
          )}
        </div>
      )}
    </div>
  );
}
