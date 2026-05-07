import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { MobileTopBar } from "./MobileTopBar.js";
import { useUiStore } from "../stores/ui-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useAuthStore } from "../stores/auth-store.js";
import { usePlanStore, type ParsedPlan } from "../stores/plan-store.js";

beforeEach(() => {
  useUiStore.setState({ sidebarOpen: false });
  useWsStore.setState({ connected: true });
  // Defaults that hide OrgChip and RunnerStatus so the title + hamburger
  // tests can run in isolation. Individual tests below override to
  // exercise the SaaS path.
  useRunnerStore.setState({
    mode: "standalone",
    runners: [],
    loaded: true,
    lastRunnersFetchedAt: Date.now(),
  });
  useOrgStore.setState({
    memberships: [],
    loaded: true,
    lastFetchedAt: Date.now(),
  });
  useAuthStore.setState({
    user: { id: "u-1", email: "u@x.com", orgId: "org-1" },
    loading: false,
    error: null,
  });
  usePlanStore.setState({ selectedPlan: null });
});

afterEach(() => {
  cleanup();
});

function renderTopBar(initialPath: string = "/") {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <MobileTopBar />
    </MemoryRouter>,
  );
}

describe("MobileTopBar", () => {
  it("renders the hamburger button and opens the sidebar on click", () => {
    renderTopBar();
    expect(useUiStore.getState().sidebarOpen).toBe(false);
    fireEvent.click(screen.getByTestId("mobile-top-bar-hamburger"));
    expect(useUiStore.getState().sidebarOpen).toBe(true);
  });

  it("hamburger is a toggle: a second tap closes the sidebar", () => {
    renderTopBar();
    const btn = screen.getByTestId("mobile-top-bar-hamburger");
    fireEvent.click(btn);
    expect(useUiStore.getState().sidebarOpen).toBe(true);
    fireEvent.click(btn);
    expect(useUiStore.getState().sidebarOpen).toBe(false);
  });

  it("renders 'Plans' as the title at the project dashboard root", () => {
    renderTopBar("/");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("Plans");
  });

  it("renders 'Plans' on /plans", () => {
    renderTopBar("/plans");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("Plans");
  });

  it("renders the selected plan title on /plans/:planName", () => {
    const plan: ParsedPlan = {
      name: "my-plan",
      filePath: "my-plan.yaml",
      title: "My nice plan",
      context: "",
      project: null,
      createdAt: "2026-04-12T00:00:00Z",
      modifiedAt: "2026-04-12T00:00:00Z",
      phases: [],
    };
    usePlanStore.setState({ selectedPlan: plan });
    renderTopBar("/plans/my-plan");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("My nice plan");
  });

  it("falls back to the URL slug when /plans/:planName has no selected plan loaded", () => {
    renderTopBar("/plans/some-slug");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("some-slug");
  });

  it("renders 'Agents' on /agents and /agents/:id", () => {
    const { unmount } = renderTopBar("/agents");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("Agents");
    unmount();
    renderTopBar("/agents/abc-123");
    expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe("Agents");
  });

  it("renders 'Activity', 'Archive', 'Admin', 'Runners', 'New Plan' for their respective routes", () => {
    const cases: Array<[string, string]> = [
      ["/audit", "Activity"],
      ["/archive", "Archive"],
      ["/admin", "Admin"],
      ["/admin/members", "Admin"],
      ["/runners", "Runners"],
      ["/new-plan", "New Plan"],
    ];
    for (const [path, expected] of cases) {
      const { unmount } = renderTopBar(path);
      expect(screen.getByTestId("mobile-top-bar-title").textContent).toBe(expected);
      unmount();
    }
  });

  it("renders the connection dot in green when WS connected, red when disconnected", () => {
    useWsStore.setState({ connected: true });
    const { unmount } = renderTopBar();
    let dot = screen.getByTestId("mobile-top-bar-connection");
    expect(dot.className).toMatch(/bg-emerald-500/);
    expect(dot.getAttribute("aria-label")).toBe("Connected");
    unmount();

    useWsStore.setState({ connected: false });
    renderTopBar();
    dot = screen.getByTestId("mobile-top-bar-connection");
    expect(dot.className).toMatch(/bg-red-500/);
    expect(dot.getAttribute("aria-label")).toBe("Disconnected");
  });

  it("includes md:hidden so it never renders on `≥md`", () => {
    // The bar relies on Tailwind responsive utilities, not a JS check.
    // jsdom can't compute layout, so the surface assertion is class
    // presence — the desktop integration test in App.tsx covers the
    // rest.
    renderTopBar();
    const bar = screen.getByTestId("mobile-top-bar");
    expect(bar.className).toMatch(/md:hidden/);
  });
});
