import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ACTIVE_ORG_STORAGE_KEY,
  readActiveOrgFromStorage,
  useOrgStore,
  writeActiveOrgToStorage,
} from "./org-store.js";
import { useAuthStore } from "./auth-store.js";
import { usePlanStore } from "./plan-store.js";
import { useAgentStore } from "./agent-store.js";

/// Map-backed localStorage stand-in. jsdom's default about:blank origin
/// disables localStorage; setting a real URL helps but the implementation
/// vitest 3 ships still leaves `setItem`/`getItem` as non-functions in
/// some configurations. A lightweight Map gives every test a clean,
/// deterministic store without depending on jsdom's behaviour.
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
  useOrgStore.setState({
    memberships: [],
    loaded: false,
    lastFetchedAt: null,
  });
  useAuthStore.setState({
    user: null,
    loading: false,
    error: null,
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("org-store storage helpers", () => {
  it("write + read round-trips an org id", () => {
    writeActiveOrgToStorage("org-123");
    expect(readActiveOrgFromStorage()).toBe("org-123");
    expect(globalThis.localStorage?.getItem(ACTIVE_ORG_STORAGE_KEY)).toBe("org-123");
  });

  it("write(null) clears the pin", () => {
    writeActiveOrgToStorage("org-123");
    writeActiveOrgToStorage(null);
    expect(readActiveOrgFromStorage()).toBeNull();
  });

  it("read returns null when localStorage is unavailable", () => {
    // Some sandboxed environments (Safari private browsing, certain
    // iframes) throw on access. The helper degrades to "no pin" rather
    // than propagating the error — keeping the dashboard rendered.
    const fake = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {},
      removeItem: () => {},
      clear: () => {},
      length: 0,
      key: () => null,
    } as Storage;
    vi.stubGlobal("localStorage", fake);
    expect(readActiveOrgFromStorage()).toBeNull();
  });
});

describe("org-store fetchOrgs", () => {
  it("populates memberships and flips `loaded`", async () => {
    const memberships = [
      {
        id: "org-1",
        name: "Acme",
        slug: "acme",
        role: "owner",
        memberCount: 3,
      },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(JSON.stringify(memberships), {
            status: 200,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );

    await useOrgStore.getState().fetchOrgs();

    const s = useOrgStore.getState();
    expect(s.memberships).toEqual(memberships);
    expect(s.loaded).toBe(true);
    expect(s.lastFetchedAt).not.toBeNull();
  });

  it("coerces missing memberCount to 0 (forward-compat with older server builds)", async () => {
    // Simulate a server that hasn't shipped the 4.3 backend change yet:
    // the org rows are present but memberCount is absent. The store
    // hydrates without crashing — it'll render "0 members" until the
    // backend catches up.
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify([{ id: "org-1", name: "Acme", slug: "acme", role: "owner" }]),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );

    await useOrgStore.getState().fetchOrgs();
    const m = useOrgStore.getState().memberships;
    expect(m).toHaveLength(1);
    expect(m[0]!.memberCount).toBe(0);
  });

  it("drops a stale localStorage pin when the org is no longer in the membership list", async () => {
    writeActiveOrgToStorage("org-deleted");
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify([
              {
                id: "org-1",
                name: "Acme",
                slug: "acme",
                role: "owner",
                memberCount: 1,
              },
            ]),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );

    await useOrgStore.getState().fetchOrgs();
    expect(readActiveOrgFromStorage()).toBeNull();
  });

  it("preserves a valid localStorage pin when the org is in the membership list", async () => {
    writeActiveOrgToStorage("org-1");
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify([
              {
                id: "org-1",
                name: "Acme",
                slug: "acme",
                role: "owner",
                memberCount: 1,
              },
              {
                id: "org-2",
                name: "Beta",
                slug: "beta",
                role: "member",
                memberCount: 2,
              },
            ]),
            { status: 200, headers: { "Content-Type": "application/json" } },
          ),
      ),
    );

    await useOrgStore.getState().fetchOrgs();
    expect(readActiveOrgFromStorage()).toBe("org-1");
  });

  it("leaves `loaded` false on fetch error so the chip stays hidden", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("server error", { status: 500, statusText: "Internal" })),
    );

    await useOrgStore.getState().fetchOrgs();
    expect(useOrgStore.getState().loaded).toBe(false);
    expect(useOrgStore.getState().memberships).toEqual([]);
  });

  it("coalesces concurrent callers onto a single in-flight fetch", async () => {
    const fetchSpy = vi.fn(
      async () =>
        new Response("[]", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
    );
    vi.stubGlobal("fetch", fetchSpy);

    const a = useOrgStore.getState().fetchOrgs();
    const b = useOrgStore.getState().fetchOrgs();
    await Promise.all([a, b]);

    expect(fetchSpy).toHaveBeenCalledTimes(1);
  });
});

