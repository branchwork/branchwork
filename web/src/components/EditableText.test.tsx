import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import axe from "axe-core";
import { EditableText } from "./EditableText.js";

afterEach(cleanup);

describe("EditableText (display state)", () => {
  it("renders as a button with `Edit ${label}` accessible name", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    const trigger = screen.getByRole("button", { name: "Edit plan title" });
    expect(trigger.tagName).toBe("BUTTON");
    expect(trigger.textContent).toBe("Hello");
  });

  it("shows placeholder when value is empty", () => {
    render(
      <EditableText value="" onSave={() => {}} label="plan title" placeholder="Add title..." />,
    );
    expect(screen.getByText("Add title...")).toBeTruthy();
  });

  it("clicking the button enters edit mode and reveals an input", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    expect(input.tagName).toBe("INPUT");
  });

  it("focuses the input on entering edit mode", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    expect(document.activeElement).toBe(input);
  });
});

describe("EditableText (edit state, single-line)", () => {
  it("Enter on the input submits the form and calls onSave", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.change(input, { target: { value: "Updated" } });
    // The single-line input is wrapped in a <form>; native Enter submits
    // through form.onSubmit. Simulate via fireEvent.submit on the form.
    const form = input.closest("form") as HTMLFormElement;
    fireEvent.submit(form);
    expect(onSave).toHaveBeenCalledWith("Updated");
  });

  it("trims whitespace before calling onSave", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.change(input, { target: { value: "  spaced  " } });
    fireEvent.submit(input.closest("form") as HTMLFormElement);
    expect(onSave).toHaveBeenCalledWith("spaced");
  });

  it("does not call onSave when the value is unchanged", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.submit(input.closest("form") as HTMLFormElement);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("Esc cancels without calling onSave", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.change(input, { target: { value: "Updated" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(onSave).not.toHaveBeenCalled();
    // Edit mode closed; trigger is back.
    expect(screen.getByRole("button", { name: "Edit plan title" })).toBeTruthy();
  });

  it("returns focus to the trigger after submit", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.change(input, { target: { value: "Updated" } });
    fireEvent.submit(input.closest("form") as HTMLFormElement);
    const trigger = screen.getByRole("button", { name: "Edit plan title" });
    expect(document.activeElement).toBe(trigger);
  });

  it("returns focus to the trigger after Esc cancel", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.keyDown(input, { key: "Escape" });
    const trigger = screen.getByRole("button", { name: "Edit plan title" });
    expect(document.activeElement).toBe(trigger);
  });

  it("blur (click-outside) saves the draft", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const input = screen.getByRole("textbox", { name: "plan title" });
    fireEvent.change(input, { target: { value: "Updated" } });
    fireEvent.blur(input);
    expect(onSave).toHaveBeenCalledWith("Updated");
  });
});

describe("EditableText (edit state, multiline)", () => {
  it("renders a textarea when multiline is true", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan context" multiline />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan context" }));
    const ta = screen.getByRole("textbox", { name: "plan context" });
    expect(ta.tagName).toBe("TEXTAREA");
  });

  it("plain Enter inside textarea does NOT submit (default newline)", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan context" multiline />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan context" }));
    const ta = screen.getByRole("textbox", { name: "plan context" });
    fireEvent.change(ta, { target: { value: "Line 1" } });
    fireEvent.keyDown(ta, { key: "Enter" });
    // Still in edit mode; onSave not called.
    expect(onSave).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox", { name: "plan context" })).not.toBeNull();
  });

  it("Cmd/Ctrl+Enter submits multiline", () => {
    const onSave = vi.fn();
    render(<EditableText value="Hello" onSave={onSave} label="plan context" multiline />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan context" }));
    const ta = screen.getByRole("textbox", { name: "plan context" });
    fireEvent.change(ta, { target: { value: "Updated" } });
    fireEvent.keyDown(ta, { key: "Enter", metaKey: true });
    expect(onSave).toHaveBeenCalledWith("Updated");
  });
});

describe("EditableText accessibility", () => {
  it("axe-core finds no violations on the trigger button", async () => {
    const { container } = render(
      <EditableText value="Hello" onSave={() => {}} label="plan title" />,
    );
    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });

  it("axe-core finds no violations on the edit form", async () => {
    const { container } = render(
      <EditableText value="Hello" onSave={() => {}} label="plan title" />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });

  it("the edit form exposes its accessible name", () => {
    render(<EditableText value="Hello" onSave={() => {}} label="plan title" />);
    fireEvent.click(screen.getByRole("button", { name: "Edit plan title" }));
    const form = screen
      .getByRole("textbox", { name: "plan title" })
      .closest("form") as HTMLFormElement;
    expect(form.getAttribute("aria-label")).toBe("Edit plan title");
  });
});
