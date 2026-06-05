import { fetchJson } from "../api.js";

/// One declared cross-plan input on a v2 (DAG) plan, with its resolved
/// producer and whether that producer's output has been recorded yet.
/// Mirrors the Rust `PlanGraphInput` shape from `GET /api/plan-graph`.
export interface PlanGraphInput {
  /// The consuming plan's local name for this artifact.
  name: string;
  /// The producing plan (`input.fromPlan`). `null` for a malformed input
  /// with no producer — the dashboard flags it as unresolvable.
  fromPlan: string | null;
  /// The produced artifact name (`input.artifact`, defaulting to `name`).
  /// `null` when `fromPlan` is `null`.
  artifact: string | null;
  /// Whether the producing plan's End gate has recorded the output.
  satisfied: boolean;
}

/// One declared output on a v2 plan, with whether its End gate recorded it.
export interface PlanGraphOutput {
  name: string;
  satisfied: boolean;
}

/// Cross-plan artifact overlay for a single v2 plan. Only plans that declare
/// at least one input or output appear in the `/api/plan-graph` response.
export interface PlanGraphEntry {
  plan: string;
  inputs: PlanGraphInput[];
  outputs: PlanGraphOutput[];
}

/// Fetch the cross-plan dependency overlay (Task 4.3 of the DAG-based plan
/// model). Returns one entry per `schema_version: 2` plan that declares
/// cross-plan `inputs:`/`outputs:`; plans without artifacts (incl. every v1
/// plan) are omitted. The `MultiPlanBoard` joins this with the plan list it
/// already has (titles + progress + project grouping) to draw the graph.
export function fetchPlanGraph(): Promise<PlanGraphEntry[]> {
  return fetchJson<PlanGraphEntry[]>("/api/plan-graph");
}
