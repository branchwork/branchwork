import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useNavigate } from "react-router-dom";
import axe from "axe-core";
import { Sidebar } from "./Sidebar.js";
import { useUiStore } from "../stores/ui-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useAuthStore } from "../stores/auth-store.js";

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

  it("dismiss-warning button has an accessible name", () => {
    usePlanStore.setState({
      warnings: [
        {
          name: "broken-plan",
          file: "broken-plan.yaml",
          error: "yaml syntax error",
          timestamp: Date.now(),
        },
      ],
    });
    renderSidebar();
    // Sidebar starts closed (aria-hidden). Opening it makes the warning
    // banner reachable to RTL's role-based queries.
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const btn = screen.getByRole("button", {
      name: "Dismiss broken-plan.yaml warning",
    });
    expect(btn.tagName).toBe("BUTTON");
  });

  it("Admin link's accessible name omits the gear glyph", () => {
    renderSidebar();
    act(() => {
      useUiStore.getState().openSidebar();
    });
    // The visible text is "⚙ Admin", but the gear is wrapped in
    // aria-hidden so screen readers read just "Admin".
    const link = screen.getByRole("link", { name: "Admin" });
    expect(link.getAttribute("href")).toBe("/admin");
  });

  it("axe-core reports zero icon-button-name violations on the rendered sidebar", async () => {
    usePlanStore.setState({
      plans: [
        {
          name: "alpha",
          title: "Alpha plan",
          project: "branchwork",
          phaseCount: 1,
          taskCount: 0,
          doneCount: 0,
          createdAt: new Date().toISOString(),
          modifiedAt: new Date().toISOString(),
        },
      ],
      warnings: [
        {
          name: "broken-plan",
          file: "broken-plan.yaml",
          error: "yaml syntax error",
          timestamp: Date.now(),
        },
      ],
    });
    const { container } = renderSidebar();
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });

  it("auto-closes the sidebar on route change", () => {
    function Harness() {
      const navigate = useNavigate();
      return (
        <button data-testid="navigate-to-agents" onClick={() => navigate("/agents")}>
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

  it("right-clicking a plan row opens an Export context menu (Phase 1.2)", () => {
    usePlanStore.setState({
      plans: [
        {
          name: "alpha",
          title: "Alpha plan",
          project: "branchwork",
          phaseCount: 1,
          taskCount: 0,
          doneCount: 0,
          createdAt: new Date().toISOString(),
          modifiedAt: new Date().toISOString(),
        },
      ],
    });
    renderSidebar();
    act(() => {
      useUiStore.getState().openSidebar();
    });
    const row = screen.getByRole("link", { name: /Alpha plan/ });
    fireEvent.contextMenu(row);
    const menu = screen.getByTestId("plan-row-context-menu-alpha");
    expect(menu).toBeTruthy();
    // The single menu item is "Export".
    const exportItem = screen.getByRole("menuitem", { name: /Export/ });
    expect(exportItem).toBeTruthy();
  });
});

describe("Sidebar worktree disk usage", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    useOrgStore.setState({ memberships: [] });
    useAuthStore.setState({ user: null });
  });

  function planFor(project: string) {
    return {
      name: project.toLowerCase().replace(/[^a-z0-9]/g, "-"),
      title: `${project} plan`,
      project,
      phaseCount: 1,
      taskCount: 2,
      doneCount: 1,
      createdAt: new Date().toISOString(),
      modifiedAt: new Date().toISOString(),
    };
  }

  function seedMember(projects: string[]) {
    useOrgStore.setState({
      memberships: [
        { id: "org-1", name: "Org One", slug: "org-1", role: "member", memberCount: 1 },
      ],
    });
    useAuthStore.setState({ user: { id: "u1", email: "u@x.test", orgId: "org-1" } });
    usePlanStore.setState({ plans: projects.map(planFor) });
  }

  /// Stub `fetch` so only the disk-usage endpoint returns `projects`; every
  /// other Sidebar fetch (/api/health, /api/health/state) rejects — the same
  /// "network disabled" behaviour the other Sidebar tests rely on (those
  /// components catch the rejection and render a safe fallback). Returns the
  /// spy so callers can assert which URLs were hit.
  function stubFetch(projects: { slug: string; size_bytes: number; size_human: string }[]) {
    const spy = vi.fn(async (input: RequestInfo | URL) => {
      const path = typeof input === "string" ? input : input.toString();
      if (path.includes("/worktree-disk-usage")) {
        return new Response(JSON.stringify({ projects }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      throw new Error("network disabled in test");
    });
    vi.stubGlobal("fetch", spy);
    return spy;
  }

  it("renders the du-measured size next to a project group when the slug matches", async () => {
    // `MyProj` slugifies to `myproj` (same rule as project_slug_for_worktree).
    seedMember(["MyProj"]);
    stubFetch([{ slug: "myproj", size_bytes: 1572864, size_human: "1.5M" }]);

    renderSidebar();
    const size = await screen.findByTestId("disk-usage-myproj");
    expect(size.textContent).toBe("1.5M");
  });

  it("shows a size only for groups whose slug has a worktree entry", async () => {
    seedMember(["MyProj", "Other Thing"]);
    // Response carries only `myproj`; `other-thing` has no worktree tree.
    stubFetch([{ slug: "myproj", size_bytes: 1048576, size_human: "1.0M" }]);

    renderSidebar();
    // Wait for the matching chip to confirm the fetch resolved.
    expect((await screen.findByTestId("disk-usage-myproj")).textContent).toBe("1.0M");
    expect(screen.queryByTestId("disk-usage-other-thing")).toBeNull();
  });

  it("does not fetch disk usage when the user has no org membership", async () => {
    // Default state (afterEach reset): no memberships → diskUsageSlug is null →
    // no disk-usage fetch ever fires.
    usePlanStore.setState({ plans: [planFor("MyProj")] });
    const spy = stubFetch([]);
    renderSidebar();
    // Give effects a tick to run; the disk-usage URL must never be requested.
    await Promise.resolve();
    const diskUsageCalls = spy.mock.calls.filter((c) =>
      String(c[0]).includes("/worktree-disk-usage"),
    );
    expect(diskUsageCalls).toHaveLength(0);
  });
});
