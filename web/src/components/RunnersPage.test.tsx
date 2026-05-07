import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { RunnersPage } from "./RunnersPage.js";
import { useRunnerStore, type Runner } from "../stores/runner-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { useToastStore } from "../stores/toast-store.js";

function seedRunner(overrides: Partial<Runner> = {}): Runner {
  return {
    id: "runner-1",
    name: "laptop",
    status: "online",
    hostname: "host-1.example.com",
    version: "0.4.2",
    lastSeenAt: new Date(Date.now() - 60 * 1000).toISOString(),
    createdAt: new Date(Date.now() - 86_400 * 1000).toISOString(),
    ...overrides,
  };
}

function seedAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "a1",
    session_id: "s1",
    pid: 1,
    parent_agent_id: null,
    plan_name: "p",
    task_id: "1.1",
    cwd: "/tmp",
    status: "running",
    mode: "pty",
    prompt: null,
    started_at: new Date().toISOString(),
    finished_at: null,
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: null,
    source_branch: null,
    cost_usd: null,
    driver: "claude",
    ...overrides,
  };
}

beforeEach(() => {
  // Most tests run in saas mode; the standalone-redirect test
  // overrides this in-place.
  useRunnerStore.setState({
    mode: "saas",
    loaded: true,
    runners: [],
    fetchRunners: vi.fn().mockResolvedValue(undefined),
    createRunnerToken: vi.fn().mockResolvedValue({
      token: "deadbeef",
      runner_name: "laptop",
    }),
  });
  useAgentStore.setState({ agents: [] });
  useToastStore.getState().clear();
});

afterEach(() => {
  cleanup();
  useRunnerStore.getState().reset();
  useAgentStore.setState({ agents: [] });
  useToastStore.getState().clear();
});

describe("RunnersPage", () => {
  it("shows a loading shell while bootstrap is still resolving", () => {
    useRunnerStore.setState({ loaded: false });
    render(<RunnersPage />);
    expect(screen.getByText(/Loading runners/i)).toBeTruthy();
    // Empty state must NOT flash before bootstrap resolves.
    expect(screen.queryByTestId("runners-empty")).toBeNull();
  });

  it("renders the empty state with an Enrol button when no runners exist", () => {
    render(<RunnersPage />);
    expect(screen.getByTestId("runners-empty")).toBeTruthy();
    expect(screen.getAllByRole("button", { name: /Enrol a runner/i }).length).toBeGreaterThan(0);
  });

  it("lists registered runners with diagnostics", () => {
    useRunnerStore.setState({
      runners: [
        seedRunner({ id: "r1", name: "laptop", status: "online" }),
        seedRunner({
          id: "r2",
          name: "ci-runner",
          status: "offline",
          hostname: "ci.internal",
          version: "0.5.0",
        }),
      ],
    });
    useAgentStore.setState({
      agents: [seedAgent({ status: "running" }), seedAgent({ id: "a2", status: "completed" })],
    });
    render(<RunnersPage />);

    expect(screen.getByTestId("runners-list")).toBeTruthy();
    const rows = screen.getAllByTestId("runner-row");
    expect(rows.length).toBe(2);

    // Empty state must NOT render alongside the populated list.
    expect(screen.queryByTestId("runners-empty")).toBeNull();

    expect(screen.getByText(/laptop/)).toBeTruthy();
    expect(screen.getByText(/ci-runner/)).toBeTruthy();
    expect(screen.getByText(/host-1.example.com/)).toBeTruthy();
    expect(screen.getByText(/0\.4\.2/)).toBeTruthy();
    expect(screen.getByText(/0\.5\.0/)).toBeTruthy();
    // 1 in-flight (running), 1 ignored (completed) — header tally.
    expect(screen.getByText(/1 agent in flight/)).toBeTruthy();
  });

  it("opens the enroll modal when the header button is clicked", async () => {
    render(<RunnersPage />);
    fireEvent.click(screen.getByTestId("enroll-runner-button"));
    await waitFor(() => {
      expect(screen.getByTestId("runner-name-input")).toBeTruthy();
    });
  });

  it("redirects standalone deployments to / and pushes a warn toast", async () => {
    useRunnerStore.setState({ mode: "standalone", loaded: true });
    render(<RunnersPage />);
    // Navigate replaces the route; the page tree no longer contains
    // the runners content, so neither the empty state nor the loading
    // shell should be rendered after the effect runs.
    await waitFor(() => {
      expect(screen.queryByTestId("runners-empty")).toBeNull();
      expect(screen.queryByText(/Loading runners/)).toBeNull();
    });
    const toasts = useToastStore.getState().toasts;
    expect(toasts.some((t) => t.kind === "warn" && t.title.includes("SaaS"))).toBe(true);
  });

  it("does not redirect or toast while mode is still `unknown`", () => {
    useRunnerStore.setState({ mode: "unknown", loaded: false });
    render(<RunnersPage />);
    expect(screen.getByText(/Loading runners/i)).toBeTruthy();
    expect(useToastStore.getState().toasts.length).toBe(0);
  });

  it("renders a driver-inventory chip with N drivers · M ready summary", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      driversByRunnerId: {
        r1: [
          { name: "claude", status: { state: "api_key" } },
          { name: "aider", status: { state: "not_installed" } },
          { name: "codex", status: { state: "oauth", account: "alice" } },
          { name: "gemini", status: { state: "unauthenticated" } },
        ],
      },
    });
    render(<RunnersPage />);
    const chip = screen.getByTestId("runner-driver-chip");
    // 4 drivers / 2 ready (api_key + oauth). Two `not_installed` /
    // `unauthenticated` count as not-ready.
    expect(chip.textContent).toMatch(/4 drivers/);
    expect(chip.textContent).toMatch(/2 ready/);
  });

  it("driver-inventory popover shows per-driver state when expanded on hover", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      driversByRunnerId: {
        r1: [
          { name: "claude", status: { state: "api_key" } },
          { name: "aider", status: { state: "not_installed" } },
        ],
      },
    });
    render(<RunnersPage />);
    // Chip wrapper is the parent of the trigger button — hover over it.
    const chip = screen.getByTestId("runner-driver-chip");
    fireEvent.mouseEnter(chip.querySelector("button")!);
    const popover = screen.getByTestId("runner-driver-chip-popover");
    expect(popover.textContent).toMatch(/claude/);
    expect(popover.textContent).toMatch(/API key/);
    expect(popover.textContent).toMatch(/aider/);
    expect(popover.textContent).toMatch(/not installed/);
  });

  it("Select button calls setSelectedRunnerId with the row's id", () => {
    const setSelectedRunnerId = vi.fn();
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop" })],
      selectedRunnerId: null,
      setSelectedRunnerId,
    });
    render(<RunnersPage />);
    fireEvent.click(screen.getByTestId("runner-select-r1"));
    expect(setSelectedRunnerId).toHaveBeenCalledWith("r1");
  });
});
