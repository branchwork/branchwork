import { useMemo, useState } from "react";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";

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

interface ProjectStats {
  name: string;
  plans: PlanSummary[];
  activePlans: PlanSummary[];
  donePlans: PlanSummary[];
  totalTasks: number;
  doneTasks: number;
  activeAgents: number;
  totalCost: number;
  lastActivity: string;
}

export function ProjectDashboard() {
  const plans = usePlanStore((s) => s.plans);
  const loading = usePlanStore((s) => s.loading);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const agents = useAgentStore((s) => s.agents);

  const projectStats: ProjectStats[] = useMemo(() => {
    const byProject = new Map<string, PlanSummary[]>();
    for (const p of plans) {
      const key = p.project ?? "Unassigned";
      if (!byProject.has(key)) byProject.set(key, []);
      byProject.get(key)!.push(p);
    }

    const activeByPlan = new Map<string, number>();
    for (const a of agents) {
      if (a.status !== "running" && a.status !== "starting") continue;
      if (!a.plan_name) continue;
      activeByPlan.set(a.plan_name, (activeByPlan.get(a.plan_name) ?? 0) + 1);
    }

    const stats: ProjectStats[] = [];
    for (const [name, projectPlans] of byProject) {
      const totalTasks = projectPlans.reduce((s, p) => s + p.taskCount, 0);
      const doneTasks = projectPlans.reduce((s, p) => s + p.doneCount, 0);
      const totalCost = projectPlans.reduce(
        (s, p) => s + (p.totalCostUsd ?? 0),
        0
      );
      const activeAgents = projectPlans.reduce(
        (s, p) => s + (activeByPlan.get(p.name) ?? 0),
        0
      );
      const lastActivity =
        [...projectPlans].sort((a, b) =>
          b.modifiedAt.localeCompare(a.modifiedAt)
        )[0]?.modifiedAt ?? "";

      const sortedPlans = [...projectPlans].sort((a, b) =>
        b.modifiedAt.localeCompare(a.modifiedAt)
      );
      stats.push({
        name,
        plans: sortedPlans,
        activePlans: sortedPlans.filter((p) => !isPlanDone(p)),
        donePlans: sortedPlans.filter(isPlanDone),
        totalTasks,
        doneTasks,
        activeAgents,
        totalCost,
        lastActivity,
      });
    }

    return stats.sort((a, b) => {
      if (a.name === "Unassigned") return 1;
      if (b.name === "Unassigned") return -1;
      if (a.activeAgents !== b.activeAgents) {
        return b.activeAgents - a.activeAgents;
      }
      return a.name.localeCompare(b.name);
    });
  }, [plans, agents]);

  const totalProjects = projectStats.length;
  const totalPlans = plans.length;
  const totalActiveAgents = agents.filter(
    (a) => a.status === "running" || a.status === "starting"
  ).length;
  const totalCost = plans.reduce((s, p) => s + (p.totalCostUsd ?? 0), 0);

  if (loading && plans.length === 0) {
    return (
      <div className="flex items-center justify-center h-full text-gray-500">
        Loading...
      </div>
    );
  }

  if (plans.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="text-4xl mb-3 text-gray-700">&#9776;</div>
          <p className="text-gray-500">No plans yet</p>
          <p className="text-xs text-gray-600 mt-1">
            Plans are loaded from ~/.claude/plans/
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      {/* Dashboard header */}
      <div className="mb-6">
        <h2 className="text-xl font-bold mb-1">Projects</h2>
        <div className="text-xs text-gray-500 flex items-center gap-3 flex-wrap">
          <span>
            {totalProjects} project{totalProjects !== 1 ? "s" : ""}
          </span>
          <span className="text-gray-700">/</span>
          <span>
            {totalPlans} plan{totalPlans !== 1 ? "s" : ""}
          </span>
          {totalActiveAgents > 0 && (
            <>
              <span className="text-gray-700">/</span>
              <span className="text-emerald-400">
                {totalActiveAgents} active agent
                {totalActiveAgents !== 1 ? "s" : ""}
              </span>
            </>
          )}
          {totalCost > 0 && (
            <>
              <span className="text-gray-700">/</span>
              <span className="text-amber-400">
                Total cost ${totalCost.toFixed(2)}
              </span>
            </>
          )}
        </div>
      </div>

      {/* Project cards grid */}
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-4">
        {projectStats.map((ps) => (
          <ProjectCard
            key={ps.name}
            stats={ps}
            onPlanClick={(name) => {
              void selectPlan(name);
            }}
          />
        ))}
      </div>
    </div>
  );
}

