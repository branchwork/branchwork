import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type Ref,
} from "react";

/// Props the consumer must thread through to the trigger `<button>`.
/// Refs in React 19 are regular props on intrinsic elements, so spreading
/// `{...renderProps}` onto a `<button>` wires `ref` automatically.
export interface DropdownTriggerRenderProps {
  ref: Ref<HTMLButtonElement>;
  onClick: (e: ReactMouseEvent<HTMLButtonElement>) => void;
  onKeyDown: (e: ReactKeyboardEvent<HTMLButtonElement>) => void;
  "aria-haspopup": "menu";
  "aria-expanded": boolean;
  "aria-controls"?: string;
}

interface DropdownProps {
  /// Render-prop trigger. The render fn must return a real `<button>`
  /// (or component that forwards props onto a `<button>`) so native
  /// keyboard focus, Enter/Space activation, and `:focus-visible` work
  /// without re-implementation. Spread `{...props}` onto it.
  trigger: (props: DropdownTriggerRenderProps) => ReactNode;
  /// Accessible label for the menu (`aria-label` on `[role="menu"]`).
  /// Helpful when the trigger is icon-only.
  label: string;
  /// Horizontal alignment of the popover relative to the trigger.
  align?: "left" | "right";
  /// Vertical position. `bottom` opens downward; `top` opens upward.
  side?: "bottom" | "top";
  /// Extra classes applied to the popover container — useful when a
  /// caller needs a wider min-width or custom z-index.
  className?: string;
  /// Menu items + optional `<DropdownSeparator/>` rows.
  children: ReactNode;
}

