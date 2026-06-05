import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { InitGateModal } from "./InitGateModal.js";
import { usePlanStore, type InitGatePending } from "../stores/plan-store.js";
import { handleWsMessage } from "../stores/ws-store.js";

const PLAN = "p1";

function defaultGate(overrides: Partial<InitGatePending> = {}): InitGatePending {
  return {
    nodeId: "init",
    planTitle: "My DAG Plan",
    context: "Build the thing carefully.",
    checks: ["git_repo", "remote_configured", "clean_tree"],
    dismissed: false,
    ...overrides,
  };
}

function seed(gate: InitGatePending | null): void {
  usePlanStore.setState({ initGates: gate ? { [PLAN]: gate } : {} });
}

/// Install a fetch mock that captures requests. Returns the captured
/// requests array so tests can inspect what the Start button posted.
function installFetchMock(
  responses: Record<string, () => Response | Promise<Response>>,
): Array<{ path: string; method: string; body?: unknown }> {
  const captured: Array<{ path: string; method: string; body?: unknown }> = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      const method = init?.method ?? "GET";
      let body: unknown;
      if (typeof init?.body === "string") {
        try {
          body = JSON.parse(init.body);
        } catch {
          body = init.body;
        }
      }
      captured.push({ path, method, body });
      const responder = responses[`${method} ${path}`] ?? responses[path];
      if (!responder) {
        return new Response(JSON.stringify({}), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return responder();
    }),
  );
  return captured;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn());
  seed(null);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  seed(null);
});

