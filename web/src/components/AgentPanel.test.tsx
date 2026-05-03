import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { DiffView } from "./AgentPanel.js";
import { useAgentStore, type AgentDiff } from "../stores/agent-store.js";

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
  return render(
    <DiffView agentId={AGENT_ID} canMerge={true} sourceBranch={null} />
  );
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
    expect(
      screen.queryByRole("button", { name: /Choose target branch/ })
    ).toBeNull();
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
    const featureItem = await screen.findByRole("button", { name: "feature/x" });
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

    // Sanity-check: dropdown is open (feature/x item visible).
    await screen.findByRole("button", { name: "feature/x" });

    // Click somewhere outside the dropdown wrapper.
    fireEvent.mouseDown(document.body);

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "feature/x" })
      ).toBeNull();
    });
    expect(mergeAgentBranch).not.toHaveBeenCalled();
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
});
