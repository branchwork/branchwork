import { useCallback, useEffect, useMemo, useState } from "react";
import {
  HttpError,
  deleteJson,
  fetchJson,
  postJson,
  putJson,
} from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { toastError } from "../../lib/toast.js";
import { Banner } from "../ui/Banner.js";
import { Button } from "../ui/Button.js";

/// Roles the server accepts on `POST /api/orgs/:slug/members` and on
/// `PUT /api/orgs/:slug/members/:user_id/role`. Source of truth lives in
/// `server-rs/src/auth/orgs.rs::VALID_ROLES`. Owner is intentionally not
/// in the picker — promoting a second owner is a deliberate action that
/// the server allows but should not be one click away.
const ASSIGNABLE_ROLES: ReadonlyArray<{ value: string; label: string }> = [
  { value: "admin", label: "Admin" },
  { value: "member", label: "Member" },
  { value: "viewer", label: "Viewer" },
];

interface OrgMember {
  userId: string;
  email: string;
  role: string;
  joinedAt: string;
}

interface OrgDetail {
  id: string;
  name: string;
  slug: string;
  createdAt: string;
  members: OrgMember[];
}

interface MembersTabProps {
  orgSlug: string;
  /// Caller's role in the org. Drives whether mutating UI (invite,
  /// remove, role pick) renders. Server enforces the same authz; this
  /// is purely UX (audit §17 — non-admin views are read-only).
  callerRole: string | null;
  /// Caller's user id. Used to render a "(you)" badge on the caller's
  /// own row and to allow self-removal even when the caller is not an
  /// admin.
  callerUserId: string | null;
}