/// Accessible dropdown menu primitive.
///
/// Audit §8 flagged four hand-rolled menu/tab patterns that lacked
/// ARIA roles and keyboard nav. This primitive owns the keyboard
/// contract so callers can focus on layout:
///
/// - Trigger: real `<button>` with `aria-haspopup="menu"` +
///   `aria-expanded` + `aria-controls`.
/// - Open: click, Enter, Space (native button), or ArrowDown/ArrowUp.
/// - Navigate: ArrowDown / ArrowUp (wrapping), Home, End.
/// - Activate: Enter, Space, click on a `[role="menuitem"]`.
/// - Close: Escape (returns focus to trigger), Tab (closes without
///   stealing focus so the natural Tab order continues), outside
///   click, or selecting an item.
///
/// **Decision: hand-rolled, not `@radix-ui/react-dropdown-menu` or
/// `react-aria`.** Both add 30–50 KB gz to the bundle for behaviour
/// the project can implement in ~150 LoC; this matches the lean-deps
/// stance taken for valibot (over zod) and the in-house `Modal` /
/// `Switch` primitives. If we later want a context menu, sub-menus,
/// or virtualised long lists we can revisit.
export function Dropdown({
  trigger,
  label,
  align = "left",
  side = "bottom",
  className = "",
  children,
}: DropdownProps) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();
  // Tracks whether the menu was opened with the keyboard. When true we
  // auto-focus the first item on open; when false (mouse click) we
  // leave focus alone so the open menu doesn't steal the click target.
  const openedByKeyboard = useRef(false);

  const getItems = useCallback((): HTMLElement[] => {
    const root = menuRef.current;
    if (!root) return [];
    return Array.from(
      root.querySelectorAll<HTMLElement>(
        '[role="menuitem"]:not([data-disabled="true"])',
      ),
    );
  }, []);

  // Auto-focus the first item only when keyboard-opened.
  useEffect(() => {
    if (!open) return;
    if (!openedByKeyboard.current) return;
    const items = getItems();
    items[0]?.focus();
  }, [open, getItems]);

  // Outside click closes the menu (no focus return — the click landed
  // somewhere intentional).
  useEffect(() => {
    if (!open) return;
    function onMouseDown(e: MouseEvent) {
      const target = e.target as Node;
      if (menuRef.current?.contains(target)) return;
      if (triggerRef.current?.contains(target)) return;
      setOpen(false);
    }
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [open]);

  const closeAndRestore = useCallback(() => {
    setOpen(false);
    triggerRef.current?.focus();
  }, []);

  const handleMenuKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      const items = getItems();
      if (items.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          closeAndRestore();
        }
        return;
      }
      const active = document.activeElement as HTMLElement | null;
      const current = items.findIndex((el) => el === active);
      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          const next = current < 0 ? 0 : (current + 1) % items.length;
          items[next]?.focus();
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          const next =
            current < 0
              ? items.length - 1
              : (current - 1 + items.length) % items.length;
          items[next]?.focus();
          break;
        }
        case "Home": {
          e.preventDefault();
          items[0]?.focus();
          break;
        }
        case "End": {
          e.preventDefault();
          items[items.length - 1]?.focus();
          break;
        }
        case "Escape": {
          e.preventDefault();
          closeAndRestore();
          break;
        }
        case "Tab": {
          // Let the browser advance focus naturally; just close the menu
          // so the next Tab leaves it instead of cycling inside.
          setOpen(false);
          break;
        }
      }
    },
    [getItems, closeAndRestore],
  );

  const triggerProps: DropdownTriggerRenderProps = {
    ref: triggerRef,
    "aria-haspopup": "menu",
    "aria-expanded": open,
    "aria-controls": open ? menuId : undefined,
    onClick: () => {
      openedByKeyboard.current = false;
      setOpen((o) => !o);
    },
    onKeyDown: (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        openedByKeyboard.current = true;
        setOpen(true);
      } else if (e.key === "Enter" || e.key === " ") {
        // Native button click() will follow; flag keyboard so the
        // newly-opened menu auto-focuses its first item.
        openedByKeyboard.current = true;
      }
    },
  };

  const positionClass = side === "top" ? "bottom-full mb-1" : "top-full mt-1";
  const alignClass = align === "right" ? "right-0" : "left-0";

  return (
    <div className="relative inline-block">
      {trigger(triggerProps)}
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label={label}
          tabIndex={-1}
          onKeyDown={handleMenuKeyDown}
          className={`absolute z-20 ${positionClass} ${alignClass} bg-gray-800 border border-gray-700 rounded-md shadow-lg py-1 min-w-[140px] outline-none ${className}`}
        >
          {children}
        </div>
      )}
    </div>
  );
}

interface DropdownItemProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onSelect" | "role"> {
  /// Called when the item is activated (click or Enter/Space). The
  /// containing `Dropdown` does not auto-close — call the consumer's
  /// close hook (or just rely on the next state change to unmount the
  /// menu) inside `onSelect` if needed.
  onSelect: () => void;
  /// Visual variant. `danger` styles destructive actions (Reset, Delete).
  variant?: "default" | "danger";
  children: ReactNode;
}

export function DropdownItem({
  onSelect,
  variant = "default",
  disabled,
  className = "",
  children,
  ...rest
}: DropdownItemProps) {
  const variantClass =
    variant === "danger"
      ? "text-red-400/80 hover:bg-red-950/40 hover:text-red-300 focus:bg-red-950/40 focus:text-red-300"
      : "text-gray-300 hover:bg-gray-700 hover:text-white focus:bg-gray-700 focus:text-white";
  return (
    <button
      type="button"
      role="menuitem"
      tabIndex={-1}
      data-disabled={disabled ? "true" : undefined}
      disabled={disabled}
      onClick={() => {
        if (disabled) return;
        onSelect();
      }}
      className={`w-full text-left px-3 py-1 text-xs flex items-center gap-2 outline-none disabled:opacity-50 ${variantClass} ${className}`}
      {...rest}
    >
      {children}
    </button>
  );
}

export function DropdownSeparator() {
  return (
    <div role="separator" aria-orientation="horizontal" className="border-t border-gray-700 my-1" />
  );
}
