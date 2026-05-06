import { afterEach, describe, expect, it } from "vitest";
import { useRunnerStore, type Runner } from "./runner-store.js";

afterEach(() => {
  useRunnerStore.getState().reset();
});

function seedRunner(overrides: Partial<Runner> = {}): Runner {
  return {
    id: "r1",
    name: "primary",
    status: "online",
    hostname: "host-1",
    version: "1.0.0",
    lastSeenAt: "2026-04-12T00:00:00Z",
    createdAt: "2026-04-01T00:00:00Z",
    ...overrides,
  };
}

describe("runner-store", () => {
  it("starts in `unknown` mode with `loaded=false`", () => {
    const s = useRunnerStore.getState();
    expect(s.mode).toBe("unknown");
    expect(s.runners).toEqual([]);
    expect(s.loaded).toBe(false);
  });

  it("applyConnected upserts an existing row and flips it online", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1", status: "offline", name: "stale" })],
    });
    useRunnerStore
      .getState()
      .applyConnected({ runner_id: "r1", runner_name: "primary" });
    const next = useRunnerStore.getState().runners;
    expect(next).toHaveLength(1);
    expect(next[0].status).toBe("online");
    expect(next[0].name).toBe("primary");
  });

  it("applyConnected synthesizes a row when the runner is unknown", () => {
    // WS event for a runner the dashboard hasn't refetched yet — the
    // indicator must still flip emerald, the missing fields fill in later.
    useRunnerStore.setState({ mode: "saas", loaded: true, runners: [] });
    useRunnerStore
      .getState()
      .applyConnected({ runner_id: "fresh", runner_name: "fresh-runner" });
    const next = useRunnerStore.getState().runners;
    expect(next).toHaveLength(1);
    expect(next[0]).toMatchObject({
      id: "fresh",
      name: "fresh-runner",
      status: "online",
    });
  });

  it("applyConnected promotes mode from unknown to saas", () => {
    // A runner connecting mid-bootstrap (before /api/runners returned) is
    // unambiguous evidence this is a SaaS deployment — patch the discriminator
    // so the indicator can flip without waiting for a refetch.
    useRunnerStore.setState({
      mode: "unknown",
      loaded: false,
      runners: [],
    });
    useRunnerStore
      .getState()
      .applyConnected({ runner_id: "r1", runner_name: "primary" });
    expect(useRunnerStore.getState().mode).toBe("saas");
  });

  it("applyDisconnected flips a tracked row to offline", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1", status: "online" })],
    });
    useRunnerStore.getState().applyDisconnected({ runner_id: "r1" });
    expect(useRunnerStore.getState().runners[0].status).toBe("offline");
  });

  it("applyDisconnected ignores unknown runner ids", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1", status: "online" })],
    });
    useRunnerStore.getState().applyDisconnected({ runner_id: "ghost" });
    // Existing row is untouched, no synthetic ghost row added.
    expect(useRunnerStore.getState().runners).toHaveLength(1);
    expect(useRunnerStore.getState().runners[0].status).toBe("online");
  });

  it("reset returns the store to its initial shape", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner()],
      lastRunnersFetchedAt: 12345,
    });
    useRunnerStore.getState().reset();
    const s = useRunnerStore.getState();
    expect(s.mode).toBe("unknown");
    expect(s.runners).toEqual([]);
    expect(s.loaded).toBe(false);
    expect(s.lastRunnersFetchedAt).toBeNull();
  });
});
