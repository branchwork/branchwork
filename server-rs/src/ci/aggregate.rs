//! Auto-mode CI aggregation rule.
//!
//! Both the standalone server path (`saas::dispatch::get_ci_run_status_dispatch`)
//! and the runner-side handler (`bin/branchwork_runner.rs::aggregate_runs`)
//! build a [`CiAggregate`] from a list of per-workflow runs for one merged
//! SHA. The shape of the rule — "any failure poisons every downstream skip"
//! — is the same in both modes; only the data source differs (local `gh`
//! shell-out vs runner-side `gh` shell-out). [`compute`] is that single
//! place — a pure function over already-built summaries — and
//! [`mark_upstream_skips`] is the helper that flips
//! [`CiRunSummary::skipped_due_to_upstream`] when the run set has any
//! failure.
//!
//! ## The Reglyze rule
//!
//! Multi-workflow CI where a downstream `deploy` workflow is `skipped`
//! because an upstream `tests` workflow failed must NOT register as a
//! success — the loop would advance to the next task on a red CI. The rule
//! compresses precise `needs:` / `workflow_run` dependency tracking (which
//! `gh` doesn't expose cleanly) into the conservative "skip-when-set-failed":
//! if any run failed, every skipped run in the same SHA is treated as
//! upstream-poisoned.
//!
//! See `aggregate_runs_reglyze_fixture_*` in this module's tests for the
//! canonical regression test.

use crate::saas::runner_protocol::{CiAggregate, CiRunSummary};

/// Apply the auto-mode CI aggregation rule to a slice of per-run summaries.
///
/// The summaries are expected to have [`CiRunSummary::skipped_due_to_upstream`]
/// already set by the caller (typically via [`mark_upstream_skips`]) and to be
/// in workflow-graph order — the runner sorts by `gh`'s `createdAt` ascending
/// before calling, so the standalone path should do the same so that
/// `failing_run_id` resolves to the root-cause failure rather than a
/// downstream one.
///
/// Rules:
/// - If any run has `conclusion in {failure, cancelled, timed_out}` ⇒
///   aggregate `conclusion = "failure"`.
/// - If every run has `status == "completed"` ⇒ aggregate `status =
///   "completed"`; otherwise `"in_progress"` (still polling).
/// - If every run is `success` or `skipped` AND nothing failed ⇒ aggregate
///   `conclusion = "success"`. Note: when no failure exists in the set,
///   `skipped_due_to_upstream` is never set on any run, so this branch is
///   reached only when downstream skips are intentional.
/// - `failing_run_id` is the first failing run by input order (callers sort
///   by `createdAt` upstream).
pub fn compute(runs: &[CiRunSummary]) -> CiAggregate {
    let any_failing = runs.iter().any(is_terminal_failure);

    let all_completed = runs.iter().all(|s| s.status == "completed");
    let agg_status = if all_completed {
        "completed".to_string()
    } else {
        "in_progress".to_string()
    };

    let agg_conclusion = if any_failing {
        Some("failure".to_string())
    } else if all_completed
        && runs
            .iter()
            .all(|s| matches!(s.conclusion.as_deref(), Some("success") | Some("skipped")))
    {
        Some("success".to_string())
    } else {
        None
    };

    let failing_run_id = runs
        .iter()
        .find(|r| is_terminal_failure(r))
        .map(|r| r.run_id.clone());

    CiAggregate {
        status: agg_status,
        conclusion: agg_conclusion,
        runs: runs.to_vec(),
        failing_run_id,
    }
}

/// Set `skipped_due_to_upstream = true` on every run with `conclusion =
/// "skipped"` when the input has any failing run. This collapses precise
/// `needs:` / `workflow_run` skip detection (which `gh` doesn't expose
/// cleanly) into the conservative "skip-when-set-failed" — the bug we're
/// guarding against is the loop reading a downstream `deploy: skipped` as
/// success while `tests: failure` is in the same run set; either heuristic
/// catches that case.
///
/// No-op when the input has no failing run (every skipped run there is
/// intentional).
pub fn mark_upstream_skips(runs: &mut [CiRunSummary]) {
    let any_failing = runs.iter().any(is_terminal_failure);
    if !any_failing {
        return;
    }
    for run in runs.iter_mut() {
        if run.conclusion.as_deref() == Some("skipped") {
            run.skipped_due_to_upstream = true;
        }
    }
}

