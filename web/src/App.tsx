import { useEffect, useState } from "react";
import { usePlanStore } from "./stores/plan-store.js";
import { useAgentStore } from "./stores/agent-store.js";
import { useWsStore } from "./stores/ws-store.js";
import { useSettingsStore } from "./stores/settings-store.js";
import { useAuthStore } from "./stores/auth-store.js";
import { Sidebar } from "./components/Sidebar.js";
import { PlanBoard } from "./components/PlanBoard.js";
import { ProjectDashboard } from "./components/ProjectDashboard.js";
import { AgentTree } from "./components/AgentTree.js";
import { AgentPanel } from "./components/AgentPanel.js";
import { NewPlanForm } from "./components/NewPlanForm.js";
import { AuditLog } from "./components/AuditLog.js";
import { LoginPage } from "./components/LoginPage.js";
import { AdminPage } from "./components/AdminPage.js";

type View = "plans" | "agents" | "new-plan" | "audit" | "admin";

export function App() {
  const [view, setView] = useState<View>("plans");
  const connected = useWsStore((s) => s.connected);
  const connect = useWsStore((s) => s.connect);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const selectedPlan = usePlanStore((s) => s.selectedPlan);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  const fetchSettings = useSettingsStore((s) => s.fetchSettings);
  const fetchDrivers = useSettingsStore((s) => s.fetchDrivers);

  const user = useAuthStore((s) => s.user);
  const authLoading = useAuthStore((s) => s.loading);
  const fetchMe = useAuthStore((s) => s.fetchMe);
  const logout = useAuthStore((s) => s.logout);

  // Resolve auth first. Other stores/WS are gated below so unauthenticated
  // requests don't spam 401s into the dashboard.
  useEffect(() => {
    fetchMe();
  }, [fetchMe]);

  useEffect(() => {
    if (!user) return;
    connect();
    fetchPlans().catch(() => {});
    fetchAgents().catch(() => {});
    fetchSettings().catch(() => {});
    fetchDrivers().catch(() => {});
  }, [user]);

  // Refetch when the tab becomes visible again — covers events missed
  // while the browser throttled or suspended the WebSocket.
  useEffect(() => {
    if (!user) return;
    const onVisible = () => {
      if (document.visibilityState === "visible") {
        fetchPlans().catch(() => {});
        fetchAgents().catch(() => {});
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  }, [user, fetchPlans, fetchAgents]);

  if (authLoading) {
    return (
      <div className="flex h-screen items-center justify-center bg-gray-950 text-gray-500 text-sm">
        …
      </div>
    );
  }

  if (!user) {
    return <LoginPage />;
  }

  return (
    <div className="flex h-screen bg-gray-950 text-gray-100">
      <Sidebar
        view={view}
        onViewChange={setView}
      />

      <main className="flex-1 flex overflow-hidden">
        <div className="flex-1 overflow-auto">
          {view === "plans" && (selectedPlan ? <PlanBoard /> : <ProjectDashboard />)}
          {view === "agents" && <AgentTree />}
          {view === "audit" && <AuditLog />}
          {view === "admin" && <AdminPage />}
          {view === "new-plan" && (
            <NewPlanForm onClose={() => setView("plans")} />
          )}
        </div>

        {selectedAgentId && (
          <div className="w-[600px] border-l border-gray-800 h-full">
            <AgentPanel />
          </div>
        )}
      </main>

      {/* Connection indicator + logout */}
      <div className="fixed bottom-3 right-3 flex items-center gap-3 text-xs text-gray-500">
        <span className="flex items-center gap-2">
          <span
            className={`inline-block w-2 h-2 rounded-full ${
              connected ? "bg-emerald-500" : "bg-red-500"
            }`}
          />
          {connected ? "Connected" : "Disconnected"}
        </span>
        <span className="text-gray-600">·</span>
        <span className="text-gray-500">{user.email}</span>
        <button
          onClick={() => logout()}
          className="text-gray-600 hover:text-gray-300 transition"
        >
          Sign out
        </button>
      </div>
    </div>
  );
}
