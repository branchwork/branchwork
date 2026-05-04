import { useState } from "react";
import type { PlanTask } from "../stores/plan-store.js";
import { postJson, putJson, deleteJson } from "../api.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import {
  isDriverReady,
  useSettingsStore,
  type AuthStatus,
} from "../stores/settings-store.js";
import { EditableText } from "./EditableText.js";

interface Props {
  task: PlanTask;
  planName: string;
  phaseNumber: number;
}

const STATUS_ORDER = ["pending", "in_progress", "completed", "skipped"] as const;

const ciConfig: Record<
  string,
  { label: string; bg: string; dot: string; title: string }
> = {
  pending:   { label: "CI",     bg: "bg-amber-600/20 text-amber-400",   dot: "bg-amber-400",                title: "CI run queued" },
  running:   { label: "CI",     bg: "bg-amber-600/20 text-amber-400",   dot: "bg-amber-400 animate-pulse",  title: "CI run in progress" },
  success:   { label: "CI \u2713", bg: "bg-emerald-600/20 text-emerald-400", dot: "bg-emerald-400",          title: "CI passed" },
  failure:   { label: "CI \u2717", bg: "bg-red-600/20 text-red-400",     dot: "bg-red-400",                  title: "CI failed" },
  cancelled: { label: "CI \u2014", bg: "bg-gray-600/20 text-gray-400",   dot: "bg-gray-400",                 title: "CI cancelled or skipped" },
  unknown:   { label: "CI ?",   bg: "bg-gray-600/20 text-gray-500",     dot: "bg-gray-500",                 title: "No CI run found for this commit" },
};

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

