import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useSettingsStore, type EffortLevel } from "../stores/settings-store.js";
import { useAuthStore } from "../stores/auth-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { putJson } from "../api.js";
import { Banner } from "./ui/Banner.js";
import { Button } from "./ui/Button.js";
import { Tabs } from "./ui/Tabs.js";
import { MembersTab } from "./admin/MembersTab.js";
import { BudgetTab } from "./admin/BudgetTab.js";
import { KillSwitchTab } from "./admin/KillSwitchTab.js";
import { UserQuotasTab } from "./admin/UserQuotasTab.js";
import { SsoTab } from "./admin/SsoTab.js";
import { DiagnosticsTab } from "./admin/DiagnosticsTab.js";
import {
  NOTIFICATION_CLASSES,
  NOTIFICATION_CLASS_ORDER,
  getNotificationPermission,
  isClassEnabled,
  requestNotificationPermission,
  setClassEnabled,
  type NotificationClass,
} from "../lib/notifications.js";

const EFFORT_LEVELS: { value: EffortLevel; label: string }[] = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "max", label: "Max" },
];

const RETENTION_MIN = 0;
const RETENTION_MAX = 365;

/// Clamp a typed retention-days string into the server's accepted range.
/// Non-numeric / empty / NaN inputs fall back to the previous saved value
/// so a half-typed entry on blur can't wipe the setting.
export function clampRetentionDays(input: string, fallback: number): number {
  const parsed = Number.parseInt(input.trim(), 10);
  if (!Number.isFinite(parsed)) return fallback;
  if (parsed < RETENTION_MIN) return RETENTION_MIN;
  if (parsed > RETENTION_MAX) return RETENTION_MAX;
  return parsed;
}

/// Live preview copy for the Retention input. Mirrors the modal copy in
/// `DeletePlanModal` so the admin sees the exact phrasing the operator
/// will see when deleting (0 = permanent path, >0 = archive-then-purge).
export function retentionPreview(days: number): string {
  if (days === 0) return "Soft-delete disabled — Delete is permanent.";
  return `Soft-deleted plans are kept for ${days} day${days === 1 ? "" : "s"}.`;
}

/// Org roles that may see + actuate the org-scoped admin tabs (Budget,
/// Kill switch, Quotas, SSO). Members and viewers see only Settings +
/// Members (read-only) on a SaaS deployment.
export function isAdminRole(role: string | null | undefined): boolean {
  return role === "owner" || role === "admin";
}

const ADMIN_TABS = [
  "settings",
  "diagnostics",
  "members",
  "budget",
  "kill-switch",
  "quotas",
  "sso",
] as const;
export type AdminTab = (typeof ADMIN_TABS)[number];

const TAB_LABELS: Record<AdminTab, string> = {
  settings: "Settings",
  diagnostics: "Diagnostics",
  members: "Members",
  budget: "Budget",
  "kill-switch": "Kill switch",
  quotas: "User quotas",
  sso: "SSO",
};

