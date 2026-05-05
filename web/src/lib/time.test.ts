import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatAbsolute,
  formatRelative,
  formatShortDate,
  formatTimestamp,
} from "./time.js";

const FIXED_NOW = new Date("2026-04-12T12:00:00Z").getTime();

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(FIXED_NOW);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("formatRelative", () => {
  it("returns 'just now' for sub-minute deltas", () => {
    expect(formatRelative(new Date(FIXED_NOW - 30_000))).toBe("just now");
  });

  it("returns minutes for sub-hour deltas", () => {
    expect(formatRelative(new Date(FIXED_NOW - 5 * 60_000))).toBe("5m ago");
  });

  it("returns hours for sub-day deltas", () => {
    expect(formatRelative(new Date(FIXED_NOW - 3 * 60 * 60_000))).toBe("3h ago");
  });

  it("returns days under 30", () => {
    expect(formatRelative(new Date(FIXED_NOW - 5 * 24 * 60 * 60_000))).toBe(
      "5d ago",
    );
  });

  it("falls back to short date past 30 days", () => {
    const out = formatRelative(new Date(FIXED_NOW - 60 * 24 * 60 * 60_000));
    // Locale-dependent but must contain a digit (day) and not look relative
    expect(out).not.toMatch(/ago/);
    expect(out).toMatch(/\d/);
  });

  it("normalizes naive UTC strings (no Z)", () => {
    const naive = "2026-04-12 11:55:00"; // 5 minutes before FIXED_NOW
    expect(formatRelative(naive)).toBe("5m ago");
  });

  it("returns empty string for falsy input", () => {
    expect(formatRelative("")).toBe("");
  });
});

describe("formatTimestamp", () => {
  it("matches formatRelative for the first 7 days", () => {
    expect(formatTimestamp(new Date(FIXED_NOW - 6 * 24 * 60 * 60_000))).toBe(
      "6d ago",
    );
  });

  it("falls back to absolute (date + time) past 7 days", () => {
    const out = formatTimestamp(new Date(FIXED_NOW - 30 * 24 * 60 * 60_000));
    expect(out).not.toMatch(/ago/);
    expect(out).toMatch(/\d/);
  });
});

describe("locale honours navigator.language", () => {
  // Acceptance criterion: date formatting uses navigator.language at runtime
  // instead of a hardcoded "en-US". We spy on Intl.DateTimeFormat and assert
  // it receives the navigator-reported locale.
  it("forwards navigator.language to Intl.DateTimeFormat in formatShortDate", () => {
    const spy = vi.spyOn(Intl, "DateTimeFormat");
    const navStub = vi
      .spyOn(navigator, "language", "get")
      .mockReturnValue("fr-FR");

    formatShortDate(new Date(FIXED_NOW));

    expect(navStub).toHaveBeenCalled();
    expect(spy).toHaveBeenCalled();
    const firstCallArgs = spy.mock.calls[0];
    expect(firstCallArgs[0]).toBe("fr-FR");
  });

  it("forwards navigator.language to Intl.DateTimeFormat in formatAbsolute", () => {
    const spy = vi.spyOn(Intl, "DateTimeFormat");
    vi.spyOn(navigator, "language", "get").mockReturnValue("de-DE");

    formatAbsolute(new Date(FIXED_NOW));

    const args = spy.mock.calls[0];
    expect(args[0]).toBe("de-DE");
    expect(args[1]).toMatchObject({ hour: "2-digit", minute: "2-digit" });
  });

  it("formatRelative routes through formatShortDate (locale-aware) past 30d", () => {
    const spy = vi.spyOn(Intl, "DateTimeFormat");
    vi.spyOn(navigator, "language", "get").mockReturnValue("ja-JP");

    formatRelative(new Date(FIXED_NOW - 60 * 24 * 60 * 60_000));

    expect(spy).toHaveBeenCalled();
    expect(spy.mock.calls[0][0]).toBe("ja-JP");
  });
});

describe("formatShortDate / formatAbsolute pure outputs", () => {
  it("formatShortDate omits year and time", () => {
    vi.spyOn(navigator, "language", "get").mockReturnValue("en-US");
    const out = formatShortDate(new Date("2026-04-05T00:00:00Z"));
    expect(out).toMatch(/Apr/);
    expect(out).not.toMatch(/2026/);
    expect(out).not.toMatch(/:/);
  });

  it("formatAbsolute includes time", () => {
    vi.spyOn(navigator, "language", "get").mockReturnValue("en-US");
    const out = formatAbsolute(new Date("2026-04-05T14:30:00Z"));
    expect(out).toMatch(/:/); // hour:minute separator
  });
});
