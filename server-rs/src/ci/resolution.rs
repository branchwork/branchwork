//! Resolution helpers for the per-plan CI workflow filter and the
//! phase-verification command.
//!
//! Phase 0.3 of the `branchwork-phase-verify-and-ci-filter` plan: pure
//! functions over already-parsed config (plan YAML, phase YAML, repo
//! `branchwork.toml`) that combine the four precedence layers Cyril
//! locked in the design.
//!
//! Both helpers are pure — they only read their inputs — so they're
//! exhaustively unit-testable without spinning up a server, a DB, or a
//! filesystem fixture. Consumption lives in phases 1 (CI aggregate
//! filter) and 2 (phase-end Check agent); this task just defines the
//! resolution rule.
//!
//! # `resolve_blocking_workflows`
//!
//! Returns the effective allowlist of workflow names that block
//! merges / auto-mode advancement. Precedence (first match wins):
//!
//! 1. `phase.ci_blocking_workflows` if `Some`
//! 2. `plan.ci_blocking_workflows` if `Some`
//! 3. `repo_config.ci.blocking_workflows` if `Some`
//! 4. Smart classifier — every discovered workflow whose name does
//!    NOT match `(?i)docker|deploy|publish|release|bench|fuzz`.
//!
//! The 4th-level classifier is applied to a caller-supplied list of
//! discovered workflow names (typically the workflow names attached to
//! the runs for the merged SHA). Keeping the discovery out of this
//! function is what makes it pure.
//!
//! `repo_config.ci.blocking_workflows_skip` is intentionally NOT
//! consulted here — its precedence relative to `blocking_workflows`
//! is owned by task 1.1's consumer (see [`crate::repo_config::CiConfig`]).
//!
//! # `resolve_phase_verification`
//!
//! Returns the phase-end verify command. Precedence:
//!
//! 1. `phase.phase_verification` if `Some`
//! 2. `plan.phase_verification` if `Some`
//! 3. `repo_config.phase.verification` if `Some`
//! 4. `None` (no smart fallback — verify is opt-in).

use std::sync::OnceLock;

use regex::Regex;

use crate::plan_parser::{ParsedPlan, PlanPhase};
use crate::repo_config::RepoConfig;

/// Compiled once per process. The pattern is case-insensitive and
/// unanchored — it matches anywhere in the workflow name, so
/// `Docker Publish`, `release-notes`, and `nightly-fuzz` are all
/// classified as non-blocking.
fn classifier_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)docker|deploy|publish|release|bench|fuzz").unwrap())
}

/// `true` when a workflow name should block by default — i.e. its name
/// does NOT match the deploy/release/bench/fuzz pattern. Public so that
/// later phases (UI surface, audit log) can show the same classification
/// the resolver uses.
#[allow(dead_code)] // consumed by phase 1.1+
pub fn is_workflow_blocking_by_default(name: &str) -> bool {
    !classifier_regex().is_match(name)
}

/// Resolve the effective blocking-workflows allowlist for a phase.
///
/// `discovered_workflows` is only consulted when none of the three
/// explicit precedence layers is `Some` — pass it from the workflow
/// names that came back from `gh` (or whatever the consumer's source
/// of truth is). When the explicit allowlist is set, this function
/// returns it verbatim without filtering against `discovered_workflows`,
/// because the user has explicitly opted into that exact list.
#[allow(dead_code)] // consumed by phase 1.1+
pub fn resolve_blocking_workflows(
    plan: &ParsedPlan,
    phase: &PlanPhase,
    repo_config: Option<&RepoConfig>,
    discovered_workflows: &[String],
) -> Vec<String> {
    if let Some(list) = phase.ci_blocking_workflows.as_ref() {
        return list.clone();
    }
    if let Some(list) = plan.ci_blocking_workflows.as_ref() {
        return list.clone();
    }
    if let Some(list) = repo_config.and_then(|c| c.ci.blocking_workflows.as_ref()) {
        return list.clone();
    }
    discovered_workflows
        .iter()
        .filter(|name| is_workflow_blocking_by_default(name))
        .cloned()
        .collect()
}

