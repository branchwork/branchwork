import { HttpError } from "../api.js";
import { readActiveOrgFromStorage } from "../stores/org-store.js";

/// Result of a successful export. `body` is the raw YAML bytes the
/// server returned (already includes the two-line `# branchwork-plan-export`
/// header), `filename` is the canonical attachment name the server
/// suggested (or a sanitized fallback when the header was absent), and
/// `sha256Hex` is `sha256sum <filename>` over those exact bytes — both
/// halves of the cross-check the brief asks for (UI shows the hash in a
/// chip; downstream import can verify by running `sha256sum` on the
/// downloaded file).
export interface PlanExportResult {
  filename: string;
  body: string;
  sha256Hex: string;
}

const FILENAME_RE = /filename="?([^";]+)"?/i;

/// Pull the suggested filename from the server's `Content-Disposition`
/// header. Falls back to `<name>.plan.yaml` when the header is absent
/// or malformed — keeps the download usable in any unexpected proxy
/// path (the server sanitises char-by-char already, so the fallback is
/// the same shape).
function filenameFromDisposition(header: string | null, planName: string): string {
  if (header) {
    const m = FILENAME_RE.exec(header);
    if (m && m[1]) return m[1];
  }
  return `${planName}.plan.yaml`;
}

/// Compute the hex sha256 of a UTF-8 string. Uses `crypto.subtle` so it
/// works in any modern browser AND in jsdom-backed tests (Node ≥20.4
/// exposes `crypto.subtle` natively).
export async function sha256Hex(input: string): Promise<string> {
  const bytes = new TextEncoder().encode(input);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/// Trigger a browser download of `body` (text/yaml) as `filename`. The
/// `Blob`/`URL.createObjectURL` dance is the same pattern used by every
/// other CSV/text exporter in the dashboard. Side-effect: appends and
/// then removes a temporary `<a>` element from `document.body`.
function triggerDownload(filename: string, body: string): void {
  const blob = new Blob([body], { type: "application/yaml" });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.style.display = "none";
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  // Free the object URL immediately — the click has already kicked off
  // the download and Chromium/Firefox both keep their own reference.
  URL.revokeObjectURL(url);
}

/// Fetch the YAML export for `planName` from `GET /api/plans/:name/export`,
/// compute its sha256, trigger a browser download, and return both.
///
/// Throws `HttpError` on non-2xx (matches the rest of the api.ts surface)
/// so callers can `toastError(e, "Export failed")` without special casing.
/// Does NOT route through `fetchJson` because the response is `application/yaml`
/// text, not JSON. Still sends `credentials: "same-origin"` + `X-Org-Id`
/// so org gating + auth work identically to the JSON endpoints.
export async function exportPlan(planName: string): Promise<PlanExportResult> {
  const headers: Record<string, string> = {};
  // Mirror api.ts::defaultHeaders for X-Org-Id without pulling
  // `fetchJson` in (the response body is `application/yaml`, not JSON,
  // so the JSON wrapper would mangle it). Reuse the org-store storage
  // helper so we stay in sync with every other dashboard request.
  const activeOrg = readActiveOrgFromStorage();
  if (activeOrg) headers["X-Org-Id"] = activeOrg;

  const res = await fetch(`/api/plans/${encodeURIComponent(planName)}/export`, {
    credentials: "same-origin",
    headers,
  });
  if (!res.ok) {
    let body: unknown;
    try {
      body = await res.json();
    } catch {
      body = undefined;
    }
    throw new HttpError(res.status, res.statusText, body);
  }
  const text = await res.text();
  const filename = filenameFromDisposition(res.headers.get("content-disposition"), planName);
  const hash = await sha256Hex(text);
  triggerDownload(filename, text);
  return { filename, body: text, sha256Hex: hash };
}
