import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  getInFlightDriversFetch,
  getInFlightSettingsFetch,
  isDriverReady,
  useSettingsStore,
  type AuthStatus,
  type DriverInfo,
} from "./settings-store.js";
import { useAuthStore } from "./auth-store.js";

interface FetchResult {
  status: number;
  body?: unknown;
  statusText?: string;
}

type FetchHandler = (
  url: string,
  method: string,
  body: unknown,
) => FetchResult | Promise<FetchResult>;

interface CallLog {
  url: string;
  method: string;
  body: unknown;
}

function installFetchMock(handler: FetchHandler) {
  const calls: CallLog[] = [];
  const fn = vi.fn(
    async (input: string | URL | Request, init?: RequestInit) => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.pathname + input.search
            : input.url;
      const method = init?.method ?? "GET";
      const rawBody = init?.body;
      const body =
        typeof rawBody === "string" && rawBody.length > 0
          ? JSON.parse(rawBody)
          : undefined;
      calls.push({ url, method, body });
      const result = await handler(url, method, body);
      return new Response(
        result.body !== undefined ? JSON.stringify(result.body) : null,
        {
          status: result.status,
          statusText: result.statusText,
          headers: { "Content-Type": "application/json" },
        },
      );
    },
  );
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

beforeEach(() => {
  // Seed a non-null user so api.ts's 401 interceptor doesn't trip on
  // failed-test fetches. Kept symmetric with agent-store/auth-store tests.
  useAuthStore.setState({
    user: { id: "u1", email: "u@example.com" },
    error: null,
    loading: false,
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  useSettingsStore.getState().reset();
  useAuthStore.setState({ user: null, error: null, loading: false });
});

function makeDriver(overrides: Partial<DriverInfo> = {}): DriverInfo {
  return {
    name: "claude",
    binary: "/usr/bin/claude",
    capabilities: {
      supports_cost: true,
      supports_verdict: true,
      supports_session_id: true,
      interactive_only: false,
    },
    auth_status: { kind: "oauth", account: "user@example.com" },
    ...overrides,
  };
}

describe("settings-store initial state", () => {
  it("starts with the expected slice shape", () => {
    const s = useSettingsStore.getState();
    expect(s.effort).toBe("high");
    expect(s.skipPermissions).toBe(true);
    expect(s.webhookUrl).toBeNull();
    expect(s.planArchiveRetentionDays).toBe(30);
    expect(s.loaded).toBe(false);
    expect(s.drivers).toEqual([]);
    expect(s.defaultDriver).toBe("claude");
    expect(s.driversRunnerId).toBeNull();
    expect(s.driversRunnerStatus).toBe("local");
    expect(s.lastSettingsFetchedAt).toBeNull();
    expect(s.lastDriversFetchedAt).toBeNull();
    expect(getInFlightSettingsFetch()).toBeNull();
    expect(getInFlightDriversFetch()).toBeNull();
  });
});

describe("settings-store mutations (PUT setters)", () => {
  it("setEffort PUTs /api/settings and updates the local slice", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useSettingsStore.getState().setEffort("low");

    expect(useSettingsStore.getState().effort).toBe("low");
    expect(calls).toEqual([
      { url: "/api/settings", method: "PUT", body: { effort: "low" } },
    ]);
  });

  it("setSkipPermissions PUTs /api/settings and updates the local slice", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useSettingsStore.getState().setSkipPermissions(false);

    expect(useSettingsStore.getState().skipPermissions).toBe(false);
    expect(calls).toEqual([
      {
        url: "/api/settings",
        method: "PUT",
        body: { skip_permissions: false },
      },
    ]);
  });

  it("setWebhookUrl PUTs /api/settings (with null to clear)", async () => {
    useSettingsStore.setState({ webhookUrl: "https://prev.example/" });
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useSettingsStore.getState().setWebhookUrl(null);

    expect(useSettingsStore.getState().webhookUrl).toBeNull();
    expect(calls).toEqual([
      { url: "/api/settings", method: "PUT", body: { webhook_url: null } },
    ]);
  });

  it("setPlanArchiveRetentionDays PUTs /api/settings (snake_case wire field)", async () => {
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useSettingsStore.getState().setPlanArchiveRetentionDays(7);

    expect(useSettingsStore.getState().planArchiveRetentionDays).toBe(7);
    expect(calls).toEqual([
      {
        url: "/api/settings",
        method: "PUT",
        body: { plan_archive_retention_days: 7 },
      },
    ]);
  });
});