interface ProjectCardProps {
  stats: ProjectStats;
  onPlanClick: (name: string) => void;
}

function ProjectCard({ stats, onPlanClick }: ProjectCardProps) {
  const pct =
    stats.totalTasks > 0
      ? Math.round((stats.doneTasks / stats.totalTasks) * 100)
      : 0;
  const isUnassigned = stats.name === "Unassigned";
  const allDone = stats.totalTasks > 0 && stats.doneTasks === stats.totalTasks;

  return (
    <div className="rounded-lg border border-gray-800 bg-gray-900 overflow-hidden flex flex-col">
      {/* Project header */}
      <div className="p-4 border-b border-gray-800">
        <div className="flex items-start justify-between gap-3 mb-3">
          <div className="flex items-center gap-2 min-w-0">
            <span
              className={`w-2 h-2 rounded-full flex-shrink-0 ${
                isUnassigned ? "bg-gray-600" : "bg-indigo-500"
              }`}
            />
            <h3 className="text-base font-semibold truncate">{stats.name}</h3>
          </div>
          {stats.activeAgents > 0 && (
            <span className="flex-shrink-0 text-[10px] bg-emerald-900/30 border border-emerald-700/50 text-emerald-400 rounded-full px-2 py-0.5 font-medium flex items-center">
              <span className="inline-block w-1.5 h-1.5 bg-emerald-500 rounded-full mr-1.5 animate-pulse" />
              {stats.activeAgents} active
            </span>
          )}
        </div>

        {/* Stats row */}
        <div className="grid grid-cols-4 gap-2">
          <Stat
            label="Plans"
            value={stats.plans.length}
            sub={
              stats.donePlans.length > 0
                ? `${stats.donePlans.length} done`
                : undefined
            }
          />
          <Stat
            label="Tasks"
            value={
              stats.totalTasks > 0
                ? `${stats.doneTasks}/${stats.totalTasks}`
                : "-"
            }
          />
          <Stat
            label="Cost"
            value={
              stats.totalCost > 0 ? `$${stats.totalCost.toFixed(2)}` : "-"
            }
            valueClass={stats.totalCost > 0 ? "text-amber-400" : undefined}
          />
          <Stat label="Updated" value={formatDate(stats.lastActivity) || "-"} />
        </div>

        {/* Aggregate progress */}
        {stats.totalTasks > 0 && (
          <div className="mt-3">
            <div className="flex items-center justify-between text-[10px] text-gray-500 mb-1">
              <span>Overall progress</span>
              <span className="font-mono">{pct}%</span>
            </div>
            <div className="h-1.5 bg-gray-800 rounded-full overflow-hidden">
              <div
                className={`h-full rounded-full transition-all duration-300 ${
                  allDone ? "bg-emerald-500" : "bg-indigo-500"
                }`}
                style={{ width: `${pct}%` }}
              />
            </div>
          </div>
        )}
      </div>

      {/* Plans list */}
      <div className="divide-y divide-gray-800/50 max-h-80 overflow-auto">
        {stats.activePlans.length > 0 ? (
          stats.activePlans.map((p) => (
            <PlanRow
              key={p.name}
              plan={p}
              onClick={() => onPlanClick(p.name)}
            />
          ))
        ) : (
          <div className="px-4 py-3 text-xs text-gray-600">
            {stats.donePlans.length > 0
              ? "All plans completed"
              : "No plans yet"}
          </div>
        )}

        {stats.donePlans.length > 0 && (
          <DoneSection plans={stats.donePlans} onPlanClick={onPlanClick} />
        )}
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  sub,
  valueClass,
}: {
  label: string;
  value: string | number;
  sub?: string;
  valueClass?: string;
}) {
  return (
    <div>
      <div className="text-[10px] uppercase tracking-wider text-gray-600">
        {label}
      </div>
      <div
        className={`text-sm font-mono font-medium truncate ${
          valueClass ?? "text-gray-200"
        }`}
      >
        {value}
      </div>
      {sub && <div className="text-[10px] text-gray-600">{sub}</div>}
    </div>
  );
}

