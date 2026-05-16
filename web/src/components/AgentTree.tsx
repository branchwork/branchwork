import { useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRouteSelection } from "../hooks/use-route-selection.js";
import { useScrollParent } from "../hooks/use-scroll-parent.js";
import { StaleDataChip } from "./StaleDataChip.js";

// See PhaseCard.tsx for the same trade-off rationale: below this count
// we render the full list (cheaper than a virtualiser for short lists,
// avoids jsdom layout edge cases); at or above we switch to row
// virtualisation. The brief's acceptance bar (200+ historical agents)
// is comfortably above this.
const VIRTUALIZATION_THRESHOLD = 30;

export function AgentTree() {
  const agents = useAgentStore((s) => s.agents);
  const plans = usePlanStore((s) => s.plans);
  const driverCapabilities = useSettingsStore((s) => s.driverCapabilities);
  const planTitles = new Map(plans.map((p) => [p.name, p.title]));
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const { routeAgentId } = useRouteSelection();
  const selectedAgentId = routeAgentId;
  const [statusFilter, setStatusFilter] = useState<string | null>(null);
  const [planFilter, setPlanFilter] = useState<string | null>(null);

  useEffect(() => {
    fetchAgents();
  }, [fetchAgents]);

  // Filter agents
  const filtered = useMemo(() => {
    return agents.filter((a) => {
      if (statusFilter && a.status !== statusFilter) return false;
      if (planFilter && a.plan_name !== planFilter) return false;
      return true;
    });
  }, [agents, statusFilter, planFilter]);

  // Unique plan names for filter dropdown
  const agentPlanNames = useMemo(() => {
    const names = new Set<string>();
    for (const a of agents) {
      if (a.plan_name) names.add(a.plan_name);
    }
    return [...names].sort();
  }, [agents]);

  // Build tree: group by parent, then flatten to a depth-tagged list in
  // DFS order so the virtualiser can treat it as a 1D row stream while
  // we keep the visual tree shape via per-row paddingLeft.
  const flatRows = useMemo(() => {
    const filteredIds = new Set(filtered.map((a) => a.id));
    const roots = filtered.filter((a) => !a.parent_agent_id || !filteredIds.has(a.parent_agent_id));
    const byParent = new Map<string, Agent[]>();
    for (const a of filtered) {
      if (!a.parent_agent_id) continue;
      const arr = byParent.get(a.parent_agent_id) ?? [];
      arr.push(a);
      byParent.set(a.parent_agent_id, arr);
    }
    const out: { agent: Agent; depth: number }[] = [];
    function walk(a: Agent, depth: number) {
      out.push({ agent: a, depth });
      const kids = byParent.get(a.id);
      if (kids) for (const c of kids) walk(c, depth + 1);
    }
    for (const r of roots) walk(r, 0);
    return out;
  }, [filtered]);

  const statusDot: Record<string, string> = {
    starting: "bg-yellow-500",
    running: "bg-emerald-500 animate-pulse",
    completed: "bg-gray-500",
    failed: "bg-red-500",
    killed: "bg-red-400",
  };

  // Virtualisation: audit §10 major. Accounts with 200+ historical
  // agents used to render every row up-front; this bounds the DOM to
  // the visible viewport plus a small overscan.
  const listRef = useRef<HTMLDivElement>(null);
  const scrollEl = useScrollParent(listRef);
  const virtualizer = useVirtualizer({
    count: flatRows.length,
    getScrollElement: () => scrollEl,
    estimateSize: () => 84,
    overscan: 5,
    gap: 4,
    initialRect: { width: 1280, height: 800 },
  });

  function renderAgentRow(agent: Agent, depth: number) {
    const isSelected = agent.id === selectedAgentId;

    return (
      <div style={{ paddingLeft: depth * 20 }}>
        <Link
          to={`/agents/${agent.id}`}
          className={`block w-full text-left p-3 rounded-md border transition ${
            isSelected
              ? "bg-gray-800 border-indigo-600"
              : "bg-gray-900 border-gray-800 hover:border-gray-700"
          }`}
        >
          <div className="flex items-center gap-2">
            <span
              aria-hidden="true"
              className={`w-2 h-2 rounded-full flex-shrink-0 ${
                statusDot[agent.status] ?? "bg-gray-600"
              }`}
            />
            <span className="sr-only">Status: {agent.status}</span>
            <span className="text-sm font-medium truncate">
              {agent.task_id
                ? `Task ${agent.task_id}`
                : agent.plan_name
                  ? "Plan session"
                  : `Agent ${agent.id.slice(0, 8)}`}
            </span>
            {driverCapabilities(agent.driver).supports_cost && agent.cost_usd != null && (
              <span className="text-[10px] text-amber-500/80 font-mono flex-shrink-0">
                ${agent.cost_usd.toFixed(4)}
              </span>
            )}
            <span className="text-[10px] text-gray-500 ml-auto flex-shrink-0">{agent.status}</span>
          </div>

          <div className="mt-1 text-[11px] text-gray-500 space-y-0.5">
            {agent.plan_name && (
              <div>
                {planTitles.get(agent.plan_name) ?? agent.plan_name}
                <span className="text-gray-600 font-mono ml-1">{agent.plan_name}</span>
              </div>
            )}
            {agent.last_tool && (
              <div>
                Last tool: <span className="text-gray-400">{agent.last_tool}</span>
              </div>
            )}
            <div className="font-mono truncate" title={agent.cwd}>
              {agent.cwd}
            </div>
          </div>
        </Link>
      </div>
    );
  }

  return (
    <div className="p-6">
      <h2 className="text-xl font-bold mb-2 flex items-center gap-2 flex-wrap">
        <span>Agents</span>
        <span className="text-sm font-normal text-gray-500">
          {agents.filter((a) => a.status === "running" || a.status === "starting").length} active /{" "}
          {agents.length} total
        </span>
        {/* Stale marker for the agent rows below. Self-gates on
            WS-disconnect + 60s of cache age. */}
        <StaleDataChip slice="agents" />
      </h2>

      {/* Filters */}
      {agents.length > 0 && (
        <div className="flex items-center gap-3 mb-4 flex-wrap">
          <div className="flex items-center gap-1">
            <span className="text-[10px] text-gray-600 mr-1">Status</span>
            {[
              { value: null, label: "All" },
              { value: "running", label: "Running", color: "text-emerald-400" },
              { value: "completed", label: "Done", color: "text-gray-400" },
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
          {agentPlanNames.length > 1 && (
            <div className="flex items-center gap-1">
              <span className="text-[10px] text-gray-600 mr-1">Plan</span>
              <select
                value={planFilter ?? ""}
                onChange={(e) => setPlanFilter(e.target.value || null)}
                className="text-[10px] bg-gray-800 border border-gray-700 rounded px-1.5 py-0.5 text-gray-400 outline-none"
              >
                <option value="">All plans</option>
                {agentPlanNames.map((n) => (
                  <option key={n} value={n}>
                    {planTitles.get(n) ?? n}
                  </option>
                ))}
              </select>
            </div>
          )}
        </div>
      )}

      {agents.length === 0 ? (
        <div className="text-center py-12">
          <div aria-hidden="true" className="text-4xl mb-3 text-gray-700">
            &#9881;
          </div>
          <p className="text-gray-500">No agents yet</p>
          <p className="text-xs text-gray-600 mt-1">
            Start a task from the Plan Board or wait for hook events
          </p>
        </div>
      ) : flatRows.length >= VIRTUALIZATION_THRESHOLD ? (
        <div ref={listRef}>
          <div
            style={{
              height: `${virtualizer.getTotalSize()}px`,
              position: "relative",
              width: "100%",
            }}
          >
            {virtualizer.getVirtualItems().map((vRow) => {
              const row = flatRows[vRow.index];
              return (
                <div
                  key={vRow.key}
                  data-index={vRow.index}
                  ref={virtualizer.measureElement}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    transform: `translateY(${vRow.start}px)`,
                  }}
                >
                  {renderAgentRow(row.agent, row.depth)}
                </div>
              );
            })}
          </div>
        </div>
      ) : (
        <div className="space-y-1">
          {flatRows.map(({ agent, depth }) => (
            <div key={agent.id}>{renderAgentRow(agent, depth)}</div>
          ))}
        </div>
      )}
    </div>
  );
}
