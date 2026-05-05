import { describe, expect, it } from "vitest";
import type { PlanSummary } from "../stores/plan-store.js";
import { isPlanDone } from "./predicates.js";

function summary(taskCount: number, doneCount: number): PlanSummary {
  return {
    name: "p",
    title: "p",
    project: null,
    phaseCount: 1,
    taskCount,
    doneCount,
    createdAt: "2026-04-12T00:00:00Z",
    modifiedAt: "2026-04-12T00:00:00Z",
  };
}

describe("isPlanDone", () => {
  it("true when doneCount equals taskCount", () => {
    expect(isPlanDone(summary(3, 3))).toBe(true);
  });

  it("true when doneCount exceeds taskCount (defensive)", () => {
    expect(isPlanDone(summary(3, 4))).toBe(true);
  });

  it("false when doneCount is below taskCount", () => {
    expect(isPlanDone(summary(3, 2))).toBe(false);
  });

  it("false when both counts are zero (would otherwise collapse to true)", () => {
    expect(isPlanDone(summary(0, 0))).toBe(false);
  });
});
