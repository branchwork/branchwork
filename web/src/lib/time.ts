/// Locale-aware date/time formatters used throughout the dashboard.
/// Consolidates four near-identical helpers (`formatDate` in Sidebar +
/// ProjectDashboard, `timeAgo` in TaskCard, `formatRelative` in
/// NewPlanForm, `formatTimestamp` in AuditLog + ArchivePanel) so the
/// audit-flagged hardcoded `"en-US"` locale only lives in one place.
///
/// Locale resolves to `navigator.language` at call time (the audit
/// found four files hardcoding `"en-US"`). SSR/test environments
/// without a `navigator` fall back to `"en-US"` so callers stay safe.

function getLocale(): string {
  if (typeof navigator !== "undefined" && navigator.language) {
    return navigator.language;
  }
  return "en-US";
}

/// Coerce an ISO string or `Date` into a `Date`. SQLite's
/// `datetime('now')` emits naive UTC (`YYYY-MM-DD HH:MM:SS` with no
/// timezone marker); append `Z` so JS parses it as UTC and the
/// relative-math diffs are correct. Strings with an explicit `Z` or
/// numeric offset (`+01:00`, `-0500`) are passed through unchanged.
function toDate(input: string | Date): Date {
  if (input instanceof Date) return input;
  const hasTz = input.endsWith("Z") || /[+-]\d\d:?\d\d$/.test(input);
  return new Date(hasTz ? input : `${input}Z`);
}

/// Short calendar date, e.g. `Apr 5` for `en-US` and the locale's
/// equivalent elsewhere. No year, no time. Used as the >30d fallback
/// from `formatRelative` and inline anywhere a compact date is wanted.
export function formatShortDate(input: string | Date): string {
  return new Intl.DateTimeFormat(getLocale(), {
    month: "short",
    day: "numeric",
  }).format(toDate(input));
}

/// Calendar date plus time, e.g. `Apr 5, 02:30 PM`. Used as the >7d
/// fallback from `formatTimestamp` for the activity feed where exact
/// times still matter past the recent-activity window.
export function formatAbsolute(input: string | Date): string {
  return new Intl.DateTimeFormat(getLocale(), {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(toDate(input));
}

/// Fine relative timestamp: `just now` / `Nm ago` / `Nh ago` / `Nd ago`,
/// falling back to `formatShortDate` for >30d. Used by the plan list,
/// project cards, task-status timestamps, and runner last-seen labels.
/// Future dates collapse to `just now`.
export function formatRelative(input: string | Date): string {
  if (!input) return "";
  const d = toDate(input);
  const diffMs = Date.now() - d.getTime();
  const mins = Math.floor(diffMs / 60_000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 30) return `${days}d ago`;
  return formatShortDate(d);
}

/// Activity-feed timestamp: same fine relative as `formatRelative` for
/// the first 7 days, then `formatAbsolute` (date + time) for older
/// entries. Used by AuditLog and ArchivePanel where stale rows still
/// need an exact wall-clock.
export function formatTimestamp(input: string | Date): string {
  if (!input) return "";
  const d = toDate(input);
  const diffMs = Date.now() - d.getTime();
  const mins = Math.floor(diffMs / 60_000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 7) return `${days}d ago`;
  return formatAbsolute(d);
}
