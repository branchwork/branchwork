import { useCallback, useEffect, useMemo, useState } from "react";
import { fetchJson, postJson } from "../api.js";
import { useOrgStore } from "../stores/org-store.js";
import { useAuthStore } from "../stores/auth-store.js";
import { isAdminRole } from "./AdminPage.js";
import { formatRelative } from "../lib/time.js";
import { toastError } from "../lib/toast.js";
import { useToastStore } from "../stores/toast-store.js";
import { Button } from "./ui/Button.js";
import { Modal } from "./ui/Modal.js";

/// One recorded orphan worktree, snake_case to match the server's
/// `WorktreeOrphanRow` JSON (no camelCase mapping on the wire).
interface OrphanRow {
  project_slug: string | null;
  path: string;
  branch: string | null;
  last_modified: string | null;
  first_detected: string;
}

interface CleanupResult {
  removed: string[];
  failed: { path: string; error: string }[];
}

/// Pending confirmation. `paths === null` ⇒ "Cleanup all" (server cleans
/// every recorded orphan when `paths` is omitted); a one-element list ⇒ a
/// single per-row "Remove".
interface PendingCleanup {
  paths: string[] | null;
  label: string;
}

/// Admin-only sidebar section listing leaked per-agent worktrees recorded
/// by the server-boot orphan sweep (Phase 5 of worktree-per-agent-isolation).
///
/// Gating: shown only to a user with an admin/owner role on *some* org they
/// belong to. The orphan endpoint returns the GLOBAL set regardless of the
/// org slug used (orphans are a deployment-host concern, not org-scoped), so
/// any admin org works for the auth gate — this lets a standalone operator
/// (member of `default-org`, owner of their personal org) still reach their
/// leaked worktrees, matching how `api::orphan_worktrees` was tested.
///
/// The whole section self-hides when the user is not an admin or there are
/// no recorded orphans, so it adds no sidebar chrome in the common case.
export function OrphanWorktrees() {
  const memberships = useOrgStore((s) => s.memberships);
  const user = useAuthStore((s) => s.user);
  const pushToast = useToastStore((s) => s.push);

  // Prefer the active org if the caller is admin there (matches AdminPage's
  // gating); otherwise fall back to any org where they're admin.
  const adminOrg = useMemo(() => {
    const currentId = user?.orgId ?? null;
    const current = memberships.find((m) => m.id === currentId);
    if (current && isAdminRole(current.role)) return current;
    return memberships.find((m) => isAdminRole(m.role)) ?? null;
  }, [memberships, user]);

  const slug = adminOrg?.slug ?? null;

  const [orphans, setOrphans] = useState<OrphanRow[] | null>(null);
  const [pending, setPending] = useState<PendingCleanup | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!slug) return;
    try {
      const data = await fetchJson<{ orphans: OrphanRow[] }>(`/api/orgs/${slug}/orphan-worktrees`);
      setOrphans(data.orphans);
    } catch {
      // Best-effort: a failed fetch leaves the section hidden (orphans
      // stays null) rather than surfacing an error in the sidebar chrome.
      setOrphans((prev) => prev ?? null);
    }
  }, [slug]);

  useEffect(() => {
    if (slug) void load();
  }, [slug, load]);

  const runCleanup = useCallback(
    async (paths: string[] | null) => {
      if (!slug) return;
      setBusy(true);
      try {
        const result = await postJson<CleanupResult>(
          `/api/orgs/${slug}/orphan-worktrees/cleanup`,
          paths === null ? {} : { paths },
        );
        if (result.failed.length > 0) {
          pushToast({
            kind: result.removed.length > 0 ? "warn" : "error",
            title: `Removed ${result.removed.length}, ${result.failed.length} failed`,
            body: result.failed.map((f) => `${f.path}: ${f.error}`).join("\n"),
          });
        } else {
          const n = result.removed.length;
          pushToast({
            kind: "success",
            title: `Removed ${n} orphan worktree${n === 1 ? "" : "s"}`,
          });
        }
        await load();
      } catch (e) {
        toastError(e, "Cleanup failed");
      } finally {
        setBusy(false);
        setPending(null);
      }
    },
    [slug, load, pushToast],
  );

  // Hidden for non-admins and when there's nothing to reap.
  if (!adminOrg || !orphans || orphans.length === 0) return null;

  return (
    <div className="px-2 pb-2" data-testid="orphan-worktrees">
      <div className="bg-amber-900/20 border border-amber-800/40 rounded p-2">
        <div className="flex items-center justify-between mb-1.5">
          <h3 className="text-[10px] font-semibold text-amber-400 uppercase tracking-wider">
            Worktree orphans ({orphans.length})
          </h3>
          <button
            type="button"
            onClick={() =>
              setPending({
                paths: null,
                label: `all ${orphans.length} orphan worktree${orphans.length === 1 ? "" : "s"}`,
              })
            }
            disabled={busy}
            className="text-[10px] px-1.5 py-0.5 rounded border border-amber-700/60 text-amber-300 hover:bg-amber-800/40 disabled:opacity-50 transition"
          >
            Cleanup all
          </button>
        </div>
        <ul className="space-y-1 max-h-40 overflow-y-auto">
          {orphans.map((o) => {
            const name = o.path.split("/").filter(Boolean).slice(-2).join("/") || o.path;
            return (
              <li key={o.path} className="flex items-start justify-between gap-1.5 text-[10px]">
                <div className="min-w-0">
                  <div className="font-mono text-gray-300 truncate" title={o.path}>
                    {name}
                  </div>
                  <div className="text-gray-500 truncate">
                    {o.branch ?? "detached"}
                    {" · "}
                    {o.last_modified ? formatRelative(o.last_modified) : "mtime unknown"}
                  </div>
                </div>
                <button
                  type="button"
                  onClick={() => setPending({ paths: [o.path], label: name })}
                  disabled={busy}
                  aria-label={`Remove orphan worktree ${o.path}`}
                  className="flex-shrink-0 px-1.5 py-0.5 rounded border border-gray-700 text-gray-400 hover:border-red-600 hover:text-red-400 disabled:opacity-50 transition"
                >
                  Remove
                </button>
              </li>
            );
          })}
        </ul>
      </div>

      <Modal
        open={pending !== null}
        onClose={() => !busy && setPending(null)}
        title="Remove orphan worktree(s)?"
        description={
          pending
            ? `This permanently deletes ${pending.label} from disk (git worktree remove). This cannot be undone.`
            : ""
        }
      >
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setPending(null)} disabled={busy}>
            Cancel
          </Button>
          <Button
            variant="danger"
            size="sm"
            onClick={() => pending && runCleanup(pending.paths)}
            disabled={busy}
          >
            {busy ? "Removing…" : "Remove"}
          </Button>
        </div>
      </Modal>
    </div>
  );
}
