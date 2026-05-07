import { useEffect, useState } from "react";
import { Navigate } from "react-router-dom";
import {
  useRunnerStore,
  type Runner,
  type RunnerDriverInfo,
  type RunnerDriverState,
} from "../stores/runner-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { Banner } from "./ui/Banner.js";
import { Button } from "./ui/Button.js";
import { RunnerEnrollModal } from "./RunnerEnrollModal.js";
import { RunnerLogPanel } from "./RunnerLogPanel.js";
import { formatRelative } from "../lib/time.js";
import { toastWarn } from "../lib/toast.js";

const EFFORT_LEVELS: { value: string; label: string }[] = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "max", label: "Max" },
];

/// Replaces the 1.1 placeholder. Owns the full SaaS runner control
/// plane:
///
/// - List: table driven by `useRunnerStore.runners` (seeded by
///   `App.tsx` bootstrap; live updates ride
///   `runner_connected/_disconnected/_drivers` WS events).
/// - Enroll: button → `RunnerEnrollModal` which calls
///   `POST /api/runners/tokens` and surfaces the install command.
/// - Diagnostics: per-runner row showing hostname, version, created,
///   last-seen, plus a deployment-wide "agents in flight" tally drawn
///   from `useAgentStore` (today the server doesn't tag agents with
///   their runner, so the tally is per-deployment, not per-runner; that
///   surfaces the live load while still being honest about the data).
/// - Standalone redirect: when `mode === "standalone"` and the first
///   `/api/runners` fetch has resolved, redirect to `/` and push a warn
///   toast — runners only apply to SaaS deployments. The redirect
///   waits on `loaded` so an in-flight bootstrap doesn't bounce a SaaS
///   user away on a cold visit (`mode` is `unknown` until the fetch
///   resolves).
///
/// Disconnect is intentionally absent today: the server does not yet
/// expose a runner-revoke or kill-connection endpoint, and the brief
/// allows omitting it until the API lands.
export function RunnersPage() {
  const mode = useRunnerStore((s) => s.mode);
  const loaded = useRunnerStore((s) => s.loaded);
  const runners = useRunnerStore((s) => s.runners);
  const fetchRunners = useRunnerStore((s) => s.fetchRunners);
  const agents = useAgentStore((s) => s.agents);

  const [enrollOpen, setEnrollOpen] = useState(false);
  const [redirected, setRedirected] = useState(false);

  // Refresh on mount even though App.tsx's bootstrap already fetched
  // once — the page might be hit via deep link long after bootstrap
  // and the deployment-mode discriminator may have flipped (e.g. a
  // runner connected, server flipped from "standalone" to "saas").
  useEffect(() => {
    void fetchRunners();
  }, [fetchRunners]);

  // Standalone redirect: fire the toast exactly once (state guard
  // below) and let `<Navigate replace/>` do the URL change. Toasting
  // before the redirect means the user sees the explanation on the
  // page they end up on (`/`), not on the page they were leaving.
  useEffect(() => {
    if (loaded && mode === "standalone" && !redirected) {
      setRedirected(true);
      toastWarn("Runners only apply to SaaS deployments");
    }
  }, [loaded, mode, redirected]);

  if (loaded && mode === "standalone") {
    return <Navigate to="/" replace />;
  }

  // Show a thin loading shell while bootstrap is still resolving.
  // Avoids flashing "no runners yet" on a SaaS deployment that's only
  // briefly empty.
  if (!loaded) {
    return <div className="p-6 max-w-4xl mx-auto text-sm text-gray-500">Loading runners…</div>;
  }

  const inFlight = agents.filter((a) => a.status === "running" || a.status === "starting").length;

  return (
    <div className="p-6 max-w-4xl mx-auto">
      <div className="flex items-start justify-between mb-6">
        <div>
          <h1 className="text-xl font-bold text-gray-100">Runners</h1>
          <p className="text-xs text-gray-500 mt-1">
            Remote runners execute agents on your hosts so the SaaS dashboard never touches your
            filesystem. Enrol one, copy the install command, and start the runner on the target
            machine.
          </p>
        </div>
        <Button
          variant="primary"
          size="sm"
          onClick={() => setEnrollOpen(true)}
          data-testid="enroll-runner-button"
        >
          Enrol a runner
        </Button>
      </div>

      {runners.length === 0 ? (
        <EmptyState onEnroll={() => setEnrollOpen(true)} />
      ) : (
        <RunnerList runners={runners} inFlightAgents={inFlight} />
      )}

      <RunnerEnrollModal open={enrollOpen} onClose={() => setEnrollOpen(false)} />
    </div>
  );
}

