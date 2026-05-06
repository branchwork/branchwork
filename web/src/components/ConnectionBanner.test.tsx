import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { ConnectionBanner, CONNECTION_BANNER_GRACE_MS } from "./ConnectionBanner.js";
import { useWsStore } from "../stores/ws-store.js";

const INITIAL_STATE = useWsStore.getState();

afterEach(() => {
  cleanup();
  // Restore the store back to a known shape between tests so a leftover
  // `disconnectedSince` from one case can't bleed into the next.
  useWsStore.setState({
    connected: INITIAL_STATE.connected,
    socket: INITIAL_STATE.socket,
    reconnectAttempt: INITIAL_STATE.reconnectAttempt,
    disconnectedSince: null,
  });
});

describe("ConnectionBanner", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not render when WS is connected", () => {
    useWsStore.setState({ connected: true, disconnectedSince: null });
    render(<ConnectionBanner />);
    expect(screen.queryByTestId("connection-banner")).toBeNull();
  });

  it("does not render before the grace window has elapsed", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now(),
    });
    render(<ConnectionBanner />);
    // Inside the grace window — banner is still hidden.
    expect(screen.queryByTestId("connection-banner")).toBeNull();
    act(() => {
      vi.advanceTimersByTime(CONNECTION_BANNER_GRACE_MS - 100);
    });
    expect(screen.queryByTestId("connection-banner")).toBeNull();
  });

  it("renders the warning copy after the grace window elapses", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now(),
    });
    render(<ConnectionBanner />);
    act(() => {
      vi.advanceTimersByTime(CONNECTION_BANNER_GRACE_MS + 50);
    });
    const banner = screen.getByTestId("connection-banner");
    expect(banner).toBeTruthy();
    expect(banner.textContent ?? "").toMatch(/Disconnected\. Reconnecting/);
    expect(banner.getAttribute("role")).toBe("alert");
  });

  it("auto-dismisses on reconnect", () => {
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now(),
    });
    render(<ConnectionBanner />);
    act(() => {
      vi.advanceTimersByTime(CONNECTION_BANNER_GRACE_MS + 50);
    });
    expect(screen.getByTestId("connection-banner")).toBeTruthy();

    // Reconnect — store flips connected:true and clears disconnectedSince.
    act(() => {
      useWsStore.setState({ connected: true, disconnectedSince: null });
    });
    expect(screen.queryByTestId("connection-banner")).toBeNull();
  });

  it("renders immediately when mounted into an already-stale disconnect", () => {
    // Disconnect happened more than the grace window ago — no need to wait.
    useWsStore.setState({
      connected: false,
      disconnectedSince: Date.now() - (CONNECTION_BANNER_GRACE_MS + 5_000),
    });
    render(<ConnectionBanner />);
    expect(screen.getByTestId("connection-banner")).toBeTruthy();
  });

  it("does not render when disconnectedSince is null (initial bootstrap)", () => {
    useWsStore.setState({ connected: false, disconnectedSince: null });
    render(<ConnectionBanner />);
    act(() => {
      vi.advanceTimersByTime(CONNECTION_BANNER_GRACE_MS * 2);
    });
    expect(screen.queryByTestId("connection-banner")).toBeNull();
  });
});
