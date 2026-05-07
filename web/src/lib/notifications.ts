/// Browser desktop-notification surface for the dashboard.
///
/// Audit §16 fixed three failures of the prior in-line `notify()` helper
/// in `ws-store.ts`: (1) it called `Notification.requestPermission()` on
/// the very first WS connect, jamming an unsolicited browser prompt in
/// the user's face; (2) every notifiable WS event fired its own
/// notification with no per-class opt-out, so a noisy plan with eight
/// failing tasks generated eight pop-ups; (3) bursts of same-class
/// events (e.g. five `agent_stopped` arriving when auto-mode finishes
/// a phase) had no batching — the user saw every one.
///
/// This module is the single owner of the Notification API for the
/// dashboard. It exposes:
///   - `notify(klass, …)` — buffered, opt-out-aware, batched per class.
///   - `requestNotificationPermission()` — explicit, called only from
///     the AdminPage button or first-visit prompt; never on connect.
///   - `getNotificationPrefs()` / `setClassEnabled(...)` — per-class
///     opt-out stored in localStorage so a refresh keeps the user's
///     choices.
///
/// Batching contract: when ≥3 events of the same class fire within a
/// 5-second window, they collapse into one aggregated notification
/// ("5 agents finished"). With <3 events the originals fire one-by-one.
/// All events incur a small `BATCH_DEBOUNCE_MS` delay so the buffer can
/// see the third arrival before deciding — without that, the first two
/// would already be on screen and the burst couldn't collapse.

/// Classes the dashboard knows how to notify on. The string values
/// match the WS event types the server broadcasts (so call sites in
/// ws-store.ts can pass `msg.type` directly), with two deliberate
/// special cases: `task_completed` / `task_failed` split out of
/// `task_status_changed` so the user can opt out of one without the
/// other.
export type NotificationClass =
  | "agent_stopped"
  | "auto_mode_merged"
  | "auto_mode_paused"
  | "plan_checked"
  | "task_status_changed"
  | "ci_status_changed"
  | "plan_warning"
  | "phase_advanced";

/// User-facing copy for each class — used by AdminPage to render the
/// per-class checkboxes, and by the batcher to label aggregated
/// notifications ("5 agents finished").
interface ClassMeta {
  label: string;
  /// Aggregated notification title template. `{n}` is replaced with
  /// the count. Singular form is irrelevant — the batcher only fires
  /// the aggregated path at n ≥ 3.
  aggregateTitle: string;
  /// One-line explanation shown next to the checkbox in AdminPage.
  description: string;
}

export const NOTIFICATION_CLASSES: Record<NotificationClass, ClassMeta> = {
  agent_stopped: {
    label: "Agent finished",
    aggregateTitle: "{n} agents finished",
    description: "An agent process exited (success, failure, or kill).",
  },
  auto_mode_merged: {
    label: "Auto-mode merged",
    aggregateTitle: "{n} auto-mode merges",
    description: "Auto-mode merged a task branch back into the base.",
  },
  auto_mode_paused: {
    label: "Auto-mode paused",
    aggregateTitle: "{n} auto-mode pauses",
    description: "Auto-mode paused a plan (merge conflict, dirty tree, budget).",
  },
  plan_checked: {
    label: "Plan checked",
    aggregateTitle: "{n} plan checks",
    description: "A check-plan agent reported a verdict (green / red).",
  },
  task_status_changed: {
    label: "Task completed / failed",
    aggregateTitle: "{n} tasks updated",
    description: "A task transitioned to completed or failed.",
  },
  ci_status_changed: {
    label: "CI status",
    aggregateTitle: "{n} CI runs updated",
    description: "A CI run finished (success or failure).",
  },
  plan_warning: {
    label: "Plan parse warning",
    aggregateTitle: "{n} plan warnings",
    description: "The server failed to parse a plan YAML on disk.",
  },
  phase_advanced: {
    label: "Phase advanced",
    aggregateTitle: "{n} phases advanced",
    description: "Auto-mode crossed a phase boundary in a plan.",
  },
};

/// Ordered list for rendering the AdminPage checkboxes — keeps the
/// most operationally-noisy classes at the top so the user sees them
/// first when scanning to opt out.
export const NOTIFICATION_CLASS_ORDER: NotificationClass[] = [
  "agent_stopped",
  "task_status_changed",
  "ci_status_changed",
  "auto_mode_paused",
  "auto_mode_merged",
  "phase_advanced",
  "plan_checked",
  "plan_warning",
];

/// localStorage key that stores the JSON-encoded array of disabled
/// classes. Mirrors `org-store.ts::ACTIVE_ORG_STORAGE_KEY` — single key,
/// shared across tabs, cleared on logout via `resetAllStores()`.
export const NOTIFICATION_PREFS_STORAGE_KEY = "branchwork.notifications.disabledClasses";

