import { useEffect, useState } from "react";
import {
  useCredentialsStore,
  formatCredentialKind,
  parseCredentialScopes,
  type CredentialRow,
} from "../stores/credentials-store.js";
import { Button } from "./ui/Button.js";
import { formatRelative } from "../lib/time.js";
import { AddCredentialModal } from "./AddCredentialModal.js";
import { DeleteCredentialModal } from "./DeleteCredentialModal.js";

/// Task 3.4 (credentials UI). Branchwork-managed credentials live
/// under their own `/credentials` route so the operator can:
///
/// - See every credential they own at a glance (name + kind + host
///   hint + scopes + used-by-N-projects).
/// - Add a new one via the two flows the server supports:
///     1. Paste a private PEM body (server seals it with the master
///        key).
///     2. Server-side ed25519 keypair generation that returns the
///        public half once + a deep link to
///        `github.com/settings/ssh/new`.
/// - Delete one, with a 409 path that lists the projects still
///   referencing it as their default credential.
///
/// The page is intentionally simple — credentials are an
/// infrequently-touched surface and the workflow is dominated by
/// "create-then-paste-into-GitHub" rather than ongoing editing.
export function CredentialsPage() {
  const credentials = useCredentialsStore((s) => s.credentials);
  const loading = useCredentialsStore((s) => s.loading);
  const loaded = useCredentialsStore((s) => s.loaded);
  const fetchCredentials = useCredentialsStore((s) => s.fetchCredentials);

  const [addOpen, setAddOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<CredentialRow | null>(null);

  // Fetch on mount. The store deduplicates concurrent callers so a
  // race between the App.tsx bootstrap and this page's mount doesn't
  // fire two requests.
  useEffect(() => {
    void fetchCredentials();
  }, [fetchCredentials]);

  return (
    <div className="p-6 max-w-4xl mx-auto">
      <div className="flex items-start justify-between mb-6">
        <div>
          <h1 className="text-xl font-bold text-gray-100">Credentials</h1>
          <p className="text-xs text-gray-500 mt-1 max-w-2xl">
            SSH keys and personal access tokens for git hosts. Branchwork seals every secret at rest
            with the master key and ships it on-demand to the runner only when an agent needs to
            clone or push. Public halves are safe to share; the private halves never leave the
            server in any list response.
          </p>
        </div>
        <Button
          variant="primary"
          size="sm"
          onClick={() => setAddOpen(true)}
          data-testid="add-credential-button"
        >
          Add credential
        </Button>
      </div>

      {!loaded && loading && <div className="text-sm text-gray-500">Loading credentials…</div>}

      {loaded && credentials.length === 0 && (
        <div className="rounded-md border border-gray-800 bg-gray-900/40 p-8 text-center">
          <p className="text-sm text-gray-300 font-medium">No credentials yet.</p>
          <p className="text-xs text-gray-500 mt-2 max-w-md mx-auto">
            Add an SSH key or PAT so the New Project dialog can clone private repos. You can also
            generate a fresh ed25519 keypair on the server — paste the public half into GitHub and
            Branchwork seals the private half automatically.
          </p>
          <div className="mt-4">
            <Button variant="primary" size="sm" onClick={() => setAddOpen(true)}>
              Add credential
            </Button>
          </div>
        </div>
      )}

      {credentials.length > 0 && (
        <ul className="space-y-3" data-testid="credentials-list" aria-label="Credentials list">
          {credentials.map((c) => (
            <CredentialRowItem key={c.id} credential={c} onDelete={() => setDeleteTarget(c)} />
          ))}
        </ul>
      )}

      {addOpen && <AddCredentialModal onClose={() => setAddOpen(false)} />}
      {deleteTarget && (
        <DeleteCredentialModal credential={deleteTarget} onClose={() => setDeleteTarget(null)} />
      )}
    </div>
  );
}

interface CredentialRowItemProps {
  credential: CredentialRow;
  onDelete: () => void;
}

/// Single row in the credentials list. Each row is intentionally
/// self-contained (no shared row primitive elsewhere in the app) so
/// the design stays small + greppable.
export function CredentialRowItem({ credential, onDelete }: CredentialRowItemProps) {
  const scopes = parseCredentialScopes(credential.scopes);
  const usedBy = credential.usedByCount;
  return (
    <li className="rounded-md border border-gray-800 bg-gray-900/40 p-4">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="text-sm font-medium text-gray-100 truncate">{credential.name}</span>
            <span className="inline-flex items-center rounded border border-indigo-700/40 bg-indigo-900/30 px-2 py-0.5 text-[10px] uppercase tracking-wide text-indigo-200">
              {formatCredentialKind(credential.kind)}
            </span>
            {credential.hostHint && (
              <span className="inline-flex items-center rounded border border-gray-700 bg-gray-800/50 px-2 py-0.5 text-[10px] text-gray-300">
                {credential.hostHint}
              </span>
            )}
          </div>
          {credential.publicPart && (
            <div className="mt-2 break-all font-mono text-[10px] text-gray-500">
              {credential.publicPart}
            </div>
          )}
          {scopes.length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1.5" aria-label="scopes">
              {scopes.map((s) => (
                <span
                  key={s}
                  className="inline-flex items-center rounded border border-emerald-700/40 bg-emerald-900/20 px-1.5 py-0.5 text-[10px] text-emerald-200"
                >
                  {s}
                </span>
              ))}
            </div>
          )}
          <div className="mt-3 flex items-center gap-3 text-[11px] text-gray-500">
            <span>Created {formatRelative(credential.createdAt)}</span>
            <span aria-hidden="true">·</span>
            <span data-testid="used-by-count">
              {usedBy === undefined
                ? "Used by —"
                : `Used by ${usedBy} project${usedBy === 1 ? "" : "s"}`}
            </span>
          </div>
        </div>
        <div className="shrink-0">
          <Button
            variant="danger"
            size="sm"
            onClick={onDelete}
            data-testid={`delete-credential-${credential.id}`}
          >
            Delete
          </Button>
        </div>
      </div>
    </li>
  );
}
