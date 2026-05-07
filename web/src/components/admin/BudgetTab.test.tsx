import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { BudgetTab } from "./BudgetTab.js";

interface Call {
  url: string;
  method: string;
  body: unknown;
}

let calls: Call[];
let budgetResponse: unknown = {
  budget: {
    orgId: "org-1",
    maxBudgetUsd: 100,
    billingPeriod: "monthly",
    periodStart: null,
    updatedAt: "2026-01-01T00:00:00Z",
  },
  killSwitchActive: false,
};
const usageResponse = {
  summary: {
    orgId: "org-1",
    periodKey: "2026-04",
    totalCostUsd: 42,
    maxBudgetUsd: 100,
    pctUsed: 0.42,
    killSwitchActive: false,
  },
  users: [
    {
      userId: "user-1",
      email: "alice@example.com",
      costUsd: 30,
      quotaUsd: 50,
    },
    {
      userId: "user-2",
      email: "bob@example.com",
      costUsd: 12,
      quotaUsd: null,
    },
  ],
};

beforeEach(() => {
  calls = [];
  budgetResponse = {
    budget: {
      orgId: "org-1",
      maxBudgetUsd: 100,
      billingPeriod: "monthly",
      periodStart: null,
      updatedAt: "2026-01-01T00:00:00Z",
    },
    killSwitchActive: false,
  };
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
      return new Response(JSON.stringify(budgetResponse), { status: 200 });
    }
    if (url === "/api/orgs/acme/usage" && method === "GET") {
      return new Response(JSON.stringify(usageResponse), { status: 200 });
    }
    if (url === "/api/orgs/acme/budget" && method === "PUT") {
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }
    return new Response("not found", { status: 404 });
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("BudgetTab", () => {
  it("renders the seeded budget + usage bar at the right percentage", async () => {
    render(<BudgetTab orgSlug="acme" />);
    await waitFor(() =>
      expect(screen.getByTestId("budget-input")).toBeTruthy(),
    );
    expect(
      (screen.getByTestId("budget-input") as HTMLInputElement).value,
    ).toBe("100");
    expect(screen.getByTestId("budget-spent").textContent).toContain("$42.00");
    expect(screen.getByTestId("budget-pct").textContent).toContain("42%");
    expect(screen.getByTestId("budget-bar").getAttribute("aria-valuenow")).toBe(
      "42",
    );
  });

  it("Save PUTs camelCase maxBudgetUsd as a number", async () => {
    render(<BudgetTab orgSlug="acme" />);
    await waitFor(() =>
      expect(screen.getByTestId("budget-input")).toBeTruthy(),
    );
    fireEvent.change(screen.getByTestId("budget-input"), {
      target: { value: "250" },
    });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => {
      const put = calls.find(
        (c) => c.url === "/api/orgs/acme/budget" && c.method === "PUT",
      );
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({ maxBudgetUsd: 250 });
    });
  });

  it("clearing the input PUTs maxBudgetUsd: null", async () => {
    render(<BudgetTab orgSlug="acme" />);
    await waitFor(() =>
      expect(screen.getByTestId("budget-input")).toBeTruthy(),
    );
    fireEvent.change(screen.getByTestId("budget-input"), {
      target: { value: "" },
    });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() => {
      const put = calls.find(
        (c) => c.url === "/api/orgs/acme/budget" && c.method === "PUT",
      );
      expect(put).toBeTruthy();
      expect(put?.body).toEqual({ maxBudgetUsd: null });
    });
  });

  it("renders the per-user usage breakdown", async () => {
    render(<BudgetTab orgSlug="acme" />);
    await waitFor(() =>
      expect(screen.getByTestId("usage-by-user")).toBeTruthy(),
    );
    expect(screen.getByText("alice@example.com")).toBeTruthy();
    expect(screen.getByText("bob@example.com")).toBeTruthy();
    expect(screen.getByText("$30.00")).toBeTruthy();
    expect(screen.getByText("/ $50.00")).toBeTruthy();
    expect(screen.getByText("shared cap")).toBeTruthy();
  });

  it("hides the percentage badge when no budget is set", async () => {
    budgetResponse = { budget: null, killSwitchActive: false };
    render(<BudgetTab orgSlug="acme" />);
    await waitFor(() =>
      expect(screen.getByTestId("budget-input")).toBeTruthy(),
    );
    expect(screen.queryByTestId("budget-pct")).toBeNull();
    expect(screen.getByText(/no cap/i)).toBeTruthy();
  });
});
