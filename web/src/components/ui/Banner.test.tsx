import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { Banner } from "./Banner.js";

afterEach(cleanup);

describe("Banner", () => {
  it("renders the error variant by default with role=alert", () => {
    render(<Banner>Something broke</Banner>);
    const banner = screen.getByRole("alert");
    expect(banner.textContent).toBe("Something broke");
    // DANGER.softBorder token — fail-loudly if a future refactor drops it.
    expect(banner.className).toContain("border-red-700/50");
  });

  it("applies the warn palette when kind=warn", () => {
    render(<Banner kind="warn">Heads up</Banner>);
    const banner = screen.getByRole("alert");
    expect(banner.className).toContain("border-amber-700/50");
  });

  it("forwards className for layout overrides", () => {
    render(<Banner className="mt-4">Hi</Banner>);
    const banner = screen.getByRole("alert");
    expect(banner.className).toContain("mt-4");
  });
});