/// Short human-readable label for the auth blocker — used as a button
/// tooltip so users know why Start is disabled without opening settings.
function authStatusLabel(auth: AuthStatus | undefined): string {
  if (!auth) return "driver status unknown";
  switch (auth.kind) {
    case "not_installed":
      return "CLI not installed";
    case "unauthenticated":
      return auth.help;
    default:
      return "ready";
  }
}

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
  const [fixingCi, setFixingCi] = useState(false);
  const [agentId, setAgentId] = useState<string | null>(null);
  const [showMenu, setShowMenu] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [merging, setMerging] = useState(false);
  const agents = useAgentStore((s) => s.agents);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const mergeAgentBranch = useAgentStore((s) => s.mergeAgentBranch);
  const discardAgentBranch = useAgentStore((s) => s.discardAgentBranch);
  const [discarding, setDiscarding] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);

  // Find a completed agent with an unmerged branch for this task
  const branchAgent = agents.find(
    (a) =>
      a.plan_name === planName &&
      a.task_id === task.number &&
      a.branch &&
      a.status !== "running" &&
      a.status !== "starting"
  );
  // undefined/true → show Merge; false → hide Merge (task not expected to produce a commit)
  const canMerge = task.producesCommit !== false;
  // Task is locked while any agent for it is running/starting (possibly
  // spawned from another browser/session — we can't rely on the local
  // `agentId` state alone). Locked tasks hide the driver selector and
  // disable Check/Start/Continue/Retry so the user can't trigger duplicate
  // work or swap drivers mid-run. Kill/Finish stay available.
  const runningAgent = agents.find(
    (a) =>
      a.plan_name === planName &&
      a.task_id === task.number &&
      (a.status === "running" || a.status === "starting")
  );
  const taskLocked = !!runningAgent;
  const plan = usePlanStore((s) => s.selectedPlan);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const savePlan = usePlanStore((s) => s.savePlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const effort = useSettingsStore((s) => s.effort);
  const drivers = useSettingsStore((s) => s.drivers);
  const defaultDriver = useSettingsStore((s) => s.defaultDriver);
  const driverCapabilities = useSettingsStore((s) => s.driverCapabilities);
  const driverAuth = useSettingsStore((s) => s.driverAuth);
  // Per-card override. Initial value comes from plan metadata (when we add
  // `driver:` to the YAML schema) or the server default.
  const planDriver = (plan as { driver?: string } | null)?.driver;
  const [driver, setDriver] = useState<string>(planDriver ?? defaultDriver);
  const caps = driverCapabilities(driver);

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

  // Dependency gate: any declared dep not completed/skipped blocks Start.
  const completedSet = new Set<string>(
    (plan?.phases ?? [])
      .flatMap((p) => p.tasks)
      .filter((t) => t.status === "completed" || t.status === "skipped")
      .map((t) => t.number)
  );
  const unmetDeps = (task.dependencies ?? []).filter((d) => !completedSet.has(d));
  const blocked = unmetDeps.length > 0;

  // Auth gate: selected driver must be installed + authenticated.
  const auth = driverAuth(driver);
  const authReady = isDriverReady(auth);

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
        driver,
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

  async function handleFixCi() {
    if (!task.ci || task.ci.status !== "failure") return;
    setFixingCi(true);
    setError(null);
    try {
      const res = await postJson<{ agentId: string; branch: string }>(
        "/api/actions/fix-ci",
        {
          planName,
          taskNumber: task.number,
          ciRunId: task.ci.id,
          driver,
        }
      );
      setAgentId(res.agentId);
      selectAgent(res.agentId);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Fix CI failed: ${msg}`);
      console.error("Failed to start Fix CI agent:", e);
    } finally {
      setFixingCi(false);
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

  // Click anywhere on the card to open the running agent's terminal in
  // the right rail. Only active when an agent is actually running for this
  // task — otherwise the card is not clickable. Inner interactive elements
  // (buttons, inputs, textareas, anchors, EditableText fields) are excluded
  // via `closest()` so the pre-existing controls keep working without each
  // having to call `stopPropagation`.
  const handleCardClick = runningAgent
    ? (e: React.MouseEvent<HTMLDivElement>) => {
        const target = e.target as HTMLElement;
        if (
          target.closest(
            'button, input, textarea, select, a, [role="button"], [contenteditable="true"]'
          )
        ) {
          return;
        }
        selectAgent(runningAgent.id);
      }
    : undefined;

  return (
    <div
      onClick={handleCardClick}
      className={`rounded-md border p-3 transition ${
        status === "completed"
          ? "bg-gray-800/30 border-gray-800/50 opacity-70"
          : "bg-gray-800/50 border-gray-700/50 hover:border-gray-600"
      } ${runningAgent ? "cursor-pointer hover:bg-gray-800/80" : ""}`}
      title={runningAgent ? "Click to open agent terminal" : undefined}
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
                <div className="absolute top-6 left-0 z-10 bg-gray-800 border border-gray-700 rounded-md shadow-lg py-1 min-w-[140px]">
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
                  {/* Reset — clears the task_status row entirely. Useful when
                      a task has ended up stuck in `checking` or similar from
                      a dead agent. Backend refuses if an agent is still live. */}
                  <div className="border-t border-gray-700 my-1" />
                  <button
                    onClick={async () => {
                      setShowMenu(false);
                      try {
                        await postJson(
                          `/api/plans/${planName}/tasks/${task.number}/reset-status`,
                          {},
                        );
                        await selectPlan(planName);
                      } catch (e) {
                        const msg = e instanceof Error ? e.message : String(e);
                        setError(`Reset failed: ${msg}`);
                      }
                    }}
                    className="w-full text-left px-3 py-1 text-xs hover:bg-red-950/40 text-red-400/80 hover:text-red-300 flex items-center gap-2"
                    title="Clear status row — useful to unwedge a stuck 'checking' task"
                  >
                    <span className="w-1.5 h-1.5 rounded-full bg-red-400/80" />
                    Reset
                  </button>
                </div>
              )}
            </div>
            {/* Updated at */}
            {task.statusUpdatedAt && (
              <span className="text-[9px] text-gray-600" title={task.statusUpdatedAt}>
                {timeAgo(task.statusUpdatedAt)}
              </span>
            )}
            {/* Cost — hidden for drivers that don't report spend */}
            {caps.supports_cost && task.costUsd != null && task.costUsd > 0 && (
              <span
                className="text-[9px] text-amber-400/80 font-mono"
                title="Total agent cost for this task"
              >
                ${task.costUsd.toFixed(task.costUsd >= 1 ? 2 : 4)}
              </span>
            )}
            {/* CI badge + dismiss button (only shown for failed/unknown
                runs — passing or running CI shouldn't be dismissable). */}
            {task.ci && (() => {
              const c = ciConfig[task.ci.status] ?? ciConfig.unknown;
              const className = `text-[10px] px-1.5 py-0.5 rounded flex items-center gap-1 ${c.bg}`;
              const inner = (
                <>
                  <span className={`w-1.5 h-1.5 rounded-full ${c.dot}`} />
                  {c.label}
                </>
              );
              const badge = task.ci.runUrl ? (
                <a
                  href={task.ci.runUrl}
                  target="_blank"
                  rel="noreferrer noopener"
                  className={`${className} hover:opacity-80`}
                  title={`${c.title} — open run`}
                  onClick={(e) => e.stopPropagation()}
                >
                  {inner}
                </a>
              ) : (
                <span className={className} title={c.title}>{inner}</span>
              );
              const dismissable =
                task.ci.status === "failure" ||
                task.ci.status === "cancelled" ||
                task.ci.status === "unknown";
              const ciRunId = task.ci.id;
              const isFailed = task.ci.status === "failure";
              return (
                <span className="inline-flex items-center">
                  {badge}
                  {/* Fix CI — inline with the badge so the failure state and
                      its recovery action read as one unit. Spawns an agent on
                      a recovery branch off the failing commit with the failure
                      log baked into the prompt. */}
                  {isFailed && !agentId && (
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        handleFixCi();
                      }}
                      disabled={fixingCi || taskLocked || !authReady}
                      title={
                        taskLocked
                          ? "Agent running — wait for it to finish"
                          : !authReady
                            ? `${driver} not ready: ${authStatusLabel(auth)}`
                            : `Spawn an agent to fix the failing CI on ${
                                task.ci.commitSha?.slice(0, 7) ?? "the merged commit"
                              }`
                      }
                      className="ml-1 text-[10px] px-1.5 py-0.5 rounded font-medium bg-red-700 hover:bg-red-600 disabled:bg-gray-700 disabled:text-gray-500 disabled:cursor-not-allowed text-white transition"
                    >
                      {fixingCi ? "..." : "Fix CI"}
                    </button>
                  )}
                  {dismissable && ciRunId != null && (
                    <button
                      onClick={async (e) => {
                        e.stopPropagation();
                        try {
                          await deleteJson(`/api/ci/${ciRunId}`);
                          await selectPlan(planName);
                        } catch (err) {
                          const msg =
                            err instanceof Error ? err.message : String(err);
                          setError(`Dismiss CI failed: ${msg}`);
                        }
                      }}
                      className="ml-0.5 text-[10px] text-gray-500 hover:text-gray-300 hover:bg-gray-800/60 px-1 rounded transition"
                      title="Dismiss this CI result — won't affect future runs"
                    >
                      &#x2715;
                    </button>
                  )}
                </span>
              );
            })()}
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
        <div className="flex-shrink-0 flex gap-1 items-center">
          {/* Driver selector — only show when a start action is possible, and
              only when the server advertises more than one driver. Disabled
              (not hidden) while taskLocked so users can see which driver is
              in flight and why they can't change it. */}
          {drivers.length > 1 &&
            !agentId &&
            (status === "pending" || status === "in_progress" || status === "failed") && (
              <select
                value={driver}
                onChange={(e) => setDriver(e.target.value)}
                disabled={taskLocked}
                className="text-[10px] bg-gray-800 border border-gray-700 text-gray-300 rounded px-1 py-0.5 focus:outline-none focus:border-gray-500 disabled:opacity-50 disabled:cursor-not-allowed"
                title={
                  taskLocked
                    ? "Agent running — wait for it to finish"
                    : "Agent driver to use when starting this task"
                }
              >
                {drivers.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.name}
                  </option>
                ))}
              </select>
            )}
          {/* Check button — always available except while an agent is running */}
          <button
            onClick={handleCheck}
            disabled={checking || status === "checking" || taskLocked}
            className="px-2 py-1 text-xs bg-gray-700 hover:bg-gray-600 disabled:opacity-50 text-gray-300 rounded transition"
            title={
              taskLocked
                ? "Agent running — wait for it to finish"
                : "Spawn an agent to verify this task against the codebase"
            }
          >
            {checking || status === "checking" ? "..." : "Check"}
          </button>

          {/* Start — for pending tasks */}
          {status === "pending" && !agentId && (
            <button
              onClick={() => handleStart("start")}
              disabled={starting || blocked || !authReady || taskLocked}
              title={
                taskLocked
                  ? "Agent running — wait for it to finish"
                  : !authReady
                    ? `${driver} not ready: ${authStatusLabel(auth)}`
                    : blocked
                      ? `Blocked by ${unmetDeps.join(", ")}`
                      : undefined
              }
              className="px-2 py-1 text-xs bg-indigo-600 hover:bg-indigo-500 disabled:bg-gray-700 disabled:text-gray-500 disabled:cursor-not-allowed text-white rounded transition"
            >
              {starting ? "..." : "Start"}
            </button>
          )}

          {/* Continue — for in_progress tasks */}
          {status === "in_progress" && !agentId && (
            <button
              onClick={() => handleStart("continue")}
              disabled={starting || !authReady || taskLocked}
              title={
                taskLocked
                  ? "Agent running — wait for it to finish"
                  : !authReady
                    ? `${driver} not ready: ${authStatusLabel(auth)}`
                    : undefined
              }
              className="px-2 py-1 text-xs bg-amber-600 hover:bg-amber-500 disabled:bg-gray-700 disabled:text-gray-500 disabled:cursor-not-allowed text-white rounded transition"
            >
              {starting ? "..." : "Continue"}
            </button>
          )}

          {/* Retry — for failed tasks */}
          {status === "failed" && !agentId && (
            <button
              onClick={() => handleStart("continue")}
              disabled={starting || !authReady || taskLocked}
              title={
                taskLocked
                  ? "Agent running — wait for it to finish"
                  : !authReady
                    ? `${driver} not ready: ${authStatusLabel(auth)}`
                    : undefined
              }
              className="px-2 py-1 text-xs bg-red-700 hover:bg-red-600 disabled:bg-gray-700 disabled:text-gray-500 disabled:cursor-not-allowed text-white rounded transition"
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

      {/* Blocked banner — shown when unmet dependencies prevent starting */}
      {blocked && status === "pending" && (
        <div className="mt-2 flex items-center gap-2 bg-amber-950/40 border border-amber-800/40 rounded px-2 py-1">
          <span className="text-amber-400 text-[10px]">&#9888;</span>
          <span className="text-[10px] text-amber-300/90">
            Blocked by{" "}
            {unmetDeps.map((d, i) => (
              <span key={d}>
                <span className="font-mono bg-amber-900/40 px-1 rounded">{d}</span>
                {i < unmetDeps.length - 1 ? ", " : ""}
              </span>
            ))}
          </span>
        </div>
      )}

      {/* Branch banner — prominent when there's a pending branch to merge.
         For non-commit tasks (producesCommit === false) the Merge button is
         hidden but Discard is kept so the empty branch can be cleaned up. */}
      {branchAgent?.branch && (
        <div className={`mt-2 flex items-center gap-2 ${canMerge ? "bg-indigo-950/40 border-indigo-800/40" : "bg-gray-900/40 border-gray-700/40"} border rounded px-2 py-1.5`}>
          <span className={`${canMerge ? "text-indigo-400" : "text-gray-500"} text-[10px]`}>&#9739;</span>
          <div className="flex-1 min-w-0">
            {!canMerge && (
              <div className="text-[10px] text-gray-400 mb-0.5">
                Agent finished — no commit expected. Discard the scratch branch when you're done reviewing.
              </div>
            )}
            <div className="text-[10px] font-mono text-indigo-300/90 truncate" title={branchAgent.branch}>
              {branchAgent.branch}
            </div>
            <div className="text-[9px] text-gray-600">
              &#8594; {branchAgent.source_branch ?? "main"}
            </div>
          </div>
          <div className="flex-shrink-0 flex items-center gap-1">
            {confirmDiscard ? (
              <>
                <span className="text-[10px] text-red-400 mr-1">Delete branch?</span>
                <button
                  onClick={async () => {
                    setDiscarding(true);
                    setError(null);
                    const result = await discardAgentBranch(branchAgent.id);
                    if (result.ok) {
                      await selectPlan(planName);
                    } else {
                      setError(result.error ?? "Discard failed");
                    }
                    setDiscarding(false);
                    setConfirmDiscard(false);
                  }}
                  disabled={discarding}
                  className="px-2 py-1 text-xs bg-red-700 hover:bg-red-600 disabled:opacity-50 text-white rounded transition"
                >
                  {discarding ? "..." : "Yes"}
                </button>
                <button
                  onClick={() => setConfirmDiscard(false)}
                  disabled={discarding}
                  className="px-2 py-1 text-xs text-gray-400 hover:text-gray-200 disabled:opacity-50 transition"
                >
                  No
                </button>
              </>
            ) : (
              <>
                <button
                  onClick={() => setConfirmDiscard(true)}
                  disabled={merging}
                  className="px-2 py-1 text-xs text-red-400/80 hover:text-red-300 hover:bg-red-950/40 disabled:opacity-50 rounded transition"
                  title={`Delete branch ${branchAgent.branch} without merging`}
                >
                  Discard
                </button>
                {canMerge && (
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
              </>
            )}
          </div>
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
