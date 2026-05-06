import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  screen,
  waitFor,
} from "@testing-library/react";
import { AuditLog } from "./AuditLog.js";
import { useAuthStore } from "../stores/auth-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useWsStore } from "../stores/ws-store.js";
import { renderWithRouter as render } from "../test-helpers/render.js";

interface MockEntry {
  id: number;
  orgId: string;
  userId: string | null;
  userEmail: string | null;
  action: string;
  resourceType: string;
  resourceId: string | null;
  diff: string | null;
  createdAt: string;
  snapshotId?: number | null;
  recoverable?: boolean;
  restoredAt?: string | null;
}

function entry(o: Partial<MockEntry>): MockEntry {
  return {
    id: 1,
    orgId: "default-org",
    userId: null,
    userEmail: null,
    action: "auto_mode.merged",
    resourceType: "agent",
    resourceId: null,
    diff: null,
    createdAt: new Date().toISOString().slice(0, 19),
    recoverable: false,
    ...o,
  };
}

interface RestoreCase {
  status: number;
  body?: unknown;
}

function installFetchMock(
  entries: MockEntry[],
  restoreResponses: Record<number, RestoreCase> = {},
) {
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const path =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    if (path.startsWith("/api/orgs/") && path.includes("/audit-log")) {
      return new Response(
        JSON.stringify({
          entries,
          total: entries.length,
          limit: 50,
          offset: 0,
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      );
    }
    const restoreMatch = path.match(/^\/api\/snapshots\/(\d+)\/restore$/);
    if (restoreMatch && init?.method === "POST") {
      const id = Number(restoreMatch[1]);
      const r = restoreResponses[id] ?? { status: 200 };
      const body =
        r.body ??
        (r.status === 200
          ? {
              ok: true,
              plan: "p",
              snapshotId: id,
              restoredAt: "2026-05-05 03:00:00",
            }
          : { error: "unspecified" });
      return new Response(JSON.stringify(body), {
        status: r.status,
        headers: { "Content-Type": "application/json" },
        statusText:
          r.status === 200
            ? "OK"
            : r.status === 409
              ? "Conflict"
              : r.status === 410
                ? "Gone"
                : "Error",
      });
    }
    return new Response(JSON.stringify({}), { status: 404 });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

describe("AuditLog auto-mode action rendering", () => {
  beforeEach(() => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com", orgId: "default-org" },
      loading: false,
      error: null,
    });
    // The component reads `connected` and `socket`. Leave both falsy so
    // the WS effect is a no-op (no socket subscription, no live refresh).
    useWsStore.setState({ connected: false } as never);
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("renders auto_mode.merged with checkmark, label, sha and target", async () => {
    installFetchMock([
      entry({
        id: 11,
        action: "auto_mode.merged",
        resourceType: "agent",
        resourceId: "ag-123",
        diff: JSON.stringify({
          plan: "p1",
          task: "1.2",
          sha: "abcdef0123456789",
          target: "master",
        }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Auto-merged task/i)).toBeTruthy();
    });
    // Icon glyph for merged is the check mark.
    expect(screen.getByText("✓")).toBeTruthy();
    // Diff summary shows the short sha and target.
    expect(screen.getByText(/abcdef0/)).toBeTruthy();
    expect(screen.getByText(/master/)).toBeTruthy();
  });

  it("renders auto_mode.paused with reason", async () => {
    installFetchMock([
      entry({
        id: 12,
        action: "auto_mode.paused",
        resourceType: "plan",
        resourceId: "p1",
        diff: JSON.stringify({
          plan: "p1",
          task: "1.3",
          reason: "merge_conflict",
        }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Auto-mode paused/i)).toBeTruthy();
    });
    expect(screen.getByText("■")).toBeTruthy();
    expect(screen.getByText(/merge_conflict/)).toBeTruthy();
  });

  it("renders auto_mode.fix_spawned with attempt and run id", async () => {
    installFetchMock([
      entry({
        id: 13,
        action: "auto_mode.fix_spawned",
        resourceType: "plan",
        resourceId: "p1",
        diff: JSON.stringify({
          plan: "p1",
          task: "2.1",
          fix_task: "2.1-fix-2",
          fix_agent_id: "ag-fix-9",
          attempt: 2,
          ci_run_id: "987",
        }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Spawned fix agent/i)).toBeTruthy();
    });
    expect(screen.getByText("↺")).toBeTruthy();
    expect(screen.getByText(/attempt 2/)).toBeTruthy();
    expect(screen.getByText(/run 987/)).toBeTruthy();
  });

  it("renders auto_mode.ci_passed with sha and outcome", async () => {
    installFetchMock([
      entry({
        id: 14,
        action: "auto_mode.ci_passed",
        resourceType: "plan",
        resourceId: "p1",
        diff: JSON.stringify({
          plan: "p1",
          task: "3.1",
          sha: "deadbeef0011223344",
          outcome: "green",
        }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/CI passed \(advanced\)/i)).toBeTruthy();
    });
    expect(screen.getByText("→")).toBeTruthy();
    expect(screen.getByText(/deadbee/)).toBeTruthy();
    expect(screen.getByText(/green/)).toBeTruthy();
  });

  it("renders auto_mode.ci_failed with run id", async () => {
    installFetchMock([
      entry({
        id: 15,
        action: "auto_mode.ci_failed",
        resourceType: "plan",
        resourceId: "p1",
        diff: JSON.stringify({
          plan: "p1",
          task: "3.2",
          sha: "f00ba12abcdef",
          ci_run_id: "100",
        }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/^CI failed$/i)).toBeTruthy();
    });
    expect(screen.getByText("✗")).toBeTruthy();
    expect(screen.getByText(/run 100/)).toBeTruthy();
  });

  it("renders auto_mode.resumed with last completed task", async () => {
    installFetchMock([
      entry({
        id: 16,
        action: "auto_mode.resumed",
        resourceType: "plan",
        resourceId: "p1",
        diff: JSON.stringify({ last_completed_task: "1.4" }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Resumed auto-mode/i)).toBeTruthy();
    });
    expect(screen.getByText("▸")).toBeTruthy();
    expect(screen.getByText(/from T1\.4/)).toBeTruthy();
  });

  it("renders agent.auto_finish with Stop hook trigger badge", async () => {
    installFetchMock([
      entry({
        id: 21,
        action: "agent.auto_finish",
        resourceType: "agent",
        resourceId: "ag-stop",
        diff: JSON.stringify({ trigger: "stop_hook" }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Auto-finished agent/i)).toBeTruthy();
    });
    expect(screen.getByText("Stop hook")).toBeTruthy();
  });

  it("renders agent.auto_finish with idle timeout trigger badge", async () => {
    installFetchMock([
      entry({
        id: 22,
        action: "agent.auto_finish",
        resourceType: "agent",
        resourceId: "ag-idle",
        diff: JSON.stringify({ trigger: "idle_timeout" }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Auto-finished agent/i)).toBeTruthy();
    });
    expect(screen.getByText("idle timeout")).toBeTruthy();
  });

  it("renders agent.auto_finish with generic auto badge when diff is unparseable", async () => {
    installFetchMock([
      entry({
        id: 23,
        action: "agent.auto_finish",
        resourceType: "agent",
        resourceId: "ag-bad",
        diff: "not-json",
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Auto-finished agent/i)).toBeTruthy();
    });
    expect(screen.getByText("auto")).toBeTruthy();
  });

  it("renders config.auto_mode toggle entry", async () => {
    installFetchMock([
      entry({
        id: 17,
        action: "config.auto_mode",
        resourceType: "plan",
        resourceId: "p1",
        userEmail: "u@example.com",
        diff: JSON.stringify({ enabled: true, maxFixAttempts: 3 }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Configured auto-mode/i)).toBeTruthy();
    });
    expect(screen.getByText(/enabled, max fix attempts 3/)).toBeTruthy();
  });
});

describe("AuditLog Undo affordance", () => {
  beforeEach(() => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com", orgId: "default-org" },
      loading: false,
      error: null,
    });
    useWsStore.setState({ connected: false } as never);
    // Toasts may be carried across tests via the shared store; reset
    // them so the success-toast assertion is unambiguous.
    usePlanStore.setState({ toasts: [] });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("renders Undo button for recoverable plan.delete row", async () => {
    installFetchMock([
      entry({
        id: 30,
        action: "plan.delete",
        resourceType: "plan",
        resourceId: "obsolete-plan",
        diff: JSON.stringify({ snapshot_id: 5, hard: false }),
        snapshotId: 5,
        recoverable: true,
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Deleted plan/i)).toBeTruthy();
    });
    const button = screen.getByRole("button", { name: /Undo/i });
    expect(button).toBeTruthy();
    expect((button as HTMLButtonElement).disabled).toBe(false);
  });

  it("renders 'Restored' annotation when restoredAt is set", async () => {
    installFetchMock([
      entry({
        id: 31,
        action: "plan.delete",
        resourceType: "plan",
        resourceId: "p",
        diff: JSON.stringify({ snapshot_id: 5 }),
        snapshotId: 5,
        recoverable: false,
        restoredAt: new Date(Date.now() - 5 * 60_000)
          .toISOString()
          .slice(0, 19),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Deleted plan/i)).toBeTruthy();
    });
    // Match the cell content (relative time follows "Restored "), which
    // disambiguates from the "Restored plan" dropdown option in the
    // action filter.
    expect(screen.getByText(/Restored\s+(just now|\d+[mhd] ago)/i))
      .toBeTruthy();
    expect(screen.queryByRole("button", { name: /Undo/i })).toBeNull();
  });

  it("renders 'no longer recoverable' for expired snapshot", async () => {
    installFetchMock([
      entry({
        id: 32,
        action: "plan.delete",
        resourceType: "plan",
        resourceId: "p",
        diff: JSON.stringify({ snapshot_id: 6 }),
        snapshotId: 6,
        recoverable: false,
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Deleted plan/i)).toBeTruthy();
    });
    expect(screen.getByText(/no longer recoverable/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Undo/i })).toBeNull();
  });

  it("renders nothing in Undo cell for non-snapshot actions", async () => {
    installFetchMock([
      entry({
        id: 33,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "a1",
        diff: JSON.stringify({ plan: "p", task: "1.1" }),
      }),
    ]);

    render(<AuditLog />);

    await waitFor(() => {
      expect(screen.getByText(/Started agent/i)).toBeTruthy();
    });
    expect(screen.queryByRole("button", { name: /Undo/i })).toBeNull();
    expect(screen.queryByText(/no longer recoverable/i)).toBeNull();
  });

  it("clicking Undo posts to /api/snapshots/<id>/restore and hides the button", async () => {
    const fetchMock = installFetchMock(
      [
        entry({
          id: 40,
          action: "plan.delete",
          resourceType: "plan",
          resourceId: "obsolete",
          diff: JSON.stringify({ snapshot_id: 7 }),
          snapshotId: 7,
          recoverable: true,
        }),
      ],
      {
        7: {
          status: 200,
          body: {
            ok: true,
            plan: "obsolete",
            snapshotId: 7,
            restoredAt: "2026-05-05 03:00:00",
          },
        },
      },
    );

    render(<AuditLog />);

    const button = await screen.findByRole("button", { name: /Undo/i });
    fireEvent.click(button);

    await waitFor(() => {
      expect(screen.queryByRole("button", { name: /Undo/i })).toBeNull();
    });
    // Restored annotation appears in place of the button — match the
    // cell's relative time so the assertion ignores the unrelated
    // "Restored plan" dropdown option.
    expect(screen.getByText(/Restored\s+(just now|\d+[mhd] ago)/i))
      .toBeTruthy();

    // Verify the POST landed at the right URL.
    const calls = fetchMock.mock.calls.map((c) => {
      const input = c[0] as string | URL | Request;
      return typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname
          : input.url;
    });
    expect(calls.some((u) => u.includes("/api/snapshots/7/restore"))).toBe(
      true,
    );

    // Toast confirms the restore.
    expect(usePlanStore.getState().toasts.length).toBeGreaterThan(0);
    expect(
      usePlanStore.getState().toasts.some((t) => /Restored/i.test(t.message)),
    ).toBe(true);
  });

  it("on 410 already_restored, hides Undo and shows inline diagnostic", async () => {
    installFetchMock(
      [
        entry({
          id: 41,
          action: "plan.delete",
          resourceType: "plan",
          resourceId: "raced",
          diff: JSON.stringify({ snapshot_id: 8 }),
          snapshotId: 8,
          recoverable: true,
        }),
      ],
      {
        8: {
          status: 410,
          body: {
            error: "snapshot_already_restored",
            restored_at: "2026-05-05 02:00:00",
          },
        },
      },
    );

    render(<AuditLog />);

    const button = await screen.findByRole("button", { name: /Undo/i });
    fireEvent.click(button);

    await waitFor(() => {
      expect(screen.queryByRole("button", { name: /Undo/i })).toBeNull();
    });
    // Cell flips to "Restored <relative ts>" (server-supplied
    // restored_at). The dropdown option "Restored plan" is excluded by
    // the trailing time anchor.
    expect(screen.getByText(/Restored\s+(just now|\d+[mhd] ago)/i))
      .toBeTruthy();
  });

  it("on 409 slug_collision, keeps Undo button and shows the collision inline", async () => {
    installFetchMock(
      [
        entry({
          id: 42,
          action: "plan.delete",
          resourceType: "plan",
          resourceId: "twin",
          diff: JSON.stringify({ snapshot_id: 9 }),
          snapshotId: 9,
          recoverable: true,
        }),
      ],
      {
        9: {
          status: 409,
          body: { error: "slug_collision", current: "title: Twin Plan" },
        },
      },
    );

    render(<AuditLog />);

    const button = await screen.findByRole("button", { name: /Undo/i });
    fireEvent.click(button);

    await waitFor(() => {
      expect(screen.getByText(/name in use/i)).toBeTruthy();
    });
    // Button remains so the user can retry after fixing the collision.
    expect(screen.getByRole("button", { name: /Undo/i })).toBeTruthy();
    expect(screen.getByText(/Twin Plan/)).toBeTruthy();
  });
});

describe("AuditLog URL-driven filter state", () => {
  beforeEach(() => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com", orgId: "default-org" },
      loading: false,
      error: null,
    });
    useWsStore.setState({ connected: false } as never);
    // Plan dropdown reads from plan-store; seed two plans so the
    // `plan` URL param has a matching <option>.
    usePlanStore.setState({
      plans: [
        {
          name: "alpha",
          title: "Alpha",
          project: null,
          phaseCount: 0,
          taskCount: 0,
          doneCount: 0,
          createdAt: "",
          modifiedAt: "",
        },
        {
          name: "beta",
          title: "Beta",
          project: null,
          phaseCount: 0,
          taskCount: 0,
          doneCount: 0,
          createdAt: "",
          modifiedAt: "",
        },
      ],
    });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("reads `kind` from URL and sends it as `action` to the server", async () => {
    const fetchMock = installFetchMock([
      entry({
        id: 60,
        action: "auto_mode.paused",
        resourceType: "plan",
        resourceId: "alpha",
        diff: JSON.stringify({ plan: "alpha", task: "1.2", reason: "x" }),
      }),
    ]);

    render(<AuditLog />, {
      initialEntries: ["/audit?kind=auto_mode.paused"],
    });

    // The action <select> reflects the URL value.
    await waitFor(() => {
      const sel = screen.getByLabelText(/Filter by action/i) as HTMLSelectElement;
      expect(sel.value).toBe("auto_mode.paused");
    });
    // The fetch went out with `action=auto_mode.paused`.
    const calls = fetchMock.mock.calls.map((c) => {
      const input = c[0] as string | URL | Request;
      return typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    });
    expect(
      calls.some((u) => u.includes("/audit-log") && u.includes("action=auto_mode.paused")),
    ).toBe(true);
  });

  it("filters by `plan` client-side (matches resourceId or diff.plan)", async () => {
    installFetchMock([
      entry({
        id: 70,
        action: "plan.create",
        resourceType: "plan",
        resourceId: "alpha",
        diff: JSON.stringify({}),
      }),
      entry({
        id: 71,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "ag-x",
        diff: JSON.stringify({ plan: "alpha", task: "1.1" }),
      }),
      entry({
        id: 72,
        action: "plan.create",
        resourceType: "plan",
        resourceId: "beta",
        diff: JSON.stringify({}),
      }),
    ]);

    render(<AuditLog />, { initialEntries: ["/audit?plan=alpha"] });

    await waitFor(() => {
      expect(screen.getByText(/Started agent/i)).toBeTruthy();
    });
    // Both alpha rows are visible (one by resourceId, one by diff.plan).
    expect(screen.getAllByText("alpha").length).toBeGreaterThan(0);
    // The beta row is filtered out.
    expect(screen.queryByText("beta")).toBeNull();
    // Hint surfaces the gap between server total and visible rows.
    expect(screen.getByText(/visible on this page/i)).toBeTruthy();
  });

  it("filters by `q` substring across the row", async () => {
    installFetchMock([
      entry({
        id: 80,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "ag-keep",
        userEmail: "alice@example.com",
        diff: JSON.stringify({ plan: "p", task: "1.1" }),
      }),
      entry({
        id: 81,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "ag-drop",
        userEmail: "bob@example.com",
        diff: JSON.stringify({ plan: "p", task: "1.2" }),
      }),
    ]);

    render(<AuditLog />, { initialEntries: ["/audit?q=alice"] });

    await waitFor(() => {
      expect(screen.getByText("alice@example.com")).toBeTruthy();
    });
    expect(screen.queryByText("bob@example.com")).toBeNull();
  });

  it("Clear button removes every filter param from the URL", async () => {
    installFetchMock([
      entry({
        id: 90,
        action: "plan.create",
        resourceType: "plan",
        resourceId: "alpha",
        diff: JSON.stringify({ plan: "alpha" }),
      }),
    ]);

    render(<AuditLog />, {
      initialEntries: ["/audit?kind=plan.create&plan=alpha&q=alpha"],
    });

    const clearBtn = await screen.findByRole("button", { name: /^Clear$/ });
    fireEvent.click(clearBtn);

    // After clearing, the action filter snaps back to "All actions" and the
    // search input empties.
    await waitFor(() => {
      const sel = screen.getByLabelText(/Filter by action/i) as HTMLSelectElement;
      expect(sel.value).toBe("");
    });
    const q = screen.getByLabelText(/Search audit log/i) as HTMLInputElement;
    expect(q.value).toBe("");
    // The Clear button itself disappears once no filter is active.
    expect(screen.queryByRole("button", { name: /^Clear$/ })).toBeNull();
  });

  it("typing in search input round-trips through URL state (controlled by useSearchParams)", async () => {
    installFetchMock([
      entry({
        id: 100,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "ag-keep",
        diff: JSON.stringify({ plan: "p", task: "1.1" }),
      }),
      entry({
        id: 101,
        action: "agent.start",
        resourceType: "agent",
        resourceId: "ag-other",
        diff: JSON.stringify({ plan: "p", task: "1.2" }),
      }),
    ]);

    render(<AuditLog />, { initialEntries: ["/audit"] });

    const q = (await screen.findByLabelText(
      /Search audit log/i,
    )) as HTMLInputElement;
    fireEvent.change(q, { target: { value: "ag-keep" } });

    // The input value comes from `searchParams.get("q") ?? ""`, so it can
    // only retain the typed value if the change handler wrote it back to
    // searchParams (i.e. the URL). This proves the round-trip without
    // depending on MemoryRouter exposing window.location.
    await waitFor(() => {
      const after = screen.getByLabelText(
        /Search audit log/i,
      ) as HTMLInputElement;
      expect(after.value).toBe("ag-keep");
    });
    // Client-side narrowing kicks in immediately.
    expect(screen.queryByText("ag-other")).toBeNull();
    expect(screen.getByText(/visible on this page/i)).toBeTruthy();
  });
});
