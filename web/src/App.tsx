import { Suspense, lazy, useEffect } from "react";
import { Navigate, Route, Routes, useNavigate } from "react-router-dom";
import { usePlanStore } from "./stores/plan-store.js";
import { useAgentStore } from "./stores/agent-store.js";
import { useWsStore } from "./stores/ws-store.js";
import { useSettingsStore } from "./stores/settings-store.js";
import { useRunnerStore } from "./stores/runner-store.js";
import { useOrgStore } from "./stores/org-store.js";
import { useAuthStore } from "./stores/auth-store.js";
import { Sidebar } from "./components/Sidebar.js";
import { MobileTopBar } from "./components/MobileTopBar.js";
import { PlanBoard } from "./components/PlanBoard.js";
import { ProjectDashboard } from "./components/ProjectDashboard.js";
import { AgentTree } from "./components/AgentTree.js";
import { AgentRail } from "./components/AgentRail.js";
import { NewPlanForm } from "./components/NewPlanForm.js";
// Lazy-load NewProjectModal so the modal forms (URL parser, credential
// picker, repo-creation host wiring) don't ship in the main entry chunk.
// Mirrors CredentialsPage's lazy-boundary pattern — the modal is a
// low-frequency operator surface and the per-route lazy split keeps
// the dashboard's first-paint bundle inside the gzipped budget.
const NewProjectModal = lazy(() =>
  import("./components/NewProjectModal.js").then((m) => ({ default: m.NewProjectModal })),
);
import { AuditLog } from "./components/AuditLog.js";
import { ArchivePanel } from "./components/ArchivePanel.js";
import { LoginPage } from "./components/LoginPage.js";
import { AdminPage } from "./components/AdminPage.js";
import { RunnersPage } from "./components/RunnersPage.js";

// Lazy-load the credentials surface so the modal forms (PEM textareas,
// kind picker, generated-key view) don't ship in the main entry chunk.
// Credentials is a low-frequency operator surface — the per-route lazy
// boundary keeps the dashboard's first-paint bundle inside the gzipped
// budget enforced by `scripts/check-bundle-size.ts`.
const CredentialsPage = lazy(() =>
  import("./components/CredentialsPage.js").then((m) => ({ default: m.CredentialsPage })),
);
import { Toaster } from "./components/Toaster.js";
import { ConnectionBanner } from "./components/ConnectionBanner.js";
import { LearningsDuePanel } from "./components/LearningsDuePanel.js";
import { RunnerStatus } from "./components/RunnerStatus.js";
import { OrgChip } from "./components/OrgChip.js";
import { EnsurePlan } from "./components/EnsurePlan.js";
import { NotFoundPage } from "./components/NotFoundPage.js";
import { useRouteSelection } from "./hooks/use-route-selection.js";

