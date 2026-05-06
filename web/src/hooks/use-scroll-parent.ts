import { useLayoutEffect, useState } from "react";

/// Walks up the DOM from the given element to find the nearest
/// scrollable ancestor (computed `overflow-y` is `auto` or `scroll`).
/// Falls back to `document.documentElement` if no scrolling ancestor is
/// found. Used by virtualisers in PhaseCard / AgentTree, both of which
/// sit inside the App.tsx `flex-1 overflow-auto` route container — that
/// is the element @tanstack/react-virtual needs to observe.
///
/// In jsdom (vitest), `getComputedStyle` works but `getBoundingClientRect`
/// returns zeros, so callers that depend on viewport size should also
/// pass `initialRect` to seed the virtualiser.
export function useScrollParent(
  ref: React.RefObject<HTMLElement | null>,
): HTMLElement | null {
  const [scrollEl, setScrollEl] = useState<HTMLElement | null>(null);

  useLayoutEffect(() => {
    let cur: HTMLElement | null = ref.current?.parentElement ?? null;
    while (cur && cur !== document.body) {
      const css = window.getComputedStyle(cur);
      if (css.overflowY === "auto" || css.overflowY === "scroll") {
        setScrollEl(cur);
        return;
      }
      cur = cur.parentElement;
    }
    setScrollEl(document.documentElement);
  }, [ref]);

  return scrollEl;
}
