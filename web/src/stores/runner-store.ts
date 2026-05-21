import { create } from "zustand";
import { deleteJson, fetchJson, postJson, putJson } from "../api.js";

/// Deployment mode discriminator returned by `GET /api/runners`. Drives
/// whether the `RunnerStatus` indicator renders at all (audit §17 / 4.1):
///
/// - `standalone`: the dashboard owns the local filesystem; no runners are
///   expected. Indicator renders nothing.
/// - `saas`: this deployment expects a remote runner. Indicator surfaces
///   amber/red/emerald based on registration + connection state.
///
/// Server-side rule (`server-rs/src/saas/runner_ws.rs::deployment_mode`):
/// `saas` if either `BRANCHWORK_PUBLIC_URL` is set OR the org already has
/// any runner row; `standalone` otherwise. Server is the only component
/// that knows both signals so the dashboard reads a pre-computed answer
/// instead of duplicating the rule.
export type DeploymentMode = "standalone" | "saas" | "unknown";

/// Verdict for the per-runner version chip. Same vocabulary as the server
/// (`server-rs/src/saas/runner_ws.rs::VersionMismatch::as_str`).
export type VersionMismatch = "ok" | "patch" | "minor" | "major";

/// Coarser color verdict the dashboard chip + dispatcher gate consult
/// (T1.3 of the runner-install-and-spawn-reliability plan). Mirrors
/// `server-rs/src/saas/runner_ws.rs::Severity::as_str`. The rule:
/// - `green` ⇒ Ok or Patch (protocol-compatible)
/// - `amber` ⇒ pre-1.0 Patch (subsumed under Green) or 1.x+ Minor diff
/// - `red`   ⇒ pre-1.0 Minor diff OR any Major diff. Dispatcher refuses
///   to send StartAgent to a red runner unless `versionMismatchOverride`
///   is set on the row.
export type VersionSeverity = "green" | "amber" | "red";

/// Snapshot of one runner's health metrics. Populated from
/// `WireMessage::RunnerHealth` ticks (~30 s cadence) and persisted in the
/// `runners` table — every field NULL until the runner has reported once.
export interface RunnerHealth {
  outboxDepth: number | null;
  wsReconnects24h: number | null;
  ciPollMsP50: number | null;
  ciPollMsP99: number | null;
  /// ISO 8601 wall-clock for the last applied health snapshot. Lets the UI
  /// distinguish "currently green" from "was green 5 minutes ago" when the
  /// runner drops offline.
  lastHealthAt: string | null;
  /// Trailing-24 h count of session daemons reaped by the runner (Task
  /// 11.6). Bumped each time the runner finds a stale `<id>.sock` on
  /// startup or `waitpid`s a detached zombie. `null` until the runner
  /// has reported once with the field. The dashboard chip surfaces a
  /// non-zero value so the user knows the silent-rot failure mode
  /// happened, even after the cleanup ran.
  orphansReaped24h: number | null;
}

/// One point on the per-runner sparkline. `ts` is ms-since-epoch; `p50`
/// and `p99` are nullable because cold runners surface `—` rather than
/// faking a 0-ms data point.
export interface RunnerHealthSample {
  ts: number;
  p50: number | null;
  p99: number | null;
}

/// Trailing-window cap for the sparkline ring. 120 ticks @ 30 s ≈ 60 min,
/// matching the panel doc's window claim.
export const RUNNER_HEALTH_HISTORY_CAP = 120;

export interface Runner {
  id: string;
  name: string | null;
  status: string | null;
  hostname: string | null;
  version: string | null;
  lastSeenAt: string | null;
  createdAt: string | null;
  /// Last-known driver inventory the runner pushed. Persisted server-side
  /// in `runners.drivers_json` so this is set even when `status==offline`.
  /// Empty array means the runner has not reported yet (or the deserialize
  /// failed — server falls back to `[]` rather than failing the row).
  drivers?: RunnerDriverInfo[];
  /// Health metrics snapshot. All fields nullable — runners that haven't
  /// reported a `RunnerHealth` yet show `null` and the dashboard chip
  /// stays neutral.
  health?: RunnerHealth;
  /// `ok | patch | minor | major`. Computed server-side by comparing the
  /// runner's reported version against the dashboard's compiled-in
  /// `CARGO_PKG_VERSION`. `ok` for an exact match OR an unparseable
  /// runner version (custom build).
  versionMismatch?: VersionMismatch;
  /// T1.3: coarser color verdict the dashboard chip + dispatcher gate
  /// consult. `green` / `amber` / `red`. Derived server-side from
  /// `versionMismatch` + the server's own version (pre-1.0 minor diff
  /// is red, 1.x+ minor diff is amber). `undefined` ⇒ older server build
  /// that didn't ship the field; UI falls back to neutral.
  versionSeverity?: VersionSeverity;
  /// T1.3: operator-set override that lifts the version-mismatch dispatch
  /// block. When `true`, `startTask` no longer refuses to dispatch even
  /// if `versionSeverity === "red"`. Defaults to `false` for pre-T1.3
  /// rows. Toggled via `POST /api/runners/{id}/version-mismatch-override`.
  versionMismatchOverride?: boolean;
  /// T1.2 self-diagnostic: the runner's startup attempt to resolve
  /// `branchwork-server`. `path` is set when the runner found a real file;
  /// `error` is set when the lookup failed (e.g. `"not on $PATH:
  /// branchwork-server"`). BOTH null ⇒ older runner that never reported
  /// the field; dashboard renders a neutral chip in that case rather
  /// than faking a green check.
  serverBin?: ServerBinStatus;
  /// T4.3: runner-detected upgrade availability. Set by the runner via
  /// (a) the per-connect `Resume.server_version` drift check, or (b) the
  /// periodic `install-runner.sh` poll for offline-ish runners. Distinct
  /// from `versionSeverity` (which gates dispatch) — a patch-only drift
  /// is `severity=green` but `upgradeAvailable=true`, letting the
  /// dashboard offer the Upgrade button without blocking dispatch.
  ///
  /// `undefined`/`false` ⇒ no upgrade signal; UI hides the
  /// auto-detected pill (but the existing T1.3 severity chip still
  /// renders independently on amber/red drift).
  upgradeAvailable?: boolean;
}

