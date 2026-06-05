import { useEffect, useState } from "react";
import { fetchJson, postJson, HttpError } from "../api.js";
import { usePlanStore, type InitGatePending, type ParsedPlan } from "../stores/plan-store.js";
import { subscribeToWsEvents } from "../stores/ws-store.js";
import { Modal } from "./ui/Modal.js";
import { Button } from "./ui/Button.js";
import { Banner } from "./ui/Banner.js";
import { errorMessage } from "../lib/error.js";

/// Human-friendly labels for the init gate's precondition check names.
/// The server (`gates.rs::run_init_check`) runs `git_repo` /
/// `remote_configured` / `clean_tree` by default, or a custom `checks`
/// list. `gate_ready_for_approval` only fires once ALL of them passed,
/// so every rendered row is green ✓ — an unknown name falls back to the
/// raw string so a future check shows up without a UI change.
export const INIT_CHECK_LABELS: Record<string, string> = {
  git_repo: "Git repository initialized",
  remote_configured: "Git remote configured",
  clean_tree: "Working tree is clean",
};

function checkLabel(name: string): string {
  return INIT_CHECK_LABELS[name] ?? name;
}

/// Populate the `initGates` slice for a newly-ready init gate. The
/// `gate_ready_for_approval` event carries the gate node id + verified
/// checks but not the plan title/context, so fetch the plan to fill
/// those in. The fetch is best-effort: on failure we still surface the
/// modal with the summary-list title (or the plan name) so the operator
/// can approve — "Start Plan" only needs the plan name + node id, both
/// of which the event already carried.
function applyInitGateReady(planName: string, nodeId: string, checks: string[]): void {
  void (async () => {
    let planTitle =
      usePlanStore.getState().plans.find((p) => p.name === planName)?.title ?? planName;
    let context = "";
    try {
      const plan = await fetchJson<ParsedPlan>(`/api/plans/${encodeURIComponent(planName)}`);
      planTitle = plan.title || planTitle;
      context = plan.context ?? "";
    } catch {
      // Network blip — show the modal with the fallback title; the
      // approve POST is unaffected (it only needs name + node id).
    }
    usePlanStore.getState().setInitGatePending(planName, {
      nodeId,
      planTitle,
      context,
      checks,
      dismissed: false,
    });
  })();
}

/// Global init-gate approval modal (Task 3.5 of the dag-based-plan-model
/// plan). Mounted once at the App level so it appears over any route the
/// operator happens to be on when a v2 (DAG) plan's `init` gate passes
/// its preconditions.
///
/// Triggered by the `gate_ready_for_approval` WS event where
/// `gate_kind === "init"` — the gate engine (`gates.rs::execute_init_gate`)
/// broadcasts it after `git_repo` / `remote_configured` / `clean_tree`
/// (or the plan's custom `checks`) all pass, then leaves the gate
/// `Blocked` (the scheduler does not re-poll it). The operator clicks
/// "Start Plan" → `POST /api/plans/:name/gates/:node_id/approve` →
/// `dag_scheduler::try_dag_advance` schedules the downstream nodes the
/// gate was blocking.
///
/// Subscribes via `subscribeToWsEvents()` (ws-store) rather than adding a
/// dispatch-switch case — the subscription writes the pending state into
/// the transient `initGates` plan-store slice that both this modal and
/// the PlanBoard `<InitGateWaitingBanner/>` read. The subscription
/// outlives reconnects (the registry is decoupled from the socket).
export function InitGateModal() {
  // Subscribe once. The handler runs after the built-in dispatch switch;
  // gate events have no built-in case (they fall through to subscribers),
  // so this is the only consumer.
  useEffect(() => {
    const unsubscribe = subscribeToWsEvents(
      ["gate_ready_for_approval", "gate_approved", "gate_failed", "gate_status_changed"],
      (msg) => {
        const store = usePlanStore.getState();
        if (msg.type === "gate_ready_for_approval") {
          // Only init gates pop the modal. Approval gates (the other
          // human-sign-off gate kind) get a follow-up surface; CI / end
          // gates resolve out-of-band with no human action.
          if (msg.data.gate_kind !== "init") return;
          applyInitGateReady(msg.data.plan_name, msg.data.node_id, msg.data.checks ?? []);
          return;
        }
        // Terminal events clear the pending init gate, but only when they
        // name the SAME node we're tracking (a `gate_failed` for some other
        // node in the plan must not dismiss the init-gate modal).
        const existing = store.initGates[msg.data.plan_name];
        if (!existing || existing.nodeId !== msg.data.node_id) return;
        if (msg.type === "gate_approved" || msg.type === "gate_failed") {
          store.setInitGatePending(msg.data.plan_name, null);
        } else if (
          msg.type === "gate_status_changed" &&
          (msg.data.status === "completed" || msg.data.status === "failed")
        ) {
          // The scheduler broadcasts `gate_status_changed: in_progress`
          // right BEFORE `gate_ready_for_approval`; only the terminal
          // completed/failed transitions clear the modal.
          store.setInitGatePending(msg.data.plan_name, null);
        }
      },
    );
    return unsubscribe;
  }, []);

  const initGates = usePlanStore((s) => s.initGates);

  // Show the first non-dismissed pending init gate. Multiple concurrent
  // v2 plan creations queue naturally: approving/dismissing the front one
  // surfaces the next.
  const entry = Object.entries(initGates).find(([, g]) => !g.dismissed);
  if (!entry) return null;
  const [planName, gate] = entry;
  // `key` resets the inner dialog's local approve state when the front
  // plan changes.
  return <InitGateDialog key={planName} planName={planName} gate={gate} />;
}

