import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { TouchTarget } from "./TouchTarget.js";

afterEach(cleanup);

describe("TouchTarget", () => {
  it("renders the child element with merged hit-region classes (36px default)", () => {
    render(
      <TouchTarget>
        <button className="text-xs px-1 py-0.5">x</button>
      </TouchTarget>,
    );
    const btn = screen.getByRole("button", { name: "x" });
    // Existing classes survive.
    expect(btn.className).toContain("text-xs");
    expect(btn.className).toContain("px-1");
    // Pseudo-element scaffolding is on the child itself.
    expect(btn.className).toContain("relative");
    expect(btn.className).toContain("before:content-['']");
    expect(btn.className).toContain("before:absolute");
    // Centred via top/left + translate.
    expect(btn.className).toContain("before:left-1/2");
    expect(btn.className).toContain("before:top-1/2");
    expect(btn.className).toContain("before:-translate-x-1/2");
    expect(btn.className).toContain("before:-translate-y-1/2");
    // Sized to max(parent, 36px) via w-full + min-w-9.
    expect(btn.className).toContain("before:w-full");
    expect(btn.className).toContain("before:h-full");
    expect(btn.className).toContain("before:min-w-9");
    expect(btn.className).toContain("before:min-h-9");
    // Behind siblings + collapsed on md+.
    expect(btn.className).toContain("before:-z-10");
    expect(btn.className).toContain("md:before:hidden");
  });

  it("uses the 44px minimum when min='44px'", () => {
    render(
      <TouchTarget min="44px">
        <button>x</button>
      </TouchTarget>,
    );
    const btn = screen.getByRole("button", { name: "x" });
    expect(btn.className).toContain("before:min-w-11");
    expect(btn.className).toContain("before:min-h-11");
    // 36px classes must NOT bleed in.
    expect(btn.className).not.toContain("before:min-w-9");
    expect(btn.className).not.toContain("before:min-h-9");
  });

  it("preserves the child's onClick", () => {
    const onClick = vi.fn();
    render(
      <TouchTarget>
        <button onClick={onClick}>x</button>
      </TouchTarget>,
    );
    fireEvent.click(screen.getByRole("button", { name: "x" }));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("works on a <select> child (driver picker callsite)", () => {
    render(
      <TouchTarget>
        <select aria-label="Driver" defaultValue="claude">
          <option value="claude">claude</option>
          <option value="aider">aider</option>
        </select>
      </TouchTarget>,
    );
    const sel = screen.getByLabelText("Driver");
    expect(sel.tagName).toBe("SELECT");
    expect(sel.className).toContain("relative");
    expect(sel.className).toContain("before:content-['']");
    expect(sel.className).toContain("md:before:hidden");
  });

  it("merges into an empty/missing className without leading whitespace", () => {
    render(
      <TouchTarget>
        <button>x</button>
      </TouchTarget>,
    );
    const btn = screen.getByRole("button", { name: "x" });
    // No leading or trailing whitespace.
    expect(btn.className).toBe(btn.className.trim());
    // Starts with the pseudo-element scaffolding (no orphan space).
    expect(btn.className.startsWith("relative")).toBe(true);
  });

  it("returns the child unchanged when className is a function (NavLink-style)", () => {
    // NavLink passes a render function — cloning would replace the
    // function with a string and break the active-state predicate.
    const fn = () => "active-classes";
    render(
      <TouchTarget>
        {/* @ts-expect-error -- forcing function className to test the bail. */}
        <button className={fn}>x</button>
      </TouchTarget>,
    );
    const btn = screen.getByRole("button", { name: "x" });
    // The button got the raw function — JSX/JSDOM renders it as the
    // function's string repr, but crucially TouchTarget did NOT
    // overwrite it with a merged string.
    expect(btn.className).not.toContain("before:");
    expect(btn.className).not.toContain("relative");
  });
});
