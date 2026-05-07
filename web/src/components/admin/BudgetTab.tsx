import { useCallback, useEffect, useMemo, useState } from "react";
import { fetchJson, putJson } from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { toastError } from "../../lib/toast.js";
import { Banner } from "../ui/Banner.js";
import { Button } from "../ui/Button.js";

/// Wire shape of `GET /api/orgs/:slug/budget` (api/billing.rs:88).
interface BudgetResponse {
  budget: BudgetRow | null;
  killSwitchActive: boolean;
}

/// `OrgBudget` from saas/billing.rs:13. Only `maxBudgetUsd` is editable
/// via the UI; the other fields are server-managed metadata.
interface BudgetRow {
  orgId: string;
  maxBudgetUsd: number;
  billingPeriod: string;
  periodStart: string | null;
  updatedAt: string;
}

/// Wire shape of `GET /api/orgs/:slug/usage` (api/billing.rs:66).
interface UsageResponse {
  summary: {
    orgId: string;
    periodKey: string;
    totalCostUsd: number;
    maxBudgetUsd: number | null;
    pctUsed: number | null;
    killSwitchActive: boolean;
  };
  users: Array<{
    userId: string;
    email: string;
    costUsd: number;
    quotaUsd: number | null;
  }>;
}

interface BudgetTabProps {
  orgSlug: string;
}

