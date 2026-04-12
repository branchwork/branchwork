import { useEffect, useState } from "react";
import { usePlanStore } from "./stores/plan-store.js";
import { useAgentStore } from "./stores/agent-store.js";
import { useWsStore } from "./stores/ws-store.js";
import { Sidebar } from "./components/Sidebar.js";
import { PlanBoard } from "./components/PlanBoard.js";
import { AgentTree } from "./components/AgentTree.js";
import { AgentPanel } from "./components/AgentPanel.js";

type View = "plans" | "agents";

export function App() {
  const [view, setView] = useState<View>("plans");
  const connected = useWsStore((s) => s.connected);
  const connect = useWsStore((s) => s.connect);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  useEffect(() => {
    connect();
    fetchPlans();
    fetchAgents();
  }, []);

  return (
    <div className="flex h-screen bg-gray-950 text-gray-100">
      <Sidebar view={view} onViewChange={setView} />

      <main className="flex-1 flex overflow-hidden">
        <div className="flex-1 overflow-auto">
          {view === "plans" && <PlanBoard />}
          {view === "agents" && <AgentTree />}
        </div>

        {selectedAgentId && (
          <div className="w-[480px] border-l border-gray-800">
            <AgentPanel />
          </div>
        )}
      </main>

      {/* Connection indicator */}
      <div className="fixed bottom-3 right-3 flex items-center gap-2 text-xs text-gray-500">
        <span
          className={`inline-block w-2 h-2 rounded-full ${
            connected ? "bg-emerald-500" : "bg-red-500"
          }`}
        />
        {connected ? "Connected" : "Disconnected"}
      </div>
    </div>
  );
}