interface PlanRowProps {
  plan: PlanSummary;
  onClick: () => void;
  dimmed?: boolean;
}

function PlanRow({ plan, onClick, dimmed = false }: PlanRowProps) {
  const pct =
    plan.taskCount > 0 ? Math.round((plan.doneCount / plan.taskCount) * 100) : 0;
  const agents = useAgentStore((s) => s.agents);
  const planActive = agents.filter(
    (a) =>
      a.plan_name === plan.name &&
      (a.status === "running" || a.status === "starting")
  ).length;

  return (
    <button
      onClick={onClick}
      className={`w-full text-left px-4 py-2.5 hover:bg-gray-800/40 transition ${
        dimmed ? "opacity-60" : ""
      }`}
    >
      <div className="flex items-center gap-3">
        <div className="flex-1 min-w-0">
          <div className="text-sm text-gray-200 truncate flex items-center gap-1.5">
            {dimmed && (
              <span className="text-emerald-600 text-[10px]">&#10003;</span>
            )}
            <span className="truncate">{plan.title}</span>
            {planActive > 0 && (
              <span className="flex-shrink-0 inline-flex items-center gap-1 text-[10px] text-emerald-400">
                <span className="w-1.5 h-1.5 rounded-full bg-emerald-500 animate-pulse" />
                {planActive}
              </span>
            )}
          </div>
          <div className="text-[10px] font-mono text-gray-600 truncate flex items-center gap-2">
            <span className="truncate">{plan.name}</span>
            <span className="flex-shrink-0 text-gray-700">
              {formatDate(plan.modifiedAt)}
            </span>
            {plan.totalCostUsd != null && plan.totalCostUsd > 0 && (
              <span className="flex-shrink-0 text-amber-500/80">
                ${plan.totalCostUsd.toFixed(2)}
              </span>
            )}
          </div>
        </div>
        <div className="text-right flex-shrink-0">
          <div className="text-[10px] text-gray-500 flex items-center gap-1.5 justify-end">
            {plan.taskCount > 0 && (
              <span>
                {plan.doneCount}/{plan.taskCount}
              </span>
            )}
            <span className="font-mono text-gray-400 w-8 text-right">
              {pct}%
            </span>
          </div>
          <div className="w-24 h-1 bg-gray-800 rounded-full overflow-hidden mt-1">
            <div
              className={`h-full rounded-full ${
                pct === 100 ? "bg-emerald-500" : "bg-indigo-500"
              }`}
              style={{ width: `${pct}%` }}
            />
          </div>
        </div>
      </div>
    </button>
  );
}

function DoneSection({
  plans,
  onPlanClick,
}: {
  plans: PlanSummary[];
  onPlanClick: (name: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div>
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full text-left px-4 py-2 text-[10px] text-gray-600 hover:text-gray-400 hover:bg-gray-800/30 transition flex items-center gap-1.5"
      >
        <span className="text-[8px]">{expanded ? "\u25BC" : "\u25B6"}</span>
        <span className="text-emerald-700">&#10003;</span>
        {plans.length} completed plan{plans.length !== 1 ? "s" : ""}
      </button>
      {expanded &&
        plans.map((p) => (
          <PlanRow
            key={p.name}
            plan={p}
            onClick={() => onPlanClick(p.name)}
            dimmed
          />
        ))}
    </div>
  );
}
