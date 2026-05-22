import { useState } from "react";
import { HttpError } from "../api.js";
import {
  useCredentialsStore,
  formatCredentialKind,
  type CredentialRow,
} from "../stores/credentials-store.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";
import { Banner } from "./ui/Banner.js";
import { errorMessage } from "../lib/error.js";

interface DeleteCredentialModalProps {
  credential: CredentialRow;
  onClose: () => void;
}

interface CredentialInUseBody {
  error: "credential_in_use";
  message?: string;
  projectIds?: string[];
}

/// Server response body for the 409 referential-integrity refusal.
/// Mirrors `server-rs/src/api/credentials.rs::delete_credential` —
/// `projectIds` is the list of `projects.id` rows that still carry
/// this credential as their `default_credential_id`.
function isCredentialInUseBody(body: unknown): body is CredentialInUseBody {
  return (
    typeof body === "object" &&
    body !== null &&
    "error" in body &&
    (body as { error?: unknown }).error === "credential_in_use"
  );
}

/// Confirmation modal for `DELETE /api/credentials/{id}`. Two paths:
///
/// - Happy path: server archives the row, modal closes, store
///   removes the row from the in-memory list (the WS layer does not
///   currently broadcast credential mutations, so the modal owns the
///   optimistic update).
/// - 409 `credential_in_use`: server refuses because at least one
///   project still references this credential as its default. The
///   modal shows the affected project ids inline so the operator
///   knows what to clear. We deliberately do NOT auto-navigate to
///   the projects view — the operator may want to clear default on
///   multiple projects in a batch.
export function DeleteCredentialModal({ credential, onClose }: DeleteCredentialModalProps) {
  const deleteCredential = useCredentialsStore((s) => s.deleteCredential);

  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [affectedProjects, setAffectedProjects] = useState<string[] | null>(null);

  async function handleDelete() {
    setError(null);
    setSubmitting(true);
    try {
      await deleteCredential(credential.id);
      onClose();
    } catch (e) {
      if (e instanceof HttpError && e.status === 409 && isCredentialInUseBody(e.body)) {
        setAffectedProjects(e.body.projectIds ?? []);
        setError(
          e.body.message ??
            "This credential is the default for at least one project. Clear the default on those projects before deleting.",
        );
      } else if (e instanceof HttpError) {
        const body = e.body as { error?: string; message?: string } | undefined;
        setError(body?.message ?? body?.error ?? errorMessage(e));
      } else {
        setError(errorMessage(e));
      }
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Modal
      open={true}
      onClose={() => (submitting ? undefined : onClose())}
      title="Delete credential"
      description={`This archives "${credential.name}" (${formatCredentialKind(credential.kind)}). The encrypted secret stays on disk for audit; the row is soft-deleted.`}
      size="md"
    >
      <div className="mt-4 space-y-3">
        <div className="rounded border border-gray-800 bg-gray-900/40 p-3 text-xs">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-medium text-gray-100">{credential.name}</span>
            <span className="inline-flex items-center rounded border border-indigo-700/40 bg-indigo-900/30 px-2 py-0.5 text-[10px] uppercase tracking-wide text-indigo-200">
              {formatCredentialKind(credential.kind)}
            </span>
            {credential.hostHint && (
              <span className="inline-flex items-center rounded border border-gray-700 bg-gray-800/50 px-2 py-0.5 text-[10px] text-gray-300">
                {credential.hostHint}
              </span>
            )}
          </div>
          {credential.usedByCount !== undefined && credential.usedByCount > 0 && !error && (
            <p className="mt-2 text-gray-400">
              <span className="text-amber-400">⚠</span> Used by {credential.usedByCount} project
              {credential.usedByCount === 1 ? "" : "s"}.
            </p>
          )}
        </div>

        {error && (
          <Banner kind="error">
            <div>{error}</div>
            {affectedProjects && affectedProjects.length > 0 && (
              <div className="mt-2">
                <div className="font-medium text-xs">
                  Affected project{affectedProjects.length === 1 ? "" : "s"}:
                </div>
                <ul
                  className="list-disc list-inside text-[11px] mt-1 font-mono"
                  data-testid="affected-projects-list"
                >
                  {affectedProjects.map((id) => (
                    <li key={id} className="break-all">
                      {id}
                    </li>
                  ))}
                </ul>
                <p className="text-[11px] mt-2 text-gray-300">
                  Clear the default credential on each project (or delete the project rows) and try
                  again.
                </p>
              </div>
            )}
          </Banner>
        )}

        <div className="flex justify-end gap-2 pt-1">
          <Button variant="secondary" size="sm" onClick={onClose} disabled={submitting}>
            Cancel
          </Button>
          <Button
            variant="danger"
            size="sm"
            onClick={handleDelete}
            loading={submitting}
            data-testid="confirm-delete-credential"
          >
            Delete credential
          </Button>
        </div>
      </div>
    </Modal>
  );
}
