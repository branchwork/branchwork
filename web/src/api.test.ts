import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fetchJson, HttpError, SessionExpiredError } from "./api.js";
import { useAgentStore } from "./stores/agent-store.js";
import { useAuthStore } from "./stores/auth-store.js";
import { usePlanStore, type PlanSummary } from "./stores/plan-store.js";
import { useSettingsStore } from "./stores/settings-store.js";
import { useToastStore } from "./stores/toast-store.js";

interface FetchResult {
  status: number;
  body?: unknown;
  statusText?: string;
}

type FetchHandler = (url: string, method: string) => FetchResult;

interface CallLog {
  url: string;
  method: string;
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
      calls.push({ url, method });
      const { status, body, statusText } = handler(url, method);
      return new Response(
        body !== undefined ? JSON.stringify(body) : null,
        {
          status,
          statusText,
          headers: { "Content-Type": "application/json" },
        },
      );
    },
  );
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

beforeEach(() => {
  useToastStore.getState().clear();
  useAuthStore.setState({
    user: { id: "u1", email: "u@example.com" },
    error: null,
    loading: false,
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  useToastStore.getState().clear();
  useAuthStore.setState({ user: null, error: null, loading: false });
});

describe("fetchJson session-expiry interceptor", () => {
  it("on 401, calls logout (clearing user), pushes 'Session expired' warn toast, and rejects with SessionExpiredError", async () => {
    const { calls } = installFetchMock(() => ({
      status: 401,
      body: { error: "unauthorized" },
    }));

    let caught: unknown = null;
    try {
      await fetchJson("/api/plans");
    } catch (e) {
      caught = e;
    }

    // (c) sentinel rejection so callers can distinguish from generic 401
    expect(caught).toBeInstanceOf(SessionExpiredError);

    // (a) auth store cleared — logout()'s `set({ user: null })` ran
    expect(useAuthStore.getState().user).toBeNull();

    // (b) Session-expired warn toast pushed
    const toasts = useToastStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].kind).toBe("warn");
    expect(toasts[0].title).toBe("Session expired — please sign in again");

    // Recursion guard: logout's own /api/auth/logout call is not
    // re-treated as session-expiry. Both fetches happen exactly once;
    // the inner one falls through to plain HttpError (which logout's
    // try/catch swallows) rather than re-entering the 401 branch.
    expect(calls.map((c) => c.url)).toEqual([
      "/api/plans",
      "/api/auth/logout",
    ]);
  });

  it("does not fire the session-expired toast on a 401 from the bootstrap probe (user is null)", async () => {
    // Cold visit: no user yet. /api/auth/me 401 should reject with a
    // plain HttpError (so auth-store.fetchMe can branch on it) and not
    // push a misleading toast.
    useAuthStore.setState({ user: null, error: null, loading: true });

    installFetchMock(() => ({ status: 401, body: { error: "unauthorized" } }));

    let caught: unknown = null;
    try {
      await fetchJson("/api/auth/me");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(HttpError);
    expect(caught).not.toBeInstanceOf(SessionExpiredError);
    expect((caught as HttpError).status).toBe(401);
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("on 403, keeps the user logged in, pushes an error toast, and rejects with HttpError (not SessionExpiredError)", async () => {
    const { calls } = installFetchMock(() => ({
      status: 403,
      body: { error: "forbidden" },
    }));

    let caught: unknown = null;
    try {
      await fetchJson("/api/admin/secret");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(HttpError);
    expect(caught).not.toBeInstanceOf(SessionExpiredError);
    expect((caught as HttpError).status).toBe(403);

    // User remains logged in — 403 is permission, not authentication.
    expect(useAuthStore.getState().user).not.toBeNull();

    const toasts = useToastStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].kind).toBe("error");
    // Title surfaces the server's `error` body field via errorMessage.
    expect(toasts[0].title).toBe("forbidden");

    // No logout side-effect: only the original request fired.
    expect(calls).toEqual([{ url: "/api/admin/secret", method: "GET" }]);
  });

  it("on a non-auth non-2xx (e.g. 500), rejects with HttpError without toasting or logging out", async () => {
    installFetchMock(() => ({
      status: 500,
      body: { error: "boom" },
      statusText: "Internal Server Error",
    }));

    let caught: unknown = null;
    try {
      await fetchJson("/api/plans");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(HttpError);
    expect((caught as HttpError).status).toBe(500);
    expect(useAuthStore.getState().user).not.toBeNull();
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  // Audit §2 acceptance: a 401 mid-session must drop the previous
  // user's plan/agent/settings state, not just `user`. Without
  // resetAllStores() in auth-store.logout(), user B would see user A's
  // plans flicker through after a session-expiry until App.tsx's
  // bootstrap fetches resolve.
  it(
    "on 401, also resets plan-store / agent-store / settings-store " +
      "(audit §2: prevent stale data leak across users in same tab)",
    async () => {
      // Seed user A's slices.
      const seedSummary: PlanSummary = {
        name: "user-a-plan",
        title: "User A",
        project: null,
        phaseCount: 0,
        taskCount: 0,
        doneCount: 0,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
      };
      usePlanStore.setState({
        plans: [seedSummary],
        plansFetched: true,
      });
      useAgentStore.setState({
        agents: [
          {
            id: "agent-a",
            session_id: "s",
            pid: null,
            parent_agent_id: null,
            plan_name: "user-a-plan",
            task_id: null,
            cwd: "/tmp",
            status: "running",
            mode: "pty",
            prompt: null,
            started_at: "2026-04-12T00:00:00Z",
            finished_at: null,
            last_tool: null,
            last_activity_at: null,
            base_commit: null,
            branch: null,
            source_branch: null,
            cost_usd: null,
            driver: null,
          },
        ],
        agentsFetched: true,
      });
      useSettingsStore.setState({
        effort: "low",
        webhookUrl: "https://user-a.example.com",
        loaded: true,
      });

      installFetchMock(() => ({
        status: 401,
        body: { error: "unauthorized" },
      }));

      try {
        await fetchJson("/api/plans");
      } catch {
        /* expected SessionExpiredError */
      }

      // Auth cleared (existing contract).
      expect(useAuthStore.getState().user).toBeNull();
      // Cross-store state cleared (new contract from task 3.1).
      expect(usePlanStore.getState().plans).toEqual([]);
      expect(usePlanStore.getState().plansFetched).toBe(false);
      expect(useAgentStore.getState().agents).toEqual([]);
      expect(useAgentStore.getState().agentsFetched).toBe(false);
      expect(useSettingsStore.getState().effort).toBe("high");
      expect(useSettingsStore.getState().webhookUrl).toBeNull();
      expect(useSettingsStore.getState().loaded).toBe(false);
    },
  );
});

describe("fetchJson X-Org-Id header (task 4.3)", () => {
  // Map-backed localStorage stand-in. Same shape as the org-store test
  // file uses — vi.unstubAllGlobals in afterEach restores jsdom's
  // (broken) default for the rest of the suite.
  function makeMockStorage(): Storage {
    const map = new Map<string, string>();
    return {
      get length() {
        return map.size;
      },
      clear: () => map.clear(),
      getItem: (k: string) => (map.has(k) ? (map.get(k) ?? null) : null),
      setItem: (k: string, v: string) => {
        map.set(k, String(v));
      },
      removeItem: (k: string) => {
        map.delete(k);
      },
      key: (i: number) => Array.from(map.keys())[i] ?? null,
    };
  }

  it("does NOT include X-Org-Id when no org pin is set in localStorage", async () => {
    vi.stubGlobal("localStorage", makeMockStorage());
    const fetchSpy = vi.fn(
      async () =>
        new Response("{}", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    await fetchJson("/api/plans");

    const call = fetchSpy.mock.calls[0] as unknown as [string, RequestInit];
    const init = call[1];
    const headers = init.headers as Record<string, string>;
    expect(headers["X-Org-Id"]).toBeUndefined();
    // Content-Type still rides every request.
    expect(headers["Content-Type"]).toBe("application/json");
  });

  it("sends X-Org-Id from localStorage on every request", async () => {
    const storage = makeMockStorage();
    storage.setItem("branchwork.activeOrgId", "org-pinned");
    vi.stubGlobal("localStorage", storage);

    const fetchSpy = vi.fn(
      async () =>
        new Response("{}", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    await fetchJson("/api/plans");

    const call = fetchSpy.mock.calls[0] as unknown as [string, RequestInit];
    const init = call[1];
    const headers = init.headers as Record<string, string>;
    expect(headers["X-Org-Id"]).toBe("org-pinned");
  });
});
