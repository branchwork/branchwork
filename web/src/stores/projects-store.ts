import { create } from "zustand";
import { fetchJson, postJson } from "../api.js";

/// One row in the projects table — operator-visible shape from
/// `GET /api/projects`. Mirrors `server-rs/src/api/projects.rs`
/// `ProjectRow` (camelCase via serde rename_all).
///
/// A project is a host-side git repo that the operator has decided to
/// work on. Pairs an operator-friendly slug with a clone URL, a host
/// enum, the resolved workspace path on disk, and an optional default
/// credential. Plans live alongside (under `plans.project`) so each
/// project's plan list is just the existing dashboard filtered by name.
export interface ProjectRow {
  id: string;
  name: string;
  repoUrl: string;
  host: string;
  owner: string | null;
  defaultCredentialId: string | null;
  workspacePath: string;
  orgId: string;
  createdAt: string;
}

interface ListProjectsResponse {
  projects: ProjectRow[];
}

/// Body for `POST /api/projects` in `clone` mode. The caller already
/// knows the remote URL; the server just records the row and fires the
/// runner clone RPC in the background.
export interface CreateProjectCloneBody {
  mode: "clone";
  name: string;
  repo_url: string;
  host: "github" | "gitlab" | "bitbucket" | "other";
  owner?: string;
  credentialId?: string;
  workspace_path?: string;
}

/// Body for `POST /api/projects` in `create` mode. The server resolves
/// the credential, dispatches to the host API to create the remote
/// (GitHub `POST /user/repos` or `POST /orgs/{owner}/repos`; GitLab
/// `POST /projects`), and only records the row on host-API success.
export interface CreateProjectCreateBody {
  mode: "create";
  name: string;
  host: "github" | "gitlab";
  owner?: string;
  credentialId: string;
  private?: boolean;
  workspace_path?: string;
}

export type CreateProjectBody = CreateProjectCloneBody | CreateProjectCreateBody;

interface ProjectsState {
  projects: ProjectRow[];
  loading: boolean;
  loaded: boolean;
  /// Module-private in-flight promise; dedupes concurrent callers.
  /// Mirrors the audit-§4 pattern other stores use to keep React
  /// effects from racing the WS reconnect sweep.
  fetchProjects: () => Promise<void>;
  createProject: (body: CreateProjectBody) => Promise<ProjectRow>;
  /// Test escape hatch — clears every slice + in-flight markers.
  /// Wired into `lib/reset-all.ts` so user A's projects do not leak to
  /// user B after logout.
  reset: () => void;
}

let inFlightProjectsFetch: Promise<void> | null = null;

export const useProjectsStore = create<ProjectsState>((set) => ({
  projects: [],
  loading: false,
  loaded: false,

  fetchProjects: async () => {
    if (inFlightProjectsFetch) return inFlightProjectsFetch;
    set({ loading: true });
    // Capture-then-assign so the inner `.finally()` can compare
    // against the same handle even though it closes over the outer
    // name (TS2454 strict-mode workaround used by credentials-store).
    let handle: Promise<void> | null = null;
    const promise: Promise<void> = (async () => {
      try {
        const res = await fetchJson<ListProjectsResponse>("/api/projects");
        set({ projects: res.projects ?? [], loaded: true, loading: false });
      } catch {
        // Leave `loaded` false so a subsequent retry re-fires.
        set({ loading: false });
      } finally {
        if (inFlightProjectsFetch === handle) {
          inFlightProjectsFetch = null;
        }
      }
    })();
    handle = promise;
    inFlightProjectsFetch = promise;
    return promise;
  },

  createProject: async (body) => {
    const res = await postJson<ProjectRow>("/api/projects", body);
    // Optimistically prepend so the dashboard can land on the new
    // project before the next list-refetch.
    set((s) => ({ projects: [res, ...s.projects] }));
    return res;
  },

  reset: () => {
    inFlightProjectsFetch = null;
    set({ projects: [], loading: false, loaded: false });
  },
}));

/// Parse a git remote URL into `{ host, owner, name }`.
///
/// Accepts the four canonical forms a paste from GitHub / GitLab can
/// produce — same parser the auto-mode CI poller uses on the server
/// side (see `server-rs/src/ci.rs::parse_github_owner_repo`). Mirrors:
///
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `git@github.com:owner/repo.git`
/// - `ssh://git@gitlab.com/owner/repo.git`
///
/// Returns `null` when the URL doesn't match any of these shapes — the
/// modal then leaves the host picker manual and skips the autoparse
/// hint. Bare ASCII slug constraints are NOT enforced here; the server
/// re-validates names (`is_valid_project_name`) before insert.
export interface ParsedRepoUrl {
  host: "github" | "gitlab" | "bitbucket" | "other";
  owner: string;
  name: string;
  fullHost: string;
}

export function parseRepoUrl(raw: string): ParsedRepoUrl | null {
  const url = raw.trim();
  if (!url) return null;

  let match: RegExpMatchArray | null;

  // https://host/owner/repo[.git][/]
  match = url.match(/^https?:\/\/([^/]+)\/([^/]+)\/([^/?#]+?)(?:\.git)?\/?$/);
  if (!match) {
    // ssh://git@host/owner/repo[.git][/]
    match = url.match(/^ssh:\/\/[^/@]+@([^/]+)\/([^/]+)\/([^/?#]+?)(?:\.git)?\/?$/);
  }
  if (!match) {
    // git@host:owner/repo[.git]
    match = url.match(/^[^@]+@([^:]+):([^/]+)\/([^/?#]+?)(?:\.git)?\/?$/);
  }
  if (!match) {
    // git://host/owner/repo[.git]
    match = url.match(/^git:\/\/([^/]+)\/([^/]+)\/([^/?#]+?)(?:\.git)?\/?$/);
  }
  if (!match) return null;

  const fullHost = match[1].toLowerCase();
  const owner = match[2];
  const name = match[3];
  if (!owner || !name) return null;

  const host = classifyHost(fullHost);
  return { host, owner, name, fullHost };
}

function classifyHost(fullHost: string): ParsedRepoUrl["host"] {
  if (fullHost === "github.com" || fullHost.endsWith(".github.com")) return "github";
  if (
    fullHost === "gitlab.com" ||
    fullHost.endsWith(".gitlab.com") ||
    fullHost.startsWith("gitlab.")
  )
    return "gitlab";
  if (fullHost === "bitbucket.org" || fullHost.endsWith(".bitbucket.org")) return "bitbucket";
  return "other";
}