function InitGateDialog({ planName, gate }: { planName: string; gate: InitGatePending }) {
  const setInitGatePending = usePlanStore((s) => s.setInitGatePending);
  const dismissInitGate = usePlanStore((s) => s.dismissInitGate);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleStart() {
    setError(null);
    setSubmitting(true);
    try {
      await postJson(
        `/api/plans/${encodeURIComponent(planName)}/gates/${encodeURIComponent(gate.nodeId)}/approve`,
        {},
      );
      // Optimistic clear; the `gate_approved` WS event also clears the
      // slice (idempotent — a second clear is a no-op).
      setInitGatePending(planName, null);
    } catch (e) {
      const body =
        e instanceof HttpError
          ? (e.body as { error?: string; message?: string } | undefined)
          : undefined;
      setError(body?.message ?? body?.error ?? errorMessage(e));
      setSubmitting(false);
    }
  }

  return (
    <Modal
      open={true}
      onClose={() => (submitting ? undefined : dismissInitGate(planName, true))}
      title={`Start "${gate.planTitle}"?`}
      description="This plan's init gate passed all preconditions. Review the context and confirm to start the first tasks."
      size="md"
    >
      <div className="mt-4 space-y-4" data-testid="init-gate-modal">
        {gate.context && (
          <div className="rounded border border-gray-800 bg-gray-900/40 p-3 text-xs text-gray-300 whitespace-pre-wrap max-h-48 overflow-y-auto">
            {gate.context}
          </div>
        )}
        <div>
          <div className="mb-2 text-xs font-medium uppercase tracking-wide text-gray-400">
            Preconditions
          </div>
          {gate.checks.length === 0 ? (
            <div className="flex items-center gap-2 text-sm text-emerald-300">
              <span aria-hidden="true" className="text-emerald-400">
                ✓
              </span>
              All preconditions passed
            </div>
          ) : (
            <ul className="space-y-1.5" data-testid="init-gate-checks">
              {gate.checks.map((c) => (
                <li key={c} className="flex items-center gap-2 text-sm text-gray-200">
                  <span aria-hidden="true" className="text-emerald-400">
                    ✓
                  </span>
                  {checkLabel(c)}
                </li>
              ))}
            </ul>
          )}
        </div>

        {error && <Banner kind="error">{error}</Banner>}

        <div className="flex justify-end gap-2 pt-1">
          <Button
            variant="secondary"
            size="sm"
            onClick={() => dismissInitGate(planName, true)}
            disabled={submitting}
          >
            Later
          </Button>
          <Button
            variant="primary"
            size="sm"
            onClick={handleStart}
            loading={submitting}
            data-testid="init-gate-start"
          >
            Start Plan
          </Button>
        </div>
      </div>
    </Modal>
  );
}
