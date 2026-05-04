import { useEffect, useState } from "react";
import {
  usePlanStore,
  type ParsedPlan,
  type PlanConfig,
  type PlanConfigPatch,
  type PlanVerdict,
} from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { fetchJson, postJson, putJson } from "../api.js";
import { PhaseCard } from "./PhaseCard.js";
import { EditableText } from "./EditableText.js";

export function PlanBoard() {
  const plan = usePlanStore((s) => s.selectedPlan);
  const loading = usePlanStore((s) => s.loading);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const [converting, setConverting] = useState(false);
  const [resetting, setResetting] = useState(false);
  const [checkingAll, setCheckingAll] = useState(false);
  const [checkingPlan, setCheckingPlan] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState<string | null>(null);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const savePlan = usePlanStore((s) => s.savePlan);
  const driverCapabilities = useSettingsStore((s) => s.driverCapabilities);
  const agents = useAgentStore((s) => s.agents);
  const selectAgent = useAgentStore((s) => s.selectAgent);

  const isMd = plan?.filePath?.endsWith(".md") ?? false;

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full text-gray-500">
        Loading...
      </div>
    );
  }

  if (!plan) return null;

  // Aggregate stats
  const allTasks = plan.phases.flatMap((p) => p.tasks);
  const total = allTasks.length;
  const done = allTasks.filter(
    (t) => t.status === "completed" || t.status === "skipped"
  ).length;
  const inProgress = allTasks.filter((t) => t.status === "in_progress").length;
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;

  async function handleReset() {
    if (!plan) return;
    if (!confirm(`Reset all task statuses to pending for "${plan.title}"?`)) return;
    setResetting(true);
    setError(null);
    try {
      await postJson(`/api/plans/${plan.name}/reset-status`, {});
      await selectPlan(plan.name);
    } catch (e) {
      setError(`Reset failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setResetting(false);
    }
  }

  async function handleCheckAll() {
    if (!plan) return;
    const pendingCount = plan.phases
      .flatMap((p) => p.tasks)
      .filter((t) => !["completed", "skipped", "checking"].includes(t.status ?? "pending"))
      .length;
    if (!confirm(`Spawn ${pendingCount} check agents for this plan? This will use API credits.`)) return;
    setCheckingAll(true);
    setError(null);
    try {
      await postJson(`/api/plans/${plan.name}/check-all`, {});
    } catch (e) {
      setError(`Check all failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setCheckingAll(false);
    }
  }

  async function handleCheckPlan() {
    if (!plan) return;
    setCheckingPlan(true);
    setError(null);
    try {
      const res = await postJson<{ agentId: string }>(
        `/api/plans/${plan.name}/check`,
        {},
      );
      selectAgent(res.agentId);
    } catch (e) {
      setError(`Check Plan failed: ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setCheckingPlan(false);
    }
  }

  async function saveField(patch: Partial<typeof plan>) {
    if (!plan) return;
    const updated = { ...plan, ...patch };
    try {
      await savePlan(updated);
      await fetchPlans();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Save failed: ${msg}`);
    }
  }

  async function handleConvert() {
    setConverting(true);
    setError(null);
    try {
      await postJson(`/api/plans/${plan!.name}/convert`, {});
      await fetchPlans();
      await selectPlan(plan!.name);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Convert failed: ${msg}`);
      console.error("Convert failed:", e);
    } finally {
      setConverting(false);
    }
  }

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
            Created {new Date(plan.createdAt).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" })}
            {plan.modifiedAt !== plan.createdAt && (
              <> / Modified {new Date(plan.modifiedAt).toLocaleDateString("en-US", { month: "short", day: "numeric", year: "numeric" })}</>
            )}
          </span>
          {isMd && (
            <span className="text-amber-500/60 font-mono">.md</span>
          )}
        </div>
        <div className="flex items-center gap-3">
          <h2 className="text-xl font-bold">
            <EditableText
              value={plan.title}
              onSave={(v) => saveField({ title: v })}
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
            plan.totalCostUsd != null && plan.totalCostUsd > 0 && (
            <span
              className="text-xs text-amber-400 bg-amber-900/20 border border-amber-800/30 px-2 py-0.5 rounded"
              title="Total agent cost for this plan"
            >
              Total cost: ${plan.totalCostUsd.toFixed(2)}
            </span>
          )}
          <BudgetBadge plan={plan} />
          <AutoModeStatusPill planName={plan.name} />
        </div>
        <div className="flex items-center gap-3 mt-2">
          <div className="text-sm text-gray-400 max-w-3xl flex-1">
            <EditableText
              value={plan.context}
              onSave={(v) => saveField({ context: v })}
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
          <CheckPlanButton
            plan={plan}
            agents={agents}
            checkingPlan={checkingPlan}
            onCheck={handleCheckPlan}
            onViewAgent={selectAgent}
          />
          <button
            onClick={handleCheckAll}
            disabled={checkingAll || !plan.project}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-emerald-600 hover:text-emerald-400 disabled:opacity-50 disabled:hover:border-gray-700 disabled:hover:text-gray-400 text-gray-300 rounded transition"
            title="Spawn a check agent for every unfinished task in this plan"
          >
            {checkingAll ? "Spawning..." : "Check All"}
          </button>
          <button
            onClick={handleReset}
            disabled={resetting}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-red-600 hover:text-red-400 disabled:opacity-50 text-gray-300 rounded transition"
            title="Reset all task statuses to pending"
          >
            {resetting ? "Resetting..." : "Reset"}
          </button>
          <StaleBranchesButton planName={plan.name} onError={setError} onDone={() => selectPlan(plan.name)} />
        </div>
        <AutoModeControls planName={plan.name} />
        {/* Error toast */}
        {error && (
          <div className="mt-2 text-xs text-red-400 bg-red-900/20 border border-red-800/30 rounded px-3 py-2 inline-flex items-center gap-2">
            <span>{error}</span>
            <button onClick={() => setError(null)} className="text-red-600 hover:text-red-400 ml-2">
              dismiss
            </button>
          </div>
        )}
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
          <button
            key={f.value ?? "all"}
            onClick={() => setStatusFilter(f.value)}
            className={`px-2 py-0.5 text-[10px] rounded transition ${
              statusFilter === f.value
                ? `${f.color ?? "text-gray-200"} bg-gray-800 font-semibold`
                : "text-gray-600 hover:text-gray-400"
            }`}
          >
            {f.label}
          </button>
        ))}
      </div>

      {/* Phase cards -- vertical layout */}
      <div className="space-y-3 pb-4">
        {plan.phases.map((phase) => (
          <PhaseCard key={phase.number} phase={phase} planName={plan.name} statusFilter={statusFilter} />
        ))}
      </div>

      <VerificationSection verification={plan.verification ?? null} />
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
        <span className={`text-[10px] text-gray-600 transition-transform ${expanded ? "rotate-90" : ""}`}>
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
  const [draft, setDraft] = useState(
    plan.maxBudgetUsd != null ? String(plan.maxBudgetUsd) : ""
  );
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
  hasUniqueCommits: boolean;
}

interface StaleBranchesButtonProps {
  planName: string;
  onError: (msg: string | null) => void;
  onDone: () => void;
}

/// Two-stage button: click it to load the list of `branchwork/<plan>/*`
/// branches, then pick which to purge. Defaults to selecting only the
/// branches with no unique commits (the "agent exited without committing"
/// leftovers). Dangerous branches require a force opt-in.
function StaleBranchesButton({ planName, onError, onDone }: StaleBranchesButtonProps) {
  const [open, setOpen] = useState(false);
  const [loading, setLoading] = useState(false);
  const [branches, setBranches] = useState<StaleBranch[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [force, setForce] = useState(false);
  const [busy, setBusy] = useState(false);

  async function openAndLoad() {
    setOpen(true);
    setLoading(true);
    onError(null);
    try {
      const data = await fetchJson<{ branches: StaleBranch[] }>(
        `/api/plans/${planName}/branches/stale`,
      );
      setBranches(data.branches);
      // Default selection: safe-only (no unique commits).
      setSelected(
        new Set(data.branches.filter((b) => !b.hasUniqueCommits).map((b) => b.name)),
      );
    } catch (e) {
      onError(`Load branches failed: ${e instanceof Error ? e.message : String(e)}`);
      setOpen(false);
    } finally {
      setLoading(false);
    }
  }

  async function purge() {
    setBusy(true);
    onError(null);
    try {
      const toPurge = [...selected];
      const { results } = await postJson<{
        results: Array<{ branch: string; ok: boolean; error?: string }>;
      }>(`/api/plans/${planName}/branches/stale/purge`, { branches: toPurge, force });
      const failed = results.filter((r) => !r.ok);
      if (failed.length > 0) {
        onError(
          `${failed.length} failed: ` +
            failed.map((f) => `${f.branch} (${f.error})`).join(", "),
        );
      }
      setOpen(false);
      setSelected(new Set());
      setForce(false);
      onDone();
    } catch (e) {
      onError(`Purge failed: ${e instanceof Error ? e.message : String(e)}`);
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
      {open && (
        <div
          className="fixed inset-0 bg-black/50 z-50 flex items-center justify-center"
          onClick={() => !busy && setOpen(false)}
        >
          <div
            className="bg-gray-900 border border-gray-700 rounded-md shadow-xl p-4 max-w-2xl w-full max-h-[70vh] overflow-auto"
            onClick={(e) => e.stopPropagation()}
          >
            <h3 className="text-sm font-semibold mb-3">
              Stale branches for <span className="font-mono">{planName}</span>
            </h3>
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
                      return (
                        <tr key={b.name} className="border-b border-gray-800/50">
                          <td className="py-1 pr-2">
                            <input
                              type="checkbox"
                              checked={selected.has(b.name)}
                              disabled={risky && !force}
                              onChange={(e) => {
                                const next = new Set(selected);
                                if (e.target.checked) next.add(b.name);
                                else next.delete(b.name);
                                setSelected(next);
                              }}
                            />
                          </td>
                          <td className="py-1 pr-2 font-mono text-gray-300">{b.name}</td>
                          <td
                            className={`py-1 pr-2 ${
                              risky ? "text-amber-400" : "text-gray-500"
                            }`}
                          >
                            {b.commitsAheadOfTrunk ?? "?"}
                          </td>
                          <td className="py-1 pr-2 text-gray-500">
                            {b.lastCommitAgeSecs != null
                              ? formatAge(b.lastCommitAgeSecs)
                              : "?"}
                          </td>
                          <td className="py-1 pr-2 font-mono text-gray-600">
                            {b.agentId ? b.agentId.slice(0, 8) : "-"}
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
              <button
                onClick={() => setOpen(false)}
                disabled={busy}
                className="px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition"
              >
                Cancel
              </button>
              <button
                onClick={purge}
                disabled={busy || selected.size === 0}
                className="px-3 py-1.5 text-xs bg-red-700 hover:bg-red-600 disabled:opacity-50 text-white rounded transition"
              >
                {busy ? "Purging..." : `Delete ${selected.size}`}
              </button>
            </div>
          </div>
        </div>
      )}
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
  const [error, setError] = useState<string | null>(null);
  const [draftMaxFix, setDraftMaxFix] = useState<string>("");

  useEffect(() => {
    let alive = true;
    setError(null);
    fetchPlanConfig(planName)
      .then((c) => {
        if (!alive) return;
        setDraftMaxFix(String(c.maxFixAttempts));
      })
      .catch((e) => {
        if (!alive) return;
        setError(`Load config failed: ${e instanceof Error ? e.message : String(e)}`);
      });
    return () => {
      alive = false;
    };
  }, [planName, fetchPlanConfig]);

  async function update(patch: PlanConfigPatch) {
    setBusy(true);
    setError(null);
    try {
      const cfg = await putJson<PlanConfig>(`/api/plans/${planName}/config`, patch);
      setPlanConfig(planName, cfg);
      setDraftMaxFix(String(cfg.maxFixAttempts));
    } catch (e) {
      setError(`Save failed: ${e instanceof Error ? e.message : String(e)}`);
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

  if (!config && !error) {
    return <div className="mt-3 h-5" aria-hidden />;
  }

  return (
    <div className="flex items-center gap-4 mt-3 text-xs">
      {config && (
        <>
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
        </>
      )}
      {error && (
        <span className="text-red-400" role="alert">
          {error}
        </span>
      )}
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
  const [error, setError] = useState<string | null>(null);

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
    async function resume() {
      setResuming(true);
      setError(null);
      try {
        const cfg = await putJson<PlanConfig>(`/api/plans/${planName}/config`, {
          pausedReason: null,
        });
        setPlanConfig(planName, cfg);
      } catch (e) {
        setError(`Resume failed: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setResuming(false);
      }
    }
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
        {error && (
          <span className="text-red-400" role="alert">
            {error}
          </span>
        )}
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

/// Map raw `paused_reason` strings to short human-readable labels for the
/// pill copy. Unknown values fall through verbatim so future loop changes
/// don't render a stale label.
function humanPauseReason(reason: string): string {
  if (reason === "merge_conflict") return "merge conflict";
  if (reason === "fix_cap_reached") return "fix cap reached";
  if (reason === "ci_stalled") return "CI stalled";
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
