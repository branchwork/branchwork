import { useEffect, useState } from "react";
import { useSettingsStore, type EffortLevel } from "../stores/settings-store.js";

const EFFORT_LEVELS: { value: EffortLevel; label: string }[] = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "max", label: "Max" },
];

export function AdminPage() {
  const effort = useSettingsStore((s) => s.effort);
  const setEffort = useSettingsStore((s) => s.setEffort);
  const skipPermissions = useSettingsStore((s) => s.skipPermissions);
  const setSkipPermissions = useSettingsStore((s) => s.setSkipPermissions);
  const webhookUrl = useSettingsStore((s) => s.webhookUrl);
  const setWebhookUrl = useSettingsStore((s) => s.setWebhookUrl);

  const [webhookDraft, setWebhookDraft] = useState(webhookUrl ?? "");
  const [webhookStatus, setWebhookStatus] = useState<"idle" | "saving" | "saved" | "error">(
    "idle"
  );
  const [webhookError, setWebhookError] = useState<string | null>(null);

  useEffect(() => {
    setWebhookDraft(webhookUrl ?? "");
  }, [webhookUrl]);

  const dirty = (webhookDraft.trim() || null) !== webhookUrl;

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
          <button
            onClick={saveWebhook}
            disabled={!dirty || webhookStatus === "saving"}
            className="px-3 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:bg-gray-700 disabled:text-gray-500 text-white text-sm rounded transition"
          >
            {webhookStatus === "saving" ? "Saving…" : "Save"}
          </button>
        </div>
        {webhookStatus === "saved" && (
          <p className="text-[11px] text-emerald-400 mt-1.5">Saved.</p>
        )}
        {webhookStatus === "error" && webhookError && (
          <p className="text-[11px] text-red-400 mt-1.5">{webhookError}</p>
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
