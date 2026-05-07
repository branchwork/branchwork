import { useEffect, useId, useRef, useState } from "react";
import { HttpError } from "../api.js";
import { usePlanStore, type DeletePlanPreview } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { errorMessage } from "../lib/error.js";
import { useGoToAgent } from "../hooks/use-route-selection.js";

interface DeletePlanModalProps {
  planName: string;
  /// Days a soft-deleted plan stays in the archive before purge.
  /// 0 collapses the dialog to the permanent-delete confirmation
  /// (no soft path, no Undo) — the org has opted out of retention.
  retentionDays: number;
  onClose: () => void;
}

interface PlanHasRunningAgentsBody {
  error: "plan_has_running_agents";
  agents?: string[];
}

interface AutoModeInFlightBody {
  error: "auto_mode_in_flight";
}

interface GenericErrorBody {
  error?: string;
  agents?: string[];
}

/// Human-readable, pluralized labels for the cascade preview line.
/// Keys must match the table names in `api::plans::PLAN_CASCADE_TABLES`
/// (which is also what the server returns in `wouldDelete`). Singular
/// vs plural is selected on the count by the `formatCascadeSummary`
/// helper below.
const CASCADE_LABELS: Record<string, [string, string]> = {
  task_status: ["task status", "task statuses"],
  ci_runs: ["CI run", "CI runs"],
  task_fix_attempts: ["fix attempt", "fix attempts"],
  task_learnings: ["learning", "learnings"],
  plan_auto_mode: ["auto-mode setting", "auto-mode settings"],
  plan_auto_advance: ["auto-advance setting", "auto-advance settings"],
  plan_project: ["project mapping", "project mappings"],
  plan_verdicts: ["check verdict", "check verdicts"],
  plan_budget: ["budget setting", "budget settings"],
  plan_org: ["org membership", "org memberships"],
};

/// Render the per-table cascade counts as a single English sentence.
/// Skips zero-count entries (those would just add noise) and falls
/// back to a generic message when nothing would be deleted at all.
/// The cascade-table order in `wouldDelete` is unspecified at the JSON
/// layer; we use `CASCADE_LABELS`'s declaration order so the modal copy
/// is stable across renders and never depends on hashmap iteration order.
export function formatCascadeSummary(wouldDelete: Record<string, number>): string {
  const parts: string[] = [];
  for (const key of Object.keys(CASCADE_LABELS)) {
    const n = wouldDelete[key];
    if (typeof n !== "number" || n <= 0) continue;
    const [singular, plural] = CASCADE_LABELS[key];
    parts.push(`${n} ${n === 1 ? singular : plural}`);
  }
  if (parts.length === 0) return "No cascade rows to delete.";
  if (parts.length === 1) return `${parts[0]} will be deleted.`;
  if (parts.length === 2) return `${parts[0]} and ${parts[1]} will be deleted.`;
  const last = parts.pop();
  return `${parts.join(", ")}, and ${last} will be deleted.`;
}

