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
    useRunnerStore.getState().applyConnected({ runner_id: "r1", runner_name: "primary" });
    const next = useRunnerStore.getState().runners;
    expect(next).toHaveLength(1);
    expect(next[0].status).toBe("online");
    expect(next[0].name).toBe("primary");
  });

  it("applyConnected synthesizes a row when the runner is unknown", () => {
    // WS event for a runner the dashboard hasn't refetched yet — the
    // indicator must still flip emerald, the missing fields fill in later.
    useRunnerStore.setState({ mode: "saas", loaded: true, runners: [] });
    useRunnerStore.getState().applyConnected({ runner_id: "fresh", runner_name: "fresh-runner" });
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
    useRunnerStore.getState().applyConnected({ runner_id: "r1", runner_name: "primary" });
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
          return new Response(JSON.stringify({ token: "cafef00d", runner_name: "laptop" }), {
            status: 201,
            headers: { "Content-Type": "application/json" },
          });
        }
        // Warmup GET /api/runners — return an empty list so the
        // refetch lands without erroring.
        return new Response(JSON.stringify({ runners: [], mode: "saas" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }),
    );
    const issued = await useRunnerStore.getState().createRunnerToken("laptop");
    expect(issued).toEqual({ token: "cafef00d", runner_name: "laptop" });
    expect(tokenPostBody).toEqual({ runner_name: "laptop" });
  });

  it("createRunnerToken propagates server errors so the modal can render them inline", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ error: "name_taken" }), {
            status: 409,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );
    await expect(useRunnerStore.getState().createRunnerToken("dup")).rejects.toThrow();
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

  it("applyConnected auto-selects the first connecting runner when no selection is set", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [],
      selectedRunnerId: null,
    });
    useRunnerStore.getState().applyConnected({ runner_id: "r1", runner_name: "primary" });
    expect(useRunnerStore.getState().selectedRunnerId).toBe("r1");
  });

  it("applyConnected does not override an existing selection", () => {
    // User explicitly chose r2 earlier; a new r1 connecting later must
    // not steal focus.
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [],
      selectedRunnerId: "r2",
    });
    useRunnerStore.getState().applyConnected({ runner_id: "r1", runner_name: "secondary" });
    expect(useRunnerStore.getState().selectedRunnerId).toBe("r2");
  });

  it("applyDriversTouch caches the typed driver list when one is supplied", () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1" })],
      driversByRunnerId: {},
    });
    useRunnerStore.getState().applyDriversTouch({
      runner_id: "r1",
      drivers: [
        { name: "claude", status: { state: "api_key" } },
        { name: "aider", status: { state: "not_installed" } },
      ],
    });
    const map = useRunnerStore.getState().driversByRunnerId;
    expect(map.r1).toHaveLength(2);
    expect(map.r1[0]).toMatchObject({
      name: "claude",
      status: { state: "api_key" },
    });
  });

  it("setSelectedRunnerId updates the slot (legacy 4.1 listeners need not refetch immediately)", () => {
    // The Sidebar effect refetches /api/drivers on the next render —
    // this test only pins the store transition.
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1" })],
      selectedRunnerId: "r1",
    });
    useRunnerStore.getState().setSelectedRunnerId("r2");
    expect(useRunnerStore.getState().selectedRunnerId).toBe("r2");
  });

  it("fetchRunners seeds driversByRunnerId from row payload", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              runners: [
                {
                  id: "r1",
                  name: "primary",
                  status: "online",
                  hostname: "h",
                  version: "1.0",
                  lastSeenAt: null,
                  createdAt: null,
                  drivers: [{ name: "claude", status: { state: "api_key" } }],
                },
              ],
              mode: "saas",
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );
    await useRunnerStore.getState().fetchRunners();
    const s = useRunnerStore.getState();
    expect(s.driversByRunnerId.r1).toEqual([{ name: "claude", status: { state: "api_key" } }]);
    // First runner becomes the default selection.
    expect(s.selectedRunnerId).toBe("r1");
  });

  // ── per-runner config (4.7) ───────────────────────────────────────────

  it("fetchRunnerConfig caches the response under configByRunnerId", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              runnerId: "r1",
              effort: "high",
              skipPermissions: false,
              override: { effort: null, skipPermissions: null },
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );
    const cfg = await useRunnerStore.getState().fetchRunnerConfig("r1");
    expect(cfg.effort).toBe("high");
    expect(cfg.skipPermissions).toBe(false);
    expect(useRunnerStore.getState().configByRunnerId.r1).toEqual(cfg);
  });

  it("saveRunnerConfig forwards null to clear an override", async () => {
    let captured: unknown = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo, init?: RequestInit) => {
        captured = JSON.parse(String(init?.body ?? "{}"));
        return new Response(
          JSON.stringify({
            runnerId: "r1",
            effort: "high",
            skipPermissions: false,
            override: { effort: null, skipPermissions: null },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    await useRunnerStore.getState().saveRunnerConfig("r1", { effort: null, skipPermissions: null });
    expect(captured).toEqual({ effort: null, skip_permissions: null });
  });

  it("saveRunnerConfig only sends fields the caller mentioned", async () => {
    // Acceptance: PUT body must not include keys the caller didn't pass —
    // the server treats missing keys as "no change", and we don't want a
    // partial PUT to clobber the unspecified column.
    let captured: Record<string, unknown> | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_input: RequestInfo, init?: RequestInit) => {
        captured = JSON.parse(String(init?.body ?? "{}"));
        return new Response(
          JSON.stringify({
            runnerId: "r1",
            effort: "max",
            skipPermissions: false,
            override: { effort: "max", skipPermissions: null },
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    await useRunnerStore.getState().saveRunnerConfig("r1", { effort: "max" });
    expect(captured).toEqual({ effort: "max" });
    expect(captured).not.toHaveProperty("skip_permissions");
  });
});
