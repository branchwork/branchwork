import { HttpError } from "../api.js";
import { useToastStore } from "../stores/toast-store.js";

/// Best-effort human-readable message extraction from an arbitrary
/// throwable. Mirrors the private `errorMessage` in `auth-store.ts`;
/// task 0.3 promotes this into a shared `lib/error.ts` helper. Until
/// then, `toastError` owns its own copy so callsites that only need a
/// toast can adopt this helper without waiting on 0.3.
function errorMessage(e: unknown): string {
  if (e instanceof HttpError) {
    const body = e.body as { error?: string } | undefined;
    return body?.error ?? `${e.status} ${e.statusText}`;
  }
  return e instanceof Error ? e.message : String(e);
}

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
