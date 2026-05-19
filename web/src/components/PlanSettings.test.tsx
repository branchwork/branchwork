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
        if ("mergeCadence" in body) {
          state.current = {
            ...state.current,
            mergeCadence: body.mergeCadence,
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
    mergeCadence: null,
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
    // ciBlockingWorkflows is set; the other three (ci.blocking_workflows_skip,
    // phase.verification, auto_mode.merge_cadence) render as 'unset'.
    expect(screen.getAllByText(/unset/i).length).toBe(3);
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

describe("PlanSettings merge cadence panel", () => {
  it("renders three radio options labelled Task / Phase / Plan", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    const fieldset = screen.getByRole("group", { name: /Merge cadence/i });
    const radios = fieldset.querySelectorAll('input[type="radio"][name="merge-cadence"]');
    expect(radios.length).toBe(3);
    expect(Array.from(radios).map((r) => (r as HTMLInputElement).value)).toEqual([
      "task",
      "phase",
      "plan",
    ]);
  });

  /// Look up a cadence radio by its `value` attribute. The displayed
  /// label includes per-option descriptive text that overlaps across
  /// options (e.g. the "phase" description mentions "plans"), so the
  /// accessible-name regex matcher is too loose. The DOM `value`
  /// attribute is the load-bearing discriminator.
  function cadenceRadio(value: "task" | "phase" | "plan"): HTMLInputElement {
    const node = document.querySelector(
      `input[type="radio"][name="merge-cadence"][value="${value}"]`,
    );
    if (!node) {
      throw new Error(`no merge-cadence radio with value=${value}`);
    }
    return node as HTMLInputElement;
  }

  it("highlights the inherited default ('phase') when no plan-level pin is set", async () => {
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    expect(cadenceRadio("phase").checked).toBe(true);
    expect(cadenceRadio("task").checked).toBe(false);
    expect(cadenceRadio("plan").checked).toBe(false);
    // Inherited badge appears next to the active row when the row is not
    // an explicit plan-level pin.
    expect(screen.getAllByText(/inherited/i).length).toBeGreaterThanOrEqual(1);
    expect(screen.queryByText(/Plan-level cadence pin is active/i)).toBeNull();
    expect(screen.getByText(/Resolved cadence: the built-in default \(phase\)/i)).toBeTruthy();
  });

  it("sends a PUT with the chosen cadence and updates the active selection", async () => {
    const state: MockState = { current: defaultSettings(), putBodies: [] };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    fireEvent.click(cadenceRadio("plan"));
    await waitFor(() => expect(state.putBodies.length).toBe(1));
    expect(state.putBodies[0]).toEqual({ mergeCadence: "plan" });
    await waitFor(() => {
      expect(cadenceRadio("plan").checked).toBe(true);
    });
    // After the explicit pin, the badge flips from inherited to plan.
    expect(screen.getByText(/Plan-level cadence pin is active/i)).toBeTruthy();
  });

  it("shows an Inherit button only when a plan-level pin is active", async () => {
    // Fresh — no pin, no button.
    installFetchMock({ current: defaultSettings(), putBodies: [] });
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    const cadenceSection = screen.getByRole("group", { name: /Merge cadence/i }).parentElement!;
    expect(cadenceSection.querySelector('button[title*="inherit"]')).toBeNull();
    cleanup();

    // Pinned to 'task' — Inherit button appears.
    const state: MockState = {
      current: defaultSettings({ mergeCadence: "task" }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    // Two Inherit buttons exist when both verify and cadence are
    // overridden, so disambiguate by the cadence-specific title.
    const inheritBtn = document.querySelector<HTMLButtonElement>(
      'button[title*="inherit from the project default"]',
    );
    expect(inheritBtn).toBeTruthy();
    fireEvent.click(inheritBtn!);
    await waitFor(() => expect(state.putBodies.length).toBe(1));
    expect(state.putBodies[0]).toEqual({ mergeCadence: null });
  });

  it("surfaces the repo-default cadence in the inherited copy when branchwork.toml overrides it", async () => {
    const state: MockState = {
      current: defaultSettings({
        repoDefaults: { mergeCadence: "task" },
      }),
      putBodies: [],
    };
    installFetchMock(state);
    render(<PlanSettings planName="p1" />);
    await waitFor(() => screen.getByText(/Merge cadence/i));
    // Resolved cadence reflects the repo default, not the hard-coded
    // 'phase'. The Task radio is the active selection.
    expect(screen.getByText(/Resolved cadence: repo default: task/i)).toBeTruthy();
    expect(cadenceRadio("task").checked).toBe(true);
  });
});
