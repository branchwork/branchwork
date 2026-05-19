import { useEffect, useMemo, useState } from "react";
import {
  usePlanStore,
  type ParsedPlan,
  type PlanConfig,
  type PlanConfigPatch,
  type PlanVerdict,
} from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { fetchJson, postJson, putJson } from "../api.js";
import { PhaseCard } from "./PhaseCard.js";
import { EditableText } from "./EditableText.js";
import { DeletePlanModal } from "./DeletePlanModal.js";
import { PlanSettings } from "./PlanSettings.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";
import { Tabs } from "./ui/Tabs.js";
import { TouchTarget } from "./ui/TouchTarget.js";
import { toastError } from "../lib/toast.js";
import { exportPlan } from "../lib/plan-export.js";
import { useGoToAgent } from "../hooks/use-route-selection.js";
import { StaleDataChip } from "./StaleDataChip.js";
import { CompletedTasksContext } from "../lib/plan-context.js";

type PlanBoardTab = "board" | "settings";

export function PlanBoard() {
  const plan = usePlanStore((s) => s.selectedPlan);
  const loading = usePlanStore((s) => s.loading);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const [converting, setConverting] = useState(false);
  const [resetting, setResetting] = useState(false);
  const [checkingAll, setCheckingAll] = useState(false);
  const [checkingPlan, setCheckingPlan] = useState(false);
  const [startingSession, setStartingSession] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [exportHash, setExportHash] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<PlanBoardTab>("board");
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [confirmReset, setConfirmReset] = useState(false);
  const [confirmCheckAll, setConfirmCheckAll] = useState(false);
  const planArchiveRetentionDays = useSettingsStore((s) => s.planArchiveRetentionDays);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const savePlan = usePlanStore((s) => s.savePlan);
  const driverCapabilities = useSettingsStore((s) => s.driverCapabilities);
  const agents = useAgentStore((s) => s.agents);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const goToAgent = useGoToAgent();

  const isMd = plan?.filePath?.endsWith(".md") ?? false;

  // Plan-wide set of completed/skipped task numbers. Computed once per
  // `plan` ref change and passed to descendants via `CompletedTasksContext`,
  // so each TaskCard's dependency-gate check is O(deps) instead of every
  // card rebuilding a fresh Set over every task in the plan (audit §10
  // minor). `useMemo` is hoisted above the early returns so React's hook
  // order stays stable across renders.
  const completedSet = useMemo<ReadonlySet<string>>(() => {
    if (!plan) return new Set<string>();
    return new Set<string>(
      plan.phases
        .flatMap((p) => p.tasks)
        .filter((t) => t.status === "completed" || t.status === "skipped")
        .map((t) => t.number),
    );
  }, [plan]);

  if (loading) {
    return <div className="flex items-center justify-center h-full text-gray-500">Loading...</div>;
  }

  if (!plan) return null;

  // Aggregate stats
  const allTasks = plan.phases.flatMap((p) => p.tasks);
  const total = allTasks.length;
  const done = allTasks.filter((t) => t.status === "completed" || t.status === "skipped").length;
  const inProgress = allTasks.filter((t) => t.status === "in_progress").length;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;

  async function handleReset() {
    if (!plan) return;
    setConfirmReset(false);
    setResetting(true);
    try {
      await postJson(`/api/plans/${plan.name}/reset-status`, {});
      await selectPlan(plan.name);
    } catch (e) {
      toastError(e, "Reset failed");
    } finally {
      setResetting(false);
    }
  }

  async function handleCheckAll() {
    if (!plan) return;
    setConfirmCheckAll(false);
    setCheckingAll(true);
    try {
      await postJson(`/api/plans/${plan.name}/check-all`, {});
    } catch (e) {
      toastError(e, "Check all failed");
    } finally {
      setCheckingAll(false);
    }
  }

  async function handleCheckPlan() {
    if (!plan) return;
    setCheckingPlan(true);
    try {
      const res = await postJson<{ agentId: string }>(`/api/plans/${plan.name}/check`, {});
      selectAgent(res.agentId);
      goToAgent(res.agentId);
    } catch (e) {
      toastError(e, "Check Plan failed");
    } finally {
      setCheckingPlan(false);
    }
  }

  async function handleStartSession() {
    if (!plan) return;
    setStartingSession(true);
    try {
      const res = await postJson<{ agentId: string }>(`/api/plans/${plan.name}/start-session`, {});
      selectAgent(res.agentId);
      goToAgent(res.agentId);
    } catch (e) {
      toastError(e, "Start session failed");
    } finally {
      setStartingSession(false);
    }
  }

  async function saveField(patch: Partial<typeof plan>) {
    if (!plan) return;
    const updated = { ...plan, ...patch };
    try {
      await savePlan(updated);
      await fetchPlans();
    } catch (e) {
      toastError(e, "Save failed");
    }
  }

  async function handleExport() {
    if (!plan) return;
    setExporting(true);
    try {
      const { sha256Hex: hash } = await exportPlan(plan.name);
      setExportHash(hash);
    } catch (e) {
      toastError(e, "Export failed");
    } finally {
      setExporting(false);
    }
  }

  async function handleConvert() {
    setConverting(true);
    try {
      await postJson(`/api/plans/${plan!.name}/convert`, {});
      await fetchPlans();
      await selectPlan(plan!.name);
    } catch (e) {
      toastError(e, "Convert failed");
    } finally {
      setConverting(false);
    }
  }

  const pendingCheckCount = plan.phases
    .flatMap((p) => p.tasks)
    .filter((t) => !["completed", "skipped", "checking"].includes(t.status ?? "pending")).length;

  return (
    <div className="p-6">
      {/* Plan header */}
      <div className="mb-6">
        <div className="flex items-center gap-3 mb-1 text-xs">
          {plan.project && (
            <span className="text-indigo-400 font-medium flex items-center gap-1.5">
              <span className="w-1.5 h-1.5 rounded-full bg-indigo-500" />
              {plan.project}
            </span>
          )}
          <span className="text-gray-600">
            Created{" "}
            {new Date(plan.createdAt).toLocaleDateString("en-US", {
              month: "short",
              day: "numeric",
              year: "numeric",
            })}
            {plan.modifiedAt !== plan.createdAt && (
              <>
                {" "}
                / Modified{" "}
                {new Date(plan.modifiedAt).toLocaleDateString("en-US", {
                  month: "short",
                  day: "numeric",
                  year: "numeric",
                })}
              </>
            )}
          </span>
          {isMd && <span className="text-amber-500/60 font-mono">.md</span>}
          {/* Stale-data marker. Self-gates on WS-disconnect + 60s of
              cache age — surfaces "this plan view may be out of date"
              right alongside the displayed Modified timestamp. */}
          <StaleDataChip slice="plans" />
        </div>
        <div className="flex items-center gap-3">
          <h2 className="text-xl font-bold">
            <EditableText
              value={plan.title}
              onSave={(v) => saveField({ title: v })}
              label="plan title"
              className="text-xl font-bold"
              editClassName="text-xl font-bold"
            />
            <span className="text-sm font-mono font-normal text-gray-600 ml-2">{plan.name}</span>
          </h2>
          <span className="text-xs text-gray-500 bg-gray-800 px-2 py-0.5 rounded">
            {done}/{total} tasks done ({pct}%)
            {inProgress > 0 && (
              <span className="text-amber-400 ml-1"> | {inProgress} in progress</span>
            )}
          </span>
          {driverCapabilities((plan as ParsedPlan & { driver?: string }).driver).supports_cost &&
            plan.totalCostUsd != null &&
            plan.totalCostUsd > 0 && (
              <span
                className="text-xs text-amber-400 bg-amber-900/20 border border-amber-800/30 px-2 py-0.5 rounded"
                title="Total agent cost for this plan"
              >
                Total cost: ${plan.totalCostUsd.toFixed(2)}
              </span>
            )}
          <BudgetBadge plan={plan} />
          <AutoModeStatusPill planName={plan.name} />
          <AutoPushRebasedPill planName={plan.name} />
          <AwaitingCadenceChip planName={plan.name} />
        </div>
        <div className="flex items-center gap-3 mt-2">
          <div className="text-sm text-gray-400 max-w-3xl flex-1">
            <EditableText
              value={plan.context}
              onSave={(v) => saveField({ context: v })}
              label="plan context"
              multiline
              className="line-clamp-2"
              editClassName="text-sm"
              placeholder="Add context..."
            />
          </div>
          {isMd && (
            <button
              onClick={handleConvert}
              disabled={converting}
              className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-amber-600 hover:text-amber-400 disabled:opacity-50 text-gray-300 rounded transition"
              title="Convert this plan from Markdown to YAML format"
            >
              {converting ? "Converting..." : "Convert to YAML"}
            </button>
          )}
          <ExportPlanButton exporting={exporting} exportHash={exportHash} onExport={handleExport} />
          <button
            onClick={handleStartSession}
            disabled={startingSession}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 disabled:opacity-50 text-gray-300 rounded transition"
            title="Spawn an agent in the plan's project directory with the plan loaded as context. No task assigned — the agent waits for your instructions."
          >
            {startingSession ? "Starting..." : "Start session"}
          </button>
          <CheckPlanButton
            plan={plan}
            agents={agents}
            checkingPlan={checkingPlan}
            onCheck={handleCheckPlan}
            onViewAgent={(id) => {
              selectAgent(id);
              goToAgent(id);
            }}
          />
          <button
            onClick={() => setConfirmCheckAll(true)}
            disabled={checkingAll || !plan.project}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-emerald-600 hover:text-emerald-400 disabled:opacity-50 disabled:hover:border-gray-700 disabled:hover:text-gray-400 text-gray-300 rounded transition"
            title="Spawn a check agent for every unfinished task in this plan"
          >
            {checkingAll ? "Spawning..." : "Check All"}
          </button>
          <button
            onClick={() => setConfirmReset(true)}
            disabled={resetting}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-red-600 hover:text-red-400 disabled:opacity-50 text-gray-300 rounded transition"
            title="Reset all task statuses to pending"
          >
            {resetting ? "Resetting..." : "Reset"}
          </button>
          <StaleBranchesButton planName={plan.name} onDone={() => selectPlan(plan.name)} />
        </div>
        <div className="flex items-center justify-between gap-3">
          <AutoModeControls planName={plan.name} />
          <button
            type="button"
            onClick={() => setDeleteOpen(true)}
            className="flex-shrink-0 mt-3 px-3 py-1.5 text-xs bg-red-900/30 border border-red-800/50 hover:bg-red-800/40 hover:border-red-600 text-red-300 hover:text-red-200 rounded transition"
            title="Move this plan to the archive (recoverable for the configured retention window)"
          >
            Delete plan
          </button>
        </div>
        {deleteOpen && (
          <DeletePlanModal
            planName={plan.name}
            retentionDays={planArchiveRetentionDays}
            onClose={() => setDeleteOpen(false)}
          />
        )}
        <ConfirmDialog
          open={confirmReset}
          title="Reset task statuses"
          description={`Reset all task statuses to pending for "${plan.title}"?`}
          confirmLabel={resetting ? "Resetting..." : "Reset"}
          confirmVariant="danger"
          confirmDisabled={resetting}
          onConfirm={handleReset}
          onCancel={() => setConfirmReset(false)}
        />
        <ConfirmDialog
          open={confirmCheckAll}
          title="Check all tasks"
          description={`Spawn ${pendingCheckCount} check agents for this plan? This will use API credits.`}
          confirmLabel={checkingAll ? "Spawning..." : `Spawn ${pendingCheckCount}`}
          confirmVariant="primary"
          confirmDisabled={checkingAll}
          onConfirm={handleCheckAll}
          onCancel={() => setConfirmCheckAll(false)}
        />
        {/* Overall progress */}
        {total > 0 && (
          <div className="mt-3 h-1.5 bg-gray-800 rounded-full overflow-hidden max-w-md">
            <div
              className="h-full bg-emerald-500 rounded-full transition-all duration-300"
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
      </div>

      <UncommittedWorkBanner planName={plan.name} />
      <RunnerOfflineBanner planName={plan.name} />
      <AutoPushRebaseConflictBanner planName={plan.name} />
      <BlockingWorkflowStalledBanner planName={plan.name} />
      <DeferredMergesBanner planName={plan.name} />

      {/* Tabs: Board (phase cards + filter) vs Settings (per-plan
          ci_blocking_workflows + phase_verification overrides + repo
          defaults). The header (title, controls, banners) is shared
          across both — only the body switches. */}
      <Tabs<PlanBoardTab>
        label="Plan view"
        value={activeTab}
        onChange={setActiveTab}
        className="border-b border-gray-800 mb-4"
        tabs={[
          { value: "board", label: "Board" },
          { value: "settings", label: "Settings" },
        ]}
      />

      {activeTab === "board" ? (
        <>
          {/* Status filter */}
          <div className="flex items-center gap-1 mb-4">
            <span className="text-[10px] text-gray-600 mr-1">Filter</span>
            {[
              { value: null, label: "All" },
              { value: "pending", label: "Pending", color: "text-gray-400" },
              { value: "in_progress", label: "Active", color: "text-amber-400" },
              { value: "completed", label: "Done", color: "text-emerald-400" },
              { value: "failed", label: "Failed", color: "text-red-400" },
            ].map((f) => (
              <TouchTarget key={f.value ?? "all"}>
                <button
                  onClick={() => setStatusFilter(f.value)}
                  className={`px-2 py-0.5 text-[10px] rounded transition ${
                    statusFilter === f.value
                      ? `${f.color ?? "text-gray-200"} bg-gray-800 font-semibold`
                      : "text-gray-600 hover:text-gray-400"
                  }`}
                >
                  {f.label}
                </button>
              </TouchTarget>
            ))}
          </div>

          {/* Phase cards -- vertical layout. The CompletedTasksContext
              provider hoists the plan-wide done/skipped Set to a single
              allocation per plan render (audit §10 minor): every TaskCard
              dependency-gate check reads from this Set instead of rebuilding
              its own. */}
          <CompletedTasksContext.Provider value={completedSet}>
            <div className="space-y-3 pb-4">
              {plan.phases.map((phase) => (
                <PhaseCard
                  key={phase.number}
                  phase={phase}
                  planName={plan.name}
                  statusFilter={statusFilter}
                />
              ))}
            </div>
          </CompletedTasksContext.Provider>

          <VerificationSection verification={plan.verification ?? null} />
        </>
      ) : (
        <PlanSettings planName={plan.name} />
      )}
    </div>
  );
}

interface CheckPlanButtonProps {
  plan: ParsedPlan;
  agents: Agent[];
  checkingPlan: boolean;
  onCheck: () => void;
  onViewAgent: (agentId: string) => void;
}

/// "Check Plan" button with verdict badge. Lives next to the plan header,
/// not per-task. Disabled when the plan has no `verification` block or no
/// associated project — tooltip explains which. While a plan-level check
/// agent is running it shows a spinner and the View affordance re-opens the
/// terminal. Verdict badge is persisted server-side and survives reloads.
interface ExportPlanButtonProps {
  exporting: boolean;
  exportHash: string | null;
  onExport: () => void;
}

/// "Export" affordance for the plan detail page (Phase 1, Task 1.2).
///
/// Fires `GET /api/plans/:name/export` via `exportPlan()`, which downloads
/// `<name>.plan.yaml` and surfaces the sha256 of the exact bytes that
/// went through the browser. The hash chip stays visible after the
/// download so a follow-up paste or re-upload can be cross-checked
/// against `sha256sum <name>.plan.yaml`.
export function ExportPlanButton({ exporting, exportHash, onExport }: ExportPlanButtonProps) {
  const shortHash = exportHash ? exportHash.slice(0, 7) : null;
  return (
    <span className="flex-shrink-0 inline-flex items-center gap-2">
      <button
        type="button"
        onClick={onExport}
        disabled={exporting}
        className="inline-flex items-center gap-1.5 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 disabled:opacity-50 text-gray-300 rounded transition"
        title="Download this plan's canonical YAML; the displayed hash matches `sha256sum <name>.plan.yaml`."
      >
        {exporting ? "Exporting..." : "Export"}
      </button>
      {shortHash && exportHash && (
        <span
          className="font-mono text-[10px] text-gray-500 bg-gray-900 border border-gray-800 px-1.5 py-0.5 rounded select-all"
          title={`sha256:${exportHash}\nMatches the bytes downloaded as the .plan.yaml attachment.`}
        >
          sha256:{shortHash}&hellip;
        </span>
      )}
    </span>
  );
}

function CheckPlanButton({
  plan,
  agents,
  checkingPlan,
  onCheck,
  onViewAgent,
}: CheckPlanButtonProps) {
  const hasVerification = !!(plan.verification && plan.verification.trim());
  const hasProject = !!plan.project;

  // A plan-level check agent is one whose plan_name matches but task_id is
  // null. There can only be one outstanding (the backend doesn't enforce it,
  // but in practice only one Check Plan button click is live at a time).
  const runningAgent = agents.find(
    (a) =>
      a.plan_name === plan.name &&
      !a.task_id &&
      (a.status === "running" || a.status === "starting"),
  );
  const running = !!runningAgent || checkingPlan;

  const verdict = plan.verdict ?? null;
  const badge = verdictBadge(verdict);
  const hasView = !!runningAgent || !!verdict?.agentId;

  const disabled = running || !hasVerification || !hasProject;
  const disabledReason = !hasVerification
    ? "Plan has no verification block — add one in the YAML"
    : !hasProject
      ? "Plan has no associated project"
      : null;
  const title = disabledReason
    ? disabledReason
    : running
      ? "Check Plan agent is running — click View to open terminal"
      : verdict
        ? `Last check: ${verdict.verdict}${verdict.reason ? ` — ${verdict.reason}` : ""} (${new Date(verdict.checkedAt).toLocaleString()})`
        : "Spawn a Check Plan agent to verify this plan against its verification block";

  return (
    <span className="flex-shrink-0 inline-flex items-center">
      <button
        onClick={onCheck}
        disabled={disabled}
        className={`inline-flex items-center gap-1.5 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-emerald-600 hover:text-emerald-400 disabled:opacity-50 disabled:hover:border-gray-700 disabled:hover:text-gray-400 text-gray-300 transition ${hasView ? "rounded-l" : "rounded"}`}
        title={title}
      >
        {running ? (
          <span
            className="w-3 h-3 rounded-full border-2 border-gray-500 border-t-emerald-400 animate-spin"
            aria-label="Checking"
          />
        ) : null}
        {running ? "Checking..." : "Check Plan"}
        {badge}
      </button>
      {runningAgent && (
        <button
          onClick={() => onViewAgent(runningAgent.id)}
          className="px-2 py-1.5 text-xs bg-gray-800 border border-l-0 border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-300 rounded-r transition"
          title="Open the Check Plan agent's terminal"
        >
          View
        </button>
      )}
      {!runningAgent && verdict?.agentId && (
        <button
          onClick={() => onViewAgent(verdict.agentId!)}
          className="px-2 py-1.5 text-xs bg-gray-800 border border-l-0 border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-300 rounded-r transition"
          title="Open the last Check Plan agent's terminal"
        >
          View
        </button>
      )}
    </span>
  );
}

/// Verdict badge renderer. Maps the three plan_verdicts statuses to a
/// coloured pill rendered inside the Check Plan button. Returns null when
/// no verdict has ever been recorded.
function verdictBadge(verdict: PlanVerdict | null) {
  if (!verdict) return null;
  const map: Record<string, { label: string; cls: string }> = {
    completed: {
      label: "\u2713",
      cls: "bg-emerald-600/30 text-emerald-300 border-emerald-500/40",
    },
    in_progress: {
      label: "\u25D0",
      cls: "bg-amber-600/30 text-amber-300 border-amber-500/40",
    },
    pending: {
      label: "\u2717",
      cls: "bg-red-600/30 text-red-300 border-red-500/40",
    },
  };
  const cfg = map[verdict.verdict] ?? map.pending;
  return (
    <span
      className={`inline-flex items-center justify-center text-[10px] font-semibold w-4 h-4 rounded-full border ${cfg.cls}`}
    >
      {cfg.label}
    </span>
  );
}

function VerificationSection({ verification }: { verification: string | null }) {
  const [expanded, setExpanded] = useState(false);
  if (!verification) return null;
  return (
    <div className="mt-6 pt-4 border-t border-gray-800">
      <button
        onClick={() => setExpanded((v) => !v)}
        className="w-full flex items-center gap-2 text-left text-xs text-gray-500 hover:text-gray-300 transition"
      >
        <span
          className={`text-[10px] text-gray-600 transition-transform ${expanded ? "rotate-90" : ""}`}
        >
          &#9654;
        </span>
        <span className="uppercase tracking-wide font-semibold">Verification</span>
      </button>
      {expanded && (
        <div className="mt-3 text-sm text-gray-400 whitespace-pre-wrap max-w-3xl">
          {verification}
        </div>
      )}
    </div>
  );
}

function BudgetBadge({ plan }: { plan: ParsedPlan }) {
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(plan.maxBudgetUsd != null ? String(plan.maxBudgetUsd) : "");
  const [saving, setSaving] = useState(false);

  const spent = plan.totalCostUsd ?? 0;
  const max = plan.maxBudgetUsd ?? null;
  const pct = max != null && max > 0 ? (spent / max) * 100 : 0;
  const exceeded = max != null && spent >= max;
  const approaching = max != null && !exceeded && pct >= 80;

  async function save(value: number | null) {
    setSaving(true);
    try {
      await putJson(`/api/plans/${plan.name}/budget`, {
        maxBudgetUsd: value,
      });
      await selectPlan(plan.name);
      await fetchPlans();
      setEditing(false);
    } finally {
      setSaving(false);
    }
  }

  if (editing) {
    return (
      <span className="text-xs bg-gray-800 border border-gray-700 rounded px-2 py-0.5 flex items-center gap-1.5">
        <span className="text-gray-500">Budget $</span>
        <input
          autoFocus
          type="number"
          min="0"
          step="0.01"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              const v = parseFloat(draft);
              save(Number.isFinite(v) && v > 0 ? v : null);
            } else if (e.key === "Escape") {
              setEditing(false);
            }
          }}
          className="bg-gray-900 border border-gray-700 rounded px-1 py-0 w-16 text-xs text-gray-200 outline-none focus:border-indigo-500"
          disabled={saving}
        />
        <button
          onClick={() => {
            const v = parseFloat(draft);
            save(Number.isFinite(v) && v > 0 ? v : null);
          }}
          disabled={saving}
          className="text-emerald-400 hover:text-emerald-300"
        >
          save
        </button>
        {max != null && (
          <button
            onClick={() => save(null)}
            disabled={saving}
            className="text-gray-500 hover:text-red-400"
            title="Clear budget"
          >
            clear
          </button>
        )}
      </span>
    );
  }

  if (max == null) {
    return (
      <button
        onClick={() => setEditing(true)}
        className="text-xs text-gray-500 hover:text-indigo-400 bg-gray-800/50 border border-dashed border-gray-700 px-2 py-0.5 rounded"
        title="Set a maximum budget for this plan"
      >
        + Set budget
      </button>
    );
  }

  const classes = exceeded
    ? "text-red-400 bg-red-900/20 border-red-800/40"
    : approaching
      ? "text-amber-300 bg-amber-900/30 border-amber-700/50"
      : "text-emerald-400 bg-emerald-900/20 border-emerald-800/30";

  return (
    <button
      onClick={() => {
        setDraft(String(max));
        setEditing(true);
      }}
      className={`text-xs px-2 py-0.5 rounded border ${classes}`}
      title={
        exceeded
          ? "Budget exceeded -- new agents are blocked"
          : approaching
            ? `Approaching budget limit (${pct.toFixed(0)}%)`
            : `Under budget (${pct.toFixed(0)}%)`
      }
    >
      {exceeded
        ? `Budget exceeded: $${spent.toFixed(2)} / $${max.toFixed(2)}`
        : `Budget: $${spent.toFixed(2)} / $${max.toFixed(2)}`}
    </button>
  );
}

