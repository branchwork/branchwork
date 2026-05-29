import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PromotionsTab } from "./PromotionsTab.js";
import { useLearningsStore } from "../../stores/learnings-store.js";
import { useProjectsStore } from "../../stores/projects-store.js";
import { useCredentialsStore } from "../../stores/credentials-store.js";
import { useAuthStore } from "../../stores/auth-store.js";

interface Call {
  url: string;
  method: string;
  body: string | null;
}

let calls: Call[];
let candidatesBody: unknown;
let projectsBody: unknown;
let credentialsBody: unknown;
let promoteStatus: number;
let promoteBody: unknown;

function candidate(overrides: Record<string, unknown> = {}) {
  return {
    learningId: "L-1",
    orgId: "default-org",
    kind: "feedback",
    category: "ci",
    slug: "deploy-by-sha",
    bodyMd: "**Why:** because.",
    hitCount: 6,
    threshold: 5,
    windowDays: 30,
    createdAt: "2026-05-23T00:00:00Z",
    promotedPrUrl: null,
    promotedAt: null,
    appendPreview: "### Deploy by sha\n\n**Why:** because.",
    ...overrides,
  };
}

function installFetchMock() {
  vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const method = init?.method ?? "GET";
    calls.push({ url, method, body: (init?.body as string) ?? null });
    if (url.includes("/api/learnings/promotion-candidates")) {
      return new Response(JSON.stringify(candidatesBody), { status: 200 });
    }
    if (url.includes("/promote")) {
      return new Response(JSON.stringify(promoteBody), { status: promoteStatus });
    }
    if (url.includes("/api/projects")) {
      return new Response(JSON.stringify(projectsBody), { status: 200 });
    }
    if (url.includes("/api/credentials")) {
      return new Response(JSON.stringify(credentialsBody), { status: 200 });
    }
    return new Response("not found", { status: 404 });
  });
}

beforeEach(() => {
  calls = [];
  candidatesBody = { items: [] };
  projectsBody = { projects: [] };
  credentialsBody = { credentials: [] };
  promoteStatus = 200;
  promoteBody = { ok: true, prUrl: "https://github.com/o/r/pull/5", prNumber: 5 };
  useAuthStore.setState({
    user: { id: "u-1", email: "a@b.c", orgId: "default-org" },
  });
  installFetchMock();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  useAuthStore.setState({ user: null });
  useLearningsStore.getState().reset();
  useProjectsStore.getState().reset();
  useCredentialsStore.getState().reset();
});

const githubProject = {
  id: "proj-1",
  name: "my-repo",
  repoUrl: "https://github.com/o/r.git",
  host: "github",
  owner: "o",
  defaultCredentialId: "cred-1",
};

describe("PromotionsTab", () => {
  it("shows the empty state when there are no candidates", async () => {
    render(<PromotionsTab />);
    expect(await screen.findByTestId("promotions-empty")).toBeTruthy();
  });

  it("renders a candidate with the diff preview and a Promote button", async () => {
    candidatesBody = { items: [candidate()] };
    projectsBody = { projects: [githubProject] };
    render(<PromotionsTab />);

    expect(await screen.findByTestId("promotion-candidate-L-1")).toBeTruthy();
    // Diff preview shows the appended block: the H3-wrapped rule as a
    // `+` addition (the `## Other rules` anchor text also appears in the
    // header copy, so assert the unique added line instead).
    expect(screen.getByText(/\+ ### Deploy by sha/)).toBeTruthy();
    // Promote button present + enabled (a github project exists).
    const btn = screen.getByTestId("promotion-approve-L-1") as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });

  it("warns + disables Promote when no GitHub project is configured", async () => {
    candidatesBody = { items: [candidate()] };
    projectsBody = { projects: [] };
    render(<PromotionsTab />);
    await screen.findByTestId("promotion-candidate-L-1");
    const btn = screen.getByTestId("promotion-approve-L-1") as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
  });

  it("clicking Promote POSTs the selected project to the promote endpoint", async () => {
    candidatesBody = { items: [candidate()] };
    projectsBody = { projects: [githubProject] };
    render(<PromotionsTab />);
    await screen.findByTestId("promotion-candidate-L-1");

    fireEvent.click(screen.getByTestId("promotion-approve-L-1"));

    await waitFor(() => {
      expect(
        calls.some((c) => c.method === "POST" && c.url.includes("/api/learnings/L-1/promote")),
      ).toBe(true);
    });
    const post = calls.find((c) => c.method === "POST")!;
    expect(JSON.parse(post.body!)).toEqual({ projectId: "proj-1" });
  });

  it("renders a PR link instead of the button for an already-promoted candidate", async () => {
    candidatesBody = {
      items: [candidate({ promotedPrUrl: "https://github.com/o/r/pull/9" })],
    };
    projectsBody = { projects: [githubProject] };
    render(<PromotionsTab />);
    await screen.findByTestId("promotion-candidate-L-1");

    const link = screen.getByTestId("promotion-pr-link-L-1") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://github.com/o/r/pull/9");
    // No Promote button on a promoted row.
    expect(screen.queryByTestId("promotion-approve-L-1")).toBeNull();
  });

  it("surfaces the server error inline when promote fails", async () => {
    candidatesBody = { items: [candidate()] };
    projectsBody = { projects: [githubProject] };
    promoteStatus = 422;
    promoteBody = { error: "credential_not_pat", message: "needs a GitHub PAT credential" };
    render(<PromotionsTab />);
    await screen.findByTestId("promotion-candidate-L-1");

    fireEvent.click(screen.getByTestId("promotion-approve-L-1"));
    expect(await screen.findByText(/needs a GitHub PAT credential/)).toBeTruthy();
  });
});
