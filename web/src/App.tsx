import { useEffect } from "react";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";
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
import { ArchivePanel } from "./components/ArchivePanel.js";
import { LoginPage } from "./components/LoginPage.js";
import { AdminPage } from "./components/AdminPage.js";
import { RunnersPage } from "./components/RunnersPage.js";
import { Toaster } from "./components/Toaster.js";
import { EnsurePlan } from "./components/EnsurePlan.js";
import { EnsureAgents } from "./components/EnsureAgents.js";
import { NotFoundPage } from "./components/NotFoundPage.js";
import { useRouteSelection } from "./hooks/use-route-selection.js";

export function App() {
  const connected = useWsStore((s) => s.connected);
  const connect = useWsStore((s) => s.connect);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);

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
      <RouteSync />
      <Sidebar />

      <main className="flex-1 flex overflow-hidden">
        <div className="flex-1 overflow-auto">
          <Routes>
            <Route path="/" element={<ProjectDashboard />} />
            <Route path="/plans" element={<ProjectDashboard />} />
            <Route
              path="/plans/:planName"
              element={
                <EnsurePlan>
                  <PlanBoard />
                </EnsurePlan>
              }
            />
            <Route path="/agents" element={<AgentTree />} />
            <Route path="/agents/:agentId" element={<AgentTree />} />
            <Route path="/audit" element={<AuditLog />} />
            <Route path="/archive" element={<ArchivePanel />} />
            <Route path="/admin" element={<AdminPage />} />
            <Route path="/admin/:section" element={<AdminPage />} />
            <Route path="/runners" element={<RunnersPage />} />
            <Route path="/new-plan" element={<NewPlanRoute />} />
            <Route path="/login" element={<Navigate to="/" replace />} />
            <Route path="*" element={<NotFoundPage />} />
          </Routes>
        </div>

        <AgentRail />
      </main>

      <Toaster />

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

/// One-way URL → store sync. After 1.1 the URL is the source of truth
/// for selected plan / agent; this component watches the route and
/// pushes the resolved id into the store so existing components that
/// still read `selectedPlan` / `selectedAgentId` keep rendering. The
/// reverse direction (store → URL) is driven by Link / navigate at the
/// click site, not here, so we never get into a feedback loop.
function RouteSync() {
  const { routePlanName, routeAgentId } = useRouteSelection();
  const selectPlan = usePlanStore((s) => s.selectPlan);
  const clearSelectedPlan = usePlanStore((s) => s.clearSelectedPlan);
  const selectedPlanName = usePlanStore((s) => s.selectedPlan?.name ?? null);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);

  useEffect(() => {
    if (routePlanName) {
      if (routePlanName !== selectedPlanName) {
        selectPlan(routePlanName).catch(() => {});
      }
    } else if (selectedPlanName) {
      clearSelectedPlan();
    }
  }, [routePlanName, selectedPlanName]);

  useEffect(() => {
    const next = routeAgentId ?? null;
    if (next !== selectedAgentId) {
      selectAgent(next);
    }
  }, [routeAgentId, selectedAgentId]);

  return null;
}

/// Right-rail AgentPanel mounts only when an agent id sits in the URL,
/// either as the deep-link path /agents/:agentId or as the cross-route
/// `?agent=<id>` overlay. Keeps today's behaviour where the agent
/// panel can sit beside any main view (notably PlanBoard). The
/// `<EnsureAgents/>` wrapper handles the data dependency: it ensures
/// the agent list has been fetched before rendering the panel, and
/// surfaces a not-found message in the rail when the URL id is
/// stale.
function AgentRail() {
  const { routeAgentId } = useRouteSelection();
  if (!routeAgentId) return null;
  return (
    <div className="w-[600px] border-l border-gray-800 h-full">
      <EnsureAgents>
        <AgentPanel />
      </EnsureAgents>
    </div>
  );
}

/// /new-plan renders the modal and routes back to /plans on close.
/// Wraps the existing NewPlanForm prop so its onClose contract stays
/// router-agnostic — tests still drive it with a plain callback.
function NewPlanRoute() {
  const navigate = useNavigate();
  return <NewPlanForm onClose={() => navigate("/plans")} />;
}
