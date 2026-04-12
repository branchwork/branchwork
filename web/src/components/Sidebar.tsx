import { useMemo, useState } from "react";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { postJson } from "../api.js";

function formatDate(iso: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  const now = new Date();
  const diffMs = now.getTime() - d.getTime();
  const diffDays = Math.floor(diffMs / 86400000);
  if (diffDays === 0) return "today";
  if (diffDays === 1) return "yesterday";
  if (diffDays < 30) return `${diffDays}d ago`;
  return d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
}

function isPlanDone(p: PlanSummary): boolean {
  return p.taskCount > 0 && p.doneCount >= p.taskCount;
}

interface Props {
  view: "plans" | "agents";
  onViewChange: (v: "plans" | "agents") => void;
}

export function Sidebar({ view, onViewChange }: Props) {
  const plans = usePlanStore((s) => s.plans);
  const selectedPlan = usePlanStore((s) => s.selectedPlan);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const agents = useAgentStore((s) => s.agents);
  const activeCount = agents.filter(
    (a) => a.status === "running" || a.status === "starting"
  ).length;
  const [syncingAll, setSyncingAll] = useState(false);
  const [showDone, setShowDone] = useState<Record<string, boolean>>({});

  async function handleSyncAll() {
    setSyncingAll(true);
    try {
      await postJson("/api/plans/sync-all", {});
      await fetchPlans();
      if (selectedPlan) await selectPlan(selectedPlan.name);
    } catch (e) {
      console.error("Sync all failed:", e);
    } finally {
      setSyncingAll(false);
    }
  }

  // Group plans by project, split active vs done within each group
  const grouped = useMemo(() => {
    const groups = new Map<string, { active: PlanSummary[]; done: PlanSummary[] }>();
    for (const p of plans) {
      const key = p.project ?? "Unassigned";
      if (!groups.has(key)) groups.set(key, { active: [], done: [] });
      const g = groups.get(key)!;
      if (isPlanDone(p)) {
        g.done.push(p);
      } else {
        g.active.push(p);
      }
    }
    return [...groups.entries()].sort((a, b) => {
      if (a[0] === "Unassigned") return 1;
      if (b[0] === "Unassigned") return -1;
      return a[0].localeCompare(b[0]);
    });
  }, [plans]);

  function renderPlanItem(p: PlanSummary, dimmed = false) {
    const pct = p.taskCount > 0 ? Math.round((p.doneCount / p.taskCount) * 100) : 0;
    return (
      <li key={p.name}>
        <button
          onClick={() => selectPlan(p.name)}
          className={`w-full text-left px-2 py-1.5 rounded text-sm transition ${
            selectedPlan?.name === p.name
              ? "bg-gray-800 text-white"
              : dimmed
              ? "text-gray-600 hover:text-gray-400 hover:bg-gray-800/50"
              : "text-gray-400 hover:text-gray-200 hover:bg-gray-800/50"
          }`}
          title={p.title}
        >
          <div className="truncate flex items-center gap-1.5">
            {dimmed && <span className="text-emerald-600 text-[10px]">&#10003;</span>}
            <span className="truncate">{p.title}</span>
          </div>
          <div className="text-[10px] text-gray-600 flex items-center gap-1">
            {p.taskCount > 0 && (
              <>
                <span>{p.doneCount}/{p.taskCount}</span>
                <span className="text-gray-700">({pct}%)</span>
              </>
            )}
            {p.taskCount === 0 && <span>{p.phaseCount} phases</span>}
            <span className="text-gray-700 ml-auto">{formatDate(p.modifiedAt)}</span>
          </div>
        </button>
      </li>
    );
  }

  return (
    <aside className="w-64 bg-gray-900 border-r border-gray-800 flex flex-col">
      {/* Logo */}
      <div className="p-4 border-b border-gray-800">
        <h1 className="text-lg font-bold tracking-tight">
          orchestr<span className="text-indigo-400">AI</span>
        </h1>
        <p className="text-xs text-gray-500 mt-0.5">Claude Code Dashboard</p>
      </div>

      {/* Nav */}
      <nav className="p-2 flex gap-1">
        <button
          onClick={() => onViewChange("plans")}
          className={`flex-1 px-3 py-1.5 rounded text-sm font-medium transition ${
            view === "plans"
              ? "bg-indigo-600 text-white"
              : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
          }`}
        >
          Plans
        </button>
        <button
          onClick={() => onViewChange("agents")}
          className={`flex-1 px-3 py-1.5 rounded text-sm font-medium transition relative ${
            view === "agents"
              ? "bg-indigo-600 text-white"
              : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
          }`}
        >
          Agents
          {activeCount > 0 && (
            <span className="absolute -top-1 -right-1 bg-emerald-500 text-white text-[10px] w-4 h-4 rounded-full flex items-center justify-center">
              {activeCount}
            </span>
          )}
        </button>
      </nav>

      {/* Sync all button */}
      <div className="px-2 pb-2">
        <button
          onClick={handleSyncAll}
          disabled={syncingAll}
          className="w-full px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-indigo-600 hover:text-indigo-400 disabled:opacity-50 text-gray-400 rounded transition"
        >
          {syncingAll ? "Scanning projects..." : "Sync All Statuses"}
        </button>
      </div>

      {/* Plan list grouped by project */}
      <div className="flex-1 overflow-auto p-2">
        {grouped.map(([project, { active, done }]) => (
          <div key={project} className="mb-3">
            <h3 className="text-[10px] font-semibold text-gray-500 uppercase tracking-wider px-2 mb-1 flex items-center gap-1.5">
              <span
                className={`w-1.5 h-1.5 rounded-full ${
                  project === "Unassigned" ? "bg-gray-600" : "bg-indigo-500"
                }`}
              />
              {project}
              <span className="text-gray-600 font-normal">
                ({active.length + done.length})
              </span>
            </h3>

            {/* Active plans */}
            <ul className="space-y-0.5">
              {active.map((p) => renderPlanItem(p))}
            </ul>

            {/* Completed plans — folded */}
            {done.length > 0 && (
              <div className="mt-1">
                <button
                  onClick={() =>
                    setShowDone((prev) => ({ ...prev, [project]: !prev[project] }))
                  }
                  className="w-full text-left px-2 py-1 text-[10px] text-gray-600 hover:text-gray-400 transition flex items-center gap-1"
                >
                  <span className="text-[8px]">
                    {showDone[project] ? "\u25BC" : "\u25B6"}
                  </span>
                  <span className="text-emerald-700">&#10003;</span>
                  {done.length} completed plan{done.length !== 1 ? "s" : ""}
                </button>
                {showDone[project] && (
                  <ul className="space-y-0.5">
                    {done.map((p) => renderPlanItem(p, true))}
                  </ul>
                )}
              </div>
            )}
          </div>
        ))}
      </div>
    </aside>
  );
}
