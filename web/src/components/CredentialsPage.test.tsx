import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { renderWithRouter as render } from "../test-helpers/render.js";
import { CredentialsPage } from "./CredentialsPage.js";
import { useCredentialsStore, type CredentialRow } from "../stores/credentials-store.js";
import { useAuthStore } from "../stores/auth-store.js";

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
  useAuthStore.setState({ user: null, error: null, loading: false });
  vi.restoreAllMocks();
});

const row1: CredentialRow = {
  id: "cred-1",
  name: "Work laptop key",
  kind: "ssh_key",
  publicPart: "ssh-ed25519 AAAAC3 user@laptop",
  hostHint: "github.com",
  scopes: "repo,workflow",
  ownerUserId: "u1",
  createdAt: "2026-05-22T00:00:00Z",
  usedByCount: 2,
};

const row2: CredentialRow = {
  id: "cred-2",
  name: "Lab GitLab PAT",
  kind: "gitlab_pat",
  publicPart: null,
  hostHint: "gitlab.acme.io",
  scopes: null,
  ownerUserId: "u1",
  createdAt: "2026-05-21T00:00:00Z",
  usedByCount: 0,
};

describe("CredentialsPage", () => {
  it("renders the empty state when the list is empty", () => {
    seedCredentials([]);
    render(<CredentialsPage />);
    expect(screen.getByText(/No credentials yet/i)).toBeTruthy();
    // Both header + empty-state CTA show "Add credential"
    expect(screen.getAllByText(/Add credential/i).length).toBeGreaterThanOrEqual(1);
  });

  it("renders one row per credential with kind + host hint + scopes", () => {
    seedCredentials([row1, row2]);
    render(<CredentialsPage />);
    expect(screen.getByText("Work laptop key")).toBeTruthy();
    expect(screen.getByText("Lab GitLab PAT")).toBeTruthy();
    expect(screen.getByText("SSH key")).toBeTruthy();
    expect(screen.getByText("GitLab PAT")).toBeTruthy();
    expect(screen.getByText("github.com")).toBeTruthy();
    expect(screen.getByText("gitlab.acme.io")).toBeTruthy();
    expect(screen.getByText("repo")).toBeTruthy();
    expect(screen.getByText("workflow")).toBeTruthy();
  });

  it("renders the used-by-N-projects count next to each row", () => {
    seedCredentials([row1, row2]);
    render(<CredentialsPage />);
    const usedBy = screen.getAllByTestId("used-by-count");
    expect(usedBy.length).toBe(2);
    expect(usedBy[0].textContent).toContain("Used by 2 projects");
    expect(usedBy[1].textContent).toContain("Used by 0 projects");
  });

  it("renders the used-by count as Used by — for older server responses", () => {
    // The store keeps usedByCount as `undefined` when the server omits
    // the field (older builds). The UI should not pretend 0 projects.
    const noCount: CredentialRow = { ...row1, usedByCount: undefined };
    seedCredentials([noCount]);
    render(<CredentialsPage />);
    expect(screen.getByTestId("used-by-count").textContent).toContain("Used by —");
  });

  it("opens the AddCredentialModal when the header button is clicked", async () => {
    seedCredentials([row1]);
    render(<CredentialsPage />);
    const button = screen.getByTestId("add-credential-button");
    fireEvent.click(button);
    // The modal title appears in the dialog
    expect(await screen.findByRole("dialog", { name: /Add credential/i })).toBeTruthy();
  });

  it("opens the DeleteCredentialModal when the per-row Delete is clicked", async () => {
    seedCredentials([row1]);
    render(<CredentialsPage />);
    fireEvent.click(screen.getByTestId("delete-credential-cred-1"));
    expect(await screen.findByRole("dialog", { name: /Delete credential/i })).toBeTruthy();
  });

  it("calls fetchCredentials on mount", () => {
    const fetchCredentials = vi.fn().mockResolvedValue(undefined);
    useCredentialsStore.setState({
      credentials: [],
      loading: false,
      loaded: true,
      fetchCredentials,
      createCredential: vi.fn(),
      deleteCredential: vi.fn().mockResolvedValue(undefined),
    });
    render(<CredentialsPage />);
    expect(fetchCredentials).toHaveBeenCalledTimes(1);
  });

  it("shows the loading shell when not yet loaded", () => {
    useCredentialsStore.setState({
      credentials: [],
      loading: true,
      loaded: false,
      fetchCredentials: vi.fn().mockResolvedValue(undefined),
      createCredential: vi.fn(),
      deleteCredential: vi.fn().mockResolvedValue(undefined),
    });
    render(<CredentialsPage />);
    expect(screen.getByText(/Loading credentials/i)).toBeTruthy();
  });

  it("renders the SSH public key inline as monospace text", () => {
    seedCredentials([row1]);
    render(<CredentialsPage />);
    expect(screen.getByText("ssh-ed25519 AAAAC3 user@laptop")).toBeTruthy();
  });

  it("uses singular phrasing for usedByCount === 1", () => {
    const single: CredentialRow = { ...row1, usedByCount: 1 };
    seedCredentials([single]);
    render(<CredentialsPage />);
    expect(screen.getByTestId("used-by-count").textContent).toContain("Used by 1 project");
    expect(screen.getByTestId("used-by-count").textContent).not.toContain("projects");
  });
});

