import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatCredentialKind,
  parseCredentialScopes,
  useCredentialsStore,
} from "./credentials-store.js";
import { useAuthStore } from "./auth-store.js";

interface FetchResult {
  status: number;
  body?: unknown;
}

type FetchHandler = (url: string, method: string, body: unknown) => FetchResult;

interface CallLog {
  url: string;
  method: string;
  body: unknown;
}

function installFetchMock(handler: FetchHandler) {
  const calls: CallLog[] = [];
  const fn = vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
    const url =
      typeof input === "string"
        ? input
        : input instanceof URL
          ? input.pathname + input.search
          : input.url;
    const method = init?.method ?? "GET";
    const rawBody = init?.body;
    const body =
      typeof rawBody === "string" && rawBody.length > 0 ? JSON.parse(rawBody) : undefined;
    calls.push({ url, method, body });
    const result = handler(url, method, body);
    return new Response(result.body !== undefined ? JSON.stringify(result.body) : null, {
      status: result.status,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fn);
  return { calls, fn };
}

beforeEach(() => {
  useAuthStore.setState({
    user: { id: "u1", email: "u@example.com" },
    error: null,
    loading: false,
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  useCredentialsStore.getState().reset();
  useAuthStore.setState({ user: null, error: null, loading: false });
});

describe("formatCredentialKind", () => {
  it("maps known kinds to human labels", () => {
    expect(formatCredentialKind("ssh_key")).toBe("SSH key");
    expect(formatCredentialKind("gh_pat")).toBe("GitHub PAT");
    expect(formatCredentialKind("gitlab_pat")).toBe("GitLab PAT");
  });

  it("falls back to the raw string on unknown kinds (forward-compat)", () => {
    expect(formatCredentialKind("future_kind")).toBe("future_kind");
  });
});

describe("parseCredentialScopes", () => {
  it("splits a comma-separated string into trimmed chips", () => {
    expect(parseCredentialScopes("repo, workflow,read:user")).toEqual([
      "repo",
      "workflow",
      "read:user",
    ]);
  });

  it("returns [] for null/undefined/empty", () => {
    expect(parseCredentialScopes(null)).toEqual([]);
    expect(parseCredentialScopes(undefined)).toEqual([]);
    expect(parseCredentialScopes("")).toEqual([]);
    expect(parseCredentialScopes("   ,  ,   ")).toEqual([]);
  });

  it("deduplicates repeated scopes", () => {
    expect(parseCredentialScopes("repo,repo,workflow")).toEqual(["repo", "workflow"]);
  });
});

describe("useCredentialsStore", () => {
  it("fetchCredentials populates the list and flips loaded=true", async () => {
    const sampleRow = {
      id: "cred-1",
      name: "work key",
      kind: "ssh_key",
      publicPart: "ssh-ed25519 AAAA…",
      hostHint: "github.com",
      scopes: null,
      ownerUserId: "u1",
      createdAt: "2026-05-22T00:00:00Z",
      usedByCount: 2,
    };
    const { calls } = installFetchMock((url, method) => {
      if (url === "/api/credentials" && method === "GET") {
        return { status: 200, body: { credentials: [sampleRow] } };
      }
      return { status: 404 };
    });

    await useCredentialsStore.getState().fetchCredentials();
    const s = useCredentialsStore.getState();
    expect(s.loaded).toBe(true);
    expect(s.loading).toBe(false);
    expect(s.credentials.length).toBe(1);
    expect(s.credentials[0].usedByCount).toBe(2);
    expect(calls.length).toBe(1);
    expect(calls[0].url).toBe("/api/credentials");
  });

  it("deduplicates concurrent fetchCredentials callers", async () => {
    const { calls } = installFetchMock((url, method) => {
      if (url === "/api/credentials" && method === "GET") {
        return { status: 200, body: { credentials: [] } };
      }
      return { status: 404 };
    });
    const a = useCredentialsStore.getState().fetchCredentials();
    const b = useCredentialsStore.getState().fetchCredentials();
    await Promise.all([a, b]);
    expect(calls.length).toBe(1);
  });

  it("createCredential POSTs and prepends the new row", async () => {
    const { calls } = installFetchMock((url, method, body) => {
      if (url === "/api/credentials" && method === "POST") {
        const b = body as { name: string; kind: string };
        return {
          status: 201,
          body: {
            id: "new-cred",
            name: b.name,
            kind: b.kind === "generate" ? "ssh_key" : b.kind,
            publicPart: "ssh-ed25519 AAAA fresh",
            hostHint: "github.com",
            scopes: null,
            ownerUserId: "u1",
            createdAt: "2026-05-22T00:01:00Z",
            usedByCount: 0,
            generated: b.kind === "generate",
          },
        };
      }
      return { status: 404 };
    });

    const res = await useCredentialsStore.getState().createCredential({
      name: "fresh key",
      kind: "generate",
    });
    expect(res.generated).toBe(true);
    expect(res.publicPart).toContain("ssh-ed25519");
    expect(useCredentialsStore.getState().credentials.length).toBe(1);
    expect(useCredentialsStore.getState().credentials[0].id).toBe("new-cred");
    expect(calls[0].method).toBe("POST");
    expect((calls[0].body as { kind: string }).kind).toBe("generate");
  });

  it("deleteCredential removes the row from the local list", async () => {
    useCredentialsStore.setState({
      credentials: [
        {
          id: "c1",
          name: "k1",
          kind: "ssh_key",
          publicPart: null,
          hostHint: null,
          scopes: null,
          ownerUserId: "u1",
          createdAt: "2026-05-22T00:00:00Z",
          usedByCount: 0,
        },
      ],
      loaded: true,
    });
    installFetchMock((url, method) => {
      if (url === "/api/credentials/c1" && method === "DELETE") {
        return { status: 200, body: { ok: true, id: "c1" } };
      }
      return { status: 404 };
    });
    await useCredentialsStore.getState().deleteCredential("c1");
    expect(useCredentialsStore.getState().credentials).toEqual([]);
  });

  it("deleteCredential propagates 409 credential_in_use without removing the row", async () => {
    useCredentialsStore.setState({
      credentials: [
        {
          id: "c2",
          name: "k2",
          kind: "ssh_key",
          publicPart: null,
          hostHint: null,
          scopes: null,
          ownerUserId: "u1",
          createdAt: "2026-05-22T00:00:00Z",
          usedByCount: 1,
        },
      ],
      loaded: true,
    });
    installFetchMock((url, method) => {
      if (url === "/api/credentials/c2" && method === "DELETE") {
        return {
          status: 409,
          body: {
            error: "credential_in_use",
            projectIds: ["p1"],
            message: "1 project(s) reference this credential",
          },
        };
      }
      return { status: 404 };
    });
    await expect(useCredentialsStore.getState().deleteCredential("c2")).rejects.toBeDefined();
    expect(useCredentialsStore.getState().credentials.length).toBe(1);
  });

  it("reset() clears credentials + loaded flag", async () => {
    useCredentialsStore.setState({
      credentials: [
        {
          id: "c1",
          name: "k1",
          kind: "ssh_key",
          publicPart: null,
          hostHint: null,
          scopes: null,
          ownerUserId: "u1",
          createdAt: "2026-05-22T00:00:00Z",
          usedByCount: 0,
        },
      ],
      loaded: true,
      loading: false,
    });
    useCredentialsStore.getState().reset();
    const s = useCredentialsStore.getState();
    expect(s.credentials).toEqual([]);
    expect(s.loaded).toBe(false);
    expect(s.loading).toBe(false);
  });
});