/// Server-bin self-diagnostic shipped on `RunnerHello` and surfaced on
/// the runner row next to the version chip. Lets the operator see at a
/// glance whether the runner can actually spawn session daemons —
/// closes the silent gap where a missing `branchwork-server` only
/// surfaced via `AgentSpawnFailed` after the first Start session click.
export interface ServerBinStatus {
  /// Absolute (canonicalised when possible) path to the resolved
  /// binary. Set on success. `null` on the not-found path.
  path: string | null;
  /// Short human label describing the resolution failure, e.g.
  /// `"not on $PATH: branchwork-server"` or `"path does not exist:
  /// /usr/local/bin/branchwork-server"`. Set on failure; `null` on
  /// success.
  error: string | null;
}

/// Deployment-wide agent counts grouped by `agents.status`. Populated by
/// `GET /api/runners` (per-org `SELECT status, COUNT(*) GROUP BY status`).
/// Per-runner attribution is not feasible today: the agents table doesn't
/// carry `runner_id` (T4.2 learning), so the dashboard's `Health` panel
/// labels these honestly as "deployment-wide" rather than faking per-runner.
export type AgentCounts = Record<string, number>;

interface RunnersResponse {
  runners: Runner[];
  mode?: "standalone" | "saas";
  agentCounts?: AgentCounts;
  serverVersion?: string;
}

const SELECTED_RUNNER_STORAGE_KEY = "branchwork.selectedRunnerId";

function readPersistedSelectedRunnerId(): string | null {
  if (typeof window === "undefined") return null;
  try {
    const v = window.localStorage?.getItem(SELECTED_RUNNER_STORAGE_KEY);
    return v && v.length > 0 ? v : null;
  } catch {
    return null;
  }
}

function writePersistedSelectedRunnerId(id: string | null): void {
  if (typeof window === "undefined") return;
  try {
    if (id) window.localStorage?.setItem(SELECTED_RUNNER_STORAGE_KEY, id);
    else window.localStorage?.removeItem(SELECTED_RUNNER_STORAGE_KEY);
  } catch {
    // Storage may be disabled (private mode, quota, …); selection
    // simply won't persist.
  }
}

/// Server response for `POST /api/runners/tokens`. The 32-byte hex
/// token is shown to the operator exactly once — there is no `GET` to
/// re-read it later (only the SHA-256 hash is persisted server-side).
export interface RunnerTokenIssued {
  token: string;
  runner_name: string;
}

/// Server response for `POST /api/runners/install-command` (4.8). Bundles
/// the freshly-issued token, the ready-to-paste `curl … | sh -s -- '<TOKEN>'`
/// command, the resolved SaaS URL, and the runner_name as the dashboard
/// will key WS `runner_connected` events. The token field is kept on the
/// response so a clipboard-aware modal can fall back to copying the raw
/// token if the operator wants to paste it into a custom installer.
export interface RunnerInstallCommand {
  token: string;
  command: string;
  runner_name: string;
  saas_url: string;
}

/// Snapshot of one driver's auth state on a runner. Mirrors the
/// server-side `DriverAuthInfo` wire shape (snake_case `state` tag) and
/// is surfaced verbatim by `GET /api/runners` per row, so each row can
/// render its inventory chip without an extra `/api/drivers?runner_id=`
/// round-trip. This is the runner-protocol shape, not the dashboard's
/// `AuthStatus` (which uses a `kind` tag and is built by the backend's
/// `list_drivers_dispatch` for `/api/drivers` callers).
export type RunnerDriverState =
  | { state: "not_installed" }
  | { state: "unauthenticated"; help?: string | null }
  | { state: "oauth"; account?: string | null }
  | { state: "api_key" }
  | { state: "cloud_provider"; provider: string }
  | { state: "unknown" };

export interface RunnerDriverInfo {
  name: string;
  status: RunnerDriverState;
}

/// One line of runner stdout/stderr the dashboard tails on
/// `/runners/{id}`. Pushed by `runner_log` WS events; rendered by
/// `RunnerLogPanel`. The local `id` field is a per-runner monotonic
/// counter so React keys stay stable when the ring evicts.
export interface RunnerLogEntry {
  id: number;
  ts: string;
  level: string;
  line: string;
}

