import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { RunnerStatus } from "./RunnerStatus.js";
import { useRunnerStore, type Runner } from "../stores/runner-store.js";
import { handleWsMessage } from "../stores/ws-store.js";

afterEach(() => {
  cleanup();
  useRunnerStore.setState({
    mode: "unknown",
    runners: [],
    loaded: false,
    lastRunnersFetchedAt: null,
  });
});

function makeRunner(overrides: Partial<Runner> = {}): Runner {
  return {
    id: "runner-1",
    name: "primary",
    status: "online",
    hostname: "host-1",
    version: "1.0.0",
    lastSeenAt: "2026-04-12T00:00:00Z",
    createdAt: "2026-04-01T00:00:00Z",
    ...overrides,
  };
}

function renderRunnerStatus() {
  return render(
    <MemoryRouter>
      <RunnerStatus />
    </MemoryRouter>,
  );
}

describe("RunnerStatus", () => {
  it("renders nothing on standalone deployments", () => {
    useRunnerStore.setState({ mode: "standalone", runners: [], loaded: true });
    const { container } = renderRunnerStatus();
    expect(container.querySelector('[data-testid="runner-status"]')).toBeNull();
  });

  it("renders nothing while the initial fetch is still in flight", () => {
    // `loaded === false` is the bootstrap state. Without this guard a
    // standalone box flashes the amber prompt for ~50ms before the first
    // /api/runners response lands.
    useRunnerStore.setState({ mode: "unknown", runners: [], loaded: false });
    const { container } = renderRunnerStatus();
    expect(container.querySelector('[data-testid="runner-status"]')).toBeNull();
  });

  it("shows the amber 'register' prompt on SaaS with no runners", () => {
    useRunnerStore.setState({ mode: "saas", runners: [], loaded: true });
    renderRunnerStatus();
    const indicator = screen.getByTestId("runner-status");
    expect(indicator.getAttribute("data-state")).toBe("no-runner");
    expect(indicator.textContent ?? "").toMatch(/No runner/);
    const link = indicator.querySelector("a");
    expect(link?.getAttribute("href")).toBe("/runners");
  });

  it("shows red 'offline' when the most-recent runner is offline", () => {
    useRunnerStore.setState({
      mode: "saas",
      runners: [makeRunner({ status: "offline", name: "host-a" })],
      loaded: true,
    });
    renderRunnerStatus();
    const indicator = screen.getByTestId("runner-status");
    expect(indicator.getAttribute("data-state")).toBe("offline");
    expect(indicator.textContent ?? "").toMatch(/Runner offline/);
  });

  it("shows emerald with the runner name when at least one is online", () => {
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        makeRunner({ id: "old", status: "offline", name: "stale" }),
        makeRunner({ id: "new", status: "online", name: "primary" }),
      ],
      loaded: true,
    });
    renderRunnerStatus();
    const indicator = screen.getByTestId("runner-status");
    expect(indicator.getAttribute("data-state")).toBe("online");
    expect(indicator.textContent ?? "").toMatch(/Runner: primary/);
  });

  it("flips to amber/offline within one render after `runner_disconnected`", () => {
    // Live-update repro the brief calls out: simulate a `runner_disconnected`
    // event arriving after the runner was online — indicator must flip without
    // a refetch.
    useRunnerStore.setState({
      mode: "saas",
      runners: [makeRunner({ id: "r1", status: "online", name: "primary" })],
      loaded: true,
    });
    renderRunnerStatus();
    expect(screen.getByTestId("runner-status").getAttribute("data-state")).toBe(
      "online",
    );
    act(() => {
      handleWsMessage({
        type: "runner_disconnected",
        data: { runner_id: "r1" },
      });
    });
    expect(screen.getByTestId("runner-status").getAttribute("data-state")).toBe(
      "offline",
    );
  });

  it("flips to emerald within one render after `runner_connected`", () => {
    useRunnerStore.setState({ mode: "saas", runners: [], loaded: true });
    renderRunnerStatus();
    expect(screen.getByTestId("runner-status").getAttribute("data-state")).toBe(
      "no-runner",
    );
    act(() => {
      handleWsMessage({
        type: "runner_connected",
        data: { runner_id: "r1", runner_name: "primary" },
      });
    });
    const indicator = screen.getByTestId("runner-status");
    expect(indicator.getAttribute("data-state")).toBe("online");
    expect(indicator.textContent ?? "").toMatch(/Runner: primary/);
  });
});