describe("org-store switchOrg", () => {
  function seedTwoOrgs() {
    useOrgStore.setState({
      memberships: [
        {
          id: "org-1",
          name: "Acme",
          slug: "acme",
          role: "owner",
          memberCount: 3,
        },
        {
          id: "org-2",
          name: "Beta",
          slug: "beta",
          role: "member",
          memberCount: 5,
        },
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
  }

  it("is a no-op when the requested org id is not in the membership list", async () => {
    seedTwoOrgs();
    useAuthStore.setState({
      user: { id: "u-1", email: "u@x.com", orgId: "org-1" },
    });
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);

    await useOrgStore.getState().switchOrg("org-not-mine");

    // No fetchMe() roundtrip, no localStorage write.
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(readActiveOrgFromStorage()).toBeNull();
  });

  it("is a no-op when the requested org is already the active one", async () => {
    seedTwoOrgs();
    useAuthStore.setState({
      user: { id: "u-1", email: "u@x.com", orgId: "org-1" },
    });
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);

    await useOrgStore.getState().switchOrg("org-1");

    expect(fetchSpy).not.toHaveBeenCalled();
    // Idempotent: localStorage stays unset (the user was on org-1 by
    // server-side first-membership resolution, not via an explicit pin).
    expect(readActiveOrgFromStorage()).toBeNull();
  });

  it("writes the localStorage pin, resets cross-org stores, and re-fetches /api/auth/me", async () => {
    seedTwoOrgs();
    useAuthStore.setState({
      user: { id: "u-1", email: "u@x.com", orgId: "org-1" },
    });
    // Seed plan-store + agent-store with org-1 data we expect to be
    // wiped by resetAllStores.
    usePlanStore.setState({
      plans: [
        {
          name: "p-1",
          title: "P 1",
          project: null,
          phaseCount: 0,
          taskCount: 0,
          doneCount: 0,
          createdAt: "",
          modifiedAt: "",
        },
      ],
    });
    useAgentStore.setState({
      agents: [
        {
          id: "a-1",
          session_id: "",
          pid: null,
          parent_agent_id: null,
          plan_name: "p-1",
          task_id: null,
          cwd: "",
          status: "running",
          mode: "pty",
          prompt: null,
          started_at: "",
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
    });

    // /api/auth/me is the only request switchOrg makes; org-store does
    // NOT call /api/auth/switch-org in the localStorage approach (the
    // pin alone tells the server which org to resolve).
    const fetchSpy = vi.fn(async (url: string) => {
      if (url === "/api/auth/me") {
        return new Response(JSON.stringify({ id: "u-1", email: "u@x.com", orgId: "org-2" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response("not found", { status: 404 });
    });
    vi.stubGlobal("fetch", fetchSpy);

    await useOrgStore.getState().switchOrg("org-2");

    // localStorage now pins org-2 — every future fetchJson() rides X-Org-Id.
    expect(readActiveOrgFromStorage()).toBe("org-2");
    // Cross-org slices are wiped (resetAllStores ran).
    expect(usePlanStore.getState().plans).toEqual([]);
    expect(useAgentStore.getState().agents).toEqual([]);
    // org-store itself is wiped — App.tsx bootstrap on user-change
    // re-runs fetchOrgs; we don't assert that here since the post-reset
    // refetch lives in App.tsx, not in switchOrg.
    expect(useOrgStore.getState().memberships).toEqual([]);
    // /api/auth/me was called as part of switchOrg (the recovery path
    // for `user.orgId`). The X-Org-Id header is built from localStorage
    // by api.ts at fetch time.
    expect(fetchSpy).toHaveBeenCalledWith("/api/auth/me", expect.objectContaining({}));
    // Auth-store reflects the new active org.
    expect(useAuthStore.getState().user?.orgId).toBe("org-2");
  });
});

describe("org-store reset", () => {
  it("clears memberships and the localStorage pin", () => {
    writeActiveOrgToStorage("org-7");
    useOrgStore.setState({
      memberships: [
        {
          id: "org-7",
          name: "Foo",
          slug: "foo",
          role: "owner",
          memberCount: 1,
        },
      ],
      loaded: true,
      lastFetchedAt: 123,
    });

    useOrgStore.getState().reset();

    const s = useOrgStore.getState();
    expect(s.memberships).toEqual([]);
    expect(s.loaded).toBe(false);
    expect(s.lastFetchedAt).toBeNull();
    expect(readActiveOrgFromStorage()).toBeNull();
  });
});
