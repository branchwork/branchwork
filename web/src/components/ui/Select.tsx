import { forwardRef, type SelectHTMLAttributes } from "react";

type SelectSize = "sm" | "md";

interface SelectProps extends Omit<SelectHTMLAttributes<HTMLSelectElement>, "size"> {
  /// Visual size; `sm` matches the existing inline status filter / driver
  /// dropdown footprints, `md` matches the form fields.
  size?: SelectSize;
}

const SIZE_CLASSES: Record<SelectSize, string> = {
  sm: "px-2 py-1 text-xs",
  md: "px-3 py-1.5 text-sm",
};

/// Native `<select>` styled to match the rest of the dashboard. Wraps
/// the element so call sites that previously hand-rolled the same
/// "menu div" pattern (audit §1) can adopt one consistent surface.
///
/// The status menu, driver dropdown, and merge-target dropdown stay on
/// hand-rolled markup for now — they need keyboard nav / typeahead /
/// search behaviour that a native select can't deliver. Per the task
/// brief, those become a separate `Dropdown` primitive in 6.2.
export const Select = forwardRef<HTMLSelectElement, SelectProps>(function Select(
  { size = "md", className, children, ...rest },
  ref,
) {
  return (
    <select
      ref={ref}
      className={[
        "bg-gray-800 border border-gray-700 rounded text-gray-200",
        "focus:outline-none focus:border-indigo-600",
        "disabled:opacity-50 disabled:cursor-not-allowed",
        SIZE_CLASSES[size],
        className,
      ]
        .filter(Boolean)
        .join(" ")}
      {...rest}
    >
      {children}
    </select>
  );
});
