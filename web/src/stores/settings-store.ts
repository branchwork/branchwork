import { create } from "zustand";
import { fetchJson, putJson } from "../api.js";

export type EffortLevel = "low" | "medium" | "high" | "max";

export interface DriverCapabilities {
  supports_cost: boolean;
  supports_verdict: boolean;
  supports_session_id: boolean;
  interactive_only: boolean;
}

export type AuthStatus =
  | { kind: "not_installed" }
  | { kind: "unauthenticated"; help: string }
  | { kind: "oauth"; account: string | null }
  | { kind: "api_key" }
  | { kind: "cloud_provider"; provider: string }
  | { kind: "unknown" };

export interface DriverInfo {
  name: string;
  binary: string;
  capabilities: DriverCapabilities;
  auth_status?: AuthStatus;
}

/// True when the auth status is good enough to spawn agents with this driver.
/// We treat `unknown` as OK because not every driver can reliably introspect
/// its auth — blocking on false negatives would be worse than letting the
/// agent spawn attempt fail with a clearer error in the terminal.
export function isDriverReady(auth: AuthStatus | undefined): boolean {
  if (!auth) return true;
  return auth.kind !== "not_installed" && auth.kind !== "unauthenticated";
}

/// Fallback for agents or plans referencing an unknown driver — assume the
/// Claude-shaped "all features" profile so UIs degrade to showing data
/// rather than hiding it. Mirrors `DriverCapabilities::default` on the Rust
/// side.
const DEFAULT_CAPABILITIES: DriverCapabilities = {
  supports_cost: true,
  supports_verdict: true,
  supports_session_id: true,
  interactive_only: false,
};

/// Driver-inventory load status. Distinguishes "we never asked" from
/// "we asked but the runner has not reported yet" so the sidebar can
/// render the right copy for SaaS deployments where the runner is
/// missing or freshly enrolled.
///
/// - `online`:        live runner pushed an inventory recently.
/// - `offline`:       runner is registered but disconnected; we are
///                    serving its last-known DB-persisted state.
/// - `missing`:       SaaS deployment has no runner row for the org.
/// - `never_reported`:runner row exists but never sent a hello/auth
///                    report (newly enrolled, never connected, …).
/// - `local`:         standalone deployment — server's compiled
///                    DriverRegistry is authoritative.
export type DriverRunnerStatus = "online" | "offline" | "missing" | "never_reported" | "local";

interface SettingsStore {
  effort: EffortLevel;
  skipPermissions: boolean;
  webhookUrl: string | null;
  /// Days a soft-deleted plan's snapshot survives before purge. 0
  /// collapses soft delete to hard delete (drives the modal copy
  /// for `DeletePlanModal`). Server clamps to [0, 365]; the default
  /// of 30 is mirrored here so a missing /api/settings doesn't
  /// surprise the modal copy.
  planArchiveRetentionDays: number;
  loaded: boolean;
  drivers: DriverInfo[];
  defaultDriver: string;
  /// Runner the current `drivers` array describes. `null` ≡ standalone
  /// or "we couldn't resolve a runner" — used by the sidebar to render
  /// the runner label (or hide it). Tracks the value the user passed
  /// most recently to `fetchDrivers(runnerId)`.
  driversRunnerId: string | null;
  /// Server's verdict on the runner that filled `drivers`. The sidebar
  /// uses this to render an offline/never-reported chip on top of the
  /// list when the data is stale.
  driversRunnerStatus: DriverRunnerStatus;
  /// ms-since-epoch when the last `fetchSettings()` resolved. Mirrors the
  /// plan-store / agent-store debounce contract so reconnect-driven
  /// refetches (audit §4: settings + drivers were missed on reconnect)
  /// can be skipped if a fresh response just landed.
  lastSettingsFetchedAt: number | null;
  /// ms-since-epoch when the last `fetchDrivers()` resolved.
  lastDriversFetchedAt: number | null;
  fetchSettings: () => Promise<void>;
  /// Optional `runnerId` selects which runner's inventory to fetch in
  /// SaaS mode. Standalone deployments ignore the parameter and always
  /// return the server's local registry. Concurrent calls with the same
  /// runner id coalesce; calls with a different runner id replace any
  /// in-flight call so the sidebar reflects the runner the user just
  /// switched to instead of the stale runner.
  fetchDrivers: (runnerId?: string | null) => Promise<void>;
  setEffort: (level: EffortLevel) => Promise<void>;
  setSkipPermissions: (value: boolean) => Promise<void>;
  setWebhookUrl: (value: string | null) => Promise<void>;
  setPlanArchiveRetentionDays: (value: number) => Promise<void>;
  driverCapabilities: (name: string | null | undefined) => DriverCapabilities;
  driverAuth: (name: string | null | undefined) => AuthStatus | undefined;
  /// Drop every slice back to its initial shape. Driven by
  /// `lib/reset-all.ts::resetAllStores()` on logout so user A's
  /// effort/webhook/driver settings don't leak into user B's session.
  /// Also clears both in-flight fetch handles.
  reset: () => void;
}

/// Module-level handle to the single in-flight `fetchSettings()` round trip.
/// Coalesces concurrent callers (App.tsx bootstrap + ws-store reconnect)
/// onto one network request. ws-store also reads this to debounce
/// reconnect refetches.
let inFlightSettingsFetch: Promise<void> | null = null;
/// In-flight drivers fetch + the runner it was launched against. We
/// coalesce calls for the same runner but cancel-replace when the user
/// switches runners — otherwise the sidebar shows the previous runner's
/// drivers until the in-flight fetch finishes.
let inFlightDriversFetch: Promise<void> | null = null;
let inFlightDriversFetchRunnerId: string | null | undefined = undefined;

