import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import axe from "axe-core";
import { Tabs } from "./Tabs.js";

afterEach(cleanup);

type V = "one" | "two" | "three";

function Harness({ disabledTwo = false }: { disabledTwo?: boolean }) {
  const [v, setV] = useState<V>("one");
  return (
    <div>
      <Tabs<V>
        label="Demo tabs"
        value={v}
        onChange={setV}
        tabs={[
          { value: "one", label: "One" },
          { value: "two", label: "Two", disabled: disabledTwo },
          { value: "three", label: "Three" },
        ]}
      />
      <div data-testid="active">{v}</div>
    </div>
  );
}

describe("Tabs", () => {
  it("renders with role=tablist and aria-label", () => {
    render(<Harness />);
    const list = screen.getByRole("tablist");
    expect(list.getAttribute("aria-label")).toBe("Demo tabs");
  });

  it("each tab has role=tab and aria-selected", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs.length).toBe(3);
    expect(tabs[0].getAttribute("aria-selected")).toBe("true");
    expect(tabs[1].getAttribute("aria-selected")).toBe("false");
    expect(tabs[2].getAttribute("aria-selected")).toBe("false");
  });

  it("uses roving tabindex (selected=0, others=-1)", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs[0].getAttribute("tabindex")).toBe("0");
    expect(tabs[1].getAttribute("tabindex")).toBe("-1");
    expect(tabs[2].getAttribute("tabindex")).toBe("-1");
  });

  it("ArrowRight selects the next tab and moves focus", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    tabs[0].focus();
    fireEvent.keyDown(tabs[0], { key: "ArrowRight" });
    expect(document.activeElement).toBe(tabs[1]);
    expect(tabs[1].getAttribute("aria-selected")).toBe("true");
    expect(screen.getByTestId("active").textContent).toBe("two");
  });

  it("ArrowLeft selects the previous tab; wraps from first to last", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    tabs[0].focus();
    fireEvent.keyDown(tabs[0], { key: "ArrowLeft" });
    expect(document.activeElement).toBe(tabs[2]);
    expect(screen.getByTestId("active").textContent).toBe("three");
  });

  it("Home jumps to first; End jumps to last", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    tabs[0].focus();
    fireEvent.keyDown(tabs[0], { key: "End" });
    expect(document.activeElement).toBe(tabs[2]);
    expect(screen.getByTestId("active").textContent).toBe("three");
    fireEvent.keyDown(tabs[2], { key: "Home" });
    expect(document.activeElement).toBe(tabs[0]);
    expect(screen.getByTestId("active").textContent).toBe("one");
  });

  it("ArrowDown / ArrowUp also navigate (matches WAI-ARIA)", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    tabs[0].focus();
    fireEvent.keyDown(tabs[0], { key: "ArrowDown" });
    expect(document.activeElement).toBe(tabs[1]);
    fireEvent.keyDown(tabs[1], { key: "ArrowUp" });
    expect(document.activeElement).toBe(tabs[0]);
  });

  it("disabled tabs are skipped during arrow navigation", () => {
    render(<Harness disabledTwo />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs[1].hasAttribute("disabled")).toBe(true);
    tabs[0].focus();
    fireEvent.keyDown(tabs[0], { key: "ArrowRight" });
    expect(document.activeElement).toBe(tabs[2]);
    expect(screen.getByTestId("active").textContent).toBe("three");
  });

  it("clicking a tab selects it", () => {
    render(<Harness />);
    const tabs = screen.getAllByRole("tab");
    fireEvent.click(tabs[2]);
    expect(screen.getByTestId("active").textContent).toBe("three");
    expect(tabs[2].getAttribute("aria-selected")).toBe("true");
  });

  it("clicking a disabled tab is a no-op", () => {
    render(<Harness disabledTwo />);
    const tabs = screen.getAllByRole("tab");
    fireEvent.click(tabs[1]);
    expect(screen.getByTestId("active").textContent).toBe("one");
  });

  it("axe-core reports zero structural a11y violations", async () => {
    const { container } = render(<Harness />);
    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
