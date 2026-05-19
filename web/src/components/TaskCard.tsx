import { memo, useEffect, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import type { PlanTask } from "../stores/plan-store.js";
import { fetchJson, postJson, putJson, deleteJson } from "../api.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { isDriverReady, useSettingsStore, type AuthStatus } from "../stores/settings-store.js";
import { EditableText } from "./EditableText.js";
import { Button } from "./ui/Button.js";
import { Dropdown, DropdownItem, DropdownSeparator } from "./ui/Dropdown.js";
import { TouchTarget } from "./ui/TouchTarget.js";
import { formatRelative } from "../lib/time.js";
import { toastError } from "../lib/toast.js";
import { httpErrorBody } from "../lib/error.js";
import { useGoToAgent } from "../hooks/use-route-selection.js";
import { useCompletedTasks } from "../lib/plan-context.js";

interface Props {
  task: PlanTask;
  planName: string;
  phaseNumber: number;
}

const ciConfig: Record<string, { label: string; bg: string; dot: string; title: string }> = {
  pending: {
    label: "CI",
    bg: "bg-amber-600/20 text-amber-400",
    dot: "bg-amber-400",
    title: "CI run queued",
  },
  running: {
    label: "CI",
    bg: "bg-amber-600/20 text-amber-400",
    dot: "bg-amber-400 animate-pulse",
    title: "CI run in progress",
  },
  success: {
    label: "CI \u2713",
    bg: "bg-emerald-600/20 text-emerald-400",
    dot: "bg-emerald-400",
    title: "CI passed",
  },
  failure: {
    label: "CI \u2717",
    bg: "bg-red-600/20 text-red-400",
    dot: "bg-red-400",
    title: "CI failed",
  },
  cancelled: {
    label: "CI \u2014",
    bg: "bg-gray-600/20 text-gray-400",
    dot: "bg-gray-400",
    title: "CI cancelled or skipped",
  },
  unknown: {
    label: "CI ?",
    bg: "bg-gray-600/20 text-gray-500",
    dot: "bg-gray-500",
    title: "No CI run found for this commit",
  },
};

const statusConfig: Record<string, { label: string; bg: string; dot: string }> = {
  pending: { label: "Pending", bg: "bg-gray-700 text-gray-300", dot: "bg-gray-400" },
  in_progress: {
    label: "In Progress",
    bg: "bg-amber-600/20 text-amber-400",
    dot: "bg-amber-400 animate-pulse",
  },
  completed: { label: "Done", bg: "bg-emerald-600/20 text-emerald-400", dot: "bg-emerald-400" },
  failed: { label: "Failed", bg: "bg-red-600/20 text-red-400", dot: "bg-red-400" },
  skipped: { label: "Skipped", bg: "bg-gray-600/20 text-gray-500", dot: "bg-gray-500" },
  checking: {
    label: "Checking...",
    bg: "bg-blue-600/20 text-blue-400",
    dot: "bg-blue-400 animate-pulse",
  },
};

/// Derived pill states that sit BETWEEN the canonical `task.status` row
/// in the DB and the merged-into-default state. Not picked from a
/// dropdown — TaskCard renders these instead of the matching status
/// pill whenever the auto-mode loop signals a transient cadence event
/// (Task 2.2 deferred, Task 2.3 flush, Task 3.1 visual).
///
/// `awaiting-cadence`: task agent completed cleanly under a `phase` or
/// `plan` merge cadence, but the cadence boundary hasn't been crossed
/// yet (Task 2.2). The agent is sitting on
/// `merge_status='deferred_for_cadence'` with its branch intact; the
/// pill is muted purple so the user can distinguish "done, will merge
/// later" from regular `Done` (already merged) without poking the
/// agent row. Tooltip surfaces phase progress so the user knows how
/// many siblings still need to finish before the drain runs.
///
/// `merging`: live transient label driven by the per-plan auto-mode
/// state machine when `auto_mode_state.state === "merging"` AND
/// `task === this task`. Replaces the awaiting-cadence pill for the
/// few seconds each task spends inside `run_merge_step` during a
/// drain (or a non-cadence merge), then naturally drops back to the
/// completed pill once the branch clears.
const derivedStateConfig: Record<
  "awaiting_cadence" | "merging",
  { label: string; bg: string; dot: string }
> = {
  awaiting_cadence: {
    label: "Awaiting cadence",
    bg: "bg-purple-700/25 text-purple-300/90",
    dot: "bg-purple-400/80",
  },
  merging: {
    label: "Merging...",
    bg: "bg-indigo-600/25 text-indigo-300",
    dot: "bg-indigo-400 animate-pulse",
  },
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

function TaskCardInner({ task, planName, phaseNumber }: Props) {
  const [starting, setStarting] = useState(false);
  const [checking, setChecking] = useState(false);
  const [fixingCi, setFixingCi] = useState(false);
  const [agentId, setAgentId] = useState<string | null>(null);
  const [merging, setMerging] = useState(false);
  // Narrow agents subscription: pick out the two rows this task actually
  // cares about (the running one for the lock + click-to-open behaviour,
  // and the completed-with-branch one for the merge banner). `useShallow`
  // shallow-compares the returned object's keys, so unrelated agent
  // updates that don't change either reference don't re-render this card.
  // Audit §10 minor: pre-fix every TaskCard re-rendered on every agent
  // store update because they all subscribed to `s.agents` directly.
  const { branchAgent, runningAgent, spawnFailedAgent } = useAgentStore(
    useShallow((s) => {
      let branch: import("../stores/agent-store.js").Agent | undefined;
      let running: import("../stores/agent-store.js").Agent | undefined;
      let spawnFailed: import("../stores/agent-store.js").Agent | undefined;
      for (const a of s.agents) {
        if (a.plan_name !== planName || a.task_id !== task.number) continue;
        if (a.status === "running" || a.status === "starting") {
          if (!running) running = a;
        } else if (a.branch) {
          if (!branch) branch = a;
        }
        // The agents store is ordered started_at DESC, so the first
        // failed-with-spawn_error match is the most recent. Surface it
        // on the task card so a fresh Start click that crashed the
        // runner spawn doesn't go silent (Task 1.1, runner-install-and-
        // spawn-reliability plan). A subsequent successful spawn flips
        // the row's `status` away from `failed` (and a fresh Start
        // produces a new row entirely), so the banner naturally clears.
        if (a.status === "failed" && a.spawn_error && !spawnFailed && !running && !branch) {
          spawnFailed = a;
        }
      }
      return {
        branchAgent: branch,
        runningAgent: running,
        spawnFailedAgent: spawnFailed,
      };
    }),
  );
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const goToAgent = useGoToAgent();
  const mergeAgentBranch = useAgentStore((s) => s.mergeAgentBranch);
  const discardAgentBranch = useAgentStore((s) => s.discardAgentBranch);
  const [discarding, setDiscarding] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  // Two-step confirm for the destructive Reset menu item. First click arms,
  // second commits. Auto-disarms after 4 s so a stale armed state can't fire
  // accidentally when the user reopens the menu later.
  const [resetArmed, setResetArmed] = useState(false);
  // Inline message inside the status dropdown when the server rejects
  // `completed` because the working tree is dirty. Critically NOT a toast
  // and NOT a roadblock: the dropdown stays open so the operator can pick
  // a different status (e.g. `skipped`) without re-opening the menu. The
  // server returns 409 with body `{error: "working_tree_dirty", message,
  // files}` from `api/plans.rs::set_task_status`.
  const [dirtyTreeError, setDirtyTreeError] = useState<string | null>(null);

  // undefined/true → show Merge; false → hide Merge (task not expected to produce a commit)
  const canMerge = task.producesCommit !== false;
  // Task is locked while any agent for it is running/starting (possibly
  // spawned from another browser/session — we can't rely on the local
  // `agentId` state alone). Locked tasks hide the driver selector and
  // disable Check/Start/Continue/Retry so the user can't trigger duplicate
  // work or swap drivers mid-run. Kill/Finish stay available.
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
        tasks: p.tasks.map((t) => (t.number === task.number ? { ...t, ...patch } : t)),
      })),
    };
    try {
      await savePlan(updated);
      await fetchPlans();
    } catch (e) {
      toastError(e, "Save failed");
    }
  }

  const status = task.status ?? "pending";

  // Derived pill state for the cadence transient labels. Reads two
  // separate signals:
  //
  //   1. `branchAgent.merge_status === 'deferred_for_cadence'` — set by
  //      the auto-mode loop at the moment it decides not to merge yet
  //      (Task 2.2). Survives reloads because it lives on the agents
  //      row, so refreshing the page keeps the purple pill until the
  //      drain runs.
  //
  //   2. `autoModeRuntimes[plan].state === 'merging'` + `task === this
  //      task` — live transient label driven by `auto_mode_state` WS
  //      events fired from `run_merge_step`. The drain merges agents
  //      sequentially so this label hops from task to task during a
  //      flush; each pill shows it for the few seconds it takes the
  //      server to land that merge, then naturally drops back to the
  //      regular `Done` pill once `agent_branch_merged` clears
  //      `branch` on the agents row.
  //
  // Order matters: `merging` overrides `awaiting-cadence` so a task in
  // mid-merge looks transient even though its branchAgent still carries
  // the deferred flag for a few ms before run_merge_step clears it.
  const autoModeRuntime = usePlanStore((s) => s.autoModeRuntimes[planName] ?? null);
  const isMergingNow = autoModeRuntime?.state === "merging" && autoModeRuntime.task === task.number;
  const isAwaitingCadence =
    status === "completed" && !isMergingNow && branchAgent?.merge_status === "deferred_for_cadence";

  // Phase progress counts for the awaiting-cadence tooltip. Computed
  // from the selected plan's phase tasks (we already subscribed to
  // `plan` above for plan-level edits). Skipped counts as done because
  // `should_merge_now` (Task 2.1) treats it that way — surfacing the
  // same number here keeps the tooltip honest about when the drain
  // will actually fire.
  const currentPhase = plan?.phases.find((p) => p.number === phaseNumber);
  const phaseTaskCount = currentPhase?.tasks.length ?? 0;
  const phaseDoneCount =
    currentPhase?.tasks.filter((t) => t.status === "completed" || t.status === "skipped").length ??
    0;

  // Final pill config. `cfg` is what the pill DISPLAYS — the dropdown
  // continues to operate on the canonical `status` value, so picking
  // "Skipped" while the pill shows "Awaiting cadence" still writes the
  // intended status and the purple label evaporates on the next render.
  const derivedKey: "awaiting_cadence" | "merging" | null = isMergingNow
    ? "merging"
    : isAwaitingCadence
      ? "awaiting_cadence"
      : null;
  const derivedCfg = derivedKey ? derivedStateConfig[derivedKey] : null;
  const cfg = derivedCfg ?? statusConfig[status] ?? statusConfig.pending;

  // Pill tooltip — only set when a derived state is active so the
  // default behaviour (no tooltip on the regular pill) is preserved.
  // Phrasing matches the Task 3.1 brief verbatim except for the live
  // count substitution.
  const pillTooltip = isMergingNow
    ? "Auto-mode is merging this branch into the default branch now."
    : isAwaitingCadence
      ? `Completed; waiting for phase boundary to merge (${phaseDoneCount} of ${phaseTaskCount} tasks done).`
      : "Open status menu";

  // Dependency gate: any declared dep not completed/skipped blocks Start.
  // The plan-wide done/skipped Set is built once at PlanBoard and shared
  // via `CompletedTasksContext` (audit §10 minor) — pre-fix every card
  // rebuilt the Set over every task in the plan on every render.
  const completedSet = useCompletedTasks();
  const unmetDeps = (task.dependencies ?? []).filter((d) => !completedSet.has(d));
  const blocked = unmetDeps.length > 0;

  // Auth gate: selected driver must be installed + authenticated.
  const auth = driverAuth(driver);
  const authReady = isDriverReady(auth);

  async function handleStart(mode: "start" | "continue" = "start") {
    setStarting(true);
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
      goToAgent(res.agentId);
      await updateStatus("in_progress");
    } catch (e) {
      toastError(e, "Start failed");
    } finally {
      setStarting(false);
    }
  }

  async function handleCheck() {
    setChecking(true);
    try {
      const res = await postJson<{ agentId: string }>(
        `/api/plans/${planName}/tasks/${task.number}/check`,
        {},
      );
      setAgentId(res.agentId);
      selectAgent(res.agentId);
      goToAgent(res.agentId);
      // Refresh plan after a delay to pick up the result
      setTimeout(() => selectPlan(planName), 3000);
      setTimeout(() => selectPlan(planName), 10000);
      setTimeout(() => selectPlan(planName), 30000);
    } catch (e) {
      toastError(e, "Check failed");
    } finally {
      setChecking(false);
    }
  }

  async function handleFixCi() {
    if (!task.ci || task.ci.status !== "failure") return;
    setFixingCi(true);
    try {
      const res = await postJson<{ agentId: string; branch: string }>("/api/actions/fix-ci", {
        planName,
        taskNumber: task.number,
        ciRunId: task.ci.id,
        driver,
      });
      setAgentId(res.agentId);
      selectAgent(res.agentId);
      goToAgent(res.agentId);
    } catch (e) {
      toastError(e, "Fix CI failed");
    } finally {
      setFixingCi(false);
    }
  }

  async function updateStatus(newStatus: string) {
    setResetArmed(false);
    try {
      await putJson(`/api/plans/${planName}/tasks/${task.number}/status`, {
        status: newStatus,
      });
      await selectPlan(planName);
      // Any successful status write clears a stale "completed blocked"
      // hint — even when the new status is not `completed`, the user
      // has moved on.
      setDirtyTreeError(null);
    } catch (e) {
      // 409 working_tree_dirty is operator-recoverable: surface it
      // inline inside the dropdown rather than toasting + closing the
      // menu, so the operator can pick a different status (e.g.
      // `skipped`) without re-opening it.
      const body = httpErrorBody<{
        error?: string;
        message?: string;
        files?: unknown;
      }>(e);
      if (body?.error === "working_tree_dirty") {
        const fileCount = Array.isArray(body.files) ? body.files.length : null;
        const suffix =
          fileCount != null
            ? ` ${fileCount} uncommitted file${fileCount === 1 ? "" : "s"} in the project.`
            : "";
        setDirtyTreeError(`Can't mark completed yet — commit working-tree changes first.${suffix}`);
        return;
      }
      toastError(e, "Status update failed");
    }
  }

  async function resetStatus() {
    if (!resetArmed) {
      // First click — arm. Second click within 4 s commits the reset.
      setResetArmed(true);
      return;
    }
    setResetArmed(false);
    try {
      await postJson(`/api/plans/${planName}/tasks/${task.number}/reset-status`, {});
      await selectPlan(planName);
    } catch (e) {
      toastError(e, "Reset failed");
    }
  }

  // Auto-disarm the Reset confirm so a stale armed state from an abandoned
  // dropdown can't fire when the user comes back later.
  useEffect(() => {
    if (!resetArmed) return;
    const t = setTimeout(() => setResetArmed(false), 4000);
    return () => clearTimeout(t);
  }, [resetArmed]);

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
            'button, input, textarea, select, a, [role="button"], [contenteditable="true"]',
          )
        ) {
          return;
        }
        selectAgent(runningAgent.id);
        goToAgent(runningAgent.id);
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
            <span className="text-[10px] font-mono text-gray-500">{task.number}</span>
            {/* Status pill IS the dropdown trigger — replaces the prior
                click-to-cycle pill + separate kebab. Click cycling
                through 6 states was past the cognitive ceiling (users
                couldn't predict where the next click landed and a 409
                on `completed` for a dirty tree silently blocked the
                cycle from reaching `skipped`). Dropdown lists every
                valid status verbatim so each transition is a single
                explicit pick; keyboard a11y (arrow keys + Enter) comes
                free from the `Dropdown` primitive. */}
            <Dropdown
              label={`Set status for task ${task.number}`}
              trigger={(props) => (
                <button
                  {...props}
                  type="button"
                  aria-label={`Status menu for task ${task.number}`}
                  title={pillTooltip}
                  className={`text-[10px] px-1.5 py-0.5 rounded cursor-pointer hover:opacity-80 flex items-center gap-1 ${cfg.bg}`}
                >
                  <span className={`w-1.5 h-1.5 rounded-full ${cfg.dot}`} />
                  {cfg.label}
                </button>
              )}
            >
              {dirtyTreeError && (
                <div
                  role="status"
                  aria-live="polite"
                  className="px-2 py-1.5 mx-1 mb-1 text-[11px] text-amber-300 bg-amber-950/40 border border-amber-900/40 rounded"
                >
                  {dirtyTreeError}
                </div>
              )}
              {Object.entries(statusConfig).map(([key, val]) => (
                <DropdownItem
                  key={key}
                  onSelect={() => updateStatus(key)}
                  className={key === status ? "text-white font-medium" : ""}
                >
                  <span className={`w-1.5 h-1.5 rounded-full ${val.dot}`} />
                  {val.label}
                </DropdownItem>
              ))}
              <DropdownSeparator />
              {/* Reset — clears the task_status row entirely. Useful when
                  a task has ended up stuck in `checking` or similar from
                  a dead agent. Backend refuses if an agent is still live.
                  Confirm-click pattern: first activation arms, second commits.
                  Auto-disarms after 4 s so a stale armed state can't fire
                  when the user reopens the menu later. */}
              <DropdownItem
                onSelect={resetStatus}
                variant="danger"
                title={
                  resetArmed
                    ? "Click again to confirm — clears the task's status row"
                    : "Clear status row — useful to unwedge a stuck 'checking' task"
                }
                aria-label={resetArmed ? "Confirm reset task status" : "Reset task status"}
              >
                <span className="w-1.5 h-1.5 rounded-full bg-red-400/80" />
                {resetArmed ? "Click again to confirm" : "Reset"}
              </DropdownItem>
            </Dropdown>
            {/* Updated at */}
            {task.statusUpdatedAt && (
              <span className="text-[9px] text-gray-600" title={task.statusUpdatedAt}>
                {formatRelative(task.statusUpdatedAt)}
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
            {task.ci &&
              (() => {
                const c = ciConfig[task.ci.status] ?? ciConfig.unknown;
                const className = `text-[10px] px-1.5 py-0.5 rounded flex items-center gap-1 ${c.bg}`;
                const viaFix = task.ci.viaFixAttempt ?? null;
                const tooltip = viaFix != null ? `passed via fix attempt ${viaFix}` : c.title;
                const inner = (
                  <>
                    <span className={`w-1.5 h-1.5 rounded-full ${c.dot}`} />
                    {c.label}
                    {viaFix != null && <span className="opacity-70">fix #{viaFix}</span>}
                  </>
                );
                const badge = task.ci.runUrl ? (
                  <a
                    href={task.ci.runUrl}
                    target="_blank"
                    rel="noreferrer noopener"
                    className={`${className} hover:opacity-80`}
                    title={`${tooltip} — open run`}
                    onClick={(e) => e.stopPropagation()}
                  >
                    {inner}
                  </a>
                ) : (
                  <span className={className} title={tooltip}>
                    {inner}
                  </span>
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
                      <TouchTarget>
                        <button
                          onClick={async (e) => {
                            e.stopPropagation();
                            try {
                              await deleteJson(`/api/ci/${ciRunId}`);
                              await selectPlan(planName);
                            } catch (err) {
                              toastError(err, "Dismiss CI failed");
                            }
                          }}
                          className="ml-0.5 text-[10px] text-gray-500 hover:text-gray-300 hover:bg-gray-800/60 px-1 rounded transition"
                          title="Dismiss this CI result — won't affect future runs"
                        >
                          &#x2715;
                        </button>
                      </TouchTarget>
                    )}
                  </span>
                );
              })()}
          </div>
          <h4 className="text-sm font-medium mt-0.5 leading-tight">
            <EditableText
              value={task.title}
              onSave={(v) => saveTaskField({ title: v })}
              label={`task ${task.number} title`}
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
              <TouchTarget>
                {/* Native `<select>` deliberately kept (audit §8 listed
                    this alongside hand-rolled menus, but a native select
                    already provides Tab focus, Enter/ArrowDown to open,
                    arrow-key navigation, Esc to close, and type-ahead
                    search — it is strictly more accessible than any
                    custom `<Dropdown/>` we could ship. Adding the
                    explicit `aria-label` so axe-core has a programmatic
                    name beyond the `title` tooltip. */}
                <select
                  value={driver}
                  onChange={(e) => setDriver(e.target.value)}
                  disabled={taskLocked}
                  aria-label={`Driver for task ${task.number}`}
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
              </TouchTarget>
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
            <Button
              variant="primary"
              size="sm"
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
              className="px-2 py-1"
            >
              {starting ? "..." : "Start"}
            </Button>
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
              onClick={() => {
                selectAgent(agentId);
                goToAgent(agentId);
              }}
              className="px-2 py-1 text-xs bg-gray-700 hover:bg-gray-600 text-gray-300 rounded transition"
            >
              View
            </button>
          )}
        </div>
      </div>

      {/* Spawn-failure banner — Task 1.1, runner-install-and-spawn-
          reliability plan. Shows when the runner failed to spawn the
          session daemon (typically the `branchwork-server session`
          binary missing on the runner's PATH, or a permissions issue).
          The structured `spawn_error` column was populated by the SaaS
          runner_ws handler when it received the `AgentSpawnFailed`
          envelope; rendering it inline turns a previously-silent Start
          click into an actionable error the operator can fix. */}
      {spawnFailedAgent?.spawn_error && (
        <div
          className="mt-2 flex items-center gap-2 bg-red-950/40 border border-red-800/40 rounded px-2 py-1"
          data-testid="spawn-error-banner"
        >
          <span className="text-red-400 text-[10px]">&#9888;</span>
          <span className="text-[10px] text-red-300/90 font-mono break-all">
            {spawnFailedAgent.spawn_error}
          </span>
        </div>
      )}

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
        <div
          className={`mt-2 flex items-center gap-2 ${canMerge ? "bg-indigo-950/40 border-indigo-800/40" : "bg-gray-900/40 border-gray-700/40"} border rounded px-2 py-1.5`}
        >
          <span className={`${canMerge ? "text-indigo-400" : "text-gray-500"} text-[10px]`}>
            &#9739;
          </span>
          <div className="flex-1 min-w-0">
            {!canMerge && (
              <div className="text-[10px] text-gray-400 mb-0.5">
                Agent finished — no commit expected. Discard the scratch branch when you're done
                reviewing.
              </div>
            )}
            <div
              className="text-[10px] font-mono text-indigo-300/90 truncate"
              title={branchAgent.branch}
            >
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
                    const result = await discardAgentBranch(branchAgent.id);
                    if (result.ok) {
                      await selectPlan(planName);
                    } else {
                      toastError(result.error ?? "Discard failed");
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
                      const result = await mergeAgentBranch(branchAgent.id);
                      if (result.ok) {
                        await selectPlan(planName);
                      } else {
                        toastError(result.error ?? "Merge failed");
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
            <span className="text-[10px] text-gray-600">+{task.filePaths.length - 3} more</span>
          )}
        </div>
      )}

      {/* Description — editable */}
      <div className="mt-1.5 text-[11px] text-gray-400">
        <EditableText
          value={task.description}
          onSave={(v) => saveTaskField({ description: v })}
          label={`task ${task.number} description`}
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
          label={`task ${task.number} acceptance`}
          className="line-clamp-1"
          editClassName="text-[11px]"
          placeholder="Add acceptance criteria..."
        />
      </div>

      {/* Learnings — collapsed by default; lazy-fetches on first expand
          to keep large plans from firing N GETs at mount time. Backed by
          the append-only POST /api/plans/:name/tasks/:num/learnings; the
          EditableText acts as a single-shot "Add learning" input that
          remounts (via the count-keyed key) after each save so the
          previously-typed text doesn't linger in the next edit pass. */}
      <TaskLearnings planName={planName} taskNumber={task.number} />
    </div>
  );
}

