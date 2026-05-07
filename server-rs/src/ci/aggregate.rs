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
//!
//! ## Per-plan blocking-workflow filter (Phase 1.1)
//!
//! [`compute_with_filter`] takes the same per-run input plus an optional
//! [`BlockingFilter`] (the resolved allowlist + the source — explicit
//! config or smart classifier — that produced it). When present, the
//! aggregator partitions runs into "blocking" and "informational":
//!
//! - **Blocking** runs go through the existing failure / success rules.
//! - **Informational** runs (those whose `workflow_name` is *not* in the
//!   allowlist) are still surfaced in the per-run breakdown via
//!   [`CiRunSummary::informational`] = `true`, but they never affect
//!   `conclusion`, `failing_run_id`, or `status`. A failing nightly-bench
//!   on a green tests + lint run set therefore reports `success`.
//!
//! Every per-run filter decision logs at INFO via `eprintln!` with the
//! run id, workflow name, classification (`blocking` vs `informational`),
//! and source tag (`via-config` or `via-classifier`) so a post-mortem can
//! reconstruct exactly why a run was excluded.

use crate::saas::runner_protocol::{CiAggregate, CiRunSummary};

/// How the caller derived the blocking-workflow allowlist. The value is
/// load-bearing only for audit-log purposes — the aggregator does not
/// consult it for the actual filtering rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed once Phase 1.x wires resolve_blocking_workflows into the dispatcher
pub enum BlockingSource {
    /// User explicitly opted into the allowlist via phase-, plan-, or
    /// repo-level configuration. Audit log shows `via-config`.
    Config,
    /// Smart classifier supplied the allowlist as a default fallback
    /// (every discovered workflow whose name does NOT match the
    /// `(?i)docker|deploy|publish|release|bench|fuzz` pattern). Audit
    /// log shows `via-classifier`.
    Classifier,
}

impl BlockingSource {
    fn label(self) -> &'static str {
        match self {
            BlockingSource::Config => "via-config",
            BlockingSource::Classifier => "via-classifier",
        }
    }
}

/// Effective per-plan filter: the resolved allowlist plus the source tag
/// that produced it. Constructed by the caller from the resolution helper
/// in [`crate::ci::resolution`] (or the equivalent runner-side hook).
///
/// `'a` borrows the allowlist so callers don't need to clone the
/// resolution-helper return value just to call into the aggregator.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // consumed once Phase 1.x wires resolve_blocking_workflows into the dispatcher
pub struct BlockingFilter<'a> {
    /// Workflow names that block merge / auto-mode advancement. Matched
    /// case-sensitively against [`CiRunSummary::workflow_name`]; the
    /// resolver in [`crate::ci::resolution`] owns case normalisation
    /// upstream so no normalisation happens here.
    pub allowlist: &'a [String],
    /// Where the allowlist came from. Logged per-run so audit trails
    /// can distinguish a user-pinned filter from a classifier fallback.
    pub source: BlockingSource,
}

#[allow(dead_code)] // consumed once Phase 1.x wires resolve_blocking_workflows into the dispatcher
impl<'a> BlockingFilter<'a> {
    /// Allowlist resolved from explicit user configuration (phase, plan,
    /// or repo TOML). Logged as `via-config`.
    pub fn config(allowlist: &'a [String]) -> Self {
        Self {
            allowlist,
            source: BlockingSource::Config,
        }
    }

    /// Allowlist supplied by the smart classifier fallback. Logged as
    /// `via-classifier`.
    pub fn classifier(allowlist: &'a [String]) -> Self {
        Self {
            allowlist,
            source: BlockingSource::Classifier,
        }
    }
}

/// Apply the auto-mode CI aggregation rule to a slice of per-run summaries.
///
/// Equivalent to [`compute_with_filter(runs, None)`](compute_with_filter):
/// every run is treated as blocking, no `informational` flag is set, and
/// no INFO-level filter logs are emitted. This is the historical entry
/// point — use [`compute_with_filter`] when a per-plan / per-phase /
/// repo-level filter is in play.
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
    compute_with_filter(runs, None)
}

