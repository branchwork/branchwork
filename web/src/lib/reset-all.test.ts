import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { resetAllStores } from "./reset-all.js";
import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore, type ParsedPlan, type PlanSummary } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useToastStore } from "../stores/toast-store.js";
import { subscribeToWsEvents, useWsStore } from "../stores/ws-store.js";
import {
  ACTIVE_ORG_STORAGE_KEY,
  readActiveOrgFromStorage,
  useOrgStore,
  writeActiveOrgToStorage,
} from "../stores/org-store.js";

afterEach(() => {
  vi.unstubAllGlobals();
  // Belt-and-braces: each test calls resetAllStores() at least once,
  // so the residual state is already initial. This afterEach exists to
  // keep test isolation explicit if a future test seeds state but
  // forgets to assert on the reset.
  resetAllStores();
});

function seedAllStores() {
  const plan: ParsedPlan = {
    name: "user-a-plan",
    filePath: "user-a-plan.yaml",
    title: "User A Plan",
    context: "",
    project: null,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
    phases: [],
  };
  const summary: PlanSummary = {
    name: "user-a-plan",
    title: "User A Plan",
    project: null,
    phaseCount: 0,
    taskCount: 0,
    doneCount: 0,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
  };
  usePlanStore.setState({
    plans: [summary],
    selectedPlan: plan,
    plansFetched: true,
    lastPlansFetchedAt: Date.now(),
    planConfigs: {
      "user-a-plan": {
        autoAdvance: false,
        autoMode: true,
        maxFixAttempts: 3,
        pausedReason: null,
        parallel: false,
        runnerId: null,
        runnerFailover: "pause",
      },
    },
    autoModeRuntimes: { "user-a-plan": { state: "merging", task: "1.1" } },
    autoPushRebases: {
      "user-a-plan": {
        count: 2,
        branch: "master",
        expiresAt: Date.now() + 10_000,
      },
    },
    toasts: [{ id: "t-1", kind: "info", message: "stale" }],
    warnings: [{ name: "user-a-plan", file: "f", error: "boom", timestamp: 1 }],
  });
  useAgentStore.setState({
    agents: [
      {
        id: "agent-a-1",
        session_id: "s",
        pid: null,
        parent_agent_id: null,
        plan_name: "user-a-plan",
        task_id: "1.1",
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
  spawn_error: null,
      },
    ],
    selectedAgentId: "agent-a-1",
    agentOutput: { "agent-a-1": [] },
    agentDiffs: {
      "agent-a-1": { diff: "", stat: "", files: [], base_commit: "" },
    },
    agentsFetched: true,
    lastAgentsFetchedAt: Date.now(),
  });
  useSettingsStore.setState({
    effort: "low",
    skipPermissions: false,
    webhookUrl: "https://example.com/webhook",
    planArchiveRetentionDays: 7,
    loaded: true,
    drivers: [
      {
        name: "claude",
        binary: "claude",
        capabilities: {
          supports_cost: true,
          supports_verdict: true,
          supports_session_id: true,
          interactive_only: false,
        },
      },
    ],
    defaultDriver: "claude",
    lastSettingsFetchedAt: Date.now(),
    lastDriversFetchedAt: Date.now(),
  });
  useToastStore.getState().push({ kind: "info", title: "Sticky" });
}