describe("InitGateModal", () => {
  it("renders nothing when no init gate is pending", () => {
    render(<InitGateModal />);
    expect(screen.queryByTestId("init-gate-modal")).toBeNull();
  });

  it("renders the approve dialog (title, context, green checks, Start) for a pending gate", () => {
    seed(defaultGate());
    render(<InitGateModal />);
    expect(screen.getByTestId("init-gate-modal")).toBeTruthy();
    // Title interpolates the plan title.
    expect(screen.getByText('Start "My DAG Plan"?')).toBeTruthy();
    // Context summary rendered.
    expect(screen.getByText("Build the thing carefully.")).toBeTruthy();
    // Each check rendered with a friendly label.
    const checks = screen.getByTestId("init-gate-checks");
    expect(checks.textContent).toContain("Git repository initialized");
    expect(checks.textContent).toContain("Git remote configured");
    expect(checks.textContent).toContain("Working tree is clean");
    expect(screen.getByTestId("init-gate-start")).toBeTruthy();
  });

  it("falls back to the raw check name for unknown checks", () => {
    seed(defaultGate({ checks: ["custom_thing"] }));
    render(<InitGateModal />);
    expect(screen.getByTestId("init-gate-checks").textContent).toContain("custom_thing");
  });

  it("renders a generic 'all passed' line when there are no named checks", () => {
    seed(defaultGate({ checks: [] }));
    render(<InitGateModal />);
    expect(screen.queryByTestId("init-gate-checks")).toBeNull();
    expect(screen.getByText("All preconditions passed")).toBeTruthy();
  });

  it("Start Plan posts to the approve endpoint and clears the slice", async () => {
    seed(defaultGate());
    const captured = installFetchMock({
      "POST /api/plans/p1/gates/init/approve": () =>
        jsonResponse({ ok: true, plan: PLAN, nodeId: "init", status: "completed" }),
    });
    render(<InitGateModal />);

    fireEvent.click(screen.getByTestId("init-gate-start"));

    await waitFor(() => {
      expect(
        captured.some((c) => c.method === "POST" && c.path === "/api/plans/p1/gates/init/approve"),
      ).toBe(true);
    });
    // Optimistic clear: slice gone, modal hidden.
    await waitFor(() => {
      expect(usePlanStore.getState().initGates[PLAN]).toBeUndefined();
    });
    expect(screen.queryByTestId("init-gate-modal")).toBeNull();
  });

  it("surfaces an inline error and keeps the modal open when approve fails", async () => {
    seed(defaultGate());
    installFetchMock({
      "POST /api/plans/p1/gates/init/approve": () =>
        jsonResponse({ error: "not_a_gate", message: "boom" }, 409),
    });
    render(<InitGateModal />);

    fireEvent.click(screen.getByTestId("init-gate-start"));

    await waitFor(() => {
      expect(screen.getByRole("alert").textContent).toContain("boom");
    });
    // Slice still present (not cleared on failure).
    expect(usePlanStore.getState().initGates[PLAN]).toBeTruthy();
  });

  it("'Later' dismisses (modal hides) but keeps the slice with dismissed=true", () => {
    seed(defaultGate());
    render(<InitGateModal />);
    fireEvent.click(screen.getByText("Later"));
    expect(screen.queryByTestId("init-gate-modal")).toBeNull();
    expect(usePlanStore.getState().initGates[PLAN]?.dismissed).toBe(true);
  });

  it("populates the slice (fetching plan title + context) on gate_ready_for_approval[init]", async () => {
    installFetchMock({
      "GET /api/plans/p1": () =>
        jsonResponse({ name: PLAN, title: "Fetched Title", context: "Fetched context" }),
    });
    render(<InitGateModal />);

    act(() => {
      handleWsMessage({
        type: "gate_ready_for_approval",
        data: {
          plan_name: PLAN,
          node_id: "init",
          gate_kind: "init",
          checks: ["git_repo"],
        },
      });
    });

    // Modal appears once the async plan fetch resolves and the slice is set.
    expect(await screen.findByTestId("init-gate-modal")).toBeTruthy();
    expect(screen.getByText('Start "Fetched Title"?')).toBeTruthy();
    expect(screen.getByText("Fetched context")).toBeTruthy();
    const slice = usePlanStore.getState().initGates[PLAN];
    expect(slice?.nodeId).toBe("init");
    expect(slice?.checks).toEqual(["git_repo"]);
  });

  it("ignores gate_ready_for_approval when gate_kind is not init", async () => {
    installFetchMock({});
    render(<InitGateModal />);

    act(() => {
      handleWsMessage({
        type: "gate_ready_for_approval",
        data: {
          plan_name: PLAN,
          node_id: "approval-step",
          gate_kind: "approval",
          checks: [],
        },
      });
    });

    // Give any (wrongly-scheduled) async fetch a tick to resolve.
    await Promise.resolve();
    expect(usePlanStore.getState().initGates[PLAN]).toBeUndefined();
    expect(screen.queryByTestId("init-gate-modal")).toBeNull();
  });

  it("clears the pending init gate on gate_approved for the same node", () => {
    seed(defaultGate());
    render(<InitGateModal />);
    expect(screen.getByTestId("init-gate-modal")).toBeTruthy();

    act(() => {
      handleWsMessage({
        type: "gate_approved",
        data: { plan_name: PLAN, node_id: "init" },
      });
    });

    expect(usePlanStore.getState().initGates[PLAN]).toBeUndefined();
    expect(screen.queryByTestId("init-gate-modal")).toBeNull();
  });

  it("does NOT clear the init gate when gate_failed names a different node", () => {
    seed(defaultGate());
    render(<InitGateModal />);

    act(() => {
      handleWsMessage({
        type: "gate_failed",
        data: { plan_name: PLAN, node_id: "some-other-node", reason: "unrelated" },
      });
    });

    expect(usePlanStore.getState().initGates[PLAN]).toBeTruthy();
    expect(screen.getByTestId("init-gate-modal")).toBeTruthy();
  });

  it("does NOT clear on gate_status_changed[in_progress] (fires before ready)", () => {
    seed(defaultGate());
    render(<InitGateModal />);

    act(() => {
      handleWsMessage({
        type: "gate_status_changed",
        data: { plan_name: PLAN, node_id: "init", status: "in_progress", gate_kind: "init" },
      });
    });

    expect(usePlanStore.getState().initGates[PLAN]).toBeTruthy();
  });
});
