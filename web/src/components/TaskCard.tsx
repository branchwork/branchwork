import { useState } from "react";
import type { PlanTask } from "../stores/plan-store.js";
import { postJson, putJson } from "../api.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";

interface Props {
  task: PlanTask;
  planName: string;
  phaseNumber: number;
}

const STATUS_ORDER = ["pending", "in_progress", "completed", "skipped"] as const;

const statusConfig: Record<
  string,
  { label: string; bg: string; dot: string }
> = {
  pending: { label: "Pending", bg: "bg-gray-700 text-gray-300", dot: "bg-gray-400" },
  in_progress: { label: "In Progress", bg: "bg-amber-600/20 text-amber-400", dot: "bg-amber-400 animate-pulse" },
  completed: { label: "Done", bg: "bg-emerald-600/20 text-emerald-400", dot: "bg-emerald-400" },
  failed: { label: "Failed", bg: "bg-red-600/20 text-red-400", dot: "bg-red-400" },
  skipped: { label: "Skipped", bg: "bg-gray-600/20 text-gray-500", dot: "bg-gray-500" },
  checking: { label: "Checking...", bg: "bg-blue-600/20 text-blue-400", dot: "bg-blue-400 animate-pulse" },
};

function timeAgo(iso?: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  const now = new Date();
  const diffMs = now.getTime() - d.getTime();
  const mins = Math.floor(diffMs / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 30) return `${days}d ago`;
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

export function TaskCard({ task, planName, phaseNumber }: Props) {
  const [starting, setStarting] = useState(false);
  const [checking, setChecking] = useState(false);
  const [agentId, setAgentId] = useState<string | null>(null);
  const [showMenu, setShowMenu] = useState(false);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const selectPlan = usePlanStore((s) => s.selectPlan);

  const status = task.status ?? "pending";
  const cfg = statusConfig[status] ?? statusConfig.pending;

  async function handleStart(mode: "start" | "continue" = "start") {
    setStarting(true);
    try {
      const res = await postJson<{ agentId: string }>("/api/actions/start-task", {
        planName,
        phaseNumber,
        taskNumber: task.number,
        mode,
      });
      setAgentId(res.agentId);
      selectAgent(res.agentId);
      await updateStatus("in_progress");
    } catch (e) {
      console.error("Failed to start task:", e);
    } finally {
      setStarting(false);
    }
  }

  async function handleCheck() {
    setChecking(true);
    try {
      const res = await postJson<{ agentId: string }>(
        `/api/plans/${planName}/tasks/${task.number}/check`,
        {}
      );
      setAgentId(res.agentId);
      selectAgent(res.agentId);
      // Refresh plan after a delay to pick up the result
      setTimeout(() => selectPlan(planName), 3000);
      setTimeout(() => selectPlan(planName), 10000);
      setTimeout(() => selectPlan(planName), 30000);
    } catch (e) {
      console.error("Failed to check task:", e);
    } finally {
      setChecking(false);
    }
  }

  async function updateStatus(newStatus: string) {
    try {
      await putJson(`/api/plans/${planName}/tasks/${task.number}/status`, {
        status: newStatus,
      });
      await selectPlan(planName);
    } catch (e) {
      console.error("Failed to update status:", e);
    }
    setShowMenu(false);
  }

  async function cycleStatus() {
    const idx = STATUS_ORDER.indexOf(status as (typeof STATUS_ORDER)[number]);
    const next = STATUS_ORDER[(idx + 1) % STATUS_ORDER.length];
    await updateStatus(next);
  }

  return (
    <div
      className={`rounded-md border p-3 transition ${
        status === "completed"
          ? "bg-gray-800/30 border-gray-800/50 opacity-70"
          : "bg-gray-800/50 border-gray-700/50 hover:border-gray-600"
      }`}
    >
      {/* Header */}
      <div className="flex items-start justify-between gap-2">
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-1.5 flex-wrap">
            <span className="text-[10px] font-mono text-gray-500">
              {task.number}
            </span>
            {/* Clickable status badge */}
            <div className="relative">
              <button
                onClick={cycleStatus}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setShowMenu(!showMenu);
                }}
                className={`text-[10px] px-1.5 py-0.5 rounded cursor-pointer hover:opacity-80 flex items-center gap-1 ${cfg.bg}`}
                title="Click to cycle status, right-click for menu"
              >
                <span className={`w-1.5 h-1.5 rounded-full ${cfg.dot}`} />
                {cfg.label}
              </button>

              {/* Status dropdown menu */}
              {showMenu && (
                <div className="absolute top-6 left-0 z-10 bg-gray-800 border border-gray-700 rounded-md shadow-lg py-1 min-w-[120px]">
                  {Object.entries(statusConfig)
                    .filter(([k]) => k !== "checking")
                    .map(([key, val]) => (
                    <button
                      key={key}
                      onClick={() => updateStatus(key)}
                      className={`w-full text-left px-3 py-1 text-xs hover:bg-gray-700 flex items-center gap-2 ${
                        key === status ? "text-white" : "text-gray-400"
                      }`}
                    >
                      <span className={`w-1.5 h-1.5 rounded-full ${val.dot}`} />
                      {val.label}
                    </button>
                  ))}
                </div>
              )}
            </div>
            {/* Updated at */}
            {task.statusUpdatedAt && (
              <span className="text-[9px] text-gray-600" title={task.statusUpdatedAt}>
                {timeAgo(task.statusUpdatedAt)}
              </span>
            )}
          </div>
          <h4 className="text-sm font-medium mt-0.5 leading-tight">
            {task.title}
          </h4>
        </div>

        {/* Actions */}
        <div className="flex-shrink-0 flex gap-1">
          {/* Check button — always available */}
          <button
            onClick={handleCheck}
            disabled={checking || status === "checking"}
            className="px-2 py-1 text-xs bg-gray-700 hover:bg-gray-600 disabled:opacity-50 text-gray-300 rounded transition"
            title="Spawn an agent to verify this task against the codebase"
          >
            {checking || status === "checking" ? "..." : "Check"}
          </button>

          {/* Start — for pending tasks */}
          {status === "pending" && !agentId && (
            <button
              onClick={() => handleStart("start")}
              disabled={starting}
              className="px-2 py-1 text-xs bg-indigo-600 hover:bg-indigo-500 disabled:bg-gray-700 disabled:text-gray-500 text-white rounded transition"
            >
              {starting ? "..." : "Start"}
            </button>
          )}

          {/* Continue — for in_progress tasks */}
          {status === "in_progress" && !agentId && (
            <button
              onClick={() => handleStart("continue")}
              disabled={starting}
              className="px-2 py-1 text-xs bg-amber-600 hover:bg-amber-500 disabled:bg-gray-700 disabled:text-gray-500 text-white rounded transition"
            >
              {starting ? "..." : "Continue"}
            </button>
          )}

          {/* Retry — for failed tasks */}
          {status === "failed" && !agentId && (
            <button
              onClick={() => handleStart("continue")}
              disabled={starting}
              className="px-2 py-1 text-xs bg-red-700 hover:bg-red-600 disabled:bg-gray-700 disabled:text-gray-500 text-white rounded transition"
            >
              {starting ? "..." : "Retry"}
            </button>
          )}

          {agentId && (
            <button
              onClick={() => selectAgent(agentId)}
              className="px-2 py-1 text-xs bg-gray-700 hover:bg-gray-600 text-gray-300 rounded transition"
            >
              View
            </button>
          )}
        </div>
      </div>

      {/* File paths */}
      {task.filePaths.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1">
          {task.filePaths.slice(0, 3).map((fp) => (
            <span
              key={fp}
              className="text-[10px] font-mono text-gray-500 bg-gray-800 px-1 py-0.5 rounded truncate max-w-[200px]"
              title={fp}
            >
              {fp.split("/").pop()}
            </span>
          ))}
          {task.filePaths.length > 3 && (
            <span className="text-[10px] text-gray-600">
              +{task.filePaths.length - 3} more
            </span>
          )}
        </div>
      )}

      {/* Acceptance */}
      {task.acceptance && (
        <p className="mt-1.5 text-[11px] text-gray-500 line-clamp-2">
          {task.acceptance}
        </p>
      )}
    </div>
  );
}
