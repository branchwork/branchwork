import { useState } from "react";
import { usePlanStore, type GateKind, type PlanNode } from "../stores/plan-store.js";
import { postJson } from "../api.js";
import { toastError } from "../lib/toast.js";

/// The five visual states a gate card can render (Task 3.4). Derived from
/// the node's runtime `status` plus its `gateKind` — `node_status` alone is
/// `in_progress` for both "awaiting a human sign-off" (init / approval
/// gates) and "CI is in flight" (ci / end gates), so the kind disambiguates.
export type GateVisualState = "pending" | "checking" | "awaiting_approval" | "passed" | "failed";

/// Map a DAG gate node's runtime state to its visual state.
///
/// - `completed` / `skipped` → `passed` (green check)
/// - `failed` → `failed` (red X + Retry)
/// - `in_progress` + `init`/`approval` gate → `awaiting_approval` (pulsing
///   blue + Approve) — the gate engine left it `Blocked` on a human decision
/// - `in_progress` + `ci`/`end` gate → `checking` (spinner) — blocked on CI
/// - anything else (`pending`, unknown) → `pending` (gray)
export function gateVisualState(node: PlanNode): GateVisualState {
  const status = node.status ?? "pending";
  if (status === "completed" || status === "skipped") return "passed";
  if (status === "failed") return "failed";
  if (status === "in_progress") {
    return node.gateKind === "init" || node.gateKind === "approval"
      ? "awaiting_approval"
      : "checking";
  }
  return "pending";
}

function gateKindLabel(kind: GateKind | undefined): string {
  switch (kind) {
    case "init":
      return "Init";
    case "end":
      return "End";
    case "ci":
      return "CI";
    case "approval":
      return "Approval";
    default:
      return "Gate";
  }
}

interface StateConfig {
  /// Human label for the state pill.
  label: string;
  /// Glyph shown inside the diamond (ignored for `checking`, which renders a
  /// spinner). No emoji — text-presentation Unicode only, matching the repo
  /// convention (verdict badges use the same glyphs).
  glyph: string;
  /// Card chrome (border + background tint).
  chrome: string;
  /// Diamond indicator chrome (border + text colour + background).
  diamond: string;
  /// State pill chrome.
  pill: string;
  /// Whether the diamond pulses (awaiting approval draws the eye).
  pulse: boolean;
}

const GATE_STATE_CONFIG: Record<GateVisualState, StateConfig> = {
  pending: {
    label: "Pending",
    glyph: "◇", // ◇ white diamond
    chrome: "border-gray-700 bg-gray-900/40",
    diamond: "border-gray-600 bg-gray-800 text-gray-400",
    pill: "bg-gray-800 text-gray-400 border border-gray-700",
    pulse: false,
  },
  checking: {
    label: "Checking",
    glyph: "◈", // ◈ (unused — spinner renders instead)
    chrome: "border-amber-700/50 bg-amber-900/15",
    diamond: "border-amber-500/70 bg-amber-900/30 text-amber-300",
    pill: "bg-amber-900/40 text-amber-300 border border-amber-700/50",
    pulse: false,
  },
  awaiting_approval: {
    label: "Awaiting approval",
    glyph: "?",
    chrome: "border-blue-600/60 bg-blue-900/20 ring-1 ring-blue-500/30",
    diamond: "border-blue-400 bg-blue-900/40 text-blue-200",
    pill: "bg-blue-900/40 text-blue-200 border border-blue-600/50",
    pulse: true,
  },
  passed: {
    label: "Passed",
    glyph: "✓", // ✓
    chrome: "border-emerald-700/50 bg-emerald-900/15",
    diamond: "border-emerald-500/70 bg-emerald-900/30 text-emerald-300",
    pill: "bg-emerald-900/40 text-emerald-300 border border-emerald-700/50",
    pulse: false,
  },
  failed: {
    label: "Failed",
    glyph: "✗", // ✗
    chrome: "border-red-700/60 bg-red-900/20",
    diamond: "border-red-500/70 bg-red-900/40 text-red-300",
    pill: "bg-red-900/40 text-red-300 border border-red-600/50",
    pulse: false,
  },
};

interface GateCardProps {
  planName: string;
  /// The **scoped** node id (`init` at top level, `parent.child` inside a
  /// sub-plan) — used for the approve / retry endpoints and to keep the WS
  /// patch in sync. Defaults to `node.id` at the top level.
  nodeId: string;
  node: PlanNode;
}

