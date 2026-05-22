import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import {
  NewProjectModal,
  credentialMatchesHost,
  credentialCanCreateRemoteOn,
  previewWorkspacePath,
} from "./NewProjectModal.js";
import {
  useProjectsStore,
  type ProjectRow,
  type CreateProjectBody,
} from "../stores/projects-store.js";
import { useCredentialsStore, type CredentialRow } from "../stores/credentials-store.js";
import { useAuthStore } from "../stores/auth-store.js";

const ghPat: CredentialRow = {
  id: "cred-gh",
  name: "personal GH PAT",
  kind: "gh_pat",
  publicPart: null,
  hostHint: "github.com",
  scopes: "repo,workflow",
  ownerUserId: "u1",
  createdAt: "2026-05-22T00:00:00Z",
  usedByCount: 0,
};

const gitlabPat: CredentialRow = {
  id: "cred-gl",
  name: "lab GL PAT",
  kind: "gitlab_pat",
  publicPart: null,
  hostHint: "gitlab.com",
  scopes: "api",
  ownerUserId: "u1",
  createdAt: "2026-05-22T00:00:00Z",
  usedByCount: 0,
};

const sshKey: CredentialRow = {
  id: "cred-ssh",
  name: "work laptop key",
  kind: "ssh_key",
  publicPart: "ssh-ed25519 AAAA",
  hostHint: "github.com",
  scopes: null,
  ownerUserId: "u1",
  createdAt: "2026-05-22T00:00:00Z",
  usedByCount: 1,
};

function seedCredentials(credentials: CredentialRow[]) {
  useCredentialsStore.setState({
    credentials,
    loading: false,
    loaded: true,
    fetchCredentials: vi.fn().mockResolvedValue(undefined),
    createCredential: vi.fn(),
    deleteCredential: vi.fn().mockResolvedValue(undefined),
  });
}

function seedProjects(createProject: ReturnType<typeof vi.fn>) {
  useProjectsStore.setState({
    projects: [],
    loading: false,
    loaded: true,
    fetchProjects: vi.fn().mockResolvedValue(undefined),
    createProject,
  });
}

beforeEach(() => {
  useAuthStore.setState({
    user: { id: "u1", email: "u@example.com" },
    error: null,
    loading: false,
  });
});

afterEach(() => {
  cleanup();
  useCredentialsStore.getState().reset();
  useProjectsStore.getState().reset();
  useAuthStore.setState({ user: null, error: null, loading: false });
  vi.restoreAllMocks();
});

describe("credentialMatchesHost", () => {
  it("returns true when host is null or 'other'", () => {
    expect(credentialMatchesHost(sshKey, null)).toBe(true);
    expect(credentialMatchesHost(sshKey, "other")).toBe(true);
  });

  it("matches via hostHint substring", () => {
    expect(credentialMatchesHost(ghPat, "github")).toBe(true);
    expect(credentialMatchesHost(gitlabPat, "gitlab")).toBe(true);
  });

  it("matches a github PAT against gitlab host as false (hostHint disagrees)", () => {
    expect(credentialMatchesHost(ghPat, "gitlab")).toBe(false);
  });

  it("falls back to kind matching when hostHint is null", () => {
    const noHint: CredentialRow = { ...ghPat, hostHint: null };
    expect(credentialMatchesHost(noHint, "github")).toBe(true);
  });

  it("SSH keys are visible under both github and gitlab", () => {
    expect(credentialMatchesHost(sshKey, "github")).toBe(true);
    const sshNoHint: CredentialRow = { ...sshKey, hostHint: null };
    expect(credentialMatchesHost(sshNoHint, "gitlab")).toBe(true);
  });
});

describe("credentialCanCreateRemoteOn", () => {
  it("gh_pat → github only", () => {
    expect(credentialCanCreateRemoteOn(ghPat, "github")).toBe(true);
    expect(credentialCanCreateRemoteOn(ghPat, "gitlab")).toBe(false);
  });

  it("gitlab_pat → gitlab only", () => {
    expect(credentialCanCreateRemoteOn(gitlabPat, "gitlab")).toBe(true);
    expect(credentialCanCreateRemoteOn(gitlabPat, "github")).toBe(false);
  });

  it("ssh_key cannot mint a remote on either host", () => {
    expect(credentialCanCreateRemoteOn(sshKey, "github")).toBe(false);
    expect(credentialCanCreateRemoteOn(sshKey, "gitlab")).toBe(false);
  });
});

describe("previewWorkspacePath", () => {
  it("renders ~/<name> for a non-empty name", () => {
    expect(previewWorkspacePath("bar")).toBe("~/bar");
  });

  it("returns an empty string for whitespace / empty input", () => {
    expect(previewWorkspacePath("")).toBe("");
    expect(previewWorkspacePath("   ")).toBe("");
  });
});

