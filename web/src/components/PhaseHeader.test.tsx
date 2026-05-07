import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PhaseHeader } from "./PhaseHeader.js";
import type { PhaseSettings } from "../api/plans.js";

interface MockState {
  current: PhaseSettings;
  putBodies: unknown[];
}

function installFetchMock(state: MockState) {
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const path =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    if (path.startsWith("/api/plans/") && /\/phases\/\d+\/settings$/.test(path)) {
      if (init?.method === "PUT") {
        const body = init.body ? JSON.parse(init.body as string) : {};
        state.putBodies.push(body);
        if ("phaseVerification" in body) {
          state.current = { ...state.current, phaseVerification: body.phaseVerification };
        }
        return new Response(JSON.stringify(state.current), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response(JSON.stringify(state.current), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    return new Response(JSON.stringify({}), { status: 404 });
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

function defaultSettings(overrides: Partial<PhaseSettings> = {}): PhaseSettings {
  return {
    phaseNumber: 1,
    phaseVerification: null,
    planVerification: null,
    repoVerification: null,
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("PhaseHeader accordion", () => {
  it("starts collapsed and lazy-loads on first open", async () => {
    const fetchMock = installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PhaseHeader planName="p1" phaseNumber={1} />);
    // Trigger button visible.
    const toggle = screen.getByRole("button", { name: /Phase override/i });
    expect(toggle).toBeTruthy();
    // No fetch issued before the user clicks.
    expect(fetchMock).not.toHaveBeenCalled();

    fireEvent.click(toggle);
    await waitFor(() => {
      expect(fetchMock).toHaveBeenCalled();
    });
  });

  it("renders the inheritance source badge once loaded", async () => {
    installFetchMock({
      current: defaultSettings({ planVerification: "bash plan.sh" }),
      putBodies: [],
    });
    render(<PhaseHeader planName="p1" phaseNumber={2} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByText(/plan default/i));
    // Inheritance hint copy includes the plan-level command.
    expect(screen.getByText(/Inheriting from plan/i)).toBeTruthy();
  });

  it("Override button stages inherited value into the textarea", async () => {
    installFetchMock({
      current: defaultSettings({ planVerification: "bash plan.sh" }),
      putBodies: [],
    });
    render(<PhaseHeader planName="p1" phaseNumber={1} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByRole("button", { name: /^Override$/i }));

    fireEvent.click(screen.getByRole("button", { name: /^Override$/i }));
    const input = screen.getByLabelText(/Phase 1 verification command/i) as HTMLInputElement;
    expect(input.value).toBe("bash plan.sh");
    // Save button visible only when the draft diverges from the persisted value.
    expect(screen.getByRole("button", { name: /^Save$/i })).toBeTruthy();
  });

  it("Save persists the override via PUT", async () => {
    const state: MockState = {
      current: defaultSettings({ planVerification: "bash plan.sh" }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PhaseHeader planName="p1" phaseNumber={3} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByRole("button", { name: /^Override$/i }));
    fireEvent.click(screen.getByRole("button", { name: /^Override$/i }));

    const input = screen.getByLabelText(/Phase 3 verification command/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "bash phase3.sh" } });
    fireEvent.click(screen.getByRole("button", { name: /^Save$/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBe(1);
    });
    expect(state.putBodies[0]).toEqual({ phaseVerification: "bash phase3.sh" });
  });

  it("empty draft saves as null (Inherit)", async () => {
    const state: MockState = {
      current: defaultSettings({
        phaseVerification: "bash phase4.sh",
        planVerification: "bash plan.sh",
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PhaseHeader planName="p1" phaseNumber={4} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByRole("button", { name: /^Inherit$/i }));

    const input = screen.getByLabelText(/Phase 4 verification command/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "  " } }); // whitespace only
    fireEvent.click(screen.getByRole("button", { name: /^Save$/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBe(1);
    });
    expect(state.putBodies[0]).toEqual({ phaseVerification: null });
  });

  it("Inherit button posts an explicit null", async () => {
    const state: MockState = {
      current: defaultSettings({
        phaseVerification: "bash phase5.sh",
        planVerification: "bash plan.sh",
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PhaseHeader planName="p1" phaseNumber={5} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByRole("button", { name: /^Inherit$/i }));
    fireEvent.click(screen.getByRole("button", { name: /^Inherit$/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBe(1);
    });
    expect(state.putBodies[0]).toEqual({ phaseVerification: null });
  });

  it("forceCollapsed hides the panel even when the user opened it", async () => {
    installFetchMock({
      current: defaultSettings({ planVerification: "bash plan.sh" }),
      putBodies: [],
    });
    const { rerender } = render(<PhaseHeader planName="p1" phaseNumber={1} />);
    fireEvent.click(screen.getByRole("button", { name: /Phase override/i }));
    await waitFor(() => screen.getByRole("button", { name: /^Override$/i }));

    rerender(<PhaseHeader planName="p1" phaseNumber={1} forceCollapsed />);
    expect(screen.queryByRole("button", { name: /^Override$/i })).toBeNull();
  });
});