/// Confirmation modal for the per-plan Delete action. Drives the
/// shape decisions documented in plan-deletion 1.1:
///
/// - Default click path is *soft delete* — the modal copy explains
///   archive + retention + cascade and emphasizes that the agent rows
///   stick around.
/// - Holding Shift while clicking the primary button flips the modal
///   to a *hard-delete* re-confirm step (one extra click is
///   intentional friction). When the org's `plan_archive_retention_days`
///   is 0, the modal opens directly in hard-confirm mode because
///   soft delete collapses to hard at retention=0.
/// - The 409 paths (`plan_has_running_agents`,
///   `auto_mode_in_flight`) keep the modal open and surface an
///   inline banner so the user can click into the offending agent's
///   terminal without dismissing first.
///
/// Accessibility: `role="dialog"` + `aria-modal="true"` +
/// `aria-labelledby` (title) + `aria-describedby` (body). Focus
/// lands on Cancel on mount, traps inside the dialog (Tab cycles),
/// returns to the trigger element on unmount, ESC closes when not
/// busy. axe-core passes (verified in `DeletePlanModal.test.tsx`).
export function DeletePlanModal({ planName, retentionDays, onClose }: DeletePlanModalProps) {
  const titleId = useId();
  const descId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const cancelButtonRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLElement | null>(null);
  const [busy, setBusy] = useState(false);
  // 'soft'  → first click issues a soft delete (or hard, when retentionDays=0).
  // 'hard'  → user held Shift on the primary; one more click commits hard.
  const initialStage: "soft" | "hard" = retentionDays === 0 ? "hard" : "soft";
  const [stage, setStage] = useState<"soft" | "hard">(initialStage);
  const [error, setError] = useState<string | null>(null);
  const [runningAgents, setRunningAgents] = useState<string[] | null>(null);
  // Server-side cascade preview. `null` until the dry-run fetch
  // resolves; `undefined` if the fetch errored (we still let the user
  // try the delete — the real DELETE will surface a fresh error and
  // the cascade preview is best-effort UX, not a gate).
  const [preview, setPreview] = useState<DeletePlanPreview | null | undefined>(null);

  const deletePlan = usePlanStore((s) => s.deletePlan);
  const previewDeletePlan = usePlanStore((s) => s.previewDeletePlan);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const goToAgent = useGoToAgent();

  // Kick off the dry-run preview on open. The cascade is independent
  // of soft/hard (both flush the same DB rows; only the file action
  // differs), so we fetch once.
  useEffect(() => {
    let cancelled = false;
    previewDeletePlan(planName)
      .then((p) => {
        if (cancelled) return;
        setPreview(p);
        // Surface gate state proactively so the user does not have to
        // click Delete to discover the plan is blocked.
        if (p.blockedBy) {
          if (p.blockedBy.runningAgents.length > 0) {
            setRunningAgents(p.blockedBy.runningAgents);
            setError("Cannot delete: this plan has running agents.");
          } else if (p.blockedBy.autoModeInFlight) {
            setError(
              "Cannot delete: auto-mode is mid-flight (open fix attempt). Pause auto-mode or wait for it to settle.",
            );
          }
        }
      })
      .catch(() => {
        if (cancelled) return;
        // Preview is best-effort — fall back to "no preview shown".
        // The user can still click Delete; the real DELETE will
        // surface any fresh error.
        setPreview(undefined);
      });
    return () => {
      cancelled = true;
    };
  }, [planName, previewDeletePlan]);

  useEffect(() => {
    triggerRef.current = (document.activeElement as HTMLElement | null) ?? null;
    cancelButtonRef.current?.focus();
    return () => {
      // Return focus to the element that opened the modal. Guarded
      // because the trigger may have been unmounted (e.g. plan switch).
      const el = triggerRef.current;
      if (el && typeof el.focus === "function" && document.contains(el)) {
        el.focus();
      }
    };
  }, []);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        if (!busy) onClose();
        return;
      }
      if (e.key !== "Tab") return;
      const root = dialogRef.current;
      if (!root) return;
      const items = Array.from(
        root.querySelectorAll<HTMLElement>(
          "button:not([disabled]), [href], input:not([disabled]), [tabindex]:not([tabindex='-1'])",
        ),
      ).filter((el) => !el.hasAttribute("data-focus-skip"));
      if (items.length === 0) return;
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (active === first || !root.contains(active)) {
          e.preventDefault();
          last.focus();
        }
      } else if (active === last) {
        e.preventDefault();
        first.focus();
      }
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const isHardConfirm = stage === "hard";
  const bodyText = isHardConfirm
    ? "Permanently deletes the plan file and cascades related rows. This cannot be undone."
    : `Moves the plan file to the archive and cascades related rows. Recoverable for ${retentionDays} day${retentionDays === 1 ? "" : "s"} from the Activity tab. Agents that ran for this plan stay in the agent list for historic reference.`;
  const heading = isHardConfirm
    ? `Permanently delete plan ${planName}?`
    : `Delete plan ${planName}?`;
  const primaryLabel = isHardConfirm
    ? busy
      ? "Deleting…"
      : "Permanently delete"
    : busy
      ? "Deleting…"
      : "Delete";
  // Only show the Shift hint when we're on the soft path AND the org
  // hasn't opted out of retention — otherwise the modifier does nothing.
  const showShiftHint = !isHardConfirm && retentionDays > 0;

  // The dry-run preview disables Delete proactively when the plan is
  // blocked. Once the user is *busy* committing a delete, this latch
  // doesn't matter — the in-flight request handles its own UI.
  const previewBlocked = preview != null && preview.blockedBy != null;
  const primaryDisabled = busy || previewBlocked;

  async function runDelete(hard: boolean) {
    setError(null);
    setRunningAgents(null);
    setBusy(true);
    try {
      await deletePlan(planName, hard ? { hard: true } : undefined);
      onClose();
    } catch (e) {
      if (e instanceof HttpError && e.status === 409) {
        const body = e.body as
          | PlanHasRunningAgentsBody
          | AutoModeInFlightBody
          | GenericErrorBody
          | undefined;
        if (body && body.error === "plan_has_running_agents") {
          const agents = (body as PlanHasRunningAgentsBody).agents ?? [];
          setRunningAgents(agents);
          setError("Cannot delete: this plan has running agents.");
        } else if (body && body.error === "auto_mode_in_flight") {
          setError(
            "Cannot delete: auto-mode is mid-flight (open fix attempt). Pause auto-mode or wait for it to settle.",
          );
        } else {
          setError(`Cannot delete: ${body?.error ?? "blocked"}`);
        }
      } else if (e instanceof HttpError && e.status === 404) {
        // Plan already gone (e.g. another tab raced this delete). The
        // sidebar will catch up via the WS event; treat as success so
        // the user isn't left staring at an unactionable error.
        onClose();
        return;
      } else {
        setError(`Delete failed: ${errorMessage(e)}`);
      }
    } finally {
      setBusy(false);
    }
  }

  function onPrimary(e: React.MouseEvent<HTMLButtonElement>) {
    if (busy) return;
    if (isHardConfirm) {
      runDelete(true);
      return;
    }
    if (e.shiftKey) {
      // Stage transition only — the Shift modifier doesn't issue the
      // delete on its own. The user confirms by clicking the
      // "Permanently delete" button on the re-rendered modal.
      setStage("hard");
      setError(null);
      setRunningAgents(null);
      return;
    }
    runDelete(false);
  }

  function onAgentClick(id: string) {
    selectAgent(id);
    goToAgent(id);
    onClose();
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onClick={() => !busy && onClose()}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descId}
        className="bg-gray-900 border border-gray-700 rounded-md shadow-xl p-5 w-full max-w-md"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="text-base font-semibold text-gray-100">
          {heading}
        </h2>
        <p id={descId} className="mt-2 text-sm text-gray-300">
          {bodyText}
        </p>
        {showShiftHint && (
          <p className="mt-2 text-xs text-gray-500">
            Hold{" "}
            <kbd className="px-1 py-0.5 rounded border border-gray-700 bg-gray-800 font-mono text-[10px] text-gray-300">
              Shift
            </kbd>{" "}
            while clicking Delete to permanently delete (skip archive).
          </p>
        )}
        {/* Cascade preview from the dry-run fetch. Loading shows
            placeholder copy; resolved shows the per-table summary;
            error silently collapses (the user can still try Delete). */}
        <div
          className="mt-3 rounded border border-gray-700 bg-gray-800/40 px-3 py-2 text-xs text-gray-300"
          data-testid="delete-plan-cascade-preview"
        >
          {preview === null ? (
            <span className="text-gray-500">Computing cascade preview…</span>
          ) : preview === undefined ? (
            <span className="text-gray-500">
              Cascade preview unavailable. The delete will still proceed.
            </span>
          ) : (
            formatCascadeSummary(preview.wouldDelete)
          )}
        </div>
        {error && (
          <div
            role="alert"
            className="mt-3 rounded border border-red-700/50 bg-red-900/20 px-3 py-2 text-xs text-red-200"
          >
            <div>{error}</div>
            {runningAgents && runningAgents.length > 0 && (
              <ul className="mt-2 space-y-1">
                {runningAgents.map((id) => (
                  <li key={id}>
                    <button
                      type="button"
                      onClick={() => onAgentClick(id)}
                      className="font-mono text-red-100 underline decoration-dotted hover:text-white"
                      title="Open this agent's terminal"
                    >
                      {id}
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
        <div className="mt-5 flex items-center justify-end gap-2">
          <button
            ref={cancelButtonRef}
            type="button"
            onClick={onClose}
            disabled={busy}
            className="px-3 py-1.5 text-xs text-gray-300 hover:text-gray-100 disabled:opacity-50 transition"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onPrimary}
            disabled={primaryDisabled}
            className="px-3 py-1.5 text-xs bg-red-700 hover:bg-red-600 disabled:opacity-50 disabled:cursor-not-allowed text-white rounded transition"
            title={
              previewBlocked
                ? "Plan is currently blocked. Resolve the blocker before deleting."
                : undefined
            }
          >
            {primaryLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
