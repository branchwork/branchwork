import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { DiagnosticsTab, formatUptime } from "./DiagnosticsTab.js";

interface FetchCall {
  url: string;
  method: string;
}

let calls: FetchCall[];
let healthBody: Record<string, unknown>;
let runnersBody: Record<string, unknown>;
let healthStatus: number;
let runnersStatus: number;

function installFetchMock() {
  vi.stubGlobal("fetch", async (input: RequestInfo, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const method = init?.method ?? "GET";
    calls.push({ url, method });
    if (url === "/api/health") {
      return new Response(JSON.stringify(healthBody), { status: healthStatus });
    }
    if (url === "/api/runners") {
      return new Response(JSON.stringify(runnersBody), {
        status: runnersStatus,
      });
    }
    return new Response("not found", { status: 404 });
  });
}

beforeEach(() => {
  calls = [];
  healthStatus = 200;
  runnersStatus = 200;
  healthBody = {
    status: "ok",
    version: "0.3.0",
    uptimeSeconds: 90061, // 1d 1h 1m 1s
    wsConnections: 3,
    timestamp: "2026-05-07T00:00:00Z",
  };
  runnersBody = { runners: [], mode: "standalone" };
  installFetchMock();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("formatUptime", () => {
  it("renders days+hours+minutes for >24h", () => {
    expect(formatUptime(90061)).toBe("1d 1h 1m");
  });

  it("renders hours+minutes for <24h", () => {
    expect(formatUptime(7320)).toBe("2h 2m");
  });

  it("renders minutes+seconds for <1h", () => {
    expect(formatUptime(125)).toBe("2m 5s");
  });

  it("renders seconds-only for <1m", () => {
    expect(formatUptime(45)).toBe("45s");
  });

  it("renders 0s at zero", () => {
    expect(formatUptime(0)).toBe("0s");
  });

  it("renders em-dash on NaN / negative", () => {
    expect(formatUptime(Number.NaN)).toBe("—");
    expect(formatUptime(-5)).toBe("—");
  });
});

describe("DiagnosticsTab", () => {
  it("fetches /api/health and /api/runners on mount and renders the values", async () => {
    render(<DiagnosticsTab refreshMs={60_000} />);
    await waitFor(() =>
      expect(screen.queryByTestId("diagnostics-version")).toBeTruthy(),
    );
    expect(screen.getByTestId("diagnostics-version").textContent).toBe(
      "0.3.0",
    );
    expect(screen.getByTestId("diagnostics-uptime").textContent).toBe(
      "1d 1h 1m",
    );
    expect(screen.getByTestId("diagnostics-ws-connections").textContent).toBe(
      "3",
    );
    expect(screen.getByTestId("diagnostics-mode").textContent).toBe(
      "standalone",
    );
    expect(calls.some((c) => c.url === "/api/health")).toBe(true);
    expect(calls.some((c) => c.url === "/api/runners")).toBe(true);
  });

  it("renders the standalone empty-state copy when there are no runners", async () => {
    render(<DiagnosticsTab refreshMs={60_000} />);
    await waitFor(() =>
      expect(screen.queryByTestId("diagnostics-no-runners")).toBeTruthy(),
    );
    expect(screen.getByTestId("diagnostics-no-runners").textContent).toMatch(
      /Standalone deployment/i,
    );
  });

  it("renders a runner row with status badge when one is registered", async () => {
    runnersBody = {
      mode: "saas",
      runners: [
        {
          id: "r-1",
          name: "edge-runner",
          status: "online",
          hostname: "host-1",
          version: "0.3.0",
          lastSeenAt: new Date().toISOString(),
          createdAt: "2026-05-01T00:00:00Z",
        },
      ],
    };
    render(<DiagnosticsTab refreshMs={60_000} />);
    await waitFor(() =>
      expect(screen.queryByTestId("diagnostics-runners-table")).toBeTruthy(),
    );
    expect(screen.getByText("edge-runner")).toBeTruthy();
    expect(screen.getByText("host-1")).toBeTruthy();
    // Status badge contains the literal "online" text.
    expect(screen.getAllByText(/online/i).length).toBeGreaterThan(0);
  });

  it("polls every refreshMs while document is visible", async () => {
    vi.useFakeTimers();
    render(<DiagnosticsTab refreshMs={1000} />);
    // Allow the initial fetch to resolve.
    await vi.runOnlyPendingTimersAsync();
    const initialCalls = calls.length;
    expect(initialCalls).toBeGreaterThanOrEqual(2); // health + runners
    // Advance past one refresh tick — should fire health + runners again.
    await vi.advanceTimersByTimeAsync(1000);
    await vi.runOnlyPendingTimersAsync();
    expect(calls.length).toBeGreaterThanOrEqual(initialCalls + 2);
  });

  it("skips refresh ticks when document.visibilityState is not visible", async () => {
    vi.useFakeTimers();
    // jsdom defaults visibilityState to "visible"; flip it to hidden so
    // the interval predicate sees a backgrounded tab.
    Object.defineProperty(document, "visibilityState", {
      value: "hidden",
      configurable: true,
    });
    render(<DiagnosticsTab refreshMs={1000} />);
    await vi.runOnlyPendingTimersAsync();
    // Initial mount fetches unconditionally — that's expected.
    const initialCalls = calls.length;
    // While hidden, the 1s tick should NOT trigger another fetch.
    await vi.advanceTimersByTimeAsync(3000);
    await vi.runOnlyPendingTimersAsync();
    expect(calls.length).toBe(initialCalls);
    // Restore for downstream tests.
    Object.defineProperty(document, "visibilityState", {
      value: "visible",
      configurable: true,
    });
  });

  it("surfaces a banner when /api/health fetch fails", async () => {
    healthStatus = 500;
    healthBody = { error: "kaboom" };
    render(<DiagnosticsTab refreshMs={60_000} />);
    await waitFor(() =>
      expect(screen.queryByText(/kaboom|http 500|server error/i)).toBeTruthy(),
    );
  });
});