describe("CredentialsPage + AddCredentialModal flow", () => {
  it("submitting the paste form calls createCredential with the form values", async () => {
    const createCredential = vi.fn().mockResolvedValue({
      id: "new-cred",
      name: "from form",
      kind: "ssh_key",
      publicPart: null,
      hostHint: "github.com",
      scopes: null,
      ownerUserId: "u1",
      createdAt: "2026-05-22T00:01:00Z",
      usedByCount: 0,
      generated: false,
    });
    useCredentialsStore.setState({
      credentials: [],
      loading: false,
      loaded: true,
      fetchCredentials: vi.fn().mockResolvedValue(undefined),
      createCredential,
      deleteCredential: vi.fn().mockResolvedValue(undefined),
    });
    render(<CredentialsPage />);
    fireEvent.click(screen.getByTestId("add-credential-button"));
    fireEvent.change(screen.getByTestId("credential-name"), { target: { value: "from form" } });
    fireEvent.change(screen.getByTestId("credential-secret"), {
      target: { value: "-----BEGIN OPENSSH PRIVATE KEY-----\ndummy\n" },
    });
    fireEvent.click(screen.getByTestId("credential-submit"));
    await waitFor(() => {
      expect(createCredential).toHaveBeenCalled();
    });
    const arg = createCredential.mock.calls[0][0];
    expect(arg.name).toBe("from form");
    expect(arg.kind).toBe("ssh_key");
    expect(arg.secret).toContain("BEGIN OPENSSH");
  });

  it("kind=generate keeps the modal open and shows the public-key copy view", async () => {
    const createCredential = vi.fn().mockResolvedValue({
      id: "gen-1",
      name: "generated",
      kind: "ssh_key",
      publicPart: "ssh-ed25519 AAAAFRESH branchwork:generated",
      hostHint: "github.com",
      scopes: null,
      ownerUserId: "u1",
      createdAt: "2026-05-22T00:02:00Z",
      usedByCount: 0,
      generated: true,
    });
    useCredentialsStore.setState({
      credentials: [],
      loading: false,
      loaded: true,
      fetchCredentials: vi.fn().mockResolvedValue(undefined),
      createCredential,
      deleteCredential: vi.fn().mockResolvedValue(undefined),
    });
    render(<CredentialsPage />);
    fireEvent.click(screen.getByTestId("add-credential-button"));
    // Switch to generate mode
    fireEvent.click(screen.getByTestId("mode-generate"));
    fireEvent.change(screen.getByTestId("credential-name"), { target: { value: "generated" } });
    fireEvent.click(screen.getByTestId("credential-submit"));
    // Confirmation view appears: public key textarea + GitHub deep link
    const pk = (await screen.findByTestId("generated-public-key")) as HTMLTextAreaElement;
    expect(pk.value).toContain("ssh-ed25519");
    const link = screen.getByTestId("github-deep-link") as HTMLAnchorElement;
    expect(link.href).toContain("github.com/settings/ssh/new");
  });
});

describe("CredentialsPage + DeleteCredentialModal flow", () => {
  it("calls deleteCredential on confirm", async () => {
    const deleteCredential = vi.fn().mockResolvedValue(undefined);
    useCredentialsStore.setState({
      credentials: [row1],
      loading: false,
      loaded: true,
      fetchCredentials: vi.fn().mockResolvedValue(undefined),
      createCredential: vi.fn(),
      deleteCredential,
    });
    render(<CredentialsPage />);
    fireEvent.click(screen.getByTestId("delete-credential-cred-1"));
    fireEvent.click(await screen.findByTestId("confirm-delete-credential"));
    await waitFor(() => {
      expect(deleteCredential).toHaveBeenCalledWith("cred-1");
    });
  });

  it("renders affected projects when the server returns 409 credential_in_use", async () => {
    const { HttpError } = await import("../api.js");
    const deleteCredential = vi.fn().mockRejectedValue(
      new HttpError(409, "Conflict", {
        error: "credential_in_use",
        projectIds: ["proj-a", "proj-b"],
        message:
          "2 project(s) reference this credential as their default; clear the default on those projects first",
      }),
    );
    useCredentialsStore.setState({
      credentials: [row1],
      loading: false,
      loaded: true,
      fetchCredentials: vi.fn().mockResolvedValue(undefined),
      createCredential: vi.fn(),
      deleteCredential,
    });
    render(<CredentialsPage />);
    fireEvent.click(screen.getByTestId("delete-credential-cred-1"));
    fireEvent.click(await screen.findByTestId("confirm-delete-credential"));
    const list = await screen.findByTestId("affected-projects-list");
    expect(list.textContent).toContain("proj-a");
    expect(list.textContent).toContain("proj-b");
    // Modal stays open — Delete button still present
    expect(screen.getByTestId("confirm-delete-credential")).toBeTruthy();
  });
});