export function AdminPage() {
  const { section } = useParams<{ section?: string }>();
  const navigate = useNavigate();

  const mode = useRunnerStore((s) => s.mode);
  const memberships = useOrgStore((s) => s.memberships);
  const user = useAuthStore((s) => s.user);

  // Resolve the active membership the same way OrgChip does so the
  // header chip's slug and AdminPage's tab content always agree on
  // which org we're acting on.
  const current = useMemo(() => {
    if (memberships.length === 0) return null;
    const id = user?.orgId ?? null;
    return memberships.find((m) => m.id === id) ?? memberships[0] ?? null;
  }, [memberships, user]);
  const role = current?.role ?? null;
  const isAdmin = isAdminRole(role);

  // Standalone deployments have no org concept — only the existing
  // Settings form makes sense (audit §17, mirrors OrgChip / RunnerStatus
  // gate). Tab bar is hidden so the page looks identical to its
  // pre-tabs shape on a single-tenant box.
  const showOrgTabs = mode === "saas" && current !== null;
  const showAdminTabs = showOrgTabs && isAdmin;

  const allowedTabs: AdminTab[] = useMemo(() => {
    // Diagnostics is read-only triage and is universally useful (server
    // version, uptime, WS connections). Visible on both standalone and
    // SaaS, regardless of role.
    const t: AdminTab[] = ["settings", "diagnostics"];
    if (showOrgTabs) t.push("members");
    if (showAdminTabs) t.push("budget", "kill-switch", "quotas", "sso");
    return t;
  }, [showOrgTabs, showAdminTabs]);

  const requested = (section ?? "settings") as AdminTab;
  const activeTab: AdminTab = allowedTabs.includes(requested) ? requested : "settings";

  // Visiting `/admin/budget` as a non-admin (or on a standalone box)
  // bounces to /admin/settings so the URL stays in sync with the
  // visible tab. Server enforces the same authz; this is purely a
  // UX nicety.
  useEffect(() => {
    if (section && !allowedTabs.includes(section as AdminTab)) {
      navigate("/admin/settings", { replace: true });
    }
  }, [section, allowedTabs, navigate]);

  return (
    <div className="max-w-3xl p-8">
      <div className="mb-6">
        <h1 className="text-xl font-bold text-gray-100">Admin</h1>
        <p className="text-xs text-gray-500 mt-1">
          Server-wide defaults and per-organization controls.
        </p>
      </div>

      {allowedTabs.length > 1 && (
        <Tabs<AdminTab>
          value={activeTab}
          onChange={(v) => navigate(`/admin/${v}`)}
          label="Admin sections"
          tabs={allowedTabs.map((value) => ({
            value,
            label: TAB_LABELS[value],
          }))}
          className="mb-6 border-b border-gray-800"
        />
      )}

      {activeTab === "settings" && <SettingsTab />}
      {activeTab === "diagnostics" && <DiagnosticsTab />}
      {activeTab === "members" && current && (
        <MembersTab orgSlug={current.slug} callerRole={role} callerUserId={user?.id ?? null} />
      )}
      {activeTab === "budget" && current && <BudgetTab orgSlug={current.slug} />}
      {activeTab === "kill-switch" && current && <KillSwitchTab orgSlug={current.slug} />}
      {activeTab === "quotas" && current && <UserQuotasTab orgSlug={current.slug} />}
      {activeTab === "sso" && current && <SsoTab orgSlug={current.slug} />}
    </div>
  );
}

