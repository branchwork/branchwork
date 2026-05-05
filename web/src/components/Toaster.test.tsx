import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Toaster } from "./Toaster.js";
import { useToastStore } from "../stores/toast-store.js";

beforeEach(() => {
  useToastStore.getState().clear();
});

afterEach(cleanup);

describe("Toaster", () => {
  it("renders nothing when the queue is empty", () => {
    const { container } = render(<Toaster />);
    expect(container.querySelector("[data-testid=toaster]")).toBeNull();
  });

  it("renders a pushed toast title and body", () => {
    render(<Toaster />);
    act(() => {
      useToastStore
        .getState()
        .push({ kind: "info", title: "Hello", body: "world" });
    });
    expect(screen.getByText("Hello")).toBeTruthy();
    expect(screen.getByText("world")).toBeTruthy();
  });

  it("uses role=alert for error toasts", () => {
    render(<Toaster />);
    act(() => {
      useToastStore.getState().push({ kind: "error", title: "Boom" });
    });
    const alerts = screen.getAllByRole("alert");
    expect(alerts).toHaveLength(1);
    expect(alerts[0].textContent).toContain("Boom");
  });

  it("uses role=status for info/success toasts", () => {
    render(<Toaster />);
    act(() => {
      useToastStore.getState().push({ kind: "info", title: "FYI" });
      useToastStore.getState().push({ kind: "success", title: "Done" });
    });
    const statuses = screen.getAllByRole("status");
    expect(statuses).toHaveLength(2);
  });

  it("dismiss button removes the toast", () => {
    render(<Toaster />);
    act(() => {
      useToastStore.getState().push({ kind: "error", title: "Pinned" });
    });
    expect(screen.getByText("Pinned")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss notification" }));
    expect(screen.queryByText("Pinned")).toBeNull();
  });
});
