import { create } from "zustand";
import { fetchJson } from "../api.js";
import { useAuthStore } from "./auth-store.js";
import { resetAllStores } from "../lib/reset-all.js";

/// localStorage key for the user's currently-active org. Read by `api.ts`
/// so the X-Org-Id header rides every request, and by this store on
/// hydration so a refresh restores the same org context the user picked.
///
/// One key per browser, shared across tabs (matches localStorage's own
/// semantics — switching org in one tab updates every tab's next request).
/// We deliberately do NOT scope by user id: when user A signs out and user
/// B signs in, `resetAllStores()` -> `useOrgStore.reset()` clears the key
/// so user B doesn't inherit user A's pin.
export const ACTIVE_ORG_STORAGE_KEY = "branchwork.activeOrgId";

/// Helpers that defend against environments without a working
/// `localStorage` (older test runners, restricted iframes). The store
/// degrades to "no pin" rather than throwing.
export function readActiveOrgFromStorage(): string | null {
  try {
    return globalThis.localStorage?.getItem(ACTIVE_ORG_STORAGE_KEY) ?? null;
  } catch {
    return null;
  }
}

export function writeActiveOrgToStorage(orgId: string | null): void {
  try {
    if (orgId === null) {
      globalThis.localStorage?.removeItem(ACTIVE_ORG_STORAGE_KEY);
    } else {
      globalThis.localStorage?.setItem(ACTIVE_ORG_STORAGE_KEY, orgId);
    }
  } catch {
    // No-op — best-effort persistence.
  }
}

export interface OrgMembership {
  id: string;
  name: string;
  slug: string;
  role: string;
  memberCount: number;
}

interface OrgStore {
  /// Orgs the user belongs to. Empty until `fetchOrgs()` resolves.
  memberships: OrgMembership[];
  /// `true` once the first `fetchOrgs()` resolves. The chip stays hidden
  /// while this is `false` so a SaaS user doesn't see an empty placeholder
  /// during boot.
  loaded: boolean;
  /// ms-since-epoch of the last successful fetch. Mirrors the debounce
  /// contract used by other stores (plan-store/agent-store/runner-store).
  lastFetchedAt: number | null;

  fetchOrgs: () => Promise<void>;
  /// Switch to a different org the user belongs to. Persists the pick to
  /// localStorage (so future requests carry the right `X-Org-Id`), then
  /// resets every dashboard store and re-bootstraps so plan-store /
  /// agent-store / settings-store don't bleed across the org boundary.
  ///
  /// Idempotent: switching to the org you're already in is a no-op (no
  /// reset, no refetch).
  switchOrg: (orgId: string) => Promise<void>;
  /// Drop everything back to its initial shape. Driven by `reset-all.ts`
  /// on logout.
  reset: () => void;
}

let inFlightOrgsFetch: Promise<void> | null = null;

export function getInFlightOrgsFetch(): Promise<void> | null {
  return inFlightOrgsFetch;
}

const INITIAL_STATE: Pick<OrgStore, "memberships" | "loaded" | "lastFetchedAt"> = {
  memberships: [],
  loaded: false,
  lastFetchedAt: null,
};

export const useOrgStore = create<OrgStore>((set, get) => ({
  ...INITIAL_STATE,

  fetchOrgs: () => {
    if (inFlightOrgsFetch) return inFlightOrgsFetch;
    const promise = (async () => {
      try {
        // `memberCount` was added in 4.3 — coerce missing/null to 0 so
        // an old server build still hydrates the chip (it'll render
        // "0 members" until the server catches up rather than crashing).
        const raw = await fetchJson<
          Array<{
            id: string;
            name: string;
            slug: string;
            role: string;
            memberCount?: number | null;
          }>
        >("/api/orgs");
        const memberships: OrgMembership[] = raw.map((m) => ({
          id: m.id,
          name: m.name,
          slug: m.slug,
          role: m.role,
          memberCount: typeof m.memberCount === "number" ? m.memberCount : 0,
        }));
        // Reconcile a stale localStorage pin: if the persisted org isn't in
        // the current membership list (org deleted, user removed), drop it
        // so future requests fall back to the server's first-membership
        // resolution instead of silently 403'ing every call.
        const pinned = readActiveOrgFromStorage();
        if (pinned && !memberships.some((m) => m.id === pinned)) {
          writeActiveOrgToStorage(null);
        }
        set({
          memberships,
          loaded: true,
          lastFetchedAt: Date.now(),
        });
      } catch {
        // Leave `loaded` false on error so the chip stays hidden until a
        // future reconnect/refetch succeeds. A failed fetch is not a
        // standalone signal — the runner-store mode discriminator owns
        // that decision.
        set({ loaded: false });
      }
    })();
    inFlightOrgsFetch = promise;
    promise.finally(() => {
      if (inFlightOrgsFetch === promise) inFlightOrgsFetch = null;
    });
    return promise;
  },

  switchOrg: async (orgId) => {
    const memberships = get().memberships;
    if (!memberships.some((m) => m.id === orgId)) {
      // Defensive: callers should only pass IDs from `memberships`, but if
      // a stale dropdown click slips through we don't want to corrupt
      // localStorage with an org the user can't access.
      return;
    }

    // The auth-store / reset-all imports above form a cycle:
    //   auth-store -> reset-all -> org-store -> {auth-store, reset-all}
    // ESM lets this work because none of the bindings are read at
    // module-init time — they're only accessed inside closures (here,
    // and inside resetAllStores). By the time `switchOrg` actually runs,
    // every store module is fully evaluated.
    const currentOrgId = useAuthStore.getState().user?.orgId;
    if (currentOrgId === orgId) return;

    // Drop every cross-org slice (plan-store, agent-store, settings-store,
    // runner-store, toasts, ws subscribers). resetAllStores ALSO clears
    // the localStorage pin (logout's contract), so the new pin must be
    // written AFTER reset, before fetchMe — otherwise the X-Org-Id
    // header on /api/auth/me would be empty and the server would resolve
    // to first-membership instead of the user's pick.
    resetAllStores();
    writeActiveOrgToStorage(orgId);
    await useAuthStore.getState().fetchMe();

    // App.tsx bootstrap useEffect re-runs whenever `user` changes object
    // identity — fetchMe() above replaces the user object on success, so
    // plans/agents/settings/runners refetch + WS reconnects without us
    // needing to call them by hand here.
  },

  reset: () => {
    inFlightOrgsFetch = null;
    writeActiveOrgToStorage(null);
    set({ ...INITIAL_STATE });
  },
}));
