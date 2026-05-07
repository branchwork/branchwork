import { useCallback, useEffect, useMemo, useState } from "react";
import { fetchJson, putJson } from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { toastError } from "../../lib/toast.js";
import { Banner } from "../ui/Banner.js";
import { Button } from "../ui/Button.js";

/// `UserQuota` from saas/billing.rs:23.
interface UserQuotaRow {
  orgId: string;
  userId: string;
  maxBudgetUsd: number;
  updatedAt: string;
}

/// `UserUsage` from saas/billing.rs:43 — surfaced as `users[]` on
/// `GET /api/orgs/:slug/usage`. Used to render an email + period-spend
/// next to each quota row, since `/user-quotas` only carries user_id.
interface UsageUser {
  userId: string;
  email: string;
  costUsd: number;
  quotaUsd: number | null;
}

interface UserQuotasTabProps {
  orgSlug: string;
}

export function UserQuotasTab({ orgSlug }: UserQuotasTabProps) {
  const [quotas, setQuotas] = useState<UserQuotaRow[] | null>(null);
  const [users, setUsers] = useState<UsageUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [busyUserId, setBusyUserId] = useState<string | null>(null);
  const [newUserEmail, setNewUserEmail] = useState("");
  const [newUserAmount, setNewUserAmount] = useState("");
  const [newSaving, setNewSaving] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [quotasRes, usageRes] = await Promise.all([
        fetchJson<UserQuotaRow[]>(
          `/api/orgs/${encodeURIComponent(orgSlug)}/user-quotas`,
        ),
        fetchJson<{ users: UsageUser[] }>(
          `/api/orgs/${encodeURIComponent(orgSlug)}/usage`,
        ),
      ]);
      setQuotas(quotasRes);
      setUsers(usageRes.users ?? []);
      // Seed editable drafts from the server. Drafts that the user has
      // already typed are not clobbered on refetch (would be very
      // surprising mid-edit) — only fill in keys we don't already
      // track.
      setDrafts((prev) => {
        const next = { ...prev };
        for (const q of quotasRes) {
          if (!(q.userId in next)) next[q.userId] = String(q.maxBudgetUsd);
        }
        return next;
      });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [orgSlug]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Map user_id -> email/cost lookup so each quota row can show the
  // human-readable email rather than the opaque uuid. Quota rows for
  // users with zero spend in the current period won't be in `users[]`,
  // so we fall back to "unknown" for those.
  const usersById = useMemo(() => {
    const m = new Map<string, UsageUser>();
    for (const u of users) m.set(u.userId, u);
    return m;
  }, [users]);

  async function setQuota(userId: string, raw: string) {
    setBusyUserId(userId);
    try {
      const trimmed = raw.trim();
      const max = trimmed === "" ? null : Number.parseFloat(trimmed);
      if (max !== null && (Number.isNaN(max) || max < 0)) {
        toastError("Quota must be a non-negative number");
        return;
      }
      await putJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/user-quotas/${encodeURIComponent(userId)}`,
        { maxBudgetUsd: max },
      );
      await refresh();
    } catch (e) {
      toastError(e, "Save failed");
    } finally {
      setBusyUserId(null);
    }
  }

  async function addQuotaForEmail() {
    const email = newUserEmail.trim().toLowerCase();
    if (!email) return;
    const u = users.find((x) => x.email.toLowerCase() === email);
    if (!u) {
      toastError(
        "User has no agent activity in this period yet — quotas can only be set for known users",
        "Quota failed",
      );
      return;
    }
    const trimmed = newUserAmount.trim();
    if (trimmed === "") {
      toastError("Quota amount required");
      return;
    }
    const max = Number.parseFloat(trimmed);
    if (Number.isNaN(max) || max < 0) {
      toastError("Quota must be a non-negative number");
      return;
    }
    setNewSaving(true);
    try {
      await putJson(
        `/api/orgs/${encodeURIComponent(orgSlug)}/user-quotas/${encodeURIComponent(u.userId)}`,
        { maxBudgetUsd: max },
      );
      setNewUserEmail("");
      setNewUserAmount("");
      await refresh();
    } catch (e) {
      toastError(e, "Save failed");
    } finally {
      setNewSaving(false);
    }
  }

  return (
    <div className="space-y-6">
      <div className="rounded border border-gray-800 bg-gray-900/40 p-4 max-w-2xl">
        <h2 className="text-sm font-semibold text-gray-200">
          Set per-user quota
        </h2>
        <p className="text-[11px] text-gray-500 mt-1 mb-3 leading-relaxed">
          Override the org budget for individual users. Quotas apply to the
          monthly billing period and reset when it rolls over.
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="email"
            value={newUserEmail}
            onChange={(e) => setNewUserEmail(e.target.value)}
            placeholder="user@example.com"
            aria-label="Quota target email"
            data-testid="quota-new-email"
            className="flex-1 min-w-[220px] bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
          />
          <input
            type="number"
            inputMode="decimal"
            min={0}
            step={1}
            value={newUserAmount}
            onChange={(e) => setNewUserAmount(e.target.value)}
            placeholder="USD"
            aria-label="Quota amount USD"
            data-testid="quota-new-amount"
            className="w-28 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
          />
          <Button
            onClick={addQuotaForEmail}
            loading={newSaving}
            disabled={newSaving || newUserEmail.trim() === ""}
            variant="primary"
            size="sm"
          >
            Set
          </Button>
        </div>
      </div>

      <div>
        <h2 className="text-sm font-semibold text-gray-200 mb-2">
          Current quotas
        </h2>
        {loading && (
          <p className="text-xs text-gray-500" data-testid="quotas-loading">
            Loading…
          </p>
        )}
        {error && <Banner>{error}</Banner>}
        {!loading && !error && quotas && quotas.length === 0 && (
          <p className="text-xs text-gray-500">
            No per-user quotas set. All users inherit the org budget.
          </p>
        )}
        {!loading && !error && quotas && quotas.length > 0 && (
          <div
            className="rounded border border-gray-800 divide-y divide-gray-800"
            data-testid="quotas-list"
          >
            {quotas.map((q) => {
              const u = usersById.get(q.userId);
              const draft = drafts[q.userId] ?? String(q.maxBudgetUsd);
              const dirty = draft !== String(q.maxBudgetUsd);
              const busy = busyUserId === q.userId;
              return (
                <div
                  key={q.userId}
                  className="flex flex-wrap items-center gap-3 px-3 py-2"
                  data-testid={`quota-row-${q.userId}`}
                >
                  <div className="flex-1 min-w-0">
                    <div className="text-sm text-gray-200 truncate">
                      {u?.email ?? (
                        <span className="text-gray-500">{q.userId}</span>
                      )}
                    </div>
                    {u && (
                      <div className="text-[11px] text-gray-500">
                        Spent ${u.costUsd.toFixed(2)} this period
                      </div>
                    )}
                  </div>
                  <span className="text-xs text-gray-500">$</span>
                  <input
                    type="number"
                    inputMode="decimal"
                    min={0}
                    step={1}
                    value={draft}
                    onChange={(e) =>
                      setDrafts((prev) => ({
                        ...prev,
                        [q.userId]: e.target.value,
                      }))
                    }
                    aria-label={`Quota for ${u?.email ?? q.userId}`}
                    data-testid={`quota-input-${q.userId}`}
                    className="w-24 bg-gray-800 border border-gray-700 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-indigo-600"
                  />
                  <Button
                    onClick={() => setQuota(q.userId, draft)}
                    disabled={!dirty || busy}
                    loading={busy}
                    variant="primary"
                    size="sm"
                    data-testid={`quota-save-${q.userId}`}
                  >
                    Save
                  </Button>
                  <Button
                    onClick={() => setQuota(q.userId, "")}
                    disabled={busy}
                    variant="ghost"
                    size="sm"
                    data-testid={`quota-clear-${q.userId}`}
                  >
                    Clear
                  </Button>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
