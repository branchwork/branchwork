import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import axe from "axe-core";
import { Modal } from "./Modal.js";

afterEach(cleanup);

describe("Modal", () => {
  it("renders nothing when closed", () => {
    const { container } = render(
      <Modal open={false} onClose={() => {}} title="t">
        body
      </Modal>,
    );
    expect(container.innerHTML).toBe("");
  });

  it("renders with role=dialog and aria-modal=true", () => {
    render(
      <Modal open onClose={() => {}} title="Confirm">
        body
      </Modal>,
    );
    const dlg = screen.getByRole("dialog");
    expect(dlg.getAttribute("aria-modal")).toBe("true");
  });

  it("labels the dialog by the title element", () => {
    render(
      <Modal open onClose={() => {}} title="Confirm delete">
        body
      </Modal>,
    );
    const dlg = screen.getByRole("dialog");
    const labelId = dlg.getAttribute("aria-labelledby");
    expect(labelId).toBeTruthy();
    const labelEl = document.getElementById(labelId as string);
    expect(labelEl?.textContent).toBe("Confirm delete");
  });

  it("wires aria-describedby when description is provided", () => {
    render(
      <Modal open onClose={() => {}} title="t" description="why this matters">
        body
      </Modal>,
    );
    const dlg = screen.getByRole("dialog");
    const descId = dlg.getAttribute("aria-describedby");
    expect(descId).toBeTruthy();
    const descEl = document.getElementById(descId as string);
    expect(descEl?.textContent).toBe("why this matters");
  });

  it("invokes onClose on Escape", () => {
    let calls = 0;
    render(
      <Modal
        open
        onClose={() => {
          calls += 1;
        }}
        title="t"
      >
        body
      </Modal>,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(calls).toBe(1);
  });

  it("does not close on backdrop click by default", () => {
    let calls = 0;
    render(
      <Modal
        open
        onClose={() => {
          calls += 1;
        }}
        title="t"
      >
        body
      </Modal>,
    );
    const backdrop = screen.getByRole("dialog").parentElement as HTMLElement;
    fireEvent.click(backdrop);
    expect(calls).toBe(0);
  });

  it("closes on backdrop click when opted in", () => {
    let calls = 0;
    render(
      <Modal
        open
        onClose={() => {
          calls += 1;
        }}
        title="t"
        closeOnBackdropClick
      >
        body
      </Modal>,
    );
    const backdrop = screen.getByRole("dialog").parentElement as HTMLElement;
    fireEvent.click(backdrop);
    expect(calls).toBe(1);
  });

  it("axe-core reports zero structural a11y violations on the rendered dialog", async () => {
    render(
      <Modal
        open
        onClose={() => {}}
        title="Confirm delete"
        description="This action cannot be undone."
      >
        <div className="mt-4 flex justify-end gap-2">
          <button type="button">Cancel</button>
          <button type="button">Confirm</button>
        </div>
      </Modal>,
    );
    const dialog = screen.getByRole("dialog");
    const results = await axe.run(dialog, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
