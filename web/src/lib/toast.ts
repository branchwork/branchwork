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
