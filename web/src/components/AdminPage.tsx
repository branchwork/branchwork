import { useEffect, useState } from "react";
import { useSettingsStore, type EffortLevel } from "../stores/settings-store.js";
import { Banner } from "./ui/Banner.js";
import { Button } from "./ui/Button.js";

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

export function AdminPage() {
  const effort = useSettingsStore((s) => s.effort);
  const setEffort = useSettingsStore((s) => s.setEffort);
  const skipPermissions = useSettingsStore((s) => s.skipPermissions);
  const setSkipPermissions = useSettingsStore((s) => s.setSkipPermissions);
  const webhookUrl = useSettingsStore((s) => s.webhookUrl);
  const setWebhookUrl = useSettingsStore((s) => s.setWebhookUrl);
  const planArchiveRetentionDays = useSettingsStore(
    (s) => s.planArchiveRetentionDays,
  );
  const setPlanArchiveRetentionDays = useSettingsStore(
    (s) => s.setPlanArchiveRetentionDays,
  );

  const [webhookDraft, setWebhookDraft] = useState(webhookUrl ?? "");
  const [webhookStatus, setWebhookStatus] = useState<"idle" | "saving" | "saved" | "error">(
    "idle"
  );
  const [webhookError, setWebhookError] = useState<string | null>(null);

  const [retentionDraft, setRetentionDraft] = useState(
    String(planArchiveRetentionDays),
  );
  const [retentionStatus, setRetentionStatus] = useState<
    "idle" | "saving" | "saved" | "error"
  >("idle");
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
  const retentionPreviewDays = clampRetentionDays(
    retentionDraft,
    planArchiveRetentionDays,
  );

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
    <div className="max-w-2xl p-8">
      <div className="mb-8">
        <h1 className="text-xl font-bold text-gray-100">Admin</h1>
        <p className="text-xs text-gray-500 mt-1">
          Server-wide defaults. Changes apply to new agents and persist across restarts.
        </p>
      </div>

      <Section
        title="Default effort"
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
      </Section>

      <Section
        title="Skip permissions"
        description={
          <>
            Spawn Claude agents with <code className="text-gray-400">--dangerously-skip-permissions</code>.
            Requires <code className="text-gray-400">"skipDangerousModePermissionPrompt": true</code> in
            <code className="text-gray-400"> ~/.claude/settings.json</code> (see README), otherwise the
            session ends on first launch.
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
        {webhookStatus === "saved" && (
          <p className="text-[11px] text-emerald-400 mt-1.5">Saved.</p>
        )}
        {webhookStatus === "error" && webhookError && (
          <Banner className="mt-1.5">{webhookError}</Banner>
        )}
      </Section>

      <Section
        title="Retention"
        description={
          <>
            Days a soft-deleted plan stays in the archive before purge. Set
            to 0 to disable soft delete — Delete becomes permanent and
            skips the archive. Range 0–365; out-of-range values clamp on
            blur.
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

interface SectionProps {
  title: string;
  description: React.ReactNode;
  children: React.ReactNode;
}

function Section({ title, description, children }: SectionProps) {
  return (
    <div className="mb-6 pb-6 border-b border-gray-800 last:border-b-0">
      <h2 className="text-sm font-semibold text-gray-200">{title}</h2>
      <p className="text-[11px] text-gray-500 mt-1 mb-3 leading-relaxed">{description}</p>
      {children}
    </div>
  );
}