interface StaleBranch {
  name: string;
  sha: string | null;
  commitsAheadOfTrunk: number | null;
  lastCommitAgeSecs: number | null;
  agentId: string | null;
  /// Agent row's status (running, starting, completed, failed, ...).
  /// `running` / `starting` are "live" — the modal disables the
  /// checkbox to mirror the server-side guard in
  /// `purge_stale_branches`, which refuses these even with `force`.
  agentStatus: string | null;
  hasUniqueCommits: boolean;
}

function isLiveAgent(s: string | null | undefined): boolean {
  return s === "running" || s === "starting";
}

interface StaleBranchesButtonProps {
  planName: string;
  onDone: () => void;
}

/// Two-stage button: click it to load the list of `branchwork/<plan>/*`
/// branches, then pick which to purge. Defaults to selecting only the
/// branches with no unique commits (the "agent exited without committing"
/// leftovers). Dangerous branches require a force opt-in.
export function StaleBranchesButton({ planName, onDone }: StaleBranchesButtonProps) {
  const [open, setOpen] = useState(false);
  const [loading, setLoading] = useState(false);
  const [branches, setBranches] = useState<StaleBranch[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [force, setForce] = useState(false);
  const [busy, setBusy] = useState(false);

  async function openAndLoad() {
    setOpen(true);
    setLoading(true);
    try {
      const data = await fetchJson<{ branches: StaleBranch[] }>(
        `/api/plans/${planName}/branches/stale`,
      );
      setBranches(data.branches);
      // Default selection: safe-only (no unique commits, no live agent).
      // Live-agent rows are also disabled below, so this just keeps the
      // initial state consistent with what the user can interact with.
      setSelected(
        new Set(
          data.branches
            .filter((b) => !b.hasUniqueCommits && !isLiveAgent(b.agentStatus))
            .map((b) => b.name),
        ),
      );
    } catch (e) {
      toastError(e, "Load branches failed");
      setOpen(false);
    } finally {
      setLoading(false);
    }
  }

  async function purge() {
    setBusy(true);
    try {
      const toPurge = [...selected];
      const { results } = await postJson<{
        results: Array<{ branch: string; ok: boolean; error?: string }>;
      }>(`/api/plans/${planName}/branches/stale/purge`, { branches: toPurge, force });
      const failed = results.filter((r) => !r.ok);
      if (failed.length > 0) {
        toastError(
          `${failed.length} failed: ` + failed.map((f) => `${f.branch} (${f.error})`).join(", "),
        );
      }
      setOpen(false);
      setSelected(new Set());
      setForce(false);
      onDone();
    } catch (e) {
      toastError(e, "Purge failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <button
        onClick={openAndLoad}
        className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-red-600 hover:text-red-400 disabled:opacity-50 text-gray-300 rounded transition"
        title="List and delete stale branchwork/* branches"
      >
        Clean Branches
      </button>
      <Modal
        open={open}
        onClose={() => {
          if (!busy) setOpen(false);
        }}
        title={`Stale branches for ${planName}`}
        size="2xl"
      >
        <div className="mt-3">
          {loading ? (
            <div className="text-gray-500 text-xs">Loading...</div>
          ) : branches.length === 0 ? (
            <div className="text-gray-500 text-xs">No branchwork/* branches found.</div>
          ) : (
            <>
              <table className="w-full text-xs">
                <thead className="text-gray-500">
                  <tr className="text-left border-b border-gray-800">
                    <th className="py-1 pr-2">Pick</th>
                    <th className="py-1 pr-2">Branch</th>
                    <th className="py-1 pr-2">Commits ahead</th>
                    <th className="py-1 pr-2">Age</th>
                    <th className="py-1 pr-2">Agent</th>
                  </tr>
                </thead>
                <tbody>
                  {branches.map((b) => {
                    const risky = b.hasUniqueCommits;
                    const live = isLiveAgent(b.agentStatus);
                    const checkboxTitle = live
                      ? `Agent ${b.agentStatus} — kill or finish it before deleting this branch`
                      : risky && !force
                        ? "Branch has unique commits — toggle 'force' below to allow deletion"
                        : undefined;
                    return (
                      <tr key={b.name} className="border-b border-gray-800/50">
                        <td className="py-1 pr-2">
                          <input
                            type="checkbox"
                            aria-label={`Select branch ${b.name}`}
                            checked={selected.has(b.name)}
                            disabled={live || (risky && !force)}
                            title={checkboxTitle}
                            onChange={(e) => {
                              const next = new Set(selected);
                              if (e.target.checked) next.add(b.name);
                              else next.delete(b.name);
                              setSelected(next);
                            }}
                          />
                        </td>
                        <td className="py-1 pr-2 font-mono text-gray-300">{b.name}</td>
                        <td className={`py-1 pr-2 ${risky ? "text-amber-400" : "text-gray-500"}`}>
                          {b.commitsAheadOfTrunk ?? "?"}
                        </td>
                        <td className="py-1 pr-2 text-gray-500">
                          {b.lastCommitAgeSecs != null ? formatAge(b.lastCommitAgeSecs) : "?"}
                        </td>
                        <td className="py-1 pr-2 font-mono text-gray-600">
                          {b.agentId ? (
                            <span className="inline-flex items-center gap-1.5">
                              <span>{b.agentId.slice(0, 8)}</span>
                              {live && (
                                <span
                                  className="px-1 py-0.5 text-[10px] rounded bg-indigo-900/40 text-indigo-300 border border-indigo-700/50"
                                  title="Agent is live — branch cannot be deleted until it stops"
                                >
                                  {b.agentStatus}
                                </span>
                              )}
                            </span>
                          ) : (
                            "-"
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
              <label className="flex items-center gap-2 mt-3 text-xs text-amber-400">
                <input
                  type="checkbox"
                  checked={force}
                  onChange={(e) => setForce(e.target.checked)}
                />
                Allow branches with unique commits (force)
              </label>
            </>
          )}
          <div className="flex justify-end gap-2 mt-4">
            <Button variant="ghost" size="sm" onClick={() => setOpen(false)} disabled={busy}>
              Cancel
            </Button>
            <Button
              variant="danger"
              size="sm"
              onClick={purge}
              disabled={busy || selected.size === 0}
              loading={busy}
            >
              {busy ? "Purging..." : `Delete ${selected.size}`}
            </Button>
          </div>
        </div>
      </Modal>
    </>
  );
}

function formatAge(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

const AUTO_MODE_TOOLTIP =
  "Auto-mode: merges each task on completion, waits for CI, fixes failures up to N times before pausing.";
const AUTO_ADVANCE_TOOLTIP =
  "Auto-advance: when a task completes, automatically start the next ready task in the plan.";
/// Hover text for the disabled Parallel switch. The toggle is rendered but
/// inert until worktree-per-agent isolation (ADR 0002) ships and the
/// project opts in. PUTs of `parallel: true` are rejected 412 in the
/// meantime — keeping the switch visible documents the future capability
/// without letting an operator trip the gate by accident.
const PARALLEL_DISABLED_TOOLTIP = "Available once worktree isolation ships";

/// Hover text for the per-plan Runner pin (T11.4). Explains the trade-off
/// upfront so the user understands why the plan can pause if the pinned
/// runner goes offline.
const RUNNER_PICKER_TOOLTIP =
  "Pin every spawn for this plan to a specific runner. Auto-mode pauses with reason 'runner_offline' if the pinned runner goes offline. Default 'any' picks the first online runner.";

/// Hover text for the per-plan Failover policy (T11.5). Surfaces the
/// shared-filesystem requirement upfront so the user understands the
/// trade-off before opting into sibling failover.
const RUNNER_FAILOVER_TOOLTIP =
  "When the pinned runner is offline: 'pause' (default) — pause auto-mode with reason 'runner_offline' and wait for the runner to return. 'sibling' — re-dispatch the next agent to a sibling online runner. For sibling to be useful, your runners must share project directories (NFS, git remote with auto-pull, or similar) — otherwise the sibling lacks the deleted runner's branches and uncommitted state.";

/// Disabled-tooltip shown when the user has not pinned a runner yet —
/// failover is meaningless without a pin to fail over from.
const RUNNER_FAILOVER_DISABLED_TOOLTIP =
  "Pin a runner first. Failover only applies when a plan is pinned to a specific runner.";

/// Plan-level auto-mode + auto-advance toggles. Reads/writes
/// `/api/plans/:name/config`. The `max_fix_attempts` input only renders
/// when auto-mode is on; 0 means "merge but never spawn a fix agent".
/// Edits to the number input are committed on blur or Enter, not per
/// keystroke — avoids a PUT per character and lets the user type "10".
function AutoModeControls({ planName }: { planName: string }) {
  const config = usePlanStore((s) => s.planConfigs[planName] ?? null);
  const fetchPlanConfig = usePlanStore((s) => s.fetchPlanConfig);
  const setPlanConfig = usePlanStore((s) => s.setPlanConfig);
  const [busy, setBusy] = useState(false);
  const [draftMaxFix, setDraftMaxFix] = useState<string>("");

  useEffect(() => {
    let alive = true;
    fetchPlanConfig(planName)
      .then((c) => {
        if (!alive) return;
        setDraftMaxFix(String(c.maxFixAttempts));
      })
      .catch((e) => {
        if (!alive) return;
        toastError(e, "Load config failed");
      });
    return () => {
      alive = false;
    };
  }, [planName, fetchPlanConfig]);

  async function update(patch: PlanConfigPatch) {
    setBusy(true);
    try {
      const cfg = await putJson<PlanConfig>(`/api/plans/${planName}/config`, patch);
      setPlanConfig(planName, cfg);
      setDraftMaxFix(String(cfg.maxFixAttempts));
    } catch (e) {
      toastError(e, "Save failed");
    } finally {
      setBusy(false);
    }
  }

  function commitMaxFix() {
    if (!config) return;
    const v = parseInt(draftMaxFix, 10);
    if (Number.isFinite(v) && v >= 0 && v <= 10) {
      if (v !== config.maxFixAttempts) {
        update({ maxFixAttempts: v });
      }
    } else {
      // revert invalid entry
      setDraftMaxFix(String(config.maxFixAttempts));
    }
  }

  if (!config) {
    return <div className="mt-3 h-5" aria-hidden />;
  }

  return (
    <div className="flex items-center gap-4 mt-3 text-xs">
      <Switch
        label="Auto-advance"
        title={AUTO_ADVANCE_TOOLTIP}
        checked={config.autoAdvance}
        disabled={busy}
        onChange={(v) => update({ autoAdvance: v })}
      />
      <Switch
        label="Auto-mode"
        title={AUTO_MODE_TOOLTIP}
        checked={config.autoMode}
        disabled={busy}
        onChange={(v) => update({ autoMode: v })}
      />
      <Switch
        label="Parallel"
        title={PARALLEL_DISABLED_TOOLTIP}
        checked={config.parallel}
        disabled
        onChange={() => {}}
      />
      <RunnerPicker
        runnerId={config.runnerId}
        disabled={busy}
        onChange={(rid) => update({ runnerId: rid })}
      />
      <FailoverPicker
        runnerId={config.runnerId}
        runnerFailover={config.runnerFailover ?? "pause"}
        disabled={busy}
        onChange={(policy) => update({ runnerFailover: policy })}
      />
      {config.autoMode && (
        <label
          className="flex items-center gap-1.5 text-gray-400"
          title="Max fix attempts per task before auto-mode pauses (0 = merge only, never spawn a fix agent)."
        >
          <span>Fix attempts</span>
          <input
            type="number"
            min={0}
            max={10}
            step={1}
            value={draftMaxFix}
            disabled={busy}
            onChange={(e) => setDraftMaxFix(e.target.value)}
            onBlur={commitMaxFix}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                (e.target as HTMLInputElement).blur();
              } else if (e.key === "Escape") {
                setDraftMaxFix(String(config.maxFixAttempts));
                (e.target as HTMLInputElement).blur();
              }
            }}
            className="bg-gray-900 border border-gray-700 rounded px-1.5 py-0.5 w-12 text-center text-gray-200 outline-none focus:border-indigo-500 disabled:opacity-50"
          />
        </label>
      )}
    </div>
  );
}

/// Per-plan runner pin (T11.4). Lists ONLINE runners + an "any" option;
/// reads/writes via `PlanConfigPatch.runnerId`. If the plan is currently
/// pinned to a runner that is offline (or no longer exists), the pinned
/// id is still rendered as an explicit option so the user can see what
/// it's set to and either repin or clear. Hidden in standalone mode
/// (no SaaS runners) — the picker is only meaningful when there are
/// runners to pick between.
export function RunnerPicker({
  runnerId,
  disabled,
  onChange,
}: {
  runnerId: string | null;
  disabled?: boolean;
  onChange: (id: string | null) => void;
}) {
  const runners = useRunnerStore((s) => s.runners);
  const mode = useRunnerStore((s) => s.mode);

  // Standalone deployment: hide entirely. The pin only matters when the
  // dispatcher is routing to runners.
  if (mode === "standalone") return null;
  // No runners enrolled yet: the dropdown would only carry "any". Hide
  // until there is a real choice.
  if (runners.length === 0 && !runnerId) return null;

  const value = runnerId ?? "__any__";
  const pinnedKnown = runnerId === null || runners.some((r) => r.id === runnerId);

  return (
    <label className="flex items-center gap-1.5 text-gray-400" title={RUNNER_PICKER_TOOLTIP}>
      <span>Runner</span>
      <select
        value={value}
        disabled={disabled}
        onChange={(e) => {
          const v = e.target.value;
          onChange(v === "__any__" ? null : v);
        }}
        className="bg-gray-900 border border-gray-700 rounded px-1.5 py-0.5 text-gray-200 outline-none focus:border-indigo-500 disabled:opacity-50"
        aria-label="Runner pin for this plan"
      >
        <option value="__any__">any (online)</option>
        {runners.map((r) => (
          <option key={r.id} value={r.id}>
            {r.name ?? r.id} {r.status !== "online" ? "(offline)" : ""}
          </option>
        ))}
        {/* Stale pin: the runner row vanished (revoked, never connected
            from this server, etc). Surface it explicitly so the user can
            see and clear it instead of silently swapping back to "any". */}
        {!pinnedKnown && runnerId && (
          <option key={runnerId} value={runnerId}>
            {runnerId} (unknown)
          </option>
        )}
      </select>
    </label>
  );
}

/// Per-plan runner failover policy (T11.5). Two-state toggle: "pause"
/// (today's behaviour, T11.4) or "sibling" (re-dispatch on offline).
/// Disabled until the user pins a runner — failover is meaningless
/// without a pin to fail over from. The shared-filesystem caveat lives
/// in the tooltip rather than the toggle label so the header stays
/// scannable.
export function FailoverPicker({
  runnerId,
  runnerFailover,
  disabled,
  onChange,
}: {
  runnerId: string | null;
  runnerFailover: "pause" | "sibling";
  disabled?: boolean;
  onChange: (policy: "pause" | "sibling") => void;
}) {
  const mode = useRunnerStore((s) => s.mode);

  // Standalone deployment: no runners, no failover concept.
  if (mode === "standalone") return null;

  const noPin = runnerId === null;
  const tooltip = noPin ? RUNNER_FAILOVER_DISABLED_TOOLTIP : RUNNER_FAILOVER_TOOLTIP;

  return (
    <label className="flex items-center gap-1.5 text-gray-400" title={tooltip}>
      <span>Failover</span>
      <select
        value={runnerFailover}
        disabled={disabled || noPin}
        onChange={(e) => {
          const v = e.target.value as "pause" | "sibling";
          onChange(v);
        }}
        className="bg-gray-900 border border-gray-700 rounded px-1.5 py-0.5 text-gray-200 outline-none focus:border-indigo-500 disabled:opacity-50"
        aria-label="Runner failover policy for this plan"
      >
        <option value="pause">pause</option>
        <option value="sibling">sibling</option>
      </select>
    </label>
  );
}

/// Banner above the board for the `agent_left_uncommitted_work` pause.
/// The Stop hook fires before graceful_exit in this case (per the
/// non-negotiable in the auto-mode plan), so the task agent's row is
/// still `running` and the user needs to inspect, then commit/discard,
/// then click Resume on the pill above. The "Inspect agent" button opens
/// the running agent's terminal panel so they can do exactly that.
///
/// T3.2 of the dirty-tree-check plan: the banner also reads the trimmed
/// `pausedFiles` list (≤5 paths) that the server captured at pause time
/// (T3.1) and renders it as a collapsible list. Each file has a
/// "Mark as operational" button that opens a modal previewing the
/// `branchwork.toml` ignore-list entry that would silence the false
/// positive (T2.1 of this plan supplies the matcher; the modal does NOT
/// write the file — operators copy the snippet manually because in SaaS
/// mode `branchwork.toml` lives on the runner host, not the server).
/// "Resume anyway" PUTs `pausedReason: null` to the existing config
/// endpoint, mirroring the AutoModeStatusPill resume path.
export function UncommittedWorkBanner({ planName }: { planName: string }) {
  const config = usePlanStore((s) => s.planConfigs[planName] ?? null);
  const setPlanConfig = usePlanStore((s) => s.setPlanConfig);
  const agents = useAgentStore((s) => s.agents);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const goToAgent = useGoToAgent();
  const [resuming, setResuming] = useState(false);
  const [markFile, setMarkFile] = useState<string | null>(null);

  if (config?.pausedReason !== "agent_left_uncommitted_work") return null;

  // Per the non-negotiable, the Stop-hook handler does NOT fire
  // graceful_exit when the tree is dirty — so the task agent that
  // triggered the pause is still in `running` (or transiently
  // `starting`). Sequential gate (3.5.1) keeps at most one task agent
  // alive per plan, so the first match is the one to inspect.
  const runningAgent = agents.find(
    (a) =>
      a.plan_name === planName &&
      a.task_id !== null &&
      (a.status === "running" || a.status === "starting"),
  );

  const pausedFiles = config.pausedFiles ?? [];

  async function handleResume() {
    setResuming(true);
    try {
      const cfg = await putJson<PlanConfig>(`/api/plans/${planName}/config`, {
        pausedReason: null,
      });
      setPlanConfig(planName, cfg);
    } catch (e) {
      toastError(e, "Resume failed");
    } finally {
      setResuming(false);
    }
  }

  return (
    <>
      <div
        role="alert"
        className="mb-4 flex items-start gap-3 rounded border border-red-700/50 bg-red-900/20 px-4 py-3 text-sm"
      >
        <span
          aria-hidden="true"
          className="mt-0.5 inline-flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-red-700/60 text-xs font-bold text-red-100"
        >
          !
        </span>
        <div className="flex-1 text-red-100">
          <div className="font-medium">Auto-mode paused: agent left uncommitted work.</div>
          <div className="mt-0.5 text-red-200/80">
            Inspect and either commit, discard, or click Resume.
          </div>
          {pausedFiles.length > 0 && (
            <details className="mt-2 text-red-200/90">
              <summary className="cursor-pointer select-none text-xs font-medium text-red-100/90 hover:text-white">
                Uncommitted file{pausedFiles.length === 1 ? "" : "s"} ({pausedFiles.length})
              </summary>
              <ul className="mt-1.5 space-y-1">
                {pausedFiles.map((file) => (
                  <li key={file} className="flex items-center justify-between gap-2 text-xs">
                    <span className="font-mono break-all text-red-100/90">{file}</span>
                    <button
                      type="button"
                      onClick={() => setMarkFile(file)}
                      className="flex-shrink-0 rounded border border-red-700/60 bg-red-900/30 px-2 py-0.5 text-[11px] text-red-100/90 transition hover:bg-red-800/50 hover:text-white"
                      title={`Preview adding ${file} to branchwork.toml's ignore list`}
                    >
                      Mark as operational
                    </button>
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
        <div className="flex flex-shrink-0 flex-col items-stretch gap-2 self-center sm:flex-row">
          <button
            type="button"
            onClick={() => {
              if (!runningAgent) return;
              selectAgent(runningAgent.id);
              goToAgent(runningAgent.id);
            }}
            disabled={!runningAgent}
            className="flex-shrink-0 rounded border border-red-700/60 bg-red-900/40 px-3 py-1 text-xs text-red-100 transition hover:bg-red-800/50 hover:text-white disabled:opacity-50 disabled:hover:bg-red-900/40 disabled:hover:text-red-100"
            title={
              runningAgent
                ? "Open the agent's terminal panel"
                : "No running agent found for this plan"
            }
          >
            Inspect agent
          </button>
          <button
            type="button"
            onClick={handleResume}
            disabled={resuming}
            className="flex-shrink-0 rounded border border-red-700/60 bg-red-900/40 px-3 py-1 text-xs text-red-100 transition hover:bg-red-800/50 hover:text-white disabled:opacity-50 disabled:hover:bg-red-900/40 disabled:hover:text-red-100"
            title="Clear the pause and re-evaluate from the last completed task"
          >
            {resuming ? "Resuming..." : "Resume anyway"}
          </button>
        </div>
      </div>
      <MarkAsOperationalModal
        open={markFile !== null}
        onClose={() => setMarkFile(null)}
        file={markFile ?? ""}
      />
    </>
  );
}

/// Modal that previews adding a file path to the
/// `[auto_mode.dirty_tree].ignore` list in `branchwork.toml`. Preview only
/// — no write: the file lives on the runner host in SaaS mode and on the
/// project root in standalone, so the dashboard can't reliably author
/// edits. The operator copies the snippet and applies it themselves.
///
/// Renders a synthetic unified-diff hunk so the user sees exactly the
/// line that would be added in the context of the section. The proposed
/// pattern is the path verbatim (gitignore-style: a basename-only
/// pattern like `server.log` matches the file at any depth, a pattern
/// with `/` matches the full path — see
/// `server-rs/src/repo_config.rs::matches_ignore_pattern`).
export function MarkAsOperationalModal({
  open,
  onClose,
  file,
}: {
  open: boolean;
  onClose: () => void;
  file: string;
}) {
  return (
    <Modal
      open={open}
      onClose={onClose}
      title="Mark as operational"
      description="Add this file to branchwork.toml's dirty-tree ignore list so it stops pausing auto-mode."
      size="2xl"
      closeOnBackdropClick={true}
    >
      <div className="mt-3 space-y-3 text-sm text-gray-200">
        <div>
          Edit{" "}
          <code className="rounded bg-gray-800 px-1.5 py-0.5 font-mono text-xs text-gray-100">
            branchwork.toml
          </code>{" "}
          at the project root and add the line marked{" "}
          <span className="font-mono text-emerald-400">+</span> below.
        </div>
        <pre
          aria-label="branchwork.toml diff preview"
          className="overflow-x-auto rounded border border-gray-700 bg-gray-950 p-3 font-mono text-xs leading-relaxed text-gray-200"
        >
          <span className="text-gray-500"> </span>
          <span className="text-gray-300">[auto_mode.dirty_tree]</span>
          {"\n"}
          <span className="text-gray-500"> </span>
          <span className="text-gray-300">ignore = [</span>
          {"\n"}
          <span className="text-gray-500"> </span>
          <span className="text-gray-500">{"    # ...existing patterns..."}</span>
          {"\n"}
          <span className="text-emerald-400">+</span>
          <span className="text-emerald-300">{`    "${file}",`}</span>
          {"\n"}
          <span className="text-gray-500"> </span>
          <span className="text-gray-300">]</span>
        </pre>
        <div className="text-xs text-gray-400">
          Patterns are gitignore-style: <code className="font-mono text-gray-300">basename</code>{" "}
          matches at any depth; <code className="font-mono text-gray-300">a/b/**</code> matches
          everything under a directory. Edit the value above if you want a wildcard family (e.g.{" "}
          <code className="font-mono text-gray-300">*.log</code>) instead of a literal path.
        </div>
        <div className="flex justify-end gap-2 pt-2">
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
        </div>
      </div>
    </Modal>
  );
}

/// Banner above the board for the `runner_offline` pause (T11.4). The
/// dispatcher pauses the plan when the pinned runner went offline at
/// spawn time. Resolution paths: (a) the pinned runner reconnects and
/// the user clicks Resume on the pill, OR (b) the user repins to a
/// different runner via the Runner picker (a re-pin to an online runner
/// auto-clears this pause server-side, see `put_plan_config`).
export function RunnerOfflineBanner({ planName }: { planName: string }) {
  const config = usePlanStore((s) => s.planConfigs[planName] ?? null);
  const runners = useRunnerStore((s) => s.runners);

  if (config?.pausedReason !== "runner_offline") return null;

  const pinned = runners.find((r) => r.id === config.runnerId);
  const pinnedLabel = pinned?.name ?? config.runnerId ?? "this runner";

  return (
    <div
      role="alert"
      className="mb-4 flex items-start gap-3 rounded border border-amber-700/50 bg-amber-900/20 px-4 py-3 text-sm"
    >
      <span
        aria-hidden="true"
        className="mt-0.5 inline-flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-amber-700/60 text-xs font-bold text-amber-100"
      >
        !
      </span>
      <div className="flex-1 text-amber-100">
        <div className="font-medium">
          Auto-mode paused: pinned runner <span className="font-mono">{pinnedLabel}</span> is
          offline.
        </div>
        <div className="mt-0.5 text-amber-200/80">
          Wait for the runner to reconnect and click Resume, or pick a different runner from the
          dropdown above.
        </div>
      </div>
    </div>
  );
}

/// Banner above the board for the `auto_push_rebase_conflict` pause —
/// the dedicated reason `ci::trigger_after_merge` writes when a post-
/// merge `git push` failed as non-fast-forward AND the follow-up
/// `git pull --rebase` produced CONFLICT (the rebased commit touches
/// the same lines as a commit on origin; auto-bump bumping `Cargo.toml`
/// line 3 while the task agent also edited line 3 is the canonical
/// trigger). The rebase was aborted before this banner rendered, so
/// the working tree is clean — the operator just needs to resolve the
/// overlap and click Resume on the pill above.
///
/// File list comes from the transient `autoPushRebaseConflicts` slice
/// in plan-store, populated by the `auto_mode_paused` WS handler. The
/// persistent half (`pausedReason === "auto_push_rebase_conflict"`)
/// survives reload; the file list does not, so a reload after the
/// pause renders the banner with a generic "see audit log" hint.
export function AutoPushRebaseConflictBanner({ planName }: { planName: string }) {
  const config = usePlanStore((s) => s.planConfigs[planName] ?? null);
  const conflict = usePlanStore((s) => s.autoPushRebaseConflicts[planName] ?? null);

  if (config?.pausedReason !== "auto_push_rebase_conflict") return null;

  // `conflict` may be null after a reload (server only persists the
  // reason, not the file list). The banner still shows, but with the
  // fallback copy that points the operator at the audit log.
  const branch = conflict?.branch ?? null;
  const files = conflict?.files ?? [];
  const fileCount = conflict?.fileCount ?? files.length;
  const overflow = fileCount > files.length;

  return (
    <div
      role="alert"
      className="mb-4 flex items-start gap-3 rounded border border-amber-700/50 bg-amber-900/20 px-4 py-3 text-sm"
    >
      <span
        aria-hidden="true"
        className="mt-0.5 inline-flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-amber-700/60 text-xs font-bold text-amber-100"
      >
        !
      </span>
      <div className="flex-1 text-amber-100">
        <div className="font-medium">
          Auto-mode paused: post-merge rebase hit a conflict
          {branch ? (
            <>
              {" "}
              on <span className="font-mono">{branch}</span>
            </>
          ) : null}
          .
        </div>
        {files.length > 0 ? (
          <div className="mt-1 text-amber-200/90">
            Conflicting file{files.length === 1 ? "" : "s"}:{" "}
            <span className="font-mono">{files.join(", ")}</span>
            {overflow ? (
              <span className="text-amber-200/70"> (+{fileCount - files.length} more)</span>
            ) : null}
          </div>
        ) : (
          <div className="mt-1 text-amber-200/90">
            File list not in memory (reload?) — see the Activity log for the full diff.
          </div>
        )}
        <div className="mt-1 text-amber-200/80">
          Resolve the overlap on the merge target (origin moved while the agent's branch was being
          merged), then click Resume.
        </div>
      </div>
    </div>
  );
}

/// Compact split-button chip that lives in the plan title bar alongside
/// `AutoModeStatusPill` and `AutoPushRebasedPill`. Mirrors the gating
/// logic of `DeferredMergesBanner` (Task 2.3) — both surfaces drain to
/// the same `/api/plans/:name/flush-merges` endpoint — but presents the
/// information in a single horizontal row suitable for the header
/// (Task 3.2). The detailed banner sits below the header and adds the
/// collapsible task list; the two coexist following the
/// `AutoModeStatusPill` + `UncommittedWorkBanner` precedent (header
/// pill + below-banner with detail).
///
/// Purple palette matches the per-task `awaiting_cadence` pill in
/// `TaskCard.tsx` (Task 3.1) so the user sees a consistent visual
/// language between the row-level pill and the plan-level chip. This
/// deliberately diverges from the `DeferredMergesBanner` indigo to
/// keep the header signal distinct from the more-verbose banner; the
/// banner remains the informational/action surface, the chip is the
/// at-a-glance indicator.
export function AwaitingCadenceChip({ planName }: { planName: string }) {
  const agents = useAgentStore((s) => s.agents);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [flushing, setFlushing] = useState(false);

  const count = useMemo(
    () =>
      agents.reduce(
        (n, a) =>
          a.plan_name === planName && a.merge_status === "deferred_for_cadence" ? n + 1 : n,
        0,
      ),
    [agents, planName],
  );

  if (count === 0) return null;

  const plural = count === 1 ? "" : "s";

  async function handleFlush() {
    setFlushing(true);
    try {
      const res = await postJson<{
        ok: boolean;
        merged: { agentId: string; taskId: string }[];
        count: number;
        paused: boolean;
        ciTriggered: boolean;
        message: string;
      }>(`/api/plans/${encodeURIComponent(planName)}/flush-merges`, {});
      if (res.paused) {
        toastError(new Error(res.message), "Flush paused");
      }
    } catch (e) {
      toastError(e, "Flush failed");
    } finally {
      setFlushing(false);
      setConfirmOpen(false);
    }
  }

  return (
    <>
      <span className="flex-shrink-0 inline-flex items-center gap-1.5 text-xs">
        <span
          className="inline-flex items-center gap-1.5 px-2 py-0.5 rounded-l border border-purple-700/60 bg-purple-900/30 text-purple-200"
          title={`${count} task${plural} finished cleanly but the auto-mode merge is waiting for the next cadence boundary`}
        >
          <span aria-hidden="true" className="w-1.5 h-1.5 rounded-full bg-purple-400" />
          {count} task{plural} awaiting cadence
        </span>
        <button
          type="button"
          onClick={() => setConfirmOpen(true)}
          disabled={flushing}
          className="px-2 py-0.5 -ml-1.5 rounded-r border border-l-0 border-purple-700/60 bg-purple-900/40 text-purple-100 hover:bg-purple-800/50 hover:text-white disabled:opacity-50 transition"
          title="Force-merge every deferred task right now and trigger a build outside the cadence"
        >
          {flushing ? "Flushing..." : "Flush now"}
        </button>
      </span>
      <ConfirmDialog
        open={confirmOpen}
        title="Flush deferred merges"
        description={`Merge ${count} deferred task${plural} to master now? This will trigger a build + deploy outside the configured cadence.`}
        confirmLabel={flushing ? "Flushing..." : `Merge ${count} now`}
        confirmVariant="primary"
        confirmDisabled={flushing}
        onConfirm={handleFlush}
        onCancel={() => setConfirmOpen(false)}
      />
    </>
  );
}

/// Banner that appears when one or more task agents in the plan have
/// completed cleanly but are sitting on `merge_status='deferred_for_cadence'`
/// — the cadence boundary hasn't been crossed yet (phase / plan).
///
/// Surfaces a Flush now button that POSTs to
/// `/api/plans/:name/flush-merges` and unconditionally drains every
/// deferred row (Task 2.3). The final merge in the batch triggers a
/// build + deploy outside the configured cadence; the confirm dialog
/// spells that out so the operator opts into the build cost
/// explicitly.
///
/// Indigo palette (informational, not error) to distinguish from the
/// red "paused on dirty tree" banner and the amber operator-recovery
/// banners (runner offline, rebase conflict, blocking workflow).
export function DeferredMergesBanner({ planName }: { planName: string }) {
  const agents = useAgentStore((s) => s.agents);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [flushing, setFlushing] = useState(false);

  const deferred = useMemo(
    () =>
      agents.filter((a) => a.plan_name === planName && a.merge_status === "deferred_for_cadence"),
    [agents, planName],
  );

  if (deferred.length === 0) return null;

  const count = deferred.length;
  const plural = count === 1 ? "" : "s";

  async function handleFlush() {
    setFlushing(true);
    try {
      const res = await postJson<{
        ok: boolean;
        merged: { agentId: string; taskId: string }[];
        count: number;
        paused: boolean;
        ciTriggered: boolean;
        message: string;
      }>(`/api/plans/${encodeURIComponent(planName)}/flush-merges`, {});
      // Per-merge `auto_mode_merged` events refresh the agent list as
      // each row's `merge_status` flips from `deferred_for_cadence` to
      // NULL — the banner disappears naturally once the list is empty.
      // The single `auto_mode_flushed_deferred` broadcast at the end
      // also lands but is informational (UI doesn't need to react to it
      // beyond the natural agent-list refresh).
      if (res.paused) {
        toastError(new Error(res.message), "Flush paused");
      }
    } catch (e) {
      toastError(e, "Flush failed");
    } finally {
      setFlushing(false);
      setConfirmOpen(false);
    }
  }

  return (
    <>
      <div
        role="status"
        className="mb-4 flex items-start gap-3 rounded border border-indigo-700/50 bg-indigo-900/20 px-4 py-3 text-sm"
      >
        <span
          aria-hidden="true"
          className="mt-0.5 inline-flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-indigo-700/60 text-xs font-bold text-indigo-100"
        >
          {count}
        </span>
        <div className="flex-1 text-indigo-100">
          <div className="font-medium">
            {count} task{plural} deferred for cadence
          </div>
          <div className="mt-0.5 text-indigo-200/80">
            {count === 1
              ? "An agent has finished, but the auto-mode merge is waiting for the next cadence boundary."
              : "Agents have finished, but their merges are batched until the next cadence boundary."}
          </div>
          <details className="mt-2 text-indigo-200/90">
            <summary className="cursor-pointer select-none text-xs font-medium text-indigo-100/90 hover:text-white">
              Deferred task{plural} ({count})
            </summary>
            <ul className="mt-1.5 space-y-0.5">
              {deferred.map((a) => (
                <li key={a.id} className="font-mono text-xs text-indigo-100/90">
                  {a.task_id ?? "(unknown task)"}
                </li>
              ))}
            </ul>
          </details>
        </div>
        <div className="flex flex-shrink-0 items-center">
          <button
            type="button"
            onClick={() => setConfirmOpen(true)}
            disabled={flushing}
            className="flex-shrink-0 rounded border border-indigo-700/60 bg-indigo-900/40 px-3 py-1 text-xs text-indigo-100 transition hover:bg-indigo-800/50 hover:text-white disabled:opacity-50 disabled:hover:bg-indigo-900/40 disabled:hover:text-indigo-100"
            title="Force-merge every deferred task right now and trigger a build outside the cadence"
          >
            {flushing ? "Flushing..." : "Flush now"}
          </button>
        </div>
      </div>
      <ConfirmDialog
        open={confirmOpen}
        title="Flush deferred merges"
        description={`Merge ${count} deferred task${plural} to master now? This will trigger a build + deploy outside the configured cadence.`}
        confirmLabel={flushing ? "Flushing..." : `Merge ${count} now`}
        confirmVariant="primary"
        confirmDisabled={flushing}
        onConfirm={handleFlush}
        onCancel={() => setConfirmOpen(false)}
      />
    </>
  );
}

/// Banner above the board that fires when a plan's
/// `ciBlockingWorkflows` lists a workflow name that does NOT exist as a
/// file in `.github/workflows/`. Without this hint the operator
/// discovers the misconfiguration only after `wait_for_ci` times out
/// (~20 min) and pauses the plan with `ci_stalled` — by then they have
/// already burned compute on an agent that was never going to merge.
///
/// Detection: polls
/// `GET /api/plans/:name/blocking-workflow-status` every 30 seconds.
/// The server returns `{ stalled: [{ workflow, branch, taskId, agentId }] }`
/// — one entry per (configured-but-missing workflow × running task
/// agent). Empty list → banner hidden.
///
/// Reuses the amber palette of `RunnerOfflineBanner` /
/// `AutoPushRebaseConflictBanner` since this is the same class of
/// operator-recoverable warning (fix the gate or the workflow trigger,
/// then push again).
export const BLOCKING_WORKFLOW_POLL_MS = 30_000;

type BlockingWorkflowEntry = {
  workflow: string;
  branch: string;
  taskId: string;
  agentId: string;
};

export function BlockingWorkflowStalledBanner({ planName }: { planName: string }) {
  const [stalled, setStalled] = useState<BlockingWorkflowEntry[]>([]);

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const res = await fetchJson<{ stalled: BlockingWorkflowEntry[] }>(
          `/api/plans/${encodeURIComponent(planName)}/blocking-workflow-status`,
        );
        if (!cancelled) setStalled(res.stalled);
      } catch {
        // Silent failure: a transient 5xx or auth blip shouldn't make
        // the banner flap. The next tick will retry.
        if (!cancelled) setStalled([]);
      }
    };
    load();
    const handle = setInterval(load, BLOCKING_WORKFLOW_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(handle);
    };
  }, [planName]);

  if (stalled.length === 0) return null;

  return (
    <div
      role="alert"
      className="mb-4 flex items-start gap-3 rounded border border-amber-700/50 bg-amber-900/20 px-4 py-3 text-sm"
    >
      <span
        aria-hidden="true"
        className="mt-0.5 inline-flex h-5 w-5 flex-shrink-0 items-center justify-center rounded-full bg-amber-700/60 text-xs font-bold text-amber-100"
      >
        !
      </span>
      <div className="flex-1 text-amber-100">
        <div className="font-medium">Blocking workflow has no matching file in the repo.</div>
        <ul className="mt-1 space-y-1 text-amber-200/90">
          {stalled.map((s) => (
            <li key={`${s.agentId}-${s.workflow}`}>
              Blocking workflow <span className="font-mono">{s.workflow}</span> hasn't run on{" "}
              <span className="font-mono">{s.branch}</span>. The plan will pause with{" "}
              <span className="font-mono">ci_stalled</span> — either fix the workflow trigger or
              change the gate in plan settings.
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

/// Live status pill for the auto-mode loop. Renders alongside the plan
/// title and reflects the current loop phase via the `auto_mode_state` /
/// `auto_mode_paused` / `auto_mode_merged` / `auto_mode_fix_spawned` WS
/// events. Hidden when auto-mode is off (no row, or `autoMode = false`)
/// AND no transient runtime state is in flight. Paused states render a
/// Resume button that PUTs `pausedReason: null` to the config endpoint.
export function AutoModeStatusPill({ planName }: { planName: string }) {
  const config = usePlanStore((s) => s.planConfigs[planName] ?? null);
  const runtime = usePlanStore((s) => s.autoModeRuntimes[planName] ?? null);
  const setPlanConfig = usePlanStore((s) => s.setPlanConfig);
  const [resuming, setResuming] = useState(false);

  // Pause is the persistent slice (lives in PlanConfig); transient labels
  // are the WS-driven slice (runtime). Persistent wins so a fresh page
  // load with `pausedReason` set still renders the paused pill.
  const isPaused = !!config?.pausedReason || runtime?.state === "paused";

  // Hide when auto-mode is off AND not paused AND no runtime in flight.
  // The "off but paused" combination can happen if the user toggles
  // auto-mode off while paused — we still show the paused pill so they
  // can resume; the toggle UX handles the actual disable.
  if (!config?.autoMode && !isPaused && !runtime) return null;
  if (!config) return null;

  if (isPaused) {
    const reason = config.pausedReason ?? runtime?.reason ?? "unknown";
    const label = humanPauseReason(reason);
    const resume = async () => {
      setResuming(true);
      try {
        const cfg = await putJson<PlanConfig>(`/api/plans/${planName}/config`, {
          pausedReason: null,
        });
        setPlanConfig(planName, cfg);
      } catch (e) {
        toastError(e, "Resume failed");
      } finally {
        setResuming(false);
      }
    };
    return (
      <span className="flex-shrink-0 inline-flex items-center gap-1.5 text-xs">
        <span
          className="inline-flex items-center gap-1.5 px-2 py-0.5 rounded-l border border-red-700/60 bg-red-900/30 text-red-300"
          title={`Auto-mode paused: ${reason}`}
        >
          <span className="w-1.5 h-1.5 rounded-full bg-red-400" />
          auto: paused — {label}
        </span>
        <button
          type="button"
          onClick={resume}
          disabled={resuming}
          className="px-2 py-0.5 -ml-1.5 rounded-r border border-l-0 border-red-700/60 bg-red-900/40 text-red-200 hover:bg-red-800/50 hover:text-red-100 disabled:opacity-50 transition"
          title="Clear the pause and re-evaluate from the last completed task"
        >
          {resuming ? "Resuming..." : "Resume"}
        </button>
      </span>
    );
  }

  if (runtime?.state === "auto_finishing") {
    const taskLabel = runtime.task ? ` task ${runtime.task}` : "";
    return (
      <span
        className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-sky-700/50 bg-sky-900/30 text-sky-200"
        title="Stop hook fired — auto-finishing the agent before the merge step"
      >
        <span className="w-1.5 h-1.5 rounded-full bg-sky-400 animate-pulse" />
        auto: finishing{taskLabel}
      </span>
    );
  }

  if (runtime?.state === "merging") {
    const taskLabel = runtime.task ? ` task ${runtime.task}` : "";
    return (
      <span
        className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-amber-700/50 bg-amber-900/30 text-amber-200"
        title="Auto-mode is merging the task branch into the canonical default"
      >
        <span className="w-1.5 h-1.5 rounded-full bg-amber-400 animate-pulse" />
        auto: merging{taskLabel}
      </span>
    );
  }

  if (runtime?.state === "awaiting_ci") {
    return (
      <span
        className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-indigo-700/50 bg-indigo-900/30 text-indigo-200"
        title="Auto-mode is waiting on CI for the merged commit"
      >
        <span className="w-1.5 h-1.5 rounded-full bg-indigo-400 animate-pulse" />
        auto: waiting on CI
      </span>
    );
  }

  if (runtime?.state === "fixing_ci") {
    const cap = config.maxFixAttempts;
    const attempt = runtime.attempt ?? 1;
    return (
      <span
        className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-orange-700/50 bg-orange-900/30 text-orange-200"
        title="A fix agent is running to address a CI failure"
      >
        <span className="w-1.5 h-1.5 rounded-full bg-orange-400 animate-pulse" />
        auto: fixing CI (attempt {attempt}/{cap})
      </span>
    );
  }

  // No runtime + auto-mode on + not paused → idle. Render a quiet badge so
  // the user knows the loop is armed (not running) without being noisy.
  return (
    <span
      className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-emerald-700/40 bg-emerald-900/20 text-emerald-300"
      title="Auto-mode is enabled and idle"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-emerald-500" />
      auto: idle
    </span>
  );
}

/// Transient pill that surfaces an auto-push rebase retry burst. Renders
/// for ~10 s after the most recent `auto_push_rebased` WS event;
/// re-renders on every event so a streak of N retries shows the count
/// climbing in real time. Sits next to `AutoModeStatusPill` so operators
/// see "the loop is racing the auto-bump" without needing to crack the
/// audit log open. Phase 1.2 of auto-push-rebase-on-non-fast-forward.
export function AutoPushRebasedPill({ planName }: { planName: string }) {
  const entry = usePlanStore((s) => s.autoPushRebases[planName] ?? null);
  // Local tick so the pill clears itself when `expiresAt` elapses
  // without needing another WS event to trigger a re-render. The
  // ws-store also schedules a setTimeout that explicitly clears the
  // store entry — this useEffect just guarantees the pill disappears
  // immediately when the user navigates back to a stale plan whose
  // expiresAt is already in the past.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!entry) return;
    const remaining = entry.expiresAt - Date.now();
    if (remaining <= 0) {
      setNow(Date.now());
      return;
    }
    const t = setTimeout(() => setNow(Date.now()), remaining + 50);
    return () => clearTimeout(t);
  }, [entry]);

  if (!entry) return null;
  if (entry.expiresAt <= now) return null;

  const label = entry.count === 1 ? `rebased on origin` : `rebased on origin (${entry.count})`;
  return (
    <span
      className="flex-shrink-0 inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border border-sky-700/40 bg-sky-900/20 text-sky-300"
      title={`Auto-mode rebased ${entry.branch} on origin after a non-fast-forward push (${entry.count} retr${entry.count === 1 ? "y" : "ies"})`}
    >
      <span className="w-1.5 h-1.5 rounded-full bg-sky-400" />
      {label}
    </span>
  );
}

/// Map raw `paused_reason` strings to short human-readable labels for the
/// pill copy. Unknown values fall through verbatim so future loop changes
/// don't render a stale label.
function humanPauseReason(reason: string): string {
  if (reason === "merge_conflict") return "merge conflict";
  if (reason === "fix_cap_reached") return "fix cap reached";
  if (reason === "ci_stalled") return "CI stalled";
  if (reason === "runner_offline") return "runner offline";
  if (reason === "agent_left_uncommitted_work") return "uncommitted work";
  if (reason === "auto_push_rebase_conflict") return "push rebase conflict";
  if (reason.startsWith("merge_failed")) return "merge failed";
  if (reason.startsWith("fix_merge_failed")) return "fix merge failed";
  if (reason.startsWith("fix_spawn_failed")) return "fix spawn failed";
  return reason;
}

interface SwitchProps {
  label: string;
  title: string;
  checked: boolean;
  disabled?: boolean;
  onChange: (v: boolean) => void;
}

function Switch({ label, title, checked, disabled, onChange }: SwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      disabled={disabled}
      title={title}
      className="flex items-center gap-2 disabled:opacity-50 disabled:cursor-not-allowed transition group"
    >
      <span
        className={`relative inline-flex h-4 w-7 rounded-full transition-colors ${
          checked
            ? "bg-emerald-600 group-hover:bg-emerald-500"
            : "bg-gray-700 group-hover:bg-gray-600"
        }`}
      >
        <span
          className={`absolute top-0.5 h-3 w-3 rounded-full bg-white shadow transition-transform ${
            checked ? "translate-x-3.5" : "translate-x-0.5"
          }`}
        />
      </span>
      <span
        className={
          checked ? "text-gray-200 font-medium" : "text-gray-400 group-hover:text-gray-300"
        }
      >
        {label}
      </span>
    </button>
  );
}

interface ConfirmDialogProps {
  open: boolean;
  title: string;
  description: string;
  confirmLabel: string;
  confirmVariant: "primary" | "danger";
  confirmDisabled: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/// Small confirm dialog wrapper around the `Modal` primitive — replaces
/// the `confirm()` calls flagged in audit §1 (PlanBoard reset / check-all,
/// PhaseCard check-phase). The Modal handles focus trap, Esc, and return
/// focus; this wrapper just supplies the standard two-button footer.
function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel,
  confirmVariant,
  confirmDisabled,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  return (
    <Modal open={open} onClose={onCancel} title={title} description={description}>
      <div className="mt-4 flex justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={onCancel} disabled={confirmDisabled}>
          Cancel
        </Button>
        <Button variant={confirmVariant} size="sm" onClick={onConfirm} disabled={confirmDisabled}>
          {confirmLabel}
        </Button>
      </div>
    </Modal>
  );
}
