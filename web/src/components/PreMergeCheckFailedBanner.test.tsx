import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { PreMergeCheckFailedBanner } from "./PlanBoard.js";
import {
  usePlanStore,
  type PlanConfig,
  type PreMergeCheckFailureState,
} from "../stores/plan-store.js";
import { useToastStore } from "../stores/toast-store.js";

const PLAN = "p1";

function defaultConfig(overrides: Partial<PlanConfig> = {}): PlanConfig {
  return {
    autoAdvance: false,
    autoMode: true,
    maxFixAttempts: 3,
    pausedReason: null,
    parallel: false,
    runnerId: null,
    runnerFailover: "pause",
    ...overrides,
  };
}

function defaultFailure(
  overrides: Partial<PreMergeCheckFailureState> = {},
): PreMergeCheckFailureState {
  return {
    checkName: "cargo-fmt",
    exitCode: 1,
    outputSnippet: "Diff in src/foo.rs\n+ formatted line",
    agentId: "agent-1",
    ...overrides,
  };
}

function seed(config: PlanConfig | null, failure: PreMergeCheckFailureState | null = null): void {
  usePlanStore.setState({
    planConfigs: config ? { [PLAN]: config } : {},
    preMergeCheckFailures: failure ? { [PLAN]: failure } : {},
  });
}

/// Install a fetch mock that captures requests. Returns the captured
/// requests array so tests can inspect what the Resume button posted.
function installFetchMock(
  responses: Record<string, () => Response | Promise<Response>>,
): Array<{ path: string; method: string; body?: unknown }> {
  const captured: Array<{ path: string; method: string; body?: unknown }> = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      const method = init?.method ?? "GET";
      let body: unknown;
      if (typeof init?.body === "string") {
        try {
          body = JSON.parse(init.body);
        } catch {
          body = init.body;
        }
      }
      captured.push({ path, method, body });
      const responder = responses[`${method} ${path}`] ?? responses[path];
      if (!responder) {
        return new Response(JSON.stringify({}), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return responder();
    }),
  );
  return captured;
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn());
  useToastStore.getState().clear();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  usePlanStore.setState({ planConfigs: {}, preMergeCheckFailures: {} });
  useToastStore.getState().clear();
});

