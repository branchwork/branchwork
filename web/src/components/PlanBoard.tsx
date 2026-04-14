import { useState } from "react";
import { usePlanStore, type ParsedPlan } from "../stores/plan-store.js";
import { postJson, putJson } from "../api.js";
import { PhaseCard } from "./PhaseCard.js";
import { EditableText } from "./EditableText.js";

interface SyncResult {
  summary: { total: number; completed: number; in_progress: number; pending: number };
}

export function PlanBoard() {
  const plan = usePlanStore((s) => s.selectedPlan);
  const loading = usePlanStore((s) => s.loading);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const [syncing, setSyncing] = useState(false);
  const [syncResult, setSyncResult] = useState<SyncResult | null>(null);
  const [converting, setConverting] = useState(false);
  const [resetting, setResetting] = useState(false);
  const [checkingAll, setCheckingAll] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [statusFilter, setStatusFilter] = useState<string | null>(null);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const savePlan = usePlanStore((s) => s.savePlan);

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

  async function handleSync() {
    setSyncing(true);
    setSyncResult(null);
    setError(null);
    try {
      const result = await postJson<SyncResult>(
        `/api/plans/${plan!.name}/auto-status`,
        {}
      );
      setSyncResult(result);
      // Refresh plan to get updated statuses
      await selectPlan(plan!.name);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Sync failed: ${msg}`);
      console.error("Sync failed:", e);
    } finally {
      setSyncing(false);
    }
  }

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
          {plan.totalCostUsd != null && plan.totalCostUsd > 0 && (
            <span
              className="text-xs text-amber-400 bg-amber-900/20 border border-amber-800/30 px-2 py-0.5 rounded"
              title="Total agent cost for this plan"
            >
              Total cost: ${plan.totalCostUsd.toFixed(2)}
            </span>
          )}
          <BudgetBadge plan={plan} />
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
          <button
            onClick={handleSync}
            disabled={syncing || !plan.project}
            className="flex-shrink-0 px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 disabled:opacity-50 disabled:hover:border-gray-700 disabled:hover:text-gray-400 text-gray-300 rounded transition"
            title={plan.project ? "Scan project files and git history to detect task statuses" : "Set a project first to enable auto-detection"}
          >
            {syncing ? "Scanning..." : "Sync Status"}
          </button>
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
        </div>
        {/* Error toast */}
        {error && (
          <div className="mt-2 text-xs text-red-400 bg-red-900/20 border border-red-800/30 rounded px-3 py-2 inline-flex items-center gap-2">
            <span>{error}</span>
            <button onClick={() => setError(null)} className="text-red-600 hover:text-red-400 ml-2">
              dismiss
            </button>
          </div>
        )}
        {/* Sync result toast */}
        {syncResult && (
          <div className="mt-2 text-xs text-gray-400 bg-gray-800/50 border border-gray-700 rounded px-3 py-2 inline-flex items-center gap-3">
            <span className="text-emerald-400">{syncResult.summary.completed} done</span>
            <span className="text-amber-400">{syncResult.summary.in_progress} active</span>
            <span className="text-gray-500">{syncResult.summary.pending} pending</span>
            <button onClick={() => setSyncResult(null)} className="text-gray-600 hover:text-gray-400 ml-2">
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
