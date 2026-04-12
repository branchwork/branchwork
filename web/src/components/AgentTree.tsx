import { useEffect } from "react";
import { useAgentStore, type Agent } from "../stores/agent-store.js";

export function AgentTree() {
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  useEffect(() => {
    fetchAgents();
  }, []);

  // Build tree: group by parent
  const roots = agents.filter((a) => !a.parent_agent_id);
  const childrenOf = (parentId: string) =>
    agents.filter((a) => a.parent_agent_id === parentId);

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
        <button
          onClick={() => selectAgent(agent.id)}
          className={`w-full text-left p-3 rounded-md border transition mb-1 ${
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
            <span className="text-[10px] text-gray-500 ml-auto flex-shrink-0">
              {agent.status}
            </span>
          </div>

          <div className="mt-1 text-[11px] text-gray-500 space-y-0.5">
            {agent.plan_name && (
              <div>Plan: {agent.plan_name}</div>
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
        </button>

        {children.map((child) => renderAgent(child, depth + 1))}
      </div>
    );
  }

  return (
    <div className="p-6">
      <h2 className="text-xl font-bold mb-4">
        Agents
        <span className="text-sm font-normal text-gray-500 ml-2">
          {agents.filter((a) => a.status === "running" || a.status === "starting").length} active
          / {agents.length} total
        </span>
      </h2>

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
