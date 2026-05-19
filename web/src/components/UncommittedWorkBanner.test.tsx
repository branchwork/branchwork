import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { UncommittedWorkBanner } from "./PlanBoard.js";
import { usePlanStore, type PlanConfig } from "../stores/plan-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";

const PLAN = "p1";

function defaultConfig(overrides: Partial<PlanConfig> = {}): PlanConfig {
  return {
    autoAdvance: false,
    autoMode: true,
    maxFixAttempts: 3,
    pausedReason: null,
    parallel: false,
    runnerId: null,
    runnerFailover: "pause",
    ...overrides,
  };
}

function installFetchMock(
  handler: (path: string, init?: RequestInit) => Response | Promise<Response>,
): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      return handler(path, init);
    }),
  );
}

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-id-default",
    session_id: "sess-default",
    pid: 1234,
    parent_agent_id: null,
    plan_name: PLAN,
    task_id: "1.1",
    cwd: "/tmp/wd",
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
    ...overrides,
  };
}

function seed(config: PlanConfig | null, agents: Agent[]): void {
  usePlanStore.setState({
    planConfigs: config ? { [PLAN]: config } : {},
  });
  useAgentStore.setState({ agents });
}

beforeEach(() => {
  // Default no-op fetch — individual tests override when they exercise
  // the Resume PUT.
  vi.stubGlobal("fetch", vi.fn());
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  usePlanStore.setState({ planConfigs: {} });
  useAgentStore.setState({ agents: [], selectedAgentId: null });
});

