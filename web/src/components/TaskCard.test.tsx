import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { TaskCard } from "./TaskCard.js";
import {
  usePlanStore,
  type CiStatus,
  type ParsedPlan,
  type PlanTask,
} from "../stores/plan-store.js";
import { useAgentStore } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

const PLAN = "p1";

function task(overrides: Partial<PlanTask> = {}): PlanTask {
  return {
    number: "1.1",
    title: "Sample task",
    description: "",
    filePaths: [],
    acceptance: "",
    ...overrides,
  };
}

function ci(overrides: Partial<CiStatus> = {}): CiStatus {
  return {
    id: 1,
    status: "success",
    updatedAt: new Date().toISOString(),
    ...overrides,
  };
}

function plan(t: PlanTask): ParsedPlan {
  return {
    name: PLAN,
    filePath: "/tmp/p1.yaml",
    title: "Plan 1",
    context: "",
    project: null,
    createdAt: new Date().toISOString(),
    modifiedAt: new Date().toISOString(),
    phases: [
      {
        number: 1,
        title: "Phase 1",
        description: "",
        tasks: [t],
      },
    ],
  };
}

function seed(t: PlanTask): void {
  useAgentStore.setState({ agents: [], selectAgent: vi.fn() });
  usePlanStore.setState({
    selectedPlan: plan(t),
    selectPlan: vi.fn().mockResolvedValue(undefined),
    savePlan: vi.fn().mockResolvedValue(undefined),
    fetchPlans: vi.fn().mockResolvedValue(undefined),
  });
  // Default settings-store initial state already has effort/drivers/defaultDriver.
  // Force loaded=true so any conditional gating on it stays neutral.
  useSettingsStore.setState({ loaded: true });
}

afterEach(() => {
  cleanup();
  useAgentStore.setState({ agents: [] });
  usePlanStore.setState({ selectedPlan: null });
});

// The merge-button gate in TaskCard.tsx (line 99):
//   const canMerge = task.producesCommit !== false;
// This mirrors the exact expression — undefined/true → show Merge, false → hide.
function canMerge(t: Pick<PlanTask, "producesCommit">): boolean {
  return t.producesCommit !== false;
}

describe("TaskCard canMerge gate", () => {
  it("shows Merge when producesCommit is undefined (default)", () => {
    expect(canMerge({})).toBe(true);
  });

  it("shows Merge when producesCommit is true", () => {
    expect(canMerge({ producesCommit: true })).toBe(true);
  });

  it("hides Merge when producesCommit is false", () => {
    expect(canMerge({ producesCommit: false })).toBe(false);
  });
});

