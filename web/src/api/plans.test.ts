import { describe, expect, it } from "vitest";
import {
  isWorkflowBlockingByDefault,
  resolvePhaseVerification,
  resolveWorkflows,
  type PhaseSettings,
  type PlanSettings,
} from "./plans.js";

function settingsWith(overrides: Partial<PlanSettings>): PlanSettings {
  return {
    ciBlockingWorkflows: null,
    phaseVerification: null,
    availableWorkflows: [],
    repoDefaults: {},
    ...overrides,
  };
}

describe("isWorkflowBlockingByDefault", () => {
  // Mirrors the parity tests in server-rs/src/ci/resolution.rs::tests so
  // a regex drift is caught here too.
  it("treats CI/test/lint as blocking", () => {
    expect(isWorkflowBlockingByDefault("CI")).toBe(true);
    expect(isWorkflowBlockingByDefault("ci")).toBe(true);
    expect(isWorkflowBlockingByDefault("tests")).toBe(true);
    expect(isWorkflowBlockingByDefault("lint")).toBe(true);
    expect(isWorkflowBlockingByDefault("clippy")).toBe(true);
  });

  it("treats deploy/docker/publish/release/bench/fuzz as informational", () => {
    expect(isWorkflowBlockingByDefault("Docker")).toBe(false);
    expect(isWorkflowBlockingByDefault("docker")).toBe(false);
    expect(isWorkflowBlockingByDefault("Deploy")).toBe(false);
    expect(isWorkflowBlockingByDefault("publish-crate")).toBe(false);
    expect(isWorkflowBlockingByDefault("release")).toBe(false);
    expect(isWorkflowBlockingByDefault("bench")).toBe(false);
    expect(isWorkflowBlockingByDefault("fuzz-corpus")).toBe(false);
  });
});

describe("resolveWorkflows", () => {
  it("uses smart-default when neither plan nor repo defines a list", () => {
    const got = resolveWorkflows(settingsWith({ availableWorkflows: ["ci", "deploy", "tests"] }));
    expect(got).toEqual([
      { name: "ci", blocking: true, source: "smart default" },
      { name: "deploy", blocking: false, source: "smart default" },
      { name: "tests", blocking: true, source: "smart default" },
    ]);
  });

  it("uses repo-default when plan list is null but repo list is set", () => {
    const got = resolveWorkflows(
      settingsWith({
        availableWorkflows: ["ci", "deploy", "tests"],
        repoDefaults: { ciBlockingWorkflows: ["ci", "deploy"] },
      }),
    );
    expect(got).toEqual([
      { name: "ci", blocking: true, source: "repo default" },
      { name: "deploy", blocking: true, source: "repo default" },
      { name: "tests", blocking: false, source: "repo default" },
    ]);
  });

  it("uses plan list verbatim when set, even when classifier disagrees", () => {
    // Plan list explicitly includes 'deploy' even though the classifier
    // would call it informational. The plan source must win.
    const got = resolveWorkflows(
      settingsWith({
        availableWorkflows: ["ci", "deploy", "tests"],
        ciBlockingWorkflows: ["deploy"],
      }),
    );
    expect(got).toEqual([
      { name: "ci", blocking: false, source: "plan" },
      { name: "deploy", blocking: true, source: "plan" },
      { name: "tests", blocking: false, source: "plan" },
    ]);
  });

  it("explicit empty plan list opts out everything", () => {
    const got = resolveWorkflows(
      settingsWith({
        availableWorkflows: ["ci", "tests"],
        ciBlockingWorkflows: [],
      }),
    );
    expect(got.every((w) => !w.blocking)).toBe(true);
    expect(got.every((w) => w.source === "plan")).toBe(true);
  });

  it("returns empty array when no workflows are discovered", () => {
    expect(resolveWorkflows(settingsWith({}))).toEqual([]);
  });
});

describe("resolvePhaseVerification", () => {
  function ps(overrides: Partial<PhaseSettings>): PhaseSettings {
    return {
      phaseNumber: 1,
      phaseVerification: null,
      planVerification: null,
      repoVerification: null,
      ...overrides,
    };
  }

  it("falls through phase → plan → repo → none", () => {
    expect(resolvePhaseVerification(ps({}))).toEqual({ value: null, source: "none" });
    expect(resolvePhaseVerification(ps({ repoVerification: "make verify" }))).toEqual({
      value: "make verify",
      source: "repo",
    });
    expect(
      resolvePhaseVerification(ps({ repoVerification: "make verify", planVerification: "plan.sh" })),
    ).toEqual({ value: "plan.sh", source: "plan" });
    expect(
      resolvePhaseVerification(
        ps({
          repoVerification: "make verify",
          planVerification: "plan.sh",
          phaseVerification: "phase.sh",
        }),
      ),
    ).toEqual({ value: "phase.sh", source: "phase" });
  });

  it("treats explicit empty string as a real phase override (not inheritance)", () => {
    // The server stores `null` for an absent field and a string for an
    // explicit value. An empty-string override is unusual but legal —
    // the resolver must not silently fall through.
    const got = resolvePhaseVerification(
      ps({ phaseVerification: "", planVerification: "plan.sh" }),
    );
    expect(got).toEqual({ value: "", source: "phase" });
  });
});