/// Maximum number of lines kept in the per-runner ring. Older entries are
/// dropped (FIFO) once the cap is hit. Matches the brief's documented
/// "last N lines (in-memory ring, default 500)".
export const RUNNER_LOG_RING_CAP = 500;

/// Per-runner config response (`GET /api/runners/{id}/config`). `effort`
/// and `skipPermissions` are the *effective* (resolved) values; `override`
/// captures what was set explicitly so the UI can render "Inherit server
/// default" vs "Explicit override that happens to equal the default".
export interface RunnerConfig {
  runnerId: string;
  effort: string;
  skipPermissions: boolean;
  override: {
    effort: string | null;
    skipPermissions: boolean | null;
  };
}

/// Body for `PUT /api/runners/{id}/config`. Each field uses
/// `null | string | boolean | undefined`:
/// - `undefined` → omit from the body, don't change.
/// - `null` → clear the override (back to inherit).
/// - typed value → set the override.
export interface RunnerConfigPatch {
  effort?: string | null;
  skipPermissions?: boolean | null;
}

/// Server response for `POST /api/runners/{id}/shutdown`. `inFlightAgents`
/// is the snapshot of agents the server enumerated when it enqueued the
/// `ShutdownRequest` wire frame — the dashboard echoes the count back into
/// the toast so the operator sees what they just signed off on. The runner
/// is responsible for draining these gracefully; the server cannot force-
/// kill them.
export interface RunnerShutdownResponse {
  queued: boolean;
  seq: number;
  inFlightAgents: string[];
}

/// Server response for `DELETE /api/runners/{id}`. `tokensRevoked` is the
/// number of `runner_tokens` rows the server deleted; usually 1, but a
/// runner may have been re-tokened without a previous revoke so the count
/// is informative rather than authoritative.
export interface RunnerRevokeResponse {
  revoked: boolean;
  tokensRevoked: number;
}

/// Server response for `POST /api/runners/{id}/upgrade`. The server has
/// enqueued an `UpgradeRunner` wire frame; whether the runner picks it up
/// and successfully swaps binaries is observed via subsequent `runner_log`
/// lines and the eventual reconnect carrying the new `RunnerHello.version`.
export interface RunnerUpgradeResponse {
  queued: boolean;
  seq: number;
}

interface RunnerStore {
  /// Resolved deployment mode. Starts as `unknown` so the indicator stays
  /// hidden until we know the answer (avoids flashing the amber "register
  /// a runner" prompt on a standalone dashboard during boot).
  mode: DeploymentMode;
  runners: Runner[];
  /// Per-runner driver inventory, keyed by `runner.id`. Populated from the
  /// `runner_drivers` WS event AND from `/api/runners` rows (each row
  /// carries its last-known inventory). The RunnersPage chip reads from
  /// this map; the sidebar's DriverStatusList prefers the typed map for
  /// the selected runner's row but still calls /api/drivers for the
  /// dashboard-shaped `AuthStatus.kind` payload it needs.
  driversByRunnerId: Record<string, RunnerDriverInfo[]>;
  /// Currently-active runner the dashboard targets for driver lookups
  /// and per-runner views. Defaults to `null` (no selection) until the
  /// first runner row is observed; `applyConnected` and `fetchRunners`
  /// auto-select the first runner if none is selected. Persisted to
  /// localStorage so a returning user lands on the same runner they
  /// left on (audit §17 multi-runner UX).
  selectedRunnerId: string | null;
  /// `false` until the first `fetchRunners()` resolves. Indicator subscribes
  /// to this so it can decide whether `mode === "unknown"` is "still loading"
  /// vs "fetch errored".
  loaded: boolean;
  /// ms-since-epoch of the last successful fetch. Mirrors the debounce
  /// contract used by plan/agent/settings stores.
  lastRunnersFetchedAt: number | null;

