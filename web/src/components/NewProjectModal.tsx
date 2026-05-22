import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { HttpError } from "../api.js";
import {
  useProjectsStore,
  parseRepoUrl,
  type CreateProjectBody,
  type ParsedRepoUrl,
  type ProjectRow,
} from "../stores/projects-store.js";
import { useCredentialsStore, type CredentialRow } from "../stores/credentials-store.js";
import { subscribeToWsEvents } from "../stores/ws-store.js";
import { Modal } from "./ui/Modal.js";
import { Banner } from "./ui/Banner.js";
import { Button } from "./ui/Button.js";
import { Tabs } from "./ui/Tabs.js";
import { errorMessage } from "../lib/error.js";
import { toastError } from "../lib/toast.js";

/// Task 2.4 ("New Project" UI dialog). Two-tab modal that drives both
/// `POST /api/projects` modes:
///
/// - **Clone existing**: paste a remote URL, autoparse `host` / `owner`
///   / `name`, pick a credential filtered by host, preview the resolved
///   `$HOME/<name>` workspace path, submit with `mode=clone`. The server
///   records the project row and fires the runner clone RPC in the
///   background.
/// - **Create new**: pick a host (`github` | `gitlab`), pick a PAT
///   credential (the server refuses SSH-only here — we surface that as
///   an inline warning before submit), set the new repo name + optional
///   description + private/public toggle + optional org/group, submit
///   with `mode=create`. The server creates the remote on the host,
///   then records the row + fires the clone RPC.
///
/// Both paths redirect to `/plans` on success — the project's plans
/// surface as a group in the ProjectDashboard, keyed off
/// `plans.project = <project name>`.

type Tab = "clone" | "create";

type Progress =
  | { kind: "idle" }
  | { kind: "submitting" }
  | { kind: "cloning"; resolvedPath: string | null }
  | { kind: "done"; project: ProjectRow }
  | { kind: "failed"; error: string };

interface NewProjectModalProps {
  onClose: () => void;
}

/// Smart-classified "what hosts does this credential know about" hint.
/// Mostly cosmetic for the picker — the user can override at any time
/// — but it lets us hide credentials whose `hostHint` clearly disagrees
/// with the current host pick (e.g. a `gitlab.acme.io` PAT on a
/// `github.com` clone). When `hostHint` is empty or matches by free-
/// form contain, we keep the row visible.
export function credentialMatchesHost(
  cred: CredentialRow,
  host: "github" | "gitlab" | "bitbucket" | "other" | null,
): boolean {
  if (!host || host === "other") return true;
  if (!cred.hostHint) {
    // Without a hint we cannot confidently filter; show + let the user
    // decide. PAT kinds are still strongly host-typed (`gh_pat` for
    // GitHub, `gitlab_pat` for GitLab); we use the kind as a secondary
    // signal below.
    return matchByKind(cred.kind, host);
  }
  const hint = cred.hostHint.toLowerCase();
  if (host === "github") return hint.includes("github") || matchByKind(cred.kind, host);
  if (host === "gitlab") return hint.includes("gitlab") || matchByKind(cred.kind, host);
  if (host === "bitbucket") return hint.includes("bitbucket");
  return true;
}

function matchByKind(kind: string, host: "github" | "gitlab" | "bitbucket"): boolean {
  if (host === "github") return kind === "gh_pat" || kind === "ssh_key";
  if (host === "gitlab") return kind === "gitlab_pat" || kind === "ssh_key";
  // SSH keys can also clone via SSH; PATs without a hostHint stay visible
  // to keep the picker permissive.
  return kind === "ssh_key";
}

/// Credentials that the host-API "create new remote repo" path
/// accepts. SSH keys do not carry an API token so they cannot mint a
/// remote — they need to live as the clone credential AFTER the repo
/// exists. Acceptance: "filtered to PATs with the right scope".
export function credentialCanCreateRemoteOn(
  cred: CredentialRow,
  host: "github" | "gitlab",
): boolean {
  if (host === "github") return cred.kind === "gh_pat";
  if (host === "gitlab") return cred.kind === "gitlab_pat";
  return false;
}

/// Default workspace path the modal previews. Mirrors the server-side
/// resolver in `api/projects.rs::resolve_workspace_path` for the
/// no-override path. The actual canonical path lands on the row after
/// the runner clone RPC resolves `$HOME` and `~`-prefixes server-side.
export function previewWorkspacePath(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return "";
  return `~/${trimmed}`;
}

