import { useAuthStore } from "./stores/auth-store.js";
import { toastError, toastWarn } from "./lib/toast.js";
import { readActiveOrgFromStorage } from "./stores/org-store.js";

const BASE = "";

/// Build the headers we attach to every dashboard request. Content-Type
/// is fixed; `X-Org-Id` rides every request when the user has explicitly
/// switched orgs (org-store persists the pick to localStorage in 4.3),
/// so the server resolves the right multi-tenancy context. Absent on a
/// cold visit — server falls back to `first-membership`, mirroring the
/// pre-4.3 behaviour.
function defaultHeaders(): Record<string, string> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const activeOrg = readActiveOrgFromStorage();
  if (activeOrg) headers["X-Org-Id"] = activeOrg;
  return headers;
}

export class HttpError extends Error {
  constructor(public status: number, public statusText: string, public body?: unknown) {
    super(`${status} ${statusText}`);
  }
}

/// Sentinel rejection for mid-session 401s. Lets callers distinguish a
/// real session-expiry (the dashboard logged the user out, the
/// session-expired toast already fired) from a generic 401 that surfaces
/// as `HttpError`. Callers that need to no-op on session expiry — e.g.
/// to avoid stacking their own error toast on top of the warn toast we
/// already pushed — can `instanceof SessionExpiredError`.
export class SessionExpiredError extends Error {
  constructor() {
    super("Session expired");
    this.name = "SessionExpiredError";
  }
}

const LOGOUT_PATH = "/api/auth/logout";

async function readBody(res: Response): Promise<unknown> {
  try {
    return await res.json();
  } catch {
    return undefined;
  }
}

/// Single fetch wrapper used by every dashboard request. Centralises
/// status-aware error handling so individual stores don't have to:
///
/// - **401 Unauthorized** (any endpoint): assume the session expired.
///   Reset auth state via `useAuthStore.getState().logout()` — which
///   also resets every dependent store (cross-task contract: auth
///   logout is the dashboard's single store-reset point, see task 3.1
///   of the dashboard-ui-overhaul plan and `lib/reset-all.ts`). Push a
///   `kind:"warn"` toast
///   ("Session expired — please sign in again") and reject with
///   `SessionExpiredError`. Skipped when there is no current user
///   (the bootstrap `/api/auth/me` probe must not fire a misleading
///   toast on a cold visit) and skipped for the logout endpoint
///   itself (recursion guard — logout's own 401 is meaningless).
/// - **403 Forbidden**: the user is signed in but lacks permission for
///   this specific action. Push a `kind:"error"` toast surfacing the
///   server's error body and reject with `HttpError` (no logout, no
///   sentinel — the session is still good).
/// - Any other non-2xx: reject with `HttpError`.
export async function fetchJson<T>(path: string, init?: RequestInit): Promise<T> {
  // Spread `defaultHeaders()` first so callers can still override
  // Content-Type explicitly via `init.headers` (mirrors the prior
  // semantics — init takes precedence). The X-Org-Id header is
  // intentionally NOT user-overridable from a single call site; it's
  // global state owned by org-store.
  const headers = { ...defaultHeaders(), ...(init?.headers ?? {}) };
  const res = await fetch(`${BASE}${path}`, {
    credentials: "same-origin",
    ...init,
    headers,
  });
  if (res.ok) {
    return res.json() as Promise<T>;
  }
  const body = await readBody(res);
  const err = new HttpError(res.status, res.statusText, body);

  if (res.status === 401 && path !== LOGOUT_PATH) {
    const auth = useAuthStore.getState();
    if (auth.user !== null) {
      // Await so the synchronous `set({ user: null })` inside logout()
      // has run before our caller observes the rejection — otherwise
      // an immediate read of useAuthStore.user could still return the
      // stale logged-in user. The recursion guard above keeps logout's
      // own /api/auth/logout call from re-entering this branch.
      await auth.logout();
      toastWarn("Session expired — please sign in again");
      throw new SessionExpiredError();
    }
  }

  if (res.status === 403) {
    toastError(err);
  }

  throw err;
}

export function postJson<T>(path: string, body: unknown): Promise<T> {
  return fetchJson<T>(path, {
    method: "POST",
    body: JSON.stringify(body),
  });
}

export function putJson<T>(path: string, body: unknown): Promise<T> {
  return fetchJson<T>(path, {
    method: "PUT",
    body: JSON.stringify(body),
  });
}

export function deleteJson<T>(path: string): Promise<T> {
  return fetchJson<T>(path, { method: "DELETE" });
}
