import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render as rtlRender,
  screen,
  waitFor,
  type RenderOptions,
} from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import type { ReactElement } from "react";
import {
  AdminPage,
  isAdminRole,
  clampRetentionDays,
  retentionPreview,
} from "./AdminPage.js";
import { useSettingsStore } from "../stores/settings-store.js";
import { useRunnerStore } from "../stores/runner-store.js";
import { useOrgStore } from "../stores/org-store.js";
import { useAuthStore } from "../stores/auth-store.js";

/// Test render helper that mounts AdminPage under a MemoryRouter so
/// `useParams` / `useNavigate` resolve. Defaults to `/admin/settings`
/// so the Settings tab is the active one — that matches the pre-tabs
/// surface (effort/webhook/retention/skip-perms inputs always present).
/// Pass `initialEntry: "/admin/members"` to land on a different tab.
function render(
  ui: ReactElement,
  options: RenderOptions & { initialEntry?: string } = {},
) {
  const { initialEntry = "/admin/settings", ...rest } = options;
  return rtlRender(
    <MemoryRouter initialEntries={[initialEntry]}>
      <Routes>
        <Route path="/admin" element={ui} />
        <Route path="/admin/:section" element={ui} />
      </Routes>
    </MemoryRouter>,
    rest,
  );
}

beforeEach(() => {
  useSettingsStore.setState({
    effort: "high",
    skipPermissions: true,
    webhookUrl: null,
    planArchiveRetentionDays: 30,
    loaded: true,
    drivers: [],
    defaultDriver: "claude",
    setEffort: vi.fn().mockResolvedValue(undefined),
    setSkipPermissions: vi.fn().mockResolvedValue(undefined),
    setWebhookUrl: vi.fn().mockResolvedValue(undefined),
    setPlanArchiveRetentionDays: vi.fn().mockResolvedValue(undefined),
  });
  // Default to standalone deployment so the existing tests see only
  // the Settings tab (no tab bar). Each gating test below overrides
  // mode + memberships to opt into the org-scoped tabs.
  useRunnerStore.setState({
    mode: "standalone",
    runners: [],
    loaded: true,
    lastRunnersFetchedAt: Date.now(),
    driversByRunnerId: {},
    selectedRunnerId: null,
    configByRunnerId: {},
  });
  useOrgStore.setState({
    memberships: [],
    loaded: true,
    lastFetchedAt: Date.now(),
  });
  useAuthStore.setState({ user: null, loading: false, error: null });
});

afterEach(() => {
  cleanup();
});

describe("clampRetentionDays", () => {
  it("clamps above max to 365", () => {
    expect(clampRetentionDays("9999", 30)).toBe(365);
  });

  it("clamps below min to 0", () => {
    expect(clampRetentionDays("-5", 30)).toBe(0);
  });

  it("returns parsed value within range", () => {
    expect(clampRetentionDays("60", 30)).toBe(60);
  });

  it("falls back when input is non-numeric", () => {
    expect(clampRetentionDays("abc", 42)).toBe(42);
  });

  it("falls back when input is empty", () => {
    expect(clampRetentionDays("   ", 7)).toBe(7);
  });
});

describe("retentionPreview", () => {
  it("renders permanent copy at 0", () => {
    expect(retentionPreview(0)).toMatch(/permanent/i);
  });

  it("renders singular at 1", () => {
    expect(retentionPreview(1)).toBe(
      "Soft-deleted plans are kept for 1 day.",
    );
  });

  it("renders plural at 30", () => {
    expect(retentionPreview(30)).toBe(
      "Soft-deleted plans are kept for 30 days.",
    );
  });
});

describe("AdminPage retention input", () => {
  it("seeds the input from the store value", () => {
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    expect(input.value).toBe("30");
    expect(screen.getByTestId("retention-preview").textContent).toMatch(
      /kept for 30 days/i,
    );
  });

  it("updates the live preview chip as the user types", () => {
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "0" } });
    expect(screen.getByTestId("retention-preview").textContent).toMatch(
      /permanent/i,
    );
    fireEvent.change(input, { target: { value: "7" } });
    expect(screen.getByTestId("retention-preview").textContent).toMatch(
      /kept for 7 days/i,
    );
  });

  it("clamps 9999 to 365 on blur and saves", async () => {
    const setRetention = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ setPlanArchiveRetentionDays: setRetention });
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "9999" } });
    fireEvent.blur(input);
    await waitFor(() => expect(setRetention).toHaveBeenCalledWith(365));
    expect(input.value).toBe("365");
  });

  it("clamps -5 to 0 on blur and saves", async () => {
    const setRetention = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ setPlanArchiveRetentionDays: setRetention });
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "-5" } });
    fireEvent.blur(input);
    await waitFor(() => expect(setRetention).toHaveBeenCalledWith(0));
    expect(input.value).toBe("0");
  });

  it("does not save when blur produces the same value as the store", async () => {
    const setRetention = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ setPlanArchiveRetentionDays: setRetention });
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "30" } });
    fireEvent.blur(input);
    // Give the click handler a microtask tick.
    await Promise.resolve();
    expect(setRetention).not.toHaveBeenCalled();
  });

  it("surfaces an error message when the store action throws", async () => {
    const setRetention = vi
      .fn()
      .mockRejectedValue(new Error("network down"));
    useSettingsStore.setState({ setPlanArchiveRetentionDays: setRetention });
    render(<AdminPage />);
    const input = screen.getByTestId("retention-input") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "60" } });
    fireEvent.blur(input);
    await waitFor(() =>
      expect(screen.getByText(/network down/i)).toBeTruthy(),
    );
  });
});

