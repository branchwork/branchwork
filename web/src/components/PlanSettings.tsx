import { useEffect, useState } from "react";
import {
  getPlanSettings,
  putPlanSettings,
  resolveWorkflows,
  type PlanSettings as PlanSettingsT,
  type ResolvedWorkflow,
  type WorkflowSource,
} from "../api/plans.js";
import { Button } from "./ui/Button.js";
import { toastError } from "../lib/toast.js";
import { HttpError } from "../api.js";

interface Props {
  planName: string;
}

const SOURCE_LABEL: Record<WorkflowSource, string> = {
  plan: "plan",
  "repo default": "repo default",
  "smart default": "smart default",
};

const SOURCE_CLS: Record<WorkflowSource, string> = {
  plan: "bg-indigo-900/40 text-indigo-300 border border-indigo-700/50",
  "repo default": "bg-amber-900/30 text-amber-300 border border-amber-700/50",
  "smart default": "bg-gray-800 text-gray-400 border border-gray-700",
};

/// Plan-scoped settings panel rendered as the alternate tab on PlanBoard
/// (Task 3.2). Surfaces three things:
///
/// 1. **Blocking CI workflows**: every workflow discovered under
///    `<project>/.github/workflows/`, with a checkbox indicating whether
///    it currently blocks. The "source" badge tells the user *why* it is
///    blocking (or not): an explicit plan-level override, the repo
///    default from `branchwork.toml`, or the smart classifier (the regex
///    in `ci/aggregate.rs::is_workflow_blocking_by_default`).
/// 2. **Phase verification command**: the plan-level override, with the
///    repo default surfaced as the placeholder when unset.
/// 3. **Repo defaults**: read-only view of the same repo defaults so
///    the user does not have to open `branchwork.toml` to understand
///    where inheritance comes from.
///
/// The two editable fields support an explicit "override / inherit"
/// affordance: when nothing is set at the plan level the inherited
/// value is shown greyed out and a single "Override" button promotes it
/// into a plan-level write. Once promoted, the user can edit freely and
/// the row gains an "Inherit" button to clear the override.
export function PlanSettings({ planName }: Props) {
  const [settings, setSettings] = useState<PlanSettingsT | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  /// In-flight save tracker for the verify-command field. Workflow
  /// toggles use their own per-name tracker because a single PUT can be
  /// in flight while the user clicks another row.
  const [savingVerify, setSavingVerify] = useState(false);
  const [savingWorkflows, setSavingWorkflows] = useState(false);
  /// Local state for the verify-command text input. Server is the source
  /// of truth; we mirror it here so the user can edit without every
  /// keystroke firing a PUT.
  const [verifyDraft, setVerifyDraft] = useState<string>("");
  /// True when the verify draft diverges from the server value and needs
  /// a Save click. Tracked separately from `savingVerify` so the button
  /// can disable cleanly.
  const verifyDirty = (() => {
    if (!settings) return false;
    const persisted = settings.phaseVerification ?? "";
    return verifyDraft !== persisted;
  })();

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    getPlanSettings(planName)
      .then((s) => {
        if (cancelled) return;
        setSettings(s);
        setVerifyDraft(s.phaseVerification ?? "");
        setLoading(false);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setLoading(false);
        if (e instanceof HttpError && typeof e.body === "object" && e.body) {
          const body = e.body as { message?: string; error?: string };
          setError(body.message ?? body.error ?? `Failed to load settings (${e.status})`);
        } else {
          setError("Failed to load plan settings.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [planName]);

  async function applyAndRefresh(
    body: Parameters<typeof putPlanSettings>[1],
    onDone?: (s: PlanSettingsT) => void,
  ): Promise<void> {
    const updated = await putPlanSettings(planName, body);
    setSettings(updated);
    onDone?.(updated);
  }

  async function toggleWorkflow(name: string, blocking: boolean): Promise<void> {
    if (!settings) return;
    const current = settings.ciBlockingWorkflows;
    // If the plan has no override yet, promote the *current resolved* set
    // to a plan-level list so a single toggle is unambiguous. The
    // resolved view already encodes the repo / classifier fallback.
    const baseline =
      current ??
      resolveWorkflows(settings)
        .filter((w) => w.blocking)
        .map((w) => w.name);
    const next = blocking
      ? Array.from(new Set([...baseline, name]))
      : baseline.filter((n) => n !== name);
    setSavingWorkflows(true);
    try {
      await applyAndRefresh({ ciBlockingWorkflows: next });
    } catch (e) {
      toastError(e, "Failed to save blocking workflows");
    } finally {
      setSavingWorkflows(false);
    }
  }

  async function inheritWorkflows(): Promise<void> {
    setSavingWorkflows(true);
    try {
      await applyAndRefresh({ ciBlockingWorkflows: null });
    } catch (e) {
      toastError(e, "Failed to clear plan-level override");
    } finally {
      setSavingWorkflows(false);
    }
  }

  async function overrideWorkflows(): Promise<void> {
    if (!settings) return;
    const baseline = resolveWorkflows(settings)
      .filter((w) => w.blocking)
      .map((w) => w.name);
    setSavingWorkflows(true);
    try {
      await applyAndRefresh({ ciBlockingWorkflows: baseline });
    } catch (e) {
      toastError(e, "Failed to create plan-level override");
    } finally {
      setSavingWorkflows(false);
    }
  }

  async function saveVerify(): Promise<void> {
    if (!settings) return;
    const draft = verifyDraft.trim();
    setSavingVerify(true);
    try {
      // Empty string clears the override (Inherit). Server treats an
      // explicit `null` as "remove the YAML key"; sending an empty
      // string would write `phase_verification: ""` which is a different
      // (broken) state.
      const value = draft === "" ? null : draft;
      await applyAndRefresh({ phaseVerification: value }, (s) => {
        setVerifyDraft(s.phaseVerification ?? "");
      });
    } catch (e) {
      toastError(e, "Failed to save phase verification");
    } finally {
      setSavingVerify(false);
    }
  }

  async function inheritVerify(): Promise<void> {
    setSavingVerify(true);
    try {
      await applyAndRefresh({ phaseVerification: null }, (s) => {
        setVerifyDraft(s.phaseVerification ?? "");
      });
    } catch (e) {
      toastError(e, "Failed to clear phase verification override");
    } finally {
      setSavingVerify(false);
    }
  }

  if (loading) {
    return (
      <div className="p-6 text-sm text-gray-500" role="status" aria-label="Loading plan settings">
        Loading settings...
      </div>
    );
  }

  if (error || !settings) {
    return (
      <div className="p-6">
        <div className="border border-red-800/40 bg-red-950/20 text-red-300 text-sm rounded p-3">
          {error ?? "No settings available."}
        </div>
      </div>
    );
  }

  const resolved = resolveWorkflows(settings);
  const planOverrideActive = settings.ciBlockingWorkflows !== null;
  const repoVerify = settings.repoDefaults.phaseVerification;
  const verifyInheriting = settings.phaseVerification === null;
  const verifyMultiline = verifyDraft.includes("\n") || (repoVerify ?? "").includes("\n");

  return (
    <div className="p-6 space-y-8 max-w-3xl">
      {/* Blocking CI workflows */}
      <section>
        <SectionHeader
          title="Blocking CI workflows"
          description="Workflows checked here block merge when failing. Unchecked workflows are informational only."
          right={
            planOverrideActive ? (
              <Button
                variant="secondary"
                size="sm"
                onClick={inheritWorkflows}
                disabled={savingWorkflows}
                title="Remove the plan-level override and inherit from the repo / smart default."
              >
                Inherit
              </Button>
            ) : (
              <Button
                variant="secondary"
                size="sm"
                onClick={overrideWorkflows}
                disabled={savingWorkflows || resolved.length === 0}
                title="Promote the currently inherited list to a plan-level override so you can edit it."
              >
                Override
              </Button>
            )
          }
        />
        {resolved.length === 0 ? (
          <p className="text-sm text-gray-500 italic mt-3">
            No workflows discovered under <code className="text-gray-400">.github/workflows/</code>.
          </p>
        ) : (
          <ul className="mt-3 divide-y divide-gray-800 border border-gray-800 rounded">
            {resolved.map((w) => (
              <WorkflowRow
                key={w.name}
                workflow={w}
                interactive={planOverrideActive}
                disabled={savingWorkflows}
                onToggle={(blocking) => toggleWorkflow(w.name, blocking)}
              />
            ))}
          </ul>
        )}
      </section>

      {/* Phase verification command */}
      <section>
        <SectionHeader
          title="Phase verification command"
          description="Command run by the Check agent at each phase boundary. Empty = no verification."
          right={
            !verifyInheriting ? (
              <Button
                variant="secondary"
                size="sm"
                onClick={inheritVerify}
                disabled={savingVerify}
                title="Remove the plan-level override and inherit from the repo default."
              >
                Inherit
              </Button>
            ) : repoVerify !== undefined ? (
              <Button
                variant="secondary"
                size="sm"
                onClick={() => {
                  setVerifyDraft(repoVerify);
                }}
                disabled={savingVerify}
                title="Promote the inherited value to a plan-level override so you can edit it."
              >
                Override
              </Button>
            ) : null
          }
        />
        <div className="mt-3">
          {verifyMultiline ? (
            <textarea
              value={verifyDraft}
              onChange={(e) => setVerifyDraft(e.target.value)}
              placeholder={repoVerify ?? "e.g. cargo test --release"}
              rows={Math.max(3, verifyDraft.split("\n").length)}
              className="w-full font-mono text-sm bg-gray-900 border border-gray-800 rounded px-3 py-2 text-gray-200 placeholder-gray-600 focus:outline-none focus:border-indigo-600"
              aria-label="Phase verification command"
              disabled={savingVerify}
            />
          ) : (
            <input
              type="text"
              value={verifyDraft}
              onChange={(e) => setVerifyDraft(e.target.value)}
              placeholder={repoVerify ?? "e.g. cargo test --release"}
              className="w-full font-mono text-sm bg-gray-900 border border-gray-800 rounded px-3 py-2 text-gray-200 placeholder-gray-600 focus:outline-none focus:border-indigo-600"
              aria-label="Phase verification command"
              disabled={savingVerify}
            />
          )}
          <div className="flex items-center justify-between mt-2">
            <p className="text-xs text-gray-500">
              {verifyInheriting && repoVerify !== undefined ? (
                <>
                  Inheriting from <code className="text-gray-400">branchwork.toml</code>:{" "}
                  <span className="text-gray-400">{repoVerify}</span>
                </>
              ) : verifyInheriting ? (
                <>No plan-level override and no repo default — verification is disabled.</>
              ) : (
                <>Plan-level override is active.</>
              )}
            </p>
            {verifyDirty && (
              <Button
                variant="primary"
                size="sm"
                onClick={saveVerify}
                disabled={savingVerify}
                loading={savingVerify}
              >
                Save
              </Button>
            )}
          </div>
        </div>
      </section>

      {/* Repo defaults (read-only) */}
      <section>
        <SectionHeader
          title="Repo defaults"
          description="Read-only view of the project's branchwork.toml. Plan-level fields override these."
        />
        <RepoDefaultsTable settings={settings} />
      </section>
    </div>
  );
}

interface SectionHeaderProps {
  title: string;
  description: string;
  right?: React.ReactNode;
}

function SectionHeader({ title, description, right }: SectionHeaderProps) {
  return (
    <div className="flex items-start justify-between gap-3">
      <div>
        <h3 className="text-sm font-semibold text-gray-200">{title}</h3>
        <p className="text-xs text-gray-500 mt-0.5">{description}</p>
      </div>
      {right && <div className="flex-shrink-0">{right}</div>}
    </div>
  );
}

interface WorkflowRowProps {
  workflow: ResolvedWorkflow;
  /// True when the plan has its own `ci_blocking_workflows` list and the
  /// checkbox is editable. False when inheriting — checkbox is rendered
  /// greyed out as a read-only preview of the inherited state.
  interactive: boolean;
  disabled: boolean;
  onToggle: (blocking: boolean) => void;
}

function WorkflowRow({ workflow, interactive, disabled, onToggle }: WorkflowRowProps) {
  const id = `wf-${workflow.name}`;
  return (
    <li
      className={`flex items-center justify-between gap-3 px-3 py-2 ${interactive ? "" : "opacity-60"}`}
    >
      <label
        htmlFor={id}
        className={`flex items-center gap-2 flex-1 ${interactive && !disabled ? "cursor-pointer" : ""}`}
      >
        <input
          id={id}
          type="checkbox"
          checked={workflow.blocking}
          disabled={!interactive || disabled}
          onChange={(e) => onToggle(e.target.checked)}
          className="w-4 h-4 accent-indigo-500"
          aria-label={`${workflow.name} ${workflow.blocking ? "blocking" : "informational"}`}
        />
        <span className="font-mono text-sm text-gray-200">{workflow.name}</span>
      </label>
      <div className="flex items-center gap-2 flex-shrink-0">
        <span
          className={`text-[10px] uppercase tracking-wide px-1.5 py-0.5 rounded ${SOURCE_CLS[workflow.source]}`}
          title={`Source: ${workflow.source}`}
        >
          {SOURCE_LABEL[workflow.source]}
        </span>
        <span
          className={`text-xs ${workflow.blocking ? "text-emerald-400" : "text-gray-500"}`}
          aria-hidden="true"
        >
          {workflow.blocking ? "blocking" : "informational"}
        </span>
      </div>
    </li>
  );
}

function RepoDefaultsTable({ settings }: { settings: PlanSettingsT }) {
  const { repoDefaults } = settings;
  const empty =
    repoDefaults.ciBlockingWorkflows === undefined &&
    repoDefaults.ciBlockingWorkflowsSkip === undefined &&
    repoDefaults.phaseVerification === undefined;
  if (empty) {
    return (
      <p className="text-sm text-gray-500 italic mt-3">
        No <code className="text-gray-400">branchwork.toml</code> in the project (or it sets none of
        these keys).
      </p>
    );
  }
  return (
    <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2 text-sm">
      <DefaultRow label="ci.blocking_workflows" value={repoDefaults.ciBlockingWorkflows} />
      <DefaultRow label="ci.blocking_workflows_skip" value={repoDefaults.ciBlockingWorkflowsSkip} />
      <DefaultRow label="phase.verification" value={repoDefaults.phaseVerification} />
    </dl>
  );
}

function DefaultRow({ label, value }: { label: string; value: string | string[] | undefined }) {
  return (
    <>
      <dt className="font-mono text-xs text-gray-500 self-center">{label}</dt>
      <dd className="font-mono text-xs text-gray-300 break-all">
        {value === undefined ? (
          <span className="text-gray-600 italic">unset</span>
        ) : Array.isArray(value) ? (
          value.length === 0 ? (
            <span className="text-gray-500">[]</span>
          ) : (
            value.join(", ")
          )
        ) : (
          value
        )}
      </dd>
    </>
  );
}
