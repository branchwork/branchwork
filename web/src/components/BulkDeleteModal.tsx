import { useEffect, useId, useRef, useState } from "react";
import { HttpError } from "../api.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { errorMessage } from "../lib/error.js";
import { useGoToAgent } from "../hooks/use-route-selection.js";

interface BulkDeleteModalProps {
  planNames: string[];
  /// Org-level retention. Same semantics as DeletePlanModal:
  /// 0 forces hard delete (no archive path).
  retentionDays: number;
  onClose: () => void;
  /// Called for every plan that the server confirms deleted. The
  /// parent removes that name from its selection set so a 409 mid-stream
  /// leaves only the blocker (and any plans after it) selected, ready
  /// for retry once the user kills the offending agent.
  onPlanDeleted: (planName: string) => void;
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

interface BlockedState {
  planName: string;
  message: string;
  agents: string[] | null;
}

/// Bulk-delete confirmation modal. Drives plan-deletion 2.1.
///
/// - Lists the selected plan names by name (no per-plan dry-run preview
///   — bulk runs would explode into N round-trips and the modal copy
///   already names every target). The cascade detail is per-plan, so
///   users who want it open the per-plan modal from PlanBoard instead.
/// - Soft delete by default. Holding Shift on the primary button flips
///   to a hard-confirm step, identical to the per-plan modal so muscle
///   memory transfers. retentionDays===0 collapses to hard-confirm
///   directly because soft has no archive to land in.
/// - Iterates the selected plans **serially** (not in parallel) — keeps
///   audit-log ordering stable and avoids hammering the file watcher
///   with concurrent file removes.
/// - On a 409 (running agents / auto-mode in flight), STOPS the
///   iteration. The modal stays open, surfaces which plan blocked
///   (and the agent IDs when available), and the parent's selection
///   keeps the blocker + any plans after it. Cancel closes without any
///   further request.
/// - Clicking an agent ID in the blocker banner navigates to that
///   agent's terminal and closes the modal (matches DeletePlanModal).
///
/// Accessibility: role="dialog" + aria-modal="true" + aria-labelledby
/// (title) + aria-describedby (body), focus trap on Tab/Shift-Tab, ESC
/// closes when not busy, focus returns to the trigger on unmount.
export function BulkDeleteModal({
  planNames,
  retentionDays,
  onClose,
  onPlanDeleted,
}: BulkDeleteModalProps) {
  const titleId = useId();
  const descId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const cancelButtonRef = useRef<HTMLButtonElement>(null);
  const triggerRef = useRef<HTMLElement | null>(null);
  const [busy, setBusy] = useState(false);
  // Names of plans that the server has confirmed deleted in the current
  // run. Tracked locally for the progress line ("2/3 deleted") because
  // the parent's selection set is the authoritative "still pending"
  // signal — we do not duplicate it here.
  const [deletedSoFar, setDeletedSoFar] = useState<string[]>([]);
  const [blocked, setBlocked] = useState<BlockedState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const initialStage: "soft" | "hard" = retentionDays === 0 ? "hard" : "soft";
  const [stage, setStage] = useState<"soft" | "hard">(initialStage);

  const deletePlan = usePlanStore((s) => s.deletePlan);
  const selectAgent = useAgentStore((s) => s.selectAgent);
  const goToAgent = useGoToAgent();

  useEffect(() => {
    triggerRef.current = (document.activeElement as HTMLElement | null) ?? null;
    cancelButtonRef.current?.focus();
    return () => {
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
  const total = planNames.length;
  const heading = isHardConfirm
    ? `Permanently delete ${total} plan${total === 1 ? "" : "s"}?`
    : `Delete ${total} plan${total === 1 ? "" : "s"}?`;
  const bodyText = isHardConfirm
    ? "Permanently deletes the listed plan files and cascades related rows. This cannot be undone."
    : `Moves the listed plan files to the archive and cascades related rows. Recoverable for ${retentionDays} day${retentionDays === 1 ? "" : "s"} from the Activity tab.`;
  const primaryLabel = isHardConfirm
    ? busy
      ? `Deleting ${deletedSoFar.length}/${total}…`
      : `Permanently delete ${total}`
    : busy
      ? `Deleting ${deletedSoFar.length}/${total}…`
      : `Delete ${total}`;
  const showShiftHint = !isHardConfirm && retentionDays > 0;

  async function runBulkDelete(hard: boolean) {
    setError(null);
    setBlocked(null);
    setBusy(true);
    const deletedNames: string[] = [];
    try {
      // SERIAL on purpose (not Promise.all): keeps audit-log ordering
      // stable and avoids racing the file watcher on `plans_dir`.
      for (const name of planNames) {
        try {
          await deletePlan(name, hard ? { hard: true } : undefined);
        } catch (e) {
          if (e instanceof HttpError && e.status === 409) {
            const body = e.body as
              | PlanHasRunningAgentsBody
              | AutoModeInFlightBody
              | GenericErrorBody
              | undefined;
            if (body && body.error === "plan_has_running_agents") {
              const agents =
                (body as PlanHasRunningAgentsBody).agents ?? [];
              setBlocked({
                planName: name,
                message: `Cannot delete "${name}": this plan has running agents.`,
                agents,
              });
            } else if (body && body.error === "auto_mode_in_flight") {
              setBlocked({
                planName: name,
                message: `Cannot delete "${name}": auto-mode is mid-flight (open fix attempt). Pause auto-mode or wait for it to settle.`,
                agents: null,
              });
            } else {
              setBlocked({
                planName: name,
                message: `Cannot delete "${name}": ${body?.error ?? "blocked"}`,
                agents: null,
              });
            }
            // Halt the loop on the first 409 — the brief is explicit
            // that prior plans stay deleted and remaining stay selected.
            return;
          }
          if (e instanceof HttpError && e.status === 404) {
            // Plan already gone (raced from another tab). Treat as a
            // success for selection-tracking purposes so the user is
            // not blocked on a stale row.
            deletedNames.push(name);
            onPlanDeleted(name);
            setDeletedSoFar((prev) => [...prev, name]);
            continue;
          }
          // Anything else — surface the message and halt. The parent's
          // selection still has this plan + the rest, so the user can
          // retry once the underlying issue is resolved.
          setError(`Delete failed on "${name}": ${errorMessage(e)}`);
          return;
        }
        deletedNames.push(name);
        onPlanDeleted(name);
        setDeletedSoFar((prev) => [...prev, name]);
      }
      // All plans deleted successfully — close the modal. The parent's
      // selection set is now empty (every name went through onPlanDeleted)
      // so the dashboard's sticky footer auto-hides on the next render.
      onClose();
    } finally {
      setBusy(false);
    }
  }

  function onPrimary(e: React.MouseEvent<HTMLButtonElement>) {
    if (busy) return;
    if (isHardConfirm) {
      runBulkDelete(true);
      return;
    }
    if (e.shiftKey) {
      setStage("hard");
      setError(null);
      setBlocked(null);
      return;
    }
    runBulkDelete(false);
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
        {/* Selected plan names. The list is the only "what will happen"
            preview the bulk modal shows — per-plan cascade counts are
            intentionally omitted (would be N HTTP round-trips and noisy
            copy). Names already deleted in this run are struck through
            so the user can see progress on a partial 409 halt. */}
        <ul
          className="mt-3 max-h-48 overflow-auto rounded border border-gray-700 bg-gray-800/40 px-3 py-2 text-xs text-gray-300 font-mono space-y-0.5"
          data-testid="bulk-delete-plan-list"
        >
          {planNames.map((name) => {
            const done = deletedSoFar.includes(name);
            const isBlocker = blocked?.planName === name;
            return (
              <li
                key={name}
                className={
                  done
                    ? "line-through text-gray-600"
                    : isBlocker
                      ? "text-red-300"
                      : ""
                }
                title={
                  done
                    ? "Deleted"
                    : isBlocker
                      ? "Blocked — see banner below"
                      : undefined
                }
              >
                {name}
              </li>
            );
          })}
        </ul>
        {blocked && (
          <div
            role="alert"
            className="mt-3 rounded border border-red-700/50 bg-red-900/20 px-3 py-2 text-xs text-red-200"
          >
            <div>{blocked.message}</div>
            {blocked.agents && blocked.agents.length > 0 && (
              <ul className="mt-2 space-y-1">
                {blocked.agents.map((id) => (
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
        {error && (
          <div
            role="alert"
            className="mt-3 rounded border border-red-700/50 bg-red-900/20 px-3 py-2 text-xs text-red-200"
          >
            {error}
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
            disabled={busy || total === 0}
            className="px-3 py-1.5 text-xs bg-red-700 hover:bg-red-600 disabled:opacity-50 disabled:cursor-not-allowed text-white rounded transition"
          >
            {primaryLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
