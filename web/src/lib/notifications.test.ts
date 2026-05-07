import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BATCH_DEBOUNCE_MS,
  NOTIFICATION_PREFS_STORAGE_KEY,
  _resetForTests,
  getNotificationPermission,
  getNotificationPrefs,
  isClassEnabled,
  notify,
  readPromptState,
  requestNotificationPermission,
  setClassEnabled,
} from "./notifications.js";

/// Map-backed localStorage stand-in — jsdom's about:blank origin
/// disables window.localStorage in some vitest 3 configs (cf.
/// org-store.test.ts:17). A small Map keeps the storage round-trip
/// deterministic without depending on jsdom's behaviour.
function makeMockStorage(): Storage {
  const map = new Map<string, string>();
  return {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => (map.has(k) ? (map.get(k) ?? null) : null),
    setItem: (k: string, v: string) => {
      map.set(k, String(v));
    },
    removeItem: (k: string) => {
      map.delete(k);
    },
    key: (i: number) => Array.from(map.keys())[i] ?? null,
  };
}

/// Stub for the window.Notification constructor + its `permission`
/// static + `requestPermission`. jsdom does not ship the Notification
/// API, so every test that exercises the firing path or the permission
/// gate has to install one. Using a class lets us record `new` calls
/// and the (title, options) arg pair via the `notifications` array.
class StubNotification {
  static permission: NotificationPermission = "default";
  static requestPermission = vi.fn(async (): Promise<NotificationPermission> => "granted");
  static notifications: { title: string; options: NotificationOptions | undefined }[] = [];

  constructor(title: string, options?: NotificationOptions) {
    StubNotification.notifications.push({ title, options });
  }
}

function installNotificationApi(initialPermission: NotificationPermission = "granted") {
  StubNotification.permission = initialPermission;
  StubNotification.requestPermission = vi.fn(
    async (): Promise<NotificationPermission> => "granted",
  );
  StubNotification.notifications = [];
  vi.stubGlobal("Notification", StubNotification as unknown as typeof Notification);
}

beforeEach(() => {
  vi.useFakeTimers();
  _resetForTests();
  vi.stubGlobal("localStorage", makeMockStorage());
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("getNotificationPermission", () => {
  it("returns 'unsupported' when the API is missing", () => {
    // No stubGlobal — jsdom default has no Notification.
    expect(getNotificationPermission()).toBe("unsupported");
  });

  it("returns the live Notification.permission value when the API exists", () => {
    installNotificationApi("granted");
    expect(getNotificationPermission()).toBe("granted");
    StubNotification.permission = "denied";
    expect(getNotificationPermission()).toBe("denied");
  });
});

describe("requestNotificationPermission", () => {
  it("resolves to 'unsupported' when the API is missing", async () => {
    expect(await requestNotificationPermission()).toBe("unsupported");
    expect(readPromptState()).toBe("never");
  });

  it("short-circuits and does not re-prompt when permission is already granted", async () => {
    installNotificationApi("granted");
    const result = await requestNotificationPermission();
    expect(result).toBe("granted");
    expect(StubNotification.requestPermission).not.toHaveBeenCalled();
    expect(readPromptState()).toBe("asked");
  });

  it("calls Notification.requestPermission when permission is 'default'", async () => {
    installNotificationApi("default");
    StubNotification.requestPermission = vi.fn(
      async (): Promise<NotificationPermission> => "granted",
    );
    const result = await requestNotificationPermission();
    expect(result).toBe("granted");
    expect(StubNotification.requestPermission).toHaveBeenCalledTimes(1);
    expect(readPromptState()).toBe("asked");
  });

  it("collapses a thrown promise to 'denied' so callers don't have to discriminate", async () => {
    installNotificationApi("default");
    StubNotification.requestPermission = vi.fn(async () => {
      throw new Error("user gesture required");
    });
    expect(await requestNotificationPermission()).toBe("denied");
  });
});

describe("notify — gating", () => {
  it("no-ops without firing or buffering when the API is missing", () => {
    notify("agent_stopped", "title", "body");
    vi.advanceTimersByTime(10_000);
    // Nothing to assert against (no API), but the call must not throw.
    expect(true).toBe(true);
  });

  it("no-ops when Notification.permission is not 'granted'", () => {
    installNotificationApi("default");
    notify("agent_stopped", "title", "body");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 100);
    expect(StubNotification.notifications).toHaveLength(0);
  });

  it("no-ops when the class is opted out", () => {
    installNotificationApi("granted");
    setClassEnabled("agent_stopped", false);
    notify("agent_stopped", "title", "body");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 100);
    expect(StubNotification.notifications).toHaveLength(0);
  });
});

describe("notify — single-event path", () => {
  it("fires a single notification after the debounce when only one event arrives", () => {
    installNotificationApi("granted");
    notify("agent_stopped", "Task 1.2 — completed", "Agent finished", "agent-X");
    expect(StubNotification.notifications).toHaveLength(0);
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(1);
    expect(StubNotification.notifications[0]).toEqual({
      title: "Task 1.2 — completed",
      options: { body: "Agent finished", tag: "agent-X", icon: "/favicon.ico" },
    });
  });

  it("fires two singles when two events arrive in the window (below threshold)", () => {
    installNotificationApi("granted");
    notify("agent_stopped", "A", "body-a");
    notify("agent_stopped", "B", "body-b");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(2);
    expect(StubNotification.notifications.map((n) => n.title)).toEqual(["A", "B"]);
  });
});