/// The original server-wide settings form. Lives inline so the existing
/// test surface (effort buttons, retention input, webhook field) keeps
/// rendering when AdminPage mounts on `/admin` or `/admin/settings`.
function SettingsTab() {
  const effort = useSettingsStore((s) => s.effort);
  const setEffort = useSettingsStore((s) => s.setEffort);
  const skipPermissions = useSettingsStore((s) => s.skipPermissions);
  const setSkipPermissions = useSettingsStore((s) => s.setSkipPermissions);
  const webhookUrl = useSettingsStore((s) => s.webhookUrl);
  const setWebhookUrl = useSettingsStore((s) => s.setWebhookUrl);
  const planArchiveRetentionDays = useSettingsStore((s) => s.planArchiveRetentionDays);
  const setPlanArchiveRetentionDays = useSettingsStore((s) => s.setPlanArchiveRetentionDays);

  const [webhookDraft, setWebhookDraft] = useState(webhookUrl ?? "");
  const [webhookStatus, setWebhookStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const [webhookError, setWebhookError] = useState<string | null>(null);

  const [retentionDraft, setRetentionDraft] = useState(String(planArchiveRetentionDays));
  const [retentionStatus, setRetentionStatus] = useState<"idle" | "saving" | "saved" | "error">(
    "idle",
  );
  const [retentionError, setRetentionError] = useState<string | null>(null);

  useEffect(() => {
    setWebhookDraft(webhookUrl ?? "");
  }, [webhookUrl]);

  // Reflect external store updates (initial fetch, optimistic resets) into
  // the input. The clamp on blur owns its own write-back, so this only
  // refires when the canonical value actually changes.
  useEffect(() => {
    setRetentionDraft(String(planArchiveRetentionDays));
  }, [planArchiveRetentionDays]);

  const dirty = (webhookDraft.trim() || null) !== webhookUrl;

  // Preview always reflects what the *committed* value would be — i.e. the
  // clamped draft if the user blurred right now. Keeps the chip consistent
  // with the modal copy after a save.
  const retentionPreviewDays = clampRetentionDays(retentionDraft, planArchiveRetentionDays);

  async function commitRetention() {
    const next = clampRetentionDays(retentionDraft, planArchiveRetentionDays);
    // Reflect the clamp back into the input even when no PUT fires, so
    // typing 9999 + blur visibly snaps to 365.
    if (String(next) !== retentionDraft) setRetentionDraft(String(next));
    if (next === planArchiveRetentionDays) return;
    setRetentionStatus("saving");
    setRetentionError(null);
    try {
      await setPlanArchiveRetentionDays(next);
      setRetentionStatus("saved");
      setTimeout(() => setRetentionStatus("idle"), 2000);
    } catch (e) {
      setRetentionStatus("error");
      setRetentionError(String(e));
    }
  }

  async function saveWebhook() {
    setWebhookStatus("saving");
    setWebhookError(null);
    try {
      const next = webhookDraft.trim() === "" ? null : webhookDraft.trim();
      await setWebhookUrl(next);
      setWebhookStatus("saved");
      setTimeout(() => setWebhookStatus("idle"), 2000);
    } catch (e) {
      setWebhookStatus("error");
      setWebhookError(String(e));
    }
  }

  return (
    <div className="max-w-2xl">
      <Section
        title="Default effort"
        serverWide
        description="Reasoning level passed to new agents. Higher values cost more and take longer."
      >
        <div className="flex gap-1">
          {EFFORT_LEVELS.map((l) => (
            <button
              key={l.value}
              onClick={() => setEffort(l.value)}
              className={`px-3 py-1.5 text-sm rounded transition border ${
                effort === l.value
                  ? "bg-indigo-600 border-indigo-500 text-white"
                  : "bg-gray-800 border-gray-700 text-gray-400 hover:text-gray-200 hover:border-gray-600"
              }`}
            >
              {l.label}
            </button>
          ))}
        </div>
        <PerRunnerOverrideNote />

      <Section
        title="Default agent driver"
        serverWide
        description="Default AI driver used when starting agents. Can be overridden per-task, per-phase, or per-plan."
      >
        <DefaultDriverSelector />
        <PerRunnerOverrideNote />
      </Section>

      </Section>

      <Section
        title="Skip permissions"
        serverWide
        description={
          <>
            Spawn Claude agents with{" "}
            <code className="text-gray-400">--dangerously-skip-permissions</code>. Requires{" "}
            <code className="text-gray-400">"skipDangerousModePermissionPrompt": true</code> in
            <code className="text-gray-400"> ~/.claude/settings.json</code> (see README), otherwise
            the session ends on first launch.
          </>
        }
      >
        <label className="flex items-center gap-2 cursor-pointer select-none">
          <input
            type="checkbox"
            checked={skipPermissions}
            onChange={(e) => setSkipPermissions(e.target.checked)}
            className="accent-amber-500 w-4 h-4"
          />
          <span className={`text-sm ${skipPermissions ? "text-amber-400" : "text-gray-400"}`}>
            {skipPermissions ? "On" : "Off"}
          </span>
        </label>
        <PerRunnerOverrideNote />
      </Section>

      <Section
        title="Notification webhook"
        description="POSTed when an agent completes or a phase advances. Slack incoming webhooks supported (sends `{text: ...}`); empty disables."
      >
        <div className="flex gap-2">
          <input
            type="text"
            value={webhookDraft}
            onChange={(e) => setWebhookDraft(e.target.value)}
            placeholder="https://hooks.slack.com/services/..."
            className="flex-1 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
          />
          <Button
            onClick={saveWebhook}
            disabled={!dirty || webhookStatus === "saving"}
            loading={webhookStatus === "saving"}
            variant="primary"
            size="sm"
          >
            {webhookStatus === "saving" ? "Saving" : "Save"}
          </Button>
        </div>
        {webhookStatus === "saved" && <p className="text-[11px] text-emerald-400 mt-1.5">Saved.</p>}
        {webhookStatus === "error" && webhookError && (
          <Banner className="mt-1.5">{webhookError}</Banner>
        )}
      </Section>

      <Section
        title="Browser notifications"
        description={
          <>
            Per-event-class opt-out for desktop notifications. The browser asks for permission once
            when you click Enable below — Branchwork never prompts on its own (audit §16). All
            classes ship enabled by default; uncheck to silence a class without affecting the
            others.
          </>
        }
      >
        <NotificationsSubsection />
      </Section>

      <Section
        title="Retention"
        description={
          <>
            Days a soft-deleted plan stays in the archive before purge. Set to 0 to disable soft
            delete — Delete becomes permanent and skips the archive. Range 0–365; out-of-range
            values clamp on blur.
          </>
        }
      >
        <div className="flex items-center gap-3">
          <input
            type="number"
            inputMode="numeric"
            min={RETENTION_MIN}
            max={RETENTION_MAX}
            step={1}
            value={retentionDraft}
            onChange={(e) => setRetentionDraft(e.target.value)}
            onBlur={commitRetention}
            disabled={retentionStatus === "saving"}
            aria-label="Plan archive retention days"
            data-testid="retention-input"
            className="w-24 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
          />
          <span className="text-xs text-gray-500">days</span>
          <span
            data-testid="retention-preview"
            className={`text-[11px] px-2 py-0.5 rounded border ${
              retentionPreviewDays === 0
                ? "border-amber-700/50 bg-amber-900/30 text-amber-200"
                : "border-gray-700 bg-gray-800 text-gray-300"
            }`}
          >
            {retentionPreview(retentionPreviewDays)}
          </span>
        </div>
        {retentionStatus === "saved" && (
          <p className="text-[11px] text-emerald-400 mt-1.5">Saved.</p>
        )}
        {retentionStatus === "error" && retentionError && (
          <Banner className="mt-1.5">{retentionError}</Banner>
        )}
      </Section>
    </div>
  );
}

/// Per-user notification preferences. Persisted in localStorage —
/// nothing reaches the server today (the brief allowed either, and
/// localStorage avoids a new endpoint + migration). The
/// "Enable notifications" button is the ONLY way to trigger
/// `Notification.requestPermission()`; the WS connect path no longer
/// prompts (audit §16).
function NotificationsSubsection() {
  const supported = typeof window !== "undefined" && "Notification" in window;

  // Track permission + the local checkbox state in component state so a
  // toggle re-renders this subsection without forcing the parent to
  // bookkeep prefs in zustand. localStorage is the source of truth on
  // every read; React state is the cache that drives re-render.
  const [permission, setPermission] = useState<NotificationPermission | "unsupported">(
    getNotificationPermission(),
  );
  const [enabledMap, setEnabledMap] = useState<Record<NotificationClass, boolean>>(() => {
    const map = {} as Record<NotificationClass, boolean>;
    for (const klass of NOTIFICATION_CLASS_ORDER) {
      map[klass] = isClassEnabled(klass);
    }
    return map;
  });
  const [requestStatus, setRequestStatus] = useState<"idle" | "requesting" | "denied" | "granted">(
    "idle",
  );

  async function handleEnable() {
    setRequestStatus("requesting");
    const result = await requestNotificationPermission();
    setPermission(result);
    if (result === "granted") setRequestStatus("granted");
    else if (result === "denied" || result === "unsupported") setRequestStatus("denied");
    else setRequestStatus("idle");
  }

  function handleToggle(klass: NotificationClass, next: boolean) {
    setClassEnabled(klass, next);
    setEnabledMap((prev) => ({ ...prev, [klass]: next }));
  }

  return (
    <div>
      <div className="flex items-center gap-3 mb-4" data-testid="notification-permission-row">
        <PermissionBadge permission={permission} />
        {supported && permission === "default" && (
          <Button
            onClick={handleEnable}
            loading={requestStatus === "requesting"}
            variant="primary"
            size="sm"
            data-testid="enable-notifications-button"
          >
            {requestStatus === "requesting" ? "Requesting…" : "Enable notifications"}
          </Button>
        )}
      </div>

      {permission === "denied" && (
        <Banner className="mb-3">
          The browser blocked notifications. Re-enable them from your browser's site settings, then
          refresh the page.
        </Banner>
      )}
      {permission === "unsupported" && (
        <Banner className="mb-3">
          This browser does not expose the Notification API. Toggles are stored locally and will
          take effect in a supported browser.
        </Banner>
      )}

      <ul
        className="space-y-2"
        data-testid="notification-class-list"
        aria-label="Notification event classes"
      >
        {NOTIFICATION_CLASS_ORDER.map((klass) => {
          const meta = NOTIFICATION_CLASSES[klass];
          return (
            <li key={klass} className="flex items-start gap-3">
              <input
                type="checkbox"
                id={`notify-${klass}`}
                checked={enabledMap[klass]}
                onChange={(e) => handleToggle(klass, e.target.checked)}
                className="mt-0.5 accent-indigo-500 w-4 h-4"
                data-testid={`notify-class-${klass}`}
              />
              <label htmlFor={`notify-${klass}`} className="cursor-pointer">
                <span className="text-sm text-gray-200">{meta.label}</span>
                <span className="block text-[11px] text-gray-500 leading-snug mt-0.5">
                  {meta.description}
                </span>
              </label>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function PermissionBadge({ permission }: { permission: NotificationPermission | "unsupported" }) {
  let label: string;
  let cls: string;
  switch (permission) {
    case "granted":
      label = "Notifications enabled";
      cls = "border-emerald-700/50 bg-emerald-900/30 text-emerald-200";
      break;
    case "denied":
      label = "Blocked by browser";
      cls = "border-red-700/50 bg-red-900/30 text-red-200";
      break;
    case "unsupported":
      label = "Unsupported browser";
      cls = "border-amber-700/50 bg-amber-900/30 text-amber-200";
      break;
    default:
      label = "Not enabled";
      cls = "border-gray-700 bg-gray-800 text-gray-300";
  }
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-[11px] px-2 py-0.5 rounded border ${cls}`}
      data-testid="notification-permission-badge"
    >
      {label}
    </span>
  );
}

interface SectionProps {
  title: string;
  description: React.ReactNode;
  children: React.ReactNode;
  /// Render a small "Applies server-wide" chip next to the title. Used
  /// by `effort` and `skip_permissions` after the audit §18 ownership
  /// fix: both settings persist at `<claude-dir>/branchwork-settings.json`
  /// and apply to every spawned agent regardless of which user clicked.
  /// Per-runner overrides live on the Runners page (see
  /// `PerRunnerOverrideNote`).
  serverWide?: boolean;
}

function Section({ title, description, children, serverWide }: SectionProps) {
  return (
    <div className="mb-6 pb-6 border-b border-gray-800 last:border-b-0">
      <div className="flex items-center gap-2">
        <h2 className="text-sm font-semibold text-gray-200">{title}</h2>
        {serverWide && (
          <span
            className="text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-amber-700/50 bg-amber-900/30 text-amber-300/90"
            title="This setting persists in branchwork-settings.json and applies to every user on this server."
          >
            Applies server-wide
          </span>
        )}
      </div>
      <p className="text-[11px] text-gray-500 mt-1 mb-3 leading-relaxed">{description}</p>
      {children}
    </div>
  );
}

/// Tiny note rendered under server-wide settings that runners can override
/// (`effort`, `skip_permissions`). Points the operator at the Runners page
/// where the per-runner expander lives.
function PerRunnerOverrideNote() {
  return (
    <p className="mt-2 text-[11px] text-gray-600">
      Runners can override this per-runner on the{" "}
      <a href="/runners" className="underline decoration-dotted hover:text-gray-400">
        Runners page
      </a>
      .
    </p>
  );
}

/// Dropdown to select the default agent driver. Reads from the settings
/// store's `drivers` array (populated by `fetchDrivers`) and persists
/// the selection via `PUT /api/settings`.
function DefaultDriverSelector() {
  const drivers = useSettingsStore((s) => s.drivers);
  const defaultDriver = useSettingsStore((s) => s.defaultDriver);
  const [draft, setDraft] = useState(defaultDriver);
  const [status, setStatus] = useState<"idle" | "saving" | "saved" | "error">("idle");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setDraft(defaultDriver);
  }, [defaultDriver]);

  const dirty = draft !== defaultDriver;

  async function save() {
    if (!dirty) return;
    setStatus("saving");
    setError(null);
    try {
      await putJson("/api/settings", { default_driver: draft });
      useSettingsStore.setState({ defaultDriver: draft });
      setStatus("saved");
      setTimeout(() => setStatus("idle"), 2000);
    } catch (e) {
      setStatus("error");
      setError(String(e));
    }
  }

  return (
    <div>
      <div className="flex gap-2 items-center">
        <select
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          disabled={status === "saving"}
          className="flex-1 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
          data-testid="default-driver-select"
        >
          {drivers.length === 0 ? (
            <option value="claude">claude (loading...)</option>
          ) : (
            drivers.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name}
              </option>
            ))
          )}
        </select>
        <Button
          onClick={save}
          disabled={!dirty || status === "saving"}
          loading={status === "saving"}
          variant="primary"
          size="sm"
        >
          {status === "saving" ? "Saving" : "Save"}
        </Button>
      </div>
      {status === "saved" && <p className="text-[11px] text-emerald-400 mt-1.5">Saved.</p>}
      {status === "error" && error && <Banner className="mt-1.5">{error}</Banner>}
    </div>
  );
}
