import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useToastStore, TOAST_CAP, toastRole } from "./toast-store.js";
import { toastError } from "../lib/toast.js";

beforeEach(() => {
  useToastStore.getState().clear();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("toast-store", () => {
  it("push appends a toast and returns the assigned id", () => {
    const id = useToastStore.getState().push({ kind: "info", title: "Hello" });
    const toasts = useToastStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].id).toBe(id);
    expect(toasts[0].kind).toBe("info");
    expect(toasts[0].title).toBe("Hello");
  });

  it("dismiss removes the toast by id", () => {
    const a = useToastStore.getState().push({ kind: "info", title: "A" });
    useToastStore.getState().push({ kind: "success", title: "B" });
    useToastStore.getState().dismiss(a);
    const titles = useToastStore.getState().toasts.map((t) => t.title);
    expect(titles).toEqual(["B"]);
  });

  it("clear empties the queue", () => {
    useToastStore.getState().push({ kind: "info", title: "A" });
    useToastStore.getState().push({ kind: "info", title: "B" });
    useToastStore.getState().clear();
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("info/success default to a 6s TTL and auto-dismiss", () => {
    useToastStore.getState().push({ kind: "info", title: "A" });
    useToastStore.getState().push({ kind: "success", title: "B" });
    expect(useToastStore.getState().toasts).toHaveLength(2);
    vi.advanceTimersByTime(5999);
    expect(useToastStore.getState().toasts).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("error toasts default to sticky (no TTL) — must be dismissed manually", () => {
    const id = useToastStore.getState().push({ kind: "error", title: "Boom" });
    vi.advanceTimersByTime(60_000);
    expect(useToastStore.getState().toasts).toHaveLength(1);
    useToastStore.getState().dismiss(id);
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("explicit ttlMs overrides the default for error toasts", () => {
    useToastStore.getState().push({ kind: "error", title: "Transient", ttlMs: 1000 });
    expect(useToastStore.getState().toasts).toHaveLength(1);
    vi.advanceTimersByTime(1000);
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });

  it("caps queue at 5 and evicts the oldest when a 6th is pushed", () => {
    for (let i = 0; i < TOAST_CAP; i += 1) {
      useToastStore.getState().push({ kind: "info", title: `t${i}`, ttlMs: 0 });
    }
    expect(useToastStore.getState().toasts).toHaveLength(TOAST_CAP);
    useToastStore.getState().push({ kind: "info", title: "t5", ttlMs: 0 });
    const titles = useToastStore.getState().toasts.map((t) => t.title);
    expect(titles).toEqual(["t1", "t2", "t3", "t4", "t5"]);
  });

  it("ttlMs=0 is treated as sticky even on info/success", () => {
    useToastStore.getState().push({ kind: "info", title: "Pinned", ttlMs: 0 });
    vi.advanceTimersByTime(60_000);
    expect(useToastStore.getState().toasts).toHaveLength(1);
  });

  it("toastRole maps info/success → status, warn/error → alert", () => {
    expect(toastRole("info")).toBe("status");
    expect(toastRole("success")).toBe("status");
    expect(toastRole("warn")).toBe("alert");
    expect(toastRole("error")).toBe("alert");
  });
});

describe("toastError", () => {
  beforeEach(() => {
    useToastStore.getState().clear();
  });

  it("produces a kind:error toast titled with the Error message (no Error: prefix)", () => {
    toastError(new Error("x"));
    const toasts = useToastStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0].kind).toBe("error");
    expect(toasts[0].title).toBe("x");
  });

  it("falls back to String(e) for non-Error throwables", () => {
    toastError("nope");
    const t = useToastStore.getState().toasts[0];
    expect(t.kind).toBe("error");
    expect(t.title).toBe("nope");
  });

  it("prefixes the message when prefix is given", () => {
    toastError(new Error("denied"), "Save failed");
    const t = useToastStore.getState().toasts[0];
    expect(t.title).toBe("Save failed: denied");
  });

  it("returns the toast id so callers can dismiss programmatically", () => {
    const id = toastError(new Error("oops"));
    expect(id).toBe(useToastStore.getState().toasts[0].id);
    useToastStore.getState().dismiss(id);
    expect(useToastStore.getState().toasts).toHaveLength(0);
  });
});