describe("TaskCard CI badge — via_fix_attempt marker", () => {
  it("appends a fix #N chip and 'passed via fix attempt N' tooltip when set", () => {
    const t = task({ ci: ci({ viaFixAttempt: 1 }) });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    // Chip text reads "fix #1" next to the green check.
    expect(screen.getByText(/fix #1/i)).toBeTruthy();
    // The CI label is preserved (green check is part of c.label "CI ✓").
    expect(screen.getByText(/CI/i)).toBeTruthy();
    // Tooltip is on the badge wrapper. No runUrl so the badge is a <span>;
    // querying by title attribute pulls the wrapper directly.
    const badge = document.querySelector('[title="passed via fix attempt 1"]');
    expect(badge).not.toBeNull();
  });

  it("renders unchanged when viaFixAttempt is not set", () => {
    const t = task({ ci: ci() });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    // No fix chip on the original-CI path.
    expect(screen.queryByText(/fix #/i)).toBeNull();
    // Tooltip is the original c.title for the success case.
    const badge = document.querySelector('[title="CI passed"]');
    expect(badge).not.toBeNull();
    // And the "passed via fix attempt" wording must NOT leak into a no-fix
    // run's tooltip.
    expect(document.querySelector('[title*="passed via fix attempt"]')).toBeNull();
  });

  it("appends ' — open run' to the fix tooltip when runUrl is present", () => {
    const t = task({
      ci: ci({
        viaFixAttempt: 2,
        runUrl: "https://example.invalid/run/42",
      }),
    });
    seed(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    expect(screen.getByText(/fix #2/i)).toBeTruthy();
    const link = document.querySelector('[title="passed via fix attempt 2 — open run"]');
    expect(link).not.toBeNull();
    expect(link?.tagName).toBe("A");
  });
});

interface LearningCall {
  url: string;
  method: string;
  body?: unknown;
}

function installLearningsFetch(initial: { id: number; learning: string; createdAt: string }[]) {
  const calls: LearningCall[] = [];
  const store = [...initial];
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(init.body as string) : undefined;
    calls.push({ url, method, body });
    const match = url.match(/^\/api\/plans\/([^/]+)\/tasks\/([^/]+)\/learnings$/);
    if (match && method === "GET") {
      return new Response(JSON.stringify(store), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    if (match && method === "POST") {
      const next = {
        id: store.length + 1,
        learning: (body as { learning: string }).learning,
        createdAt: new Date().toISOString(),
      };
      // Server returns most-recent-first on subsequent GETs.
      store.unshift(next);
      return new Response(JSON.stringify({ ok: true, id: next.id, learning: next.learning }), {
        status: 201,
        headers: { "Content-Type": "application/json" },
      });
    }
    return new Response("not found", { status: 404 });
  });
  vi.stubGlobal("fetch", fn);
  return { calls };
}

describe("TaskCard learnings", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("is collapsed by default and only fetches on first expand", async () => {
    const t = task();
    seed(t);
    const { calls } = installLearningsFetch([]);

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    // No fetch fired yet.
    expect(calls.filter((c) => c.url.includes("/learnings")).length).toBe(0);

    const toggle = screen.getByRole("button", { name: /^Learnings/ });
    fireEvent.click(toggle);

    await waitFor(() => {
      expect(calls.filter((c) => c.url.endsWith("/learnings") && c.method === "GET").length).toBe(
        1,
      );
    });
    // Empty-state copy renders.
    expect(screen.getByText(/No learnings recorded yet/i)).toBeTruthy();
  });

  it("renders existing learnings and appends a new one via POST", async () => {
    const t = task();
    seed(t);
    const { calls } = installLearningsFetch([
      {
        id: 7,
        learning: "first existing note",
        createdAt: new Date().toISOString(),
      },
    ]);

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    fireEvent.click(screen.getByRole("button", { name: /^Learnings/ }));

    await waitFor(() => {
      expect(screen.getByText("first existing note")).toBeTruthy();
    });

    // Click the EditableText trigger — its accessible name is
    // "Edit add learning to task 1.1" (EditableText prefixes "Edit ").
    const trigger = screen.getByRole("button", {
      name: /Edit add learning to task 1\.1/i,
    });
    fireEvent.click(trigger);

    const input = await screen.findByLabelText("add learning to task 1.1");
    fireEvent.change(input, { target: { value: "  brand new learning  " } });
    // Blur commits — the EditableText form-submit path also works.
    fireEvent.blur(input);

    await waitFor(() => {
      const post = calls.find((c) => c.url.endsWith("/learnings") && c.method === "POST");
      expect(post).toBeTruthy();
      expect((post?.body as { learning: string } | undefined)?.learning).toBe("brand new learning");
    });

    // After the POST, load() refetches and the new entry shows.
    await waitFor(() => {
      expect(screen.getByText("brand new learning")).toBeTruthy();
    });

    // Count badge updates from "(1)" to "(2)".
    expect(screen.getByRole("button", { name: /^Learnings \(2\)/ })).toBeTruthy();
  });
});

describe("TaskCard Reset confirm-click", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  function installResetFetch() {
    const calls: { url: string; method: string }[] = [];
    const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.pathname + input.search
            : input.url;
      const method = init?.method ?? "GET";
      calls.push({ url, method });
      if (url.endsWith("/reset-status") && method === "POST") {
        return new Response(
          JSON.stringify({ ok: true, plan_name: PLAN, task_number: "1.1", cleared: 1 }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      if (url.match(/\/api\/plans\/[^/]+\/tasks\/[^/]+\/status$/) && method === "PUT") {
        return new Response(
          JSON.stringify({ ok: true, plan_name: PLAN, task_number: "1.1", status: "pending" }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response("not found", { status: 404 });
    });
    vi.stubGlobal("fetch", fn);
    return { calls };
  }

  function openMenu() {
    const trigger = screen.getByRole("button", { name: /Status menu for task 1\.1/ });
    fireEvent.click(trigger);
  }

  it("first click on Reset arms; second click commits and POSTs reset-status", async () => {
    const t = task({ status: "checking" });
    seed(t);
    const { calls } = installResetFetch();

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    // First click: arm. Label flips to the confirm copy. No POST yet.
    const reset = screen.getByRole("menuitem", { name: /Reset task status/ });
    fireEvent.click(reset);
    expect(calls.find((c) => c.url.endsWith("/reset-status"))).toBeUndefined();

    const confirm = await screen.findByRole("menuitem", { name: /Confirm reset task status/ });

    // Second click: commits.
    fireEvent.click(confirm);
    await waitFor(() => {
      const post = calls.find((c) => c.url.endsWith("/reset-status") && c.method === "POST");
      expect(post).toBeDefined();
      expect(post?.url).toBe(`/api/plans/${PLAN}/tasks/1.1/reset-status`);
    });
  });

  it("auto-disarms after 4 s so a stale armed state can't fire later", async () => {
    vi.useFakeTimers();
    const t = task({ status: "checking" });
    seed(t);
    installResetFetch();

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    fireEvent.click(screen.getByRole("menuitem", { name: /Reset task status/ }));
    // Armed copy is visible.
    expect(screen.getByText(/Click again to confirm/)).toBeTruthy();

    // Advance past the 4 s auto-disarm. `act` flushes the resulting state
    // update so the next assertion sees the disarmed render.
    act(() => {
      vi.advanceTimersByTime(4500);
    });

    // Label reverts to "Reset" — the next click would re-arm, not commit.
    expect(screen.getByRole("menuitem", { name: /Reset task status/ })).toBeTruthy();
    expect(screen.queryByText(/Click again to confirm/)).toBeNull();
  });

  it("picking another status from the menu disarms the Reset confirm", async () => {
    const t = task({ status: "checking" });
    seed(t);
    const { calls } = installResetFetch();

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    // Arm the reset.
    fireEvent.click(screen.getByRole("menuitem", { name: /Reset task status/ }));
    expect(screen.getByText(/Click again to confirm/)).toBeTruthy();

    // Pick a different status — disarms.
    const pending = screen.getByRole("menuitem", { name: /Pending/ });
    fireEvent.click(pending);

    await waitFor(() => {
      // PUT for the status went out, but no reset-status POST.
      const reset = calls.find((c) => c.url.endsWith("/reset-status"));
      expect(reset).toBeUndefined();
    });
    // And the Reset item is back to its idle label.
    expect(screen.getByRole("menuitem", { name: /Reset task status/ })).toBeTruthy();
  });
});
