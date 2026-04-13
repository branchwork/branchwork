import { useState } from "react";
import type { PlanTask } from "../stores/plan-store.js";
import { postJson, putJson } from "../api.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { EditableText } from "./EditableText.js";

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
  const [error, setError] = useState<string | null>(null);
  const [merging, setMerging] = useState(false);
  const agents = useAgentStore((s) => s.agents);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const mergeAgentBranch = useAgentStore((s) => s.mergeAgentBranch);

  // Find a completed agent with an unmerged branch for this task
  const branchAgent = agents.find(
    (a) =>
      a.plan_name === planName &&
      a.task_id === task.number &&
      a.branch &&
      a.status !== "running" &&
      a.status !== "starting"
  );
  const plan = usePlanStore((s) => s.selectedPlan);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const savePlan = usePlanStore((s) => s.savePlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const effort = useSettingsStore((s) => s.effort);

  async function saveTaskField(patch: Partial<PlanTask>) {
    if (!plan) return;
    const updated = {
      ...plan,
      phases: plan.phases.map((p) => ({
        ...p,
        tasks: p.tasks.map((t) =>
          t.number === task.number ? { ...t, ...patch } : t
        ),
      })),
    };
    try {
      await savePlan(updated);
      await fetchPlans();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Save failed: ${msg}`);
    }
  }

  const status = task.status ?? "pending";
  const cfg = statusConfig[status] ?? statusConfig.pending;

  async function handleStart(mode: "start" | "continue" = "start") {
    setStarting(true);
    setError(null);
    try {
      const res = await postJson<{ agentId: string }>("/api/actions/start-task", {
        planName,
        phaseNumber,
        taskNumber: task.number,
        mode,
        effort,
      });
      setAgentId(res.agentId);
      selectAgent(res.agentId);
      await updateStatus("in_progress");
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Start failed: ${msg}`);
      console.error("Failed to start task:", e);
    } finally {
      setStarting(false);
    }
  }

  async function handleCheck() {
    setChecking(true);
    setError(null);
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
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Check failed: ${msg}`);
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
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Status update failed: ${msg}`);
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
            <EditableText
              value={task.title}
              onSave={(v) => saveTaskField({ title: v })}
              className="text-sm font-medium"
              editClassName="text-sm font-medium"
            />
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

          {/* Merge — when agent has an unmerged branch */}
          {branchAgent && (
            <button
              onClick={async () => {
                setMerging(true);
                setError(null);
                const result = await mergeAgentBranch(branchAgent.id);
                if (result.ok) {
                  await selectPlan(planName);
                } else {
                  setError(result.error ?? "Merge failed");
                }
                setMerging(false);
              }}
              disabled={merging}
              className="px-2 py-1 text-xs bg-emerald-700 hover:bg-emerald-600 disabled:opacity-50 text-white rounded transition"
              title={`Merge branch ${branchAgent.branch} into ${branchAgent.source_branch ?? "main"}`}
            >
              {merging ? "..." : "Merge"}
            </button>
          )}
        </div>
      </div>

      {/* Branch indicator */}
      {branchAgent?.branch && (
        <div className="mt-1.5 flex items-center gap-1.5 text-[10px]">
          <span className="text-indigo-400">&#9739;</span>
          <span className="font-mono text-indigo-400/70 truncate" title={branchAgent.branch}>
            {branchAgent.branch}
          </span>
          <span className="text-gray-600">&#8594; {branchAgent.source_branch ?? "main"}</span>
        </div>
      )}

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

      {/* Description — editable */}
      <div className="mt-1.5 text-[11px] text-gray-400">
        <EditableText
          value={task.description}
          onSave={(v) => saveTaskField({ description: v })}
          multiline
          className="line-clamp-2"
          editClassName="text-[11px]"
          placeholder="Add description..."
        />
      </div>

      {/* Acceptance — editable */}
      <div className="mt-1 text-[11px] text-gray-500">
        <EditableText
          value={task.acceptance}
          onSave={(v) => saveTaskField({ acceptance: v })}
          className="line-clamp-1"
          editClassName="text-[11px]"
          placeholder="Add acceptance criteria..."
        />
      </div>

      {/* Error display */}
      {error && (
        <div className="mt-2 text-[11px] text-red-400 bg-red-900/20 border border-red-800/30 rounded px-2 py-1 flex items-start justify-between gap-1">
          <span className="line-clamp-2">{error}</span>
          <button
            onClick={() => setError(null)}
            className="text-red-600 hover:text-red-400 flex-shrink-0"
          >
            x
          </button>
        </div>
      )}
    </div>
  );
}
