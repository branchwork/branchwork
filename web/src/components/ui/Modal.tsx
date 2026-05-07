import { useEffect, useId, useRef, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";

interface ModalProps {
  open: boolean;
  onClose: () => void;
  /// Title rendered inside the dialog. Always present so the dialog has
  /// an accessible name (`aria-labelledby`); pass `titleVisuallyHidden`
  /// when the visual design needs to omit the heading.
  title: string;
  titleVisuallyHidden?: boolean;
  /// Optional secondary description used for `aria-describedby`. Renders
  /// directly after the title when provided.
  description?: string;
  /// When true, clicking the backdrop closes the modal. Default false:
  /// destructive flows (audit §1: `confirm()` replacements) should
  /// commit to a button so an accidental backdrop click doesn't dismiss.
  closeOnBackdropClick?: boolean;
  /// Optional ref to the element that should receive focus on open. If
  /// omitted, the dialog itself takes focus (it is `tabIndex={-1}`), so
  /// keyboard users can Tab into the first focusable child.
  initialFocusRef?: RefObject<HTMLElement | null>;
  /// Panel max-width. Default `md` matches the original confirmation
  /// dialogs; `2xl` is for table-driven dialogs (e.g. StaleBranchesButton)
  /// that need to fit a row preview without horizontal scroll.
  size?: "md" | "2xl";
  children: ReactNode;
}

const SIZE_CLASS: Record<NonNullable<ModalProps["size"]>, string> = {
  md: "max-w-md",
  "2xl": "max-w-2xl",
};

/// Portal-rendered modal. Implements the contract surfaced by audit §8
/// (StaleBranchesButton modal had no focus trap, no Esc, no return-focus
/// on close):
///
/// - `role="dialog"` + `aria-modal="true"` + `aria-labelledby` (title)
///   + optional `aria-describedby` (description).
/// - Esc dismisses.
/// - Tab cycles focus inside the dialog (focus trap).
/// - Focus returns to the element that was focused when the modal
///   opened.
/// - Backdrop click dismisses only when explicitly opted in.
///
/// `BulkDeleteModal` and `DeletePlanModal` hand-rolled the same
/// contract before this primitive existed; both still pass their own
/// axe tests and were left in place during 6.1 because the refactor
/// is a deduplication, not an a11y fix. Migrating them remains a
/// follow-up.
export function Modal({
  open,
  onClose,
  title,
  titleVisuallyHidden,
  description,
  closeOnBackdropClick = false,
  initialFocusRef,
  size = "md",
  children,
}: ModalProps) {
  const titleId = useId();
  const descId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open) return;
    triggerRef.current = (document.activeElement as HTMLElement | null) ?? null;
    const target = initialFocusRef?.current ?? dialogRef.current;
    target?.focus();
    return () => {
      const el = triggerRef.current;
      if (el && typeof el.focus === "function" && document.contains(el)) {
        el.focus();
      }
    };
  }, [open, initialFocusRef]);

  useEffect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
        return;
      }
      if (e.key !== "Tab") return;
      const root = dialogRef.current;
      if (!root) return;
      const items = Array.from(
        root.querySelectorAll<HTMLElement>(
          "button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])",
        ),
      );
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
  }, [open, onClose]);

  if (!open) return null;

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onClick={() => {
        if (closeOnBackdropClick) onClose();
      }}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={description ? descId : undefined}
        tabIndex={-1}
        className={`bg-gray-900 border border-gray-700 rounded-md shadow-xl p-5 w-full ${SIZE_CLASS[size]} max-h-[85vh] overflow-y-auto outline-none`}
        onClick={(e) => e.stopPropagation()}
      >
        <h2
          id={titleId}
          className={titleVisuallyHidden ? "sr-only" : "text-base font-semibold text-gray-100"}
        >
          {title}
        </h2>
        {description && (
          <p id={descId} className="mt-2 text-sm text-gray-300">
            {description}
          </p>
        )}
        {children}
      </div>
    </div>,
    document.body,
  );
}