describe("settings-store fetchers (stubbed fetch)", () => {
  it("fetchSettings hydrates effort/skipPermissions/webhookUrl/retention/loaded", async () => {
    const before = Date.now();
    installFetchMock(() => ({
      status: 200,
      body: {
        effort: "medium",
        skip_permissions: false,
        webhook_url: "https://hook.example",
        plan_archive_retention_days: 14,
      },
    }));

    await useSettingsStore.getState().fetchSettings();

    const s = useSettingsStore.getState();
    expect(s.effort).toBe("medium");
    expect(s.skipPermissions).toBe(false);
    expect(s.webhookUrl).toBe("https://hook.example");
    expect(s.planArchiveRetentionDays).toBe(14);
    expect(s.loaded).toBe(true);
    expect(s.lastSettingsFetchedAt).not.toBeNull();
    expect(s.lastSettingsFetchedAt!).toBeGreaterThanOrEqual(before);
  });

  it("fetchSettings tolerates a server that omits plan_archive_retention_days (falls back to 30)", async () => {
    installFetchMock(() => ({
      status: 200,
      body: {
        effort: "high",
        skip_permissions: true,
        webhook_url: null,
      },
    }));

    await useSettingsStore.getState().fetchSettings();

    expect(useSettingsStore.getState().planArchiveRetentionDays).toBe(30);
  });

  it("fetchSettings coalesces concurrent callers onto a single round trip", async () => {
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: {
        effort: "high",
        skip_permissions: true,
        webhook_url: null,
      },
    }));

    const a = useSettingsStore.getState().fetchSettings();
    const b = useSettingsStore.getState().fetchSettings();
    expect(a).toBe(b);
    expect(getInFlightSettingsFetch()).toBe(a);
    await Promise.all([a, b]);
    expect(calls.filter((c) => c.url === "/api/settings")).toHaveLength(1);
    expect(getInFlightSettingsFetch()).toBeNull();
  });

  it("fetchDrivers (no runner_id) hits /api/drivers and fills drivers/defaultDriver/runnerStatus", async () => {
    const drivers = [makeDriver({ name: "claude" }), makeDriver({ name: "aider" })];
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: { drivers, default: "claude", runner_status: "local" },
    }));

    await useSettingsStore.getState().fetchDrivers();

    const s = useSettingsStore.getState();
    expect(s.drivers).toEqual(drivers);
    expect(s.defaultDriver).toBe("claude");
    expect(s.driversRunnerId).toBeNull();
    expect(s.driversRunnerStatus).toBe("local");
    expect(s.lastDriversFetchedAt).not.toBeNull();
    expect(calls[0].url).toBe("/api/drivers");
  });

  it("fetchDrivers (runner_id) URL-encodes and surfaces server runner_id/status", async () => {
    const drivers = [makeDriver({ name: "claude" })];
    const { calls } = installFetchMock(() => ({
      status: 200,
      body: {
        drivers,
        default: "claude",
        runner_id: "runner abc",
        runner_status: "online",
      },
    }));

    await useSettingsStore.getState().fetchDrivers("runner abc");

    const s = useSettingsStore.getState();
    expect(s.driversRunnerId).toBe("runner abc");
    expect(s.driversRunnerStatus).toBe("online");
    // %20 is the URL-encoded space → confirms encodeURIComponent.
    expect(calls[0].url).toBe("/api/drivers?runner_id=runner%20abc");
  });

  it("fetchDrivers stale-guard: a runner switch mid-flight discards the older response", async () => {
    // Resolve order: A's request fires -> B's request fires -> B resolves
    // first -> A resolves last. The stale guard MUST drop A's set() so the
    // sidebar reflects B (the runner the user just switched to).
    let resolveA: (r: FetchResult) => void = () => {};
    let resolveB: (r: FetchResult) => void = () => {};

    installFetchMock((url) => {
      if (url.includes("runner_id=A")) {
        return new Promise<FetchResult>((res) => {
          resolveA = res;
        });
      }
      return new Promise<FetchResult>((res) => {
        resolveB = res;
      });
    });

    const aPromise = useSettingsStore.getState().fetchDrivers("A");
    const bPromise = useSettingsStore.getState().fetchDrivers("B");
    // Same in-flight slot; cancel-replaced (different runner id ≠ same id).
    expect(aPromise).not.toBe(bPromise);

    // Resolve B first. Its result must land on the slice.
    resolveB({
      status: 200,
      body: {
        drivers: [makeDriver({ name: "claude" })],
        default: "claude",
        runner_id: "B",
        runner_status: "online",
      },
    });
    await bPromise;
    expect(useSettingsStore.getState().driversRunnerId).toBe("B");

    // A resolves later; the stale guard must DROP it (`driversRunnerId` stays "B").
    resolveA({
      status: 200,
      body: {
        drivers: [makeDriver({ name: "aider" })],
        default: "aider",
        runner_id: "A",
        runner_status: "offline",
      },
    });
    await aPromise;
    expect(useSettingsStore.getState().driversRunnerId).toBe("B");
    expect(useSettingsStore.getState().defaultDriver).toBe("claude");
  });
});

