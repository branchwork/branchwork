import { useCallback, useEffect, useState } from "react";
import { fetchJson, putJson } from "../../api.js";
import { errorMessage } from "../../lib/error.js";
import { toastError } from "../../lib/toast.js";
import { Banner } from "../ui/Banner.js";
import { Button } from "../ui/Button.js";

/// Wire shape of `GET /api/orgs/:slug/budget` (api/billing.rs:88) — the
/// kill switch's read side is folded into the budget endpoint server-side
/// (`is_kill_switch_active`). There is no dedicated GET for the switch.
interface BudgetResponse {
  budget: { maxBudgetUsd: number } | null;
  killSwitchActive: boolean;
}

interface KillSwitchTabProps {
  orgSlug: string;
}

export function KillSwitchTab({ orgSlug }: KillSwitchTabProps) {
  const [active, setActive] = useState<boolean | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const b = await fetchJson<BudgetResponse>(`/api/orgs/${encodeURIComponent(orgSlug)}/budget`);
      setActive(b.killSwitchActive);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, [orgSlug]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  async function toggle(next: boolean) {
    setBusy(true);
    try {
      await putJson(`/api/orgs/${encodeURIComponent(orgSlug)}/kill-switch`, {
        active: next,
        // `reason` is only meaningful when activating — clearing the
        // switch shouldn't carry a justification field.
        reason: next ? reason.trim() || null : null,
      });
      setActive(next);
      if (!next) setReason("");
    } catch (e) {
      toastError(e, "Save failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="rounded border border-gray-800 bg-gray-900/40 p-4 max-w-2xl">
      <h2 className="text-sm font-semibold text-gray-200">Kill switch</h2>
      <p className="text-[11px] text-gray-500 mt-1 mb-4 leading-relaxed">
        When active, the server refuses to spawn new agents for this org. Already-running agents
        continue. The switch flips on automatically when the org's monthly budget is exceeded;
        toggle here to override manually.
      </p>

      {loading && (
        <p className="text-xs text-gray-500" data-testid="kill-loading">
          Loading…
        </p>
      )}
      {error && <Banner>{error}</Banner>}
      {!loading && !error && active !== null && (
        <>
          <div className="flex items-center gap-3 mb-3">
            <span
              className={`inline-block w-2.5 h-2.5 rounded-full ${
                active ? "bg-red-500" : "bg-emerald-500"
              }`}
              aria-hidden="true"
            />
            <span className="text-sm text-gray-200" data-testid="kill-state">
              {active ? "Active — new agents blocked" : "Inactive — spawning allowed"}
            </span>
          </div>

          {!active && (
            <div className="space-y-2">
              <textarea
                value={reason}
                onChange={(e) => setReason(e.target.value)}
                placeholder="Reason (optional, recorded in audit log)"
                rows={2}
                aria-label="Kill switch reason"
                data-testid="kill-reason"
                className="w-full bg-gray-800 border border-gray-700 rounded px-3 py-1.5 text-sm text-gray-200 placeholder:text-gray-600 focus:outline-none focus:border-indigo-600"
              />
              <Button
                onClick={() => toggle(true)}
                loading={busy}
                disabled={busy}
                variant="danger"
                size="sm"
                data-testid="kill-activate"
              >
                Activate kill switch
              </Button>
            </div>
          )}

          {active && (
            <Button
              onClick={() => toggle(false)}
              loading={busy}
              disabled={busy}
              variant="primary"
              size="sm"
              data-testid="kill-deactivate"
            >
              Deactivate
            </Button>
          )}
        </>
      )}
    </div>
  );
}