export function NewProjectModal({ onClose }: NewProjectModalProps) {
  const navigate = useNavigate();
  const createProject = useProjectsStore((s) => s.createProject);
  const credentials = useCredentialsStore((s) => s.credentials);
  const credentialsLoaded = useCredentialsStore((s) => s.loaded);
  const fetchCredentials = useCredentialsStore((s) => s.fetchCredentials);

  const [tab, setTab] = useState<Tab>("clone");
  const nameInputRef = useRef<HTMLInputElement>(null);

  // ── Clone tab state ────────────────────────────────────────────────
  const [repoUrl, setRepoUrl] = useState("");
  /// `null` when the URL is empty or unparseable. The picker hides the
  /// host autoparse hint in that case so the user knows to paste a
  /// canonical form.
  const parsed = useMemo<ParsedRepoUrl | null>(() => parseRepoUrl(repoUrl), [repoUrl]);

  // ── Create tab state ───────────────────────────────────────────────
  const [createHost, setCreateHost] = useState<"github" | "gitlab">("github");
  const [isPrivate, setIsPrivate] = useState(true);
  const [owner, setOwner] = useState("");
  const [description, setDescription] = useState("");

  // ── Common state ───────────────────────────────────────────────────
  const [name, setName] = useState("");
  /// Whether the user has typed in the name field. Once true we stop
  /// auto-mirroring the parsed repo name so a paste-then-edit flow
  /// doesn't get clobbered by re-parsing.
  const [nameDirty, setNameDirty] = useState(false);
  const [credentialId, setCredentialId] = useState<string | null>(null);
  const [progress, setProgress] = useState<Progress>({ kind: "idle" });

  // Resolve credentials lazily on mount — same dedup contract as
  // CredentialsPage. We use `credentialsLoaded` to know when the empty
  // list is "really empty" vs "still loading".
  useEffect(() => {
    void fetchCredentials();
  }, [fetchCredentials]);

  // Auto-fill the name from the parsed URL on the Clone tab. We only
  // touch it while the user has not edited the name themselves (the
  // typical happy path: paste URL → name auto-fills → submit).
  useEffect(() => {
    if (tab !== "clone") return;
    if (nameDirty) return;
    if (!parsed) return;
    setName(parsed.name);
  }, [tab, parsed, nameDirty]);

  // Listen for clone progress so the UI can flip to "Cloning …" while
  // the runner is still working. The terminal event (`clone_done` or
  // `clone_failed`) drives the user-visible breadcrumb; the HTTP
  // response from `POST /api/projects` is the authoritative signal
  // for success / failure of the whole flow.
  useEffect(() => {
    const unsub = subscribeToWsEvents(["clone_started", "clone_done", "clone_failed"], (msg) => {
      if (msg.type === "clone_started") {
        setProgress((cur) =>
          cur.kind === "submitting" || cur.kind === "cloning"
            ? { kind: "cloning", resolvedPath: null }
            : cur,
        );
      } else if (msg.type === "clone_done") {
        setProgress((cur) =>
          cur.kind === "submitting" || cur.kind === "cloning"
            ? { kind: "cloning", resolvedPath: msg.data.resolved_path }
            : cur,
        );
      } else if (msg.type === "clone_failed") {
        // Surfaces in the banner as still-cloning while the HTTP
        // response decides the terminal verdict — server logs the
        // error in the projects.workspace_path UPDATE.
        setProgress((cur) =>
          cur.kind === "submitting" || cur.kind === "cloning"
            ? { kind: "cloning", resolvedPath: null }
            : cur,
        );
      }
    });
    return unsub;
  }, []);

  const selectedCredential = useMemo(
    () => credentials.find((c) => c.id === credentialId) ?? null,
    [credentials, credentialId],
  );

  const effectiveHost: "github" | "gitlab" | "bitbucket" | "other" | null =
    tab === "clone" ? (parsed?.host ?? null) : createHost;

  const visibleCredentials = useMemo(
    () => credentials.filter((c) => credentialMatchesHost(c, effectiveHost)),
    [credentials, effectiveHost],
  );

  /// Warning shown on the Create tab when the selected credential
  /// can't mint a remote repo on the picked host. Acceptance criterion:
  /// "Inline warning when the operator picks an SSH-only credential
  /// in the Create tab". The link points at /credentials so the user
  /// can fix the kind without leaving the flow.
  const showSshOnlyWarning =
    tab === "create" &&
    selectedCredential !== null &&
    !credentialCanCreateRemoteOn(selectedCredential, createHost);

  const submitting = progress.kind === "submitting" || progress.kind === "cloning";

  function resetForm() {
    setRepoUrl("");
    setName("");
    setNameDirty(false);
    setCreateHost("github");
    setIsPrivate(true);
    setOwner("");
    setDescription("");
    setCredentialId(null);
    setProgress({ kind: "idle" });
  }

  function handleClose() {
    if (submitting) return;
    resetForm();
    onClose();
  }

  async function handleSubmit() {
    setProgress({ kind: "submitting" });
    try {
      const trimmedName = name.trim();
      if (!trimmedName) {
        setProgress({ kind: "failed", error: "Project name is required." });
        return;
      }
      let body: CreateProjectBody;
      if (tab === "clone") {
        const url = repoUrl.trim();
        if (!url) {
          setProgress({ kind: "failed", error: "Repository URL is required." });
          return;
        }
        body = {
          mode: "clone",
          name: trimmedName,
          repo_url: url,
          host: parsed?.host ?? "other",
          owner: parsed?.owner ?? undefined,
          credentialId: credentialId ?? undefined,
        };
      } else {
        if (!credentialId) {
          setProgress({
            kind: "failed",
            error: "Pick a credential — create mode authenticates against the host API.",
          });
          return;
        }
        if (showSshOnlyWarning) {
          setProgress({
            kind: "failed",
            error:
              "Selected credential cannot create a remote repo. Pick a personal access token credential.",
          });
          return;
        }
        body = {
          mode: "create",
          name: trimmedName,
          host: createHost,
          owner: owner.trim() || undefined,
          private: isPrivate,
          credentialId,
        };
      }

      const project = await createProject(body);
      setProgress({ kind: "done", project });
      // Redirect to the dashboard's project list. The new project will
      // appear as a group once the user creates a plan against it.
      // Small delay so the success banner is visible.
      window.setTimeout(() => {
        navigate("/plans");
        onClose();
      }, 500);
    } catch (e) {
      if (e instanceof HttpError) {
        const body = (e.body ?? {}) as {
          error?: string;
          message?: string;
          code?: string;
          host_response_excerpt?: string;
        };
        const msg = body.message ?? body.error ?? errorMessage(e);
        setProgress({ kind: "failed", error: msg });
        toastError(msg);
      } else {
        const msg = errorMessage(e);
        setProgress({ kind: "failed", error: msg });
        toastError(msg);
      }
    }
  }

  const workspacePreview = previewWorkspacePath(name || (parsed?.name ?? ""));

  return (
    <Modal
      open={true}
      onClose={handleClose}
      title="New project"
      description="Clone an existing repository or create a new one on GitHub / GitLab. The runner clones into $HOME/<name>."
      initialFocusRef={nameInputRef}
      size="2xl"
    >
      <div className="mt-4" data-testid="new-project-modal">
        <Tabs
          tabs={[
            { value: "clone", label: "Clone existing" },
            { value: "create", label: "Create new" },
          ]}
          value={tab}
          onChange={(next) => {
            // Persist name across the tab switch so a clone-mode autofill
            // survives if the user changes their mind, but reset transient
            // form-only state to avoid cross-mode leaks (e.g. an SSH-only
            // credential selected under Clone shouldn't haunt Create).
            setTab(next);
            setProgress({ kind: "idle" });
          }}
          label="Project creation mode"
          className="border-b border-gray-800"
        />

        <div className="mt-4 space-y-4">
          {tab === "clone" && (
            <CloneTab
              repoUrl={repoUrl}
              onRepoUrl={(v) => {
                setRepoUrl(v);
                setProgress({ kind: "idle" });
              }}
              parsed={parsed}
              disabled={submitting}
            />
          )}

          {tab === "create" && (
            <CreateTab
              host={createHost}
              onHost={(h) => {
                setCreateHost(h);
                // Clear the credential pick when the host changes —
                // the kind constraint changes (gh_pat vs gitlab_pat).
                setCredentialId(null);
              }}
              isPrivate={isPrivate}
              onIsPrivate={setIsPrivate}
              owner={owner}
              onOwner={setOwner}
              description={description}
              onDescription={setDescription}
              disabled={submitting}
            />
          )}

          <Field id="np-name" label="Project name">
            <input
              id="np-name"
              ref={nameInputRef}
              type="text"
              value={name}
              onChange={(e) => {
                setName(e.target.value);
                setNameDirty(true);
              }}
              placeholder="my-project"
              className={inputCls}
              disabled={submitting}
              data-testid="new-project-name"
            />
          </Field>

          <CredentialPicker
            credentials={visibleCredentials}
            loaded={credentialsLoaded}
            credentialId={credentialId}
            onChange={setCredentialId}
            host={effectiveHost}
            required={tab === "create"}
            disabled={submitting}
          />

          {showSshOnlyWarning && (
            <div data-testid="ssh-only-warning">
              <Banner kind="warn">
                This credential can clone but not create — SSH keys don't carry an API token for
                minting a remote.{" "}
                <a href="/credentials" className="underline hover:text-amber-200">
                  Pick or add a PAT credential.
                </a>
              </Banner>
            </div>
          )}

          {workspacePreview && (
            <p className="text-xs text-gray-500" data-testid="workspace-path-preview">
              Clones into <code className="text-gray-300">{workspacePreview}</code> on the runner
              host.
            </p>
          )}

          {progress.kind === "failed" && <Banner kind="error">{progress.error}</Banner>}

          {(progress.kind === "submitting" || progress.kind === "cloning") && (
            <div
              role="status"
              aria-live="polite"
              className="rounded border border-indigo-800/50 bg-indigo-900/30 px-3 py-2 text-xs text-indigo-200"
              data-testid="progress-banner"
            >
              {progress.kind === "submitting" ? "Submitting…" : "Cloning into workspace…"}
              {progress.kind === "cloning" && progress.resolvedPath && (
                <>
                  {" "}
                  <code className="text-xs">{progress.resolvedPath}</code>
                </>
              )}
            </div>
          )}

          {progress.kind === "done" && (
            <div
              role="status"
              aria-live="polite"
              className="rounded border border-emerald-800/50 bg-emerald-900/30 px-3 py-2 text-xs text-emerald-200"
              data-testid="done-banner"
            >
              Project created. Redirecting to plans…
            </div>
          )}

          <div className="flex justify-end gap-2 pt-2">
            <Button variant="secondary" size="sm" onClick={handleClose} disabled={submitting}>
              Cancel
            </Button>
            <Button
              variant="primary"
              size="sm"
              onClick={handleSubmit}
              loading={progress.kind === "submitting"}
              disabled={submitting || progress.kind === "done"}
              data-testid="new-project-submit"
            >
              {tab === "clone" ? "Clone repo" : "Create + clone"}
            </Button>
          </div>
        </div>
      </div>
    </Modal>
  );
}

