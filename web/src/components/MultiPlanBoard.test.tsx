import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { MultiPlanBoard } from "./MultiPlanBoard.js";
import { type PlanSummary } from "../stores/plan-store.js";
import { type PlanGraphEntry } from "../api/plan-graph.js";

function plan(name: string, overrides: Partial<PlanSummary> = {}): PlanSummary {
  return {
    name,
    title: `${name} title`,
    project: "proj",
    phaseCount: 1,
    taskCount: 2,
    doneCount: 0,
    createdAt: "2026-01-01T00:00:00Z",
    modifiedAt: "2026-01-02T00:00:00Z",
    ...overrides,
  };
}

/// Stub `/api/plan-graph` to return `entries`. Any other URL → empty 200.
function installGraphFetch(entries: PlanGraphEntry[]): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const path = typeof input === "string" ? input : input.toString();
      const body = path.includes("/api/plan-graph") ? entries : {};
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }),
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("MultiPlanBoard", () => {
  it("renders one card per plan with title + progress", async () => {
    installGraphFetch([]);
    render(
      <MultiPlanBoard
        projects={[
          {
            name: "proj",
            plans: [plan("a", { doneCount: 1 }), plan("b", { doneCount: 2 })],
          },
        ]}
      />,
    );
    await waitFor(() => expect(screen.getByTestId("plan-graph-card-a")).toBeTruthy());
    expect(screen.getByTestId("plan-graph-card-b")).toBeTruthy();
    expect(screen.getByText("a title")).toBeTruthy();
    // b is 2/2 → completed
    expect(screen.getByTestId("plan-graph-card-b").textContent).toContain("completed");
  });

  it("draws a dependency arrow between two interdependent plans (acceptance)", async () => {
    installGraphFetch([
      { plan: "producer", inputs: [], outputs: [{ name: "schema", satisfied: false }] },
      {
        plan: "consumer",
        inputs: [{ name: "schema", fromPlan: "producer", artifact: "schema", satisfied: false }],
        outputs: [],
      },
    ]);
    render(
      <MultiPlanBoard projects={[{ name: "proj", plans: [plan("producer"), plan("consumer")] }]} />,
    );

    // The edge path between the two cards is the "visible dependency arrow".
    const edge = await waitFor(() => screen.getByTestId("dependency-edge-producer-consumer"));
    expect(edge.tagName.toLowerCase()).toBe("path");
    // Arrowhead marker is wired up → it reads as an arrow, not a plain line.
    expect(edge.getAttribute("marker-end")).toContain("arrow-proj");
    // Unsatisfied dependency → amber stroke.
    expect(edge.getAttribute("class") ?? "").toContain("amber");
  });

  it("shows 'waiting for X from Y' on a blocked plan (acceptance)", async () => {
    installGraphFetch([
      {
        plan: "consumer",
        inputs: [{ name: "schema", fromPlan: "producer", artifact: "schema", satisfied: false }],
        outputs: [],
      },
    ]);
    render(
      <MultiPlanBoard projects={[{ name: "proj", plans: [plan("producer"), plan("consumer")] }]} />,
    );
    const blocked = await waitFor(() => screen.getByTestId("plan-graph-blocked-consumer"));
    const text = blocked.textContent ?? "";
    expect(text).toContain("waiting for");
    expect(text).toContain("schema");
    expect(text).toContain("producer");
  });

  it("uses an emerald edge once the input is satisfied", async () => {
    installGraphFetch([
      { plan: "producer", inputs: [], outputs: [{ name: "schema", satisfied: true }] },
      {
        plan: "consumer",
        inputs: [{ name: "schema", fromPlan: "producer", artifact: "schema", satisfied: true }],
        outputs: [],
      },
    ]);
    render(
      <MultiPlanBoard
        projects={[{ name: "proj", plans: [plan("producer", { doneCount: 2 }), plan("consumer")] }]}
      />,
    );
    const edge = await waitFor(() => screen.getByTestId("dependency-edge-producer-consumer"));
    expect(edge.getAttribute("class") ?? "").toContain("emerald");
    // No blocked badge when every input is satisfied.
    expect(screen.queryByTestId("plan-graph-blocked-consumer")).toBeNull();
  });

  it("reveals the producing plan + status when an input is clicked", async () => {
    installGraphFetch([
      {
        plan: "consumer",
        inputs: [{ name: "schema", fromPlan: "producer", artifact: "schema", satisfied: false }],
        outputs: [],
      },
    ]);
    render(
      <MultiPlanBoard
        projects={[
          {
            name: "proj",
            // producer is in-progress (1/2) so the tooltip status reads it.
            plans: [plan("producer", { doneCount: 1 }), plan("consumer")],
          },
        ]}
      />,
    );
    const chip = await waitFor(() => screen.getByTestId("plan-graph-input-schema"));
    // Tooltip is closed initially.
    expect(screen.queryByTestId("plan-graph-input-tooltip-schema")).toBeNull();
    fireEvent.click(chip);
    const tip = screen.getByTestId("plan-graph-input-tooltip-schema");
    expect(tip.textContent).toContain("Produced by");
    expect(tip.textContent).toContain("producer");
    expect(tip.textContent).toContain("in progress");
    expect(tip.textContent).toContain("artifact pending");
  });

  it("renders a cross-project dependency as a blocked badge with no in-project edge", async () => {
    installGraphFetch([
      {
        plan: "consumer",
        inputs: [{ name: "schema", fromPlan: "producer", artifact: "schema", satisfied: false }],
        outputs: [],
      },
    ]);
    render(
      <MultiPlanBoard
        projects={[
          { name: "proj-a", plans: [plan("consumer", { project: "proj-a" })] },
          { name: "proj-b", plans: [plan("producer", { project: "proj-b" })] },
        ]}
      />,
    );
    await waitFor(() => screen.getByTestId("plan-graph-blocked-consumer"));
    // No edge: the producer lives in a different project's sub-graph.
    expect(screen.queryByTestId("dependency-edge-producer-consumer")).toBeNull();
  });

  it("renders the empty state when there are no projects", () => {
    installGraphFetch([]);
    render(<MultiPlanBoard projects={[]} />);
    expect(screen.getByTestId("multi-plan-board-empty")).toBeTruthy();
  });

  it("renders isolated cards (no overlay entry) without edges", async () => {
    installGraphFetch([]);
    render(<MultiPlanBoard projects={[{ name: "proj", plans: [plan("solo")] }]} />);
    await waitFor(() => screen.getByTestId("plan-graph-card-solo"));
    expect(screen.queryByTestId("plan-graph-blocked-solo")).toBeNull();
  });
});