  fetchRunners: () => Promise<void>;
  /// User-selected runner the rest of the dashboard targets. Persists to
  /// localStorage so the choice survives reloads. Pass `null` to clear.
  setSelectedRunnerId: (id: string | null) => void;
  /// Create a runner enrolment token. The full token is only present
  /// in this response — the dashboard MUST surface it to the operator
  /// immediately (the install-command modal in `RunnersPage`) because
  /// nothing stores it. On success we refetch `/api/runners` so a row
  /// appears once the runner connects via `branchwork-runner --token`.
  createRunnerToken: (runnerName: string) => Promise<RunnerTokenIssued>;
  /// Mint a token AND format the one-line install command. Used by the
  /// `/runners` page's Add-runner modal (4.8). Same DB write path as
  /// `createRunnerToken`, just with the formatted command appended so
  /// the modal shows a copy-pasteable `curl … | sh` line. The runner
  /// name on the response is the trimmed value the server stored — the
  /// modal subscribes to `runner_connected` WS events keyed by this
  /// name to flip to its "Connected!" state.
  fetchInstallCommand: (runnerName: string) => Promise<RunnerInstallCommand>;
  /// Wired from `ws-store.ts` for `runner_connected`. Optimistically inserts
  /// or updates the row so the indicator flips to emerald the moment the
  /// runner registers, without waiting for a refetch.
  applyConnected: (payload: { runner_id: string; runner_name?: string | null }) => void;
  /// Wired from `ws-store.ts` for `runner_disconnected`. Marks the matching
  /// row offline; the indicator flips amber/red without a refetch.
  applyDisconnected: (payload: { runner_id: string }) => void;
  /// Wired from `ws-store.ts` for `runner_drivers`. Updates the runner's
  /// `lastSeenAt` AND its entry in `driversByRunnerId` so the RunnersPage
  /// chip reflects the latest auth state without a refetch. Drivers are
  /// optional: 4.1's call site passes only `runner_id`; 4.5+ passes the
  /// payload's `drivers` array as well.
  applyDriversTouch: (payload: { runner_id: string; drivers?: RunnerDriverInfo[] }) => void;
  /// Wired from `ws-store.ts` for `runner_health` (T11.3). Patches the
  /// matching runner row's `health` snapshot + `versionMismatch` so the
  /// per-row chip and Health panel update without a refetch. Drops the
  /// payload silently when the runner row is unknown (a future fetchRunners
  /// will reconcile).
  applyHealth: (payload: {
    runner_id: string;
    outbox_depth: number;
    ws_reconnects_24h: number;
    ci_poll_ms_p50: number | null;
    ci_poll_ms_p99: number | null;
    /// Optional for back-compat: older runner builds don't include this
    /// field. The store treats missing/null as 0 for chip purposes.
    orphans_reaped_24h?: number | null;
    version_mismatch: VersionMismatch | string;
    /// T1.3: coarser color verdict. Optional for back-compat with older
    /// server builds that haven't shipped the field yet.
    version_severity?: VersionSeverity | string;
    /// T1.3: operator-set override flag echoed back on every health tick
    /// so the store stays in sync. Optional for back-compat.
    version_mismatch_override?: boolean;
  }) => void;
  /// T1.3: POST /api/runners/{id}/version-mismatch-override. Used by the
  /// "Connect anyway" button on the runner row when severity is red.
  setVersionMismatchOverride: (
    runnerId: string,
    override: boolean,
  ) => Promise<{ runnerId: string; override: boolean }>;
  /// Deployment-wide agent counts surfaced by `/api/runners`. Updated on
  /// fetch; not driven by WS today (the existing `agent_started`/
  /// `agent_stopped` events update agent-store but the per-org tally
  /// here would need a re-aggregate that is cheaper to do server-side).
  agentCounts: AgentCounts;
  /// Server version pinned at build time (`env!("CARGO_PKG_VERSION")`).
  /// Surfaced so the version chip can render `runner_version → server_version`
  /// without a separate endpoint.
  serverVersion: string | null;
  /// Per-runner log ring, keyed by `runner.id`. Pushed by the `runner_log`
  /// WS handler in `ws-store.ts`; consumed by `RunnerLogPanel`. Capped at
  /// `RUNNER_LOG_RING_CAP` lines per runner, FIFO eviction.
  logsByRunnerId: Record<string, RunnerLogEntry[]>;
  /// Push one line into the ring for `runnerId`. Drops the oldest entry
  /// when the cap is hit. Side-effect-only — no fetch, no toast.
  pushRunnerLog: (runnerId: string, entry: { ts: string; level: string; line: string }) => void;
  /// Per-runner config cache, keyed by `runner.id`. Populated lazily by
  /// `fetchRunnerConfig`; the Settings expander on RunnersPage subscribes
  /// here so the values survive a row re-render.
  configByRunnerId: Record<string, RunnerConfig>;
  /// Trailing CI-poll latency history per runner, populated from
  /// `RunnerHealth` WS events (one point per ~30 s tick). The last
  /// `RUNNER_HEALTH_HISTORY_CAP` points are kept; the panel sparkline
  /// reads from here. Each entry stores both p50 and p99 + the apply
  /// timestamp so the panel can render a 60-minute window without an
  /// extra fetch.
  healthHistoryByRunnerId: Record<string, RunnerHealthSample[]>;
  /// Fetch (or refresh) `GET /api/runners/{id}/config` and cache the result
  /// under `configByRunnerId[id]`. The Settings expander calls this on first
  /// open.
  fetchRunnerConfig: (runnerId: string) => Promise<RunnerConfig>;
  /// PUT `/api/runners/{id}/config`. The server returns the same shape as
  /// GET so we update `configByRunnerId` from the response — no follow-up
  /// fetch needed.
  saveRunnerConfig: (runnerId: string, patch: RunnerConfigPatch) => Promise<RunnerConfig>;
  /// POST `/api/runners/{id}/shutdown`. Reliable: the server enqueues a
  /// `WireMessage::ShutdownRequest` that survives a runner-side disconnect.
  /// The runner is expected to drain its session daemons and exit. If it
  /// ignores the request (wedged event loop, etc.), the dashboard's
  /// secondary affordance is `revokeRunner`.
  requestRunnerShutdown: (
    runnerId: string,
    body: { reason?: string; confirmInFlight?: boolean },
  ) => Promise<RunnerShutdownResponse>;
  /// DELETE `/api/runners/{id}`. Server-side: deletes every
  /// `runner_tokens` row claimed by this runner (the next reconnect
  /// attempt with the now-revoked token gets 401 from the WS upgrade
  /// handler) and soft-deletes the runner row. Updates the in-memory
  /// store so the row disappears immediately.
  revokeRunner: (runnerId: string) => Promise<RunnerRevokeResponse>;
  /// POST `/api/runners/{id}/upgrade`. Reliable: the server enqueues a
  /// `WireMessage::UpgradeRunner` that survives a runner-side disconnect.
  /// The runner is expected to fetch the install script, swap binaries,
  /// and restart via systemd. Successful upgrade is observed when the
  /// runner reconnects with a new `RunnerHello.version` and the row's
  /// `versionSeverity` flips to green; until then the row stays where
  /// it was, so a no-op click against an already-up-to-date runner is
  /// silent (the install script exits 0 without restarting the unit).
  upgradeRunner: (
    runnerId: string,
    body?: { reason?: string },
  ) => Promise<RunnerUpgradeResponse>;
  /// Stamp the time of the most recent `upgradeRunner` call so the row
  /// can render "Upgrade requested <Xm ago>" while the script is fetching
  /// + installing. Cleared when the runner reconnects with a fresh
  /// version (`applyConnected`).
  upgradeRequestedAt: Record<string, number>;
  /// Track the moment a shutdown was requested for `runnerId`, so the UI
  /// can show "Requested at <ts>" and surface the 30-second "runner
  /// ignored shutdown" fallback affordance. Cleared by `applyDisconnected`
  /// when the runner actually drops the connection (the cooperative
  /// path) and by `revokeRunner` (the escalation path).
  shutdownRequestedAt: Record<string, number>;
  /// Drop everything back to its initial shape. Driven by `reset-all.ts` on
  /// logout so user A's runner inventory doesn't bleed into user B's tab.
  reset: () => void;
}