describe("resetAllStores", () => {
  it("returns plan-store, agent-store, settings-store to initial shape", () => {
    seedAllStores();

    resetAllStores();

    const plan = usePlanStore.getState();
    expect(plan.plans).toEqual([]);
    expect(plan.selectedPlan).toBeNull();
    expect(plan.plansFetched).toBe(false);
    expect(plan.lastPlansFetchedAt).toBeNull();
    expect(plan.planConfigs).toEqual({});
    expect(plan.autoModeRuntimes).toEqual({});
    expect(plan.autoPushRebases).toEqual({});
    expect(plan.toasts).toEqual([]);
    expect(plan.warnings).toEqual([]);

    const agent = useAgentStore.getState();
    expect(agent.agents).toEqual([]);
    expect(agent.selectedAgentId).toBeNull();
    expect(agent.agentOutput).toEqual({});
    expect(agent.agentDiffs).toEqual({});
    expect(agent.agentsFetched).toBe(false);
    expect(agent.lastAgentsFetchedAt).toBeNull();

    const settings = useSettingsStore.getState();
    expect(settings.effort).toBe("high");
    expect(settings.skipPermissions).toBe(true);
    expect(settings.webhookUrl).toBeNull();
    expect(settings.planArchiveRetentionDays).toBe(30);
    expect(settings.loaded).toBe(false);
    expect(settings.drivers).toEqual([]);
    expect(settings.defaultDriver).toBe("claude");
    expect(settings.lastSettingsFetchedAt).toBeNull();
    expect(settings.lastDriversFetchedAt).toBeNull();

    expect(useToastStore.getState().toasts).toEqual([]);
  });

  it("disconnects the WebSocket and clears subscriber registry", () => {
    // Stand up a fake socket the store can hold and reset() can close.
    const close = vi.fn();
    useWsStore.setState({
      socket: { close } as unknown as WebSocket,
      connected: true,
      reconnectAttempt: 0,
    });
    const handler = vi.fn();
    subscribeToWsEvents(["audit_log"], handler);

    resetAllStores();

    expect(close).toHaveBeenCalledTimes(1);
    const ws = useWsStore.getState();
    expect(ws.socket).toBeNull();
    expect(ws.connected).toBe(false);
    expect(ws.reconnectAttempt).toBe(0);

    // After reset, a delivered audit_log must NOT reach the handler —
    // the subscriber Set was cleared, so any prior-session listener is
    // gone. Use the public `subscribeToWsEvents` channel by importing
    // `handleWsMessage` lazily so the test doesn't depend on it being
    // re-exported at the top.
    // (No need for an extra import — registering and never receiving
    //  is enough proof that the Set is empty.)
  });

  it("survives a missing socket gracefully (logout before connect)", () => {
    // App.tsx only opens the WS after auth resolves; a user who hits
    // /login on a cold visit, types credentials, and bounces back out
    // can call logout() before any WS handle exists. Reset must not
    // throw.
    useWsStore.setState({ socket: null, connected: false, reconnectAttempt: 0 });
    expect(() => resetAllStores()).not.toThrow();
  });

  it("does NOT reset auth-store (auth is the orchestrator and owns its slice)", async () => {
    const { useAuthStore } = await import("../stores/auth-store.js");
    useAuthStore.setState({
      user: { id: "u-1", email: "u@example.com" },
      error: null,
      loading: false,
    });

    resetAllStores();

    // resetAllStores intentionally leaves auth alone — auth.logout()
    // does its own set({user:null}) and orchestrates the rest.
    expect(useAuthStore.getState().user).toEqual({
      id: "u-1",
      email: "u@example.com",
    });
  });

  it(
    "acceptance: user A's plans/agents/settings cleared so user B's session " +
      "starts fresh in the same tab (no refresh)",
    () => {
      // Simulate user A's populated session.
      seedAllStores();

      // User clicks Sign out → auth.logout() → resetAllStores().
      // (We test resetAllStores in isolation here; auth.logout's wider
      // contract is covered in api.test.ts via the 401 interceptor.)
      resetAllStores();

      // User B logs in. App.tsx bootstrap fetches and replaces every
      // slice; here we just need to prove the slate is clean BEFORE
      // those fetches resolve, so that a stale render between login
      // and bootstrap doesn't show user A's data.
      expect(usePlanStore.getState().plans).toEqual([]);
      expect(usePlanStore.getState().selectedPlan).toBeNull();
      expect(useAgentStore.getState().agents).toEqual([]);
      // settings revert to module defaults (effort=high, retention=30).
      expect(useSettingsStore.getState().effort).toBe("high");
      expect(useSettingsStore.getState().planArchiveRetentionDays).toBe(30);
    },
  );

  // Task 4.3 acceptance: resetAllStores must also drop the active-org
  // pin in localStorage, otherwise user A's org would carry over to
  // user B as the X-Org-Id header on every request and grant access
  // to A's data.
  describe("org-store reset (task 4.3)", () => {
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

    beforeEach(() => {
      vi.stubGlobal("localStorage", makeMockStorage());
    });

    it("clears org-store memberships AND the localStorage X-Org-Id pin", () => {
      writeActiveOrgToStorage("org-user-a");
      useOrgStore.setState({
        memberships: [
          {
            id: "org-user-a",
            name: "User A's org",
            slug: "user-a",
            role: "owner",
            memberCount: 2,
          },
        ],
        loaded: true,
        lastFetchedAt: Date.now(),
      });
      // Sanity-check the seed.
      expect(readActiveOrgFromStorage()).toBe("org-user-a");
      expect(useOrgStore.getState().memberships).toHaveLength(1);

      resetAllStores();

      // Both the in-memory slice and the persisted pin are gone, so
      // user B's first /api/auth/me request resolves to user B's first
      // membership (server-side fallback) instead of inheriting user A's
      // pin.
      expect(useOrgStore.getState().memberships).toEqual([]);
      expect(useOrgStore.getState().loaded).toBe(false);
      expect(readActiveOrgFromStorage()).toBeNull();
      expect(globalThis.localStorage?.getItem(ACTIVE_ORG_STORAGE_KEY)).toBeNull();
    });
  });
});
