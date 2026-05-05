import { IconButton } from "./ui/IconButton.js";
import {
  useToastStore,
  toastRole,
  type Toast,
  type ToastKind,
} from "../stores/toast-store.js";

/// Tailwind palette per kind. Mirrors the soft-surface tokens used by
/// `Badge`/`AutoModeStatusPill` (info=indigo, success=emerald,
/// warn=amber, error=red) so the toaster reads as part of the same
/// visual family rather than a one-off accent.
const KIND_CLASSES: Record<ToastKind, string> = {
  info: "border-indigo-700/50 bg-indigo-900/40 text-indigo-100",
  success: "border-emerald-700/50 bg-emerald-900/40 text-emerald-100",
  warn: "border-amber-700/50 bg-amber-900/40 text-amber-100",
  error: "border-red-700/50 bg-red-900/40 text-red-100",
};

/// Toaster outlet. Reads from `useToastStore` and renders fixed
/// bottom-right above the connection indicator (which sits at
/// `bottom-3`). Non-blocking — pointer-events stay on the toasts
/// themselves, never on the wrapper.
export function Toaster() {
  const toasts = useToastStore((s) => s.toasts);
  const dismiss = useToastStore((s) => s.dismiss);

  if (toasts.length === 0) return null;

  return (
    <div
      data-testid="toaster"
      className="fixed bottom-12 right-3 z-50 flex flex-col gap-2 max-w-sm pointer-events-none"
    >
      {toasts.map((t) => (
        <ToastCard key={t.id} toast={t} onDismiss={() => dismiss(t.id)} />
      ))}
    </div>
  );
}

interface ToastCardProps {
  toast: Toast;
  onDismiss: () => void;
}

function ToastCard({ toast, onDismiss }: ToastCardProps) {
  return (
    <div
      role={toastRole(toast.kind)}
      className={`pointer-events-auto rounded border shadow-lg px-3 py-2 text-sm flex items-start gap-2 ${KIND_CLASSES[toast.kind]}`}
    >
      <div className="flex-1 min-w-0">
        <div className="font-medium break-words">{toast.title}</div>
        {toast.body && (
          <div className="mt-0.5 text-xs opacity-80 break-words">
            {toast.body}
          </div>
        )}
      </div>
      <IconButton
        aria-label="Dismiss notification"
        icon="×"
        size="sm"
        variant="ghost"
        onClick={onDismiss}
        className="-mr-1 -mt-0.5 text-current opacity-70 hover:opacity-100"
      />
    </div>
  );
}
