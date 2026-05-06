import { errorMessage } from "./error.js";
import { useToastStore } from "../stores/toast-store.js";

/// Push a `kind: "error"` toast for an arbitrary throwable. The toast
/// title is the extracted message (no `Error:` prefix — `kind: "error"`
/// already conveys severity visually and via `role="alert"`); pass
/// `prefix` to add caller context, e.g.
/// `toastError(e, "Save failed")` → `"Save failed: <message>"`.
export function toastError(e: unknown, prefix?: string): string {
  const msg = errorMessage(e);
  const title = prefix ? `${prefix}: ${msg}` : msg;
  return useToastStore.getState().push({ kind: "error", title });
}

/// Push a `kind: "warn"` toast with a literal title — for known,
/// caller-controlled copy (e.g. session expired) where there is no
/// throwable to extract a message from. Auto-dismisses on the warn
/// kind's default TTL; pass `body` for a second line of detail.
export function toastWarn(title: string, body?: string): string {
  return useToastStore.getState().push({ kind: "warn", title, body });
}
