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
    merge_status: null,
    spawn_error: null,
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

  it("Shutdown opens a confirmation modal that gates on the in-flight checkbox", async () => {
    const requestRunnerShutdown = vi.fn().mockResolvedValue({
      queued: true,
      seq: 1,
      inFlightAgents: ["a1"],
    });
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      requestRunnerShutdown,
    });
    useAgentStore.setState({ agents: [seedAgent({ id: "a1", status: "running" })] });

    render(<RunnersPage />);
    fireEvent.click(screen.getByTestId("runner-shutdown-r1"));

    // Modal renders with the "1 in-flight agent" warning.
    await waitFor(() => {
      expect(screen.getByTestId("shutdown-modal-in-flight")).toBeTruthy();
    });

    // Submit is disabled until the user explicitly checks the
    // "I understand" box.
    const submit = screen.getByTestId("shutdown-modal-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    expect(requestRunnerShutdown).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("shutdown-modal-confirm"));
    expect(submit.disabled).toBe(false);

    // Type a reason and submit.
    fireEvent.change(screen.getByTestId("shutdown-modal-reason"), {
      target: { value: "host upgrade" },
    });
    fireEvent.click(submit);

    await waitFor(() => {
      expect(requestRunnerShutdown).toHaveBeenCalledWith("r1", {
        reason: "host upgrade",
        confirmInFlight: true,
      });
    });
  });

  it("Shutdown submit is allowed without confirm when no agents are in flight", async () => {
    const requestRunnerShutdown = vi.fn().mockResolvedValue({
      queued: true,
      seq: 1,
      inFlightAgents: [],
    });
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      requestRunnerShutdown,
    });
    useAgentStore.setState({ agents: [] });

    render(<RunnersPage />);
    fireEvent.click(screen.getByTestId("runner-shutdown-r1"));
    await waitFor(() => {
      expect(screen.getByTestId("shutdown-modal-submit")).toBeTruthy();
    });

    // No checkbox should be rendered when no agents are in flight.
    expect(screen.queryByTestId("shutdown-modal-confirm")).toBeNull();

    const submit = screen.getByTestId("shutdown-modal-submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    await waitFor(() => {
      expect(requestRunnerShutdown).toHaveBeenCalledWith("r1", {
        reason: undefined,
        confirmInFlight: undefined,
      });
    });
  });

  it("Revoke opens a confirmation modal and DELETEs on submit", async () => {
    const revokeRunner = vi.fn().mockResolvedValue({ revoked: true, tokensRevoked: 1 });
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      revokeRunner,
    });

    render(<RunnersPage />);
    fireEvent.click(screen.getByTestId("runner-revoke-r1"));

    await waitFor(() => {
      expect(screen.getByTestId("revoke-modal-warning")).toBeTruthy();
    });
    fireEvent.click(screen.getByTestId("revoke-modal-submit"));

    await waitFor(() => {
      expect(revokeRunner).toHaveBeenCalledWith("r1");
    });
  });

  it("renders the 'Requested at' label after a shutdown request", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      shutdownRequestedAt: { r1: Date.now() },
    });
    render(<RunnersPage />);
    expect(screen.getByTestId("runner-shutdown-requested-r1")).toBeTruthy();
    // Original Request-shutdown button must NOT be in the DOM while the
    // request is in flight — the label takes its place.
    expect(screen.queryByTestId("runner-shutdown-r1")).toBeNull();
  });

  it("renders the 30s 'shutdown ignored' fallback after the timeout elapses", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", name: "laptop", status: "online" })],
      shutdownRequestedAt: { r1: Date.now() - 60_000 },
    });
    render(<RunnersPage />);
    expect(screen.getByTestId("runner-shutdown-requested-r1").textContent).toMatch(
      /Shutdown ignored/,
    );
    expect(screen.getByTestId("runner-shutdown-fallback-revoke-r1")).toBeTruthy();
  });

  // ── T1.2 server-bin self-diagnostic chip ───────────────────────────

  it("server-bin chip renders the resolved path with a green check", () => {
    useRunnerStore.setState({
      runners: [
        seedRunner({
          id: "r1",
          name: "laptop",
          status: "online",
          serverBin: { path: "/usr/local/bin/branchwork-server", error: null },
        }),
      ],
    });
    render(<RunnersPage />);
    const chip = screen.getByTestId("server-bin-chip");
    expect(chip).toBeTruthy();
    expect(chip.textContent).toContain("/usr/local/bin/branchwork-server");
    // Green-emerald palette signals "found". The exact class chain isn't
    // pinned (Tailwind reflows would break it); assert the chip's
    // accessibility label and the SR-only text instead.
    expect(chip.textContent).toContain("branchwork-server found");
  });

  it("server-bin chip renders the resolution error with a red cross", () => {
    useRunnerStore.setState({
      runners: [
        seedRunner({
          id: "r1",
          name: "laptop",
          status: "online",
          serverBin: { path: null, error: "not on $PATH: branchwork-server" },
        }),
      ],
    });
    render(<RunnersPage />);
    const chip = screen.getByTestId("server-bin-chip");
    expect(chip).toBeTruthy();
    expect(chip.textContent).toContain("not on $PATH: branchwork-server");
    expect(chip.textContent).toContain("branchwork-server not found");
  });

  it("server-bin chip is hidden when the runner has not reported the field", () => {
    // T1.2 back-compat: older runners pre-T1.2 don't ship `server_bin`
    // on RunnerHello, so `serverBin` lands as undefined on the row. The
    // chip must render nothing rather than fake a verdict.
    useRunnerStore.setState({
      runners: [
        seedRunner({
          id: "r1",
          name: "laptop",
          status: "online",
          serverBin: undefined,
        }),
      ],
    });
    render(<RunnersPage />);
    expect(screen.queryByTestId("server-bin-chip")).toBeNull();
  });
});
