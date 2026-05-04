import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { AuditLog } from "./AuditLog.js";
import { useAuthStore } from "../stores/auth-store.js";
import { useWsStore } from "../stores/ws-store.js";

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
    ...o,
  };
}

function installFetchMock(entries: MockEntry[]) {
  const fn = vi.fn(async (input: string | URL | Request) => {
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
