import { create } from "zustand";

export type ToastKind = "info" | "success" | "warn" | "error";

export interface Toast {
  id: string;
  kind: ToastKind;
  title: string;
  body?: string;
  /// Auto-dismiss after this many ms. `0` (or omitted on `error`)
  /// keeps the toast pinned until `dismiss(id)` is called.
  ttlMs?: number;
}

export interface ToastInput {
  kind: ToastKind;
  title: string;
  body?: string;
  ttlMs?: number;
}

interface ToastStore {
  toasts: Toast[];
  push: (t: ToastInput) => string;
  dismiss: (id: string) => void;
  clear: () => void;
}

/// Maximum number of toasts visible at once. Pushes beyond the cap
/// evict the oldest entry (FIFO) so a flood of background errors
/// can't drown out the latest user-visible state.
export const TOAST_CAP = 5;

/// Default TTLs by kind. `error` is sticky because the user must
/// acknowledge it; `warn` is also high-attention but auto-dismisses
/// like info/success since it's not a hard failure (callers can
/// pass `ttlMs: 0` on a `warn` to make it sticky).
const DEFAULT_TTL_MS: Record<ToastKind, number> = {
  info: 6000,
  success: 6000,
  warn: 6000,
  error: 0,
};

function genId(): string {
  return `toast-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

export const useToastStore = create<ToastStore>((set, get) => ({
  toasts: [],

  push: ({ kind, title, body, ttlMs }) => {
    const id = genId();
    const effectiveTtl = ttlMs ?? DEFAULT_TTL_MS[kind];
    const next: Toast = { id, kind, title, body, ttlMs: effectiveTtl };
    set((s) => {
      const combined = [...s.toasts, next];
      const trimmed =
        combined.length > TOAST_CAP ? combined.slice(combined.length - TOAST_CAP) : combined;
      return { toasts: trimmed };
    });
    if (effectiveTtl && effectiveTtl > 0) {
      setTimeout(() => {
        get().dismiss(id);
      }, effectiveTtl);
    }
    return id;
  },

  dismiss: (id) => {
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
  },

  clear: () => set({ toasts: [] }),
}));

/// Map a toast kind to its ARIA live-region role. `info`/`success` are
/// non-urgent (`role="status"` → `aria-live="polite"`); `warn`/`error`
/// interrupt the screen reader (`role="alert"` → `aria-live="assertive"`).
export function toastRole(kind: ToastKind): "status" | "alert" {
  return kind === "warn" || kind === "error" ? "alert" : "status";
}