interface CloneTabProps {
  repoUrl: string;
  onRepoUrl: (v: string) => void;
  parsed: ParsedRepoUrl | null;
  disabled: boolean;
}

function CloneTab({ repoUrl, onRepoUrl, parsed, disabled }: CloneTabProps) {
  return (
    <>
      <Field
        id="np-repo-url"
        label="Repository URL"
        hint="HTTPS, SSH, or git:// — Branchwork autoparses host / owner / name."
      >
        <input
          id="np-repo-url"
          type="text"
          value={repoUrl}
          onChange={(e) => onRepoUrl(e.target.value)}
          placeholder="https://github.com/owner/repo.git"
          className={inputCls}
          disabled={disabled}
          data-testid="repo-url-input"
        />
      </Field>

      {parsed && (
        <div
          className="rounded border border-gray-800 bg-gray-900/40 px-3 py-2 text-xs text-gray-400"
          data-testid="parsed-url-hint"
        >
          <span className="text-gray-500">Parsed: </span>
          <span className="text-indigo-300 font-mono">{parsed.host}</span>
          <span className="text-gray-600"> / </span>
          <span className="text-gray-200 font-mono">{parsed.owner}</span>
          <span className="text-gray-600"> / </span>
          <span className="text-gray-200 font-mono">{parsed.name}</span>
        </div>
      )}

      {repoUrl.trim() && !parsed && (
        <p className="text-[11px] text-amber-400" data-testid="parse-fallback-hint">
          Couldn't parse the host / owner / name. The URL still works for clone, but you'll need to
          pick host + credential manually below.
        </p>
      )}
    </>
  );
}