/// Diamond-shaped card for a DAG gate node (Task 3.4 of dag-based-plan-model).
/// Visually distinct from the rectangular `TaskCard`: a prominent rotated
/// diamond status indicator anchors the row so gates read as verification
/// checkpoints between task groups rather than as work items.
///
/// Drives the two operator affordances:
/// - `awaiting_approval` → "Approve" → `POST .../gates/:id/approve`
/// - `failed` → "Retry" → `POST .../gates/:id/retry`
///
/// Both endpoints re-enter the DAG scheduler server-side; the card updates
/// optimistically and the paired WS event (`gate_status_changed`) reconciles.
export function GateCard({ planName, nodeId, node }: GateCardProps) {
  const patchNodeStatus = usePlanStore((s) => s.patchNodeStatus);
  const [busy, setBusy] = useState(false);

  const state = gateVisualState(node);
  const cfg = GATE_STATE_CONFIG[state];
  const title = node.title || nodeId;

  async function approve() {
    setBusy(true);
    try {
      await postJson(`/api/plans/${planName}/gates/${encodeURIComponent(nodeId)}/approve`, {});
      // Optimistic — the paired `gate_status_changed` WS event reconciles.
      patchNodeStatus(planName, nodeId, "completed");
    } catch (e) {
      toastError(e, "Approve gate failed");
    } finally {
      setBusy(false);
    }
  }

  async function retry() {
    setBusy(true);
    try {
      await postJson(`/api/plans/${planName}/gates/${encodeURIComponent(nodeId)}/retry`, {});
      patchNodeStatus(planName, nodeId, "pending");
    } catch (e) {
      toastError(e, "Retry gate failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="my-3 flex justify-center" data-testid="gate-card" data-gate-state={state}>
      <div
        className={`relative flex w-full max-w-2xl items-center gap-3 rounded-lg border px-4 py-2.5 ${cfg.chrome}`}
      >
        {/* Diamond status indicator — the defining visual that sets a gate
            apart from a rectangular task card. Rotated square with the glyph
            counter-rotated upright inside. */}
        <span
          aria-hidden="true"
          className={`flex h-9 w-9 flex-shrink-0 rotate-45 items-center justify-center rounded-[4px] border-2 ${cfg.diamond} ${cfg.pulse ? "animate-pulse" : ""}`}
        >
          {state === "checking" ? (
            <span className="h-3.5 w-3.5 -rotate-45 animate-spin rounded-full border-2 border-current border-t-transparent" />
          ) : (
            <span className="-rotate-45 text-sm font-bold leading-none">{cfg.glyph}</span>
          )}
        </span>

        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="text-[10px] font-semibold uppercase tracking-wide text-gray-500">
              {gateKindLabel(node.gateKind)} gate
            </span>
            <span className={`rounded-full px-1.5 py-0.5 text-[10px] font-medium ${cfg.pill}`}>
              {cfg.label}
            </span>
          </div>
          <div className="truncate text-sm font-medium text-gray-200" title={title}>
            {title}
          </div>
          {node.checks && node.checks.length > 0 && (
            <div className="mt-0.5 truncate font-mono text-[10px] text-gray-500">
              {node.checks.join(" · ")}
            </div>
          )}
        </div>

        {state === "awaiting_approval" && (
          <button
            type="button"
            onClick={approve}
            disabled={busy}
            className="flex-shrink-0 rounded border border-blue-500/60 bg-blue-600/30 px-3 py-1 text-xs font-medium text-blue-100 transition hover:bg-blue-600/50 hover:text-white disabled:opacity-50"
            title="Approve this gate and unblock the downstream nodes"
          >
            {busy ? "Approving…" : "Approve"}
          </button>
        )}
        {state === "failed" && (
          <button
            type="button"
            onClick={retry}
            disabled={busy}
            className="flex-shrink-0 rounded border border-red-500/60 bg-red-600/30 px-3 py-1 text-xs font-medium text-red-100 transition hover:bg-red-600/50 hover:text-white disabled:opacity-50"
            title="Re-run this gate's checks against the current state"
          >
            {busy ? "Retrying…" : "Retry"}
          </button>
        )}
      </div>
    </div>
  );
}
