import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Switch } from "./Switch.js";

afterEach(cleanup);

describe("Switch", () => {
  it("renders with role=switch and aria-checked=false", () => {
    render(<Switch label="Auto-mode" checked={false} onChange={() => {}} />);
    const sw = screen.getByRole("switch", { name: /Auto-mode/ });
    expect(sw.getAttribute("aria-checked")).toBe("false");
  });

  it("reflects checked=true via aria-checked", () => {
    render(<Switch label="Auto-mode" checked onChange={() => {}} />);
    const sw = screen.getByRole("switch", { name: /Auto-mode/ });
    expect(sw.getAttribute("aria-checked")).toBe("true");
  });

  it("calls onChange with the negated value on click", () => {
    let next: boolean | null = null;
    render(
      <Switch
        label="x"
        checked={false}
        onChange={(v) => {
          next = v;
        }}
      />,
    );
    fireEvent.click(screen.getByRole("switch", { name: "x" }));
    expect(next).toBe(true);
  });

  it("does not fire onChange when disabled", () => {
    let calls = 0;
    render(
      <Switch
        label="x"
        checked={false}
        disabled
        onChange={() => {
          calls += 1;
        }}
      />,
    );
    fireEvent.click(screen.getByRole("switch", { name: "x" }));
    expect(calls).toBe(0);
  });
});
