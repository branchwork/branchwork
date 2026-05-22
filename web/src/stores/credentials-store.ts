import { create } from "zustand";
import { deleteJson, fetchJson, postJson } from "../api.js";

/// One row in the credentials table — operator-safe shape (no secrets
/// ever appear on the wire). Mirrors `server-rs/src/api/credentials.rs`
/// `CredentialRow` verbatim (camelCase via serde rename_all).
///
/// `kind` is one of `ssh_key`, `gh_pat`, `gitlab_pat`. The server-side
/// `generate` shape always lands as `ssh_key` after minting.
///
/// `publicPart` is:
/// - For `ssh_key`: the OpenSSH one-line public key (`ssh-ed25519 …`).
/// - For PATs: optional human label / fingerprint hint.
///
/// `hostHint` is a free-form label like `github.com` or `gitlab.acme.io`
/// used by the credentials list to disambiguate when an operator stores
/// multiple PATs for different hosts.
///
/// `scopes` is a free-form comma-separated list of scope tags
/// (e.g. `repo,workflow` for GitHub PATs) displayed as chips next to
/// the row.
///
/// `usedByCount` is the number of `projects.default_credential_id`
/// references this row has — surfaced server-side so the list endpoint
/// can render the "used by N projects" hint without a per-row probe.
export interface CredentialRow {
  id: string;
  name: string;
  kind: string;
  publicPart: string | null;
  hostHint: string | null;
  scopes: string | null;
  ownerUserId: string;
  createdAt: string;
  /// Number of `projects.default_credential_id` references to this row.
  /// 0 when nothing depends on it (Delete proceeds without a 409).
  /// `undefined` ⇒ older server build that didn't ship the field; UI
  /// degrades to "—" rather than "0" so the operator doesn't trust a
  /// stale count.
  usedByCount?: number;
}

/// Response shape of `POST /api/credentials`. The freshly-created row
/// reads back identical to a list entry, plus `generated: true` on the
/// `kind=generate` path so the UI can prompt the operator to copy the
/// public half once.
export interface CreateCredentialResponse extends CredentialRow {
  generated: boolean;
}

/// `kind` discriminator the modal sends on POST. `generate` is the
/// server-side ed25519 mint flow; the others store a caller-supplied
/// secret as-is.
export type CredentialCreateKind = "ssh_key" | "gh_pat" | "gitlab_pat" | "generate";

/// Body for `POST /api/credentials`. See server module-level docs for
/// the kind-shape semantics — `secret` is required for non-generate
/// kinds and refused on the `generate` path.
export interface CreateCredentialBody {
  name: string;
  kind: CredentialCreateKind;
  secret?: string;
  publicPart?: string;
  hostHint?: string;
  scopes?: string;
}

interface ListCredentialsResponse {
  credentials: CredentialRow[];
}

interface CredentialsState {
  credentials: CredentialRow[];
  loading: boolean;
  loaded: boolean;
  /// Module-private in-flight promise, deduped across concurrent
  /// callers. Mirrors the audit-§4 pattern other stores use to keep
  /// React effects from racing the WS reconnect sweep.
  fetchCredentials: () => Promise<void>;
  createCredential: (body: CreateCredentialBody) => Promise<CreateCredentialResponse>;
  deleteCredential: (id: string) => Promise<void>;
  /// Test escape hatch — resets every slice and clears in-flight markers.
  /// Called from `lib/reset-all.ts` on logout.
  reset: () => void;
}

let inFlightCredentialsFetch: Promise<void> | null = null;

export const useCredentialsStore = create<CredentialsState>((set) => ({
  credentials: [],
  loading: false,
  loaded: false,

  fetchCredentials: async () => {
    if (inFlightCredentialsFetch) return inFlightCredentialsFetch;
    set({ loading: true });
    // Capture-then-assign so the inner `.finally()` can compare against
    // the same handle even though it closes over the outer name.
    let handle: Promise<void> | null = null;
    const promise: Promise<void> = (async () => {
      try {
        const res = await fetchJson<ListCredentialsResponse>("/api/credentials");
        set({ credentials: res.credentials ?? [], loaded: true, loading: false });
      } catch {
        // Leave `loaded` false so a subsequent retry re-fires; UI shows
        // a loading shell. Toast is surfaced by `api.ts` already
        // (401 → SessionExpired, 403 → toastError).
        set({ loading: false });
      } finally {
        if (inFlightCredentialsFetch === handle) {
          inFlightCredentialsFetch = null;
        }
      }
    })();
    handle = promise;
    inFlightCredentialsFetch = promise;
    return promise;
  },

  createCredential: async (body) => {
    const res = await postJson<CreateCredentialResponse>("/api/credentials", body);
    // Append optimistically so the modal can close + the page lands
    // on the row immediately. The list endpoint is the source of
    // truth on the next refetch.
    set((s) => ({
      credentials: [
        // Strip the `generated` discriminator before persisting in the
        // list — it's a one-shot flag, not a row attribute.
        {
          id: res.id,
          name: res.name,
          kind: res.kind,
          publicPart: res.publicPart,
          hostHint: res.hostHint,
          scopes: res.scopes,
          ownerUserId: res.ownerUserId,
          createdAt: res.createdAt,
          usedByCount: res.usedByCount ?? 0,
        },
        ...s.credentials,
      ],
    }));
    return res;
  },

  deleteCredential: async (id) => {
    await deleteJson<{ ok: boolean; id: string }>(`/api/credentials/${id}`);
    set((s) => ({ credentials: s.credentials.filter((c) => c.id !== id) }));
  },

  reset: () => {
    inFlightCredentialsFetch = null;
    set({ credentials: [], loading: false, loaded: false });
  },
}));

/// Human-readable label for a credential `kind`. Falls back to the
/// raw string so a future kind silently surfaces instead of being
/// silently lost.
export function formatCredentialKind(kind: string): string {
  switch (kind) {
    case "ssh_key":
      return "SSH key";
    case "gh_pat":
      return "GitHub PAT";
    case "gitlab_pat":
      return "GitLab PAT";
    default:
      return kind;
  }
}

/// Parse the comma-separated `scopes` string into trimmed, deduped
/// chips. Empty / null returns `[]` so the renderer renders nothing.
export function parseCredentialScopes(scopes: string | null | undefined): string[] {
  if (!scopes) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of scopes.split(",")) {
    const s = raw.trim();
    if (!s || seen.has(s)) continue;
    seen.add(s);
    out.push(s);
  }
  return out;
}
