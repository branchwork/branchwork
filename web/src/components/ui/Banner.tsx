import type { ReactNode } from "react";
import { DANGER, WARN } from "../../lib/tokens.js";

export type BannerKind = "error" | "warn";

interface BannerProps {
  kind?: BannerKind;
  children: ReactNode;
  className?: string;
}

const KIND_CLASSES: Record<BannerKind, string> = {
  error: `${DANGER.softBg} ${DANGER.softBorder} ${DANGER.softText}`,
  warn: `${WARN.softBg} ${WARN.softBorder} ${WARN.softText}`,
};

/// Inline status banner — used for form-level error messages where a
/// dismissable global toast would confuse the flow (audit-listed cases:
/// LoginPage submit, AdminPage per-section saves). Renders with
/// `role="alert"` so screen readers announce the message immediately,
/// matching the toast primitive for `error`/`warn` kinds.
export function Banner({ kind = "error", children, className }: BannerProps) {
  const classes = ["rounded border px-3 py-2 text-xs", KIND_CLASSES[kind], className]
    .filter(Boolean)
    .join(" ");
  return (
    <div role="alert" className={classes}>
      {children}
    </div>
  );
}
