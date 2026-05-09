import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

// xterm.js, the FitAddon, and the WebLinks addon are stubbed so the PTY
// component can be mounted in jsdom without requiring a canvas. The shared
// `callLog` records xterm method calls and WebSocket sends in the order they
// happen so the resync ordering (reset → \x0c → resize) can be asserted.
const { mockTerm, mockFit, callLog } = vi.hoisted(() => {
  const callLog: string[] = [];
  const mockTerm = {
    cols: 100,
    rows: 30,
    reset: vi.fn(() => {
      callLog.push("term.reset");
    }),
    open: vi.fn(),
    loadAddon: vi.fn(),
    write: vi.fn(),
    onData: vi.fn(),
    dispose: vi.fn(),
  };
  const mockFit = {
    fit: vi.fn(),
  };
  return { mockTerm, mockFit, callLog };
});

vi.mock("@xterm/xterm", () => ({
  Terminal: vi.fn(() => mockTerm),
}));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: vi.fn(() => mockFit),
}));
vi.mock("@xterm/addon-web-links", () => ({
  WebLinksAddon: vi.fn(() => ({})),
}));
vi.mock("@xterm/xterm/css/xterm.css", () => ({}));

class FakeWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static lastInstance: FakeWebSocket | null = null;

  url: string;
  readyState = FakeWebSocket.CONNECTING;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: Event) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.lastInstance = this;
  }

  send(data: string) {
    if (data === "\x0c") {
      callLog.push("ws.send:formfeed");
    } else {
      callLog.push(`ws.send:${data}`);
    }
  }

  close() {
    this.readyState = FakeWebSocket.CLOSED;
  }

  addEventListener(_type: string, _listener: EventListener) {}
  removeEventListener(_type: string, _listener: EventListener) {}
}

// Replace the global jsdom-layout ResizeObserver stub with one that does NOT
// auto-fire on observe(). The test drives the resize callback manually so the
// ordering of the onopen vs resize paths is deterministic.
let resizeCallback: ResizeObserverCallback | null = null;
class ManualResizeObserver {
  constructor(cb: ResizeObserverCallback) {
    resizeCallback = cb;
  }
  observe() {}
  unobserve() {}
  disconnect() {
    resizeCallback = null;
  }
}

import PtyTerminal from "./PtyTerminal.js";

beforeEach(() => {
  callLog.length = 0;
  mockTerm.reset.mockClear();
  mockTerm.cols = 100;
  mockTerm.rows = 30;
  mockFit.fit.mockClear();
  resizeCallback = null;
  vi.stubGlobal("WebSocket", FakeWebSocket);
  vi.stubGlobal("ResizeObserver", ManualResizeObserver);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  FakeWebSocket.lastInstance = null;
});

function flushAnimationFrame(): Promise<void> {
  return new Promise((resolve) => {
    requestAnimationFrame(() => resolve());
  });
}

describe("PtyTerminal resync", () => {
  it("on initial WS open, calls term.reset() then sends \\x0c then sends resize", async () => {
    render(<PtyTerminal agentId="agent-1" />);
    const ws = FakeWebSocket.lastInstance;
    if (!ws) throw new Error("WebSocket was not constructed");

    ws.readyState = FakeWebSocket.OPEN;
    ws.onopen?.(new Event("open"));
    await flushAnimationFrame();

    const trimmed = callLog.filter(
      (e) => e === "term.reset" || e === "ws.send:formfeed" || e.startsWith("ws.send:{"),
    );
    expect(trimmed).toEqual([
      "term.reset",
      "ws.send:formfeed",
      `ws.send:${JSON.stringify({ type: "resize", cols: 100, rows: 30 })}`,
    ]);
  });

  it("on resize, calls term.reset() then sends \\x0c then sends resize at the new geometry", async () => {
    render(<PtyTerminal agentId="agent-1" />);
    const ws = FakeWebSocket.lastInstance;
    if (!ws) throw new Error("WebSocket was not constructed");

    // Bring the WS to OPEN and dispatch the initial onopen so we can assert
    // the resize path on top of an already-converged baseline.
    ws.readyState = FakeWebSocket.OPEN;
    ws.onopen?.(new Event("open"));
    await flushAnimationFrame();

    callLog.length = 0;
    mockTerm.cols = 120;
    mockTerm.rows = 40;
    if (!resizeCallback) throw new Error("ResizeObserver did not register a callback");
    resizeCallback([], {} as ResizeObserver);
    await flushAnimationFrame();

    expect(callLog).toEqual([
      "term.reset",
      "ws.send:formfeed",
      `ws.send:${JSON.stringify({ type: "resize", cols: 120, rows: 40 })}`,
    ]);
  });

  it("on resize while WS is still CONNECTING, calls term.reset() but does not send", async () => {
    render(<PtyTerminal agentId="agent-1" />);
    const ws = FakeWebSocket.lastInstance;
    if (!ws) throw new Error("WebSocket was not constructed");

    expect(ws.readyState).toBe(FakeWebSocket.CONNECTING);
    if (!resizeCallback) throw new Error("ResizeObserver did not register a callback");
    resizeCallback([], {} as ResizeObserver);
    await flushAnimationFrame();

    expect(callLog).toEqual(["term.reset"]);
  });
});
