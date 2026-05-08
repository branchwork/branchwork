import { memo, useEffect, useMemo, useState } from "react";
import { Link, NavLink, useLocation } from "react-router-dom";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useUiStore } from "../stores/ui-store.js";
import { postJson } from "../api.js";
import { formatRelative } from "../lib/time.js";
import { toastError } from "../lib/toast.js";
import { isPlanDone } from "../lib/predicates.js";
import { StaleDataChip } from "./StaleDataChip.js";
import { HealthStateIndicator } from "./HealthStateIndicator.js";
import { TouchTarget } from "./ui/TouchTarget.js";

export function Sidebar() {
  const plans = usePlanStore((s) => s.plans);
  const selectedPlanName = usePlanStore((s) => s.selectedPlan?.name ?? null);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const warnings = usePlanStore((s) => s.warnings);
  const dismissWarning = usePlanStore((s) => s.dismissWarning);
  // Primitive selector — `activeCount` is a number, so the default
  // `Object.is` comparison short-circuits when the count is unchanged.
  // Audit §10 minor: pre-fix the bare `s => s.agents` selector caused
  // the whole Sidebar to re-render on every agent-store update (status,
  // last_activity_at, etc.) even when the badge value didn't change.
  const activeCount = useAgentStore((s) => {
    let n = 0;
    for (const a of s.agents) {
      if (a.status === "running" || a.status === "starting") n += 1;
    }
    return n;
  });
  const [convertingAll, setConvertingAll] = useState(false);
  const [showDone, setShowDone] = useState<Record<string, boolean>>({});
  const [search, setSearch] = useState("");

  const sidebarOpen = useUiStore((s) => s.sidebarOpen);
  const closeSidebar = useUiStore((s) => s.closeSidebar);
  const location = useLocation();

  // Auto-close the slide-over on route change so a plan tap doesn't
  // leave the sidebar covering the page. Desktop ignores this state
  // (the sidebar is always visible on `≥md` regardless of the flag).
  useEffect(() => {
    closeSidebar();
  }, [location.pathname, closeSidebar]);

  const hasMdPlans = plans.some((p) => p.name && !p.name.endsWith(".yaml"));

  async function handleConvertAll() {
    setConvertingAll(true);
    try {
      await postJson("/api/plans/convert-all", {});
      await fetchPlans();
      if (selectedPlanName) await selectPlan(selectedPlanName);
    } catch (e) {
      toastError(e, "Convert failed");
    } finally {
      setConvertingAll(false);
    }
  }

  // Filter and group plans by project, split active vs done
  const grouped = useMemo(() => {
    const q = search.toLowerCase().trim();
    const filtered = q
      ? plans.filter(
          (p) =>
            p.title.toLowerCase().includes(q) ||
            p.name.toLowerCase().includes(q) ||
            (p.project ?? "").toLowerCase().includes(q),
        )
      : plans;

    const groups = new Map<string, { active: PlanSummary[]; done: PlanSummary[] }>();
    for (const p of filtered) {
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
  }, [plans, search]);

  function renderPlanItem(p: PlanSummary, dimmed = false) {
    const isSelected = selectedPlanName === p.name;
    return <PlanItem key={p.name} plan={p} isSelected={isSelected} dimmed={dimmed} />;
  }

  return (
    <>
      {/* Mobile backdrop — taps anywhere outside the slide-over close
          it. Hidden on `≥md` where the sidebar is persistent. */}
      {sidebarOpen && (
        <div
          data-testid="sidebar-backdrop"
          onClick={closeSidebar}
          className="md:hidden fixed inset-0 z-30 bg-black/50"
          aria-hidden="true"
        />
      )}
      <aside
        data-testid="sidebar"
        data-open={sidebarOpen}
        aria-hidden={!sidebarOpen ? true : undefined}
        className={`w-64 bg-gray-900 border-r border-gray-800 flex flex-col fixed inset-y-0 left-0 z-40 transform transition-transform duration-200 ease-out md:static md:translate-x-0 md:transition-none ${
          sidebarOpen ? "translate-x-0" : "-translate-x-full"
        }`}
      >
        {/* Logo — click to return to project dashboard */}
        <Link
          to="/"
          className="p-4 border-b border-gray-800 text-left hover:bg-gray-800/30 transition block"
          title="Back to project dashboard"
        >
          <h1 className="text-lg font-bold tracking-tight">
            Branch<span className="text-indigo-400">work</span>
          </h1>
          <p className="text-xs text-gray-500 mt-0.5">Claude Code Dashboard</p>
        </Link>

        {/* Nav */}
        <nav className="p-2 flex gap-1">
          <PlansNavLink />
          <NavLink
            to="/agents"
            className={({ isActive }) =>
              `flex-1 px-2 py-1.5 rounded text-xs font-medium transition relative text-center ${
                isActive
                  ? "bg-indigo-600 text-white"
                  : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
              }`
            }
          >
            Agents
            {activeCount > 0 && (
              <span className="absolute -top-1 -right-1 bg-emerald-500 text-white text-[10px] w-4 h-4 rounded-full flex items-center justify-center">
                {activeCount}
              </span>
            )}
          </NavLink>
          <NavLink
            to="/audit"
            className={({ isActive }) =>
              `flex-1 px-2 py-1.5 rounded text-xs font-medium transition text-center ${
                isActive
                  ? "bg-indigo-600 text-white"
                  : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
              }`
            }
          >
            Activity
          </NavLink>
          <NavLink
            to="/archive"
            title="Soft-deleted plans pending retention"
            className={({ isActive }) =>
              `flex-1 px-2 py-1.5 rounded text-xs font-medium transition text-center ${
                isActive
                  ? "bg-indigo-600 text-white"
                  : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
              }`
            }
          >
            Archive
          </NavLink>
        </nav>

        {/* Global actions */}
        <div className="px-2 pb-2">
          {hasMdPlans && (
            <button
              onClick={handleConvertAll}
              disabled={convertingAll}
              className="w-full px-3 py-1.5 text-xs bg-gray-800 border border-gray-700 hover:border-amber-600 hover:text-amber-400 disabled:opacity-50 text-gray-400 rounded transition"
            >
              {convertingAll ? "Converting..." : "Convert All to YAML"}
            </button>
          )}
          <NewPlanLink hasMdPlans={hasMdPlans} />
        </div>

        {/* Driver auth status */}
        <DriverStatusList />

        {/* Self-check footer indicator. Polls /api/health/state and
            surfaces wedged states (unknown agent statuses, stuck
            checking task_status rows, dismissed CI runs, time since
            last orphan sweep). When warnings > 0 the pill flips amber
            and clicking expands a popover that links each affected
            plan to its plan board so the user can run the Phase-1
            recovery flows directly. */}
        <HealthStateIndicator />

        {/* Admin link */}
        <div className="px-2 pb-2">
          <NavLink
            to="/admin"
            className={({ isActive }) =>
              `block w-full text-left px-2 py-1 text-[10px] rounded transition ${
                isActive
                  ? "bg-gray-800 text-indigo-400"
                  : "text-gray-600 hover:text-gray-300 hover:bg-gray-800/50"
              }`
            }
          >
            <span aria-hidden="true">⚙ </span>Admin
          </NavLink>
        </div>

        {/* Search */}
        <div className="px-2 pb-2">
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Search plans..."
            className="w-full px-2 py-1 text-xs bg-gray-800 border border-gray-700 rounded text-gray-300 placeholder-gray-600 outline-none focus:border-indigo-600 transition"
          />
        </div>

        {/* Plan list grouped by project */}
        <div className="flex-1 overflow-auto p-2">
          {/* Stale-data marker for the whole plan list. Self-gates: only
            appears when WS is disconnected AND lastPlansFetchedAt is
            >60s old. Sits at the top of the list because the plan rows
            below all share the same fetch lifecycle. */}
          <div className="px-2 mb-1.5 empty:hidden">
            <StaleDataChip slice="plans" />
          </div>
          {warnings.length > 0 && (
            <div className="mb-3 space-y-1">
              {warnings.map((w) => (
                <div
                  key={w.name}
                  className="bg-amber-900/30 border border-amber-700/50 rounded px-2 py-1.5 text-xs"
                >
                  <div className="flex items-start justify-between gap-1">
                    <span className="text-amber-400 font-medium truncate">{w.name}.yaml</span>
                    <button
                      onClick={() => dismissWarning(w.name)}
                      aria-label={`Dismiss ${w.name}.yaml warning`}
                      className="text-gray-600 hover:text-gray-400 flex-shrink-0"
                    >
                      <span aria-hidden="true">x</span>
                    </button>
                  </div>
                  <p className="text-amber-500/70 text-[10px] mt-0.5 line-clamp-2">{w.error}</p>
                </div>
              ))}
            </div>
          )}
          {grouped.map(([project, { active, done }]) => (
            <div key={project} className="mb-3">
              <h3 className="text-[10px] font-semibold text-gray-500 uppercase tracking-wider px-2 mb-1 flex items-center gap-1.5">
                <span
                  aria-hidden="true"
                  className={`w-1.5 h-1.5 rounded-full ${
                    project === "Unassigned" ? "bg-gray-600" : "bg-indigo-500"
                  }`}
                />
                {project}
                <span className="text-gray-600 font-normal">({active.length + done.length})</span>
              </h3>

              {/* Active plans */}
              <ul className="space-y-0.5">{active.map((p) => renderPlanItem(p))}</ul>

              {/* Completed plans — folded */}
              {done.length > 0 && (
                <div className="mt-1">
                  <button
                    onClick={() => setShowDone((prev) => ({ ...prev, [project]: !prev[project] }))}
                    className="w-full text-left px-2 py-1 text-[10px] text-gray-600 hover:text-gray-400 transition flex items-center gap-1"
                  >
                    <span aria-hidden="true" className="text-[8px]">
                      {showDone[project] ? "▼" : "▶"}
                    </span>
                    <span aria-hidden="true" className="text-emerald-700">
                      &#10003;
                    </span>
                    {done.length} completed plan{done.length !== 1 ? "s" : ""}
                  </button>
                  {showDone[project] && (
                    <ul className="space-y-0.5">{done.map((p) => renderPlanItem(p, true))}</ul>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>
      </aside>
    </>
  );
}

/// "Plans" highlights for both `/` and `/plans/...` (any plan board).
/// Per-row plan tile in the sidebar's grouped list. Wrapped in `memo` so
/// selecting a different plan (which changes `selectedPlanName` upstream
/// and therefore `isSelected` for exactly two rows) does NOT re-render
/// every other plan row. Audit §10 minor / acceptance: "Selecting a plan
/// does not re-render Sidebar plan rows."
interface PlanItemProps {
  plan: PlanSummary;
  isSelected: boolean;
  dimmed: boolean;
}

const PlanItem = memo(function PlanItem({ plan: p, isSelected, dimmed }: PlanItemProps) {
  const pct = p.taskCount > 0 ? Math.round((p.doneCount / p.taskCount) * 100) : 0;
  return (
    <li>
      <TouchTarget>
        <Link
          to={`/plans/${p.name}`}
          className={`block w-full text-left px-2 py-1.5 rounded text-sm transition ${
            isSelected
              ? "bg-gray-800 text-white"
              : dimmed
                ? "text-gray-600 hover:text-gray-400 hover:bg-gray-800/50"
                : "text-gray-400 hover:text-gray-200 hover:bg-gray-800/50"
          }`}
          title={p.title}
        >
          <div className="truncate flex items-center gap-1.5">
            {dimmed && (
              <span aria-hidden="true" className="text-emerald-600 text-[10px]">
                &#10003;
              </span>
            )}
            <span className="truncate">{p.title}</span>
          </div>
          <div className="text-[9px] font-mono text-gray-700 truncate">{p.name}</div>
          <div className="text-[10px] text-gray-600 flex items-center gap-1">
            {p.taskCount > 0 && (
              <>
                <span>
                  {p.doneCount}/{p.taskCount}
                </span>
                <span className="text-gray-700">({pct}%)</span>
              </>
            )}
            {p.taskCount === 0 && <span>{p.phaseCount} phases</span>}
            <span className="text-gray-700 ml-auto">{formatRelative(p.modifiedAt)}</span>
          </div>
        </Link>
      </TouchTarget>
    </li>
  );
});

/// NavLink's default `end` matches only on exact equality, which would
/// drop the highlight when a plan is open — explicit matcher keeps it
/// lit across the whole plans subtree.
function PlansNavLink() {
  const location = useLocation();
  const isActive = location.pathname === "/" || location.pathname.startsWith("/plans");
  return (
    <Link
      to="/"
      className={`flex-1 px-2 py-1.5 rounded text-xs font-medium transition text-center ${
        isActive
          ? "bg-indigo-600 text-white"
          : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
      }`}
    >
      Plans
    </Link>
  );
}

function NewPlanLink({ hasMdPlans }: { hasMdPlans: boolean }) {
  return (
    <NavLink
      to="/new-plan"
      className={({ isActive }) =>
        `block w-full text-center px-3 py-1.5 text-xs border rounded transition ${hasMdPlans ? "mt-1" : ""} ${
          isActive
            ? "bg-indigo-600 border-indigo-600 text-white"
            : "bg-gray-800 border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-400"
        }`
      }
    >
      + New Plan
    </NavLink>
  );
}

/// Compact driver auth status: one row per installed driver, showing whether
/// it's ready to spawn agents. Rendered under the effort selector so users
/// see auth problems without digging into a settings page.
///
/// SaaS mode: scoped to the runner the user has currently selected
/// (`useRunnerStore.selectedRunnerId`). Switching runners refetches via
/// `/api/drivers?runner_id=…` so the list reflects the runner's local
/// install state, not the dashboard's. Standalone mode: no runner is
/// selected, so we fetch the server's local registry — same surface the
/// historical `/api/drivers` returned.
function DriverStatusList() {
  const drivers = useSettingsStore((s) => s.drivers);
  const driversRunnerStatus = useSettingsStore((s) => s.driversRunnerStatus);
  const fetchDrivers = useSettingsStore((s) => s.fetchDrivers);
  const selectedRunnerId = useRunnerStore((s) => s.selectedRunnerId);
  const runners = useRunnerStore((s) => s.runners);
  const mode = useRunnerStore((s) => s.mode);
  const [expanded, setExpanded] = useState<string | null>(null);

  // Refetch whenever the user switches runners — the inventory of
  // `claude` / `aider` / … on runner A says nothing about runner B.
  // In standalone mode (`mode==="standalone"`), pass `null` so the
  // server returns its local registry; in SaaS mode pass the selected
  // runner id (or null until a runner is selected, falling back to
  // the local registry shape so the sidebar shows driver names + caps).
  useEffect(() => {
    const target = mode === "saas" ? selectedRunnerId : null;
    fetchDrivers(target).catch(() => {
      // Silently ignore — if /api/drivers is down we fall back to assuming ready.
    });
  }, [fetchDrivers, mode, selectedRunnerId]);

  if (drivers.length === 0) {
    if (mode === "saas" && driversRunnerStatus === "never_reported") {
      return (
        <div className="px-2 pb-2 text-[10px] text-gray-500 italic">
          Waiting for runner to report drivers…
        </div>
      );
    }
    return null;
  }

  const runnerLabel =
    mode === "saas" && selectedRunnerId
      ? (runners.find((r) => r.id === selectedRunnerId)?.name ?? selectedRunnerId.slice(0, 8))
      : null;

  return (
    <div className="px-2 pb-2">
      <div className="flex items-baseline justify-between mb-0.5 px-1">
        <div className="text-[9px] uppercase tracking-wider text-gray-600">Drivers</div>
        {runnerLabel && (
          <div
            className="text-[9px] text-gray-600 truncate max-w-[60%]"
            title={`Drivers reflect runner ${runnerLabel}'s install state`}
          >
            on {runnerLabel}
          </div>
        )}
      </div>
      {driversRunnerStatus === "offline" && (
        <div className="px-1 mb-1 text-[9px] text-amber-400/80">
          Runner offline · last-known state
        </div>
      )}
      {drivers.map((d) => {
        const auth = d.auth_status;
        // Map auth kind → label + color. `unknown` is intentionally rendered
        // as green because we assume ready when we can't introspect.
        const kind = auth?.kind ?? "unknown";
        const tone =
          kind === "not_installed" || kind === "unauthenticated"
            ? "text-red-400"
            : kind === "api_key" || kind === "oauth" || kind === "cloud_provider"
              ? "text-emerald-400"
              : "text-gray-500";
        const label =
          kind === "not_installed"
            ? "not installed"
            : kind === "unauthenticated"
              ? "needs auth"
              : kind === "oauth"
                ? auth && "account" in auth && auth.account
                  ? auth.account.toLowerCase()
                  : "signed in"
                : kind === "api_key"
                  ? "API key"
                  : kind === "cloud_provider"
                    ? auth && "provider" in auth
                      ? auth.provider
                      : "cloud"
                    : "unknown";
        const canExpand = auth?.kind === "unauthenticated" || auth?.kind === "not_installed";
        const isExpanded = expanded === d.name;
        const help =
          auth?.kind === "unauthenticated"
            ? auth.help
            : auth?.kind === "not_installed"
              ? `Install the \`${d.binary}\` CLI and make sure it's on your PATH.`
              : "";
        return (
          <div key={d.name} className="text-[10px]">
            <button
              onClick={() => canExpand && setExpanded(isExpanded ? null : d.name)}
              disabled={!canExpand}
              className={`w-full flex items-center justify-between px-1 py-0.5 rounded transition ${
                canExpand ? "hover:bg-gray-800 cursor-pointer" : "cursor-default"
              }`}
              title={help || undefined}
            >
              <span className="text-gray-400">{d.name}</span>
              <span className={`font-mono ${tone}`}>{label}</span>
            </button>
            {isExpanded && help && (
              <div className="px-1 py-1 mt-0.5 bg-amber-950/30 border border-amber-800/30 rounded text-[10px] text-amber-300/80 whitespace-pre-wrap">
                {help}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