/// Filtered variant of [`compute`]. When `filter` is `Some`, partitions
/// runs into the blocking subset (workflows whose name is in
/// [`BlockingFilter::allowlist`]) and the informational subset (everyone
/// else). All aggregation — `status`, `conclusion`, `failing_run_id` — is
/// computed over the blocking subset only. Informational runs are still
/// returned in `CiAggregate::runs` with [`CiRunSummary::informational`]
/// set to `true`, so the dashboard can render them without having to
/// know the filter rule itself.
///
/// When `filter` is `None`, behaves identically to [`compute`].
///
/// Empty allowlist (`Some(BlockingFilter { allowlist: &[], .. })`) is a
/// valid input: every run becomes informational, and the aggregate is
/// `status="completed"` + `conclusion="success"` (the rules apply
/// vacuously over the empty blocking subset). This matches the
/// "informational never affects conclusion" contract — if nothing
/// blocks, nothing blocks.
///
/// Logs every per-run classification at INFO via `eprintln!`:
/// `[ci-aggregate] run <id> (<workflow>): blocking via-config`
/// `[ci-aggregate] run <id> (<workflow>): informational via-classifier`
#[allow(dead_code)] // consumed once Phase 1.x wires the dispatcher
pub fn compute_with_filter(
    runs: &[CiRunSummary],
    filter: Option<&BlockingFilter<'_>>,
) -> CiAggregate {
    let Some(filter) = filter else {
        // No filter: short-circuit through the unfiltered rule. Avoids
        // walking the allowlist per run and keeps the no-filter path
        // log-quiet (it has always been silent).
        return aggregate_unfiltered(runs);
    };

    let source_label = filter.source.label();

    // Build a partitioned copy of `runs` with `informational` set on
    // every workflow whose name is NOT in the allowlist. We clone here
    // (rather than holding two `&CiRunSummary` slices) because the
    // returned `CiAggregate::runs` needs to embed the partition decision
    // — the dashboard reads `informational` directly off each summary.
    let labelled: Vec<CiRunSummary> = runs
        .iter()
        .map(|r| {
            let blocking = filter.allowlist.iter().any(|name| name == &r.workflow_name);
            let class_label = if blocking {
                "blocking"
            } else {
                "informational"
            };
            eprintln!(
                "[ci-aggregate] run {run_id} ({workflow}): {class_label} {source_label}",
                run_id = r.run_id,
                workflow = r.workflow_name,
            );
            CiRunSummary {
                informational: !blocking,
                ..r.clone()
            }
        })
        .collect();

    // Apply the existing failure/success rules over the blocking subset
    // only. `informational` runs are passed through into `runs` but
    // never consulted for `conclusion`, `failing_run_id`, or `status`.
    let blocking_runs: Vec<&CiRunSummary> = labelled.iter().filter(|r| !r.informational).collect();

    let any_failing = blocking_runs.iter().any(|r| is_terminal_failure(r));

    let all_completed = blocking_runs.iter().all(|s| s.status == "completed");
    let agg_status = if all_completed {
        "completed".to_string()
    } else {
        "in_progress".to_string()
    };

    let agg_conclusion = if any_failing {
        Some("failure".to_string())
    } else if all_completed
        && blocking_runs
            .iter()
            .all(|s| matches!(s.conclusion.as_deref(), Some("success") | Some("skipped")))
    {
        Some("success".to_string())
    } else {
        None
    };

    let failing_run_id = blocking_runs
        .iter()
        .find(|r| is_terminal_failure(r))
        .map(|r| r.run_id.clone());

    CiAggregate {
        status: agg_status,
        conclusion: agg_conclusion,
        runs: labelled,
        failing_run_id,
    }
}

