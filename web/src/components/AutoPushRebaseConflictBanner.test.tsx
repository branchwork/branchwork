import { afterEach, describe, expect, it } from "vitest";
import { cleanup, screen } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { AutoPushRebaseConflictBanner } from "./PlanBoard.js";
import {
  usePlanStore,
  type PlanConfig,
  type AutoPushRebaseConflictState,
} from "../stores/plan-store.js";

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

function seed(config: PlanConfig | null, conflict: AutoPushRebaseConflictState | null): void {
  usePlanStore.setState({
    planConfigs: config ? { [PLAN]: config } : {},
    autoPushRebaseConflicts: conflict ? { [PLAN]: conflict } : {},
  });
}

afterEach(() => {
  cleanup();
  usePlanStore.setState({
    planConfigs: {},
    autoPushRebaseConflicts: {},
  });
});

describe("AutoPushRebaseConflictBanner", () => {
  it("does not render when pausedReason is null", () => {
    seed(defaultConfig({ pausedReason: null }), null);
    const { container } = render(<AutoPushRebaseConflictBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("does not render for unrelated pause reasons", () => {
    seed(defaultConfig({ pausedReason: "merge_conflict" }), null);
    const { container } = render(<AutoPushRebaseConflictBanner planName={PLAN} />);
    expect(container.innerHTML).toBe("");
  });

  it("renders branch + file list when pausedReason matches and conflict state is present", () => {
    seed(defaultConfig({ pausedReason: "auto_push_rebase_conflict" }), {
      branch: "master",
      files: ["Cargo.toml"],
      fileCount: 1,
    });
    render(<AutoPushRebaseConflictBanner planName={PLAN} />);
    expect(screen.getByText(/Auto-mode paused: post-merge rebase hit a conflict/i)).toBeTruthy();
    // Branch surfaces in the headline.
    expect(screen.getByText("master")).toBeTruthy();
    // File appears in the conflicting-file line.
    expect(screen.getByText(/Cargo\.toml/)).toBeTruthy();
    // Resolution hint that names "Resume".
    expect(screen.getByText(/click Resume/i)).toBeTruthy();
  });

  it("shows the +N more hint when fileCount > files.length", () => {
    seed(defaultConfig({ pausedReason: "auto_push_rebase_conflict" }), {
      branch: "master",
      files: ["a.rs", "b.rs"],
      fileCount: 12,
    });
    render(<AutoPushRebaseConflictBanner planName={PLAN} />);
    expect(screen.getByText(/\+10 more/)).toBeTruthy();
  });

  it("falls back to a 'see Activity log' hint when the file list is missing (post-reload)", () => {
    // Persistent half (pausedReason) survives reload; transient half
    // (files) does not. The banner should still render with the
    // fallback copy that points the operator at the audit log.
    seed(defaultConfig({ pausedReason: "auto_push_rebase_conflict" }), null);
    render(<AutoPushRebaseConflictBanner planName={PLAN} />);
    expect(screen.getByText(/Auto-mode paused: post-merge rebase hit a conflict/i)).toBeTruthy();
    expect(screen.getByText(/File list not in memory/i)).toBeTruthy();
    expect(screen.getByText(/Activity log/i)).toBeTruthy();
  });
});
