import { useEffect, useMemo, useState } from "react";
import { Navigate } from "react-router-dom";
import {
  useRunnerStore,
  worstHealthChip,
  type Runner,
  type RunnerDriverInfo,
  type RunnerDriverState,
  type ServerBinStatus,
} from "../stores/runner-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useToastStore } from "../stores/toast-store.js";
import { Banner } from "./ui/Banner.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";
import { RunnerEnrollModal } from "./RunnerEnrollModal.js";
import { RunnerLogPanel } from "./RunnerLogPanel.js";
import { RunnerHealthPanel } from "./RunnerHealthPanel.js";
import { formatRelative } from "../lib/time.js";
import { toastError, toastWarn } from "../lib/toast.js";

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
/// - Shutdown / Revoke (T11.2): each row carries two actions. "Request
///   shutdown" (orange) sends a reliable `WireMessage::ShutdownRequest`
///   over the runner's WS; the runner is expected to drain its session
///   daemons and exit. The button flips to a "Requested at <ts>" label
///   and, after 30s with no `runner_disconnected`, surfaces a "Runner
///   ignored shutdown — revoke instead?" affordance. "Revoke" (red,
///   danger) opens a confirmation modal that calls `DELETE
///   /api/runners/{id}` to drop every runner_token row + soft-delete
///   the runner record. The runner's next reconnect attempt with the
///   now-revoked token gets 401. The current WS stays alive until the
///   runner closes it or the server's keepalive times out — the modal
///   documents this honestly so the user doesn't expect a kill -9.
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
  const [shutdownOpen, setShutdownOpen] = useState(false);
  const [revokeOpen, setRevokeOpen] = useState(false);

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
          <HealthChip runner={runner} />
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
          <RunnerActions
            runner={runner}
            onShutdown={() => setShutdownOpen(true)}
            onRevoke={() => setRevokeOpen(true)}
          />
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
      <ServerBinChip serverBin={runner.serverBin} />
      <VersionSeverityChip runner={runner} />
      {settingsOpen && <RunnerSettings runnerId={runner.id} />}
      {/* Live tail of the runner's stdout/stderr (T11.1) + Health panel
          (T11.3). Rendered inline under the selected row instead of on a
          dedicated `/runners/{id}` subroute — the brief mentions that URL
          but the dashboard's existing per-runner UX is the Select toggle,
          and inline keeps the user's mental model of "this row is the
          runner I'm looking at" intact. */}
      {isSelected && <RunnerHealthPanel runnerId={runner.id} />}
      {isSelected && <RunnerLogPanel runnerId={runner.id} />}
      <ShutdownRunnerModal
        open={shutdownOpen}
        runner={runner}
        onClose={() => setShutdownOpen(false)}
      />
      <RevokeRunnerModal open={revokeOpen} runner={runner} onClose={() => setRevokeOpen(false)} />
    </li>
  );
}

/// 30 seconds after a Request-shutdown click, surface a fallback affordance
/// pointing the operator at Revoke. The brief calls this out explicitly:
/// "if the runner doesn't disconnect within 30s, a 'Runner ignored
/// shutdown — revoke token instead?' secondary affordance appears."
const SHUTDOWN_FALLBACK_MS = 30_000;

/// Action cluster for one runner row: Request shutdown (orange) +
/// Revoke (red). Pulled out of the row so the row's render stays
/// readable. Uses the new `Button` "warn" variant for the orange
/// shutdown button so the audit-log warn pill, banners, and this
/// button all share the WARN palette.
function RunnerActions({
  runner,
  onShutdown,
  onRevoke,
}: {
  runner: Runner;
  onShutdown: () => void;
  onRevoke: () => void;
}) {
  const requestedAt = useRunnerStore((s) => s.shutdownRequestedAt[runner.id] ?? null);

  // Timer to flip into the 30s fallback state. `now` rerenders the row
  // periodically while a request is in flight so the relative label and
  // the fallback affordance update without a separate parent rerender.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (requestedAt === null) return;
    const id = setInterval(() => setNow(Date.now()), 5_000);
    return () => clearInterval(id);
  }, [requestedAt]);

  const isOnline = runner.status === "online";
  const isOffline = runner.status === "offline";
  const elapsed = requestedAt === null ? 0 : now - requestedAt;
  const ignored = requestedAt !== null && elapsed >= SHUTDOWN_FALLBACK_MS && !isOffline;

  return (
    <div className="flex items-center gap-2">
      {requestedAt === null ? (
        <Button
          variant="warn"
          size="sm"
          onClick={onShutdown}
          disabled={!isOnline}
          title={
            isOnline
              ? "Ask the runner to drain its session daemons and exit"
              : "Runner is offline — nothing to ask"
          }
          data-testid={`runner-shutdown-${runner.id}`}
        >
          Request shutdown
        </Button>
      ) : (
        <span
          className="text-[11px] px-2 py-0.5 rounded border border-amber-700/50 bg-amber-900/30 text-amber-200"
          data-testid={`runner-shutdown-requested-${runner.id}`}
        >
          {ignored
            ? "Shutdown ignored"
            : `Requested ${formatRelative(new Date(requestedAt).toISOString())}`}
        </span>
      )}
      {ignored && (
        <Button
          variant="danger"
          size="sm"
          onClick={onRevoke}
          data-testid={`runner-shutdown-fallback-revoke-${runner.id}`}
        >
          Revoke instead
        </Button>
      )}
      <Button
        variant="danger"
        size="sm"
        onClick={onRevoke}
        data-testid={`runner-revoke-${runner.id}`}
      >
        Revoke
      </Button>
    </div>
  );
}

