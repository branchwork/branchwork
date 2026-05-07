import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PlanSettings } from "./PlanSettings.js";
import type { PlanSettings as PlanSettingsT } from "../api/plans.js";

interface MockState {
  current: PlanSettingsT;
  /// Records every PUT body so tests can assert what was sent on the
  /// wire (the response is already shape-checked by tsc).
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
    if (path.startsWith("/api/plans/") && path.endsWith("/settings")) {
      if (init?.method === "PUT") {
        const body = init.body ? JSON.parse(init.body as string) : {};
        state.putBodies.push(body);
        // Apply the PUT to the in-memory state so the post-update GET
        // shape (returned from the same call) reflects the change.
        if ("ciBlockingWorkflows" in body) {
          state.current = {
            ...state.current,
            ciBlockingWorkflows: body.ciBlockingWorkflows,
          };
        }
        if ("phaseVerification" in body) {
          state.current = {
            ...state.current,
            phaseVerification: body.phaseVerification,
          };
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

function defaultSettings(overrides: Partial<PlanSettingsT> = {}): PlanSettingsT {
  return {
    ciBlockingWorkflows: null,
    phaseVerification: null,
    availableWorkflows: ["ci", "deploy", "tests"],
    repoDefaults: {},
    ...overrides,
  };
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("PlanSettings load and render", () => {
  it("shows a loading state then renders sections", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    expect(screen.getByRole("status", { name: /Loading plan settings/i })).toBeTruthy();
    await waitFor(() => {
      expect(screen.getByText(/Blocking CI workflows/i)).toBeTruthy();
    });
    expect(screen.getByText(/Phase verification command/i)).toBeTruthy();
    expect(screen.getByText(/Repo defaults/i)).toBeTruthy();
  });

  it("renders one row per discovered workflow with the right source badge", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText("ci"));

    // All three rows should render with smart-default badges (plan list
    // is null and repo defaults are empty).
    expect(screen.getAllByText(/smart default/i).length).toBeGreaterThanOrEqual(3);

    // The classifier marks 'deploy' as informational; the others as blocking.
    const ciCheckbox = screen.getByLabelText(/^ci/) as HTMLInputElement;
    const deployCheckbox = screen.getByLabelText(/^deploy/) as HTMLInputElement;
    const testsCheckbox = screen.getByLabelText(/^tests/) as HTMLInputElement;
    expect(ciCheckbox.checked).toBe(true);
    expect(deployCheckbox.checked).toBe(false);
    expect(testsCheckbox.checked).toBe(true);
  });

  it("disables checkboxes until Override is clicked when inheriting", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText("ci"));
    const ciCheckbox = screen.getByLabelText(/^ci/) as HTMLInputElement;
    expect(ciCheckbox.disabled).toBe(true);
    expect(screen.getByRole("button", { name: /Override/i })).toBeTruthy();
  });
});

describe("PlanSettings workflow toggle", () => {
  it("Override seeds the plan list with the resolved blocking set", async () => {
    const state: MockState = { current: defaultSettings(), putBodies: [] };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText("ci"));

    fireEvent.click(screen.getByRole("button", { name: /Override/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBeGreaterThan(0);
    });
    // Smart-default seed: ci + tests are blocking, deploy is not.
    const body = state.putBodies[0] as { ciBlockingWorkflows: string[] };
    expect(body.ciBlockingWorkflows.sort()).toEqual(["ci", "tests"]);
  });

  it("toggling a checkbox updates the plan-level list verbatim", async () => {
    const state: MockState = {
      current: defaultSettings({ ciBlockingWorkflows: ["ci"] }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText("ci"));

    // Start: ci checked, deploy unchecked, tests unchecked. Add deploy.
    fireEvent.click(screen.getByLabelText(/^deploy/));
    await waitFor(() => {
      expect(state.putBodies.length).toBeGreaterThan(0);
    });
    const body = state.putBodies[0] as { ciBlockingWorkflows: string[] };
    expect(body.ciBlockingWorkflows.sort()).toEqual(["ci", "deploy"]);
  });

  it("Inherit clears the plan-level override (sends explicit null)", async () => {
    const state: MockState = {
      current: defaultSettings({ ciBlockingWorkflows: ["ci", "deploy"] }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText("ci"));

    fireEvent.click(screen.getByRole("button", { name: /Inherit/i }));
    await waitFor(() => {
      expect(state.putBodies.length).toBeGreaterThan(0);
    });
    expect(state.putBodies[0]).toEqual({ ciBlockingWorkflows: null });
  });
});

describe("PlanSettings phase verification", () => {
  it("shows the resolved value as placeholder when inheriting from repo", async () => {
    const state: MockState = {
      current: defaultSettings({
        phaseVerification: null,
        repoDefaults: { phaseVerification: "make verify" },
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Phase verification command/i));
    const input = screen.getByLabelText(/Phase verification command/i) as HTMLInputElement;
    expect(input.value).toBe("");
    expect(input.placeholder).toBe("make verify");
    expect(screen.getByText(/Inheriting from/i)).toBeTruthy();
  });

  it("Save sends the trimmed value", async () => {
    const state: MockState = { current: defaultSettings(), putBodies: [] };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Phase verification command/i));

    const input = screen.getByLabelText(/Phase verification command/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "  cargo test --release  " } });
    fireEvent.click(screen.getByRole("button", { name: /Save/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBeGreaterThan(0);
    });
    expect(state.putBodies[0]).toEqual({ phaseVerification: "cargo test --release" });
  });

  it("clearing the field and saving sends explicit null", async () => {
    const state: MockState = {
      current: defaultSettings({ phaseVerification: "cargo test" }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Phase verification command/i));

    const input = screen.getByLabelText(/Phase verification command/i) as HTMLInputElement;
    expect(input.value).toBe("cargo test");
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: /Save/i }));

    await waitFor(() => {
      expect(state.putBodies.length).toBeGreaterThan(0);
    });
    expect(state.putBodies[0]).toEqual({ phaseVerification: null });
  });
});

describe("PlanSettings repo defaults panel", () => {
  it("shows the empty-state hint when no repo defaults are present", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Repo defaults/i));
    // The dl-row labels never render in the empty state, so absence of
    // those labels is the load-bearing assertion. Match the prose
    // verbatim because the word "No" appears in adjacent sections too.
    expect(screen.queryByText("ci.blocking_workflows")).toBeNull();
    expect(screen.getByText(/sets none of these keys/)).toBeTruthy();
  });

  it("shows 'unset' for individual fields missing from a partial branchwork.toml", async () => {
    const state: MockState = {
      current: defaultSettings({
        repoDefaults: { ciBlockingWorkflows: ["ci"] },
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Repo defaults/i));
    // ciBlockingWorkflows is set; the other two render as 'unset'.
    expect(screen.getAllByText(/unset/i).length).toBe(2);
  });

  it("shows the populated repo-default values verbatim", async () => {
    const state: MockState = {
      current: defaultSettings({
        repoDefaults: {
          ciBlockingWorkflows: ["ci", "lint"],
          phaseVerification: "make verify",
        },
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Repo defaults/i));
    expect(screen.getByText("ci, lint")).toBeTruthy();
    // Verify command is shared with the inheriting placeholder, so use
    // the dt label as the discriminator.
    expect(screen.getByText("phase.verification")).toBeTruthy();
  });
});
