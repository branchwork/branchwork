import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { StaleBranchesButton } from "./PlanBoard.js";

const PLAN = "demo-plan";

interface FakeBranch {
  name: string;
  sha: string | null;
  commitsAheadOfTrunk: number | null;
  lastCommitAgeSecs: number | null;
  agentId: string | null;
  agentStatus: string | null;
  hasUniqueCommits: boolean;
}

function branch(overrides: Partial<FakeBranch> = {}): FakeBranch {
  return {
    name: `branchwork/${PLAN}/1.1`,
    sha: "abc1234",
    commitsAheadOfTrunk: 0,
    lastCommitAgeSecs: 600,
    agentId: null,
    agentStatus: null,
    hasUniqueCommits: false,
    ...overrides,
  };
}

interface PurgeResult {
  branch: string;
  ok: boolean;
  error?: string;
}

interface FetchInstall {
  calls: { url: string; method: string; body: unknown }[];
  setListPayload: (branches: FakeBranch[]) => void;
  setPurgePayload: (results: PurgeResult[]) => void;
}

function installFetch(initial: FakeBranch[]): FetchInstall {
  let listPayload: FakeBranch[] = initial;
  let purgePayload: PurgeResult[] = [];
  const calls: { url: string; method: string; body: unknown }[] = [];
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const method = init?.method ?? "GET";
    let body: unknown = null;
    if (typeof init?.body === "string") {
      try {
        body = JSON.parse(init.body);
      } catch {
        body = init.body;
      }
    }
    calls.push({ url, method, body });
    if (url.endsWith("/branches/stale") && method === "GET") {
      return new Response(JSON.stringify({ branches: listPayload }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    if (url.endsWith("/branches/stale/purge") && method === "POST") {
      return new Response(JSON.stringify({ results: purgePayload }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    return new Response("not found", { status: 404 });
  });
  vi.stubGlobal("fetch", fn);
  return {
    calls,
    setListPayload: (b: FakeBranch[]) => {
      listPayload = b;
    },
    setPurgePayload: (r: PurgeResult[]) => {
      purgePayload = r;
    },
  };
}

beforeEach(() => {
  vi.unstubAllGlobals();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("StaleBranchesButton", () => {
  async function openModal(branches: FakeBranch[]): Promise<FetchInstall> {
    const install = installFetch(branches);
    render(<StaleBranchesButton planName={PLAN} onDone={() => {}} />);
    fireEvent.click(screen.getByRole("button", { name: /Clean Branches/ }));
    await waitFor(() => {
      // Modal title appears once load resolves and the table renders.
      expect(screen.getByText(new RegExp(`Stale branches for ${PLAN}`))).toBeTruthy();
    });
    return install;
  }

  it("default-selects empty branches and skips risky ones", async () => {
    await openModal([
      branch({
        name: `branchwork/${PLAN}/1.1`,
        commitsAheadOfTrunk: 0,
        hasUniqueCommits: false,
      }),
      branch({
        name: `branchwork/${PLAN}/1.2`,
        commitsAheadOfTrunk: 3,
        hasUniqueCommits: true,
      }),
    ]);

    const empty = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.1`,
    }) as HTMLInputElement;
    const risky = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.2`,
    }) as HTMLInputElement;
    expect(empty.checked).toBe(true);
    expect(empty.disabled).toBe(false);
    expect(risky.checked).toBe(false);
    expect(risky.disabled).toBe(true);
  });

  it("disables and skips selection for live-agent rows even when the branch is empty", async () => {
    await openModal([
      branch({
        name: `branchwork/${PLAN}/1.1`,
        agentId: "agent-live-1",
        agentStatus: "running",
      }),
      branch({
        name: `branchwork/${PLAN}/1.2`,
        agentId: "agent-done-1",
        agentStatus: "completed",
      }),
    ]);

    const live = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.1`,
    }) as HTMLInputElement;
    const done = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.2`,
    }) as HTMLInputElement;
    expect(live.disabled).toBe(true);
    expect(live.checked).toBe(false);
    expect(done.disabled).toBe(false);
    expect(done.checked).toBe(true);
    // Status badge surfaces the live state next to the agent id.
    expect(screen.getByText("running")).toBeTruthy();
    expect(screen.queryByText("completed")).toBeNull();
  });

  it("force toggle re-enables risky checkboxes but not live-agent ones", async () => {
    await openModal([
      branch({
        name: `branchwork/${PLAN}/1.1`,
        commitsAheadOfTrunk: 4,
        hasUniqueCommits: true,
      }),
      branch({
        name: `branchwork/${PLAN}/1.2`,
        agentId: "agent-live-2",
        agentStatus: "starting",
      }),
    ]);

    fireEvent.click(screen.getByRole("checkbox", { name: /Allow branches with unique commits/ }));

    const risky = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.1`,
    }) as HTMLInputElement;
    const live = screen.getByRole("checkbox", {
      name: `Select branch branchwork/${PLAN}/1.2`,
    }) as HTMLInputElement;
    expect(risky.disabled).toBe(false);
    expect(live.disabled).toBe(true);
  });

  it("Delete posts selected branches with force=false by default", async () => {
    const install = await openModal([
      branch({ name: `branchwork/${PLAN}/1.1` }),
      branch({ name: `branchwork/${PLAN}/1.2` }),
    ]);
    install.setPurgePayload([
      { branch: `branchwork/${PLAN}/1.1`, ok: true },
      { branch: `branchwork/${PLAN}/1.2`, ok: true },
    ]);

    fireEvent.click(screen.getByRole("button", { name: /Delete 2/ }));

    await waitFor(() => {
      const post = install.calls.find(
        (c) => c.url.endsWith("/branches/stale/purge") && c.method === "POST",
      );
      expect(post).toBeDefined();
      expect(post?.body).toEqual({
        branches: [`branchwork/${PLAN}/1.1`, `branchwork/${PLAN}/1.2`],
        force: false,
      });
    });
  });
});