export function getInFlightSettingsFetch(): Promise<void> | null {
  return inFlightSettingsFetch;
}

export function getInFlightDriversFetch(): Promise<void> | null {
  return inFlightDriversFetch;
}

export const useSettingsStore = create<SettingsStore>((set, get) => ({
  effort: "high",
  skipPermissions: true,
  webhookUrl: null,
  planArchiveRetentionDays: 30,
  loaded: false,
  drivers: [],
  defaultDriver: "claude",
  driversRunnerId: null,
  driversRunnerStatus: "local",
  lastSettingsFetchedAt: null,
  lastDriversFetchedAt: null,

  fetchSettings: () => {
    if (inFlightSettingsFetch) return inFlightSettingsFetch;
    const promise = (async () => {
      const data = await fetchJson<{
        effort: EffortLevel;
        skip_permissions: boolean;
        webhook_url: string | null;
        plan_archive_retention_days?: number;
        default_driver?: string;
      }>("/api/settings");
      set({
        effort: data.effort,
        skipPermissions: data.skip_permissions,
        webhookUrl: data.webhook_url ?? null,
        planArchiveRetentionDays: data.plan_archive_retention_days ?? 30,
        defaultDriver: data.default_driver ?? "claude",
        loaded: true,
        lastSettingsFetchedAt: Date.now(),
      });
    })();
    inFlightSettingsFetch = promise;
    promise.finally(() => {
      if (inFlightSettingsFetch === promise) inFlightSettingsFetch = null;
    });
    return promise;
  },

  fetchDrivers: (runnerId) => {
    const targetRunnerId = runnerId ?? null;
    // Coalesce only when the in-flight fetch is for the same runner. A
    // call with a different runner id supersedes the previous fetch so
    // the sidebar reflects the runner the user just switched to.
    if (inFlightDriversFetch && inFlightDriversFetchRunnerId === targetRunnerId) {
      return inFlightDriversFetch;
    }
    const path = targetRunnerId
      ? `/api/drivers?runner_id=${encodeURIComponent(targetRunnerId)}`
      : "/api/drivers";
    const promise = (async () => {
      const data = await fetchJson<{
        drivers: DriverInfo[];
        default: string;
        runner_id?: string | null;
        runner_status?: DriverRunnerStatus;
      }>(path);
      // Stale guard: if the user switched runners while this fetch was
      // in flight, drop the result silently — the newer fetch's set()
      // will overwrite shortly.
      if (inFlightDriversFetchRunnerId !== targetRunnerId) return;
      set({
        drivers: data.drivers,
        defaultDriver: data.default,
        driversRunnerId: data.runner_id ?? targetRunnerId,
        driversRunnerStatus: data.runner_status ?? "local",
        lastDriversFetchedAt: Date.now(),
      });
    })();
    inFlightDriversFetch = promise;
    inFlightDriversFetchRunnerId = targetRunnerId;
    promise.finally(() => {
      if (inFlightDriversFetch === promise) {
        inFlightDriversFetch = null;
        inFlightDriversFetchRunnerId = undefined;
      }
    });
    return promise;
  },

  setEffort: async (level) => {
    await putJson("/api/settings", { effort: level });
    set({ effort: level });
  },

  setSkipPermissions: async (value) => {
    await putJson("/api/settings", { skip_permissions: value });
    set({ skipPermissions: value });
  },

  setWebhookUrl: async (value) => {
    await putJson("/api/settings", { webhook_url: value });
    set({ webhookUrl: value });
  },

  setPlanArchiveRetentionDays: async (value) => {
    await putJson("/api/settings", { plan_archive_retention_days: value });
    set({ planArchiveRetentionDays: value });
  },

  // Look up capabilities by driver name. Falls back to the default driver's
  // profile when the name is missing/unknown, and to DEFAULT_CAPABILITIES
  // when /api/drivers hasn't loaded yet — we'd rather flash cost UI and
  // hide it once drivers arrive than permanently hide columns on slow boot.
  driverCapabilities: (name) => {
    const { drivers, defaultDriver } = get();
    if (drivers.length === 0) return DEFAULT_CAPABILITIES;
    const match =
      (name && drivers.find((d) => d.name === name)) ||
      drivers.find((d) => d.name === defaultDriver);
    return match?.capabilities ?? DEFAULT_CAPABILITIES;
  },

  // Look up auth status by driver name. Returns undefined when /api/drivers
  // hasn't loaded yet — callers should treat that as "probably authenticated"
  // via isDriverReady() rather than block the UI.
  driverAuth: (name) => {
    const { drivers, defaultDriver } = get();
    const match =
      (name && drivers.find((d) => d.name === name)) ||
      drivers.find((d) => d.name === defaultDriver);
    return match?.auth_status;
  },

  reset: () => {
    inFlightSettingsFetch = null;
    inFlightDriversFetch = null;
    inFlightDriversFetchRunnerId = undefined;
    set({
      effort: "high",
      skipPermissions: true,
      webhookUrl: null,
      planArchiveRetentionDays: 30,
      loaded: false,
      drivers: [],
      defaultDriver: "claude",
      driversRunnerId: null,
      driversRunnerStatus: "local",
      lastSettingsFetchedAt: null,
      lastDriversFetchedAt: null,
    });
  },
}));
