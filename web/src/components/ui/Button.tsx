import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import { BRAND, DANGER, MUTED, WARN } from "../../lib/tokens.js";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "danger" | "warn";
export type ButtonSize = "sm" | "md";

/// Variant class strings shared with `IconButton`. Re-exported so the
/// icon-only primitive can apply the same palette without duplicating
/// the token wiring.
export const BUTTON_VARIANT_CLASSES: Record<ButtonVariant, string> = {
  primary: `${BRAND.solidBg} ${BRAND.solidBgHover} ${BRAND.solidText} ${MUTED.disabledBg} ${MUTED.disabledText}`,
  secondary: `${MUTED.solidBg} border ${MUTED.softBorder} ${MUTED.solidText} hover:border-gray-600 disabled:opacity-50`,
  ghost: "bg-transparent text-gray-300 hover:text-gray-100 disabled:opacity-50",
  danger: `${DANGER.solidBg} ${DANGER.solidBgHover} ${DANGER.solidText} disabled:opacity-50`,
  /// Amber/warn surface used for non-destructive but-attention-needed
  /// actions: "Request shutdown" on the runner row sits between the
  /// gray Settings/Select buttons and the red Revoke. Picks up the
  /// shared WARN token so the audit-log warn pill, banners, and this
  /// button all read the same hue.
  warn: `${WARN.solidBg} ${WARN.solidBgHover} ${WARN.solidText} disabled:opacity-50`,
};

const SIZE_CLASSES: Record<ButtonSize, string> = {
  sm: "px-3 py-1.5 text-xs",
  md: "px-4 py-2 text-sm",
};

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  /// When true, renders a leading spinner and forces `disabled`. The
  /// outer button also gets `aria-busy="true"` so screen readers announce
  /// the in-flight state.
  loading?: boolean;
  children?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  {
    variant = "primary",
    size = "md",
    loading,
    disabled,
    children,
    className,
    type = "button",
    ...rest
  },
  ref,
) {
  const classes = [
    "inline-flex items-center justify-center gap-1.5 rounded transition",
    "disabled:cursor-not-allowed",
    SIZE_CLASSES[size],
    BUTTON_VARIANT_CLASSES[variant],
    className,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      className={classes}
      {...rest}
    >
      {loading && <Spinner />}
      {children}
    </button>
  );
});

function Spinner() {
  return (
    <span
      aria-hidden="true"
      className="inline-block w-3 h-3 rounded-full border-2 border-current border-r-transparent animate-spin"
    />
  );
}
