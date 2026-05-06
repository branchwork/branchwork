import { useAgentStore } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useToastStore } from "../stores/toast-store.js";
import { useWsStore } from "../stores/ws-store.js";

/// Drop every dashboard store (except `auth-store`) back to its initial
/// shape. Single source of truth for the "I just signed out, drop the
/// previous user's data" cleanup so each new store gets one obvious
/// place to wire its `reset()` instead of every callsite acquiring a
/// new dependency.
///
/// Called from `auth-store.logout()` AFTER the `/api/auth/logout` POST
/// resolves, and indirectly from the `api.ts` 401 interceptor (which
/// re-enters `auth.logout()`). Order:
///
/// 1. `useWsStore.reset()` — closes the socket and drops subscribers
///    BEFORE the data slices are cleared, so a late-arriving broadcast
///    can't repopulate state we're about to wipe.
/// 2. Data slices (plan / agent / settings) — fresh init shape, in-
///    flight fetch handles cleared.
/// 3. `useToastStore.clear()` — drops any sticky error toasts from the
///    previous session so user B doesn't inherit user A's red boxes.
///
/// `auth-store` is intentionally NOT reset here — it's the orchestrator,
/// and `logout()` does its own `set({ user: null })`. Keeping auth out
/// avoids a circular reset loop and means external 401-handlers can
/// call `resetAllStores()` without trampling the auth slice they don't
/// own.
export function resetAllStores(): void {
  useWsStore.getState().reset();
  usePlanStore.getState().reset();
  useAgentStore.getState().reset();
  useSettingsStore.getState().reset();
  useRunnerStore.getState().reset();
  // Drops memberships + clears the localStorage X-Org-Id pin so the
  // next sign-in starts on the new user's first-membership org. Sits
  // after runner-store because the org pin influences which org-scoped
  // requests run during the next bootstrap (deterministic ordering
  // matters less than presence — but keeping the pin clear before the
  // next /api/auth/me fires is the load-bearing part).
  useOrgStore.getState().reset();
  useToastStore.getState().clear();
}
