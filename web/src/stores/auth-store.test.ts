import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useAuthStore } from "./auth-store.js";
import { useAgentStore } from "./agent-store.js";
import { usePlanStore, type PlanSummary } from "./plan-store.js";
import { useSettingsStore } from "./settings-store.js";
import { useToastStore } from "./toast-store.js";
import { HttpError, SessionExpiredError } from "../api.js";

interface FetchResult {
  status: number;
  body?: unknown;
  statusText?: string;
}

type FetchHandler = (
  url: string,
  method: string,
  body: unknown,
) => FetchResult | Promise<FetchResult> | { reject: unknown };

interface CallLog {
  url: string;
  method: string;
  body: unknown;
}

function isReject(value: unknown): value is { reject: unknown } {
  return (
    typeof value === "object" && value !== null && "reject" in (value as Record<string, unknown>)
  );
}

function installFetchMock(handler: FetchHandler) {
  const calls: CallLog[] = [];
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const method = init?.method ?? "GET";
    const rawBody = init?.body;
    const body =
      typeof rawBody === "string" && rawBody.length > 0 ? JSON.parse(rawBody) : undefined;
    calls.push({ url, method, body });
    const result = await handler(url, method, body);
    if (isReject(result)) {
      throw result.reject;
    }
    return new Response(result.body !== undefined ? JSON.stringify(result.body) : null, {
      status: result.status,
      statusText: result.statusText,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

beforeEach(() => {
  // Hard-reset the store back to its post-create() state. Tests below
  // mutate `user`, `loading`, `error` freely.
  useAuthStore.setState({ user: null, loading: true, error: null });
  // toast-store rides every fetchJson rejection (warn on 401, error on
  // 403). Clear so each test asserts its own toasts.
  useToastStore.getState().clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
  useAuthStore.setState({ user: null, loading: true, error: null });
  useToastStore.getState().clear();
});

describe("auth-store initial state", () => {
  it("starts with user=null, loading=true, error=null", () => {
    // The reset in beforeEach mirrors the create<>(...) shape so a fresh
    // import of the module yields the same observable state.
    const s = useAuthStore.getState();
    expect(s.user).toBeNull();
    expect(s.loading).toBe(true);
    expect(s.error).toBeNull();
  });
});

describe("auth-store login", () => {
  it("(success) POSTs /api/auth/login and sets user, error=null", async () => {
    const user = { id: "u1", email: "u@example.com", orgId: "org-1" };
    const { calls } = installFetchMock(() => ({ status: 200, body: user }));

    await useAuthStore.getState().login("u@example.com", "pw");

    expect(useAuthStore.getState().user).toEqual(user);
    expect(useAuthStore.getState().error).toBeNull();
    expect(calls).toEqual([
      {
        url: "/api/auth/login",
        method: "POST",
        body: { email: "u@example.com", password: "pw" },
      },
    ]);
  });

  it("(401 from a fresh tab) sets error to body.error and rethrows HttpError", async () => {
    // `user` stays null on a cold visit, so api.ts's 401 interceptor
    // shouldn't fire its session-expiry side-effect (no auto-logout, no
    // session-expired toast). The login attempt rejects with a plain
    // HttpError carrying the server's body.
    installFetchMock(() => ({
      status: 401,
      body: { error: "invalid_credentials" },
      statusText: "Unauthorized",
    }));

    let caught: unknown = null;
    try {
      await useAuthStore.getState().login("u@example.com", "wrong");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(HttpError);
    expect(caught).not.toBeInstanceOf(SessionExpiredError);
    // errorMessage() surfaces body.error if present.
    expect(useAuthStore.getState().error).toBe("invalid_credentials");
    expect(useAuthStore.getState().user).toBeNull();
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("(network error) sets error to the message and rethrows", async () => {
    installFetchMock(() => ({ reject: new TypeError("network down") }));

    let caught: unknown = null;
    try {
      await useAuthStore.getState().login("u@example.com", "pw");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(TypeError);
    expect(useAuthStore.getState().error).toBe("network down");
  });
});

describe("auth-store signup", () => {
  it("(success) POSTs /api/auth/signup and sets user", async () => {
    const user = { id: "u2", email: "new@example.com" };
    const { calls } = installFetchMock(() => ({ status: 201, body: user }));

    await useAuthStore.getState().signup("new@example.com", "pw");

    expect(useAuthStore.getState().user).toEqual(user);
    expect(useAuthStore.getState().error).toBeNull();
    expect(calls).toEqual([
      {
        url: "/api/auth/signup",
        method: "POST",
        body: { email: "new@example.com", password: "pw" },
      },
    ]);
  });

  it("(400 email-in-use) sets error and rethrows HttpError", async () => {
    installFetchMock(() => ({
      status: 400,
      body: { error: "email_taken" },
    }));

    let caught: unknown = null;
    try {
      await useAuthStore.getState().signup("dupe@example.com", "pw");
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(HttpError);
    expect((caught as HttpError).status).toBe(400);
    expect(useAuthStore.getState().error).toBe("email_taken");
    expect(useAuthStore.getState().user).toBeNull();
  });
});

describe("auth-store fetchMe", () => {
  it("(200) sets user, flips loading=false, clears error", async () => {
    useAuthStore.setState({
      user: null,
      loading: true,
      error: "stale",
    });
    const user = { id: "u1", email: "u@example.com" };
    installFetchMock(() => ({ status: 200, body: user }));

    await useAuthStore.getState().fetchMe();

    const s = useAuthStore.getState();
    expect(s.user).toEqual(user);
    expect(s.loading).toBe(false);
    expect(s.error).toBeNull();
  });

  it("(401 cold-visit) silently sets user=null, loading=false, error=null without toasting", async () => {
    // `user` is already null; api.ts skips the session-expiry branch on
    // null user, so this is a vanilla HttpError that auth-store maps to
    // the logged-out shape.
    useAuthStore.setState({ user: null, loading: true, error: null });
    installFetchMock(() => ({ status: 401, body: { error: "unauthorized" } }));

    await useAuthStore.getState().fetchMe();

    const s = useAuthStore.getState();
    expect(s.user).toBeNull();
    expect(s.loading).toBe(false);
    expect(s.error).toBeNull();
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("(500) sets user=null, loading=false, surfaces the error message", async () => {
    useAuthStore.setState({ user: null, loading: true, error: null });
    installFetchMock(() => ({
      status: 500,
      body: { error: "boom" },
      statusText: "Internal Server Error",
    }));

    await useAuthStore.getState().fetchMe();

    const s = useAuthStore.getState();
    expect(s.user).toBeNull();
    expect(s.loading).toBe(false);
    // errorMessage(HttpError) surfaces body.error.
    expect(s.error).toBe("boom");
  });

  it("(network error) sets user=null, loading=false, surfaces the throwable's message", async () => {
    useAuthStore.setState({ user: null, loading: true, error: null });
    installFetchMock(() => ({ reject: new TypeError("offline") }));

    await useAuthStore.getState().fetchMe();

    const s = useAuthStore.getState();
    expect(s.user).toBeNull();
    expect(s.loading).toBe(false);
    expect(s.error).toBe("offline");
  });
});

describe("auth-store logout", () => {
  it("POSTs /api/auth/logout and clears user", async () => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com" },
      loading: false,
      error: null,
    });
    const { calls } = installFetchMock(() => ({ status: 200, body: {} }));

    await useAuthStore.getState().logout();

    expect(useAuthStore.getState().user).toBeNull();
    expect(useAuthStore.getState().error).toBeNull();
    expect(calls.map((c) => c.url)).toEqual(["/api/auth/logout"]);
  });

  it("swallows server errors but still clears user (offline-friendly intent match)", async () => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com" },
      loading: false,
      error: null,
    });
    installFetchMock(() => ({ status: 500, body: { error: "boom" } }));

    // logout never throws — its try/catch swallows by design.
    await useAuthStore.getState().logout();

    expect(useAuthStore.getState().user).toBeNull();
  });

  it(
    "fires resetAllStores: drops plan/agent/settings slices on logout " +
      "(audit §2: prevent stale data leak across users in same tab)",
    async () => {
      // Seed cross-store state on the previous user's session.
      const summary: PlanSummary = {
        name: "user-a-plan",
        title: "User A",
        project: null,
        phaseCount: 0,
        taskCount: 0,
        doneCount: 0,
        createdAt: "2026-04-12T00:00:00Z",
        modifiedAt: "2026-04-12T00:00:00Z",
      };
      usePlanStore.setState({ plans: [summary], plansFetched: true });
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
            merge_status: null,
          },
        ],
        agentsFetched: true,
      });
      useSettingsStore.setState({
        effort: "low",
        webhookUrl: "https://user-a.example.com",
        loaded: true,
      });
      useAuthStore.setState({
        user: { id: "u1", email: "u@example.com" },
        loading: false,
        error: null,
      });
      installFetchMock(() => ({ status: 200, body: {} }));

      await useAuthStore.getState().logout();

      expect(useAuthStore.getState().user).toBeNull();
      // resetAllStores wired into logout (task 3.1).
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

// Note: auth-store has no direct WS handlers — it's the orchestrator that
// drives `resetAllStores()` on logout (covered above). The cross-store
// 401 → resetAllStores path is exercised in src/api.test.ts and
// reset-all.test.ts.
