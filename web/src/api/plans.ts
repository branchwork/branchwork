import { fetchJson, putJson } from "../api.js";

/// Repo-level (`branchwork.toml`) defaults the plan settings panel
/// surfaces as read-only context. Each field is `undefined` when the
/// project has no `branchwork.toml` (or it leaves that key absent).
export interface RepoDefaults {
  ciBlockingWorkflows?: string[];
  ciBlockingWorkflowsSkip?: string[];
  phaseVerification?: string;
  /// `[auto_mode] merge_cadence` from the project `branchwork.toml`.
  /// Omitted when the file is absent OR when it carries the hard-coded
  /// default (`phase`) — the UI's fallback chain treats `undefined` as
  /// "use the hard-coded default".
  mergeCadence?: MergeCadence;
}

/// The three legal cadences for `plan_auto_mode.merge_cadence` /
/// `[auto_mode] merge_cadence` in `branchwork.toml`. Mirrors the Rust
/// `MergeCadence` enum (see server-rs/src/repo_config.rs).
export type MergeCadence = "task" | "phase" | "plan";

/// Hard-coded fallback used by both the server-side
/// `RepoConfig::default()` and the client when nothing else in the
/// resolution chain (`plan -> repo -> default`) sets a value. Kept in
/// lockstep with `MergeCadence::Phase` as the `#[default]` variant.
export const DEFAULT_MERGE_CADENCE: MergeCadence = "phase";

/// Mirrors the Rust `PlanSettings` shape returned by GET
/// `/api/plans/:name/settings`. `ciBlockingWorkflows` and
/// `phaseVerification` are the plan-level overrides — `null` (or absent)
/// means "inherit from the lower layer (repo default → smart default)".
export interface PlanSettings {
  ciBlockingWorkflows: string[] | null;
  phaseVerification: string | null;
  /// Plan-level merge-cadence override. `null` (or absent) means
  /// "inherit the project default" — UI falls back to
  /// `repoDefaults.mergeCadence`, then `DEFAULT_MERGE_CADENCE`.
  mergeCadence: MergeCadence | null;
  /// Workflow names enumerated from `<project>/.github/workflows/*.yml|*.yaml`.
  /// Empty when the project has no workflows directory or none parse.
  availableWorkflows: string[];
  repoDefaults: RepoDefaults;
}

/// Three-state field for PUT: `undefined` leaves the YAML key untouched,
/// `null` removes it, a value writes/replaces. Mirrors the
/// `deserialize_some` shim on the server.
export type Tristate<T> = T | null | undefined;

export interface PlanSettingsBody {
  ciBlockingWorkflows?: Tristate<string[]>;
  phaseVerification?: Tristate<string>;
  /// Tristate cadence override. `undefined` leaves the DB column
  /// untouched, `null` clears the override back to "inherit project
  /// default", a value writes the explicit pin.
  mergeCadence?: Tristate<MergeCadence>;
}

/// Resolve which level of the cadence inheritance chain provides the
/// active value, mirroring how the auto-mode loop will read it
/// server-side. Plan-level explicit pin → repo default → hard-coded
/// `DEFAULT_MERGE_CADENCE` (`phase`).
export type MergeCadenceSource = "plan" | "repo default" | "default";

export function resolveMergeCadence(s: PlanSettings): {
  value: MergeCadence;
  source: MergeCadenceSource;
} {
  if (s.mergeCadence !== null) return { value: s.mergeCadence, source: "plan" };
  if (s.repoDefaults.mergeCadence !== undefined) {
    return { value: s.repoDefaults.mergeCadence, source: "repo default" };
  }
  return { value: DEFAULT_MERGE_CADENCE, source: "default" };
}

export function getPlanSettings(planName: string): Promise<PlanSettings> {
  return fetchJson<PlanSettings>(`/api/plans/${encodeURIComponent(planName)}/settings`);
}

export function putPlanSettings(planName: string, body: PlanSettingsBody): Promise<PlanSettings> {
  return putJson<PlanSettings>(`/api/plans/${encodeURIComponent(planName)}/settings`, body);
}

/// Mirrors the Rust `PhaseSettings` returned by GET
/// `/api/plans/:name/phases/:number/settings`. `phaseVerification` is
/// the phase-level override (`null` when unset → inherits from plan →
/// repo). `planVerification` and `repoVerification` are the inherited
/// values surfaced verbatim so the UI can show the chain without a
/// separate fetch.
export interface PhaseSettings {
  phaseNumber: number;
  phaseVerification: string | null;
  planVerification: string | null;
  repoVerification: string | null;
}

export interface PhaseSettingsBody {
  phaseVerification?: Tristate<string>;
}

export function getPhaseSettings(planName: string, phaseNumber: number): Promise<PhaseSettings> {
  return fetchJson<PhaseSettings>(
    `/api/plans/${encodeURIComponent(planName)}/phases/${phaseNumber}/settings`,
  );
}

export function putPhaseSettings(
  planName: string,
  phaseNumber: number,
  body: PhaseSettingsBody,
): Promise<PhaseSettings> {
  return putJson<PhaseSettings>(
    `/api/plans/${encodeURIComponent(planName)}/phases/${phaseNumber}/settings`,
    body,
  );
}

/// Resolve which level of the inheritance chain provides the active
/// verification command for a phase. Mirrors the server-side
/// `resolve_phase_verification` precedence used by the phase-check
/// listener in `agents::phase_check`. When the phase has its own
/// override, the source is `phase`; otherwise plan, then repo, then
/// none.
export type PhaseVerificationSource = "phase" | "plan" | "repo" | "none";

export function resolvePhaseVerification(s: PhaseSettings): {
  value: string | null;
  source: PhaseVerificationSource;
} {
  if (s.phaseVerification !== null) return { value: s.phaseVerification, source: "phase" };
  if (s.planVerification !== null) return { value: s.planVerification, source: "plan" };
  if (s.repoVerification !== null) return { value: s.repoVerification, source: "repo" };
  return { value: null, source: "none" };
}

/// Mirror of the server-side smart classifier
/// (`server-rs/src/ci/aggregate.rs::is_workflow_blocking_by_default`).
/// Returns `true` when a workflow is treated as blocking by default —
/// i.e. its name does NOT match `docker|deploy|publish|release|bench|fuzz`.
/// The two sides MUST stay in sync; the Rust regex is the source of truth.
export function isWorkflowBlockingByDefault(name: string): boolean {
  return !/docker|deploy|publish|release|bench|fuzz/i.test(name);
}

export type WorkflowSource = "plan" | "repo default" | "smart default";

export interface ResolvedWorkflow {
  name: string;
  blocking: boolean;
  source: WorkflowSource;
}

/// Apply the same precedence order as the server's
/// `resolve_blocking_workflows` for the workflows the panel renders:
/// plan-level explicit list → repo-default explicit list → smart
/// classifier. Phase-level overrides are not surfaced through this API
/// (the panel is plan-scoped) so they are not represented here.
export function resolveWorkflows(settings: PlanSettings): ResolvedWorkflow[] {
  const plan = settings.ciBlockingWorkflows;
  const repo = settings.repoDefaults.ciBlockingWorkflows;
  return settings.availableWorkflows.map((name) => {
    if (plan !== null && plan !== undefined) {
      return { name, blocking: plan.includes(name), source: "plan" as const };
    }
    if (repo !== undefined) {
      return { name, blocking: repo.includes(name), source: "repo default" as const };
    }
    return {
      name,
      blocking: isWorkflowBlockingByDefault(name),
      source: "smart default" as const,
    };
  });
}
