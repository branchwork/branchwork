import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";
import {
  BUTTON_VARIANT_CLASSES,
  type ButtonVariant,
  type ButtonSize,
} from "./Button.js";

interface IconButtonProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "aria-label" | "children"> {
  /// Required: an icon-only control has no visible label, so the
  /// accessible name must come from `aria-label`. The TypeScript type
  /// makes this a compile-time error if omitted, mirroring the audit
  /// finding that flagged unlabelled `x` / kebab buttons in §8.
  "aria-label": string;
  variant?: ButtonVariant;
  size?: ButtonSize;
  /// The icon glyph (svg or character). Wrapped in `aria-hidden`
  /// because the accessible name is carried by `aria-label`.
  icon: ReactNode;
}

const PADDING_CLASSES: Record<ButtonSize, string> = {
  sm: "p-1 text-xs",
  md: "p-1.5 text-sm",
};

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(
  function IconButton(
    { icon, variant = "ghost", size = "md", type = "button", className, ...rest },
    ref,
  ) {
    const classes = [
      "inline-flex items-center justify-center rounded transition aspect-square",
      "disabled:cursor-not-allowed",
      PADDING_CLASSES[size],
      BUTTON_VARIANT_CLASSES[variant],
      className,
    ]
      .filter(Boolean)
      .join(" ");
    return (
      <button ref={ref} type={type} className={classes} {...rest}>
        <span aria-hidden="true">{icon}</span>
      </button>
    );
  },
);