/// Unfiltered aggregator — every run is blocking. Lifted out of [`compute`]
/// so [`compute_with_filter`] with `None` and [`compute`] share one body.
fn aggregate_unfiltered(runs: &[CiRunSummary]) -> CiAggregate {
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
                informational: false,
            },
            CiRunSummary {
                run_id: "101".into(),
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "102".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
                informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "201".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
                informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "301".into(),
                workflow_name: "deploy".into(),
                status: "in_progress".into(),
                conclusion: None,
                skipped_due_to_upstream: false,
                informational: false,
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
            informational: false,
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
            informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "later".into(),
                workflow_name: "integration".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
                informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "501".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: false,
                informational: false,
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
                informational: false,
            },
            CiRunSummary {
                run_id: "601".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                skipped_due_to_upstream: true,
                informational: false,
            },
        ];
        let aggregate = compute(&runs);
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("600"));
    }

    // ── Phase 1.1: per-plan blocking-workflow filter ────────────────────

    /// Parity gate: passing `None` to `compute_with_filter` must produce
    /// byte-identical output to the historical [`compute`] entry point.
    /// Pins acceptance criterion (a) — "Existing tests still pass with
    /// no allowlist (None preserves current behavior)".
    #[test]
    fn compute_with_filter_none_matches_compute_for_reglyze_fixture() {
        let mut runs = reglyze_summaries();
        mark_upstream_skips(&mut runs);

        let baseline = compute(&runs);
        let filtered = compute_with_filter(&runs, None);

        assert_eq!(filtered, baseline);
        assert!(
            filtered.runs.iter().all(|r| !r.informational),
            "no filter ⇒ no run should be marked informational"
        );
    }

    /// Pins acceptance criterion (b) — "every blocking run is success
    /// and a non-blocking run failed → aggregate is success". This is
    /// the headline Phase 1.1 behaviour: a deploy/release/bench failure
    /// must NOT poison a green tests/lint run set when those workflows
    /// are explicitly outside the blocking allowlist.
    #[test]
    fn non_blocking_failure_does_not_affect_conclusion() {
        let runs = vec![
            CiRunSummary {
                run_id: "700".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "701".into(),
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "702".into(),
                workflow_name: "nightly-bench".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
        ];
        let allowlist = vec!["tests".to_string(), "lint".to_string()];
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(aggregate.status, "completed");
        assert_eq!(
            aggregate.conclusion.as_deref(),
            Some("success"),
            "non-blocking failure must not flip aggregate to failure"
        );
        assert!(
            aggregate.failing_run_id.is_none(),
            "failing_run_id must be None — the only failing run is informational"
        );

        // The informational marker is observable on the run itself so
        // the dashboard can render the bench failure as a non-gating
        // signal without re-running the partition rule.
        let by_workflow = aggregate
            .runs
            .iter()
            .map(|r| (r.workflow_name.as_str(), r))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(!by_workflow["tests"].informational);
        assert!(!by_workflow["lint"].informational);
        assert!(
            by_workflow["nightly-bench"].informational,
            "workflow outside the allowlist must be flagged informational"
        );
    }

    /// Inverse of the above: a blocking failure must still poison the
    /// aggregate even when an informational run is happily green.
    #[test]
    fn blocking_failure_poisons_aggregate_even_when_informational_runs_are_green() {
        let runs = vec![
            CiRunSummary {
                run_id: "800".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "801".into(),
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
        ];
        let allowlist = vec!["tests".to_string()];
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("800"));
    }

    /// In-progress non-blocking runs must not delay the aggregate — once
    /// every blocking run is `completed`, the gate is open even if
    /// nightly-bench is still running. Mirror image of the criterion (b)
    /// case but for `status` instead of `conclusion`.
    #[test]
    fn non_blocking_in_progress_does_not_delay_status() {
        let runs = vec![
            CiRunSummary {
                run_id: "900".into(),
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                skipped_due_to_upstream: false,
                informational: false,
            },
            CiRunSummary {
                run_id: "901".into(),
                workflow_name: "deploy".into(),
                status: "in_progress".into(),
                conclusion: None,
                skipped_due_to_upstream: false,
                informational: false,
            },
        ];
        let allowlist = vec!["tests".to_string()];
        let filter = BlockingFilter::classifier(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(
            aggregate.status, "completed",
            "in-progress informational deploy must not hold the gate"
        );
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
    }

    /// Empty allowlist is a valid input — every run becomes informational
    /// and the aggregate vacuously succeeds. This is the "filter says
    /// nothing blocks ⇒ nothing blocks" semantic.
    #[test]
    fn empty_allowlist_makes_every_run_informational() {
        let runs = reglyze_summaries();
        // Without the empty allowlist, this fixture aggregates to failure.
        let allowlist: Vec<String> = vec![];
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(aggregate.status, "completed");
        assert_eq!(
            aggregate.conclusion.as_deref(),
            Some("success"),
            "empty allowlist ⇒ no blocking runs ⇒ vacuous success"
        );
        assert!(aggregate.failing_run_id.is_none());
        assert!(
            aggregate.runs.iter().all(|r| r.informational),
            "every run must be flagged informational"
        );
    }

    /// Allowlist entries that don't appear in the run set are silently
    /// ignored — discovery is the caller's job. The aggregator only
    /// classifies the runs it sees.
    #[test]
    fn allowlist_entries_not_in_run_set_are_silently_ignored() {
        let runs = vec![CiRunSummary {
            run_id: "1000".into(),
            workflow_name: "tests".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            skipped_due_to_upstream: false,
            informational: false,
        }];
        let allowlist = vec![
            "tests".to_string(),
            "workflow-that-was-removed-yesterday".to_string(),
        ];
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(aggregate.runs.len(), 1);
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
    }

    /// Workflow-name matching is case-sensitive by design — the resolver
    /// in `ci::resolution` owns case normalisation upstream so the
    /// aggregator stays a pure equality check.
    #[test]
    fn workflow_name_matching_is_case_sensitive() {
        let runs = vec![CiRunSummary {
            run_id: "1100".into(),
            workflow_name: "Tests".into(),
            status: "completed".into(),
            conclusion: Some("failure".into()),
            skipped_due_to_upstream: false,
            informational: false,
        }];
        let allowlist = vec!["tests".to_string()]; // lowercase ≠ "Tests"
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(
            aggregate.conclusion.as_deref(),
            Some("success"),
            "case mismatch ⇒ run treated as informational ⇒ no failure"
        );
        assert!(aggregate.runs[0].informational);
    }

    /// `BlockingSource::label()` round-trip — the audit-log strings are
    /// load-bearing for downstream log parsers, so pin them here.
    #[test]
    fn blocking_source_labels_match_audit_contract() {
        assert_eq!(BlockingSource::Config.label(), "via-config");
        assert_eq!(BlockingSource::Classifier.label(), "via-classifier");
    }

    /// Reglyze-with-filter: when only `tests` is blocking, the original
    /// failure still wins because `tests` itself failed. Pins the
    /// "filter doesn't accidentally hide a real blocking failure" path
    /// in case a future refactor swaps the filter and aggregation order.
    #[test]
    fn reglyze_fixture_with_tests_only_in_allowlist_still_reports_failure() {
        let mut runs = reglyze_summaries();
        mark_upstream_skips(&mut runs);

        let allowlist = vec!["tests".to_string()];
        let filter = BlockingFilter::config(&allowlist);
        let aggregate = compute_with_filter(&runs, Some(&filter));

        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("100"));
        // lint and deploy are now informational — they shouldn't
        // disappear from the run list, just be flagged as such.
        let by_workflow = aggregate
            .runs
            .iter()
            .map(|r| (r.workflow_name.as_str(), r))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(!by_workflow["tests"].informational);
        assert!(by_workflow["lint"].informational);
        assert!(by_workflow["deploy"].informational);
    }
}
