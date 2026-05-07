import { useEffect, type ReactNode } from "react";
import { useAgentStore } from "../stores/agent-store.js";
import { useRouteSelection } from "../hooks/use-route-selection.js";

/// Right-rail data-dependency wrapper. Mounted by `AgentRail` whenever
/// the URL carries a selected agent (deep-link `/agents/:agentId` or
/// `?agent=<id>` overlay). Calls `fetchAgents()` if the agent list has
/// not been fetched yet, shows a loading state until done, and renders
/// an "Agent not found" panel when the URL id is absent from the list.
///
/// Membership is the load-bearing check: `<AgentPanel/>` looks up the
/// agent in the store, and a stale `selectedAgentId` would otherwise
/// produce an empty rail with no feedback. The wrapper renders inside
/// the rail container, so the not-found copy is the same `w-[600px]`
/// width as a normal panel — keeps layout from shifting under the user.
export function EnsureAgents({ children }: { children: ReactNode }) {
  const { routeAgentId } = useRouteSelection();
  const agents = useAgentStore((s) => s.agents);
  const agentsFetched = useAgentStore((s) => s.agentsFetched);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);

  useEffect(() => {
    if (!agentsFetched) {
      fetchAgents().catch(() => {});
    }
  }, [agentsFetched, fetchAgents]);

  if (!agentsFetched) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-gray-500">
        Loading...
      </div>
    );
  }

  const exists = !!routeAgentId && agents.some((a) => a.id === routeAgentId);
  if (!exists) {
    return <AgentNotFound agentId={routeAgentId ?? ""} />;
  }

  return <>{children}</>;
}

function AgentNotFound({ agentId }: { agentId: string }) {
  return (
    <div role="alert" className="flex h-full flex-col p-4">
      <div className="rounded border border-red-700/50 bg-red-900/20 p-4">
        <h2 className="text-sm font-semibold text-red-100">Agent not found</h2>
        <p className="mt-1 text-xs text-red-200/80">
          No agent matches{" "}
          <span className="font-mono text-red-100">{agentId ? agentId.slice(0, 8) : "—"}</span>. It
          may have already exited and been cleaned up.
        </p>
      </div>
    </div>
  );
}
