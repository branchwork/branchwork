import { type HTMLAttributes, type ReactNode } from "react";
import { BRAND, DANGER, MUTED, SUCCESS, WARN } from "../../lib/tokens.js";

export type BadgeVariant = "brand" | "success" | "warn" | "danger" | "muted";

interface BadgeProps extends HTMLAttributes<HTMLSpanElement> {
  variant?: BadgeVariant;
  /// When true, renders an animated dot for in-progress semantics
  /// (matches the existing "merging / awaiting CI / fixing CI" pills).
  pulse?: boolean;
  /// Hide the leading status dot. The pill text alone carries semantics
  /// for badges that want a denser footprint (filter chips, tags).
  hideDot?: boolean;
  children?: ReactNode;
}

const VARIANT_CLASSES: Record<
  BadgeVariant,
  { bg: string; border: string; text: string; dot: string }
> = {
  brand: {
    bg: BRAND.softBg,
    border: BRAND.softBorder,
    text: BRAND.softText,
    dot: BRAND.softDot,
  },
  success: {
    bg: SUCCESS.softBg,
    border: SUCCESS.softBorder,
    text: SUCCESS.softText,
    dot: SUCCESS.softDot,
  },
  warn: {
    bg: WARN.softBg,
    border: WARN.softBorder,
    text: WARN.softText,
    dot: WARN.softDot,
  },
  danger: {
    bg: DANGER.softBg,
    border: DANGER.softBorder,
    text: DANGER.softText,
    dot: DANGER.softDot,
  },
  muted: {
    bg: MUTED.softBg,
    border: MUTED.softBorder,
    text: MUTED.softText,
    dot: MUTED.softDot,
  },
};

/// Status pill primitive. Same anatomy as the inline pills in
/// `AutoModeStatusPill` and the status-filter chips: rounded border,
/// soft background, optional pulsing dot. Variants map to the semantic
/// palette in `lib/tokens.ts`.
export function Badge({
  variant = "muted",
  pulse,
  hideDot,
  children,
  className,
  ...rest
}: BadgeProps) {
  const v = VARIANT_CLASSES[variant];
  return (
    <span
      className={[
        "inline-flex items-center gap-1.5 px-2 py-0.5 text-xs rounded border",
        v.bg,
        v.border,
        v.text,
        className,
      ]
        .filter(Boolean)
        .join(" ")}
      {...rest}
    >
      {!hideDot && (
        <span
          aria-hidden="true"
          className={`w-1.5 h-1.5 rounded-full ${v.dot}${pulse ? " animate-pulse" : ""}`}
        />
      )}
      {children}
    </span>
  );
}