function EmptyState({ onEnroll }: { onEnroll: () => void }) {
  return (
    <div
      data-testid="runners-empty"
      className="rounded border border-dashed border-gray-700 bg-gray-900/40 p-6 text-center"
    >
      <p className="text-sm text-gray-300">No runners yet.</p>
      <p className="mt-1 text-xs text-gray-500">
        Enrol your first runner to start executing agents on a remote host.
      </p>
      <div className="mt-4">
        <Button variant="primary" size="sm" onClick={onEnroll}>
          Enrol a runner
        </Button>
      </div>
    </div>
  );
}

function RunnerList({ runners, inFlightAgents }: { runners: Runner[]; inFlightAgents: number }) {
  return (
    <div data-testid="runners-list">
      <p className="text-[11px] text-gray-500 mb-2">
        {runners.length} runner{runners.length === 1 ? "" : "s"} · {inFlightAgents} agent
        {inFlightAgents === 1 ? "" : "s"} in flight
      </p>
      <ul className="divide-y divide-gray-800 border border-gray-800 rounded">
        {runners.map((r) => (
          <RunnerRow key={r.id} runner={r} />
        ))}
      </ul>
    </div>
  );
}

const EMPTY_DRIVERS: RunnerDriverInfo[] = [];

function RunnerRow({ runner }: { runner: Runner }) {
  const status = runner.status ?? "unknown";
  const statusClass =
    status === "online" ? "bg-emerald-500" : status === "offline" ? "bg-red-500" : "bg-gray-500";
  const label = runner.name?.trim() || runner.id.slice(0, 8);
  // Subscribe via stable references only — returning a fresh `[]` from
  // the selector tripped React's getSnapshot warning and caused an
  // infinite re-render loop. We pull the map slot directly and fall
  // back to the row's persisted `runner.drivers` (or a hoisted empty
  // array) outside the selector.
  const driversFromStore = useRunnerStore((s) => s.driversByRunnerId[runner.id]);
  const drivers = driversFromStore ?? runner.drivers ?? EMPTY_DRIVERS;
  const selectedRunnerId = useRunnerStore((s) => s.selectedRunnerId);
  const setSelectedRunnerId = useRunnerStore((s) => s.setSelectedRunnerId);
  const isSelected = selectedRunnerId === runner.id;
  const [settingsOpen, setSettingsOpen] = useState(false);

  return (
    <li className="px-4 py-3" data-testid="runner-row">
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-3 min-w-0">
          <span aria-hidden="true" className={`inline-block w-2 h-2 rounded-full ${statusClass}`} />
          <div className="min-w-0">
            <div className="text-sm text-gray-100 font-mono truncate">{label}</div>
            <div className="text-[11px] text-gray-500 capitalize">{status}</div>
          </div>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <DriverInventoryChip drivers={drivers} />
          <div className="text-right text-[11px] text-gray-500">
            {runner.lastSeenAt ? (
              <div>Last seen {formatRelative(runner.lastSeenAt)}</div>
            ) : (
              <div>Never seen</div>
            )}
            {runner.createdAt && <div>Enrolled {formatRelative(runner.createdAt)}</div>}
          </div>
          <button
            type="button"
            onClick={() => setSettingsOpen((v) => !v)}
            aria-expanded={settingsOpen}
            className={`text-[11px] px-2 py-0.5 rounded border transition ${
              settingsOpen
                ? "border-indigo-700/50 bg-indigo-900/30 text-indigo-200"
                : "border-gray-700 bg-gray-900/40 text-gray-400 hover:bg-gray-800 hover:text-gray-200"
            }`}
            data-testid={`runner-settings-toggle-${runner.id}`}
          >
            Settings
          </button>
          <button
            type="button"
            onClick={() => setSelectedRunnerId(runner.id)}
            disabled={isSelected}
            className={`text-[11px] px-2 py-0.5 rounded border transition ${
              isSelected
                ? "border-emerald-700/50 bg-emerald-900/30 text-emerald-300 cursor-default"
                : "border-gray-700 bg-gray-900/40 text-gray-400 hover:bg-gray-800 hover:text-gray-200"
            }`}
            data-testid={`runner-select-${runner.id}`}
          >
            {isSelected ? "Selected" : "Select"}
          </button>
        </div>
      </div>
      <dl className="mt-2 grid grid-cols-2 gap-x-4 gap-y-1 text-[11px] text-gray-500">
        {runner.hostname && (
          <div>
            <dt className="inline text-gray-600">Host: </dt>
            <dd className="inline font-mono text-gray-400">{runner.hostname}</dd>
          </div>
        )}
        {runner.version && (
          <div>
            <dt className="inline text-gray-600">Version: </dt>
            <dd className="inline font-mono text-gray-400">{runner.version}</dd>
          </div>
        )}
      </dl>
      {settingsOpen && <RunnerSettings runnerId={runner.id} />}
      {/* Live tail of the runner's stdout/stderr (T11.1). Rendered inline
          under the selected row instead of on a dedicated `/runners/{id}`
          subroute — the brief mentions that URL but the dashboard's
          existing per-runner UX is the Select toggle, and inline keeps the
          users mental model of "this row is the runner I'm looking at"
          intact. */}
      {isSelected && <RunnerLogPanel runnerId={runner.id} />}
    </li>
  );
}

