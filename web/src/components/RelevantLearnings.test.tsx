import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { RelevantLearnings, type RelevantLearning } from "./TaskCard.js";

const PLAN = "p1";
const TASK = "1.1";

function relevantLearning(overrides: Partial<RelevantLearning> = {}): RelevantLearning {
  return {
    id: "L-1",
    orgId: "default-org",
    kind: "feedback",
    category: "spawn_ops",
    slug: "spawn-ops-gotcha",
    bodyMd: "Refresh runners after dispatch — see ADR 0007.",
    sourceAgentId: null,
    sourceCiRunId: null,
    createdAt: "2026-04-12T10:00:00",
    archivedAt: null,
    archivedReason: null,
    sourceRunUrl: null,
    sourceWorkflow: null,
    ...overrides,
  };
}

interface FetchCall {
  path: string;
  method: string;
}

function installRelevantLearningsMock(items: RelevantLearning[]): { calls: FetchCall[] } {
  const calls: FetchCall[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = typeof input === "string" ? input : input.toString();
      calls.push({ path, method: init?.method ?? "GET" });
      return new Response(JSON.stringify({ items }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
  return { calls };
}

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn());
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("RelevantLearnings", () => {
  it("renders the toggle button collapsed by default and skips the fetch", () => {
    installRelevantLearningsMock([]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    const toggle = screen.getByTestId(`relevant-learnings-toggle-${TASK}`);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    // Panel is NOT in the DOM until expanded.
    expect(screen.queryByTestId(`relevant-learnings-panel-${TASK}`)).toBeNull();
  });

  it("lazy-fetches /api/.../relevant-learnings on first expand", async () => {
    const { calls } = installRelevantLearningsMock([relevantLearning()]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => {
      expect(
        calls.some((c) => c.path.includes(`/api/plans/${PLAN}/tasks/${TASK}/relevant-learnings`)),
      ).toBe(true);
    });
    // Panel is in the DOM after expand.
    expect(screen.getByTestId(`relevant-learnings-panel-${TASK}`)).toBeTruthy();
  });

  it("renders the empty-state copy when the endpoint returns []", async () => {
    installRelevantLearningsMock([]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => {
      expect(screen.getByText(/No past learnings will be injected for this task/i)).toBeTruthy();
    });
  });

  it("renders the per-learning row with kind + category preview", async () => {
    installRelevantLearningsMock([
      relevantLearning({
        id: "L-spawn",
        kind: "feedback",
        category: "spawn_ops",
        bodyMd: "Always restart the runner after a Hetzner deploy.",
      }),
    ]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => {
      const row = screen.getByTestId("relevant-learning-L-spawn");
      expect(row.textContent).toContain("feedback");
      expect(row.textContent).toContain("spawn_ops");
    });
    // Body markdown is NOT visible until the row is expanded.
    expect(screen.queryByText(/Always restart the runner after a Hetzner deploy/i)).toBeNull();
  });

  it("expands an item to show the full markdown body on click", async () => {
    installRelevantLearningsMock([
      relevantLearning({
        id: "L-body",
        bodyMd: "Detailed instructions on multiple\nlines preserve formatting.",
      }),
    ]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => screen.getByTestId("relevant-learning-L-body"));

    const row = screen.getByTestId("relevant-learning-L-body");
    // The per-row toggle button is the row container's first <button>.
    const rowToggle = row.querySelector("button");
    expect(rowToggle).toBeTruthy();
    fireEvent.click(rowToggle!);

    await waitFor(() => {
      expect(screen.getByText(/Detailed instructions on multiple/i)).toBeTruthy();
    });
  });

  it("renders a source-link to GitHub for CI-backed learnings", async () => {
    installRelevantLearningsMock([
      relevantLearning({
        id: "L-ci",
        sourceCiRunId: 777,
        sourceRunUrl: "https://github.com/owner/repo/actions/runs/777",
        sourceWorkflow: "tests.yml",
      }),
    ]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => screen.getByTestId("relevant-learning-L-ci"));

    // Expand the row to reveal the source link.
    const row = screen.getByTestId("relevant-learning-L-ci");
    fireEvent.click(row.querySelector("button")!);

    await waitFor(() => {
      const link = screen.getByTestId("relevant-learning-source-L-ci");
      expect(link.getAttribute("href")).toBe("https://github.com/owner/repo/actions/runs/777");
      expect(link.getAttribute("target")).toBe("_blank");
      // rel must include both for security on target=_blank.
      const rel = link.getAttribute("rel") ?? "";
      expect(rel).toContain("noreferrer");
      expect(rel).toContain("noopener");
    });
  });

  it("does NOT render a source-link for learnings without a CI backlink", async () => {
    installRelevantLearningsMock([
      relevantLearning({
        id: "L-no-source",
        sourceCiRunId: null,
        sourceRunUrl: null,
        sourceWorkflow: null,
      }),
    ]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    fireEvent.click(screen.getByTestId(`relevant-learnings-toggle-${TASK}`));
    await waitFor(() => screen.getByTestId("relevant-learning-L-no-source"));

    const row = screen.getByTestId("relevant-learning-L-no-source");
    fireEvent.click(row.querySelector("button")!);

    // After expansion, no source-link testid should be present.
    await waitFor(() => {
      const sourceLink = screen.queryByTestId("relevant-learning-source-L-no-source");
      expect(sourceLink).toBeNull();
    });
  });

  it("uses the count in the toggle label after a successful fetch", async () => {
    installRelevantLearningsMock([
      relevantLearning({ id: "L-a" }),
      relevantLearning({ id: "L-b", slug: "b" }),
      relevantLearning({ id: "L-c", slug: "c" }),
    ]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    const toggle = screen.getByTestId(`relevant-learnings-toggle-${TASK}`);
    fireEvent.click(toggle);
    await waitFor(() => {
      // After the fetch lands, the label includes "(3)".
      expect(toggle.textContent).toContain("(3)");
    });
  });

  it("does not refire the fetch on second toggle (cache hit)", async () => {
    const { calls } = installRelevantLearningsMock([relevantLearning()]);
    render(<RelevantLearnings planName={PLAN} taskNumber={TASK} />);
    const toggle = screen.getByTestId(`relevant-learnings-toggle-${TASK}`);
    fireEvent.click(toggle);
    await waitFor(() =>
      expect(calls.filter((c) => c.path.includes("/relevant-learnings")).length).toBe(1),
    );
    act(() => {
      fireEvent.click(toggle); // collapse
    });
    act(() => {
      fireEvent.click(toggle); // re-expand
    });
    // Still only one fetch: rows are already in state.
    expect(calls.filter((c) => c.path.includes("/relevant-learnings")).length).toBe(1);
  });
});