describe("settings-store selectors", () => {
  it("driverCapabilities returns the named driver's profile when present", () => {
    useSettingsStore.setState({
      drivers: [
        makeDriver({
          name: "aider",
          capabilities: {
            supports_cost: false,
            supports_verdict: false,
            supports_session_id: false,
            interactive_only: true,
          },
        }),
        makeDriver({ name: "claude" }),
      ],
      defaultDriver: "claude",
    });

    const caps = useSettingsStore.getState().driverCapabilities("aider");
    expect(caps.supports_cost).toBe(false);
    expect(caps.interactive_only).toBe(true);
  });

  it("driverCapabilities falls back to the default driver when the name is unknown", () => {
    useSettingsStore.setState({
      drivers: [makeDriver({ name: "claude" })],
      defaultDriver: "claude",
    });

    const caps = useSettingsStore.getState().driverCapabilities("does-not-exist");
    expect(caps.supports_cost).toBe(true);
    expect(caps.supports_verdict).toBe(true);
  });

  it("driverCapabilities falls back to DEFAULT_CAPABILITIES when drivers list is empty", () => {
    // No drivers loaded yet — UI should show all-features rather than
    // hide columns. Mirrors `DriverCapabilities::default` on the Rust side.
    const caps = useSettingsStore.getState().driverCapabilities("claude");
    expect(caps).toEqual({
      supports_cost: true,
      supports_verdict: true,
      supports_session_id: true,
      interactive_only: false,
    });
  });

  it("driverAuth returns undefined when /api/drivers hasn't loaded yet", () => {
    expect(useSettingsStore.getState().driverAuth("claude")).toBeUndefined();
  });

  it("driverAuth returns the matching driver's auth_status", () => {
    const auth: AuthStatus = { kind: "api_key" };
    useSettingsStore.setState({
      drivers: [makeDriver({ name: "claude", auth_status: auth })],
      defaultDriver: "claude",
    });
    expect(useSettingsStore.getState().driverAuth("claude")).toEqual(auth);
  });

  it("isDriverReady (free fn) maps each AuthStatus variant to the right gate", () => {
    expect(isDriverReady(undefined)).toBe(true);
    expect(isDriverReady({ kind: "oauth", account: null })).toBe(true);
    expect(isDriverReady({ kind: "api_key" })).toBe(true);
    expect(
      isDriverReady({ kind: "cloud_provider", provider: "bedrock" }),
    ).toBe(true);
    expect(isDriverReady({ kind: "unknown" })).toBe(true);
    expect(isDriverReady({ kind: "not_installed" })).toBe(false);
    expect(
      isDriverReady({ kind: "unauthenticated", help: "log in" }),
    ).toBe(false);
  });
});

describe("settings-store reset()", () => {
  it("drops every slice back to its initial shape and clears both in-flight handles", () => {
    useSettingsStore.setState({
      effort: "low",
      skipPermissions: false,
      webhookUrl: "https://hook.example",
      planArchiveRetentionDays: 7,
      loaded: true,
      drivers: [makeDriver({ name: "claude" })],
      defaultDriver: "aider",
      driversRunnerId: "runner-x",
      driversRunnerStatus: "online",
      lastSettingsFetchedAt: 12345,
      lastDriversFetchedAt: 12345,
    });

    useSettingsStore.getState().reset();

    const s = useSettingsStore.getState();
    expect(s.effort).toBe("high");
    expect(s.skipPermissions).toBe(true);
    expect(s.webhookUrl).toBeNull();
    expect(s.planArchiveRetentionDays).toBe(30);
    expect(s.loaded).toBe(false);
    expect(s.drivers).toEqual([]);
    expect(s.defaultDriver).toBe("claude");
    expect(s.driversRunnerId).toBeNull();
    expect(s.driversRunnerStatus).toBe("local");
    expect(s.lastSettingsFetchedAt).toBeNull();
    expect(s.lastDriversFetchedAt).toBeNull();
    expect(getInFlightSettingsFetch()).toBeNull();
    expect(getInFlightDriversFetch()).toBeNull();
  });
});

// Note: settings-store has no direct WS handlers — the only WS-driven path
// to mutate it is the ws-store reconnect sweep, which calls
// `fetchSettings()` + `fetchDrivers()` in `ws.onopen` when reconnect is
// detected. That cross-store interaction is exercised in
// ws-store.test.ts; here the fetcher coverage above is sufficient.