describe("UncommittedWorkBanner", () => {
  it("does not render when pausedReason is null", () => {
    seed(defaultConfig({ pausedReason: null }), []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render for other pause reasons", () => {
    seed(defaultConfig({ pausedReason: "merge_conflict" }), []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render when there is no PlanConfig", () => {
    seed(null, []);
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders the banner copy when pausedReason is agent_left_uncommitted_work", () => {
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({ id: "running-agent-1" }),
    ]);
    render(<UncommittedWorkBanner planName={PLAN} />);
    expect(screen.getByText(/Auto-mode paused: agent left uncommitted work\./i)).toBeTruthy();
    expect(screen.getByText(/Inspect and either commit, discard, or click Resume\./i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Inspect agent/i })).toBeTruthy();
  });

  it("clicking Inspect agent selects the still-running task agent for this plan", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({
        id: "other-plan-running",
        plan_name: "p2",
        status: "running",
      }),
      agent({
        id: "this-plan-completed",
        plan_name: PLAN,
        status: "completed",
      }),
      agent({
        id: "this-plan-running",
        plan_name: PLAN,
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    fireEvent.click(screen.getByRole("button", { name: /Inspect agent/i }));

    expect(selectAgentSpy).toHaveBeenCalledTimes(1);
    expect(selectAgentSpy).toHaveBeenCalledWith("this-plan-running");
  });

  it("disables Inspect agent when no running agent exists for this plan", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({ id: "completed-1", status: "completed" }),
      agent({
        id: "other-plan",
        plan_name: "p2",
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    const button = screen.getByRole("button", {
      name: /Inspect agent/i,
    }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);

    fireEvent.click(button);
    expect(selectAgentSpy).not.toHaveBeenCalled();
  });

  it("ignores non-task (check-plan) agents when picking the inspect target", () => {
    const selectAgentSpy = vi.fn();
    useAgentStore.setState({ selectAgent: selectAgentSpy });
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), [
      agent({
        id: "check-plan-agent",
        task_id: null,
        status: "running",
      }),
      agent({
        id: "task-agent",
        task_id: "2.1",
        status: "running",
      }),
    ]);

    render(<UncommittedWorkBanner planName={PLAN} />);
    fireEvent.click(screen.getByRole("button", { name: /Inspect agent/i }));

    expect(selectAgentSpy).toHaveBeenCalledWith("task-agent");
  });

  it("renders the pausedFiles list as a collapsible details block", () => {
    seed(
      defaultConfig({
        pausedReason: "agent_left_uncommitted_work",
        pausedFiles: ["server.log", "runner.log", "web-dev.log"],
      }),
      [],
    );
    const { container } = render(<UncommittedWorkBanner planName={PLAN} />);

    // <details><summary>Uncommitted files (3)</summary>…
    const summary = screen.getByText(/Uncommitted files \(3\)/);
    expect(summary).toBeTruthy();
    expect(summary.tagName).toBe("SUMMARY");

    // All three filenames render in the collapsed payload.
    expect(container.textContent).toContain("server.log");
    expect(container.textContent).toContain("runner.log");
    expect(container.textContent).toContain("web-dev.log");

    // One "Mark as operational" button per file.
    const buttons = screen.getAllByRole("button", { name: /Mark as operational/i });
    expect(buttons).toHaveLength(3);
  });

  it("uses singular 'Uncommitted file' for a single entry", () => {
    seed(
      defaultConfig({
        pausedReason: "agent_left_uncommitted_work",
        pausedFiles: ["server.log"],
      }),
      [],
    );
    render(<UncommittedWorkBanner planName={PLAN} />);
    expect(screen.getByText(/Uncommitted file \(1\)/)).toBeTruthy();
  });

  it("omits the file list when pausedFiles is missing or empty", () => {
    seed(defaultConfig({ pausedReason: "agent_left_uncommitted_work" }), []);
    render(<UncommittedWorkBanner planName={PLAN} />);
    expect(screen.queryByText(/Uncommitted files?/)).toBeNull();
    expect(screen.queryByRole("button", { name: /Mark as operational/i })).toBeNull();
  });

  it("Resume anyway PUTs pausedReason:null and updates the store", async () => {
    const updatedConfig = defaultConfig({
      pausedReason: null,
      pausedFiles: null,
    });
    let observedBody: unknown = null;
    installFetchMock((path, init) => {
      if (path === "/api/plans/p1/config" && init?.method === "PUT") {
        observedBody = JSON.parse(init.body as string);
        return new Response(JSON.stringify(updatedConfig), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response("", { status: 404 });
    });

    seed(
      defaultConfig({
        pausedReason: "agent_left_uncommitted_work",
        pausedFiles: ["server.log"],
      }),
      [],
    );
    render(<UncommittedWorkBanner planName={PLAN} />);

    const button = screen.getByRole("button", { name: /Resume anyway/i });
    await act(async () => {
      fireEvent.click(button);
    });

    await waitFor(() => {
      expect(observedBody).toEqual({ pausedReason: null });
    });
    // setPlanConfig wrote the cleared config to the store, so the banner
    // disappears on the next render.
    await waitFor(() => {
      expect(usePlanStore.getState().planConfigs[PLAN]?.pausedReason).toBeNull();
    });
  });

  it("clicking Mark as operational opens the modal with a diff preview for the chosen file", async () => {
    seed(
      defaultConfig({
        pausedReason: "agent_left_uncommitted_work",
        pausedFiles: ["server.log", "runner.log"],
      }),
      [],
    );
    render(<UncommittedWorkBanner planName={PLAN} />);

    // Modal not in the DOM before any click.
    expect(screen.queryByRole("dialog")).toBeNull();

    // Click the first file's "Mark as operational" button.
    const buttons = screen.getAllByRole("button", { name: /Mark as operational/i });
    fireEvent.click(buttons[0]);

    const dialog = await screen.findByRole("dialog");
    expect(dialog).toBeTruthy();

    // The dialog explicitly references branchwork.toml and the
    // chosen file, with a `+`-prefixed addition.
    expect(dialog.textContent).toContain("branchwork.toml");
    expect(dialog.textContent).toContain("[auto_mode.dirty_tree]");
    expect(dialog.textContent).toContain('"server.log"');
    // The second file's name must NOT appear — modal is per-file.
    expect(dialog.textContent ?? "").not.toContain("runner.log");

    // Close button dismisses.
    fireEvent.click(screen.getByRole("button", { name: /^Close$/ }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
  });
});
