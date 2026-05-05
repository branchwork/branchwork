import { HttpError } from "../api.js";

/// Best-effort human-readable extraction from an arbitrary throwable.
/// `HttpError` surfaces the JSON body's `error` field if present, then
/// falls back to `<status> <statusText>`. Anything else collapses to
/// its `message` (Error subclasses) or string coercion.
///
/// This is the canonical implementation for the dashboard — auth-store
/// and lib/toast both used to ship private copies; both now re-import
/// from here so a fix lands in one place.
export function errorMessage(e: unknown): string {
  if (e instanceof HttpError) {
    const body = e.body as { error?: string } | undefined;
    return body?.error ?? `${e.status} ${e.statusText}`;
  }
  return e instanceof Error ? e.message : String(e);
}

/// Common shorthand for the `e.body as { ... }` cast on `HttpError`.
/// Returns the parsed JSON body typed as `T`, or `undefined` when `e`
/// is not an `HttpError` or carries no body. Use when the caller wants
/// to peek at a specific field (e.g. `error`, `resolvedFolder`,
/// `currentTitle`) past what `errorMessage` surfaces.
export function httpErrorBody<T>(e: unknown): T | undefined {
  if (!(e instanceof HttpError)) return undefined;
  if (e.body === undefined || e.body === null) return undefined;
  return e.body as T;
}