fn is_terminal_failure(s: &CiRunSummary) -> bool {
    matches!(
        s.conclusion.as_deref(),
        Some("failure") | Some("cancelled") | Some("timed_out")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three workflow runs for one merged SHA: `tests` failed, `lint`
    /// passed, `deploy` was skipped because `tests` failed upstream. This
    /// is the bug Reglyze hit in production — the original aggregator
    /// treated `deploy: skipped` as "intentional skip" and the loop
    /// advanced on a red CI. Inputs are pre-sorted by `gh`'s `createdAt`
    /// (tests first), matching the order both the runner and the
    /// standalone path produce before calling [`compute`].
    fn reglyze_summaries() -> Vec<CiRunSummary> {
        vec![
            CiRunSummary {
                run_id: "100".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "101".into(),
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "102".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
            },
        ]
    }

    /// Reglyze regression: `mark_upstream_skips` flips deploy's flag, and
    /// `compute` returns aggregate=failure with `failing_run_id = "100"`
    /// (the failing `tests` run, NOT the skipped `deploy`).
    #[test]
    fn reglyze_fixture_marks_skipped_due_to_upstream_and_picks_failing_run() {
        let mut runs = reglyze_summaries();
        mark_upstream_skips(&mut runs);

        // mark_upstream_skips sets the flag on `deploy` only — `lint` was
        // success, `tests` is the failure itself.
        let by_workflow = runs
            .iter()
            .map(|s| (s.workflow_name.as_str(), s))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(!by_workflow["tests"].skipped_due_to_upstream);
        assert!(!by_workflow["lint"].skipped_due_to_upstream);
        assert!(
            by_workflow["deploy"].skipped_due_to_upstream,
            "deploy must be marked skipped_due_to_upstream when tests failed in the same SHA"
        );

        let aggregate = compute(&runs);
        assert_eq!(aggregate.status, "completed");
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(
            aggregate.failing_run_id.as_deref(),
            Some("100"),
            "failing_run_id must point at `tests` (the root-cause failure), not `deploy`"
        );
    }

    #[test]
    fn all_success_reports_success_and_no_failing_run() {
        let runs = vec![
            CiRunSummary {
                run_id: "200".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "201".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
            },
        ];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.status, "completed");
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
        assert!(aggregate.failing_run_id.is_none());
    }

    #[test]
    fn in_progress_when_any_run_pending() {
        let runs = vec![
            CiRunSummary {
                run_id: "300".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "301".into(),
                workflow_name: "deploy".into(),
                status: "in_progress".into(),
                conclusion: None,
                skipped_due_to_upstream: false,
            },
        ];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.status, "in_progress");
        assert!(aggregate.conclusion.is_none());
    }

    #[test]
    fn cancelled_counts_as_failure() {
        let runs = vec![CiRunSummary {
            run_id: "400".into(),
            workflow_name: "tests".into(),
            status: "completed".into(),
            conclusion: Some("cancelled".into()),
            skipped_due_to_upstream: false,
        }];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("400"));
    }

    #[test]
    fn timed_out_counts_as_failure() {
        let runs = vec![CiRunSummary {
            run_id: "401".into(),
            workflow_name: "tests".into(),
            status: "completed".into(),
            conclusion: Some("timed_out".into()),
            skipped_due_to_upstream: false,
        }];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("401"));
    }

    #[test]
    fn failing_run_id_picks_first_in_input_order() {
        // Two failing runs; failing_run_id must be the first by input order
        // (caller's responsibility to sort by createdAt before this).
        let runs = vec![
            CiRunSummary {
                run_id: "earliest".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "later".into(),
                workflow_name: "integration".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            },
        ];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("earliest"));
    }

    #[test]
    fn empty_runs_yields_completed_no_conclusion() {
        // Edge case: gh returned an empty list but the caller still asked
        // for an aggregate. all_completed vacuously true, no failing,
        // no all-success branch (vacuous true on the success rule means
        // success — that's actually fine: no runs to fail means nothing
        // is breaking).
        let aggregate = compute(&[]);
        assert_eq!(aggregate.status, "completed");
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
        assert!(aggregate.failing_run_id.is_none());
    }

    #[test]
    fn mark_upstream_skips_no_op_when_nothing_failing() {
        let mut runs = vec![
            CiRunSummary {
                run_id: "500".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "501".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
            },
        ];
        mark_upstream_skips(&mut runs);
        // No failure ⇒ skipped is intentional, not upstream-poisoned.
        assert!(!runs.iter().any(|r| r.skipped_due_to_upstream));
    }

    #[test]
    fn upstream_skip_does_not_mask_failure_in_conclusion() {
        // Even with skipped_due_to_upstream pre-set on the input (the
        // caller already ran mark_upstream_skips), compute must still
        // return failure when any run failed — the rule does not silently
        // swallow upstream skips.
        let runs = vec![
            CiRunSummary {
                run_id: "600".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
            },
            CiRunSummary {
                run_id: "601".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: true,
            },
        ];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("600"));
    }
}
