import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { KillSwitchTab } from "./KillSwitchTab.js";

interface Call {
  url: string;
  method: string;
  body: unknown;
}

let calls: Call[];
let killState = false;

beforeEach(() => {
  calls = [];
  killState = false;
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
    if (url === "/api/orgs/acme/budget" && method === "GET") {
      return new Response(JSON.stringify({ budget: null, killSwitchActive: killState }), {
        status: 200,
      });
    }
    if (url === "/api/orgs/acme/kill-switch" && method === "PUT") {
      killState = (body as { active: boolean }).active;
      return new Response(JSON.stringify({ ok: true, active: killState }), {
        status: 200,
      });
    }
    return new Response("not found", { status: 404 });
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("KillSwitchTab", () => {
  it("activate sends {active: true, reason} and shows active state", async () => {
    render(<KillSwitchTab orgSlug="acme" />);
    await waitFor(() => expect(screen.getByTestId("kill-state")).toBeTruthy());
    expect(screen.getByTestId("kill-state").textContent).toMatch(/Inactive/i);
    fireEvent.change(screen.getByTestId("kill-reason"), {
      target: { value: "PoC blew the budget" },
    });
    fireEvent.click(screen.getByTestId("kill-activate"));
    await waitFor(() => {
      const put = calls.find((c) => c.url === "/api/orgs/acme/kill-switch" && c.method === "PUT");
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({
        active: true,
        reason: "PoC blew the budget",
      });
    });
    await waitFor(() => expect(screen.getByTestId("kill-state").textContent).toMatch(/Active/));
  });

  it("activate with empty reason sends reason: null (no empty-string)", async () => {
    render(<KillSwitchTab orgSlug="acme" />);
    await waitFor(() => expect(screen.getByTestId("kill-state")).toBeTruthy());
    fireEvent.click(screen.getByTestId("kill-activate"));
    await waitFor(() => {
      const put = calls.find((c) => c.url === "/api/orgs/acme/kill-switch" && c.method === "PUT");
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({ active: true, reason: null });
    });
  });

  it("deactivate sends {active: false, reason: null}", async () => {
    killState = true;
    render(<KillSwitchTab orgSlug="acme" />);
    await waitFor(() => expect(screen.getByTestId("kill-state")).toBeTruthy());
    fireEvent.click(screen.getByTestId("kill-deactivate"));
    await waitFor(() => {
      const put = calls.find((c) => c.url === "/api/orgs/acme/kill-switch" && c.method === "PUT");
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({ active: false, reason: null });
    });
  });
});
