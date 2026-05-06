import { afterEach, describe, expect, it, vi } from "vitest";
import { useRunnerStore, type Runner } from "./runner-store.js";

afterEach(() => {
  useRunnerStore.getState().reset();
  vi.unstubAllGlobals();
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

  it("createRunnerToken POSTs runner_name and surfaces the issued token", async () => {
    // Capture the FIRST POST body — the store also fires a follow-up
    // GET /api/runners (warmup refetch) which would otherwise clobber
    // the captured body with the empty body of the GET.
    let tokenPostBody: unknown = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo, init?: RequestInit) => {
        const path = typeof input === "string" ? input : (input as Request).url;
        if (path.endsWith("/api/runners/tokens")) {
          tokenPostBody = JSON.parse(String(init?.body ?? "{}"));
          return new Response(
            JSON.stringify({ token: "cafef00d", runner_name: "laptop" }),
            { status: 201, headers: { "Content-Type": "application/json" } },
          );
        }
        // Warmup GET /api/runners — return an empty list so the
        // refetch lands without erroring.
        return new Response(
          JSON.stringify({ runners: [], mode: "saas" }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    const issued = await useRunnerStore
      .getState()
      .createRunnerToken("laptop");
    expect(issued).toEqual({ token: "cafef00d", runner_name: "laptop" });
    expect(tokenPostBody).toEqual({ runner_name: "laptop" });
  });

  it("createRunnerToken propagates server errors so the modal can render them inline", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        new Response(JSON.stringify({ error: "name_taken" }), {
          status: 409,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    await expect(
      useRunnerStore.getState().createRunnerToken("dup"),
    ).rejects.toThrow();
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