function ShutdownRunnerModal({
  open,
  runner,
  onClose,
}: {
  open: boolean;
  runner: Runner;
  onClose: () => void;
}) {
  const requestRunnerShutdown = useRunnerStore((s) => s.requestRunnerShutdown);
  const agents = useAgentStore((s) => s.agents);
  // Today the server does not tag `agents` rows with their `runner_id`
  // (deployment-wide tally is what /runners shows). For SaaS deployments
  // with a single runner this matches reality; multi-runner deployments
  // get a slightly conservative count (any in-flight agent on the
  // deployment is listed). Callers will not be surprised by an over-
  // count; the alternative (silently under-count) is worse.
  const inFlightAgents = useMemo(
    () => agents.filter((a) => a.status === "running" || a.status === "starting"),
    [agents],
  );
  const [reason, setReason] = useState("");
  const [confirmInFlight, setConfirmInFlight] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const inFlightCount = inFlightAgents.length;
  const needsConfirm = inFlightCount > 0;
  const canSubmit = !submitting && (!needsConfirm || confirmInFlight);

  // Reset the form whenever the modal reopens, otherwise a previous
  // session's "I understand" checkbox state would carry over.
  useEffect(() => {
    if (open) {
      setReason("");
      setConfirmInFlight(false);
      setSubmitting(false);
    }
  }, [open]);

  async function submit() {
    setSubmitting(true);
    try {
      const trimmedReason = reason.trim();
      const resp = await requestRunnerShutdown(runner.id, {
        reason: trimmedReason.length > 0 ? trimmedReason : undefined,
        confirmInFlight: needsConfirm ? confirmInFlight : undefined,
      });
      useToastStore.getState().push({
        kind: "success",
        title: "Shutdown requested",
        body:
          resp.inFlightAgents.length > 0
            ? `${resp.inFlightAgents.length} in-flight agent${
                resp.inFlightAgents.length === 1 ? "" : "s"
              } draining; the runner is expected to exit shortly.`
            : "The runner is expected to exit shortly.",
      });
      onClose();
    } catch (e) {
      toastError(e, "Shutdown request failed");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="Request runner shutdown"
      description={`Ask runner "${runner.name?.trim() || runner.id}" to drain its session daemons and exit. This is a request — the runner can ignore it. To stop a wedged runner, revoke its token instead.`}
    >
      <div className="mt-3 text-xs text-gray-300 space-y-3">
        {needsConfirm ? (
          <div data-testid="shutdown-modal-in-flight">
            <Banner kind="warn">
              <p className="font-medium">
                {inFlightCount} in-flight agent{inFlightCount === 1 ? "" : "s"} will be drained.
              </p>
              <ul className="mt-2 list-disc pl-5 space-y-0.5 text-[11px] font-mono text-gray-400 max-h-32 overflow-y-auto">
                {inFlightAgents.slice(0, 8).map((a) => (
                  <li key={a.id}>
                    {a.id} ({a.task_id ?? "no task"})
                  </li>
                ))}
                {inFlightAgents.length > 8 && (
                  <li className="text-gray-500">… and {inFlightAgents.length - 8} more</li>
                )}
              </ul>
              <p className="mt-2 text-[11px] text-amber-200">
                Drain (graceful): the runner asks each agent's CLI to exit cleanly. Orphan: if the
                runner ignores this request, the host operator's process supervisor still owns the
                kill switch.
              </p>
            </Banner>
          </div>
        ) : (
          <p className="text-gray-400">No agents are currently in flight on this deployment.</p>
        )}

        <label className="block">
          <span className="text-[11px] font-medium text-gray-300">Reason (optional)</span>
          <input
            type="text"
            value={reason}
            onChange={(e) => setReason(e.target.value)}
            placeholder="e.g. host upgrade, draining for maintenance"
            data-testid="shutdown-modal-reason"
            className="mt-1 w-full bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-xs text-gray-200 focus:outline-none focus:border-indigo-600"
          />
          <span className="text-[11px] text-gray-500 mt-1 block">
            Echoed in the runner's `[runner] shutdown requested by SaaS:` log line.
          </span>
        </label>

        {needsConfirm && (
          <label className="flex items-start gap-2 text-[11px] text-gray-300">
            <input
              type="checkbox"
              checked={confirmInFlight}
              onChange={(e) => setConfirmInFlight(e.target.checked)}
              className="mt-0.5"
              data-testid="shutdown-modal-confirm"
            />
            <span>
              I understand this stops {inFlightCount} in-flight agent
              {inFlightCount === 1 ? "" : "s"}.
            </span>
          </label>
        )}
      </div>

      <div className="mt-5 flex items-center justify-end gap-2">
        <Button variant="secondary" size="sm" onClick={onClose} disabled={submitting}>
          Cancel
        </Button>
        <Button
          variant="warn"
          size="sm"
          onClick={() => void submit()}
          disabled={!canSubmit}
          loading={submitting}
          data-testid="shutdown-modal-submit"
        >
          Request shutdown
        </Button>
      </div>
    </Modal>
  );
}

function RevokeRunnerModal({
  open,
  runner,
  onClose,
}: {
  open: boolean;
  runner: Runner;
  onClose: () => void;
}) {
  const revokeRunner = useRunnerStore((s) => s.revokeRunner);
  const [submitting, setSubmitting] = useState(false);

  async function submit() {
    setSubmitting(true);
    try {
      const resp = await revokeRunner(runner.id);
      useToastStore.getState().push({
        kind: "success",
        title: "Runner revoked",
        body: `${resp.tokensRevoked} token${resp.tokensRevoked === 1 ? "" : "s"} revoked. The runner cannot reconnect without re-running the install command.`,
      });
      onClose();
    } catch (e) {
      toastError(e, "Revoke failed");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="Revoke runner"
      description={`Revoke runner "${runner.name?.trim() || runner.id}". This deletes its enrolment tokens and removes it from the active list.`}
    >
      <div className="mt-3 space-y-3" data-testid="revoke-modal-warning">
        <Banner kind="warn">
          <p className="font-medium">This action cannot be undone.</p>
          <p className="mt-1 text-[11px] text-amber-200">
            The runner will be unable to reconnect without re-running the install command. The
            current connection stays alive until the runner closes it or the server's keepalive
            timeout fires — revoke is not a kill -9.
          </p>
        </Banner>
      </div>
      <div className="mt-5 flex items-center justify-end gap-2">
        <Button variant="secondary" size="sm" onClick={onClose} disabled={submitting}>
          Cancel
        </Button>
        <Button
          variant="danger"
          size="sm"
          onClick={() => void submit()}
          disabled={submitting}
          loading={submitting}
          data-testid="revoke-modal-submit"
        >
          Revoke runner
        </Button>
      </div>
    </Modal>
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
/// Worst-metric chip on the runner summary list (T11.3).
///
/// Renders the single most-actionable signal so a busy operator scanning
/// the `/runners` page sees the row that needs attention without
/// expanding it. Selection rule + thresholds live in `worstHealthChip`
/// in runner-store; this component is the rendering side. Returns `null`
/// when no metric crosses its threshold (the row is healthy).
function HealthChip({ runner }: { runner: Runner }) {
  const chip = worstHealthChip(runner);
  if (chip.severity === "none") return null;
  const tone =
    chip.severity === "danger"
      ? "border-red-700/50 bg-red-900/30 text-red-300"
      : "border-amber-700/50 bg-amber-900/30 text-amber-300";
  const icon = chip.severity === "danger" ? "⛔" : "⚠";
  return (
    <span
      className={`text-[11px] px-2 py-0.5 rounded border ${tone} font-mono`}
      data-testid="runner-health-chip"
    >
      <span aria-hidden="true">{icon} </span>
      {chip.label}
    </span>
  );
}

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

/// T1.2 server-bin self-diagnostic chip. Renders inline under the runner
/// row right after the host/version table so a missing `branchwork-server`
/// surfaces BEFORE the user clicks Start session — no more silent gap
/// where the operator has to wait for the inevitable `AgentSpawnFailed`.
///
/// Three states:
///   - `found`: green check + absolute path (monospace).
///   - `not found`: red cross + reason ("not on $PATH: branchwork-server").
///   - neutral / hidden: serverBin undefined (older runner that doesn't
///     report the field) — render nothing rather than fake a verdict.
export function ServerBinChip({ serverBin }: { serverBin?: ServerBinStatus }) {
  if (!serverBin) return null;
  if (serverBin.path) {
    return (
      <div className="mt-1 text-[11px]" data-testid="server-bin-chip">
        <span className="inline-flex items-center gap-1 rounded border border-emerald-700/50 bg-emerald-900/30 px-2 py-0.5 text-emerald-300">
          <span aria-hidden="true">✓</span>
          <span className="font-mono break-all">{serverBin.path}</span>
        </span>
        <span className="sr-only">branchwork-server found at {serverBin.path}</span>
      </div>
    );
  }
  if (serverBin.error) {
    return (
      <div className="mt-1 text-[11px]" data-testid="server-bin-chip">
        <span className="inline-flex items-center gap-1 rounded border border-red-700/50 bg-red-900/30 px-2 py-0.5 text-red-300">
          <span aria-hidden="true">✗</span>
          <span className="font-mono break-all">{serverBin.error}</span>
        </span>
        <span className="sr-only">branchwork-server not found: {serverBin.error}</span>
      </div>
    );
  }
  // Both fields null is a malformed payload (server should set one or the
  // other). Treat as "not reported" and render nothing rather than fake.
  return null;
}

/// T1.3 version-vs-server severity chip. Renders the operator-actionable
/// color verdict (green / amber / red) the server-side `to_severity()`
/// rule produced, with an inline "Connect anyway" override button on the
/// red path so the operator can opt in to "I know what I'm doing" without
/// leaving the row.
///
/// - `green` ⇒ Ok or Patch difference. Chip renders as a neutral note
///   (not surfaced when no version is reported) so a healthy runner
///   stays visually quiet.
/// - `amber` ⇒ 1.x+ minor diff. Chip warns but does NOT block.
/// - `red` ⇒ pre-1.0 minor diff OR cross-major. Chip blocks dispatch
///   unless the operator clicks "Connect anyway", which sets
///   `versionMismatchOverride` on the runner row and flips the chip to
///   "Overridden" (still red palette so the risk stays visible).
///
/// Same back-compat rule as `ServerBinChip`: render nothing when the
/// field is missing (older server build that hasn't shipped severity
/// yet) rather than fake a verdict.
export function VersionSeverityChip({ runner }: { runner: Runner }) {
  const setOverride = useRunnerStore((s) => s.setVersionMismatchOverride);
  const [busy, setBusy] = useState(false);
  const severity = runner.versionSeverity;
  if (!severity || severity === "green") return null;

  const runnerVersion = runner.version ?? "?";
  const isRed = severity === "red";
  const isOverridden = runner.versionMismatchOverride === true;

  // Color the chip from the canonical severity (not the override
  // state). Even when overridden, the underlying mismatch is still red
  // — surface that so the operator notices when they come back later.
  const palette = isRed
    ? "border-red-700/50 bg-red-900/30 text-red-300"
    : "border-amber-700/50 bg-amber-900/30 text-amber-300";
  const glyph = isRed ? "⛔" : "⚠";
  const label = isRed
    ? "runner too old — upgrade required"
    : `runner version ${runnerVersion} differs from server`;

  async function handleToggle() {
    setBusy(true);
    try {
      await setOverride(runner.id, !isOverridden);
    } catch (e) {
      toastError(e, "Failed to update override");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mt-1 text-[11px]" data-testid="version-severity-chip">
      <span className={`inline-flex items-center gap-1 rounded border px-2 py-0.5 ${palette}`}>
        <span aria-hidden="true">{glyph}</span>
        <span className="font-mono break-all">{label}</span>
        {isOverridden && (
          <span
            className="ml-1 rounded bg-red-950 px-1 text-[10px] text-red-200"
            title="Operator opted into dispatching to this runner despite the version mismatch"
          >
            Overridden
          </span>
        )}
      </span>
      {isRed && (
        <Button
          variant="warn"
          size="sm"
          onClick={handleToggle}
          disabled={busy}
          className="ml-2"
          data-testid={`version-mismatch-override-${runner.id}`}
          title={
            isOverridden
              ? "Re-enable the dispatch block on this runner"
              : "I know what I'm doing — connect anyway"
          }
        >
          {isOverridden ? "Re-block" : "Connect anyway"}
        </Button>
      )}
      <span className="sr-only">
        Version severity {severity}; runner version {runnerVersion}
        {isOverridden ? "; operator override active" : ""}
      </span>
    </div>
  );
}
