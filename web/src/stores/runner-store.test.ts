import { afterEach, describe, expect, it, vi } from "vitest";
import {
  OUTBOX_DEPTH_RED_THRESHOLD,
  RUNNER_HEALTH_HISTORY_CAP,
  RUNNER_LOG_RING_CAP,
  WS_RECONNECTS_RED_THRESHOLD,
  useRunnerStore,
  worstHealthChip,
  type Runner,
} from "./runner-store.js";

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

  // ── Runner log ring (T11.1) ────────────────────────────────────────────

  it("pushRunnerLog appends to the per-runner ring with monotonic ids", () => {
    const push = useRunnerStore.getState().pushRunnerLog;
    push("r1", { ts: "2026-05-07T00:00:00.000Z", level: "info", line: "first" });
    push("r1", { ts: "2026-05-07T00:00:01.000Z", level: "warn", line: "second" });
    const ring = useRunnerStore.getState().logsByRunnerId["r1"];
    expect(ring).toHaveLength(2);
    expect(ring.map((e) => e.line)).toEqual(["first", "second"]);
    // Monotonic ids let React keys stay stable when the ring evicts.
    expect(ring[0].id).toBeLessThan(ring[1].id);
  });

  it("pushRunnerLog keeps separate rings per runner", () => {
    const push = useRunnerStore.getState().pushRunnerLog;
    push("r1", { ts: "t", level: "info", line: "one" });
    push("r2", { ts: "t", level: "info", line: "two" });
    const state = useRunnerStore.getState();
    expect(state.logsByRunnerId["r1"]?.map((e) => e.line)).toEqual(["one"]);
    expect(state.logsByRunnerId["r2"]?.map((e) => e.line)).toEqual(["two"]);
  });

  it("pushRunnerLog drops oldest entries when the cap is hit (FIFO)", () => {
    // Acceptance: the in-memory ring is bounded so a long-running runner
    // can't OOM the dashboard tab. Pushes beyond the cap drop the oldest
    // line and slide the window forward.
    const push = useRunnerStore.getState().pushRunnerLog;
    for (let i = 0; i < RUNNER_LOG_RING_CAP + 5; i++) {
      push("r1", { ts: "t", level: "info", line: `line-${i}` });
    }
    const ring = useRunnerStore.getState().logsByRunnerId["r1"];
    expect(ring).toHaveLength(RUNNER_LOG_RING_CAP);
    // Oldest 5 must have been evicted; newest line is at the tail.
    expect(ring[0].line).toBe("line-5");
    expect(ring[ring.length - 1].line).toBe(`line-${RUNNER_LOG_RING_CAP + 4}`);
  });

  it("pushRunnerLog forwards the truncated marker line verbatim", () => {
    // The runner emits a synthetic "[truncated N lines]" line as a real
    // RunnerLogLine when its 200/sec cap fires; the dashboard ring stores
    // it like any other entry so the panel renders it inline.
    const push = useRunnerStore.getState().pushRunnerLog;
    push("r1", { ts: "t", level: "warn", line: "[truncated 42 lines]" });
    const ring = useRunnerStore.getState().logsByRunnerId["r1"];
    expect(ring).toHaveLength(1);
    expect(ring[0].line).toBe("[truncated 42 lines]");
    expect(ring[0].level).toBe("warn");
  });

  it("reset clears every per-runner log ring", () => {
    const push = useRunnerStore.getState().pushRunnerLog;
    push("r1", { ts: "t", level: "info", line: "one" });
    push("r2", { ts: "t", level: "info", line: "two" });
    useRunnerStore.getState().reset();
    expect(useRunnerStore.getState().logsByRunnerId).toEqual({});
  });

  it("requestRunnerShutdown POSTs to /shutdown with reason+confirm and stamps shutdownRequestedAt", async () => {
    let postBody: unknown = null;
    let postPath: string | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo, init?: RequestInit) => {
        const path = typeof input === "string" ? input : (input as Request).url;
        postPath = path;
        postBody = JSON.parse(String(init?.body ?? "{}"));
        return new Response(
          JSON.stringify({ queued: true, seq: 7, inFlightAgents: ["a-1", "a-2"] }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }),
    );
    const before = Date.now();
    const resp = await useRunnerStore
      .getState()
      .requestRunnerShutdown("runner-77", { reason: "host upgrade", confirmInFlight: true });
    expect(resp).toEqual({ queued: true, seq: 7, inFlightAgents: ["a-1", "a-2"] });
    expect(postPath).toMatch(/\/api\/runners\/runner-77\/shutdown$/);
    expect(postBody).toEqual({ reason: "host upgrade", confirmInFlight: true });
    const stamp = useRunnerStore.getState().shutdownRequestedAt["runner-77"];
    expect(stamp).toBeGreaterThanOrEqual(before);
  });

  it("revokeRunner DELETEs /api/runners/{id} and removes the row from runners", async () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1" }), seedRunner({ id: "r2", name: "secondary" })],
      driversByRunnerId: { r1: [], r2: [] },
      shutdownRequestedAt: { r1: 1234 },
      configByRunnerId: {
        r1: {
          runnerId: "r1",
          effort: "high",
          skipPermissions: false,
          override: { effort: null, skipPermissions: null },
        },
      },
    });
    let calledMethod: string | null = null;
    let calledPath: string | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo, init?: RequestInit) => {
        calledMethod = init?.method ?? "GET";
        calledPath = typeof input === "string" ? input : (input as Request).url;
        return new Response(JSON.stringify({ revoked: true, tokensRevoked: 1 }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }),
    );
    const resp = await useRunnerStore.getState().revokeRunner("r1");
    expect(resp).toEqual({ revoked: true, tokensRevoked: 1 });
    expect(calledMethod).toBe("DELETE");
    expect(calledPath).toMatch(/\/api\/runners\/r1$/);
    const s = useRunnerStore.getState();
    expect(s.runners.map((r) => r.id)).toEqual(["r2"]);
    expect(s.driversByRunnerId).not.toHaveProperty("r1");
    expect(s.shutdownRequestedAt).not.toHaveProperty("r1");
    expect(s.configByRunnerId).not.toHaveProperty("r1");
  });

  it("revokeRunner reassigns selectedRunnerId when the revoked runner was selected", async () => {
    useRunnerStore.setState({
      mode: "saas",
      loaded: true,
      runners: [seedRunner({ id: "r1" }), seedRunner({ id: "r2", name: "second" })],
      selectedRunnerId: "r1",
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify({ revoked: true, tokensRevoked: 1 }), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );
    await useRunnerStore.getState().revokeRunner("r1");
    expect(useRunnerStore.getState().selectedRunnerId).toBe("r2");
  });

  it("applyDisconnected clears shutdownRequestedAt so the 30s fallback does not fire", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1" })],
      shutdownRequestedAt: { r1: Date.now() - 60_000 },
    });
    useRunnerStore.getState().applyDisconnected({ runner_id: "r1" });
    expect(useRunnerStore.getState().shutdownRequestedAt).not.toHaveProperty("r1");
    expect(useRunnerStore.getState().runners[0].status).toBe("offline");
  });

  // ── T11.3 health metrics ────────────────────────────────────────────────

  it("applyHealth patches the runner row + appends to the history ring", () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1" })],
    });
    useRunnerStore.getState().applyHealth({
      runner_id: "r1",
      outbox_depth: 7,
      ws_reconnects_24h: 2,
      ci_poll_ms_p50: 800,
      ci_poll_ms_p99: 1200,
      version_mismatch: "ok",
    });
    const s = useRunnerStore.getState();
    expect(s.runners[0].health).toMatchObject({
      outboxDepth: 7,
      wsReconnects24h: 2,
      ciPollMsP50: 800,
      ciPollMsP99: 1200,
    });
    expect(s.runners[0].versionMismatch).toBe("ok");
    expect(s.healthHistoryByRunnerId.r1).toHaveLength(1);
    expect(s.healthHistoryByRunnerId.r1[0]).toMatchObject({
      p50: 800,
      p99: 1200,
    });
  });

  it("applyHealth still records history when the runner row is unknown", () => {
    // Health frame can land before fetchRunners has the row (race during
    // bootstrap). The history ring keys by runner id, so we keep the
    // sparkline data and the panel can render it once the row appears.
    useRunnerStore.setState({ runners: [] });
    useRunnerStore.getState().applyHealth({
      runner_id: "rGhost",
      outbox_depth: 0,
      ws_reconnects_24h: 0,
      ci_poll_ms_p50: null,
      ci_poll_ms_p99: null,
      version_mismatch: "ok",
    });
    expect(useRunnerStore.getState().healthHistoryByRunnerId.rGhost).toHaveLength(1);
    expect(useRunnerStore.getState().runners).toHaveLength(0);
  });

  it("applyHealth coerces unknown version_mismatch values to ok (forward-compat)", () => {
    useRunnerStore.setState({ runners: [seedRunner({ id: "r1" })] });
    useRunnerStore.getState().applyHealth({
      runner_id: "r1",
      outbox_depth: 0,
      ws_reconnects_24h: 0,
      ci_poll_ms_p50: null,
      ci_poll_ms_p99: null,
      // Server adds a new verdict; client should not crash.
      version_mismatch: "future_verdict",
    });
    expect(useRunnerStore.getState().runners[0].versionMismatch).toBe("ok");
  });

  it("applyHealth caps the per-runner history ring at RUNNER_HEALTH_HISTORY_CAP", () => {
    useRunnerStore.setState({ runners: [seedRunner({ id: "r1" })] });
    for (let i = 0; i < RUNNER_HEALTH_HISTORY_CAP + 5; i++) {
      useRunnerStore.getState().applyHealth({
        runner_id: "r1",
        outbox_depth: 0,
        ws_reconnects_24h: 0,
        ci_poll_ms_p50: i,
        ci_poll_ms_p99: i,
        version_mismatch: "ok",
      });
    }
    const ring = useRunnerStore.getState().healthHistoryByRunnerId.r1;
    expect(ring).toHaveLength(RUNNER_HEALTH_HISTORY_CAP);
    // First surviving sample is index 5 (the first 5 evicted FIFO).
    expect(ring[0].p50).toBe(5);
  });

  it("worstHealthChip flags major version mismatch as danger", () => {
    const chip = worstHealthChip(
      seedRunner({ id: "r1", version: "0.2.x", versionMismatch: "major" }),
    );
    expect(chip.severity).toBe("danger");
    expect(chip.label).toContain("0.2.x");
  });

  it("worstHealthChip flags outbox saturation past the threshold", () => {
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: OUTBOX_DEPTH_RED_THRESHOLD + 42,
          wsReconnects24h: 0,
          ciPollMsP50: null,
          ciPollMsP99: null,
          lastHealthAt: null,
          orphansReaped24h: null,
        },
      }),
    );
    expect(chip.severity).toBe("danger");
    expect(chip.label).toContain("142");
  });

  it("worstHealthChip flags reconnect storms past the threshold as danger", () => {
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: 0,
          wsReconnects24h: WS_RECONNECTS_RED_THRESHOLD + 1,
          ciPollMsP50: null,
          ciPollMsP99: null,
          lastHealthAt: null,
          orphansReaped24h: null,
        },
      }),
    );
    expect(chip.severity).toBe("danger");
    expect(chip.label).toContain("reconnects");
  });

  it("worstHealthChip surfaces minor version mismatch as warn (not danger)", () => {
    const chip = worstHealthChip(
      seedRunner({ id: "r1", version: "0.4.0", versionMismatch: "minor" }),
    );
    expect(chip.severity).toBe("warn");
  });

  it("worstHealthChip returns severity:none when nothing crosses a threshold", () => {
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: 5,
          wsReconnects24h: 1,
          ciPollMsP50: 100,
          ciPollMsP99: 200,
          lastHealthAt: null,
          orphansReaped24h: 0,
        },
      }),
    );
    expect(chip.severity).toBe("none");
    expect(chip.label).toBe("");
  });

  it("worstHealthChip returns severity:none when patch mismatch alone — chip suppresses", () => {
    // Patch differences are not actionable on their own; the chip stays
    // off, the Health panel surfaces them as a neutral note.
    const chip = worstHealthChip(
      seedRunner({ id: "r1", version: "0.3.4", versionMismatch: "patch" }),
    );
    expect(chip.severity).toBe("none");
  });

  it("worstHealthChip flags one orphan reaped as warn (Task 11.6)", () => {
    // A single reap is the canonical "silent rot happened, but cleanup
    // worked" signal. Surface as warn — operator should know it
    // happened but it is not a steady-state failure.
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: 0,
          wsReconnects24h: 0,
          ciPollMsP50: null,
          ciPollMsP99: null,
          lastHealthAt: null,
          orphansReaped24h: 1,
        },
      }),
    );
    expect(chip.severity).toBe("warn");
    expect(chip.label).toContain("1 orphan reaped");
  });

  it("worstHealthChip escalates to danger past the orphan-reap threshold", () => {
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: 0,
          wsReconnects24h: 0,
          ciPollMsP50: null,
          ciPollMsP99: null,
          lastHealthAt: null,
          orphansReaped24h: 12,
        },
      }),
    );
    expect(chip.severity).toBe("danger");
    expect(chip.label).toContain("12 orphans reaped");
  });

  it("worstHealthChip ignores null orphans count (older runner, no signal)", () => {
    // Pre-Task-11.6 runners don't ship the field; the store coerces
    // missing values to null and the chip suppresses entirely.
    const chip = worstHealthChip(
      seedRunner({
        id: "r1",
        versionMismatch: "ok",
        health: {
          outboxDepth: 0,
          wsReconnects24h: 0,
          ciPollMsP50: null,
          ciPollMsP99: null,
          lastHealthAt: null,
          orphansReaped24h: null,
        },
      }),
    );
    expect(chip.severity).toBe("none");
  });

  // ── T1.3: version severity + override ─────────────────────────────────

  it("applyHealth stores versionSeverity + versionMismatchOverride", () => {
    useRunnerStore.setState({ runners: [seedRunner({ id: "r1" })] });
    useRunnerStore.getState().applyHealth({
      runner_id: "r1",
      outbox_depth: 0,
      ws_reconnects_24h: 0,
      ci_poll_ms_p50: null,
      ci_poll_ms_p99: null,
      version_mismatch: "minor",
      version_severity: "red",
      version_mismatch_override: true,
    });
    const r = useRunnerStore.getState().runners[0];
    expect(r.versionMismatch).toBe("minor");
    expect(r.versionSeverity).toBe("red");
    expect(r.versionMismatchOverride).toBe(true);
  });

  it("applyHealth coerces unknown version_severity values to green (forward-compat)", () => {
    useRunnerStore.setState({ runners: [seedRunner({ id: "r1" })] });
    useRunnerStore.getState().applyHealth({
      runner_id: "r1",
      outbox_depth: 0,
      ws_reconnects_24h: 0,
      ci_poll_ms_p50: null,
      ci_poll_ms_p99: null,
      version_mismatch: "ok",
      version_severity: "future_bucket",
    });
    expect(useRunnerStore.getState().runners[0].versionSeverity).toBe("green");
  });

  it("applyHealth preserves existing override when payload omits it", () => {
    useRunnerStore.setState({
      runners: [
        seedRunner({
          id: "r1",
          versionMismatchOverride: true,
        }),
      ],
    });
    useRunnerStore.getState().applyHealth({
      runner_id: "r1",
      outbox_depth: 0,
      ws_reconnects_24h: 0,
      ci_poll_ms_p50: null,
      ci_poll_ms_p99: null,
      version_mismatch: "ok",
      // No version_mismatch_override field — older server build.
    });
    expect(useRunnerStore.getState().runners[0].versionMismatchOverride).toBe(true);
  });

  it("setVersionMismatchOverride POSTs to /version-mismatch-override and patches the row", async () => {
    useRunnerStore.setState({
      runners: [seedRunner({ id: "r1", versionMismatchOverride: false })],
    });
    let captured: { url: string; method: string; body: unknown } | null = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        captured = {
          url,
          method: init.method ?? "GET",
          body: init.body ? JSON.parse(init.body as string) : null,
        };
        return new Response(JSON.stringify({ runnerId: "r1", override: true }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }),
    );
    const resp = await useRunnerStore.getState().setVersionMismatchOverride("r1", true);
    expect(resp).toEqual({ runnerId: "r1", override: true });
    expect(captured).not.toBeNull();
    expect(captured!.url).toBe("/api/runners/r1/version-mismatch-override");
    expect(captured!.method).toBe("POST");
    expect(captured!.body).toEqual({ override: true });
    expect(useRunnerStore.getState().runners[0].versionMismatchOverride).toBe(true);
  });

  it("fetchRunners populates agentCounts + serverVersion", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              runners: [
                {
                  id: "r1",
                  name: "p",
                  status: "online",
                  hostname: "h",
                  version: "0.3.0",
                  lastSeenAt: null,
                  createdAt: null,
                },
              ],
              mode: "saas",
              agentCounts: { running: 2, completed: 5, failed: 1 },
              serverVersion: "0.3.0",
            }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );
    await useRunnerStore.getState().fetchRunners();
    const s = useRunnerStore.getState();
    expect(s.agentCounts).toEqual({ running: 2, completed: 5, failed: 1 });
    expect(s.serverVersion).toBe("0.3.0");
  });
});