describe("AdminPage default-effort buttons", () => {
  it("renders all four levels and marks the active one", () => {
    useSettingsStore.setState({ effort: "medium" });
    render(<AdminPage />);
    for (const label of ["Low", "Medium", "High", "Max"]) {
      expect(screen.getByText(label)).toBeTruthy();
    }
    // Active button carries the indigo accent. We assert by class
    // since there is no aria-pressed wired up (kept the assertion
    // weaker than the visual contract on purpose).
    const medium = screen.getByText("Medium");
    expect(medium.className).toMatch(/bg-indigo-600/);
    const low = screen.getByText("Low");
    expect(low.className).not.toMatch(/bg-indigo-600/);
  });

  it("clicking a level calls setEffort with that value", () => {
    const setEffort = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ setEffort });
    render(<AdminPage />);
    fireEvent.click(screen.getByText("Low"));
    expect(setEffort).toHaveBeenCalledWith("low");
    fireEvent.click(screen.getByText("Max"));
    expect(setEffort).toHaveBeenCalledWith("max");
    expect(setEffort).toHaveBeenCalledTimes(2);
  });
});

describe("AdminPage skip-permissions toggle", () => {
  it("renders the checkbox checked when skipPermissions is true", () => {
    useSettingsStore.setState({ skipPermissions: true });
    render(<AdminPage />);
    const cb = screen.getByRole("checkbox") as HTMLInputElement;
    expect(cb.checked).toBe(true);
    // The label flips to "On" when enabled.
    expect(screen.getByText(/^On$/)).toBeTruthy();
  });

  it("renders the checkbox unchecked when skipPermissions is false", () => {
    useSettingsStore.setState({ skipPermissions: false });
    render(<AdminPage />);
    const cb = screen.getByRole("checkbox") as HTMLInputElement;
    expect(cb.checked).toBe(false);
    expect(screen.getByText(/^Off$/)).toBeTruthy();
  });

  it("toggling the checkbox calls setSkipPermissions with the new value", () => {
    const setSkipPermissions = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ skipPermissions: false, setSkipPermissions });
    render(<AdminPage />);
    const cb = screen.getByRole("checkbox") as HTMLInputElement;
    fireEvent.click(cb);
    expect(setSkipPermissions).toHaveBeenCalledWith(true);
  });
});

describe("AdminPage notification webhook", () => {
  it("seeds the input from the store value", () => {
    useSettingsStore.setState({
      webhookUrl: "https://hooks.slack.com/services/AAA",
    });
    render(<AdminPage />);
    const input = screen.getByPlaceholderText(
      /hooks\.slack\.com/i,
    ) as HTMLInputElement;
    expect(input.value).toBe("https://hooks.slack.com/services/AAA");
  });

  it("Save is disabled until the input is dirty", () => {
    useSettingsStore.setState({ webhookUrl: null });
    render(<AdminPage />);
    const save = screen.getByText(/^Save$/) as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    const input = screen.getByPlaceholderText(
      /hooks\.slack\.com/i,
    ) as HTMLInputElement;
    fireEvent.change(input, {
      target: { value: "https://hooks.slack.com/services/abc" },
    });
    expect(save.disabled).toBe(false);
  });

  it("Save POSTs the trimmed URL via setWebhookUrl and shows Saved", async () => {
    const setWebhookUrl = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({ webhookUrl: null, setWebhookUrl });
    render(<AdminPage />);
    const input = screen.getByPlaceholderText(
      /hooks\.slack\.com/i,
    ) as HTMLInputElement;
    fireEvent.change(input, {
      target: { value: "  https://hooks.slack.com/services/xyz  " },
    });
    fireEvent.click(screen.getByText(/^Save$/));
    await waitFor(() =>
      expect(setWebhookUrl).toHaveBeenCalledWith(
        "https://hooks.slack.com/services/xyz",
      ),
    );
    await waitFor(() => expect(screen.getByText(/^Saved\.$/)).toBeTruthy());
  });

  it("clearing the input and saving sends null (clears the webhook)", async () => {
    const setWebhookUrl = vi.fn().mockResolvedValue(undefined);
    useSettingsStore.setState({
      webhookUrl: "https://hooks.slack.com/services/EXISTING",
      setWebhookUrl,
    });
    render(<AdminPage />);
    const input = screen.getByPlaceholderText(
      /hooks\.slack\.com/i,
    ) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.click(screen.getByText(/^Save$/));
    await waitFor(() => expect(setWebhookUrl).toHaveBeenCalledWith(null));
  });

  it("surfaces an error when the webhook save throws", async () => {
    const setWebhookUrl = vi
      .fn()
      .mockRejectedValue(new Error("hook url rejected"));
    useSettingsStore.setState({ webhookUrl: null, setWebhookUrl });
    render(<AdminPage />);
    const input = screen.getByPlaceholderText(
      /hooks\.slack\.com/i,
    ) as HTMLInputElement;
    fireEvent.change(input, {
      target: { value: "https://example.invalid/hook" },
    });
    fireEvent.click(screen.getByText(/^Save$/));
    await waitFor(() =>
      expect(screen.getByText(/hook url rejected/i)).toBeTruthy(),
    );
  });
});