let inFlightRunnersFetch: Promise<void> | null = null;

export function getInFlightRunnersFetch(): Promise<void> | null {
  return inFlightRunnersFetch;
}

const INITIAL_STATE: Pick<
  RunnerStore,
  | "mode"
  | "runners"
  | "loaded"
  | "lastRunnersFetchedAt"
  | "driversByRunnerId"
  | "selectedRunnerId"
  | "configByRunnerId"
  | "logsByRunnerId"
  | "shutdownRequestedAt"
  | "upgradeRequestedAt"
  | "agentCounts"
  | "serverVersion"
  | "healthHistoryByRunnerId"
> = {
  mode: "unknown",
  runners: [],
  loaded: false,
  lastRunnersFetchedAt: null,
  driversByRunnerId: {},
  selectedRunnerId: null,
  configByRunnerId: {},
  logsByRunnerId: {},
  shutdownRequestedAt: {},
  upgradeRequestedAt: {},
  agentCounts: {},
  serverVersion: null,
  healthHistoryByRunnerId: {},
};

/// Per-runner monotonic id counter for `RunnerLogEntry.id`. Module-scoped
/// so a remount of `RunnerLogPanel` doesn't reset keys mid-stream.
const runnerLogIdSeq: Record<string, number> = {};

export const useRunnerStore = create<RunnerStore>((set, get) => ({
  ...INITIAL_STATE,
  selectedRunnerId: readPersistedSelectedRunnerId(),

  fetchRunners: () => {
    if (inFlightRunnersFetch) return inFlightRunnersFetch;
    const promise = (async () => {
      try {
        const data = await fetchJson<RunnersResponse>("/api/runners");
        const runners = data.runners ?? [];
        // Build (or rebuild) the per-runner drivers map from the row
        // payload. /api/runners is authoritative for "drivers a runner
        // ever reported" — we drop entries for runners that were removed
        // and refresh entries for runners that just reported.
        const driversByRunnerId: Record<string, RunnerDriverInfo[]> = {};
        for (const r of runners) {
          if (r.drivers && r.drivers.length > 0) {
            driversByRunnerId[r.id] = r.drivers;
          }
        }
        // Seed `selectedRunnerId` to the first runner if the user hasn't
        // chosen one OR the persisted id no longer exists. The
        // most-recent-first server ordering means the freshest runner
        // wins by default.
        const currentSelected = get().selectedRunnerId;
        const selectedStillValid =
          currentSelected !== null && runners.some((r) => r.id === currentSelected);
        const selectedRunnerId = selectedStillValid ? currentSelected : (runners[0]?.id ?? null);
        if (selectedRunnerId !== currentSelected) {
          writePersistedSelectedRunnerId(selectedRunnerId);
        }
        set({
          mode: data.mode ?? "unknown",
          runners,
          driversByRunnerId,
          selectedRunnerId,
          loaded: true,
          lastRunnersFetchedAt: Date.now(),
          agentCounts: data.agentCounts ?? {},
          serverVersion: data.serverVersion ?? null,
        });
      } catch {
        // Leave `loaded` false on error so the indicator stays hidden until
        // a future reconnect/refetch succeeds. A failed fetch is not a SaaS
        // signal; surfacing red on a transient 5xx would be misleading.
        set({ loaded: false });
      }
    })();
    inFlightRunnersFetch = promise;
    promise.finally(() => {
      if (inFlightRunnersFetch === promise) inFlightRunnersFetch = null;
    });
    return promise;
  },

  setSelectedRunnerId: (id) => {
    set({ selectedRunnerId: id });
    writePersistedSelectedRunnerId(id);
  },

  createRunnerToken: async (runnerName) => {
    const issued = await postJson<RunnerTokenIssued>("/api/runners/tokens", {
      runner_name: runnerName,
    });
    // Don't await — the modal needs the token NOW; the runner row only
    // appears once the operator runs `branchwork-runner --token` and the
    // WS handshake lands. The refetch is a best-effort warmup so a row
    // shows up faster if the runner is already running. WS
    // `runner_connected` will reconcile in any case.
    void get().fetchRunners();
    return issued;
  },

  fetchInstallCommand: async (runnerName) => {
    const issued = await postJson<RunnerInstallCommand>("/api/runners/install-command", {
      runner_name: runnerName,
    });
    // Same warmup as createRunnerToken: prefetch /api/runners so the
    // row appears as soon as the runner connects, without waiting for
    // the next page-level refetch.
    void get().fetchRunners();
    return issued;
  },

  applyConnected: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      // Auto-select the freshly-connected runner if no selection is
      // active yet. This makes the dashboard's first runner the default
      // target without any extra user click.
      const autoSelected = s.selectedRunnerId === null ? payload.runner_id : s.selectedRunnerId;
      if (autoSelected !== s.selectedRunnerId) {
        writePersistedSelectedRunnerId(autoSelected);
      }
      // T4.1: a reconnect is the success signal for an in-flight upgrade
      // — the runner restarted via systemd and is back, so clear the
      // "Upgrade requested" label. Idempotent: clearing a runner that
      // never had an entry is a no-op.
      const nextUpgradeRequestedAt = { ...s.upgradeRequestedAt };
      delete nextUpgradeRequestedAt[payload.runner_id];
      if (idx >= 0) {
        const next = s.runners.slice();
        next[idx] = {
          ...next[idx],
          status: "online",
          name: payload.runner_name ?? next[idx].name ?? null,
          lastSeenAt: nowIso,
        };
        return {
          runners: next,
          selectedRunnerId: autoSelected,
          upgradeRequestedAt: nextUpgradeRequestedAt,
        };
      }
      // New runner — server's broadcast arrived before any `/api/runners`
      // refetch carried the row. Synthesize a row so the indicator can flip
      // emerald immediately; later refetches reconcile the missing fields.
      return {
        runners: [
          {
            id: payload.runner_id,
            name: payload.runner_name ?? null,
            status: "online",
            hostname: null,
            version: null,
            lastSeenAt: nowIso,
            createdAt: null,
          },
          ...s.runners,
        ],
        // Once any runner is connected, mode is unambiguously SaaS — patch
        // up front so the indicator flips even if `mode` was still `unknown`
        // (e.g. a runner connected mid-bootstrap before fetchRunners landed).
        mode: s.mode === "unknown" ? "saas" : s.mode,
        selectedRunnerId: autoSelected,
        upgradeRequestedAt: nextUpgradeRequestedAt,
      };
    });
  },

  applyDisconnected: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      // Always clear the "shutdown requested" stamp on disconnect — the
      // runner has cooperated (or has been revoked, in which case we
      // already cleared it in revokeRunner). Either way, the 30s ignored-
      // shutdown affordance should not fire after a confirmed exit.
      const remainingShutdowns = { ...s.shutdownRequestedAt };
      delete remainingShutdowns[payload.runner_id];
      if (idx < 0) {
        return Object.keys(remainingShutdowns).length === Object.keys(s.shutdownRequestedAt).length
          ? {}
          : { shutdownRequestedAt: remainingShutdowns };
      }
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        status: "offline",
        lastSeenAt: nowIso,
      };
      return { runners: next, shutdownRequestedAt: remainingShutdowns };
    });
  },

  applyDriversTouch: (payload) => {
    set((s) => {
      const nowIso = new Date().toISOString();
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      const nextDriversByRunnerId = payload.drivers
        ? { ...s.driversByRunnerId, [payload.runner_id]: payload.drivers }
        : s.driversByRunnerId;
      if (idx < 0) {
        // Driver report can land before the runner row exists — still
        // cache the inventory so the RunnersPage chip is ready when the
        // row arrives via fetchRunners().
        return payload.drivers ? { driversByRunnerId: nextDriversByRunnerId } : {};
      }
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        lastSeenAt: nowIso,
        drivers: payload.drivers ?? next[idx].drivers,
      };
      return { runners: next, driversByRunnerId: nextDriversByRunnerId };
    });
  },

  applyHealth: (payload) => {
    set((s) => {
      const idx = s.runners.findIndex((r) => r.id === payload.runner_id);
      const verdict: VersionMismatch =
        payload.version_mismatch === "patch" ||
        payload.version_mismatch === "minor" ||
        payload.version_mismatch === "major"
          ? payload.version_mismatch
          : "ok";
      // Forward-compat: coerce unknown severity values to green so a
      // future server build that adds another bucket doesn't paint red
      // by accident on the rollback half of an asymmetric deploy.
      const severity: VersionSeverity =
        payload.version_severity === "amber" || payload.version_severity === "red"
          ? payload.version_severity
          : "green";
      const now = Date.now();
      const nowIso = new Date(now).toISOString();
      // Always extend the per-runner sparkline ring, even if the runner
      // row is unknown — fetchRunners will reconcile and the history
      // is keyed by id.
      const previous = s.healthHistoryByRunnerId[payload.runner_id] ?? [];
      const appended = [
        ...previous,
        {
          ts: now,
          p50: payload.ci_poll_ms_p50,
          p99: payload.ci_poll_ms_p99,
        },
      ];
      const trimmed =
        appended.length > RUNNER_HEALTH_HISTORY_CAP
          ? appended.slice(appended.length - RUNNER_HEALTH_HISTORY_CAP)
          : appended;
      const nextHistory = {
        ...s.healthHistoryByRunnerId,
        [payload.runner_id]: trimmed,
      };
      if (idx < 0) {
        // Health frame for an unknown runner — keep the history ring so
        // the panel has data when the row appears.
        return { healthHistoryByRunnerId: nextHistory };
      }
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        versionMismatch: verdict,
        versionSeverity: severity,
        versionMismatchOverride:
          typeof payload.version_mismatch_override === "boolean"
            ? payload.version_mismatch_override
            : next[idx].versionMismatchOverride,
        health: {
          outboxDepth: payload.outbox_depth,
          wsReconnects24h: payload.ws_reconnects_24h,
          ciPollMsP50: payload.ci_poll_ms_p50,
          ciPollMsP99: payload.ci_poll_ms_p99,
          lastHealthAt: nowIso,
          orphansReaped24h: payload.orphans_reaped_24h ?? null,
        },
      };
      return { runners: next, healthHistoryByRunnerId: nextHistory };
    });
  },

  setVersionMismatchOverride: async (runnerId, override) => {
    const resp = await postJson<{ runnerId: string; override: boolean }>(
      `/api/runners/${encodeURIComponent(runnerId)}/version-mismatch-override`,
      { override },
    );
    // Optimistically patch the row so the chip flips immediately; the
    // next runner_health tick will reconcile if the user changes their
    // mind in another tab.
    set((s) => {
      const idx = s.runners.findIndex((r) => r.id === runnerId);
      if (idx < 0) return {};
      const next = s.runners.slice();
      next[idx] = {
        ...next[idx],
        versionMismatchOverride: resp.override,
      };
      return { runners: next };
    });
    return resp;
  },

  pushRunnerLog: (runnerId, entry) => {
    const nextId = (runnerLogIdSeq[runnerId] ?? 0) + 1;
    runnerLogIdSeq[runnerId] = nextId;
    set((s) => {
      const current = s.logsByRunnerId[runnerId] ?? [];
      // Append the new line, then trim from the front so the cap is
      // never exceeded (drops oldest, FIFO). For the common path
      // (current.length < cap) this is a single allocation.
      const appended = [
        ...current,
        { id: nextId, ts: entry.ts, level: entry.level, line: entry.line },
      ];
      const trimmed =
        appended.length > RUNNER_LOG_RING_CAP
          ? appended.slice(appended.length - RUNNER_LOG_RING_CAP)
          : appended;
      return { logsByRunnerId: { ...s.logsByRunnerId, [runnerId]: trimmed } };
    });
  },

  fetchRunnerConfig: async (runnerId) => {
    const cfg = await fetchJson<RunnerConfig>(
      `/api/runners/${encodeURIComponent(runnerId)}/config`,
    );
    set((s) => ({
      configByRunnerId: { ...s.configByRunnerId, [runnerId]: cfg },
    }));
    return cfg;
  },

  saveRunnerConfig: async (runnerId, patch) => {
    // Forward exactly what the caller passed: `null` clears the override,
    // `undefined` omits the key (server treats missing as "no change").
    // Build the body manually so JSON.stringify doesn't drop `null`.
    const body: Record<string, unknown> = {};
    if (Object.prototype.hasOwnProperty.call(patch, "effort")) {
      body.effort = patch.effort ?? null;
    }
    if (Object.prototype.hasOwnProperty.call(patch, "skipPermissions")) {
      body.skip_permissions = patch.skipPermissions ?? null;
    }
    const cfg = await putJson<RunnerConfig>(
      `/api/runners/${encodeURIComponent(runnerId)}/config`,
      body,
    );
    set((s) => ({
      configByRunnerId: { ...s.configByRunnerId, [runnerId]: cfg },
    }));
    return cfg;
  },

  requestRunnerShutdown: async (runnerId, body) => {
    const resp = await postJson<RunnerShutdownResponse>(
      `/api/runners/${encodeURIComponent(runnerId)}/shutdown`,
      {
        reason: body.reason,
        confirmInFlight: body.confirmInFlight,
      },
    );
    // Stamp the request time so the row label can flip to
    // "Requested at <ts>" and the 30s fallback can fire. We deliberately
    // do not flip the runner's `status` here — only the WS
    // `runner_disconnected` broadcast or a follow-up
    // `applyDisconnected({reason:"revoked"})` should mark the runner
    // offline, so a runner that ignores the shutdown still surfaces as
    // online until the operator escalates to revoke.
    set((s) => ({
      shutdownRequestedAt: { ...s.shutdownRequestedAt, [runnerId]: Date.now() },
    }));
    return resp;
  },

  upgradeRunner: async (runnerId, body) => {
    const resp = await postJson<RunnerUpgradeResponse>(
      `/api/runners/${encodeURIComponent(runnerId)}/upgrade`,
      // Always send a JSON body so the server's optional-body extractor
      // sees a parseable shape. `reason` is omitted when undefined.
      body?.reason ? { reason: body.reason } : {},
    );
    set((s) => ({
      upgradeRequestedAt: { ...s.upgradeRequestedAt, [runnerId]: Date.now() },
    }));
    return resp;
  },

  revokeRunner: async (runnerId) => {
    const resp = await deleteJson<RunnerRevokeResponse>(
      `/api/runners/${encodeURIComponent(runnerId)}`,
    );
    // Optimistically drop the runner from the list — the server's
    // `runner_disconnected` synthetic broadcast will reconcile, but the
    // user expects the row to disappear instantly when they click
    // Revoke.
    set((s) => {
      const remainingRunners = s.runners.filter((r) => r.id !== runnerId);
      const remainingDrivers = { ...s.driversByRunnerId };
      delete remainingDrivers[runnerId];
      const remainingShutdowns = { ...s.shutdownRequestedAt };
      delete remainingShutdowns[runnerId];
      const remainingUpgrades = { ...s.upgradeRequestedAt };
      delete remainingUpgrades[runnerId];
      const remainingConfigs = { ...s.configByRunnerId };
      delete remainingConfigs[runnerId];
      const remainingLogs = { ...s.logsByRunnerId };
      delete remainingLogs[runnerId];
      // If the user revoked the currently-selected runner, fall back to
      // the next-most-recent row (or null if none remain).
      const nextSelected =
        s.selectedRunnerId === runnerId ? (remainingRunners[0]?.id ?? null) : s.selectedRunnerId;
      if (nextSelected !== s.selectedRunnerId) {
        writePersistedSelectedRunnerId(nextSelected);
      }
      return {
        runners: remainingRunners,
        driversByRunnerId: remainingDrivers,
        shutdownRequestedAt: remainingShutdowns,
        upgradeRequestedAt: remainingUpgrades,
        configByRunnerId: remainingConfigs,
        logsByRunnerId: remainingLogs,
        selectedRunnerId: nextSelected,
      };
    });
    return resp;
  },

  reset: () => {
    inFlightRunnersFetch = null;
    writePersistedSelectedRunnerId(null);
    // Clear the per-runner id counter so a fresh login starts at id=1.
    for (const key of Object.keys(runnerLogIdSeq)) delete runnerLogIdSeq[key];
    set({ ...INITIAL_STATE });
  },
}));