describe("notify — batching (acceptance criterion)", () => {
  it("collapses 5 agent_stopped events fired in 2s into exactly one aggregated notification", () => {
    installNotificationApi("granted");
    // Spread 5 events across ~1.4s with 350ms spacing — under the
    // 400ms debounce so the timer keeps resetting and every event
    // lands in the same buffer. Real-world bursts (auto-mode finishing
    // a phase that spawned 5 agents) tend to arrive even closer
    // together (~tens of ms apart), so this is the conservative case.
    for (let i = 0; i < 5; i++) {
      notify("agent_stopped", `Task ${i}`, "Agent finished", `agent-${i}`);
      if (i < 4) vi.advanceTimersByTime(350);
    }
    // Total wall-clock so far: 4 × 350ms = 1.4s. Flush.
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 100);

    expect(StubNotification.notifications).toHaveLength(1);
    expect(StubNotification.notifications[0].title).toBe("5 agents finished");
    expect(StubNotification.notifications[0].options?.tag).toBe("batch-agent_stopped");
  });

  it("aggregates exactly at the threshold of 3 events", () => {
    installNotificationApi("granted");
    notify("agent_stopped", "A", "");
    notify("agent_stopped", "B", "");
    notify("agent_stopped", "C", "");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(1);
    expect(StubNotification.notifications[0].title).toBe("3 agents finished");
  });

  it("batches per class — different classes do not interfere", () => {
    installNotificationApi("granted");
    notify("agent_stopped", "A", "");
    notify("agent_stopped", "B", "");
    notify("agent_stopped", "C", "");
    notify("ci_status_changed", "ci-1", "");
    notify("ci_status_changed", "ci-2", "");
    notify("ci_status_changed", "ci-3", "");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(2);
    const titles = StubNotification.notifications.map((n) => n.title).sort();
    expect(titles).toEqual(["3 CI runs updated", "3 agents finished"]);
  });

  it("flushes a buffer cleanly so a later isolated event fires as a single", () => {
    installNotificationApi("granted");
    // First burst: 3 events → one aggregated.
    notify("agent_stopped", "A", "");
    notify("agent_stopped", "B", "");
    notify("agent_stopped", "C", "");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(1);
    expect(StubNotification.notifications[0].title).toBe("3 agents finished");
    // Wall-clock has moved on; a later one-off event must NOT pick up
    // the previous burst's count.
    StubNotification.notifications = [];
    vi.advanceTimersByTime(10_000);
    notify("agent_stopped", "D", "body-d");
    vi.advanceTimersByTime(BATCH_DEBOUNCE_MS + 1);
    expect(StubNotification.notifications).toHaveLength(1);
    expect(StubNotification.notifications[0].title).toBe("D");
  });
});

describe("per-class opt-out persistence", () => {
  it("defaults all classes to enabled", () => {
    expect(isClassEnabled("agent_stopped")).toBe(true);
    expect(isClassEnabled("ci_status_changed")).toBe(true);
    expect(getNotificationPrefs().disabledClasses).toEqual([]);
  });

  it("setClassEnabled(klass, false) writes to localStorage and toggles isClassEnabled", () => {
    setClassEnabled("agent_stopped", false);
    expect(isClassEnabled("agent_stopped")).toBe(false);
    expect(isClassEnabled("ci_status_changed")).toBe(true);
    const raw = globalThis.localStorage.getItem(NOTIFICATION_PREFS_STORAGE_KEY);
    expect(raw).toBe(JSON.stringify(["agent_stopped"]));
  });

  it("setClassEnabled(klass, true) removes the class from the disabled list", () => {
    setClassEnabled("agent_stopped", false);
    setClassEnabled("ci_status_changed", false);
    expect(getNotificationPrefs().disabledClasses.sort()).toEqual([
      "agent_stopped",
      "ci_status_changed",
    ]);
    setClassEnabled("agent_stopped", true);
    expect(getNotificationPrefs().disabledClasses).toEqual(["ci_status_changed"]);
  });

  it("ignores unknown class names in localStorage (forward-compat)", () => {
    globalThis.localStorage.setItem(
      NOTIFICATION_PREFS_STORAGE_KEY,
      JSON.stringify(["agent_stopped", "made_up_class"]),
    );
    expect(getNotificationPrefs().disabledClasses).toEqual(["agent_stopped"]);
  });

  it("survives malformed JSON in localStorage", () => {
    globalThis.localStorage.setItem(NOTIFICATION_PREFS_STORAGE_KEY, "not-json{");
    expect(getNotificationPrefs().disabledClasses).toEqual([]);
  });
});

describe("first-visit prompt state", () => {
  it("starts at 'never'", () => {
    expect(readPromptState()).toBe("never");
  });

  it("requestNotificationPermission flips state to 'asked'", async () => {
    installNotificationApi("default");
    StubNotification.requestPermission = vi.fn(
      async (): Promise<NotificationPermission> => "denied",
    );
    await requestNotificationPermission();
    expect(readPromptState()).toBe("asked");
  });

});
