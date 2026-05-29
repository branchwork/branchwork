import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  clearInFlightForTests,
  lastNLines,
  useLearningsStore,
  type PendingLearningRow,
  type PromotionCandidate,
} from "./learnings-store.js";
import { useAuthStore } from "./auth-store.js";

function fakeCandidate(overrides: Partial<PromotionCandidate> = {}): PromotionCandidate {
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

function fakeRow(overrides: Partial<PendingLearningRow> = {}): PendingLearningRow {
  return {
    id: 1,
    agentId: "agent-1",
    agentStatus: "running",
    planName: "plan-a",
    taskNumber: "1.4",
    branch: "branchwork/plan-a/1.4",
    runId: "12345",
    runUrl: "https://example.test/runs/12345",
    workflow: "tests.yml",
    conclusion: "failure",
    failedJob: null,
    summary: "tests workflow failed",
    observedAt: "2026-05-23T00:00:00Z",
    ...overrides,
  };
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function textResponse(body: string, status = 200): Response {
  return new Response(body, {
    status,
    headers: { "Content-Type": "text/plain" },
  });
}

beforeEach(() => {
  // Seed an authenticated user so the api.ts 401 interceptor does not
  // collateral-damage our test fetches.
  useAuthStore.setState({
    user: {
      id: "test-user",
      email: "t@example.test",
      orgId: "default-org",
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  useAuthStore.setState({ user: null });
  useLearningsStore.getState().reset();
  clearInFlightForTests();
});

describe("learnings-store.fetchPending", () => {
  it("populates items on successful fetch", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ items: [fakeRow({ id: 7 })] })),
    );
    await useLearningsStore.getState().fetchPending();
    const s = useLearningsStore.getState();
    expect(s.loaded).toBe(true);
    expect(s.loading).toBe(false);
    expect(s.items.length).toBe(1);
    expect(s.items[0].id).toBe(7);
  });

  it("clears items on empty response", async () => {
    useLearningsStore.setState({ items: [fakeRow()], loaded: true });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ items: [] })),
    );
    await useLearningsStore.getState().fetchPending();
    expect(useLearningsStore.getState().items.length).toBe(0);
  });

  it("dedupes concurrent fetches into a single round trip", async () => {
    let resolveFetch!: (r: Response) => void;
    const fetchMock = vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          resolveFetch = resolve;
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const first = useLearningsStore.getState().fetchPending();
    const second = useLearningsStore.getState().fetchPending();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    resolveFetch(jsonResponse({ items: [fakeRow()] }));
    await Promise.all([first, second]);
  });

  it("leaves loaded=false on failure so a retry re-fires", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new Error("network");
      }),
    );
    await useLearningsStore.getState().fetchPending();
    const s = useLearningsStore.getState();
    expect(s.loaded).toBe(false);
    expect(s.loading).toBe(false);
  });
});

describe("learnings-store.fetchLog", () => {
  it("caches the log on success and trims to last 100 lines", async () => {
    // Build a 250-line log; expect the cache to hold only the last 100.
    const lines = Array.from({ length: 250 }, (_, i) => `line ${i}`);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => textResponse(lines.join("\n"))),
    );
    await useLearningsStore.getState().fetchLog(42);
    const entry = useLearningsStore.getState().logsByEventId[42];
    expect(entry?.status).toBe("ok");
    const cached = entry?.text ?? "";
    const cachedLines = cached.split("\n");
    expect(cachedLines.length).toBe(100);
    expect(cachedLines[0]).toBe("line 150");
    expect(cachedLines[99]).toBe("line 249");
  });

  it("records error on non-OK response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => textResponse("oh no", 404)),
    );
    await useLearningsStore.getState().fetchLog(99);
    const entry = useLearningsStore.getState().logsByEventId[99];
    expect(entry?.status).toBe("error");
    expect(entry?.error).toBe("oh no");
  });

  it("dedupes concurrent log fetches for the same event id", async () => {
    let resolveFetch!: (r: Response) => void;
    const fetchMock = vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          resolveFetch = resolve;
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const a = useLearningsStore.getState().fetchLog(1);
    const b = useLearningsStore.getState().fetchLog(1);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    resolveFetch(textResponse("ok"));
    await Promise.all([a, b]);
  });
});

