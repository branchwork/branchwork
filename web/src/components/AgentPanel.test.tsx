import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import axe from "axe-core";
import { MemoryRouter } from "react-router-dom";
import { AgentPanel, DiffView } from "./AgentPanel.js";
import { useAgentStore, type Agent, type AgentDiff } from "../stores/agent-store.js";
import { usePlanStore } from "../stores/plan-store.js";
import { useSettingsStore } from "../stores/settings-store.js";

const AGENT_ID = "agent-1";

const SAMPLE_DIFF: AgentDiff = {
  diff: [
    "diff --git a/a.ts b/a.ts",
    "index 0000000..1111111 100644",
    "--- a/a.ts",
    "+++ b/a.ts",
    "@@ -1,1 +1,1 @@",
    "-old",
    "+new",
    "",
  ].join("\n"),
  stat: "",
  files: ["a.ts"],
  base_commit: "abc1234567",
};

type MergeTargets = { default: string | null; available: string[] };

function seedStore(overrides: {
  fetchMergeTargets: ReturnType<typeof vi.fn>;
  mergeAgentBranch: ReturnType<typeof vi.fn>;
}) {
  useAgentStore.setState({
    agentDiffs: { [AGENT_ID]: SAMPLE_DIFF },
    fetchAgentDiff: vi.fn().mockResolvedValue(undefined),
    discardAgentBranch: vi.fn().mockResolvedValue({ ok: true }),
    fetchMergeTargets: overrides.fetchMergeTargets,
    mergeAgentBranch: overrides.mergeAgentBranch,
  });
}

function renderDiffView() {
  return render(<DiffView agentId={AGENT_ID} canMerge={true} sourceBranch={null} />);
}

