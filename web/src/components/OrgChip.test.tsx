import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { OrgChip } from "./OrgChip.js";
import { useOrgStore, type OrgMembership } from "../stores/org-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useAuthStore } from "../stores/auth-store.js";

function membership(overrides: Partial<OrgMembership> = {}): OrgMembership {
  return {
    id: "org-1",
    name: "Acme",
    slug: "acme",
    role: "owner",
    memberCount: 3,
    ...overrides,
  };
}

beforeEach(() => {
  // Default to a SaaS deployment with one runner-store loaded so the
  // gate behind RunnerStatus mode-discriminator allows the chip to
  // render. Each test overrides as needed.
  useRunnerStore.setState({
    mode: "saas",
    runners: [],
    loaded: true,
    lastRunnersFetchedAt: Date.now(),
  });
  useAuthStore.setState({
    user: { id: "u-1", email: "u@x.com", orgId: "org-1" },
    loading: false,
    error: null,
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  useOrgStore.setState({
    memberships: [],
    loaded: false,
    lastFetchedAt: null,
  });
  useRunnerStore.setState({
    mode: "unknown",
    runners: [],
    loaded: false,
    lastRunnersFetchedAt: null,
  });
  useAuthStore.setState({ user: null, loading: false, error: null });
});

function renderChip() {
  return render(
    <MemoryRouter>
      <OrgChip />
    </MemoryRouter>,
  );
}

describe("OrgChip", () => {
  it("renders nothing on standalone deployments", () => {
    useRunnerStore.setState({ mode: "standalone", loaded: true });
    useOrgStore.setState({
      memberships: [membership()],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    const { container } = renderChip();
    expect(container.querySelector('[data-testid="org-chip"]')).toBeNull();
  });

  it("renders nothing while runner-store is still loading", () => {
    useRunnerStore.setState({ mode: "unknown", loaded: false });
    useOrgStore.setState({
      memberships: [membership()],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    const { container } = renderChip();
    expect(container.querySelector('[data-testid="org-chip"]')).toBeNull();
  });

  it("renders nothing while org-store is still loading", () => {
    useOrgStore.setState({
      memberships: [],
      loaded: false,
      lastFetchedAt: null,
    });
    const { container } = renderChip();
    expect(container.querySelector('[data-testid="org-chip"]')).toBeNull();
  });

  it("renders nothing when the user has zero memberships", () => {
    useOrgStore.setState({
      memberships: [],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    const { container } = renderChip();
    expect(container.querySelector('[data-testid="org-chip"]')).toBeNull();
  });

  it("renders org name + member count for a single-org user (no switcher)", () => {
    useOrgStore.setState({
      memberships: [membership({ name: "Acme", memberCount: 3 })],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    renderChip();
    const chip = screen.getByTestId("org-chip");
    expect(chip.getAttribute("data-can-switch")).toBe("false");
    expect(chip.textContent ?? "").toMatch(/Acme/);
    const count = screen.getByTestId("org-chip-member-count");
    expect(count.textContent ?? "").toBe("3 members");
    expect(count.getAttribute("href")).toBe("/admin/members");
  });

  it("uses the singular 'member' label for a 1-member org", () => {
    useOrgStore.setState({
      memberships: [membership({ memberCount: 1 })],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    renderChip();
    expect(screen.getByTestId("org-chip-member-count").textContent).toBe("1 member");
  });

  it("renders the switcher button when the user has multiple orgs", () => {
    useOrgStore.setState({
      memberships: [
        membership({ id: "org-1", name: "Acme" }),
        membership({ id: "org-2", name: "Beta", memberCount: 5 }),
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    renderChip();
    const chip = screen.getByTestId("org-chip");
    expect(chip.getAttribute("data-can-switch")).toBe("true");
    // Dropdown is closed by default — only the trigger and the count chip
    // are present.
    expect(screen.queryByTestId("org-chip-menu")).toBeNull();
  });

  it("opens the dropdown on click and lists other orgs", () => {
    useOrgStore.setState({
      memberships: [
        membership({ id: "org-1", name: "Acme" }),
        membership({ id: "org-2", name: "Beta", memberCount: 5 }),
        membership({ id: "org-3", name: "Gamma", memberCount: 2 }),
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    renderChip();
    fireEvent.click(screen.getByRole("button", { name: /Acme/ }));
    const menu = screen.getByTestId("org-chip-menu");
    expect(menu).toBeTruthy();
    // Current org is NOT in the dropdown — only "others"
    expect(menu.textContent ?? "").toMatch(/Beta/);
    expect(menu.textContent ?? "").toMatch(/Gamma/);
    expect(menu.textContent ?? "").not.toMatch(/Acme/);
  });

  it("calls switchOrg when an entry in the dropdown is clicked", async () => {
    const switchOrg = vi.fn(async (_id: string) => undefined);
    useOrgStore.setState({
      memberships: [
        membership({ id: "org-1", name: "Acme" }),
        membership({ id: "org-2", name: "Beta", memberCount: 5 }),
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
      switchOrg,
    });
    renderChip();
    fireEvent.click(screen.getByRole("button", { name: /Acme/ }));
    const betaButton = screen.getByRole("menuitem", { name: /Beta/ });
    await act(async () => {
      fireEvent.click(betaButton);
    });
    expect(switchOrg).toHaveBeenCalledWith("org-2");
  });

  it("closes the dropdown on outside click", () => {
    useOrgStore.setState({
      memberships: [
        membership({ id: "org-1", name: "Acme" }),
        membership({ id: "org-2", name: "Beta", memberCount: 5 }),
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    const { container } = renderChip();
    fireEvent.click(screen.getByRole("button", { name: /Acme/ }));
    expect(screen.getByTestId("org-chip-menu")).toBeTruthy();
    // Click somewhere outside the chip container.
    act(() => {
      fireEvent.mouseDown(container);
    });
    expect(screen.queryByTestId("org-chip-menu")).toBeNull();
  });

  it("falls back to the first membership when user.orgId doesn't match any membership", () => {
    // Edge: localStorage was just cleared by reset; user.orgId is null
    // until /api/auth/me lands. Chip must still render rather than
    // showing nothing. Picks the first membership as the active one.
    useAuthStore.setState({
      user: { id: "u-1", email: "u@x.com", orgId: undefined },
    });
    useOrgStore.setState({
      memberships: [
        membership({ id: "org-1", name: "Acme" }),
        membership({ id: "org-2", name: "Beta" }),
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    renderChip();
    expect(screen.getByTestId("org-chip").textContent ?? "").toMatch(/Acme/);
  });
});