describe("NewProjectModal — Clone tab", () => {
  it("autoparses the host / owner / name from the pasted URL", () => {
    seedCredentials([ghPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    const url = screen.getByTestId("repo-url-input") as HTMLInputElement;
    fireEvent.change(url, { target: { value: "https://github.com/foo/bar.git" } });

    const hint = screen.getByTestId("parsed-url-hint");
    expect(hint.textContent).toContain("github");
    expect(hint.textContent).toContain("foo");
    expect(hint.textContent).toContain("bar");
    // Name auto-fills with "bar"
    const name = screen.getByTestId("new-project-name") as HTMLInputElement;
    expect(name.value).toBe("bar");
  });

  it("previews the workspace path as ~/<name>", () => {
    seedCredentials([ghPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    const url = screen.getByTestId("repo-url-input") as HTMLInputElement;
    fireEvent.change(url, { target: { value: "https://github.com/foo/bar.git" } });

    expect(screen.getByTestId("workspace-path-preview").textContent).toContain("~/bar");
  });

  it("shows a fallback hint for an unparseable URL", () => {
    seedCredentials([ghPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    const url = screen.getByTestId("repo-url-input") as HTMLInputElement;
    fireEvent.change(url, { target: { value: "not a url at all" } });
    expect(screen.getByTestId("parse-fallback-hint")).toBeTruthy();
  });

  it("filters credentials by the parsed host", () => {
    seedCredentials([ghPat, gitlabPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    // Until a URL is pasted, no host filtering yet → both visible.
    let picker = screen.getByTestId("credential-picker") as HTMLSelectElement;
    let names = Array.from(picker.querySelectorAll("option")).map((o) => o.textContent ?? "");
    expect(names.some((n) => n.includes("personal GH PAT"))).toBe(true);
    expect(names.some((n) => n.includes("lab GL PAT"))).toBe(true);

    // After GitHub URL paste, the GitLab PAT is filtered out.
    fireEvent.change(screen.getByTestId("repo-url-input"), {
      target: { value: "https://github.com/foo/bar" },
    });
    picker = screen.getByTestId("credential-picker") as HTMLSelectElement;
    names = Array.from(picker.querySelectorAll("option")).map((o) => o.textContent ?? "");
    expect(names.some((n) => n.includes("personal GH PAT"))).toBe(true);
    expect(names.some((n) => n.includes("lab GL PAT"))).toBe(false);
  });

  it("submits with mode=clone + autoparsed fields", async () => {
    const createProject = vi.fn(
      async (body: CreateProjectBody): Promise<ProjectRow> => ({
        id: "new",
        name: body.name,
        repoUrl: body.mode === "clone" ? body.repo_url : "",
        host: "github",
        owner: "foo",
        defaultCredentialId: "cred-gh",
        workspacePath: "/home/cpo/bar",
        orgId: "default-org",
        createdAt: "2026-05-22T00:00:00Z",
      }),
    );
    seedCredentials([ghPat]);
    seedProjects(createProject);
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.change(screen.getByTestId("repo-url-input"), {
      target: { value: "https://github.com/foo/bar.git" },
    });
    fireEvent.change(screen.getByTestId("credential-picker"), {
      target: { value: "cred-gh" },
    });
    fireEvent.click(screen.getByTestId("new-project-submit"));

    await waitFor(() => {
      expect(createProject).toHaveBeenCalledTimes(1);
    });
    const body = createProject.mock.calls[0][0] as CreateProjectBody;
    expect(body.mode).toBe("clone");
    if (body.mode === "clone") {
      expect(body.name).toBe("bar");
      expect(body.repo_url).toBe("https://github.com/foo/bar.git");
      expect(body.host).toBe("github");
      expect(body.owner).toBe("foo");
      expect(body.credentialId).toBe("cred-gh");
    }
  });
});

describe("NewProjectModal — Create tab", () => {
  it("switches to the Create tab and renders host + visibility + owner fields", () => {
    seedCredentials([ghPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    expect(screen.getByTestId("create-host")).toBeTruthy();
    expect(screen.getByTestId("create-owner")).toBeTruthy();
    expect(screen.getByTestId("create-description")).toBeTruthy();
    expect(screen.getByTestId("create-private")).toBeTruthy();
    expect(screen.getByTestId("create-public")).toBeTruthy();
  });

  it("shows the SSH-only warning when an SSH key is picked on Create", () => {
    seedCredentials([sshKey]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    fireEvent.change(screen.getByTestId("credential-picker"), {
      target: { value: "cred-ssh" },
    });
    expect(screen.getByTestId("ssh-only-warning")).toBeTruthy();
  });

  it("hides the SSH-only warning when a PAT is picked on Create", () => {
    seedCredentials([ghPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    fireEvent.change(screen.getByTestId("credential-picker"), {
      target: { value: "cred-gh" },
    });
    expect(screen.queryByTestId("ssh-only-warning")).toBeNull();
  });

  it("clears the picked credential when the host changes", () => {
    seedCredentials([ghPat, gitlabPat]);
    seedProjects(vi.fn());
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    const picker = screen.getByTestId("credential-picker") as HTMLSelectElement;
    fireEvent.change(picker, { target: { value: "cred-gh" } });
    expect(picker.value).toBe("cred-gh");
    fireEvent.change(screen.getByTestId("create-host"), { target: { value: "gitlab" } });
    // Picker resets to empty after the host change.
    expect((screen.getByTestId("credential-picker") as HTMLSelectElement).value).toBe("");
  });

  it("refuses to submit when no credential is selected on Create", async () => {
    const createProject = vi.fn();
    seedCredentials([ghPat]);
    seedProjects(createProject);
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    fireEvent.change(screen.getByTestId("new-project-name"), {
      target: { value: "my-new-repo" },
    });
    fireEvent.click(screen.getByTestId("new-project-submit"));
    expect(createProject).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toMatch(/credential/i);
  });

  it("submits with mode=create + the picked host + private toggle", async () => {
    const createProject = vi.fn(
      async (body: CreateProjectBody): Promise<ProjectRow> => ({
        id: "new",
        name: body.name,
        repoUrl: "https://github.com/cpo/my-new-repo.git",
        host: "github",
        owner: null,
        defaultCredentialId: "cred-gh",
        workspacePath: "/home/cpo/my-new-repo",
        orgId: "default-org",
        createdAt: "2026-05-22T00:00:00Z",
      }),
    );
    seedCredentials([ghPat]);
    seedProjects(createProject);
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByRole("tab", { name: /Create new/i }));
    fireEvent.change(screen.getByTestId("new-project-name"), {
      target: { value: "my-new-repo" },
    });
    fireEvent.change(screen.getByTestId("credential-picker"), {
      target: { value: "cred-gh" },
    });
    fireEvent.click(screen.getByTestId("create-public"));
    fireEvent.click(screen.getByTestId("new-project-submit"));

    await waitFor(() => {
      expect(createProject).toHaveBeenCalledTimes(1);
    });
    const body = createProject.mock.calls[0][0] as CreateProjectBody;
    expect(body.mode).toBe("create");
    if (body.mode === "create") {
      expect(body.name).toBe("my-new-repo");
      expect(body.host).toBe("github");
      expect(body.private).toBe(false);
      expect(body.credentialId).toBe("cred-gh");
    }
  });
});

describe("NewProjectModal — submit validation", () => {
  it("refuses to submit without a name", () => {
    const createProject = vi.fn();
    seedCredentials([ghPat]);
    seedProjects(createProject);
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.click(screen.getByTestId("new-project-submit"));
    expect(createProject).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toMatch(/name/i);
  });

  it("refuses to submit without a repo URL on Clone", () => {
    const createProject = vi.fn();
    seedCredentials([ghPat]);
    seedProjects(createProject);
    render(<NewProjectModal onClose={() => {}} />);

    fireEvent.change(screen.getByTestId("new-project-name"), {
      target: { value: "manual-name" },
    });
    fireEvent.click(screen.getByTestId("new-project-submit"));
    expect(createProject).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toMatch(/URL/i);
  });

  it("renders the success banner + calls onClose after a successful clone", async () => {
    vi.useFakeTimers();
    try {
      const onClose = vi.fn();
      const createProject = vi.fn(
        async (body: CreateProjectBody): Promise<ProjectRow> => ({
          id: "new",
          name: body.name,
          repoUrl: "https://github.com/foo/bar.git",
          host: "github",
          owner: "foo",
          defaultCredentialId: "cred-gh",
          workspacePath: "/home/cpo/bar",
          orgId: "default-org",
          createdAt: "2026-05-22T00:00:00Z",
        }),
      );
      seedCredentials([ghPat]);
      seedProjects(createProject);
      render(<NewProjectModal onClose={onClose} />);

      fireEvent.change(screen.getByTestId("repo-url-input"), {
        target: { value: "https://github.com/foo/bar.git" },
      });
      fireEvent.change(screen.getByTestId("credential-picker"), {
        target: { value: "cred-gh" },
      });
      fireEvent.click(screen.getByTestId("new-project-submit"));

      // Drain the microtasks the await chains schedule. We use real timers
      // for the fetch resolution then advance the redirect setTimeout.
      await vi.runAllTimersAsync();
      expect(createProject).toHaveBeenCalledTimes(1);
      expect(onClose).toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});
