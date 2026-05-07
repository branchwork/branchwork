import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MembersTab } from "./MembersTab.js";

interface FetchCall {
  url: string;
  method: string;
  body: unknown;
}

const ORG = {
  id: "org-1",
  name: "Acme",
  slug: "acme",
  createdAt: "2026-01-01T00:00:00Z",
  members: [
    {
      userId: "user-1",
      email: "owner@example.com",
      role: "owner",
      joinedAt: "2026-01-01T00:00:00Z",
    },
    {
      userId: "user-2",
      email: "alice@example.com",
      role: "member",
      joinedAt: "2026-02-01T00:00:00Z",
    },
  ],
};

let calls: FetchCall[];

beforeEach(() => {
  calls = [];
  // Stub `fetch` per-test rather than mocking a store: api.ts wraps
  // fetch directly and there is no MembersStore to set state on.
  vi.stubGlobal("fetch", async (input: RequestInfo, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const method = init?.method ?? "GET";
    let body: unknown;
    try {
      body = init?.body ? JSON.parse(init.body as string) : undefined;
    } catch {
      body = init?.body;
    }
    calls.push({ url, method, body });
    if (url === "/api/orgs/acme" && method === "GET") {
      return new Response(JSON.stringify(ORG), { status: 200 });
    }
    if (url === "/api/orgs/acme/members" && method === "POST") {
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }
    if (
      url.startsWith("/api/orgs/acme/members/") &&
      url.endsWith("/role") &&
      method === "PUT"
    ) {
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }
    if (url.startsWith("/api/orgs/acme/members/") && method === "DELETE") {
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }
    return new Response("not found", { status: 404 });
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("MembersTab", () => {
  it("loads and renders members from /api/orgs/{slug}", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    expect(screen.getByText("owner@example.com")).toBeTruthy();
    expect(screen.getByText("alice@example.com")).toBeTruthy();
    // Caller's row is tagged "you"
    expect(screen.getByText(/^you$/i)).toBeTruthy();
  });

  it("invite POSTs trimmed lowercase email + selected role", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    fireEvent.change(screen.getByTestId("invite-email"), {
      target: { value: "  Bob@Example.com  " },
    });
    fireEvent.change(screen.getByTestId("invite-role"), {
      target: { value: "admin" },
    });
    fireEvent.click(screen.getByText("Invite"));
    await waitFor(() => {
      const post = calls.find(
        (c) => c.url === "/api/orgs/acme/members" && c.method === "POST",
      );
      expect(post).toBeTruthy();
      expect(post?.body).toEqual({ email: "bob@example.com", role: "admin" });
    });
  });

  it("owner gets a role picker; non-owner gets a static role label", async () => {
    const { rerender } = render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    // alice's role rendered as a select for the owner caller
    const aliceRole = screen.getByTestId("role-user-2") as HTMLElement;
    expect(aliceRole.tagName).toBe("SELECT");

    rerender(
      <MembersTab orgSlug="acme" callerRole="admin" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    // Admins (non-owner) cannot reassign roles → static text node
    const aliceRole2 = screen.getByTestId("role-user-2") as HTMLElement;
    expect(aliceRole2.tagName).not.toBe("SELECT");
  });

  it("changing role PUTs to /role endpoint with new role", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    fireEvent.change(screen.getByTestId("role-user-2"), {
      target: { value: "admin" },
    });
    await waitFor(() => {
      const put = calls.find(
        (c) =>
          c.url === "/api/orgs/acme/members/user-2/role" && c.method === "PUT",
      );
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({ role: "admin" });
    });
  });

  it("Remove button DELETEs the member endpoint", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    fireEvent.click(screen.getByTestId("remove-user-2"));
    await waitFor(() => {
      const del = calls.find(
        (c) =>
          c.url === "/api/orgs/acme/members/user-2" && c.method === "DELETE",
      );
      expect(del).toBeTruthy();
    });
  });

  it("hides invite form for non-admin callers (read-only)", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="member" callerUserId="user-2" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    expect(screen.queryByTestId("invite-email")).toBeNull();
    // Self-row still gets a Leave button (member can leave themselves).
    expect(screen.getByTestId("remove-user-2")).toBeTruthy();
    // No Remove button on the owner row.
    expect(screen.queryByTestId("remove-user-1")).toBeNull();
  });

  it("hides Remove on the only-owner row (UX guard)", async () => {
    render(
      <MembersTab orgSlug="acme" callerRole="owner" callerUserId="user-1" />,
    );
    await waitFor(() =>
      expect(screen.getByTestId("members-list")).toBeTruthy(),
    );
    // user-1 is the only owner → no Leave button shown
    expect(screen.queryByTestId("remove-user-1")).toBeNull();
  });
});