interface LearningRow {
  id: number;
  learning: string;
  createdAt: string;
}

interface TaskLearningsProps {
  planName: string;
  taskNumber: string;
}

function TaskLearnings({ planName, taskNumber }: TaskLearningsProps) {
  const [open, setOpen] = useState(false);
  const [rows, setRows] = useState<LearningRow[] | null>(null);
  const [loading, setLoading] = useState(false);

  async function load() {
    setLoading(true);
    try {
      const data = await fetchJson<LearningRow[]>(
        `/api/plans/${encodeURIComponent(planName)}/tasks/${encodeURIComponent(
          taskNumber,
        )}/learnings`,
      );
      setRows(data);
    } catch (e) {
      toastError(e, "Load learnings failed");
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    if (open && rows === null && !loading) {
      load();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  async function addLearning(text: string) {
    const trimmed = text.trim();
    if (!trimmed) return;
    try {
      await postJson(
        `/api/plans/${encodeURIComponent(planName)}/tasks/${encodeURIComponent(
          taskNumber,
        )}/learnings`,
        { learning: trimmed },
      );
      await load();
    } catch (e) {
      toastError(e, "Add learning failed");
    }
  }

  const count = rows?.length ?? 0;
  const countLabel = rows ? ` (${count})` : "";

  return (
    <div className="mt-1.5">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="text-[10px] text-gray-500 hover:text-gray-300 transition flex items-center gap-1"
        title="Show or hide task learnings"
      >
        <span aria-hidden="true">{open ? "▾" : "▸"}</span>
        Learnings{countLabel}
      </button>
      {open && (
        <div className="mt-1 space-y-1">
          {loading && rows === null && (
            <div className="text-[10px] text-gray-600 italic">Loading…</div>
          )}
          {rows && rows.length === 0 && !loading && (
            <div className="text-[10px] text-gray-600 italic">No learnings recorded yet.</div>
          )}
          {rows?.map((row) => (
            <div
              key={row.id}
              className="text-[11px] text-gray-300 bg-gray-900/40 border border-gray-800/50 rounded px-1.5 py-1"
            >
              <div className="whitespace-pre-wrap break-words">{row.learning}</div>
              <div className="text-[9px] text-gray-600 mt-0.5" title={row.createdAt}>
                {formatRelative(row.createdAt)}
              </div>
            </div>
          ))}
          <div className="text-[11px] text-gray-400">
            <EditableText
              key={`add-${count}`}
              value=""
              onSave={addLearning}
              label={`add learning to task ${taskNumber}`}
              multiline
              placeholder="+ Add learning..."
              editClassName="text-[11px]"
            />
          </div>
        </div>
      )}
    </div>
  );
}

/// Wrapped with `memo` so an unrelated agent-store update — once the
/// useShallow selector inside `TaskCardInner` short-circuits — doesn't
/// re-render every card on the board. Re-renders are gated on the props
/// `task` / `planName` / `phaseNumber` and on the live store
/// subscriptions inside the inner component (which are now narrow).
/// Audit §10 minor.
export const TaskCard = memo(TaskCardInner);
