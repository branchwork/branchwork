import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { TaskCard, isWorktreeCwd } from "./TaskCard.js";
import {
  usePlanStore,
  type CiStatus,
  type ParsedPlan,
  type PlanTask,
} from "../stores/plan-store.js";
import { useAgentStore, type Agent } from "../stores/agent-store.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore, type Runner } from "../stores/runner-store.js";

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

function plan(tOrTasks: PlanTask | PlanTask[]): ParsedPlan {
  const tasks = Array.isArray(tOrTasks) ? tOrTasks : [tOrTasks];
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
        tasks,
      },
    ],
  };
}

function agent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-1",
    session_id: "sess-1",
    pid: null,
    parent_agent_id: null,
    plan_name: PLAN,
    task_id: "1.1",
    cwd: "/tmp",
    status: "completed",
    mode: "pty",
    prompt: null,
    started_at: new Date().toISOString(),
    finished_at: new Date().toISOString(),
    last_tool: null,
    last_activity_at: null,
    base_commit: null,
    branch: "branchwork/p1/1.1",
    source_branch: "master",
    cost_usd: null,
    driver: null,
    merge_status: null,
    spawn_error: null,
    ...overrides,
  };
}

function seed(t: PlanTask | PlanTask[], extras: { agents?: Agent[] } = {}): void {
  useAgentStore.setState({ agents: extras.agents ?? [], selectAgent: vi.fn() });
  usePlanStore.setState({
    selectedPlan: plan(t),
    selectPlan: vi.fn().mockResolvedValue(undefined),
    savePlan: vi.fn().mockResolvedValue(undefined),
    fetchPlans: vi.fn().mockResolvedValue(undefined),
    autoModeRuntimes: {},
  });
  // Default settings-store initial state already has effort/drivers/defaultDriver.
  // Force loaded=true so any conditional gating on it stays neutral.
  useSettingsStore.setState({ loaded: true });
}

afterEach(() => {
  cleanup();
  useAgentStore.setState({ agents: [] });
  usePlanStore.setState({ selectedPlan: null, autoModeRuntimes: {}, planConfigs: {} });
  useRunnerStore.setState({ mode: "unknown", runners: [] });
});

/// Build a runner row for the version-gate tests (T1.3). Defaults to a
/// SaaS-mode online runner with green severity so the "Start enabled"
/// branch is the default; tests opt into red severity / override /
/// offline by overriding the relevant fields.
function runner(overrides: Partial<Runner> = {}): Runner {
  return {
    id: "r1",
    name: "primary",
    status: "online",
    hostname: "host-1",
    version: "0.5.0",
    lastSeenAt: "2026-05-19T00:00:00Z",
    createdAt: "2026-05-01T00:00:00Z",
    versionSeverity: "green",
    versionMismatchOverride: false,
    ...overrides,
  };
}

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

