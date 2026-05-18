import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { BlockingWorkflowStalledBanner } from "./PlanBoard.js";

const PLAN = "p1";

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

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("BlockingWorkflowStalledBanner", () => {
  it("renders nothing while the stalled list is empty", async () => {
    installFetchMock(async () => jsonResponse({ stalled: [] }));
    const { container } = render(<BlockingWorkflowStalledBanner planName={PLAN} />);
    await waitFor(() => {
      // Effect resolves; no banner.
      expect(container.innerHTML).toBe("");
    });
  });

  it("renders the banner copy when a stalled entry is returned", async () => {
    installFetchMock(async (path) => {
      expect(path).toBe(`/api/plans/${PLAN}/blocking-workflow-status`);
      return jsonResponse({
        stalled: [
          {
            workflow: "task-tests-typo",
            branch: "branchwork/p1/1.1",
            taskId: "1.1",
            agentId: "agent-abc",
          },
        ],
      });
    });
    render(<BlockingWorkflowStalledBanner planName={PLAN} />);

    // Banner should appear after the initial fetch settles.
    await waitFor(() => {
      const banner = screen.getByRole("alert");
      expect(banner).toBeTruthy();
      // Copy from the brief — workflow name and branch interpolated.
      expect(banner.textContent).toContain("task-tests-typo");
      expect(banner.textContent).toContain("branchwork/p1/1.1");
      expect(banner.textContent).toContain("ci_stalled");
      expect(banner.textContent).toContain("plan settings");
    });
  });

  it("renders one row per stalled workflow", async () => {
    installFetchMock(async () =>
      jsonResponse({
        stalled: [
          { workflow: "missing-a", branch: "b1", taskId: "1.1", agentId: "a1" },
          { workflow: "missing-b", branch: "b1", taskId: "1.1", agentId: "a1" },
        ],
      }),
    );
    render(<BlockingWorkflowStalledBanner planName={PLAN} />);
    await waitFor(() => {
      const banner = screen.getByRole("alert");
      expect(banner.textContent).toContain("missing-a");
      expect(banner.textContent).toContain("missing-b");
      expect(banner.querySelectorAll("li").length).toBe(2);
    });
  });

  it("treats a transient fetch failure as empty", async () => {
    installFetchMock(async () => new Response("oops", { status: 500 }));
    const { container } = render(<BlockingWorkflowStalledBanner planName={PLAN} />);
    await waitFor(() => {
      // The catch-arm sets stalled to [] → banner hidden.
      expect(container.innerHTML).toBe("");
    });
  });
});