export function MembersTab({
  orgSlug,
  callerRole,
  callerUserId,
}: MembersTabProps) {
  const [members, setMembers] = useState<OrgMember[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [busyUserId, setBusyUserId] = useState<string | null>(null);
  const [inviteEmail, setInviteEmail] = useState("");
  const [inviteRole, setInviteRole] = useState<string>("member");
  const [inviteStatus, setInviteStatus] = useState<"idle" | "saving">("idle");

  const isOwner = callerRole === "owner";
  const canInviteOrRemove = isOwner || callerRole === "admin";
  // Server only accepts role changes from owners (`owner_only`), so a
  // plain admin sees the role label as plain text instead of a picker
  // even though they can still add/remove members.
  const canChangeRole = isOwner;

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const detail = await fetchJson<OrgDetail>(
        `/api/orgs/${encodeURIComponent(orgSlug)}`,
      );
      setMembers(detail.members);
    } catch (e) {
      // Surface inline so the operator can retry without leaving the
      // tab. We do NOT toast here: the empty state already conveys the
      // failure and a toast on every reload would be noisy.
      setError(errorMessage(e));
      setMembers(null);
    } finally {
      setLoading(false);
    }
  }, [orgSlug]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  async function invite() {
    const email = inviteEmail.trim().toLowerCase();
    if (!email) return;
    setInviteStatus("saving");
    try {
      await postJson(`/api/orgs/${encodeURIComponent(orgSlug)}/members`, {
        email,
        role: inviteRole,
      });
      setInviteEmail("");
      setInviteRole("member");
      await refresh();
    } catch (e) {
      // Inline error: surface the server's message verbatim (e.g.
      // `user_not_found`, `invalid_role`) so the operator can fix the
      // input before retrying.
      const msg =
        e instanceof HttpError && typeof e.body === "object" && e.body
          ? ((e.body as { error?: string }).error ?? errorMessage(e))
          : errorMessage(e);
      toastError(msg, "Invite failed");
    } finally {
      setInviteStatus("idle");
    }
  }

  async function removeMember(userId: string) {
    setBusyUserId(userId);
    try {
      await deleteJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/members/${encodeURIComponent(userId)}`,
      );
      await refresh();
    } catch (e) {
      toastError(e, "Remove failed");
    } finally {
      setBusyUserId(null);
    }
  }

  async function setRole(userId: string, role: string) {
    setBusyUserId(userId);
    try {
      await putJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/members/${encodeURIComponent(userId)}/role`,
        { role },
      );
      await refresh();
    } catch (e) {
      toastError(e, "Role change failed");
    } finally {
      setBusyUserId(null);
    }
  }

  const sorted = useMemo(() => {
    if (!members) return [];
    return [...members].sort((a, b) => a.email.localeCompare(b.email));
  }, [members]);

  return (
    <div className="space-y-6">
      {canInviteOrRemove && (
        <div className="rounded border border-gray-800 bg-gray-900/40 p-4">
          <h2 className="text-sm font-semibold text-gray-200">Invite member</h2>
          <p className="text-[11px] text-gray-500 mt-1 mb-3 leading-relaxed">
            The user must already have an account on this server. We do not
            email invitations — share the dashboard URL out-of-band.
          </p>
          <div className="flex flex-wrap gap-2">
            <input
              type="email"
              value={inviteEmail}
              onChange={(e) => setInviteEmail(e.target.value)}
              placeholder="user@example.com"
              aria-label="Invite member email"
              data-testid="invite-email"
              className="flex-1 min-w-[220px] bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
            />
            <select
              value={inviteRole}
              onChange={(e) => setInviteRole(e.target.value)}
              aria-label="Invite member role"
              data-testid="invite-role"
              className="bg-gray-800 border border-gray-700 rounded px-2 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
            >
              {ASSIGNABLE_ROLES.map((r) => (
                <option key={r.value} value={r.value}>
                  {r.label}
                </option>
              ))}
            </select>
            <Button
              onClick={invite}
              loading={inviteStatus === "saving"}
              disabled={inviteStatus === "saving" || inviteEmail.trim() === ""}
              variant="primary"
              size="sm"
            >
              Invite
            </Button>
          </div>
        </div>
      )}

      <div>
        <div className="flex items-baseline justify-between mb-2">
          <h2 className="text-sm font-semibold text-gray-200">Members</h2>
          {members && (
            <span className="text-[11px] text-gray-500">
              {members.length === 1 ? "1 member" : `${members.length} members`}
            </span>
          )}
        </div>
        {loading && (
          <p className="text-xs text-gray-500" data-testid="members-loading">
            Loading members…
          </p>
        )}
        {error && <Banner>{error}</Banner>}
        {!loading && !error && members && members.length === 0 && (
          <p className="text-xs text-gray-500">No members yet.</p>
        )}
        {!loading && !error && sorted.length > 0 && (
          <div
            className="rounded border border-gray-800 divide-y divide-gray-800"
            data-testid="members-list"
          >
            {sorted.map((m) => {
              const isSelf = m.userId === callerUserId;
              const isOnlyOwner =
                m.role === "owner" &&
                sorted.filter((x) => x.role === "owner").length <= 1;
              const canRemove =
                (canInviteOrRemove || isSelf) && !isOnlyOwner;
              const showRolePicker = canChangeRole && !isOnlyOwner;
              const busy = busyUserId === m.userId;
              return (
                <div
                  key={m.userId}
                  className="flex items-center gap-3 px-3 py-2"
                  data-testid={`member-row-${m.userId}`}
                >
                  <div className="flex-1 min-w-0">
                    <div className="text-sm text-gray-200 truncate">
                      {m.email}
                      {isSelf && (
                        <span className="ml-2 text-[10px] uppercase text-gray-500">
                          you
                        </span>
                      )}
                    </div>
                    <div className="text-[11px] text-gray-500">
                      Joined {m.joinedAt.replace("T", " ").slice(0, 16)}
                    </div>
                  </div>
                  {showRolePicker ? (
                    <select
                      value={m.role}
                      onChange={(e) => setRole(m.userId, e.target.value)}
                      disabled={busy}
                      aria-label={`Role for ${m.email}`}
                      data-testid={`role-${m.userId}`}
                      className="bg-gray-800 border border-gray-700 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-indigo-600 disabled:opacity-60"
                    >
                      {[
                        { value: "owner", label: "Owner" },
                        ...ASSIGNABLE_ROLES,
                      ].map((r) => (
                        <option key={r.value} value={r.value}>
                          {r.label}
                        </option>
                      ))}
                    </select>
                  ) : (
                    <span
                      className="text-xs text-gray-400 capitalize"
                      data-testid={`role-${m.userId}`}
                    >
                      {m.role}
                    </span>
                  )}
                  {canRemove && (
                    <Button
                      onClick={() => removeMember(m.userId)}
                      disabled={busy}
                      loading={busy}
                      variant="danger"
                      size="sm"
                      data-testid={`remove-${m.userId}`}
                    >
                      {isSelf ? "Leave" : "Remove"}
                    </Button>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
