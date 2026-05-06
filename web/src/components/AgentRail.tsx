import { useRouteSelection } from "../hooks/use-route-selection.js";
import { EnsureAgents } from "./EnsureAgents.js";
import { AgentPanel } from "./AgentPanel.js";

/// Right-rail AgentPanel host. Mounts only when an agent id sits in
/// the URL — either the deep-link path `/agents/:agentId` or the
/// cross-route `?agent=<id>` overlay layered on PlanBoard / etc. The
/// `<EnsureAgents/>` wrapper handles the data dependency: it ensures
/// the agent list has been fetched before rendering the panel and
/// surfaces a not-found message in the rail when the URL id is
/// stale.
///
/// Responsive shape: on `<lg` viewports (Tailwind 1024px) the rail
/// switches to a full-screen modal drawer (`fixed inset-0 z-50
/// bg-gray-950`). On `≥lg` the historical 600px right rail returns
/// (`lg:static lg:w-[600px] lg:border-l`). The breakpoint flip drives
/// xterm's FitAddon to re-measure for free: `PtyTerminal` keeps a
/// ResizeObserver on its terminal node, and a wrapper that goes from
/// viewport-sized to 600px (or vice versa) changes that node's
/// contentRect — the observer fires and refits.
///
/// Closing via AgentPanel's own X button drops the URL selection
/// (`goToAgent(null)` in use-route-selection.ts), which strips the
/// `?agent=<id>` overlay or routes back to /agents from the deep-link
/// form — so the user lands on the view they were on before opening
/// the agent.
export function AgentRail() {
  const { routeAgentId } = useRouteSelection();
  if (!routeAgentId) return null;
  return (
    <div
      data-testid="agent-rail"
      className="fixed inset-0 z-50 bg-gray-950 lg:static lg:inset-auto lg:z-auto lg:bg-transparent lg:w-[600px] lg:border-l lg:border-gray-800 lg:h-full"
    >
      <EnsureAgents>
        <AgentPanel />
      </EnsureAgents>
    </div>
  );
}