/// localStorage key that records whether the user has *seen* (and
/// therefore explicitly chosen to enable / dismiss) the first-visit
/// prompt. Without this the App would re-render the soft prompt on
/// every visit even after a user clicked "Not now". Three valid
/// values: missing (never asked), `"asked"` (we already prompted),
/// `"enabled"` (user clicked Enable; permission may be granted or
/// denied at the browser layer).
export const NOTIFICATION_PROMPT_STATE_STORAGE_KEY = "branchwork.notifications.promptState";

export interface NotificationPrefs {
  disabledClasses: NotificationClass[];
}

/// Public for tests so they can override the debounce instead of
/// burning real wall-clock seconds. Keep small enough to feel
/// responsive (single events still take this long to surface) and
/// large enough that a burst of three events arriving back-to-back
/// from the WS lands in the same buffer flush. 400ms hits the sweet
/// spot for the 5-event-in-2s acceptance test.
export const BATCH_DEBOUNCE_MS = 400;

/// Hard cap on how long the batcher holds events before flushing.
/// Ensures a sustained burst (one event every 200ms for 10s) still
/// produces periodic notifications instead of an indefinite delay.
export const BATCH_MAX_WINDOW_MS = 5_000;

/// Threshold for collapsing into an aggregated notification. <3
/// events still fire individually so a one-off "task complete" pop-up
/// keeps its useful body text.
export const BATCH_THRESHOLD = 3;

interface PendingNotification {
  title: string;
  body: string;
  tag?: string;
  /// Wall-clock when this event was queued; used by `scheduleFlush`
  /// to compute the max-wait cap.
  enqueuedAt: number;
}

interface BatchState {
  buffer: PendingNotification[];
  timer: ReturnType<typeof setTimeout> | null;
}

/// Per-class buffer + timer. Module-scoped so the state survives the
/// lifetime of the module — buffer/timer would otherwise reset on
/// every notify() call. Cleared by `_resetForTests()` between tests.
const batches = new Map<NotificationClass, BatchState>();

export function notificationsSupported(): boolean {
  return typeof window !== "undefined" && "Notification" in window;
}

/// Snapshot of the current browser permission state. Returns
/// `"unsupported"` for environments without the Notification API
/// (jsdom, mobile Safari in some embeds) so callers can branch on a
/// single value instead of also asking `notificationsSupported()`.
export function getNotificationPermission(): NotificationPermission | "unsupported" {
  if (!notificationsSupported()) return "unsupported";
  return Notification.permission;
}

/// Trigger the browser permission prompt. Caller-driven only — never
/// fired from `notify()` or `connect()`. The first-visit prompt UI
/// (AdminPage button, future onboarding tooltip) calls this in
/// response to a user click, which is also what mobile browsers
/// require for the permission dialog to actually surface.
export async function requestNotificationPermission(): Promise<
  NotificationPermission | "unsupported"
> {
  if (!notificationsSupported()) return "unsupported";
  // Record "we already asked" so a soft first-visit prompt component
  // doesn't re-surface for a user who's already engaged with the
  // dialog (granted, denied, or even dismissed).
  writePromptState("asked");
  if (Notification.permission !== "default") {
    return Notification.permission;
  }
  try {
    const result = await Notification.requestPermission();
    return result;
  } catch {
    // Some browsers (older Safari) reject the promise instead of
    // resolving to "denied". Treat as denied so the caller doesn't
    // have to discriminate.
    return "denied";
  }
}