export function BudgetTab({ orgSlug }: BudgetTabProps) {
  const [budget, setBudget] = useState<BudgetResponse | null>(null);
  const [usage, setUsage] = useState<UsageResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState<string>("");
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [b, u] = await Promise.all([
        fetchJson<BudgetResponse>(
          `/api/orgs/${encodeURIComponent(orgSlug)}/budget`,
        ),
        fetchJson<UsageResponse>(
          `/api/orgs/${encodeURIComponent(orgSlug)}/usage`,
        ),
      ]);
      setBudget(b);
      setUsage(u);
      // Seed the draft from the server. Empty string = no budget set;
      // server treats `maxBudgetUsd: null` (or 0) as "delete the budget".
      setDraft(b.budget ? String(b.budget.maxBudgetUsd) : "");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [orgSlug]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  async function save() {
    setSaving(true);
    setSaved(false);
    try {
      const trimmed = draft.trim();
      const max =
        trimmed === "" ? null : Number.parseFloat(trimmed);
      if (max !== null && (Number.isNaN(max) || max < 0)) {
        toastError("Budget must be a non-negative number");
        setSaving(false);
        return;
      }
      await putJson(`/api/orgs/${encodeURIComponent(orgSlug)}/budget`, {
        maxBudgetUsd: max,
      });
      setSaved(true);
      await refresh();
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      toastError(e, "Save failed");
    } finally {
      setSaving(false);
    }
  }

  const totalSpent = usage?.summary.totalCostUsd ?? 0;
  const max = budget?.budget?.maxBudgetUsd ?? null;
  const pct = max && max > 0 ? Math.min(1, totalSpent / max) : null;

  // Color the bar by spend tier — emerald under 80%, amber 80-100%, red
  // at or over 100%. Mirrors the BudgetStatus enum tiers in
  // saas/billing.rs:51 so the UI and server agree on what is
  // "warning" vs "exceeded".
  const barClass = useMemo(() => {
    if (pct === null) return "bg-gray-700";
    if (pct >= 1) return "bg-red-500";
    if (pct >= 0.8) return "bg-amber-500";
    return "bg-emerald-500";
  }, [pct]);

  return (
    <div className="space-y-6">
      <div className="rounded border border-gray-800 bg-gray-900/40 p-4">
        <h2 className="text-sm font-semibold text-gray-200">Monthly budget</h2>
        <p className="text-[11px] text-gray-500 mt-1 mb-3 leading-relaxed">
          Cap the total agent cost across this org for the current billing
          period. Leave blank to remove the cap. The kill-switch flips on
          automatically at 100% (see Kill switch tab).
        </p>
        {loading && (
          <p className="text-xs text-gray-500" data-testid="budget-loading">
            Loading…
          </p>
        )}
        {error && <Banner>{error}</Banner>}
        {!loading && !error && (
          <div className="flex items-center gap-2">
            <span className="text-sm text-gray-500">$</span>
            <input
              type="number"
              inputMode="decimal"
              min={0}
              step={1}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              placeholder="No cap"
              aria-label="Monthly budget USD"
              data-testid="budget-input"
              className="w-32 bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 focus:outline-none focus:border-indigo-600"
            />
            <span className="text-xs text-gray-500">USD / month</span>
            <Button
              onClick={save}
              loading={saving}
              disabled={saving}
              variant="primary"
              size="sm"
            >
              Save
            </Button>
            {saved && (
              <span className="text-[11px] text-emerald-400">Saved.</span>
            )}
          </div>
        )}
      </div>

      <div className="rounded border border-gray-800 bg-gray-900/40 p-4">
        <h2 className="text-sm font-semibold text-gray-200">
          Usage this period
        </h2>
        {usage && (
          <p className="text-[11px] text-gray-500 mt-1 mb-3">
            Period: <span className="font-mono">{usage.summary.periodKey}</span>
          </p>
        )}
        {usage && (
          <div className="mb-4">
            <div className="flex items-baseline justify-between mb-1">
              <span
                className="text-base font-semibold text-gray-100"
                data-testid="budget-spent"
              >
                ${totalSpent.toFixed(2)}
              </span>
              <span className="text-xs text-gray-500">
                {max !== null ? (
                  <>
                    of <span className="text-gray-300">${max.toFixed(2)}</span>{" "}
                    {pct !== null && (
                      <span
                        data-testid="budget-pct"
                        className={
                          pct >= 1
                            ? "text-red-400"
                            : pct >= 0.8
                              ? "text-amber-400"
                              : "text-emerald-400"
                        }
                      >
                        ({Math.round(pct * 100)}%)
                      </span>
                    )}
                  </>
                ) : (
                  "no cap"
                )}
              </span>
            </div>
            <div
              className="h-2 w-full bg-gray-800 rounded overflow-hidden"
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={pct !== null ? Math.round(pct * 100) : 0}
              data-testid="budget-bar"
            >
              <div
                className={`h-full ${barClass} transition-all`}
                style={{ width: pct !== null ? `${pct * 100}%` : "0%" }}
              />
            </div>
          </div>
        )}

        {usage && usage.users.length > 0 && (
          <div data-testid="usage-by-user">
            <h3 className="text-xs uppercase tracking-wide text-gray-500 mb-2">
              By user
            </h3>
            <div className="space-y-1">
              {usage.users
                .slice()
                .sort((a, b) => b.costUsd - a.costUsd)
                .map((u) => (
                  <UsageRow key={u.userId} user={u} planMax={max} />
                ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function UsageRow({
  user,
  planMax,
}: {
  user: { userId: string; email: string; costUsd: number; quotaUsd: number | null };
  planMax: number | null;
}) {
  // Per-user bar uses the user's own quota when set, else the org cap as
  // a relative reference. With neither, we just render the dollar
  // amount without a bar.
  const ref = user.quotaUsd ?? planMax;
  const pct =
    ref !== null && ref > 0 ? Math.min(1, user.costUsd / ref) : null;
  const color =
    pct === null
      ? "bg-gray-700"
      : pct >= 1
        ? "bg-red-500"
        : pct >= 0.8
          ? "bg-amber-500"
          : "bg-emerald-500";
  return (
    <div className="flex items-center gap-3 text-xs">
      <div className="flex-1 min-w-0 truncate text-gray-300">{user.email}</div>
      <div className="w-32 h-1.5 bg-gray-800 rounded overflow-hidden">
        <div
          className={`h-full ${color}`}
          style={{ width: pct !== null ? `${pct * 100}%` : "0%" }}
        />
      </div>
      <div className="w-20 text-right text-gray-200 tabular-nums">
        ${user.costUsd.toFixed(2)}
      </div>
      <div className="w-20 text-right text-gray-500 tabular-nums">
        {user.quotaUsd !== null
          ? `/ $${user.quotaUsd.toFixed(2)}`
          : ref === planMax && planMax !== null
            ? "shared cap"
            : "no quota"}
      </div>
    </div>
  );
}
