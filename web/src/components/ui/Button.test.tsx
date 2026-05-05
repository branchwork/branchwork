import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Button } from "./Button.js";

afterEach(cleanup);

describe("Button", () => {
  it("renders the primary variant by default", () => {
    render(<Button>Save</Button>);
    const btn = screen.getByRole("button", { name: "Save" });
    // bg-indigo-600 is the BRAND.solidBg token — fail-loudly if a future
    // refactor accidentally drops it.
    expect(btn.className).toContain("bg-indigo-600");
    expect(btn.getAttribute("type")).toBe("button");
  });

  it("applies the danger variant palette", () => {
    render(<Button variant="danger">Delete</Button>);
    const btn = screen.getByRole("button", { name: "Delete" });
    expect(btn.className).toContain("bg-red-700");
  });

  it("disables the button and sets aria-busy in the loading state", () => {
    render(<Button loading>Save</Button>);
    const btn = screen.getByRole("button", { name: /Save/i }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.getAttribute("aria-busy")).toBe("true");
  });

  it("forwards onClick when enabled", () => {
    let clicks = 0;
    render(
      <Button
        onClick={() => {
          clicks += 1;
        }}
      >
        Hi
      </Button>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Hi" }));
    expect(clicks).toBe(1);
  });

  it("does not fire onClick when disabled", () => {
    let clicks = 0;
    render(
      <Button
        disabled
        onClick={() => {
          clicks += 1;
        }}
      >
        Hi
      </Button>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Hi" }));
    expect(clicks).toBe(0);
  });

  it("does not set aria-busy when not loading", () => {
    render(<Button>Save</Button>);
    const btn = screen.getByRole("button", { name: "Save" });
    expect(btn.getAttribute("aria-busy")).toBeNull();
  });
});