// ── Health chip helpers ─────────────────────────────────────────────────────

/// Threshold past which the outbox-depth chip turns red. Mirrors the
/// runner-side saturation point cited in the wire variant doc.
export const OUTBOX_DEPTH_RED_THRESHOLD = 100;

/// Threshold past which the 24 h reconnect chip turns red. Same value the
/// runner-side `RunnerHealth` doc-comment names.
export const WS_RECONNECTS_RED_THRESHOLD = 5;

/// Threshold past which the orphans-reaped chip escalates from `warn` to
/// `danger`. One reap is worth surfacing (silent rot happened) but is not
/// alarming on its own; a steady stream above this threshold means the
/// daemon-host-supervisor pairing is unhealthy and deserves a red chip.
export const ORPHANS_REAPED_RED_THRESHOLD = 5;

/// Severity of the worst metric on a runner row, used to color the
/// summary-list chip. `none` ⇒ no chip is rendered (the row is healthy).
export type HealthChipSeverity = "none" | "warn" | "danger";

export interface HealthChip {
  severity: HealthChipSeverity;
  /// Short label like `outbox 142` / `version 0.2.x` / `6 reconnects`.
  /// Empty string when severity is `none`.
  label: string;
}

/// Pick the worst-severity metric on a runner row and produce a chip
/// label. Order of precedence (worst first): major version mismatch,
/// outbox saturation, minor version mismatch, ws reconnect storm. The
/// patch-level mismatch is not surfaced as a chip — it shows up as a
/// neutral note in the Health panel only.
export function worstHealthChip(runner: Runner): HealthChip {
  // Major version mismatch: protocol incompatibility likely; this is the
  // single most actionable signal so it wins outright.
  if (runner.versionMismatch === "major") {
    const v = runner.version ?? "?";
    return { severity: "danger", label: `version ${v}` };
  }
  const depth = runner.health?.outboxDepth ?? null;
  if (depth !== null && depth > OUTBOX_DEPTH_RED_THRESHOLD) {
    return { severity: "danger", label: `outbox ${depth}` };
  }
  const reconnects = runner.health?.wsReconnects24h ?? null;
  if (reconnects !== null && reconnects > WS_RECONNECTS_RED_THRESHOLD) {
    return { severity: "danger", label: `${reconnects} reconnects` };
  }
  if (runner.versionMismatch === "minor") {
    const v = runner.version ?? "?";
    return { severity: "warn", label: `version ${v}` };
  }
  // Orphan reap surfaces below the version-warn so a stable runner with
  // a recent reap still flags `version` first if both are non-zero.
  // `null` ⇒ unknown (older runner) ⇒ no chip.
  const orphans = runner.health?.orphansReaped24h ?? null;
  if (orphans !== null && orphans > 0) {
    const severity: HealthChipSeverity = orphans > ORPHANS_REAPED_RED_THRESHOLD ? "danger" : "warn";
    return {
      severity,
      label: `${orphans} orphan${orphans === 1 ? "" : "s"} reaped`,
    };
  }
  return { severity: "none", label: "" };
}
