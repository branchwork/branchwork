import { useEffect, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import { usePlanStore } from "../stores/plan-store.js";

/// Route data-dependency wrapper for `/plans/:planName`. Calls
/// `fetchPlans()` if the plans summary has not been fetched yet, holds
/// rendering on a loading state until the list is known, and surfaces a
/// "plan not found" UI when the URL plan name is absent from that list.
///
/// Without this guard a stale `selectedPlan` in the store would let
/// `/plans/does-not-exist` silently render the previously-open plan, or
/// render an empty PlanBoard with no signal to the user. Membership is
/// checked against the summary list (already loaded for the sidebar)
/// rather than against `selectedPlan`, which is owned by `RouteSync`
/// and updates asynchronously.
export function EnsurePlan({ children }: { children: ReactNode }) {
  const { planName } = useParams<{ planName: string }>();
  const plans = usePlanStore((s) => s.plans);
  const plansFetched = usePlanStore((s) => s.plansFetched);
  const fetchPlans = usePlanStore((s) => s.fetchPlans);

  useEffect(() => {
    if (!plansFetched) {
      fetchPlans().catch(() => {});
    }
  }, [plansFetched, fetchPlans]);

  if (!plansFetched) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-gray-500">
        Loading...
      </div>
    );
  }

  const exists = !!planName && plans.some((p) => p.name === planName);
  if (!exists) {
    return <PlanNotFound planName={planName ?? ""} />;
  }

  return <>{children}</>;
}

function PlanNotFound({ planName }: { planName: string }) {
  return (
    <div role="alert" className="p-6">
      <div className="rounded border border-red-700/50 bg-red-900/20 p-6 max-w-2xl">
        <h2 className="text-lg font-semibold text-red-100">Plan not found</h2>
        <p className="mt-1 text-sm text-red-200/80">
          No plan named <span className="font-mono text-red-100">{planName || "—"}</span> exists. It
          may have been deleted, archived, or the link is wrong.
        </p>
        <div className="mt-4">
          <Link
            to="/"
            className="inline-flex items-center rounded border border-red-700/60 bg-red-900/40 px-3 py-1.5 text-xs text-red-100 transition hover:bg-red-800/50 hover:text-white"
          >
            Back to dashboard
          </Link>
        </div>
      </div>
    </div>
  );
}
