import { useCallback, useMemo, useState } from "react";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { BulkDeleteModal } from "./BulkDeleteModal.js";

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

/// Auto-namer shape used by the Claude Code CLI: three lowercase
/// hyphen-separated tokens with the middle token ending in `-ing`
/// (adjective-verbing-noun). Examples that match: enumerated-crafting-puffin,
/// cosmic-toasting-lagoon, steady-prancing-squid. Examples that do NOT:
/// unify-check-prompts (middle is `check`), auto-mode-merge-ci-fix-loop
/// (six tokens). Anchored on both ends so a longer slug containing an
/// auto-named middle does not pass.
export const STALE_PLAN_NAME_RE = /^[a-z]+-[a-z]+ing-[a-z]+$/;

export function isAutoNamedPlan(name: string): boolean {
  return STALE_PLAN_NAME_RE.test(name);
}

export const STALE_PLAN_AGE_DAYS = 30;
const MS_PER_DAY = 86400000;

/// A plan is "stale" only when ALL three predicates hold:
/// 1. created more than 30 days ago,
/// 2. every task is completed or skipped (same as `isPlanDone`),
/// 3. the slug matches the CLI auto-namer shape.
/// Conservative on purpose — false negatives (a stale plan not flagged)
/// are recoverable; false positives flag an active plan and risk losing
/// work if the user trusts the heuristic blindly.
export function isStalePlan(p: PlanSummary, nowMs: number): boolean {
  if (!isPlanDone(p)) return false;
  if (!isAutoNamedPlan(p.name)) return false;
  if (!p.createdAt) return false;
  const created = Date.parse(p.createdAt);
  if (Number.isNaN(created)) return false;
  const ageDays = (nowMs - created) / MS_PER_DAY;
  return ageDays > STALE_PLAN_AGE_DAYS;
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
  const planArchiveRetentionDays = useSettingsStore(
    (s) => s.planArchiveRetentionDays,
  );

  // Multi-select + bulk-delete state. selectionMode toggles the
  // checkboxes on every plan row; selected holds the picked plan
  // names across project cards (a Set so add/remove/has are O(1)).
  // Both reset when the user leaves selection mode.
  const [selectionMode, setSelectionMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [bulkDeleteOpen, setBulkDeleteOpen] = useState(false);
  // Opt-in stale-plan filter. Default off so the dashboard's first
  // impression remains "every plan I have"; flipping it on filters
  // the cards and exposes a "Select all" affordance for the bulk-delete
  // workflow. Selection state is preserved across the toggle so the
  // user can mix stale + non-stale picks if they really want to.
  const [showStale, setShowStale] = useState(false);

  const toggleSelected = useCallback((name: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  }, []);

  // Drop a name from selection — used by the bulk modal as plans
  // succeed (so a 409 mid-stream leaves only the blocker + remainder
  // selected for retry). Stable identity so the modal's deps array
  // does not re-fire on every parent render.
  const removeFromSelection = useCallback((name: string) => {
    setSelected((prev) => {
      if (!prev.has(name)) return prev;
      const next = new Set(prev);
      next.delete(name);
      return next;
    });
  }, []);

  function exitSelectionMode() {
    setSelectionMode(false);
    setSelected(new Set());
    setBulkDeleteOpen(false);
  }

  const projectStats: ProjectStats[] = useMemo(() => {
    const nowMs = Date.now();
    const visiblePlans = showStale
      ? plans.filter((p) => isStalePlan(p, nowMs))
      : plans;

    const byProject = new Map<string, PlanSummary[]>();
    for (const p of visiblePlans) {
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
  }, [plans, agents, showStale]);

  const visibleStaleCount = showStale
    ? projectStats.reduce((s, ps) => s + ps.plans.length, 0)
    : 0;

  const selectAllStale = useCallback(() => {
    const nowMs = Date.now();
    setSelectionMode(true);
    setSelected((prev) => {
      const next = new Set(prev);
      for (const p of plans) {
        if (isStalePlan(p, nowMs)) next.add(p.name);
      }
      return next;
    });
  }, [plans]);

  const totalProjects = projectStats.length;
  const totalPlans = plans.length;
  const totalActiveAgents = agents.filter(
    (a) => a.status === "running" || a.status === "starting"
  ).length;
  const totalCost = plans.reduce((s, p) => s + (p.totalCostUsd ?? 0), 0);

  // Selected list ordered by their visible position on the dashboard,
  // so the modal lists plans in the same order the user sees them.
  // Walking projectStats (already sorted) lets the bulk modal show
  // names in dashboard order — important when the 409 banner names
  // a plan and the user is scanning the list to find it.
  //
  // MUST be declared before any early returns below — Rules of Hooks
  // require all hooks to fire on every render.
  const orderedSelected: string[] = useMemo(() => {
    const out: string[] = [];
    for (const ps of projectStats) {
      for (const p of ps.plans) {
        if (selected.has(p.name)) out.push(p.name);
      }
    }
    return out;
  }, [projectStats, selected]);

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
    <div className="p-6 pb-20">
      {/* Dashboard header */}
      <div className="mb-6 flex items-start justify-between gap-3">
        <div>
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
        <div className="flex-shrink-0 flex items-center gap-2">
          <button
            type="button"
            onClick={() => setShowStale((v) => !v)}
            aria-pressed={showStale}
            className={`px-3 py-1.5 text-xs rounded border transition ${
              showStale
                ? "bg-amber-900/40 border-amber-700 text-amber-200 hover:bg-amber-800/40"
                : "bg-gray-800 border-gray-700 text-gray-300 hover:border-gray-600 hover:text-gray-100"
            }`}
            title="Filter to plans likely safe to retire: completed, auto-named, and older than 30 days"
            data-testid="show-stale-toggle"
          >
            {showStale ? "Showing stale" : "Show stale plans"}
          </button>
          <button
            type="button"
            onClick={() =>
              selectionMode ? exitSelectionMode() : setSelectionMode(true)
            }
            aria-pressed={selectionMode}
            className={`px-3 py-1.5 text-xs rounded border transition ${
              selectionMode
                ? "bg-indigo-900/40 border-indigo-700 text-indigo-200 hover:bg-indigo-800/40"
                : "bg-gray-800 border-gray-700 text-gray-300 hover:border-gray-600 hover:text-gray-100"
            }`}
            title={
              selectionMode
                ? "Exit selection mode"
                : "Pick multiple plans to delete in one batch"
            }
          >
            {selectionMode ? "Cancel selection" : "Select"}
          </button>
        </div>
      </div>

      {showStale && (
        <div
          className="mb-4 rounded border border-amber-800/60 bg-amber-900/20 px-4 py-2.5 flex items-center justify-between gap-3"
          data-testid="stale-banner"
        >
          <div className="text-xs text-amber-200">
            {visibleStaleCount === 0 ? (
              <>No stale plans &mdash; your dashboard is clean.</>
            ) : (
              <>
                Showing <span className="font-mono">{visibleStaleCount}</span>{" "}
                plan{visibleStaleCount !== 1 ? "s" : ""} that look retireable
                (completed, auto-named, &gt; 30 days old).
              </>
            )}
          </div>
          {visibleStaleCount > 0 && (
            <button
              type="button"
              onClick={selectAllStale}
              className="flex-shrink-0 px-2.5 py-1 text-xs bg-amber-700/60 hover:bg-amber-600/70 text-amber-50 rounded transition"
              data-testid="select-all-stale"
            >
              Select all {visibleStaleCount}
            </button>
          )}
        </div>
      )}

      {/* Project cards grid */}
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-4">
        {projectStats.map((ps) => (
          <ProjectCard
            key={ps.name}
            stats={ps}
            onPlanClick={(name) => {
              void selectPlan(name);
            }}
            selectionMode={selectionMode}
            selected={selected}
            onToggleSelect={toggleSelected}
            forceExpandDone={showStale}
          />
        ))}
      </div>

      {/* Sticky bulk-delete footer */}
      {selectionMode && selected.size > 0 && (
        <div
          className="fixed inset-x-0 bottom-0 z-40 border-t border-gray-700 bg-gray-900/95 backdrop-blur px-6 py-3 flex items-center justify-between gap-3"
          data-testid="bulk-delete-footer"
        >
          <span className="text-sm text-gray-200">
            <span className="font-mono">{selected.size}</span> plan
            {selected.size !== 1 ? "s" : ""} selected
          </span>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={exitSelectionMode}
              className="px-3 py-1.5 text-xs text-gray-300 hover:text-gray-100 transition"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={() => setBulkDeleteOpen(true)}
              className="px-3 py-1.5 text-xs bg-red-700 hover:bg-red-600 text-white rounded transition"
            >
              Delete {selected.size}
            </button>
          </div>
        </div>
      )}

      {bulkDeleteOpen && (
        <BulkDeleteModal
          planNames={orderedSelected}
          retentionDays={planArchiveRetentionDays}
          onClose={() => setBulkDeleteOpen(false)}
          onPlanDeleted={removeFromSelection}
        />
      )}
    </div>
  );
}

interface ProjectCardProps {
  stats: ProjectStats;
  onPlanClick: (name: string) => void;
  selectionMode: boolean;
  selected: Set<string>;
  onToggleSelect: (name: string) => void;
  forceExpandDone?: boolean;
}

function ProjectCard({
  stats,
  onPlanClick,
  selectionMode,
  selected,
  onToggleSelect,
  forceExpandDone = false,
}: ProjectCardProps) {
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
              selectionMode={selectionMode}
              selected={selected.has(p.name)}
              onToggleSelect={() => onToggleSelect(p.name)}
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
          <DoneSection
            plans={stats.donePlans}
            onPlanClick={onPlanClick}
            selectionMode={selectionMode}
            selected={selected}
            onToggleSelect={onToggleSelect}
            forceExpand={forceExpandDone}
          />
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
  selectionMode: boolean;
  selected: boolean;
  onToggleSelect: () => void;
}

function PlanRow({
  plan,
  onClick,
  dimmed = false,
  selectionMode,
  selected,
  onToggleSelect,
}: PlanRowProps) {
  const pct =
    plan.taskCount > 0 ? Math.round((plan.doneCount / plan.taskCount) * 100) : 0;
  const agents = useAgentStore((s) => s.agents);
  const planActive = agents.filter(
    (a) =>
      a.plan_name === plan.name &&
      (a.status === "running" || a.status === "starting")
  ).length;

  // Selection mode: render the row as a <label> wrapping the
  // checkbox + the same content. Clicking either the checkbox OR
  // anywhere on the row toggles selection (the label's natural
  // click forwarding handles that). Tab focuses the checkbox,
  // Space toggles natively. We do NOT call onClick (selectPlan)
  // in selection mode — picking a plan must not navigate away.
  const rowBase =
    "w-full text-left px-4 py-2.5 hover:bg-gray-800/40 transition flex items-center gap-3";
  const dimmedClass = dimmed ? "opacity-60" : "";

  const inner = (
    <>
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
    </>
  );

  if (selectionMode) {
    return (
      <label
        className={`${rowBase} ${dimmedClass} cursor-pointer ${
          selected ? "bg-indigo-900/20" : ""
        }`}
      >
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggleSelect}
          aria-label={`Select plan ${plan.title}`}
          className="flex-shrink-0 w-4 h-4 accent-indigo-500"
        />
        {inner}
      </label>
    );
  }

  return (
    <button onClick={onClick} className={`${rowBase} ${dimmedClass}`}>
      {inner}
    </button>
  );
}