export function App() {
  const connected = useWsStore((s) => s.connected);
  const connect = useWsStore((s) => s.connect);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);

  const fetchSettings = useSettingsStore((s) => s.fetchSettings);
  const fetchDrivers = useSettingsStore((s) => s.fetchDrivers);
  const fetchRunners = useRunnerStore((s) => s.fetchRunners);
  const fetchOrgs = useOrgStore((s) => s.fetchOrgs);

  const user = useAuthStore((s) => s.user);
  const authLoading = useAuthStore((s) => s.loading);
  const fetchMe = useAuthStore((s) => s.fetchMe);
  const logout = useAuthStore((s) => s.logout);

  // Resolve auth first. Other stores/WS are gated below so unauthenticated
  // requests don't spam 401s into the dashboard.
  useEffect(() => {
    fetchMe();
  }, [fetchMe]);

  // Bootstrap: await the first fetch of every store BEFORE opening the WS.
  // Audit §4: previously `connect()` and the four `fetch*()` calls fired
  // simultaneously, so an early `agent_started` event could fire its own
  // `fetchAgents()` and race the bootstrap fetch — whichever resolved
  // second won, sometimes silently dropping fresh rows. Awaiting here
  // costs ~100–300ms of "first-event latency" but kills the bug class.
  // Each fetch is wrapped in its own `.catch()` so a single 503 can't
  // veto WS connection — the store-level error path leaves `*Fetched`
  // false so the loading shell stays visible, and the next reconnect
  // refetch reconciles.
  useEffect(() => {
    if (!user) return;
    let cancelled = false;
    (async () => {
      await Promise.allSettled([
        fetchPlans(),
        fetchAgents(),
        fetchSettings(),
        fetchDrivers(),
        fetchRunners(),
        fetchOrgs(),
      ]);
      if (cancelled) return;
      connect();
    })();
    return () => {
      cancelled = true;
    };
  }, [
    user,
    fetchPlans,
    fetchAgents,
    fetchSettings,
    fetchDrivers,
    fetchRunners,
    fetchOrgs,
    connect,
  ]);

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

  // Periodic agent reconcile — a safety net for the case the reconnect and
  // visibility refetches miss: a tab that stays open, visible, AND connected
  // but drops an individual `agent_stopped`/`agent_started` frame. Without a
  // poll the agent list silently drifts from DB truth ("agent shows running
  // but isn't") until the user reloads. Cheap now that `GET /api/agents`
  // omits the (multi-MB) prompt field. Agents-only; plans drift is covered by
  // plan_updated events. Paused while the tab is hidden to avoid background
  // churn — visibilitychange above re-syncs on return.
  useEffect(() => {
    if (!user) return;
    const id = setInterval(() => {
      if (document.visibilityState === "visible") {
        fetchAgents().catch(() => {});
      }
    }, 30_000);
    return () => clearInterval(id);
  }, [user, fetchAgents]);

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
    <div className="flex flex-col md:flex-row h-screen bg-gray-950 text-gray-100">
      <ConnectionBanner />
      <RouteSync />
      <MobileTopBar />
      <Sidebar />

      <main className="flex-1 flex overflow-hidden min-h-0">
        <div className="flex-1 overflow-auto">
          {/* Pending-learning queue (Phase 1.4 of learning-hub-ci-failure-
              capture). Mounts at the top of every route so an agent
              blocked on a CI failure is always visible until somebody
              captures a learning. Self-hides on empty queue. */}
          <LearningsDuePanel />
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
            <Route
              path="/credentials"
              element={
                <Suspense
                  fallback={
                    <div className="p-6 max-w-4xl mx-auto text-sm text-gray-500">
                      Loading credentials…
                    </div>
                  }
                >
                  <CredentialsPage />
                </Suspense>
              }
            />
            <Route path="/new-plan" element={<NewPlanRoute />} />
            <Route
              path="/new-project"
              element={
                <Suspense
                  fallback={
                    <div className="p-6 max-w-2xl mx-auto text-sm text-gray-500">
                      Loading new project dialog…
                    </div>
                  }
                >
                  <NewProjectRoute />
                </Suspense>
              }
            />
            <Route path="/login" element={<Navigate to="/" replace />} />
            <Route path="*" element={<NotFoundPage />} />
          </Routes>
        </div>

        <AgentRail />
      </main>

      <Toaster />

      {/* Connection indicator + org chip + logout. Hidden on `<md`
          where the org chip / runner status / connection dot reflow
          into MobileTopBar. Email + sign-out are intentionally
          desktop-only for now — mobile sign-out is a follow-up. */}
      <div className="hidden md:flex fixed bottom-3 right-3 items-center gap-3 text-xs text-gray-500">
        <OrgChip />
        <RunnerStatus />
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
        <button onClick={() => logout()} className="text-gray-600 hover:text-gray-300 transition">
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
  }, [routePlanName, selectedPlanName, selectPlan, clearSelectedPlan]);

  useEffect(() => {
    const next = routeAgentId ?? null;
    if (next !== selectedAgentId) {
      selectAgent(next);
    }
  }, [routeAgentId, selectedAgentId, selectAgent]);

  return null;
}

/// /new-plan renders the modal and routes back to /plans on close.
/// Wraps the existing NewPlanForm prop so its onClose contract stays
/// router-agnostic — tests still drive it with a plain callback.
function NewPlanRoute() {
  const navigate = useNavigate();
  return <NewPlanForm onClose={() => navigate("/plans")} />;
}

/// /new-project renders the Phase 2.4 modal and routes back to /plans on
/// close. Mirrors NewPlanRoute's wrapper pattern so tests can drive the
/// modal with a plain onClose callback.
function NewProjectRoute() {
  const navigate = useNavigate();
  return <NewProjectModal onClose={() => navigate("/plans")} />;
}
