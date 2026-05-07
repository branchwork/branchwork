import { useRef, type KeyboardEvent, type ReactNode } from "react";

interface TabConfig<V extends string> {
  value: V;
  label: ReactNode;
  disabled?: boolean;
  /// Optional `title` attribute (tooltip — used today for the disabled
  /// "Diff" tab when no base commit is available).
  title?: string;
  /// Optional id for a corresponding `<TabPanel>` to wire
  /// `aria-controls`.
  controlsId?: string;
}

interface TabsProps<V extends string> {
  value: V;
  onChange: (value: V) => void;
  /// Accessible label for the tablist (`aria-label` on
  /// `[role="tablist"]`).
  label: string;
  tabs: ReadonlyArray<TabConfig<V>>;
  className?: string;
  /// Custom render for the inside of each tab button. Defaults to the
  /// raw `label` from the config.
  renderTab?: (cfg: TabConfig<V>, selected: boolean) => ReactNode;
  /// Per-tab class string. Receives `selected` so callers can re-use
  /// existing styling. Defaults to a horizontal-bar tablist matching
  /// the AgentPanel design.
  tabClassName?: (cfg: TabConfig<V>, selected: boolean) => string;
}

function DEFAULT_TAB_CLASS<V extends string>(cfg: TabConfig<V>, selected: boolean): string {
  if (selected) {
    return "px-3 py-1.5 text-xs font-medium text-gray-200 border-b-2 border-indigo-500 transition";
  }
  if (cfg.disabled) {
    return "px-3 py-1.5 text-xs font-medium text-gray-700 cursor-not-allowed transition";
  }
  return "px-3 py-1.5 text-xs font-medium text-gray-500 hover:text-gray-300 transition";
}

/// Accessible tablist primitive.
///
/// Replaces the hand-rolled tab buttons in `AgentPanel.tsx:101` flagged
/// by audit §8 (no `role`, no arrow-key nav, no roving tabindex).
///
/// Keyboard contract (matches WAI-ARIA Authoring Practices for tabs
/// with **automatic activation** — pressing arrow keys both moves
/// focus and selects the tab; Enter/Space are not required because
/// the click is implicit). Tab content here is cheap to switch between
/// (Output / Diff are already lazy-rendered) so automatic activation
/// is the better UX:
///
/// - Tab into the tablist lands on the selected tab (roving tabindex).
/// - Arrow Left/Right and Up/Down: move focus + select previous/next
///   non-disabled tab (wraps).
/// - Home / End: jump to the first / last non-disabled tab.
/// - Tab out of the tablist proceeds to the panel content.
export function Tabs<V extends string>({
  value,
  onChange,
  label,
  tabs,
  className = "",
  renderTab,
  tabClassName,
}: TabsProps<V>) {
  const tablistRef = useRef<HTMLDivElement>(null);

  function focusAndSelect(idx: number) {
    const root = tablistRef.current;
    if (!root) return;
    const buttons = Array.from(
      root.querySelectorAll<HTMLButtonElement>('[role="tab"]:not(:disabled)'),
    );
    const t = buttons[idx];
    if (!t) return;
    t.focus();
    const v = t.dataset.value as V | undefined;
    if (v !== undefined) onChange(v);
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    const root = tablistRef.current;
    if (!root) return;
    const buttons = Array.from(
      root.querySelectorAll<HTMLButtonElement>('[role="tab"]:not(:disabled)'),
    );
    if (buttons.length === 0) return;
    const active = document.activeElement as HTMLElement | null;
    const current = buttons.findIndex((t) => t === active);
    switch (e.key) {
      case "ArrowRight":
      case "ArrowDown": {
        e.preventDefault();
        const next = current < 0 ? 0 : (current + 1) % buttons.length;
        focusAndSelect(next);
        break;
      }
      case "ArrowLeft":
      case "ArrowUp": {
        e.preventDefault();
        const next =
          current < 0 ? buttons.length - 1 : (current - 1 + buttons.length) % buttons.length;
        focusAndSelect(next);
        break;
      }
      case "Home": {
        e.preventDefault();
        focusAndSelect(0);
        break;
      }
      case "End": {
        e.preventDefault();
        focusAndSelect(buttons.length - 1);
        break;
      }
    }
  }

  return (
    <div
      ref={tablistRef}
      role="tablist"
      aria-label={label}
      onKeyDown={onKeyDown}
      tabIndex={-1}
      className={`flex ${className}`}
    >
      {tabs.map((cfg) => {
        const selected = cfg.value === value;
        const cls = tabClassName ? tabClassName(cfg, selected) : DEFAULT_TAB_CLASS(cfg, selected);
        return (
          <button
            key={cfg.value}
            type="button"
            role="tab"
            data-value={cfg.value}
            aria-selected={selected}
            aria-controls={cfg.controlsId}
            tabIndex={selected ? 0 : -1}
            disabled={cfg.disabled}
            title={cfg.title}
            onClick={() => {
              if (cfg.disabled) return;
              onChange(cfg.value);
            }}
            className={cls}
          >
            {renderTab?.(cfg, selected) ?? cfg.label}
          </button>
        );
      })}
    </div>
  );
}