function DoneSection({
  plans,
  onPlanClick,
  selectionMode,
  selected,
  onToggleSelect,
  forceExpand = false,
}: {
  plans: PlanSummary[];
  onPlanClick: (name: string) => void;
  selectionMode: boolean;
  selected: Set<string>;
  onToggleSelect: (name: string) => void;
  forceExpand?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  // Auto-expand done section in selection mode \u2014 the bootstrap-era
  // auto-named plans this feature targets are typically completed
  // (the original motivation in plan.context.md), and forcing the
  // user to expand each project's done list before they can pick
  // those plans defeats the bulk-cleanup workflow. Stale-filter mode
  // (forceExpand) shares the same rationale: every stale plan is by
  // definition completed, so collapsing them defeats the filter.
  const showRows = expanded || selectionMode || forceExpand;
  return (
    <div>
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full text-left px-4 py-2 text-[10px] text-gray-600 hover:text-gray-400 hover:bg-gray-800/30 transition flex items-center gap-1.5"
      >
        <span className="text-[8px]">{showRows ? "\u25BC" : "\u25B6"}</span>
        <span className="text-emerald-700">&#10003;</span>
        {plans.length} completed plan{plans.length !== 1 ? "s" : ""}
      </button>
      {showRows &&
        plans.map((p) => (
          <PlanRow
            key={p.name}
            plan={p}
            onClick={() => onPlanClick(p.name)}
            dimmed
            selectionMode={selectionMode}
            selected={selected.has(p.name)}
            onToggleSelect={() => onToggleSelect(p.name)}
          />
        ))}
    </div>
  );
}
