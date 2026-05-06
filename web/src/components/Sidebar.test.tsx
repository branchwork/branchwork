import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { MemoryRouter, Route, Routes, useNavigate } from "react-router-dom";
import { Sidebar } from "./Sidebar.js";
import { useUiStore } from "../stores/ui-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useWsStore } from "../stores/ws-store.js";

beforeEach(() => {
  useUiStore.setState({ sidebarOpen: false });
  // Empty plan list keeps the render cheap; the mobile slide-over
  // contract doesn't depend on plan content.
  usePlanStore.setState({ plans: [], selectedPlan: null, warnings: [] });
  useAgentStore.setState({ agents: [] });
  useSettingsStore.setState({
    drivers: [],
    driversRunnerStatus: "local",
    effort: "medium",
    skipPermissions: false,
    // Stub the network call. The Sidebar's DriverStatusList kicks
    // off a /api/drivers fetch on mount; the test harness has no
    // dev server, and undici rejects relative URLs from jsdom — the
    // resulting unhandled rejection (from `promise.finally(...)` in
    // settings-store) would noisy up the run. Replacing the action
    // with a no-op resolves it cleanly.
    fetchDrivers: vi.fn().mockResolvedValue(undefined),
  });
  useRunnerStore.setState({
    mode: "standalone",
    runners: [],
    loaded: true,
    lastRunnersFetchedAt: Date.now(),
    selectedRunnerId: null,
  });
  useWsStore.setState({ connected: true });
});

afterEach(() => {
  cleanup();
});

function renderSidebar(initialPath: string = "/") {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Sidebar />
    </MemoryRouter>,
  );
}

describe("Sidebar", () => {
  it("renders the sidebar element with translate-x classes for the slide-over", () => {
    renderSidebar();
    const aside = screen.getByTestId("sidebar");
    // Closed: starts off-screen on mobile, persistent on desktop.
    expect(aside.className).toMatch(/-translate-x-full/);
    expect(aside.className).toMatch(/md:static/);
    expect(aside.className).toMatch(/md:translate-x-0/);
  });

  it("flips translate to translate-x-0 when sidebarOpen is true", () => {
    renderSidebar();
    const aside = screen.getByTestId("sidebar");
    expect(aside.className).toMatch(/-translate-x-full/);

    // Wrap setState in act so React commits the resulting render before
    // we re-query the DOM. Without it, the className is read from the
    // pre-update tree.
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const reopened = screen.getByTestId("sidebar");
    expect(reopened.className).toMatch(/translate-x-0/);
    expect(reopened.className).not.toMatch(/-translate-x-full/);
    expect(reopened.getAttribute("data-open")).toBe("true");
  });

  it("renders a backdrop only when sidebarOpen", () => {
    renderSidebar();
    expect(screen.queryByTestId("sidebar-backdrop")).toBeNull();
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const backdrop = screen.getByTestId("sidebar-backdrop");
    expect(backdrop).toBeTruthy();
    // Hidden on `≥md` so taps on the persistent desktop sidebar don't
    // hit a phantom overlay.
    expect(backdrop.className).toMatch(/md:hidden/);
  });

  it("clicking the backdrop calls closeSidebar", () => {
    renderSidebar();
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const backdrop = screen.getByTestId("sidebar-backdrop");
    fireEvent.click(backdrop);
    expect(useUiStore.getState().sidebarOpen).toBe(false);
  });

  it("auto-closes the sidebar on route change", () => {
    function Harness() {
      const navigate = useNavigate();
      return (
        <button
          data-testid="navigate-to-agents"
          onClick={() => navigate("/agents")}
        >
          go
        </button>
      );
    }

    render(
      <MemoryRouter initialEntries={["/"]}>
        <Sidebar />
        <Routes>
          <Route path="/" element={<Harness />} />
          <Route path="/agents" element={<Harness />} />
        </Routes>
      </MemoryRouter>,
    );

    useUiStore.getState().openSidebar();
    expect(useUiStore.getState().sidebarOpen).toBe(true);

    fireEvent.click(screen.getByTestId("navigate-to-agents"));
    // useEffect tied to location.pathname fires synchronously after the
    // navigate batch under React 18 — sidebarOpen is back to false.
    expect(useUiStore.getState().sidebarOpen).toBe(false);
  });
});