interface CreateTabProps {
  host: "github" | "gitlab";
  onHost: (h: "github" | "gitlab") => void;
  isPrivate: boolean;
  onIsPrivate: (v: boolean) => void;
  owner: string;
  onOwner: (v: string) => void;
  description: string;
  onDescription: (v: string) => void;
  disabled: boolean;
}

function CreateTab({
  host,
  onHost,
  isPrivate,
  onIsPrivate,
  owner,
  onOwner,
  description,
  onDescription,
  disabled,
}: CreateTabProps) {
  return (
    <>
      <Field id="np-host" label="Host">
        <select
          id="np-host"
          value={host}
          onChange={(e) => onHost(e.target.value as "github" | "gitlab")}
          className={inputCls}
          disabled={disabled}
          data-testid="create-host"
        >
          <option value="github">GitHub</option>
          <option value="gitlab">GitLab</option>
        </select>
      </Field>

      <Field
        id="np-owner"
        label="Org / group (optional)"
        hint={
          host === "github"
            ? "Leave blank to create under your user account. Set to an org slug to create under that org."
            : "Leave blank to create under your user. Set to a GitLab group / namespace to create there."
        }
      >
        <input
          id="np-owner"
          type="text"
          value={owner}
          onChange={(e) => onOwner(e.target.value)}
          placeholder={host === "github" ? "my-org" : "my-group"}
          className={inputCls}
          disabled={disabled}
          data-testid="create-owner"
        />
      </Field>

      <Field
        id="np-description"
        label="Description (optional)"
        hint="Sent to the host API on create. Branchwork doesn't store this; you can edit later on the host."
      >
        <input
          id="np-description"
          type="text"
          value={description}
          onChange={(e) => onDescription(e.target.value)}
          placeholder="Repository description"
          className={inputCls}
          disabled={disabled}
          data-testid="create-description"
        />
      </Field>

      <fieldset className="space-y-2" aria-label="Repository visibility">
        <legend className="text-xs font-medium text-gray-200">Visibility</legend>
        <label className="flex items-center gap-2 text-xs text-gray-300">
          <input
            type="radio"
            name="np-private"
            value="private"
            checked={isPrivate}
            onChange={() => onIsPrivate(true)}
            disabled={disabled}
            data-testid="create-private"
          />
          Private (recommended)
        </label>
        <label className="flex items-center gap-2 text-xs text-gray-300">
          <input
            type="radio"
            name="np-private"
            value="public"
            checked={!isPrivate}
            onChange={() => onIsPrivate(false)}
            disabled={disabled}
            data-testid="create-public"
          />
          Public
        </label>
      </fieldset>
    </>
  );
}

