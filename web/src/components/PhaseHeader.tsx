import { useEffect, useRef, useState } from "react";
import {
  getPhaseSettings,
  putPhaseSettings,
  resolvePhaseVerification,
  type PhaseSettings,
  type PhaseVerificationSource,
} from "../api/plans.js";
import { Button } from "./ui/Button.js";
import { toastError } from "../lib/toast.js";
import { HttpError } from "../api.js";

interface Props {
  planName: string;
  phaseNumber: number;
  /// Force-collapse the accordion when its parent phase row is itself
  /// collapsed. The accordion still owns its own open/closed state when
  /// the phase row is expanded — these are two independent controls.
  forceCollapsed?: boolean;
}

const SOURCE_LABEL: Record<PhaseVerificationSource, string> = {
  phase: "phase override",
  plan: "plan default",
  repo: "repo default",
  none: "no verification",
};

const SOURCE_CLS: Record<PhaseVerificationSource, string> = {
  phase: "bg-indigo-900/40 text-indigo-300 border border-indigo-700/50",
  plan: "bg-amber-900/30 text-amber-300 border border-amber-700/50",
  repo: "bg-amber-900/30 text-amber-300 border border-amber-700/50",
  none: "bg-gray-800 text-gray-500 border border-gray-700",
};

/// Phase-scoped override accordion (Task 3.3). Renders a small
/// "Override" button on the phase row that, when clicked, expands a
/// panel to override the phase verification command. Kept narrowly
/// focused — CI workflow filter overrides at phase scope are
/// intentionally deferred (see Task 3.3 brief).
///
/// The accordion lazy-loads phase settings on first open: GET
/// `/api/plans/:name/phases/:number/settings` returns the inheritance
/// chain (phase → plan → repo). PUT to the same path edits the phase
/// block in the plan YAML so the override appears in `git diff`.
export function PhaseHeader({ planName, phaseNumber, forceCollapsed }: Props) {
  const [open, setOpen] = useState(false);
  const [settings, setSettings] = useState<PhaseSettings | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  // Single-fetch latch: keeping `loading`/`settings` in the effect deps
  // would re-fire the effect on its own state writes and cancel the
  // in-flight promise via the cleanup function. A ref decouples the
  // "have we fetched once?" question from React's render cycle.
  const fetchStartedRef = useRef(false);

  useEffect(() => {
    if (!open || fetchStartedRef.current) return;
    fetchStartedRef.current = true;
    let cancelled = false;
    setLoading(true);
    setError(null);
    getPhaseSettings(planName, phaseNumber)
      .then((s) => {
        if (cancelled) return;
        setSettings(s);
        setDraft(s.phaseVerification ?? "");
        setLoading(false);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setLoading(false);
        if (e instanceof HttpError && typeof e.body === "object" && e.body) {
          const body = e.body as { message?: string; error?: string };
          setError(body.message ?? body.error ?? `Failed to load (${e.status})`);
        } else {
          setError("Failed to load phase settings.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [open, planName, phaseNumber]);

  // Collapse when parent phase row collapses, but preserve any draft
  // edits the user has in flight so reopening the parent doesn't wipe
  // their changes.
  const effectiveOpen = open && !forceCollapsed;

  async function applyAndRefresh(value: string | null) {
    setSaving(true);
    try {
      const updated = await putPhaseSettings(planName, phaseNumber, {
        phaseVerification: value,
      });
      setSettings(updated);
      setDraft(updated.phaseVerification ?? "");
    } catch (e) {
      toastError(e, "Failed to save phase override");
    } finally {
      setSaving(false);
    }
  }

  async function saveOverride() {
    const trimmed = draft.trim();
    // Empty draft clears (Inherit). Server treats explicit `null` as
    // "remove the YAML key" — sending an empty string would write
    // `phase_verification: ""` which is a different broken state.
    await applyAndRefresh(trimmed === "" ? null : trimmed);
  }

  async function inheritOverride() {
    await applyAndRefresh(null);
  }

  /// Promote the inherited value to a phase-scoped override so the
  /// user can edit it freely. This stages the value in the textarea
  /// without persisting until the user clicks Save — keeps the cost
  /// of "I clicked Override by mistake" to zero.
  function startOverride() {
    if (!settings) return;
    const inherited = resolvePhaseVerification(settings);
    setDraft(inherited.value ?? "");
  }

  // Whether the phase has its own override OR the user has staged an
  // edit that diverges from the persisted value. Drives the Save
  // button visibility.
  const dirty = (() => {
    if (!settings) return false;
    return draft !== (settings.phaseVerification ?? "");
  })();

  const overriding = settings?.phaseVerification !== null;
  const inherited = settings ? resolvePhaseVerification(settings) : null;
  const multiline = draft.includes("\n") || (inherited?.value ?? "").includes("\n");

  return (
    <div className="border-t border-gray-800/60">
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        className="w-full flex items-center gap-2 px-3 py-1.5 text-[10px] text-gray-500 hover:text-gray-300 hover:bg-gray-800/30 transition"
        aria-expanded={effectiveOpen}
        aria-controls={`phase-${phaseNumber}-override-panel`}
      >
        <span
          className={`text-[8px] transition-transform ${effectiveOpen ? "rotate-90" : ""}`}
          aria-hidden="true"
        >
          &#9654;
        </span>
        <span className="uppercase tracking-wide">Phase override</span>
        {settings && (
          <span
            className={`ml-auto text-[9px] uppercase tracking-wide px-1.5 py-0.5 rounded ${SOURCE_CLS[inherited!.source]}`}
            title={`Verification command source: ${SOURCE_LABEL[inherited!.source]}`}
          >
            {SOURCE_LABEL[inherited!.source]}
          </span>
        )}
      </button>

      {effectiveOpen && (
        <div id={`phase-${phaseNumber}-override-panel`} className="px-3 pb-3 pt-1 space-y-2">
          {loading && (
            <p className="text-xs text-gray-500" role="status">
              Loading…
            </p>
          )}
          {error && (
            <p
              className="text-xs text-red-400 border border-red-800/40 bg-red-950/20 rounded px-2 py-1"
              role="alert"
            >
              {error}
            </p>
          )}
          {settings && !loading && (
            <>
              <div className="flex items-center justify-between gap-3">
                <div className="text-xs text-gray-500">
                  <span className="font-medium text-gray-400">Phase verification command</span>
                  <span className="ml-2">
                    {overriding ? (
                      <>Overrides plan default for this phase only.</>
                    ) : inherited?.source === "plan" ? (
                      <>
                        Inheriting from plan:{" "}
                        <code className="text-gray-400 break-all">{inherited.value}</code>
                      </>
                    ) : inherited?.source === "repo" ? (
                      <>
                        Inheriting from <code className="text-gray-400">branchwork.toml</code>:{" "}
                        <code className="text-gray-400 break-all">{inherited.value}</code>
                      </>
                    ) : (
                      <>No phase verification configured at any level.</>
                    )}
                  </span>
                </div>
                {overriding ? (
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={inheritOverride}
                    disabled={saving}
                    title="Remove the phase-level override and inherit from plan / repo."
                  >
                    Inherit
                  </Button>
                ) : (
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={startOverride}
                    disabled={saving}
                    title="Stage the inherited value so you can edit it. Click Save to persist."
                  >
                    Override
                  </Button>
                )}
              </div>
              {(overriding || dirty) && (
                <div>
                  {multiline ? (
                    <textarea
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      placeholder={inherited?.value ?? "e.g. cargo test --release"}
                      rows={Math.max(2, draft.split("\n").length)}
                      className="w-full font-mono text-xs bg-gray-900 border border-gray-800 rounded px-2 py-1.5 text-gray-200 placeholder-gray-600 focus:outline-none focus:border-indigo-600"
                      aria-label={`Phase ${phaseNumber} verification command`}
                      disabled={saving}
                    />
                  ) : (
                    <input
                      type="text"
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      placeholder={inherited?.value ?? "e.g. cargo test --release"}
                      className="w-full font-mono text-xs bg-gray-900 border border-gray-800 rounded px-2 py-1.5 text-gray-200 placeholder-gray-600 focus:outline-none focus:border-indigo-600"
                      aria-label={`Phase ${phaseNumber} verification command`}
                      disabled={saving}
                    />
                  )}
                  {dirty && (
                    <div className="flex justify-end mt-1.5">
                      <Button
                        variant="primary"
                        size="sm"
                        onClick={saveOverride}
                        disabled={saving}
                        loading={saving}
                      >
                        Save
                      </Button>
                    </div>
                  )}
                </div>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}