describe("learnings-store.fetchCandidates", () => {
  it("populates candidates on success", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ items: [fakeCandidate({ learningId: "L-9" })] })),
    );
    await useLearningsStore.getState().fetchCandidates();
    const s = useLearningsStore.getState();
    expect(s.candidatesLoaded).toBe(true);
    expect(s.candidatesLoading).toBe(false);
    expect(s.candidates.map((c) => c.learningId)).toEqual(["L-9"]);
  });

  it("leaves candidatesLoaded=false on failure so a retry re-fires", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new Error("network");
      }),
    );
    await useLearningsStore.getState().fetchCandidates();
    expect(useLearningsStore.getState().candidatesLoaded).toBe(false);
  });
});

describe("learnings-store.promote", () => {
  it("POSTs the promote request and optimistically stamps the row", async () => {
    useLearningsStore.setState({ candidates: [fakeCandidate({ learningId: "L-1" })] });
    const calls: { url: string; method: string; body: unknown }[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? "GET";
        calls.push({ url, method, body: init?.body });
        if (method === "POST") {
          return jsonResponse({
            ok: true,
            prUrl: "https://github.com/o/r/pull/5",
            prNumber: 5,
          });
        }
        // The fire-and-forget refetch — keep the row promoted.
        return jsonResponse({
          items: [
            fakeCandidate({ learningId: "L-1", promotedPrUrl: "https://github.com/o/r/pull/5" }),
          ],
        });
      }),
    );

    const res = await useLearningsStore.getState().promote("L-1", "proj-1", "cred-1");
    expect(res.prUrl).toBe("https://github.com/o/r/pull/5");

    // The POST went to the right URL with both ids in the body.
    const post = calls.find((c) => c.method === "POST")!;
    expect(post.url).toContain("/api/learnings/L-1/promote");
    const body = JSON.parse(String(post.body));
    expect(body).toEqual({ projectId: "proj-1", credentialId: "cred-1" });

    // Optimistic stamp landed (independent of the refetch timing).
    expect(useLearningsStore.getState().candidates[0].promotedPrUrl).toBe(
      "https://github.com/o/r/pull/5",
    );
  });

  it("omits credentialId from the body when not supplied", async () => {
    useLearningsStore.setState({ candidates: [fakeCandidate()] });
    let postBody: unknown;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
        if ((init?.method ?? "GET") === "POST") {
          postBody = init?.body;
          return jsonResponse({ ok: true, prUrl: "u", prNumber: 1 });
        }
        return jsonResponse({ items: [] });
      }),
    );
    await useLearningsStore.getState().promote("L-1", "proj-1");
    expect(JSON.parse(String(postBody))).toEqual({ projectId: "proj-1" });
  });

  it("propagates the error and does not stamp on failure", async () => {
    useLearningsStore.setState({ candidates: [fakeCandidate({ learningId: "L-1" })] });
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => jsonResponse({ error: "unsupported_host" }, 422)),
    );
    await expect(useLearningsStore.getState().promote("L-1", "proj-1")).rejects.toBeTruthy();
    expect(useLearningsStore.getState().candidates[0].promotedPrUrl).toBeNull();
  });
});

describe("learnings-store.reset", () => {
  it("wipes items + cache + loaded flags + candidates", () => {
    useLearningsStore.setState({
      items: [fakeRow()],
      loaded: true,
      logsByEventId: { 1: { status: "ok", text: "hi", error: null } },
      candidates: [fakeCandidate()],
      candidatesLoaded: true,
    });
    useLearningsStore.getState().reset();
    const s = useLearningsStore.getState();
    expect(s.items).toEqual([]);
    expect(s.loaded).toBe(false);
    expect(s.logsByEventId).toEqual({});
    expect(s.candidates).toEqual([]);
    expect(s.candidatesLoaded).toBe(false);
  });
});

describe("lastNLines", () => {
  it("returns the input unchanged when there are <= n lines", () => {
    expect(lastNLines("a\nb\nc", 5)).toBe("a\nb\nc");
  });

  it("returns the last n lines when there are more", () => {
    expect(lastNLines("a\nb\nc\nd\ne", 2)).toBe("d\ne");
  });

  it("returns the input unchanged when empty", () => {
    expect(lastNLines("", 10)).toBe("");
  });

  it("returns the input unchanged when n is zero", () => {
    expect(lastNLines("a\nb", 0)).toBe("a\nb");
  });
});
