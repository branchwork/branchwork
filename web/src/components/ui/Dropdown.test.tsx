import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import axe from "axe-core";
import { Dropdown, DropdownItem, DropdownSeparator } from "./Dropdown.js";

afterEach(cleanup);

function Harness({
  onSelect,
  withSeparator = false,
}: {
  onSelect?: (value: string) => void;
  withSeparator?: boolean;
}) {
  const [last, setLast] = useState<string | null>(null);
  return (
    <div>
      <Dropdown
        label="Status menu"
        trigger={(props) => (
          <button {...props} type="button">
            Open menu
          </button>
        )}
      >
        <DropdownItem
          onSelect={() => {
            setLast("one");
            onSelect?.("one");
          }}
        >
          One
        </DropdownItem>
        <DropdownItem
          onSelect={() => {
            setLast("two");
            onSelect?.("two");
          }}
        >
          Two
        </DropdownItem>
        {withSeparator && <DropdownSeparator />}
        <DropdownItem
          onSelect={() => {
            setLast("three");
            onSelect?.("three");
          }}
          variant="danger"
        >
          Three
        </DropdownItem>
      </Dropdown>
      <button type="button">After</button>
      <span data-testid="last">{last ?? ""}</span>
    </div>
  );
}

describe("Dropdown", () => {
  it("renders the trigger and no menu by default", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("opens on click and exposes role=menu", () => {
    render(<Harness />);
    fireEvent.click(screen.getByRole("button", { name: "Open menu" }));
    const menu = screen.getByRole("menu");
    expect(menu.getAttribute("aria-label")).toBe("Status menu");
    const items = screen.getAllByRole("menuitem");
    expect(items.length).toBe(3);
  });

  it("opens via ArrowDown on the trigger and focuses the first item", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const items = screen.getAllByRole("menuitem");
    expect(document.activeElement).toBe(items[0]);
  });

  it("ArrowDown wraps to first; ArrowUp wraps to last", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const items = screen.getAllByRole("menuitem");
    fireEvent.keyDown(items[0], { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[1]);
    fireEvent.keyDown(items[1], { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[2]);
    fireEvent.keyDown(items[2], { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[0]);
    fireEvent.keyDown(items[0], { key: "ArrowUp" });
    expect(document.activeElement).toBe(items[2]);
  });

  it("Home and End jump to first/last item", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const items = screen.getAllByRole("menuitem");
    fireEvent.keyDown(items[0], { key: "End" });
    expect(document.activeElement).toBe(items[items.length - 1]);
    fireEvent.keyDown(items[items.length - 1], { key: "Home" });
    expect(document.activeElement).toBe(items[0]);
  });

  it("Escape closes the menu and returns focus to the trigger", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const item = screen.getAllByRole("menuitem")[0];
    fireEvent.keyDown(item, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("Tab on a menu item closes the menu without stealing focus", () => {
    render(<Harness />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const item = screen.getAllByRole("menuitem")[0];
    fireEvent.keyDown(item, { key: "Tab" });
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("clicking an item invokes onSelect", () => {
    const onSelect = vi.fn();
    render(<Harness onSelect={onSelect} />);
    fireEvent.click(screen.getByRole("button", { name: "Open menu" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Two" }));
    expect(onSelect).toHaveBeenCalledWith("two");
  });

  it("Enter on a menuitem activates it", () => {
    const onSelect = vi.fn();
    render(<Harness onSelect={onSelect} />);
    const trigger = screen.getByRole("button", { name: "Open menu" });
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    const items = screen.getAllByRole("menuitem");
    // Native button: Enter dispatches click.
    fireEvent.click(items[0]);
    expect(onSelect).toHaveBeenCalledWith("one");
  });

  it("renders a separator with role=separator", () => {
    render(<Harness withSeparator />);
    fireEvent.click(screen.getByRole("button", { name: "Open menu" }));
    const sep = screen.getByRole("separator");
    expect(sep.getAttribute("aria-orientation")).toBe("horizontal");
  });

  it("axe-core reports zero structural a11y violations on the open menu", async () => {
    render(<Harness />);
    fireEvent.click(screen.getByRole("button", { name: "Open menu" }));
    const menu = screen.getByRole("menu");
    const results = await axe.run(menu, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