describe("isAdminRole", () => {
  it("returns true for owner and admin", () => {
    expect(isAdminRole("owner")).toBe(true);
    expect(isAdminRole("admin")).toBe(true);
  });
  it("returns false for member, viewer, missing", () => {
    expect(isAdminRole("member")).toBe(false);
    expect(isAdminRole("viewer")).toBe(false);
    expect(isAdminRole(null)).toBe(false);
    expect(isAdminRole(undefined)).toBe(false);
    expect(isAdminRole("")).toBe(false);
  });
});

/// Tab gating — the load-bearing claim the task adds. Standalone and
/// non-admin users see only the Settings tab; admins see all six.
describe("AdminPage tab gating", () => {
  function asSaasAdmin() {
    useRunnerStore.setState({ mode: "saas", loaded: true });
    useOrgStore.setState({
      memberships: [
        {
          id: "org-1",
          name: "Acme",
          slug: "acme",
          role: "owner",
          memberCount: 3,
        },
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    useAuthStore.setState({
      user: { id: "user-1", email: "u@example.com", orgId: "org-1" },
      loading: false,
      error: null,
    });
  }

  function asSaasMember() {
    useRunnerStore.setState({ mode: "saas", loaded: true });
    useOrgStore.setState({
      memberships: [
        {
          id: "org-1",
          name: "Acme",
          slug: "acme",
          role: "member",
          memberCount: 3,
        },
      ],
      loaded: true,
      lastFetchedAt: Date.now(),
    });
    useAuthStore.setState({
      user: { id: "user-1", email: "u@example.com", orgId: "org-1" },
      loading: false,
      error: null,
    });
  }

  it("standalone deployments render Settings + Diagnostics", () => {
    // Default beforeEach already sets mode=standalone + no memberships.
    // Diagnostics is a universal triage view (added in 9.2), so the
    // tab bar now appears on standalone too — but only those two tabs.
    render(<AdminPage />);
    const tabs = screen
      .getAllByRole("tab")
      .map((t) => (t.textContent ?? "").trim());
    expect(tabs).toEqual(["Settings", "Diagnostics"]);
    // Settings tab content still renders (effort buttons present).
    expect(screen.getByText(/^Low$/)).toBeTruthy();
  });

  it("SaaS members see Settings + Diagnostics + Members", () => {
    asSaasMember();
    render(<AdminPage />);
    const tablist = screen.getByRole("tablist");
    expect(tablist).toBeTruthy();
    const tabs = screen
      .getAllByRole("tab")
      .map((t) => (t.textContent ?? "").trim());
    expect(tabs).toEqual(["Settings", "Diagnostics", "Members"]);
  });

  it("SaaS owners see all admin tabs (Diagnostics included)", () => {
    asSaasAdmin();
    render(<AdminPage />);
    const tabs = screen
      .getAllByRole("tab")
      .map((t) => (t.textContent ?? "").trim());
    expect(tabs).toEqual([
      "Settings",
      "Diagnostics",
      "Members",
      "Budget",
      "Kill switch",
      "User quotas",
      "SSO",
    ]);
  });

  it("requesting an admin-only URL as a non-admin renders Settings", () => {
    asSaasMember();
    render(<AdminPage />, { initialEntry: "/admin/budget" });
    // Falls back to Settings: the effort buttons are visible.
    expect(screen.getByText(/^Low$/)).toBeTruthy();
    // Budget tab is not in the tablist for a non-admin.
    const tabs = screen
      .getAllByRole("tab")
      .map((t) => (t.textContent ?? "").trim());
    expect(tabs).not.toContain("Budget");
  });

  it("requesting an org-only URL on standalone falls back to Settings", () => {
    // Default beforeEach is standalone.
    render(<AdminPage />, { initialEntry: "/admin/members" });
    expect(screen.getByText(/^Low$/)).toBeTruthy();
    // Members is not in the standalone tablist — only Settings + Diagnostics.
    const tabs = screen
      .getAllByRole("tab")
      .map((t) => (t.textContent ?? "").trim());
    expect(tabs).not.toContain("Members");
  });
});
