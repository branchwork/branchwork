import { cloneElement, isValidElement, type ReactElement } from "react";

export type TapMin = "36px" | "44px";

interface TouchTargetProps<P extends { className?: string }> {
  /// Minimum hit-area edge at viewports below `md`. Defaults to "36px"
  /// — passes Apple/Material's 36-pt floor for compact UI; bump to
  /// "44px" only when a control sits in an isolated region (no
  /// horizontal sibling buttons within ~10px).
  min?: TapMin;
  /// Single React element. Its `className` prop is augmented with hit-
  /// region utilities; it must accept `className` as a string. The
  /// visible size stays unchanged — the click region grows via a
  /// `::before` pseudo-element pinned at `z-index: -10`, so overlapping
  /// siblings keep precedence on shared overlap.
  children: ReactElement<P>;
}

/// Tailwind classes that materialise a transparent 36/44px-minimum
/// pseudo-element centered on the host element. Layout-neutral on
/// `md+` (the pseudo-element is `display: none`); on `<md` it sits
/// behind the host's content via `-z-10` so it doesn't obscure
/// siblings' visible click regions.
const PSEUDO_BASE =
  "relative " +
  "before:content-[''] before:absolute " +
  "before:left-1/2 before:top-1/2 " +
  "before:-translate-x-1/2 before:-translate-y-1/2 " +
  "before:w-full before:h-full " +
  "before:-z-10 " +
  "md:before:hidden";

const PSEUDO_MIN: Record<TapMin, string> = {
  "36px": "before:min-w-9 before:min-h-9",
  "44px": "before:min-w-11 before:min-h-11",
};

/// Inflate a control's tap region to ≥`min` on `<md` viewports without
/// changing its visual size. Wraps a single interactive child (button /
/// link / select) and merges utility classes onto its `className`. The
/// extra hit area comes from a `::before` pseudo-element so layout is
/// untouched and surrounding siblings don't shift.
///
/// Audit §9 callsites: Sidebar plan-row Link, PlanBoard status filter
/// pills, TaskCard CI dismiss button, TaskCard driver `<select>`.
export function TouchTarget<P extends { className?: string }>({
  min = "36px",
  children,
}: TouchTargetProps<P>) {
  if (!isValidElement(children)) return children;
  const props = children.props as P;
  const existing = props.className;
  if (typeof existing !== "string" && existing !== undefined) {
    // Function-form className (NavLink) — bail rather than break the
    // active-state predicate. The four flagged callsites all use
    // string className.
    return children;
  }
  const merged = [existing, PSEUDO_BASE, PSEUDO_MIN[min]].filter(Boolean).join(" ");
  return cloneElement(children, { className: merged } as Partial<P>);
}