/// Resolve the effective phase-verification command. `None` means no
/// command is configured at any layer — phase 2's Check agent treats
/// that as a no-op.
#[allow(dead_code)] // consumed by phase 2.x+
pub fn resolve_phase_verification(
    plan: &ParsedPlan,
    phase: &PlanPhase,
    repo_config: Option<&RepoConfig>,
) -> Option<String> {
    if let Some(cmd) = phase.phase_verification.as_ref() {
        return Some(cmd.clone());
    }
    if let Some(cmd) = plan.phase_verification.as_ref() {
        return Some(cmd.clone());
    }
    repo_config.and_then(|c| c.phase.verification.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_parser::PlanTask;
    use crate::repo_config::{CiConfig, PhaseConfig, RepoConfig};

    // ── Fixtures ────────────────────────────────────────────────────────

    fn empty_plan() -> ParsedPlan {
        ParsedPlan {
            name: "p".into(),
            file_path: "p.yaml".into(),
            title: "Test plan".into(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![],
            verification: None,
            ci_blocking_workflows: None,
            phase_verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        }
    }

    fn empty_phase() -> PlanPhase {
        PlanPhase {
            number: 0,
            title: "Phase".into(),
            description: String::new(),
            tasks: Vec::<PlanTask>::new(),
            ci_blocking_workflows: None,
            phase_verification: None,
        }
    }

    fn discovered() -> Vec<String> {
        vec![
            "CI".into(),
            "lint".into(),
            "Docker".into(),
            "Docker Publish".into(),
            "deploy".into(),
            "release".into(),
            "bench".into(),
            "fuzz".into(),
        ]
    }

    // ── Classifier: positive + negative for each of the 6 suffixes ──────

    #[test]
    fn classifier_blocks_plain_ci_and_lint_names() {
        assert!(is_workflow_blocking_by_default("CI"));
        assert!(is_workflow_blocking_by_default("ci"));
        assert!(is_workflow_blocking_by_default("lint"));
        assert!(is_workflow_blocking_by_default("tests"));
        assert!(is_workflow_blocking_by_default("clippy"));
    }

    #[test]
    fn classifier_excludes_docker_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Docker"));
        assert!(!is_workflow_blocking_by_default("docker"));
        assert!(!is_workflow_blocking_by_default("Docker Publish"));
        assert!(!is_workflow_blocking_by_default("build-docker"));
        // negative — name does not contain the keyword.
        assert!(is_workflow_blocking_by_default("dock"));
        assert!(is_workflow_blocking_by_default("docke"));
    }

    #[test]
    fn classifier_excludes_deploy_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Deploy"));
        assert!(!is_workflow_blocking_by_default("deploy-prod"));
        assert!(!is_workflow_blocking_by_default("redeploy"));
        // negative — substring "epl" is not the keyword.
        assert!(is_workflow_blocking_by_default("dep"));
        assert!(is_workflow_blocking_by_default("eploy"));
    }

    #[test]
    fn classifier_excludes_publish_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Publish"));
        assert!(!is_workflow_blocking_by_default("publish-crate"));
        assert!(!is_workflow_blocking_by_default("auto-publish"));
        // negative — partial match.
        assert!(is_workflow_blocking_by_default("pub"));
        assert!(is_workflow_blocking_by_default("publis"));
    }

    #[test]
    fn classifier_excludes_release_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Release"));
        assert!(!is_workflow_blocking_by_default("release-notes"));
        assert!(!is_workflow_blocking_by_default("pre-release"));
        // negative — partial match.
        assert!(is_workflow_blocking_by_default("rel"));
        assert!(is_workflow_blocking_by_default("releas"));
    }

    #[test]
    fn classifier_excludes_bench_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Bench"));
        assert!(!is_workflow_blocking_by_default("bench-suite"));
        assert!(!is_workflow_blocking_by_default("nightly-bench"));
        // negative — partial match.
        assert!(is_workflow_blocking_by_default("ben"));
        assert!(is_workflow_blocking_by_default("benc"));
    }

    #[test]
    fn classifier_excludes_fuzz_case_insensitive() {
        assert!(!is_workflow_blocking_by_default("Fuzz"));
        assert!(!is_workflow_blocking_by_default("fuzz-cron"));
        assert!(!is_workflow_blocking_by_default("nightly-fuzz"));
        // negative — partial match.
        assert!(is_workflow_blocking_by_default("fuz"));
        assert!(is_workflow_blocking_by_default("uzz"));
    }

    // ── resolve_blocking_workflows: four precedence levels ──────────────

    #[test]
    fn level_1_phase_overrides_everything() {
        let mut plan = empty_plan();
        plan.ci_blocking_workflows = Some(vec!["plan-list".into()]);
        let mut phase = empty_phase();
        phase.ci_blocking_workflows = Some(vec!["phase-only".into()]);
        let repo = RepoConfig {
            ci: CiConfig {
                blocking_workflows: Some(vec!["repo-list".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let got = resolve_blocking_workflows(&plan, &phase, Some(&repo), &discovered());
        assert_eq!(got, vec!["phase-only".to_string()]);
    }

    #[test]
    fn level_2_plan_used_when_phase_is_none() {
        let mut plan = empty_plan();
        plan.ci_blocking_workflows = Some(vec!["plan-only".into()]);
        let phase = empty_phase();
        let repo = RepoConfig {
            ci: CiConfig {
                blocking_workflows: Some(vec!["repo-list".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let got = resolve_blocking_workflows(&plan, &phase, Some(&repo), &discovered());
        assert_eq!(got, vec!["plan-only".to_string()]);
    }

    #[test]
    fn level_3_repo_used_when_phase_and_plan_are_none() {
        let plan = empty_plan();
        let phase = empty_phase();
        let repo = RepoConfig {
            ci: CiConfig {
                blocking_workflows: Some(vec!["repo-only".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let got = resolve_blocking_workflows(&plan, &phase, Some(&repo), &discovered());
        assert_eq!(got, vec!["repo-only".to_string()]);
    }

    #[test]
    fn level_4_classifier_used_when_all_explicit_levels_unset() {
        let plan = empty_plan();
        let phase = empty_phase();
        // No repo config at all.
        let got = resolve_blocking_workflows(&plan, &phase, None, &discovered());
        // CI and lint are blocking; everything else matches the keyword regex.
        assert_eq!(got, vec!["CI".to_string(), "lint".to_string()]);
    }

    #[test]
    fn level_4_classifier_with_empty_repo_config_still_falls_through() {
        // RepoConfig::default() has ci.blocking_workflows == None,
        // which must not short-circuit level 3.
        let plan = empty_plan();
        let phase = empty_phase();
        let repo = RepoConfig::default();
        let got = resolve_blocking_workflows(&plan, &phase, Some(&repo), &discovered());
        assert_eq!(got, vec!["CI".to_string(), "lint".to_string()]);
    }

    #[test]
    fn level_4_classifier_with_empty_discovered_returns_empty() {
        let plan = empty_plan();
        let phase = empty_phase();
        let got = resolve_blocking_workflows(&plan, &phase, None, &[]);
        assert!(got.is_empty());
    }

    #[test]
    fn explicit_allowlist_is_returned_verbatim_even_when_classifier_disagrees() {
        // Level 1-3 returns the user's list as-is — we don't second-guess
        // them by re-applying the classifier or filtering against
        // discovered_workflows. Cyril explicitly asked for explicit config
        // over implicit auto-detection.
        let mut plan = empty_plan();
        plan.ci_blocking_workflows = Some(vec!["Docker".into(), "Deploy".into()]);
        let phase = empty_phase();
        let got = resolve_blocking_workflows(&plan, &phase, None, &discovered());
        assert_eq!(
            got,
            vec!["Docker".to_string(), "Deploy".to_string()],
            "explicit user allowlist must not be filtered by the smart classifier"
        );
    }

    // ── resolve_phase_verification: three precedence levels + None ──────

    #[test]
    fn verification_level_1_phase_overrides_everything() {
        let mut plan = empty_plan();
        plan.phase_verification = Some("plan-cmd".into());
        let mut phase = empty_phase();
        phase.phase_verification = Some("phase-cmd".into());
        let repo = RepoConfig {
            phase: PhaseConfig {
                verification: Some("repo-cmd".into()),
            },
            ..Default::default()
        };
        let got = resolve_phase_verification(&plan, &phase, Some(&repo));
        assert_eq!(got.as_deref(), Some("phase-cmd"));
    }

    #[test]
    fn verification_level_2_plan_used_when_phase_is_none() {
        let mut plan = empty_plan();
        plan.phase_verification = Some("plan-cmd".into());
        let phase = empty_phase();
        let repo = RepoConfig {
            phase: PhaseConfig {
                verification: Some("repo-cmd".into()),
            },
            ..Default::default()
        };
        let got = resolve_phase_verification(&plan, &phase, Some(&repo));
        assert_eq!(got.as_deref(), Some("plan-cmd"));
    }

    #[test]
    fn verification_level_3_repo_used_when_phase_and_plan_are_none() {
        let plan = empty_plan();
        let phase = empty_phase();
        let repo = RepoConfig {
            phase: PhaseConfig {
                verification: Some("repo-cmd".into()),
            },
            ..Default::default()
        };
        let got = resolve_phase_verification(&plan, &phase, Some(&repo));
        assert_eq!(got.as_deref(), Some("repo-cmd"));
    }

    #[test]
    fn verification_returns_none_when_nothing_set() {
        let plan = empty_plan();
        let phase = empty_phase();
        assert!(resolve_phase_verification(&plan, &phase, None).is_none());
        // Repo present but with no phase.verification.
        let repo = RepoConfig::default();
        assert!(resolve_phase_verification(&plan, &phase, Some(&repo)).is_none());
    }
}