describe("AgentPanel DiffView merge dropdown", () => {
  let mergeAgentBranch: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    mergeAgentBranch = vi.fn().mockResolvedValue({ ok: true });
  });

  afterEach(() => {
    cleanup();
  });

  it("does not render the chevron when there are no alternative branches", async () => {
    const targets: MergeTargets = { default: "master", available: [] };
    const fetchMergeTargets = vi.fn().mockResolvedValue(targets);
    seedStore({ fetchMergeTargets, mergeAgentBranch });

    renderDiffView();

    // Wait for the main merge button to settle with the resolved default.
    await screen.findByRole("button", { name: /Merge into master/ });

    // Chevron is the only button labeled "Choose target branch".
    expect(screen.queryByRole("button", { name: /Choose target branch/ })).toBeNull();
  });

  it("renders the chevron and routes a dropdown selection through to mergeAgentBranch", async () => {
    const targets: MergeTargets = {
      default: "master",
      available: ["feature/x"],
    };
    const fetchMergeTargets = vi.fn().mockResolvedValue(targets);
    seedStore({ fetchMergeTargets, mergeAgentBranch });

    renderDiffView();

    const chevron = await screen.findByRole("button", {
      name: /Choose target branch/,
    });

    fireEvent.click(chevron);
    // Migrated to the `Dropdown` primitive — items now carry
    // `role="menuitem"` (audit §8 fix). Older tests queried by
    // `role="button"`.
    const featureItem = await screen.findByRole("menuitem", { name: "feature/x" });
    fireEvent.click(featureItem);

    // Main merge button now reads "Merge into feature/x".
    const mergeButton = await screen.findByRole("button", {
      name: /Merge into feature\/x/,
    });
    fireEvent.click(mergeButton);

    const yes = await screen.findByRole("button", { name: "Yes" });
    fireEvent.click(yes);

    await waitFor(() => {
      expect(mergeAgentBranch).toHaveBeenCalledTimes(1);
    });
    expect(mergeAgentBranch).toHaveBeenCalledWith(AGENT_ID, "feature/x");
  });

  it("closes the dropdown on outside click without calling merge", async () => {
    const targets: MergeTargets = {
      default: "master",
      available: ["feature/x"],
    };
    const fetchMergeTargets = vi.fn().mockResolvedValue(targets);
    seedStore({ fetchMergeTargets, mergeAgentBranch });

    renderDiffView();

    const chevron = await screen.findByRole("button", {
      name: /Choose target branch/,
    });
    fireEvent.click(chevron);

    // Sanity-check: dropdown is open (feature/x menuitem visible).
    await screen.findByRole("menuitem", { name: "feature/x" });

    // Click somewhere outside the dropdown wrapper.
    fireEvent.mouseDown(document.body);

    await waitFor(() => {
      expect(screen.queryByRole("menuitem", { name: "feature/x" })).toBeNull();
    });
    expect(mergeAgentBranch).not.toHaveBeenCalled();
  });

  it("animated header status dot exposes its state to screen readers via sr-only", () => {
    const agent: Agent = {
      id: AGENT_ID,
      session_id: "sess-1",
      pid: null,
      parent_agent_id: null,
      plan_name: null,
      task_id: null,
      cwd: "/tmp/wd",
      status: "running",
      // Non-pty avoids mounting xterm in jsdom — this test is about the
      // header dot's accessible name, not the terminal.
      mode: "stream-json",
      prompt: null,
      started_at: new Date().toISOString(),
      finished_at: null,
      last_tool: null,
      last_activity_at: null,
      base_commit: null,
      branch: null,
      source_branch: null,
      cost_usd: null,
      driver: "claude",
    };
    useAgentStore.setState({
      agents: [agent],
      selectedAgentId: AGENT_ID,
      agentOutput: { [AGENT_ID]: [] },
      fetchAgentOutput: vi.fn().mockResolvedValue(undefined),
    });
    usePlanStore.setState({ plans: [] });
    useSettingsStore.setState({ drivers: [] });

    render(
      <MemoryRouter>
        <AgentPanel />
      </MemoryRouter>,
    );
    // sr-only span sits next to the colour-only dot so SRs announce
    // the running state. The visible status text further along the
    // row is fine; this is the assertion that color-only conveyance
    // got a text equivalent.
    expect(screen.getByText(/Status:\s*running/i)).toBeTruthy();
  });

  it("default click (no dropdown interaction) calls mergeAgentBranch with no target", async () => {
    const targets: MergeTargets = {
      default: "master",
      available: ["feature/x"],
    };
    const fetchMergeTargets = vi.fn().mockResolvedValue(targets);
    seedStore({ fetchMergeTargets, mergeAgentBranch });

    renderDiffView();

    const mergeButton = await screen.findByRole("button", {
      name: /Merge into master/,
    });
    fireEvent.click(mergeButton);

    const yes = await screen.findByRole("button", { name: "Yes" });
    fireEvent.click(yes);

    await waitFor(() => {
      expect(mergeAgentBranch).toHaveBeenCalledTimes(1);
    });
    // No dropdown interaction → selectedTarget remains null → second arg is undefined.
    // The agent-store turns undefined target into a `{}` body in the POST call.
    expect(mergeAgentBranch).toHaveBeenCalledWith(AGENT_ID, undefined);
  });

  it("axe-core reports zero icon-button-name violations on the diff footer", async () => {
    const targets: MergeTargets = {
      default: "master",
      available: ["feature/x"],
    };
    const fetchMergeTargets = vi.fn().mockResolvedValue(targets);
    seedStore({ fetchMergeTargets, mergeAgentBranch });

    const { container } = renderDiffView();
    // Wait for the footer's main merge button to render so axe sees
    // the chevron + dropdown alongside it.
    await screen.findByRole("button", { name: /Merge into master/ });

    const results = await axe.run(container, {
      runOnly: { type: "tag", values: ["wcag2a", "wcag2aa"] },
      // jsdom has no layout — color-contrast is not checkable here.
      rules: { "color-contrast": { enabled: false } },
    });
    expect(results.violations).toEqual([]);
  });
});
