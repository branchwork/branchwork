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

interface SettingsStore {
  effort: EffortLevel;
  skipPermissions: boolean;
  webhookUrl: string | null;
  loaded: boolean;
  drivers: DriverInfo[];
  defaultDriver: string;
  fetchSettings: () => Promise<void>;
  fetchDrivers: () => Promise<void>;
  setEffort: (level: EffortLevel) => Promise<void>;
  setSkipPermissions: (value: boolean) => Promise<void>;
  setWebhookUrl: (value: string | null) => Promise<void>;
  driverCapabilities: (name: string | null | undefined) => DriverCapabilities;
  driverAuth: (name: string | null | undefined) => AuthStatus | undefined;
}

export const useSettingsStore = create<SettingsStore>((set, get) => ({
  effort: "high",
  skipPermissions: true,
  webhookUrl: null,
  loaded: false,
  drivers: [],
  defaultDriver: "claude",

  fetchSettings: async () => {
    const data = await fetchJson<{
      effort: EffortLevel;
      skip_permissions: boolean;
      webhook_url: string | null;
    }>("/api/settings");
    set({
      effort: data.effort,
      skipPermissions: data.skip_permissions,
      webhookUrl: data.webhook_url ?? null,
      loaded: true,
    });
  },

  fetchDrivers: async () => {
    const data = await fetchJson<{ drivers: DriverInfo[]; default: string }>(
      "/api/drivers"
    );
    set({ drivers: data.drivers, defaultDriver: data.default });
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
}));
