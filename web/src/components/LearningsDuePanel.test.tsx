import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { LearningsDuePanel } from "./LearningsDuePanel.js";
import {
  clearInFlightForTests,
  useLearningsStore,
  type PendingLearningRow,
} from "../stores/learnings-store.js";
import { useAuthStore } from "../stores/auth-store.js";

function row(overrides: Partial<PendingLearningRow> = {}): PendingLearningRow {
  return {
    id: 1,
    agentId: "agent-1",
    agentStatus: "running",
    planName: "plan-a",
    taskNumber: "1.4",
    branch: "branchwork/plan-a/1.4",
    runId: "12345",
    runUrl: "https://example.test/runs/12345",
    workflow: "tests.yml",
    conclusion: "failure",
    failedJob: null,
    summary: "tests workflow failed",
    observedAt: "2026-05-23T00:00:00Z",
    ...overrides,
  };
}

beforeEach(() => {
  useAuthStore.setState({
    user: { id: "u", email: "u@example.test", orgId: "default-org" },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify({ items: [] }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    ),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  useAuthStore.setState({ user: null });
  useLearningsStore.getState().reset();
  clearInFlightForTests();
});

describe("LearningsDuePanel", () => {
  it("renders nothing when not logged in", () => {
    useAuthStore.setState({ user: null });
    const { container } = render(<LearningsDuePanel />);
    expect(container.textContent).toBe("");
  });

  it("renders nothing when the fetch has not landed yet", () => {
    // loaded=false (initial state) — panel must stay hidden.
    const { container } = render(<LearningsDuePanel />);
    expect(container.textContent).toBe("");
  });

  it("renders nothing when the queue is empty post-fetch", async () => {
    // Pre-load the store with loaded=true, items=[].
    act(() => {
      useLearningsStore.setState({ items: [], loaded: true });
    });
    const { container } = render(<LearningsDuePanel />);
    expect(container.textContent).toBe("");
  });

  it("renders one row with summary, plan, task, agent status, run link", async () => {
    act(() => {
      useLearningsStore.setState({
        items: [row({ summary: "tests workflow failed" })],
        loaded: true,
      });
    });
    render(<LearningsDuePanel />);
    const panel = screen.getByTestId("learnings-due-panel");
    expect(panel.textContent).toContain("1 agent blocked");
    expect(panel.textContent).toContain("plan-a");
    expect(panel.textContent).toContain("1.4");
    expect(panel.textContent).toContain("tests workflow failed");
    expect(panel.textContent).toContain("running");
    // Plan link points at the per-plan board route.
    const planLink = panel.querySelector('a[href="/plans/plan-a"]') as HTMLAnchorElement | null;
    expect(planLink).toBeTruthy();
    // Run link opens in a new tab.
    const runLink = panel.querySelector(
      'a[href="https://example.test/runs/12345"]',
    ) as HTMLAnchorElement | null;
    expect(runLink).toBeTruthy();
    expect(runLink?.target).toBe("_blank");
  });

  it("pluralises the heading for multiple rows", async () => {
    act(() => {
      useLearningsStore.setState({
        items: [row({ id: 1 }), row({ id: 2, planName: "plan-b" })],
        loaded: true,
      });
    });
    render(<LearningsDuePanel />);
    expect(screen.getByTestId("learnings-due-panel").textContent).toContain("2 agents blocked");
  });

  it("hides itself again when items goes from non-empty to empty", async () => {
    act(() => {
      useLearningsStore.setState({ items: [row()], loaded: true });
    });
    const { container } = render(<LearningsDuePanel />);
    expect(screen.queryByTestId("learnings-due-panel")).toBeTruthy();
    act(() => {
      useLearningsStore.setState({ items: [], loaded: true });
    });
    await waitFor(() => {
      expect(container.textContent).toBe("");
    });
  });

  it("lazy-fetches the log when a row is expanded and renders it", async () => {
    // Override the default empty-items fetch with a text-returning one
    // for the log endpoint, leaving the items fetch alone.
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL) => {
        const path = typeof input === "string" ? input : input.toString();
        if (path === "/api/learnings/pending") {
          return new Response(JSON.stringify({ items: [row({ id: 9 })] }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          });
        }
        if (path === "/api/learnings/pending/9/log") {
          return new Response("FAIL: assertion failed\nthread main panicked\n", {
            status: 200,
            headers: { "Content-Type": "text/plain" },
          });
        }
        return new Response("not found", { status: 404 });
      }),
    );
    act(() => {
      useLearningsStore.setState({
        items: [row({ id: 9 })],
        loaded: true,
      });
    });
    render(<LearningsDuePanel />);
    const details = screen
      .getByTestId("learnings-due-panel")
      .querySelector("details") as HTMLDetailsElement;
    expect(details).toBeTruthy();
    // Expand the <details>; jsdom fires the toggle event when `open`
    // flips, so wrap in act() to flush state updates.
    await act(async () => {
      details.open = true;
      details.dispatchEvent(new Event("toggle"));
    });
    await waitFor(() => {
      const pre = details.querySelector("pre");
      expect(pre?.textContent).toContain("FAIL: assertion failed");
    });
  });
});