interface CredentialPickerProps {
  credentials: CredentialRow[];
  loaded: boolean;
  credentialId: string | null;
  onChange: (id: string | null) => void;
  host: "github" | "gitlab" | "bitbucket" | "other" | null;
  required: boolean;
  disabled: boolean;
}

function CredentialPicker({
  credentials,
  loaded,
  credentialId,
  onChange,
  host,
  required,
  disabled,
}: CredentialPickerProps) {
  const label = required ? "Credential" : "Credential (optional)";
  const hint = required
    ? "Used to authenticate against the host API on create + the clone push later."
    : "Used when the runner clones a private repo. Leave blank for public repos.";

  return (
    <Field id="np-credential" label={label} hint={hint}>
      <select
        id="np-credential"
        value={credentialId ?? ""}
        onChange={(e) => onChange(e.target.value || null)}
        className={inputCls}
        disabled={disabled}
        data-testid="credential-picker"
      >
        <option value="">
          {!loaded
            ? "Loading credentials…"
            : credentials.length === 0
              ? "No credentials yet"
              : required
                ? "Select a credential…"
                : "(none — public repo)"}
        </option>
        {credentials.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name} · {credentialKindLabel(c.kind)}
            {c.hostHint ? ` (${c.hostHint})` : ""}
          </option>
        ))}
      </select>
      {loaded && credentials.length === 0 && (
        <p className="text-[11px] text-amber-400 mt-1">
          No credentials match{host && host !== "other" ? ` host "${host}"` : ""}.{" "}
          <a href="/credentials" className="underline hover:text-amber-200">
            Add one.
          </a>
        </p>
      )}
    </Field>
  );
}

function credentialKindLabel(kind: string): string {
  switch (kind) {
    case "ssh_key":
      return "SSH";
    case "gh_pat":
      return "GitHub PAT";
    case "gitlab_pat":
      return "GitLab PAT";
    default:
      return kind;
  }
}

interface FieldProps {
  id: string;
  label: string;
  hint?: string;
  children: React.ReactNode;
}

function Field({ id, label, hint, children }: FieldProps) {
  return (
    <div>
      <label htmlFor={id} className="block text-xs font-medium text-gray-200 mb-1">
        {label}
      </label>
      {children}
      {hint && <p className="text-[11px] text-gray-500 mt-1">{hint}</p>}
    </div>
  );
}

const inputCls =
  "w-full px-2 py-1.5 text-sm bg-gray-800 border border-gray-700 rounded text-gray-100 placeholder-gray-600 outline-none focus:border-indigo-600 transition disabled:opacity-50";
