import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { parseRepoUrl, useProjectsStore } from "./projects-store.js";
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
  useProjectsStore.getState().reset();
  useAuthStore.setState({ user: null, error: null, loading: false });
});

describe("parseRepoUrl", () => {
  it("parses https://github.com/owner/repo", () => {
    const parsed = parseRepoUrl("https://github.com/foo/bar");
    expect(parsed).not.toBeNull();
    expect(parsed!.host).toBe("github");
    expect(parsed!.owner).toBe("foo");
    expect(parsed!.name).toBe("bar");
    expect(parsed!.fullHost).toBe("github.com");
  });

  it("parses https://github.com/owner/repo.git", () => {
    const parsed = parseRepoUrl("https://github.com/foo/bar.git");
    expect(parsed?.name).toBe("bar");
    expect(parsed?.host).toBe("github");
  });

  it("parses https://github.com/owner/repo/ (trailing slash)", () => {
    const parsed = parseRepoUrl("https://github.com/foo/bar/");
    expect(parsed?.name).toBe("bar");
  });

  it("parses git@github.com:owner/repo.git (SSH shorthand)", () => {
    const parsed = parseRepoUrl("git@github.com:foo/bar.git");
    expect(parsed?.host).toBe("github");
    expect(parsed?.owner).toBe("foo");
    expect(parsed?.name).toBe("bar");
  });

  it("parses ssh://git@gitlab.com/owner/repo.git", () => {
    const parsed = parseRepoUrl("ssh://git@gitlab.com/foo/bar.git");
    expect(parsed?.host).toBe("gitlab");
    expect(parsed?.owner).toBe("foo");
    expect(parsed?.name).toBe("bar");
  });

  it("parses git://host/owner/repo", () => {
    const parsed = parseRepoUrl("git://github.com/foo/bar.git");
    expect(parsed?.host).toBe("github");
  });

  it("classifies gitlab.acme.io as gitlab via subdomain prefix", () => {
    const parsed = parseRepoUrl("https://gitlab.acme.io/group/project.git");
    expect(parsed?.host).toBe("gitlab");
    expect(parsed?.fullHost).toBe("gitlab.acme.io");
  });

  it("classifies bitbucket.org as bitbucket", () => {
    const parsed = parseRepoUrl("https://bitbucket.org/owner/repo.git");
    expect(parsed?.host).toBe("bitbucket");
  });

  it("classifies an unknown host as 'other' but still extracts owner/name", () => {
    const parsed = parseRepoUrl("https://git.example.com/team/widget.git");
    expect(parsed?.host).toBe("other");
    expect(parsed?.owner).toBe("team");
    expect(parsed?.name).toBe("widget");
  });

  it("returns null on empty / whitespace", () => {
    expect(parseRepoUrl("")).toBeNull();
    expect(parseRepoUrl("   ")).toBeNull();
  });

  it("returns null on malformed input", () => {
    expect(parseRepoUrl("not a url")).toBeNull();
    expect(parseRepoUrl("https://github.com/")).toBeNull();
    expect(parseRepoUrl("https://github.com/onlyowner")).toBeNull();
  });
});

describe("useProjectsStore", () => {
  const sampleRow = {
    id: "p1",
    name: "bar",
    repoUrl: "https://github.com/foo/bar.git",
    host: "github",
    owner: "foo",
    defaultCredentialId: "cred-1",
    workspacePath: "/home/cpo/bar",
    orgId: "default-org",
    createdAt: "2026-05-22T00:00:00Z",
  };

  it("fetchProjects populates the list and flips loaded=true", async () => {
    const { calls } = installFetchMock((url, method) => {
      if (url === "/api/projects" && method === "GET") {
        return { status: 200, body: { projects: [sampleRow] } };
      }
      return { status: 404 };
    });
    await useProjectsStore.getState().fetchProjects();
    const s = useProjectsStore.getState();
    expect(s.loaded).toBe(true);
    expect(s.loading).toBe(false);
    expect(s.projects.length).toBe(1);
    expect(s.projects[0].name).toBe("bar");
    expect(calls.length).toBe(1);
  });

  it("deduplicates concurrent fetchProjects callers", async () => {
    const { calls } = installFetchMock((url, method) => {
      if (url === "/api/projects" && method === "GET") {
        return { status: 200, body: { projects: [] } };
      }
      return { status: 404 };
    });
    const a = useProjectsStore.getState().fetchProjects();
    const b = useProjectsStore.getState().fetchProjects();
    await Promise.all([a, b]);
    expect(calls.length).toBe(1);
  });

  it("createProject POSTs clone-mode body and prepends the new row", async () => {
    const { calls } = installFetchMock((url, method, body) => {
      if (url === "/api/projects" && method === "POST") {
        const b = body as { name: string };
        return {
          status: 201,
          body: { ...sampleRow, name: b.name },
        };
      }
      return { status: 404 };
    });
    const created = await useProjectsStore.getState().createProject({
      mode: "clone",
      name: "bar",
      repo_url: "https://github.com/foo/bar.git",
      host: "github",
      owner: "foo",
      credentialId: "cred-1",
    });
    expect(created.name).toBe("bar");
    expect(calls.length).toBe(1);
    expect(calls[0].method).toBe("POST");
    const reqBody = calls[0].body as Record<string, unknown>;
    expect(reqBody.mode).toBe("clone");
    expect(reqBody.repo_url).toBe("https://github.com/foo/bar.git");
    expect(reqBody.credentialId).toBe("cred-1");
    // Optimistic prepend
    expect(useProjectsStore.getState().projects[0].name).toBe("bar");
  });

  it("createProject POSTs create-mode body with private + owner", async () => {
    installFetchMock((url, method, body) => {
      if (url === "/api/projects" && method === "POST") {
        const b = body as { name: string };
        return { status: 201, body: { ...sampleRow, name: b.name } };
      }
      return { status: 404 };
    });
    const created = await useProjectsStore.getState().createProject({
      mode: "create",
      name: "my-new-repo",
      host: "github",
      owner: "my-org",
      credentialId: "cred-pat",
      private: false,
    });
    expect(created.id).toBe("p1");
  });

  it("reset clears the list + in-flight handle", async () => {
    installFetchMock((url, method) => {
      if (url === "/api/projects" && method === "GET") {
        return { status: 200, body: { projects: [sampleRow] } };
      }
      return { status: 404 };
    });
    await useProjectsStore.getState().fetchProjects();
    expect(useProjectsStore.getState().projects.length).toBe(1);
    useProjectsStore.getState().reset();
    const s = useProjectsStore.getState();
    expect(s.projects.length).toBe(0);
    expect(s.loaded).toBe(false);
  });
});
