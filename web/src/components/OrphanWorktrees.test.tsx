import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { OrphanWorktrees } from "./OrphanWorktrees.js";
import { useOrgStore, type OrgMembership } from "../stores/org-store.js";
import { useAuthStore } from "../stores/auth-store.js";
import { useToastStore } from "../stores/toast-store.js";

const SLUG = "personal-abc";

const ORPHAN = {
  project_slug: "branchwork",
  path: "/home/x/.branchwork/worktrees/branchwork/my-plan/0.1-abcd1234",
  branch: "branchwork/my-plan/0.1",
  last_modified: "2026-06-01 10:00:00",
  first_detected: "2026-06-01 10:05:00",
};

interface Call {
  path: string;
  method: string;
  body: string | null;
}

/// Route GET list / POST cleanup by path+method. The list returns
/// `listOrphans` (mutable so a post-cleanup refetch can return fewer rows);
/// cleanup returns `cleanupResult`.
function installMock(): {
  calls: Call[];
  setList: (orphans: unknown[]) => void;
  setCleanup: (result: Record<string, unknown>) => void;
} {
  const calls: Call[] = [];
  let listOrphans: unknown[] = [ORPHAN];
  let cleanupResult: Record<string, unknown> = { removed: [ORPHAN.path], failed: [] };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      const method = init?.method ?? "GET";
      calls.push({ path, method, body: (init?.body as string) ?? null });
      if (path.endsWith("/orphan-worktrees/cleanup")) {
        return new Response(JSON.stringify(cleanupResult), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (path.endsWith("/orphan-worktrees")) {
        return new Response(JSON.stringify({ orphans: listOrphans }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
    }),
  );
  return {
    calls,
    setList: (orphans) => {
      listOrphans = orphans;
    },
    setCleanup: (result) => {
      cleanupResult = result;
    },
  };
}

function seedOrg(role: string) {
  const m: OrgMembership = {
    id: "org-1",
    name: "Personal",
    slug: SLUG,
    role,
    memberCount: 1,
  };
  useOrgStore.setState({ memberships: [m], loaded: true });
  useAuthStore.setState({ user: { id: "u1", email: "a@b.c", orgId: "org-1" } });
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn());
  useToastStore.getState().clear();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  useOrgStore.setState({ memberships: [], loaded: false });
  useAuthStore.setState({ user: null });
  useToastStore.getState().clear();
});

describe("OrphanWorktrees", () => {
  it("renders nothing for a non-admin user", async () => {
    installMock();
    seedOrg("member");
    render(<OrphanWorktrees />);
    // A member has no admin org → section hidden, no fetch issued.
    await waitFor(() => {
      expect(screen.queryByTestId("orphan-worktrees")).toBeNull();
    });
  });

  it("renders nothing when the orphan list is empty", async () => {
    const m = installMock();
    m.setList([]);
    seedOrg("owner");
    render(<OrphanWorktrees />);
    await waitFor(() => {
      expect(m.calls.some((c) => c.path.endsWith("/orphan-worktrees"))).toBe(true);
    });
    expect(screen.queryByTestId("orphan-worktrees")).toBeNull();
  });

  it("lists orphans with branch + last-modified for an admin", async () => {
    installMock();
    seedOrg("owner");
    render(<OrphanWorktrees />);
    expect(await screen.findByTestId("orphan-worktrees")).toBeTruthy();
    expect(screen.getByText("Worktree orphans (1)")).toBeTruthy();
    // Branch label present (last-modified relative text is colocated).
    expect(screen.getByText(/branchwork\/my-plan\/0\.1/)).toBeTruthy();
    expect(screen.getByText("Cleanup all")).toBeTruthy();
    expect(screen.getByLabelText(`Remove orphan worktree ${ORPHAN.path}`)).toBeTruthy();
  });

  it("shows a confirmation dialog before a per-row remove and POSTs the subset", async () => {
    const m = installMock();
    seedOrg("owner");
    render(<OrphanWorktrees />);
    await screen.findByTestId("orphan-worktrees");

    fireEvent.click(screen.getByLabelText(`Remove orphan worktree ${ORPHAN.path}`));
    // Confirmation dialog appears; no cleanup POST yet.
    expect(await screen.findByRole("dialog")).toBeTruthy();
    expect(m.calls.some((c) => c.method === "POST")).toBe(false);

    // After the refetch, the list is empty.
    m.setList([]);
    const dialog = screen.getByRole("dialog");
    fireEvent.click(within(dialog).getByText("Remove"));

    await waitFor(() => {
      const post = m.calls.find((c) => c.method === "POST");
      expect(post).toBeTruthy();
      expect(post!.path.endsWith("/orphan-worktrees/cleanup")).toBe(true);
      expect(JSON.parse(post!.body!)).toEqual({ paths: [ORPHAN.path] });
    });
    // List refetched and now empty → section gone.
    await waitFor(() => {
      expect(screen.queryByTestId("orphan-worktrees")).toBeNull();
    });
  });

  it("Cleanup all confirms then POSTs an empty body (clean-all)", async () => {
    const m = installMock();
    m.setCleanup({ removed: [ORPHAN.path], failed: [] });
    seedOrg("owner");
    render(<OrphanWorktrees />);
    await screen.findByTestId("orphan-worktrees");

    fireEvent.click(screen.getByText("Cleanup all"));
    const dialog = await screen.findByRole("dialog");
    m.setList([]);
    fireEvent.click(within(dialog).getByText("Remove"));

    await waitFor(() => {
      const post = m.calls.find((c) => c.method === "POST");
      expect(post).toBeTruthy();
      // Omitting `paths` ⇒ clean all.
      expect(JSON.parse(post!.body!)).toEqual({});
    });
  });

  it("Cancel in the dialog issues no cleanup POST", async () => {
    const m = installMock();
    seedOrg("owner");
    render(<OrphanWorktrees />);
    await screen.findByTestId("orphan-worktrees");

    fireEvent.click(screen.getByText("Cleanup all"));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(within(dialog).getByText("Cancel"));

    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(m.calls.some((c) => c.method === "POST")).toBe(false);
  });
});
