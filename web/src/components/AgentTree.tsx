import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRouteSelection } from "../hooks/use-route-selection.js";
import { StaleDataChip } from "./StaleDataChip.js";

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
  }, []);

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

  // Build tree: group by parent
  const filteredIds = new Set(filtered.map((a) => a.id));
  const roots = filtered.filter((a) => !a.parent_agent_id || !filteredIds.has(a.parent_agent_id));
  const childrenOf = (parentId: string) =>
    filtered.filter((a) => a.parent_agent_id === parentId);

  const statusDot: Record<string, string> = {
    starting: "bg-yellow-500",
    running: "bg-emerald-500 animate-pulse",
    completed: "bg-gray-500",
    failed: "bg-red-500",
    killed: "bg-red-400",
  };

  function renderAgent(agent: Agent, depth = 0) {
    const children = childrenOf(agent.id);
    const isSelected = agent.id === selectedAgentId;

    return (
      <div key={agent.id} style={{ paddingLeft: depth * 20 }}>
        <Link
          to={`/agents/${agent.id}`}
          className={`block w-full text-left p-3 rounded-md border transition mb-1 ${
            isSelected
              ? "bg-gray-800 border-indigo-600"
              : "bg-gray-900 border-gray-800 hover:border-gray-700"
          }`}
        >
          <div className="flex items-center gap-2">
            <span
              className={`w-2 h-2 rounded-full flex-shrink-0 ${
                statusDot[agent.status] ?? "bg-gray-600"
              }`}
            />
            <span className="text-sm font-medium truncate">
              {agent.plan_name
                ? `Task ${agent.task_id}`
                : `Agent ${agent.id.slice(0, 8)}`}
            </span>
            {driverCapabilities(agent.driver).supports_cost && agent.cost_usd != null && (
              <span className="text-[10px] text-amber-500/80 font-mono flex-shrink-0">
                ${agent.cost_usd.toFixed(4)}
              </span>
            )}
            <span className="text-[10px] text-gray-500 ml-auto flex-shrink-0">
              {agent.status}
            </span>
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

        {children.map((child) => renderAgent(child, depth + 1))}
      </div>
    );
  }

  return (
    <div className="p-6">
      <h2 className="text-xl font-bold mb-2 flex items-center gap-2 flex-wrap">
        <span>Agents</span>
        <span className="text-sm font-normal text-gray-500">
          {agents.filter((a) => a.status === "running" || a.status === "starting").length} active
          / {agents.length} total
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
          <div className="text-4xl mb-3 text-gray-700">&#9881;</div>
          <p className="text-gray-500">No agents yet</p>
          <p className="text-xs text-gray-600 mt-1">
            Start a task from the Plan Board or wait for hook events
          </p>
        </div>
      ) : (
        <div className="space-y-1">{roots.map((a) => renderAgent(a))}</div>
      )}
    </div>
  );
}
