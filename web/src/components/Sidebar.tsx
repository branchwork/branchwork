import { useEffect, useMemo, useState } from "react";
import { usePlanStore, type PlanSummary } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore, type EffortLevel } from "../stores/settings-store.js";
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
  view: "plans" | "agents" | "new-plan" | "audit" | "admin";
  onViewChange: (v: "plans" | "agents" | "new-plan" | "audit" | "admin") => void;
}

export function Sidebar({ view, onViewChange }: Props) {
  const plans = usePlanStore((s) => s.plans);
  const selectedPlan = usePlanStore((s) => s.selectedPlan);
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const clearSelectedPlan = usePlanStore((s) => s.clearSelectedPlan);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const warnings = usePlanStore((s) => s.warnings);
  const dismissWarning = usePlanStore((s) => s.dismissWarning);
  const agents = useAgentStore((s) => s.agents);
  const activeCount = agents.filter(
    (a) => a.status === "running" || a.status === "starting"
  ).length;
  const [convertingAll, setConvertingAll] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [showDone, setShowDone] = useState<Record<string, boolean>>({});
  const [search, setSearch] = useState("");

  const hasMdPlans = plans.some((p) => p.name && !p.name.endsWith(".yaml"));

  async function handleConvertAll() {
    setConvertingAll(true);
    setActionError(null);
    try {
      await postJson("/api/plans/convert-all", {});
      await fetchPlans();
      if (selectedPlan) await selectPlan(selectedPlan.name);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setActionError(`Convert failed: ${msg}`);
      console.error("Convert all failed:", e);
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
            (p.project ?? "").toLowerCase().includes(q)
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
          <div className="text-[9px] font-mono text-gray-700 truncate">{p.name}</div>
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
      {/* Logo — click to return to project dashboard */}
      <button
        onClick={() => {
          clearSelectedPlan();
          onViewChange("plans");
        }}
        className="p-4 border-b border-gray-800 text-left hover:bg-gray-800/30 transition"
        title="Back to project dashboard"
      >
        <h1 className="text-lg font-bold tracking-tight">
          Branch<span className="text-indigo-400">work</span>
        </h1>
        <p className="text-xs text-gray-500 mt-0.5">Claude Code Dashboard</p>
      </button>

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
        <button
          onClick={() => onViewChange("audit")}
          className={`flex-1 px-3 py-1.5 rounded text-sm font-medium transition ${
            view === "audit"
              ? "bg-indigo-600 text-white"
              : "text-gray-400 hover:text-gray-200 hover:bg-gray-800"
          }`}
        >
          Activity
        </button>
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
        <button
          onClick={() => onViewChange("new-plan")}
          className={`w-full px-3 py-1.5 text-xs border rounded transition ${hasMdPlans ? "mt-1" : ""} ${
            view === "new-plan"
              ? "bg-indigo-600 border-indigo-600 text-white"
              : "bg-gray-800 border-gray-700 hover:border-indigo-600 hover:text-indigo-400 text-gray-400"
          }`}
        >
          + New Plan
        </button>
        {actionError && (
          <div className="mt-1 text-[10px] text-red-400 bg-red-900/20 border border-red-800/30 rounded px-2 py-1 flex items-start justify-between gap-1">
            <span>{actionError}</span>
            <button onClick={() => setActionError(null)} className="text-red-600 hover:text-red-400 flex-shrink-0">x</button>
          </div>
        )}
      </div>

      {/* Effort level */}
      <EffortSelector />

      {/* Driver auth status */}
      <DriverStatusList />

      {/* Admin link */}
      <div className="px-2 pb-2">
        <button
          onClick={() => onViewChange("admin")}
          className={`w-full text-left px-2 py-1 text-[10px] rounded transition ${
            view === "admin"
              ? "bg-gray-800 text-indigo-400"
              : "text-gray-600 hover:text-gray-300 hover:bg-gray-800/50"
          }`}
        >
          ⚙ Admin
        </button>
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
        {warnings.length > 0 && (
          <div className="mb-3 space-y-1">
            {warnings.map((w) => (
              <div
                key={w.name}
                className="bg-amber-900/30 border border-amber-700/50 rounded px-2 py-1.5 text-xs"
              >
                <div className="flex items-start justify-between gap-1">
                  <span className="text-amber-400 font-medium truncate">
                    {w.name}.yaml
                  </span>
                  <button
                    onClick={() => dismissWarning(w.name)}
                    className="text-gray-600 hover:text-gray-400 flex-shrink-0"
                  >
                    x
                  </button>
                </div>
                <p className="text-amber-500/70 text-[10px] mt-0.5 line-clamp-2">
                  {w.error}
                </p>
              </div>
            ))}
          </div>
        )}
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

const EFFORT_LEVELS: { value: EffortLevel; label: string; color: string }[] = [
  { value: "low", label: "Low", color: "text-gray-400" },
  { value: "medium", label: "Med", color: "text-blue-400" },
  { value: "high", label: "High", color: "text-amber-400" },
  { value: "max", label: "Max", color: "text-red-400" },
];

function EffortSelector() {
  const effort = useSettingsStore((s) => s.effort);
  const setEffort = useSettingsStore((s) => s.setEffort);
  const skipPermissions = useSettingsStore((s) => s.skipPermissions);
  const setSkipPermissions = useSettingsStore((s) => s.setSkipPermissions);

  return (
    <>
      <div className="px-2 pb-2 flex items-center gap-1">
        <span className="text-[10px] text-gray-600 mr-1">Effort</span>
        {EFFORT_LEVELS.map((l) => (
          <button
            key={l.value}
            onClick={() => setEffort(l.value)}
            className={`px-1.5 py-0.5 text-[10px] rounded transition ${
              effort === l.value
                ? `${l.color} bg-gray-800 font-semibold`
                : "text-gray-600 hover:text-gray-400"
            }`}
          >
            {l.label}
          </button>
        ))}
      </div>
      <label
        className="px-2 pb-2 flex items-center gap-1.5 text-[10px] text-gray-500 hover:text-gray-300 cursor-pointer select-none"
        title={'Spawn agents with --dangerously-skip-permissions. Requires "skipDangerousModePermissionPrompt": true in ~/.claude/settings.json (see README).'}
      >
        <input
          type="checkbox"
          checked={skipPermissions}
          onChange={(e) => setSkipPermissions(e.target.checked)}
          className="accent-amber-500"
        />
        <span className={skipPermissions ? "text-amber-400" : ""}>
          Skip permissions
        </span>
      </label>
    </>
  );
}

/// Compact driver auth status: one row per installed driver, showing whether
/// it's ready to spawn agents. Rendered under the effort selector so users
/// see auth problems without digging into a settings page.
function DriverStatusList() {
  const drivers = useSettingsStore((s) => s.drivers);
  const fetchDrivers = useSettingsStore((s) => s.fetchDrivers);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    fetchDrivers().catch(() => {
      // Silently ignore — if /api/drivers is down we fall back to assuming ready.
    });
  }, [fetchDrivers]);

  if (drivers.length === 0) return null;

  return (
    <div className="px-2 pb-2">
      <div className="text-[9px] uppercase tracking-wider text-gray-600 mb-0.5 px-1">
        Drivers
      </div>
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
        const canExpand =
          auth?.kind === "unauthenticated" || auth?.kind === "not_installed";
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