const INHERIT_VALUE = "__inherit__";

/// Per-runner override for `effort` and `skip_permissions`. Lazy-loads the
/// effective config on first open and persists changes via PUT
/// `/api/runners/{id}/config`. Selecting "Inherit server default" sends
/// `null`, which clears the override server-side.
function RunnerSettings({ runnerId }: { runnerId: string }) {
  const config = useRunnerStore((s) => s.configByRunnerId[runnerId]);
  const fetchRunnerConfig = useRunnerStore((s) => s.fetchRunnerConfig);
  const saveRunnerConfig = useRunnerStore((s) => s.saveRunnerConfig);

  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);

  useEffect(() => {
    if (config) return;
    setLoading(true);
    setError(null);
    fetchRunnerConfig(runnerId)
      .catch((e: unknown) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [runnerId, config, fetchRunnerConfig]);

  async function commit(patch: { effort?: string | null; skipPermissions?: boolean | null }) {
    setSaving(true);
    setError(null);
    try {
      await saveRunnerConfig(runnerId, patch);
      setSavedAt(Date.now());
      setTimeout(() => setSavedAt(null), 2000);
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  }

  if (loading && !config) {
    return <div className="mt-3 text-[11px] text-gray-500">Loading settings…</div>;
  }
  if (!config) {
    return (
      <Banner className="mt-3" data-testid={`runner-settings-error-${runnerId}`}>
        {error ?? "Failed to load runner settings"}
      </Banner>
    );
  }

  const effortOverride = config.override.effort;
  const skipOverride = config.override.skipPermissions;
  // Server default is the *effective* value when the override is null —
  // the absence of an override means the resolved value IS the server
  // default. Cache it so the dropdown can show "Inherit (high)".
  const inheritedEffort = effortOverride === null ? config.effort : null;

  return (
    <div
      className="mt-3 rounded border border-gray-800 bg-gray-950/50 p-3"
      data-testid={`runner-settings-${runnerId}`}
    >
      <h3 className="text-[11px] font-semibold text-gray-300 uppercase tracking-wide mb-2">
        Per-runner settings
      </h3>

      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
        <SettingRow label="Effort" description="Reasoning level passed to agents on this runner.">
          <select
            value={effortOverride ?? INHERIT_VALUE}
            onChange={(e) => {
              const v = e.target.value;
              void commit({ effort: v === INHERIT_VALUE ? null : v });
            }}
            disabled={saving}
            data-testid={`runner-effort-select-${runnerId}`}
            className="bg-gray-800 border border-gray-700 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
          >
            <option value={INHERIT_VALUE}>
              Inherit server default
              {inheritedEffort ? ` (${inheritedEffort})` : ""}
            </option>
            {EFFORT_LEVELS.map((l) => (
              <option key={l.value} value={l.value}>
                {l.label}
              </option>
            ))}
          </select>
        </SettingRow>

        <SettingRow
          label="Skip permissions"
          description={
            <>
              Pass <code className="text-gray-400">--dangerously-skip-permissions</code> for Claude
              agents on this runner.
            </>
          }
        >
          <SkipTriState
            override={skipOverride}
            effective={config.skipPermissions}
            disabled={saving}
            onChange={(next) => void commit({ skipPermissions: next })}
            testId={`runner-skip-select-${runnerId}`}
          />
        </SettingRow>
      </div>

      <div className="mt-2 flex items-center gap-2 text-[11px]">
        {saving && <span className="text-gray-500">Saving…</span>}
        {!saving && savedAt && <span className="text-emerald-400">Saved.</span>}
        {error && <Banner className="grow">{error}</Banner>}
      </div>
    </div>
  );
}

function SettingRow({
  label,
  description,
  children,
}: {
  label: string;
  description: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="text-xs font-medium text-gray-300">{label}</div>
      <p className="text-[11px] text-gray-500 mt-0.5 mb-1.5 leading-relaxed">{description}</p>
      {children}
    </div>
  );
}

function SkipTriState({
  override,
  effective,
  disabled,
  onChange,
  testId,
}: {
  override: boolean | null;
  effective: boolean;
  disabled: boolean;
  onChange: (next: boolean | null) => void;
  testId: string;
}) {
  const value = override === null ? INHERIT_VALUE : override ? "on" : "off";
  // When no override is set, the effective value IS the server default,
  // so showing "(on)/(off)" next to "Inherit" is the closest hint.
  const inheritedHint = override === null ? (effective ? "on" : "off") : null;
  return (
    <select
      value={value}
      onChange={(e) => {
        const v = e.target.value;
        if (v === INHERIT_VALUE) onChange(null);
        else onChange(v === "on");
      }}
      disabled={disabled}
      data-testid={testId}
      className="bg-gray-800 border border-gray-700 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
    >
      <option value={INHERIT_VALUE}>
        Inherit server default{inheritedHint ? ` (${inheritedHint})` : ""}
      </option>
      <option value="on">On</option>
      <option value="off">Off</option>
    </select>
  );
}

/// "N drivers · M ready" chip that expands on hover/click into per-driver
/// auth state. `ready` counts every driver whose state isn't
/// `not_installed` / `unauthenticated` — `unknown` rolls up as ready
/// because we don't want to block on a flaky detector. Empty inventory
/// renders nothing; the runner just hasn't reported yet.
function DriverInventoryChip({ drivers }: { drivers: RunnerDriverInfo[] }) {
  const [open, setOpen] = useState(false);
  if (drivers.length === 0) return null;
  const ready = drivers.filter((d) => isDriverStateReady(d.status)).length;
  const tone =
    ready === 0
      ? "border-red-700/50 bg-red-900/30 text-red-300"
      : ready === drivers.length
        ? "border-emerald-700/50 bg-emerald-900/30 text-emerald-300"
        : "border-amber-700/50 bg-amber-900/30 text-amber-300";
  return (
    <div className="relative" data-testid="runner-driver-chip">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        onMouseEnter={() => setOpen(true)}
        onMouseLeave={() => setOpen(false)}
        className={`text-[11px] px-2 py-0.5 rounded border ${tone} cursor-default`}
      >
        {drivers.length} driver{drivers.length === 1 ? "" : "s"} · {ready} ready
      </button>
      {open && (
        <div
          className="absolute right-0 top-full mt-1 z-10 min-w-[180px] rounded border border-gray-700 bg-gray-950 shadow-lg"
          onMouseEnter={() => setOpen(true)}
          onMouseLeave={() => setOpen(false)}
          data-testid="runner-driver-chip-popover"
        >
          <ul className="py-1 text-[11px]">
            {drivers.map((d) => {
              const ok = isDriverStateReady(d.status);
              return (
                <li key={d.name} className="flex items-center justify-between px-3 py-1">
                  <span className="font-mono text-gray-200">{d.name}</span>
                  <span className={`font-mono ${ok ? "text-emerald-400" : "text-red-400"}`}>
                    {driverStateLabel(d.status)}
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}

function isDriverStateReady(s: RunnerDriverState): boolean {
  return s.state !== "not_installed" && s.state !== "unauthenticated";
}

function driverStateLabel(s: RunnerDriverState): string {
  switch (s.state) {
    case "not_installed":
      return "not installed";
    case "unauthenticated":
      return "needs auth";
    case "oauth":
      return s.account ?? "signed in";
    case "api_key":
      return "API key";
    case "cloud_provider":
      return s.provider;
    default:
      return "unknown";
  }
}
