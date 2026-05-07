import { Link } from "react-router-dom";
import { useRunnerStore, type Runner } from "../stores/runner-store.js";

/// Pick the "interesting" runner to surface in the chrome:
///   1. Any online runner — prefer the most-recently-seen one (refetch /
///      WS-update orders the list by `lastSeenAt`).
///   2. Otherwise the most-recently-seen runner (offline).
///   3. Otherwise `null` — caller renders the "no runner" state.
function pickActiveRunner(runners: Runner[]): Runner | null {
  if (runners.length === 0) return null;
  const online = runners.find((r) => r.status === "online");
  if (online) return online;
  return runners[0];
}

/// Compact runner-connection indicator that lives in the bottom-right
/// chrome cluster (audit §17 / dashboard-ui-overhaul 4.1). Hidden on
/// standalone deployments — those have no runner concept and shouldn't be
/// nagged about registration. On SaaS deployments it shows one of three
/// states (amber / red / emerald) so a runner outage is visible without
/// digging into `/runners`.
///
/// State source of truth is `useRunnerStore`; live updates ride the
/// `runner_connected/_disconnected/_drivers` WS events wired in
/// `ws-store.ts`. Initial fetch happens at App.tsx bootstrap.
export function RunnerStatus() {
  const mode = useRunnerStore((s) => s.mode);
  const loaded = useRunnerStore((s) => s.loaded);
  const runners = useRunnerStore((s) => s.runners);

  // Hide while bootstrap is still resolving — a flash of "register a runner"
  // on a standalone box would mislead users. `loaded === true` plus
  // `mode === "saas"` is the only render gate.
  if (!loaded) return null;
  if (mode !== "saas") return null;

  const active = pickActiveRunner(runners);
  if (!active) {
    // SaaS deployment with zero runner rows — primary call-to-action is
    // enrolling one. The link target lives at /runners (the placeholder
    // ships in 1.1; full enrolment flow ships in 4.2).
    return (
      <span data-testid="runner-status" data-state="no-runner" className="flex items-center gap-2">
        <span aria-hidden="true" className="inline-block w-2 h-2 rounded-full bg-amber-400" />
        <Link
          to="/runners"
          className="text-amber-300 hover:text-amber-200 transition"
          title="No runner registered for this org. Click to register one."
        >
          No runner — register one
        </Link>
      </span>
    );
  }

  if (active.status !== "online") {
    return (
      <span
        data-testid="runner-status"
        data-state="offline"
        className="flex items-center gap-2"
        title={active.lastSeenAt ? `Last seen ${active.lastSeenAt}` : "Runner is offline"}
      >
        <span aria-hidden="true" className="inline-block w-2 h-2 rounded-full bg-red-500" />
        <span className="text-red-300">Runner offline</span>
      </span>
    );
  }

  // Online — show the runner name (or a short id fallback) and the
  // hostname/version on hover for diagnostics.
  const label = active.name?.trim() || active.id.slice(0, 8);
  const tooltip = [
    `Runner: ${active.name ?? active.id}`,
    active.hostname ? `host: ${active.hostname}` : null,
    active.version ? `version: ${active.version}` : null,
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <span
      data-testid="runner-status"
      data-state="online"
      className="flex items-center gap-2"
      title={tooltip}
    >
      <span aria-hidden="true" className="inline-block w-2 h-2 rounded-full bg-emerald-500" />
      <span className="text-gray-400">
        Runner: <span className="font-mono text-gray-300">{label}</span>
      </span>
    </span>
  );
}