describe("PreMergeCheckFailedBanner", () => {
  it("does not render when pausedReason is null", () => {
    seed(defaultConfig({ pausedReason: null }), defaultFailure());
    const { container } = render(<PreMergeCheckFailedBanner planName={PLAN} />);
    expect(container.textContent).toBe("");
  });

  it("does not render when pausedReason is a different reason", () => {
    seed(defaultConfig({ pausedReason: "merge_failed" }), defaultFailure());
    const { container } = render(<PreMergeCheckFailedBanner planName={PLAN} />);
    expect(container.textContent).toBe("");
  });

  it("renders the failing check name + exit code + collapsed snippet when paused", () => {
    seed(
      defaultConfig({ pausedReason: "pre_merge_check_failed" }),
      defaultFailure({ checkName: "web-build", exitCode: 2 }),
    );
    render(<PreMergeCheckFailedBanner planName={PLAN} />);

    const banner = screen.getByTestId("pre-merge-check-failed-banner");
    expect(banner).toBeTruthy();
    expect(banner.textContent).toContain("Pre-merge check failed");
    expect(banner.textContent).toContain("web-build");
    expect(banner.textContent).toContain("exit");
    expect(banner.textContent).toContain("2");

    // Snippet is wrapped in <details> so collapsed by default.
    const details = banner.querySelector("details");
    expect(details).toBeTruthy();
    expect(details?.hasAttribute("open")).toBe(false);

    // Summary toggle visible
    expect(banner.textContent).toContain("Show output");
  });

  it("expands snippet when <summary> is clicked", () => {
    seed(
      defaultConfig({ pausedReason: "pre_merge_check_failed" }),
      defaultFailure({ outputSnippet: "FATAL: prettier-formatting-diff" }),
    );
    render(<PreMergeCheckFailedBanner planName={PLAN} />);
    const banner = screen.getByTestId("pre-merge-check-failed-banner");
    const details = banner.querySelector("details")!;
    const summary = details.querySelector("summary")!;
    expect(details.open).toBe(false);
    fireEvent.click(summary);
    // jsdom toggles `open` on click of <summary>
    expect(details.open).toBe(true);

    const pre = screen.getByTestId("pre-merge-check-failed-snippet");
    expect(pre.textContent).toContain("FATAL: prettier-formatting-diff");
  });

  it("falls back to a hint when no snippet is in memory (e.g. after page reload)", () => {
    seed(defaultConfig({ pausedReason: "pre_merge_check_failed" }), null);
    render(<PreMergeCheckFailedBanner planName={PLAN} />);
    const banner = screen.getByTestId("pre-merge-check-failed-banner");
    expect(banner.textContent).toContain("Output snippet not in memory");
    expect(banner.textContent).toContain("Activity log");
  });

  it("Resume button POSTs to /api/plans/:name/resume with retryGate:true", async () => {
    seed(defaultConfig({ pausedReason: "pre_merge_check_failed" }), defaultFailure());

    const captured = installFetchMock({
      "POST /api/plans/p1/resume": () =>
        new Response(
          JSON.stringify({
            ok: true,
            plan: PLAN,
            task: "1.1",
            agentId: "agent-1",
            trigger: "retry_gate",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        ),
      "GET /api/plans/p1/config": () =>
        new Response(
          JSON.stringify({
            autoAdvance: false,
            autoMode: true,
            maxFixAttempts: 3,
            pausedReason: null,
            parallel: false,
            runnerId: null,
            runnerFailover: "pause",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        ),
    });

    render(<PreMergeCheckFailedBanner planName={PLAN} />);

    const resumeBtn = screen.getByTestId("pre-merge-check-failed-resume");
    expect(resumeBtn.textContent).toContain("Resume");

    await act(async () => {
      fireEvent.click(resumeBtn);
    });

    await waitFor(() => {
      const post = captured.find((c) => c.method === "POST");
      expect(post).toBeTruthy();
    });

    const post = captured.find((c) => c.method === "POST")!;
    expect(post.path).toBe("/api/plans/p1/resume");
    expect(post.body).toEqual({ retryGate: true });
  });

  it("Resume button surfaces server errors via toast", async () => {
    seed(defaultConfig({ pausedReason: "pre_merge_check_failed" }), defaultFailure());

    installFetchMock({
      "POST /api/plans/p1/resume": () =>
        new Response(
          JSON.stringify({
            error: "not_pre_merge_pause",
            message: "Stale banner — plan is no longer paused for the gate.",
          }),
          { status: 409, headers: { "Content-Type": "application/json" } },
        ),
    });

    render(<PreMergeCheckFailedBanner planName={PLAN} />);

    const resumeBtn = screen.getByTestId("pre-merge-check-failed-resume");
    await act(async () => {
      fireEvent.click(resumeBtn);
    });

    await waitFor(() => {
      const toasts = useToastStore.getState().toasts;
      expect(toasts.length).toBeGreaterThan(0);
      expect(toasts.some((t) => t.kind === "error" && /Resume failed/.test(t.title))).toBe(true);
    });
  });

  it("Resume button shows pending state while in flight", async () => {
    seed(defaultConfig({ pausedReason: "pre_merge_check_failed" }), defaultFailure());

    let resolveResume: ((value: Response) => void) | null = null;
    const resumePromise = new Promise<Response>((r) => {
      resolveResume = r;
    });

    installFetchMock({
      "POST /api/plans/p1/resume": () => resumePromise,
    });

    render(<PreMergeCheckFailedBanner planName={PLAN} />);

    const resumeBtn = screen.getByTestId("pre-merge-check-failed-resume");
    expect(resumeBtn.textContent).toContain("Resume");
    expect((resumeBtn as HTMLButtonElement).disabled).toBe(false);

    await act(async () => {
      fireEvent.click(resumeBtn);
    });

    expect(resumeBtn.textContent).toContain("Resuming");
    expect((resumeBtn as HTMLButtonElement).disabled).toBe(true);

    // Resolve the in-flight resume so the test cleanup doesn't hang.
    await act(async () => {
      resolveResume!(
        new Response(JSON.stringify({ ok: true, plan: PLAN, task: "1.1", agentId: "a1" }), {
          status: 200,
        }),
      );
    });
  });

  it("treats missing exit code as 'killed by timeout'", () => {
    seed(
      defaultConfig({ pausedReason: "pre_merge_check_failed" }),
      defaultFailure({ exitCode: null }),
    );
    render(<PreMergeCheckFailedBanner planName={PLAN} />);
    const banner = screen.getByTestId("pre-merge-check-failed-banner");
    expect(banner.textContent).toContain("killed by timeout");
  });
});
