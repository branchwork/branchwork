import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  screen,
  waitFor,
} from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { ArchivePanel } from "./ArchivePanel.js";
import { useAuthStore } from "../stores/auth-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useWsStore } from "../stores/ws-store.js";

interface MockSnapshot {
  id: number;
  planName: string;
  kind: string;
  createdAt: string;
  expiresAt: string;
  archivePath: string | null;
  restoredAt: string | null;
}

function snap(overrides: Partial<MockSnapshot> = {}): MockSnapshot {
  return {
    id: 1,
    planName: "demo-plan",
    kind: "delete",
    createdAt: new Date(Date.now() - 60_000).toISOString().slice(0, 19),
    expiresAt: new Date(Date.now() + 4 * 86_400_000)
      .toISOString()
      .slice(0, 19),
    archivePath: "/plans/archive/demo-plan.20260505T030000Z.yaml",
    restoredAt: null,
    ...overrides,
  };
}

interface CallLog {
  url: string;
  method: string;
}

function installFetchMock(
  initial: MockSnapshot[],
  options: {
    /// Statuses to return for the next DELETE; popped in order.
    deleteStatuses?: number[];
  } = {},
): { calls: CallLog[]; fn: ReturnType<typeof vi.fn> } {
  const calls: CallLog[] = [];
  const deleteStatuses = options.deleteStatuses ?? [200];
  const fn = vi.fn(
    async (input: string | URL | Request, init?: RequestInit) => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.pathname + input.search
            : input.url;
      const method = init?.method ?? "GET";
      calls.push({ url, method });
      if (url === "/api/snapshots" && method === "GET") {
        return new Response(
          JSON.stringify({ snapshots: initial }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      const purgeMatch = url.match(/^\/api\/snapshots\/(\d+)$/);
      if (purgeMatch && method === "DELETE") {
        const id = Number(purgeMatch[1]);
        const status = deleteStatuses.shift() ?? 200;
        if (status === 200) {
          return new Response(
            JSON.stringify({ ok: true, snapshotId: id, plan: "demo-plan" }),
            { status: 200, headers: { "Content-Type": "application/json" } },
          );
        }
        return new Response(
          JSON.stringify({ error: "snapshot_not_found" }),
          {
            status,
            statusText: status === 404 ? "Not Found" : "Forbidden",
            headers: { "Content-Type": "application/json" },
          },
        );
      }
      const restoreMatch = url.match(/^\/api\/snapshots\/(\d+)\/restore$/);
      if (restoreMatch && method === "POST") {
        const id = Number(restoreMatch[1]);
        return new Response(
          JSON.stringify({
            ok: true,
            plan: "demo-plan",
            snapshotId: id,
            restoredAt: "2026-05-05 03:00:00",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({}), { status: 404 });
    },
  );
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

describe("ArchivePanel", () => {
  beforeEach(() => {
    useAuthStore.setState({
      user: { id: "u1", email: "u@example.com", orgId: "default-org" },
      loading: false,
      error: null,
    });
    useWsStore.setState({ connected: false } as never);
    // Reset toast state so assertions don't bleed across tests.
    usePlanStore.setState({ toasts: [] });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("lists snapshots with countdown chip", async () => {
    installFetchMock([
      snap({ id: 7, planName: "alpha" }),
      snap({
        id: 8,
        planName: "beta",
        expiresAt: new Date(Date.now() - 60_000).toISOString().slice(0, 19),
      }),
    ]);
    render(<ArchivePanel />);
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(await screen.findByText("beta")).toBeTruthy();
    expect(await screen.findByText(/expires in 3d/)).toBeTruthy();
    expect(await screen.findByText("expired")).toBeTruthy();
  });

  it("renders empty state when no snapshots present", async () => {
    installFetchMock([]);
    render(<ArchivePanel />);
    expect(
      await screen.findByText(
        "No snapshots in retention. Soft-deleted plans show up here.",
      ),
    ).toBeTruthy();
  });

  it("filters by kind and plan name", async () => {
    installFetchMock([
      snap({ id: 11, planName: "alpha", kind: "delete" }),
      snap({ id: 12, planName: "beta", kind: "rename" }),
      snap({ id: 13, planName: "alpha-2", kind: "delete" }),
    ]);
    render(<ArchivePanel />);
    await screen.findByText("alpha");

    fireEvent.change(screen.getByPlaceholderText("Search plan name..."), {
      target: { value: "alpha" },
    });
    expect(screen.queryByText("beta")).toBeNull();
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText("alpha-2")).toBeTruthy();

    fireEvent.change(screen.getByPlaceholderText("Search plan name..."), {
      target: { value: "" },
    });
    fireEvent.change(screen.getByDisplayValue("All kinds"), {
      target: { value: "rename" },
    });
    expect(screen.queryByText("alpha")).toBeNull();
    expect(screen.getByText("beta")).toBeTruthy();
  });

  it("requires a second confirm before purging", async () => {
    const { calls } = installFetchMock([snap({ id: 42, planName: "demo" })]);
    render(<ArchivePanel />);
    await screen.findByText("demo");

    fireEvent.click(screen.getByText("Purge now"));

    // Modal copy is the gate — no DELETE has fired yet.
    expect(
      screen.getByText("Purge snapshot for demo?"),
    ).toBeTruthy();
    expect(calls.filter((c) => c.method === "DELETE")).toHaveLength(0);

    // Cancel returns to the list with no DELETE.
    fireEvent.click(screen.getByText("Cancel"));
    expect(screen.queryByText("Purge snapshot for demo?")).toBeNull();
    expect(calls.filter((c) => c.method === "DELETE")).toHaveLength(0);

    // Reopen and confirm — DELETE fires once.
    fireEvent.click(screen.getByText("Purge now"));
    fireEvent.click(screen.getByText("Purge permanently"));
    await waitFor(() => {
      expect(calls.filter((c) => c.method === "DELETE")).toHaveLength(1);
    });
    expect(calls[calls.length - 1]).toEqual({
      url: "/api/snapshots/42",
      method: "DELETE",
    });

    // Row is optimistically removed from the table.
    await waitFor(() => {
      expect(screen.queryByText("demo")).toBeNull();
    });
  });

  it("hides Restore button after a successful restore", async () => {
    installFetchMock([snap({ id: 99, planName: "rstore" })]);
    render(<ArchivePanel />);
    await screen.findByText("rstore");

    fireEvent.click(screen.getByText("Restore"));
    await waitFor(() => {
      // The cell flips to a "Restored …" indicator and the Restore
      // button becomes disabled (no more onClick targets matching).
      expect(screen.getByText(/Restored just now|Restored \dm ago/)).toBeTruthy();
    });
    const btn = screen.getByText("Restore") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });
});
