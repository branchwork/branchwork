import { forwardRef, type ButtonHTMLAttributes } from "react";

interface SwitchProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onChange" | "children"> {
  /// Visible label rendered next to the toggle. Also serves as the
  /// accessible name (the `<button role="switch">` wraps both pieces).
  label: string;
  checked: boolean;
  onChange: (next: boolean) => void;
}

/// Toggle switch primitive. Implements the WAI-ARIA `switch` role
/// (`role="switch"` + `aria-checked`) so screen readers announce the
/// on/off state instead of a generic button. Replaces the inline
/// `Switch` defined in `PlanBoard.tsx` (`AutoModeControls`); 0.6 will
/// migrate the call sites.
export const Switch = forwardRef<HTMLButtonElement, SwitchProps>(function Switch(
  { label, checked, onChange, disabled, title, className, ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type="button"
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      disabled={disabled}
      title={title}
      className={[
        "flex items-center gap-2 disabled:opacity-50 disabled:cursor-not-allowed transition group",
        className,
      ]
        .filter(Boolean)
        .join(" ")}
      {...rest}
    >
      <span
        aria-hidden="true"
        className={`relative inline-flex h-4 w-7 rounded-full transition-colors ${
          checked
            ? "bg-emerald-600 group-hover:bg-emerald-500"
            : "bg-gray-700 group-hover:bg-gray-600"
        }`}
      >
        <span
          className={`absolute top-0.5 h-3 w-3 rounded-full bg-white shadow transition-transform ${
            checked ? "translate-x-3.5" : "translate-x-0.5"
          }`}
        />
      </span>
      <span
        className={
          checked
            ? "text-gray-200 font-medium"
            : "text-gray-400 group-hover:text-gray-300"
        }
      >
        {label}
      </span>
    </button>
  );
});
