import { create } from "zustand";
import { HttpError, fetchJson, postJson } from "../api.js";

export interface AuthUser {
  id: string;
  email: string;
  orgId?: string;
}

interface AuthStore {
  user: AuthUser | null;
  /// `true` until the first /api/auth/me completes. UI should avoid deciding
  /// between "login page" and "main app" before this flips, otherwise a
  /// logged-in user flashes the login screen on every refresh.
  loading: boolean;
  error: string | null;

  fetchMe: () => Promise<void>;
  login: (email: string, password: string) => Promise<void>;
  signup: (email: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
}

function errorMessage(e: unknown): string {
  if (e instanceof HttpError) {
    const body = e.body as { error?: string } | undefined;
    return body?.error ?? `${e.status} ${e.statusText}`;
  }
  return e instanceof Error ? e.message : String(e);
}

export const useAuthStore = create<AuthStore>((set) => ({
  user: null,
  loading: true,
  error: null,

  fetchMe: async () => {
    try {
      const user = await fetchJson<AuthUser>("/api/auth/me");
      set({ user, loading: false, error: null });
    } catch (e) {
      if (e instanceof HttpError && e.status === 401) {
        set({ user: null, loading: false, error: null });
        return;
      }
      // Any other failure leaves `user = null` so the login page shows, but
      // surface the error so it isn't silently swallowed on, say, a server
      // outage.
      set({ user: null, loading: false, error: errorMessage(e) });
    }
  },

  login: async (email, password) => {
    try {
      const user = await postJson<AuthUser>("/api/auth/login", { email, password });
      set({ user, error: null });
    } catch (e) {
      set({ error: errorMessage(e) });
      throw e;
    }
  },

  signup: async (email, password) => {
    try {
      const user = await postJson<AuthUser>("/api/auth/signup", { email, password });
      set({ user, error: null });
    } catch (e) {
      set({ error: errorMessage(e) });
      throw e;
    }
  },

  logout: async () => {
    try {
      await postJson("/api/auth/logout", {});
    } catch {
      // Non-fatal: we drop the user regardless so the client state matches
      // the user's intent even if the server is unreachable.
    }
    set({ user: null, error: null });
  },
}));