function readDisabledClassesFromStorage(): NotificationClass[] {
  try {
    const raw = globalThis.localStorage?.getItem(NOTIFICATION_PREFS_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    // Filter to known classes — protects against forward-compat (an
    // old browser tab that wrote a class we no longer recognise won't
    // poison the store).
    return parsed.filter(
      (x): x is NotificationClass => typeof x === "string" && x in NOTIFICATION_CLASSES,
    );
  } catch {
    return [];
  }
}

function writeDisabledClassesToStorage(classes: NotificationClass[]): void {
  try {
    globalThis.localStorage?.setItem(NOTIFICATION_PREFS_STORAGE_KEY, JSON.stringify(classes));
  } catch {
    // No-op — best-effort persistence (private mode, quota exceeded).
  }
}

export function getNotificationPrefs(): NotificationPrefs {
  return { disabledClasses: readDisabledClassesFromStorage() };
}

export function isClassEnabled(klass: NotificationClass): boolean {
  return !readDisabledClassesFromStorage().includes(klass);
}

/// Toggle a single class. The AdminPage checkbox onChange handler
/// calls this — keeping the merge logic here means the UI stays
/// stateless (read on each render via `isClassEnabled`).
export function setClassEnabled(klass: NotificationClass, enabled: boolean): void {
  const current = readDisabledClassesFromStorage();
  const exists = current.includes(klass);
  if (enabled && exists) {
    writeDisabledClassesToStorage(current.filter((c) => c !== klass));
  } else if (!enabled && !exists) {
    writeDisabledClassesToStorage([...current, klass]);
  }
}

export type PromptState = "never" | "asked" | "enabled";

export function readPromptState(): PromptState {
  try {
    const raw = globalThis.localStorage?.getItem(NOTIFICATION_PROMPT_STATE_STORAGE_KEY);
    if (raw === "asked" || raw === "enabled") return raw;
    return "never";
  } catch {
    return "never";
  }
}

export function writePromptState(state: PromptState): void {
  try {
    if (state === "never") {
      globalThis.localStorage?.removeItem(NOTIFICATION_PROMPT_STATE_STORAGE_KEY);
    } else {
      globalThis.localStorage?.setItem(NOTIFICATION_PROMPT_STATE_STORAGE_KEY, state);
    }
  } catch {
    // No-op.
  }
}

/// Fire a single browser notification. Best-effort: silently no-ops
/// if the browser doesn't support it, the user hasn't granted
/// permission, or the construction throws (mobile Safari + private
/// modes). Never re-thrown to the caller.
function fireNow(title: string, body: string, tag?: string): void {
  if (getNotificationPermission() !== "granted") return;
  try {
    new Notification(title, { body, tag, icon: "/favicon.ico" });
  } catch {
    // Some browsers throw on direct construction (mobile Safari).
  }
}

function flush(klass: NotificationClass): void {
  const state = batches.get(klass);
  if (!state) return;
  const items = state.buffer;
  // Reset BEFORE firing so a notification handler that triggers a
  // re-entrant `notify(klass, …)` lands in a fresh buffer instead of
  // piggy-backing on the one we're about to drain.
  batches.delete(klass);

  if (items.length === 0) return;

  if (items.length >= BATCH_THRESHOLD) {
    const meta = NOTIFICATION_CLASSES[klass];
    const title = meta.aggregateTitle.replace("{n}", String(items.length));
    fireNow(title, "", `batch-${klass}`);
    return;
  }

  // <3 events: fire each individually so the user keeps the per-event
  // body text ("Task 1.2 — completed").
  for (const item of items) {
    fireNow(item.title, item.body, item.tag);
  }
}

function scheduleFlush(klass: NotificationClass): void {
  const state = batches.get(klass);
  if (!state) return;
  if (state.timer) clearTimeout(state.timer);
  const oldest = state.buffer[0]?.enqueuedAt ?? Date.now();
  const ageMs = Date.now() - oldest;
  // Cap the wait so a sustained event stream still flushes within
  // BATCH_MAX_WINDOW_MS instead of being held forever by debounce
  // resets. Floor at 1ms so setTimeout always fires in the next tick
  // even when the cap has already elapsed.
  const remainingMaxWait = Math.max(1, BATCH_MAX_WINDOW_MS - ageMs);
  const wait = Math.min(BATCH_DEBOUNCE_MS, remainingMaxWait);
  state.timer = setTimeout(() => flush(klass), wait);
}

/// Queue a notification for the given class. Per-class opt-out and
/// the batching algorithm both apply — see module-level docstring.
///
/// `title` and `body` are the single-event copy; the aggregated form
/// uses `NOTIFICATION_CLASSES[klass].aggregateTitle` and ignores the
/// per-call body (the user is reading "5 agents finished", not the
/// 5 individual one-line bodies).
export function notify(klass: NotificationClass, title: string, body: string, tag?: string): void {
  // Three early no-ops below collapse to "the user won't see
  // anything regardless" — drop without buffering so we don't grow
  // memory in a backgrounded tab where permission was never granted.
  if (!notificationsSupported()) return;
  if (Notification.permission !== "granted") return;
  if (!isClassEnabled(klass)) return;

  let state = batches.get(klass);
  if (!state) {
    state = { buffer: [], timer: null };
    batches.set(klass, state);
  }
  state.buffer.push({ title, body, tag, enqueuedAt: Date.now() });
  scheduleFlush(klass);
}

/// Test-only: clear all in-flight buffers and timers. Called from
/// `notifications.test.ts` `beforeEach` so per-test state is fresh.
export function _resetForTests(): void {
  for (const state of batches.values()) {
    if (state.timer) clearTimeout(state.timer);
  }
  batches.clear();
}