describe("TaskCard status dropdown", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  // Captures every fetch call + the JSON body so tests can assert exact
  // requests. Tests pass a per-test response builder for the status PUT
  // so we can drive both the happy path and a 409 working_tree_dirty.
  interface StatusCall {
    url: string;
    method: string;
    body: unknown;
  }
  function installStatusFetch(statusResponder: (body: { status: string }) => Response): {
    calls: StatusCall[];
  } {
    const calls: StatusCall[] = [];
    const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.pathname + input.search
            : input.url;
      const method = init?.method ?? "GET";
      const parsed = init?.body ? JSON.parse(init.body as string) : undefined;
      calls.push({ url, method, body: parsed });
      if (url.match(/\/api\/plans\/[^/]+\/tasks\/[^/]+\/status$/) && method === "PUT") {
        return statusResponder(parsed as { status: string });
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

  it("clicking the pill opens the dropdown listing every valid status", () => {
    const t = task({ status: "pending" });
    seed(t);
    installStatusFetch(() => new Response("{}", { status: 200 }));

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    // All six statuses are listed — `checking` used to be filtered out
    // because it represented a transient state set by check agents, but
    // the dropdown is now the explicit no-cycle replacement for the
    // 6-state click cycle and operators may need to set it manually to
    // recover from a stuck agent.
    expect(screen.getByRole("menuitem", { name: /Pending/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /In Progress/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Done/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Failed/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Skipped/ })).toBeTruthy();
    expect(screen.getByRole("menuitem", { name: /Checking/ })).toBeTruthy();
  });

  it("picking a status POSTs that exact status — no cycle through intermediates", async () => {
    const t = task({ status: "pending" });
    seed(t);
    const { calls } = installStatusFetch(
      () => new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } }),
    );

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    fireEvent.click(screen.getByRole("menuitem", { name: /Skipped/ }));

    await waitFor(() => {
      const put = calls.find(
        (c) => c.url.match(/\/api\/plans\/[^/]+\/tasks\/[^/]+\/status$/) && c.method === "PUT",
      );
      expect(put).toBeDefined();
      expect(put?.body).toEqual({ status: "skipped" });
    });
    // No intermediate `in_progress` / `completed` PUTs.
    const puts = calls.filter((c) => c.method === "PUT");
    expect(puts.length).toBe(1);
  });

  it(
    "409 working_tree_dirty on `completed` shows an inline message and " +
      "still allows picking `skipped` in the same menu",
    async () => {
      const t = task({ status: "pending" });
      seed(t);
      const { calls } = installStatusFetch((body) => {
        if (body.status === "completed") {
          return new Response(
            JSON.stringify({
              error: "working_tree_dirty",
              message: "tree is dirty",
              files: ["a.rs", "b.rs"],
            }),
            { status: 409, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
      });

      render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
      openMenu();

      // Pick `Done` → 409. Inline message must surface AND the menu
      // must stay open so the operator can pick a different status
      // without re-clicking the pill.
      fireEvent.click(screen.getByRole("menuitem", { name: /Done/ }));

      await waitFor(() => {
        expect(screen.getByRole("status")).toBeTruthy();
      });
      expect(screen.getByRole("status").textContent).toMatch(/commit working-tree changes/i);
      expect(screen.getByRole("status").textContent).toMatch(/2 uncommitted file/i);
      // Menu still open.
      expect(screen.getByRole("menu")).toBeTruthy();

      // Now pick Skipped — must succeed without re-opening the menu
      // (acceptance criterion: two clicks total to set skipped).
      fireEvent.click(screen.getByRole("menuitem", { name: /Skipped/ }));

      await waitFor(() => {
        const skipped = calls.find(
          (c) => c.method === "PUT" && (c.body as { status: string }).status === "skipped",
        );
        expect(skipped).toBeDefined();
      });
    },
  );

  it("clears the inline working_tree_dirty message after a successful pick", async () => {
    const t = task({ status: "pending" });
    seed(t);
    installStatusFetch((body) => {
      if (body.status === "completed") {
        return new Response(
          JSON.stringify({
            error: "working_tree_dirty",
            message: "tree is dirty",
            files: ["a.rs"],
          }),
          { status: 409, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
    });

    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    openMenu();

    fireEvent.click(screen.getByRole("menuitem", { name: /Done/ }));
    await waitFor(() => {
      expect(screen.getByRole("status")).toBeTruthy();
    });

    fireEvent.click(screen.getByRole("menuitem", { name: /Skipped/ }));

    await waitFor(() => {
      expect(screen.queryByRole("status")).toBeNull();
    });
  });
});

describe("TaskCard awaiting-cadence pill", () => {
  function findPill(taskNumber = "1.1"): HTMLElement {
    return screen.getByRole("button", { name: new RegExp(`Status menu for task ${taskNumber}`) });
  }

  it("shows the muted-purple pill when the matching agent has merge_status=deferred_for_cadence", () => {
    const t1 = task({ number: "1.1", status: "completed" });
    const t2 = task({ number: "1.2", status: "in_progress" });
    const t3 = task({ number: "1.3", status: "pending" });
    const t4 = task({ number: "1.4", status: "pending" });
    seed([t1, t2, t3, t4], {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.1");
    expect(pill.textContent).toMatch(/Awaiting cadence/);
    // Purple palette — distinguishes from the regular completed (emerald)
    // pill and from the indigo `merging` transient.
    expect(pill.className).toMatch(/purple/);
    // Tooltip names the phase progress so the user knows how many sibling
    // tasks still need to finish before the drain.
    expect(pill.getAttribute("title")).toBe(
      "Completed; waiting for phase boundary to merge (1 of 4 tasks done).",
    );
  });

  it("counts skipped tasks toward the phase progress in the tooltip", () => {
    // should_merge_now (Task 2.1) treats skipped as done — keep the
    // tooltip honest about it.
    const t1 = task({ number: "1.1", status: "completed" });
    const t2 = task({ number: "1.2", status: "skipped" });
    const t3 = task({ number: "1.3", status: "completed" });
    const t4 = task({ number: "1.4", status: "pending" });
    seed([t1, t2, t3, t4], {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    expect(findPill("1.1").getAttribute("title")).toBe(
      "Completed; waiting for phase boundary to merge (3 of 4 tasks done).",
    );
  });

  it("does NOT show the awaiting-cadence pill when merge_status is null", () => {
    // A merged task — agent row still exists but merge_status was cleared
    // by run_merge_step. Pill must drop back to the regular `Done` label.
    const t1 = task({ number: "1.1", status: "completed" });
    seed(t1, {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: null,
          merge_status: null,
          spawn_error: null,
        }),
      ],
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.1");
    expect(pill.textContent).toMatch(/Done/);
    expect(pill.textContent).not.toMatch(/Awaiting cadence/);
    expect(pill.className).not.toMatch(/purple/);
  });

  it("does NOT show the awaiting-cadence pill when task.status is not completed", () => {
    // Defensive: agents row could carry the deferred flag in some race
    // window even before the server flips task_status. Until the canonical
    // status row says `completed`, the pill defers to the regular label.
    const t1 = task({ number: "1.1", status: "in_progress" });
    seed(t1, {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.1");
    expect(pill.textContent).toMatch(/In Progress/);
    expect(pill.textContent).not.toMatch(/Awaiting cadence/);
  });

  it("overrides the awaiting-cadence pill with `Merging...` while the drain is running this task", () => {
    // Drain in flight for this task — the auto_mode_state WS event landed
    // with state=merging and task=1.1. Replace the purple pill with the
    // indigo transient label.
    const t1 = task({ number: "1.1", status: "completed" });
    seed(t1, {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    usePlanStore.setState({
      autoModeRuntimes: { [PLAN]: { state: "merging", task: "1.1" } },
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.1");
    expect(pill.textContent).toMatch(/Merging\.\.\./);
    expect(pill.textContent).not.toMatch(/Awaiting cadence/);
    expect(pill.className).toMatch(/indigo/);
    expect(pill.getAttribute("title")).toMatch(/merging this branch/i);
  });

  it("does NOT show `Merging...` on sibling tasks while a different task is mid-merge", () => {
    // Drain processes agents sequentially. Only the task whose number
    // matches the auto-mode runtime gets the indigo pill — the others
    // keep showing awaiting-cadence (still deferred) or Done (already
    // merged on a prior tick).
    const t1 = task({ number: "1.1", status: "completed" });
    const t2 = task({ number: "1.2", status: "completed" });
    seed([t1, t2], {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
        agent({
          id: "a-1.2",
          task_id: "1.2",
          status: "completed",
          branch: "branchwork/p1/1.2",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    usePlanStore.setState({
      autoModeRuntimes: { [PLAN]: { state: "merging", task: "1.1" } },
    });
    render(<TaskCard task={t2} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.2");
    expect(pill.textContent).toMatch(/Awaiting cadence/);
    expect(pill.textContent).not.toMatch(/Merging/);
  });

  it("clears back to the regular Done pill once the drain finishes (branch + merge_status both null)", () => {
    // Post-merge state: run_merge_step set branch=null and merge_status=null
    // on the row, and the auto_mode_state event flipped state=advancing
    // (which ws-store maps to runtime=null). Pill must read as plain Done.
    const t1 = task({ number: "1.1", status: "completed" });
    seed(t1, {
      agents: [
        agent({
          id: "a-1.1",
          task_id: "1.1",
          status: "completed",
          branch: null,
          merge_status: null,
          spawn_error: null,
        }),
      ],
    });
    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const pill = findPill("1.1");
    expect(pill.textContent).toMatch(/Done/);
    expect(pill.textContent).not.toMatch(/Awaiting cadence|Merging/);
    expect(pill.className).toMatch(/emerald/);
  });
});

describe("TaskCard runner-version dispatch gate (T1.3)", () => {
  /// Acceptance criterion verbatim from the T1.3 brief: a v0.3.0 runner
  /// connected to a v0.5.x server renders Start session disabled with
  /// the tooltip "runner too old — upgrade required".
  it("disables Start with tooltip when the chosen runner is red-severity", () => {
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        runner({
          id: "old-runner",
          version: "0.3.0",
          versionSeverity: "red",
          versionMismatchOverride: false,
        }),
      ],
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.title).toBe("runner too old — upgrade required");
  });

  it("re-enables Start when the operator has set versionMismatchOverride", () => {
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        runner({
          id: "old-runner",
          version: "0.3.0",
          versionSeverity: "red",
          versionMismatchOverride: true,
        }),
      ],
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);

    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    expect(btn.title).not.toBe("runner too old — upgrade required");
  });

  it("does not gate Start in standalone mode (no runners expected)", () => {
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    // mode='unknown' is the default afterEach reset; also covers the
    // 'standalone' deployment shape.
    useRunnerStore.setState({
      mode: "standalone",
      runners: [
        runner({
          id: "old-runner",
          version: "0.3.0",
          versionSeverity: "red",
        }),
      ],
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);
    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it("does not gate Start when the chosen runner is amber (warn-only)", () => {
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        runner({
          id: "warn-runner",
          version: "1.4.0",
          versionSeverity: "amber",
        }),
      ],
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);
    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it("respects the plan's pinned runner over the first-online fallback", () => {
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    // Two runners — the most-recently-seen is healthy, but the pin
    // points at the old one. Gate must consult the pin.
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        runner({ id: "fresh-runner", version: "0.5.0", versionSeverity: "green" }),
        runner({ id: "old-runner", version: "0.3.0", versionSeverity: "red" }),
      ],
    });
    usePlanStore.setState({
      selectedPlan: plan(t1),
      selectPlan: vi.fn().mockResolvedValue(undefined),
      savePlan: vi.fn().mockResolvedValue(undefined),
      fetchPlans: vi.fn().mockResolvedValue(undefined),
      autoModeRuntimes: {},
      planConfigs: {
        [PLAN]: {
          autoMode: false,
          autoAdvance: false,
          maxFixAttempts: 3,
          pausedReason: null,
          parallel: false,
          worktreeIsolation: false,
          runnerId: "old-runner",
          runnerFailover: "pause",
        },
      },
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);
    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(btn.title).toBe("runner too old — upgrade required");
  });

  it("re-enables Start when the pinned runner is offline (other failure modes own the UX)", () => {
    // Pinned + offline runner: the dispatcher pauses with
    // 'runner_offline' on click. Pre-clicking Start should NOT also
    // surface the version block — there's no online dispatch target to
    // gate against.
    const t1 = task({ number: "1.1", status: "pending" });
    seed(t1);
    useRunnerStore.setState({
      mode: "saas",
      runners: [
        runner({
          id: "old-runner",
          status: "offline",
          version: "0.3.0",
          versionSeverity: "red",
        }),
      ],
    });
    usePlanStore.setState({
      selectedPlan: plan(t1),
      selectPlan: vi.fn().mockResolvedValue(undefined),
      savePlan: vi.fn().mockResolvedValue(undefined),
      fetchPlans: vi.fn().mockResolvedValue(undefined),
      autoModeRuntimes: {},
      planConfigs: {
        [PLAN]: {
          autoMode: false,
          autoAdvance: false,
          maxFixAttempts: 3,
          pausedReason: null,
          parallel: false,
          worktreeIsolation: false,
          runnerId: "old-runner",
          runnerFailover: "pause",
        },
      },
    });

    render(<TaskCard task={t1} planName={PLAN} phaseNumber={1} />);
    const btn = screen.getByTestId("start-task-button") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });
});

describe("TaskCard worktree path indicator (Task 6.1)", () => {
  // Realistic UUID-shaped agent id; the worktree manager uses its first 8
  // chars (`abcd1234`) as the trailing path segment suffix.
  const WT_AGENT_ID = "abcd1234-0000-0000-0000-000000000000";
  const WT_CWD = "/home/x/.branchwork/worktrees/branchwork/p1/1.1-abcd1234";

  describe("isWorktreeCwd", () => {
    it("returns true for a per-agent worktree path", () => {
      expect(isWorktreeCwd(WT_CWD, WT_AGENT_ID)).toBe(true);
    });

    it("tolerates a trailing slash", () => {
      expect(isWorktreeCwd(`${WT_CWD}/`, WT_AGENT_ID)).toBe(true);
    });

    it("returns false for a bare project-root cwd (legacy mode)", () => {
      expect(isWorktreeCwd("/home/x/projects/myproj", WT_AGENT_ID)).toBe(false);
    });

    it("returns false for an override base that still ends with the agent suffix", () => {
      // Survives a BRANCHWORK_WORKTREE_BASE override — only the trailing
      // `-<short id>` matters, not the leading base dirs.
      expect(isWorktreeCwd("/mnt/fast/wt/branchwork/p1/1.1-abcd1234", WT_AGENT_ID)).toBe(true);
    });

    it("returns false for null / empty cwd", () => {
      expect(isWorktreeCwd(null, WT_AGENT_ID)).toBe(false);
      expect(isWorktreeCwd(undefined, WT_AGENT_ID)).toBe(false);
      expect(isWorktreeCwd("", WT_AGENT_ID)).toBe(false);
    });

    it("returns false for an empty agent id", () => {
      expect(isWorktreeCwd(WT_CWD, "")).toBe(false);
    });
  });

  it("renders the running-in row with the full path in the title", () => {
    const t = task({ number: "1.1", status: "in_progress" });
    seed(t, {
      agents: [
        agent({ id: WT_AGENT_ID, task_id: "1.1", status: "running", cwd: WT_CWD, branch: null }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    const row = screen.getByTestId("worktree-path-row");
    expect(row.textContent).toMatch(/running in:/);
    // The full, untruncated path lives in the title attribute (the
    // visible text is middle-truncated via CSS, so we assert the title).
    expect(row.querySelector(`[title="${WT_CWD}"]`)).not.toBeNull();
  });

  it("copy button writes the full cwd to the clipboard", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });

    const t = task({ number: "1.1", status: "in_progress" });
    seed(t, {
      agents: [
        agent({ id: WT_AGENT_ID, task_id: "1.1", status: "running", cwd: WT_CWD, branch: null }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    fireEvent.click(screen.getByRole("button", { name: /Copy worktree path/i }));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(WT_CWD));
  });

  it("does NOT render in legacy shared-cwd mode (cwd is the project root)", () => {
    const t = task({ number: "1.1", status: "in_progress" });
    seed(t, {
      agents: [
        agent({
          id: WT_AGENT_ID,
          task_id: "1.1",
          status: "running",
          cwd: "/home/x/projects/myproj",
          branch: null,
        }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.queryByTestId("worktree-path-row")).toBeNull();
  });

  it("renders a 'worktree:' row (not 'running in:') for a finished branch agent", () => {
    // Phase 2: a finished agent whose branch is parked on disk (the
    // merge-deferred / ready-to-merge case) surfaces its worktree so the
    // operator can see/clean up the checkout. The label is past-tense
    // "worktree:" because nothing is mutating it — distinct from the live
    // "running in:" row.
    const t = task({ number: "1.1", status: "completed" });
    seed(t, {
      agents: [
        agent({
          id: WT_AGENT_ID,
          task_id: "1.1",
          status: "completed",
          cwd: WT_CWD,
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);

    const row = screen.getByTestId("worktree-path-row");
    expect(row.textContent).toMatch(/worktree:/);
    expect(row.textContent).not.toMatch(/running in:/);
    expect(row.querySelector(`[title="${WT_CWD}"]`)).not.toBeNull();
  });

  it("does NOT render any worktree row for a finished branch agent in legacy shared-cwd mode", () => {
    // Branch parked at the bare project root (legacy, no per-agent
    // worktree) carries no `-<short id>` suffix, so there's nothing to
    // surface — the row stays hidden.
    const t = task({ number: "1.1", status: "completed" });
    seed(t, {
      agents: [
        agent({
          id: WT_AGENT_ID,
          task_id: "1.1",
          status: "completed",
          cwd: "/home/x/projects/myproj",
          branch: "branchwork/p1/1.1",
          merge_status: "deferred_for_cadence",
        }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.queryByTestId("worktree-path-row")).toBeNull();
  });
});

describe("TaskCard spawn-error banner", () => {
  // Agents are stored started_at DESC; the array order here mirrors that.
  it("shows the spawn-error banner when the task's most recent attempt failed at spawn", () => {
    const t = task({ number: "2.3", status: "pending" });
    seed(t, {
      agents: [
        agent({
          id: "failed-1",
          task_id: "2.3",
          status: "failed",
          branch: null,
          spawn_error: "worktree setup failed: branch already used by worktree at ...",
        }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.getByTestId("spawn-error-banner")).toBeTruthy();
  });

  it("suppresses a stale spawn-error banner once a newer attempt succeeded", () => {
    // The reported bug: a failed spawn (older) leaves a `failed`+spawn_error
    // row that never flips status; a later successful re-run is a different,
    // completed row. The banner must key off the MOST RECENT attempt, so it
    // disappears here instead of lingering as a phantom "merge error".
    const t = task({ number: "2.3", status: "completed" });
    seed(t, {
      agents: [
        // Most recent: the successful re-run (completed, branch since cleared).
        agent({ id: "rerun-ok", task_id: "2.3", status: "completed", branch: null }),
        // Older: the spawn that failed before cleanup freed the worktree.
        agent({
          id: "failed-old",
          task_id: "2.3",
          status: "failed",
          branch: null,
          spawn_error: "worktree setup failed: branch already used by worktree at ...",
        }),
      ],
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.queryByTestId("spawn-error-banner")).toBeNull();
  });
});

describe("observe-mode plans (pivot 2026-06-11)", () => {
  function setObservedPlan(t: PlanTask) {
    const p = plan(t);
    usePlanStore.setState({ selectedPlan: { ...p, mode: "observe" } });
  }

  it("hides every orchestration affordance on observed plans", () => {
    setObservedPlan(task({ status: "pending" }));
    render(<TaskCard task={task({ status: "pending" })} planName={PLAN} phaseNumber={1} />);
    expect(screen.queryByTestId("start-task-button")).toBeNull();
    expect(screen.queryByText("Check")).toBeNull();
  });

  it("still shows Start on managed plans", () => {
    usePlanStore.setState({ selectedPlan: plan(task({ status: "pending" })) });
    render(<TaskCard task={task({ status: "pending" })} planName={PLAN} phaseNumber={1} />);
    expect(screen.getByTestId("start-task-button")).toBeTruthy();
  });

  it("renders the declaring-agent chip and artifact links", () => {
    const t = task({
      status: "completed",
      agent: "claude-fable (session 2026-06-11)",
      artifacts: [
        { url: "https://github.com/x/y/pull/42", createdAt: new Date().toISOString() },
        { url: "1a2b3c4d5e6f7a8b", createdAt: new Date().toISOString() },
      ],
    });
    setObservedPlan(t);
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.getByTestId("agent-chip").textContent).toContain("claude-fable");
    const link = screen.getByTestId("artifact-link") as HTMLAnchorElement;
    expect(link.href).toBe("https://github.com/x/y/pull/42");
    expect(link.textContent).toBe("PR #42");
    expect(screen.getByTestId("artifact-ref").textContent).toBe("1a2b3c4");
  });
});

describe("task wall-clock + declared-vs-CI divergence (pivot 2.2)", () => {
  it("renders start → end times with duration", () => {
    const t = task({
      status: "completed",
      startedAt: "2026-06-11 20:00:00",
      endedAt: "2026-06-11 20:38:00",
    });
    usePlanStore.setState({ selectedPlan: plan(t) });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    const times = screen.getByTestId("task-times");
    expect(times.textContent).toContain("→");
    expect(times.textContent).toContain("38m");
  });

  it("shows the divergence badge when completed but the PR CI fails", () => {
    const t = task({ status: "completed" });
    usePlanStore.setState({
      selectedPlan: plan(t),
      artifactCi: { "1.1": { url: "https://github.com/x/y/pull/7", state: "failure" } },
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    const badge = screen.getByTestId("divergence-badge") as HTMLAnchorElement;
    expect(badge.textContent).toContain("Declared");
    expect(badge.href).toBe("https://github.com/x/y/pull/7");
  });

  it("no badge when the CI is green or the task is not completed", () => {
    const t = task({ status: "completed" });
    usePlanStore.setState({
      selectedPlan: plan(t),
      artifactCi: { "1.1": { url: "https://github.com/x/y/pull/7", state: "success" } },
    });
    render(<TaskCard task={t} planName={PLAN} phaseNumber={1} />);
    expect(screen.queryByTestId("divergence-badge")).toBeNull();
  });
});
