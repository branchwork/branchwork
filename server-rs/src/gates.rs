//! Gate execution engine for DAG plans (Phase 3 of the DAG-based plan
//! model).
//!
//! A [`crate::dag::GateNode`] is non-agent verification that sits between
//! task nodes in the graph. [`execute_gate`] dispatches on
//! [`crate::dag::GateKind`] and returns a [`GateOutcome`] the DAG scheduler
//! ([`crate::dag_scheduler`]) acts on:
//!
//! - [`GateOutcome::Passed`] — every declared check succeeded. The
//!   scheduler marks the node `completed` and advances downstream.
//! - [`GateOutcome::Failed`] — a check failed (carries a human reason). The
//!   scheduler marks the node `failed`; the plan stalls at the gate until
//!   the underlying condition is fixed and the gate re-runs.
//! - [`GateOutcome::Blocked`] — the gate is waiting on an *external* event:
//!   a human approval (`init` / `approval` gates) or CI still in flight
//!   (`ci` / `end` gates whose `ci_green` check has not settled). The
//!   scheduler does **not** re-poll a blocked gate; completion arrives via
//!   the approval API (a `gate_approvals` row) or the CI poller observing
//!   the workflow finish.
//!
//! Per-kind semantics:
//! - **Init**: run shell preconditions (`git_repo`, `remote_configured`,
//!   `clean_tree`) in the project dir. All pass ⇒ broadcast
//!   `gate_ready_for_approval` (with the verified `checks`) **and**
//!   `gate_awaiting_approval` (the uniform approve signal), then return
//!   `Blocked` (waiting for approval); any fail ⇒ `Failed`. A pre-existing
//!   `gate_approvals` row short-circuits to `Passed`.
//! - **End**: `all_merged` (no agent for the plan still carries an unmerged
//!   branch), `compiles` (reuses [`crate::auto_mode::run_pre_merge_checks_in`]
//!   over the project's merged tree from `branchwork.toml`), and `ci_green`
//!   (polls GitHub Actions on the project HEAD).
//! - **Ci**: reuses [`crate::ci`] aggregation for the declared `workflows`
//!   on the project HEAD.
//! - **Approval**: broadcast `gate_awaiting_approval` and return `Blocked`
//!   until a `gate_approvals` row appears.
//!
//! SaaS caveat: the project working tree lives on the *runner* in SaaS
//! mode, so the shell-based checks (`git_repo` / `remote_configured` /
//! `clean_tree` / `compiles`) and the local `.github/workflows` probe see
//! nothing on the server. The CI status poll itself is mode-aware (it
//! dispatches to the runner), but the file/git probes are standalone-first;
//! wiring them through the runner is a follow-up.

use std::path::Path;
use std::time::Duration;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dag::{DagNode, GateKind};
use crate::db::Db;
use crate::state::AppState;
use crate::ws::broadcast_event;

/// Outcome of executing a gate node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Every check passed — the scheduler completes the node and advances.
    Passed,
    /// A check failed (human-readable reason) — the scheduler fails the node.
    Failed(String),
    /// Waiting on an external event (approval / CI). The scheduler leaves
    /// the node un-completed and does not re-poll it.
    Blocked(String),
}

/// One End-gate check's structured result (Task 3.6: End gate dashboard
/// rendering). The End gate runs each declared check (`all_merged`,
/// `compiles`, `ci_green`) and records one of these per check, persists the
/// set to the `gate_checks` table, and broadcasts a `gate_check_results` WS
/// event. `GET /api/plans/:name` attaches the stored set to the gate node as
/// `gateChecks` so the dashboard renders the per-check verdicts inline on the
/// GateCard (and the view survives a reload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateCheckResult {
    /// Check name: `all_merged` | `compiles` | `ci_green` (or a custom name
    /// declared on the node).
    pub name: String,
    /// `passed` | `failed` | `blocked` | `skipped`. `blocked` ≡ the check is
    /// waiting on an external event (CI still in flight); `skipped` ≡ the
    /// check was not run (no config, or an earlier check already failed).
    pub status: String,
    /// Human one-liner for the row — e.g. `"3/3 branches merged"`,
    /// `"check 'build' failed (exit 7)"`, `"CI green"`.
    pub detail: String,
    /// Captured output snippet for a failing check (the `compiles` build log).
    /// Bounded; the dashboard renders it in a collapsible `<details>` block
    /// (the PreMergeCheckFailedBanner pattern).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Link to the relevant CI run (the `ci_green` check) when derivable from
    /// the project's `origin` remote; `None` in SaaS mode or for non-github
    /// remotes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl GateCheckResult {
    fn new(name: &str, status: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: status.to_string(),
            detail: detail.into(),
            output: None,
            url: None,
        }
    }
}

/// Cap on the persisted `compiles` output snippet. Mirrors the audit-snippet
/// cap the pre-merge gate uses so the stored JSON, the WS frame, and the
/// dashboard banner all stay bounded.
const COMPILE_OUTPUT_SNIPPET_CAP_BYTES: usize = 4 * 1024;

/// Default checks per gate kind when a node declares none explicitly.
/// `parsed_plan_to_dag` always populates these for synthetic gates, but a
/// hand-written v2 plan may omit them.
fn default_init_checks() -> &'static [&'static str] {
    &["git_repo", "remote_configured", "clean_tree"]
}
fn default_end_checks() -> &'static [&'static str] {
    &["all_merged", "compiles", "ci_green"]
}

/// Execute a ready gate node and report what the scheduler should do.
///
/// Pure dispatch on `node.gate_kind`. `plan_name` resolves the project dir,
/// the org (for the CI dispatch), and the plan's agents (for `all_merged`).
/// `scoped_id` is the node's scoped id (`end` at the top level,
/// `parent.end` inside a sub-plan) — the key the scheduler / approval API /
/// `gate_checks` all use, so approval lookups and persisted check results
/// match across nested plans. `node` supplies the gate kind, the declared
/// `checks`, and (for CI gates) the declared `workflows`. Side effects are
/// limited to broadcasting the `gate_ready_for_approval` /
/// `gate_awaiting_approval` / `gate_check_results` WS events and persisting
/// the End gate's `gate_checks` — node status writes are the scheduler's job.
pub async fn execute_gate(
    state: &AppState,
    plan_name: &str,
    scoped_id: &str,
    node: &DagNode,
) -> GateOutcome {
    match node.gate_kind {
        Some(GateKind::Init) => execute_init_gate(state, plan_name, scoped_id, node).await,
        Some(GateKind::End) => execute_end_gate(state, plan_name, scoped_id, node).await,
        Some(GateKind::Ci) => execute_ci_gate(state, plan_name, node).await,
        Some(GateKind::Approval) => execute_approval_gate(state, plan_name, scoped_id, node).await,
        None => GateOutcome::Failed(format!("gate node '{}' has no gate_kind declared", node.id)),
    }
}

// ── Init gate ────────────────────────────────────────────────────────────

async fn execute_init_gate(
    state: &AppState,
    plan_name: &str,
    scoped_id: &str,
    node: &DagNode,
) -> GateOutcome {
    // Pre-approved out of band (e.g. operator approved before the gate was
    // first reached) ⇒ pass straight through.
    if gate_approved(&state.db, plan_name, scoped_id) {
        return GateOutcome::Passed;
    }

    // Cross-plan inputs gate (Phase 4): before running this plan's own
    // preconditions, every declared `inputs:` must be satisfied by the
    // producing plan's recorded output. Until then the gate stays `Blocked`
    // with `waiting for <plan>/<artifact>`; once the producing plan's End gate
    // records the output it broadcasts `plan_output_produced`, and the
    // cross-plan listener re-advances us (see
    // `crate::artifacts::handle_plan_output_produced`).
    let inputs = plan_inputs(&state.plans_dir, plan_name);
    if !crate::artifacts::check_inputs_satisfied(&state.db, plan_name, &inputs) {
        let reason = crate::artifacts::first_unsatisfied_input(&state.db, &inputs)
            .map(|(p, a)| format!("waiting for {p}/{a}"))
            .unwrap_or_else(|| "waiting for upstream plan outputs".to_string());
        return GateOutcome::Blocked(reason);
    }

    let Some(project_dir) = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name)
    else {
        return GateOutcome::Failed(
            "cannot resolve project directory to verify preconditions".to_string(),
        );
    };

    let checks: Vec<String> = if node.checks.is_empty() {
        default_init_checks()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        node.checks.clone()
    };

    for check in &checks {
        if let GateOutcome::Failed(reason) = run_init_check(check, &project_dir) {
            return GateOutcome::Failed(reason);
        }
    }

    // All preconditions hold — the gate is ready for a human to approve.
    // `gate_ready_for_approval` carries the verified `checks` detail; the
    // companion `gate_awaiting_approval` is the uniform "a human must approve
    // this gate" signal both init and approval gates emit so the dashboard
    // can drive a single Approve affordance off one event type.
    broadcast_event(
        &state.broadcast_tx,
        "gate_ready_for_approval",
        json!({
            "plan_name": plan_name,
            "node_id": scoped_id,
            "gate_kind": GateKind::Init.as_str(),
            "title": node.title,
            "checks": checks,
        }),
    );
    broadcast_event(
        &state.broadcast_tx,
        "gate_awaiting_approval",
        json!({
            "plan_name": plan_name,
            "node_id": scoped_id,
            "title": node.title,
            "gate_kind": GateKind::Init.as_str(),
        }),
    );
    GateOutcome::Blocked("preconditions passed — awaiting approval".to_string())
}

/// Run one declared init precondition in the project dir. Returns `Passed`
/// on success (or for an unknown check name, which is skipped with a log so
/// the engine stays forward-compatible) and `Failed(reason)` otherwise.
fn run_init_check(name: &str, cwd: &Path) -> GateOutcome {
    match name {
        "git_repo" => match git_stdout(cwd, &["rev-parse", "--is-inside-work-tree"]) {
            Some((true, out)) if out.trim() == "true" => GateOutcome::Passed,
            _ => GateOutcome::Failed(format!(
                "git_repo: {} is not inside a git work tree",
                cwd.display()
            )),
        },
        "remote_configured" => match git_stdout(cwd, &["remote"]) {
            Some((true, out)) if !out.trim().is_empty() => GateOutcome::Passed,
            _ => GateOutcome::Failed("remote_configured: no git remote configured".to_string()),
        },
        "clean_tree" => match git_stdout(cwd, &["status", "--porcelain", "--untracked-files=no"]) {
            Some((true, out)) if out.trim().is_empty() => GateOutcome::Passed,
            Some((true, out)) => GateOutcome::Failed(format!(
                "clean_tree: working tree has uncommitted changes:\n{}",
                truncate(out.trim(), 1024)
            )),
            _ => GateOutcome::Failed("clean_tree: could not read git status".to_string()),
        },
        other => {
            eprintln!("[gates] init gate: unknown check '{other}' — skipping");
            GateOutcome::Passed
        }
    }
}

// ── End gate ─────────────────────────────────────────────────────────────

async fn execute_end_gate(
    state: &AppState,
    plan_name: &str,
    scoped_id: &str,
    node: &DagNode,
) -> GateOutcome {
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name);

    let checks: Vec<String> = if node.checks.is_empty() {
        default_end_checks().iter().map(|s| s.to_string()).collect()
    } else {
        node.checks.clone()
    };

    // Run checks in order, recording a structured result for every one so the
    // dashboard can show all three verdicts inline. A `Failed` short-circuits
    // the *expensive* downstream checks (we don't run cargo build / poll CI
    // once an earlier check has already doomed the gate) — those are recorded
    // as `skipped`. A `Blocked` (CI still in flight) does not short-circuit;
    // it's remembered and surfaced after the rest. The overall outcome mirrors
    // the original semantics: any failure ⇒ Failed; else any block ⇒ Blocked;
    // else Passed.
    let mut results: Vec<GateCheckResult> = Vec::with_capacity(checks.len());
    let mut failed_reason: Option<String> = None;
    let mut blocked_reason: Option<String> = None;

    for check in &checks {
        if failed_reason.is_some() {
            results.push(GateCheckResult::new(
                check,
                "skipped",
                "not run — an earlier check failed",
            ));
            continue;
        }
        let (outcome, result) = match check.as_str() {
            "all_merged" => check_all_merged_detailed(&state.db, plan_name),
            "compiles" => check_compiles_detailed(project_dir.as_deref()).await,
            "ci_green" => {
                check_ci_green_detailed(state, plan_name, project_dir.as_deref(), &[]).await
            }
            other => {
                eprintln!("[gates] end gate: unknown check '{other}' — skipping");
                (
                    GateOutcome::Passed,
                    GateCheckResult::new(other, "skipped", "unknown check — skipped"),
                )
            }
        };
        results.push(result);
        match outcome {
            GateOutcome::Passed => {}
            GateOutcome::Failed(reason) => failed_reason = Some(reason),
            GateOutcome::Blocked(reason) => blocked_reason = Some(reason),
        }
    }

    // Persist the per-check verdicts and push them to the dashboard. Done for
    // every run (pass, fail, or blocked) so the GateCard always shows the
    // latest results — and a reload re-reads them from `gate_checks`.
    persist_and_broadcast_end_checks(state, plan_name, scoped_id, &results);

    match (failed_reason, blocked_reason) {
        (Some(reason), _) => GateOutcome::Failed(reason),
        (None, Some(reason)) => GateOutcome::Blocked(reason),
        (None, None) => {
            // The plan's work is verified and merged — record its declared
            // outputs as satisfied and broadcast `plan_output_produced` so the
            // cross-plan listener re-advances any consumer that was waiting on
            // them (Phase 4.2).
            record_and_notify_outputs(state, plan_name, project_dir.as_deref());
            GateOutcome::Passed
        }
    }
}

/// Record a passing End gate's declared `outputs:` and announce each one with a
/// `plan_output_produced { plan_name, artifact_name }` broadcast (Phase 4.2).
///
/// The "computed value" for each output is the plan's merged HEAD commit SHA (a
/// single point-in-time marker for everything the plan produced). `None` in
/// SaaS / when HEAD can't resolve → stored as the boolean marker `"true"` by
/// [`crate::artifacts::record_output_artifacts`].
///
/// The re-advance of dependent consumers is **not** done inline here — it is
/// driven by the cross-plan listener ([`crate::artifacts::spawn_listener`])
/// reacting to the `plan_output_produced` events. Decoupling via the event
/// means any path that records an output (not just this End gate) triggers the
/// same re-evaluation, and the dashboard gets a first-class cross-plan event.
fn record_and_notify_outputs(state: &AppState, plan_name: &str, project_dir: Option<&Path>) {
    let outputs = plan_outputs(&state.plans_dir, plan_name);
    if outputs.is_empty() {
        return;
    }
    let value = project_dir.and_then(crate::agents::git_head_sha);
    let written =
        crate::artifacts::record_output_artifacts(&state.db, plan_name, &outputs, value.as_deref());

    for artifact_name in written {
        broadcast_event(
            &state.broadcast_tx,
            "plan_output_produced",
            json!({
                "plan_name": plan_name,
                "artifact_name": artifact_name,
            }),
        );
    }
}

/// Load a plan's declared cross-plan `inputs` from its file. A missing /
/// unparseable plan (e.g. a unit-test gate with no file on disk, or a v1 plan)
/// has no declared inputs — return empty so the gate doesn't block spuriously.
fn plan_inputs(plans_dir: &Path, plan_name: &str) -> Vec<crate::dag::PlanArtifact> {
    load_dag(plans_dir, plan_name)
        .map(|d| d.inputs)
        .unwrap_or_default()
}

/// Load a plan's declared `outputs` (see [`plan_inputs`] for the empty cases).
fn plan_outputs(plans_dir: &Path, plan_name: &str) -> Vec<crate::dag::PlanArtifact> {
    load_dag(plans_dir, plan_name)
        .map(|d| d.outputs)
        .unwrap_or_default()
}

fn load_dag(plans_dir: &Path, plan_name: &str) -> Option<crate::dag::DagPlan> {
    let path = crate::plan_parser::find_plan_file(plans_dir, plan_name)?;
    crate::plan_parser::parse_plan_file_as_dag(&path).ok()
}

/// Write the End gate's per-check results to `gate_checks` and broadcast a
/// `gate_check_results` WS event so the dashboard repaints the GateCard
/// without a refetch. The persisted copy is what `GET /api/plans/:name`
/// re-reads on reload.
fn persist_and_broadcast_end_checks(
    state: &AppState,
    plan_name: &str,
    scoped_id: &str,
    results: &[GateCheckResult],
) {
    let json_str = serde_json::to_string(results).unwrap_or_else(|_| "[]".to_string());
    crate::db::write_gate_checks(&state.db, plan_name, scoped_id, &json_str);
    broadcast_event(
        &state.broadcast_tx,
        "gate_check_results",
        json!({
            "plan_name": plan_name,
            "node_id": scoped_id,
            "gate_kind": GateKind::End.as_str(),
            "checks": results,
        }),
    );
}

/// `all_merged`: every branch produced by the plan's agents has been merged
/// (i.e. `agents.branch` cleared to NULL by the merge path). Any non-NULL
/// branch is unmerged work that must land before the plan is "done". The
/// structured result reports an `N/M branches merged` count for the card.
fn check_all_merged_detailed(db: &Db, plan_name: &str) -> (GateOutcome, GateCheckResult) {
    let unmerged = unmerged_branches(db, plan_name);
    let merged = merged_branch_count(db, plan_name);
    let total = merged + unmerged.len();
    if unmerged.is_empty() {
        let detail = if total == 0 {
            "no branches to merge".to_string()
        } else {
            format!("{merged}/{total} branches merged")
        };
        (
            GateOutcome::Passed,
            GateCheckResult::new("all_merged", "passed", detail),
        )
    } else {
        let detail = format!(
            "{merged}/{total} branches merged — unmerged: {}",
            unmerged.join(", ")
        );
        let reason = format!(
            "all_merged: {} unmerged branch(es): {}",
            unmerged.len(),
            unmerged.join(", ")
        );
        (
            GateOutcome::Failed(reason),
            GateCheckResult::new("all_merged", "failed", detail),
        )
    }
}

/// `compiles`: run the project's `[auto_mode.pre_merge_checks]` against the
/// merged default-branch working tree. Opt-in — no config (or no project)
/// ⇒ `skipped` (and treated as a pass for the overall outcome). On failure
/// the structured result carries the failing check name + a bounded output
/// snippet so the dashboard can show the build log inline.
async fn check_compiles_detailed(project_dir: Option<&Path>) -> (GateOutcome, GateCheckResult) {
    let skipped = |detail: &str| {
        (
            GateOutcome::Passed,
            GateCheckResult::new("compiles", "skipped", detail),
        )
    };
    let Some(project_dir) = project_dir else {
        return skipped("no project directory on the server");
    };
    let Some(repo_cfg) = crate::repo_config::load_for_project_dir(project_dir) else {
        return skipped("no branchwork.toml — no checks configured");
    };
    let checks = &repo_cfg.auto_mode.pre_merge_checks;
    if checks.is_empty() {
        return skipped("no pre-merge checks configured");
    }
    let total_timeout = Duration::from_secs(repo_cfg.auto_mode.pre_merge_total_timeout_secs as u64);
    match crate::auto_mode::run_pre_merge_checks_in(checks, project_dir, total_timeout).await {
        crate::auto_mode::GateOutcome::Pass => (
            GateOutcome::Passed,
            GateCheckResult::new(
                "compiles",
                "passed",
                format!("{} check(s) passed", checks.len()),
            ),
        ),
        crate::auto_mode::GateOutcome::Fail {
            check,
            exit_code,
            output,
        } => {
            let exit_str = exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "killed".to_string());
            let detail = format!("check '{check}' failed (exit {exit_str})");
            let reason = format!("compiles: {detail}");
            let mut result = GateCheckResult::new("compiles", "failed", detail);
            result.output = Some(crate::auto_mode::truncate_output(
                &output,
                COMPILE_OUTPUT_SNIPPET_CAP_BYTES,
            ));
            (GateOutcome::Failed(reason), result)
        }
    }
}

/// Count of distinct merged task branches for the plan: completed task agents
/// whose `branch` was cleared to NULL by the merge path. Combined with the
/// unmerged-branch list, this gives the `N/M branches merged` count shown on
/// the gate card. A soft signal (best-effort count) — the pass/fail verdict
/// is driven solely by whether any unmerged branch remains.
fn merged_branch_count(db: &Db, plan_name: &str) -> usize {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(DISTINCT task_id) FROM agents \
         WHERE plan_name = ?1 AND task_id IS NOT NULL \
           AND branch IS NULL AND status = 'completed'",
        params![plan_name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
    .max(0) as usize
}

// ── CI gate ──────────────────────────────────────────────────────────────

async fn execute_ci_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name);
    check_ci_green(state, plan_name, project_dir.as_deref(), &node.workflows).await
}

/// Poll GitHub Actions on the project HEAD and map the aggregate to a gate
/// outcome (CI-gate path — discards the structured detail). Delegates to
/// [`check_ci_green_detailed`].
async fn check_ci_green(
    state: &AppState,
    plan_name: &str,
    project_dir: Option<&Path>,
    declared: &[String],
) -> GateOutcome {
    check_ci_green_detailed(state, plan_name, project_dir, declared)
        .await
        .0
}

/// Poll GitHub Actions on the project HEAD and report both the gate outcome
/// and the structured `ci_green` check result (Task 3.6). Reuses
/// [`crate::saas::dispatch::get_ci_run_status_dispatch`] (mode-aware: SaaS
/// dispatches to the runner, standalone shells out to `gh`).
///
/// `declared` filters the aggregate to a specific set of workflows (the CI
/// gate's `workflows:`); empty ⇒ evaluate the full blocking subset.
///
/// A project with no `.github/workflows` is vacuously green (nothing to
/// wait for) ⇒ `skipped`. No CI runs yet for the HEAD SHA ⇒ `Blocked`
/// (pending). A transient dispatch error ⇒ `Blocked` (retry next tick).
/// On a terminal aggregate the structured result links to the relevant run
/// (the failing run on failure, else the first observed run).
async fn check_ci_green_detailed(
    state: &AppState,
    plan_name: &str,
    project_dir: Option<&Path>,
    declared: &[String],
) -> (GateOutcome, GateCheckResult) {
    let Some(project_dir) = project_dir else {
        // No project dir resolvable on the server (SaaS, or unconfigured) —
        // can't probe workflows locally. Treat as vacuously green so a plan
        // without server-visible CI isn't stuck forever.
        return (
            GateOutcome::Passed,
            GateCheckResult::new("ci_green", "skipped", "no project directory on the server"),
        );
    };
    if !crate::ci::has_github_actions(project_dir) {
        return (
            GateOutcome::Passed,
            GateCheckResult::new("ci_green", "skipped", "no GitHub Actions workflows"),
        );
    }
    let Some(sha) = crate::agents::git_head_sha(project_dir) else {
        return (
            GateOutcome::Blocked("ci_green: cannot resolve project HEAD".to_string()),
            GateCheckResult::new("ci_green", "blocked", "cannot resolve project HEAD"),
        );
    };
    let org = org_for_plan(&state.db, plan_name);
    // `task_number` is wire-only correlation metadata for the dispatch; a
    // gate isn't a task, so pass an empty marker.
    match crate::saas::dispatch::get_ci_run_status_dispatch(state, &org, plan_name, "", &sha).await
    {
        Ok(Some(agg)) => {
            let outcome = evaluate_ci_aggregate(&agg, declared, &sha);
            let (status, detail) = match &outcome {
                GateOutcome::Passed => ("passed", "CI green".to_string()),
                GateOutcome::Failed(_) => ("failed", "CI failed".to_string()),
                GateOutcome::Blocked(_) => ("blocked", "CI still in progress".to_string()),
            };
            let mut result = GateCheckResult::new("ci_green", status, detail);
            // Link the most relevant run: the failing run on failure, else the
            // first observed run. Soft signal — `None` in SaaS / non-github.
            result.url =
                ci_link_run_id(&agg).and_then(|id| crate::ci::derive_run_url(project_dir, &id));
            (outcome, result)
        }
        Ok(None) => (
            GateOutcome::Blocked(format!("ci_green: no CI runs yet for {sha}")),
            GateCheckResult::new("ci_green", "blocked", "no CI runs yet for this commit"),
        ),
        Err(e) => (
            GateOutcome::Blocked(format!("ci_green: CI status unavailable ({e})")),
            GateCheckResult::new("ci_green", "blocked", "CI status unavailable"),
        ),
    }
}

/// Pick the CI run id to link from the gate card: the failing run when one is
/// known, else the first observed run (a green run worth linking to).
fn ci_link_run_id(agg: &crate::saas::runner_protocol::CiAggregate) -> Option<String> {
    agg.failing_run_id
        .clone()
        .or_else(|| agg.runs.first().map(|r| r.run_id.clone()))
}

/// Map a [`crate::saas::runner_protocol::CiAggregate`] to a gate outcome.
/// When `declared` is non-empty, evaluate only those workflows; otherwise
/// use the pre-computed blocking aggregate (`conclusion` / `status`).
fn evaluate_ci_aggregate(
    agg: &crate::saas::runner_protocol::CiAggregate,
    declared: &[String],
    sha: &str,
) -> GateOutcome {
    if declared.is_empty() {
        return match agg.conclusion.as_deref() {
            Some("success") => GateOutcome::Passed,
            Some(other) => {
                GateOutcome::Failed(format!("ci_green: CI concluded '{other}' on {sha}"))
            }
            None => GateOutcome::Blocked(format!("ci_green: CI still {} on {sha}", agg.status)),
        };
    }

    // Filter to the declared workflows (match by workflow name).
    let matched: Vec<&crate::saas::runner_protocol::CiRunSummary> = agg
        .runs
        .iter()
        .filter(|r| declared.iter().any(|w| w == &r.workflow_name))
        .collect();

    if matched.is_empty() {
        return GateOutcome::Blocked(format!(
            "ci_green: declared workflow(s) {declared:?} have not run on {sha}"
        ));
    }
    if let Some(failed) = matched
        .iter()
        .find(|r| matches!(r.conclusion.as_deref(), Some(c) if c != "success" && c != "skipped"))
    {
        return GateOutcome::Failed(format!(
            "ci_green: workflow '{}' concluded '{}' on {sha}",
            failed.workflow_name,
            failed.conclusion.as_deref().unwrap_or("?")
        ));
    }
    if matched.iter().any(|r| r.conclusion.is_none()) {
        return GateOutcome::Blocked(format!(
            "ci_green: declared workflow(s) still running on {sha}"
        ));
    }
    GateOutcome::Passed
}

// ── Approval gate ────────────────────────────────────────────────────────

async fn execute_approval_gate(
    state: &AppState,
    plan_name: &str,
    scoped_id: &str,
    node: &DagNode,
) -> GateOutcome {
    if gate_approved(&state.db, plan_name, scoped_id) {
        return GateOutcome::Passed;
    }
    broadcast_event(
        &state.broadcast_tx,
        "gate_awaiting_approval",
        json!({
            "plan_name": plan_name,
            "node_id": scoped_id,
            "title": node.title,
            "gate_kind": GateKind::Approval.as_str(),
        }),
    );
    GateOutcome::Blocked("awaiting approval".to_string())
}

// ── DB helpers ───────────────────────────────────────────────────────────

/// Whether a `gate_approvals` row exists for `(plan_name, node_id)`.
fn gate_approved(db: &Db, plan_name: &str, node_id: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT 1 FROM gate_approvals WHERE plan_name = ?1 AND node_id = ?2",
        params![plan_name, node_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Branches still attached to the plan's agents (NULL = merged/cleared).
fn unmerged_branches(db: &Db, plan_name: &str) -> Vec<String> {
    let conn = db.lock().unwrap();
    let Ok(mut stmt) =
        conn.prepare("SELECT branch FROM agents WHERE plan_name = ?1 AND branch IS NOT NULL")
    else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![plan_name], |r| r.get::<_, String>(0));
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Resolve the org for a plan from its most recent agent row, defaulting to
/// `default-org` (standalone). Needed for the CI status dispatch.
fn org_for_plan(db: &Db, plan_name: &str) -> String {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT org_id FROM agents \
         WHERE plan_name = ?1 AND org_id IS NOT NULL \
         ORDER BY started_at DESC LIMIT 1",
        params![plan_name],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "default-org".to_string())
}

// ── Shell + string helpers ───────────────────────────────────────────────

/// Run `git <args>` in `cwd`, returning `(success, stdout)`. `None` only if
/// `git` could not be spawned at all.
fn git_stdout(cwd: &Path, args: &[&str]) -> Option<(bool, String)> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()
        .map(|o| {
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).to_string(),
            )
        })
}

/// Truncate `s` to at most `max` bytes (on a char boundary), appending an
/// ellipsis when clipped. Keeps gate failure reasons bounded.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{DagNode, NodeType};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex as StdMutex};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("branchwork.db");
        (crate::db::init(&path), dir)
    }

    fn test_state(db: Db, plans_dir: PathBuf) -> (AppState, broadcast::Receiver<String>) {
        let (broadcast_tx, rx) = broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/nonexistent/branchwork-server"),
            0,
            true,
        );
        let runners = crate::saas::runner_ws::new_runner_registry();
        let state = AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-gates-test-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(HashSet::new())),
            started_at: std::time::Instant::now(),
        };
        (state, rx)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Init a git repo with a clean tree + committed initial content. No
    /// remote yet (callers add one if they need `remote_configured`).
    fn git_init(cwd: &Path) {
        std::fs::create_dir_all(cwd).unwrap();
        run_git(cwd, &["init", "-q", "-b", "master"]);
        run_git(cwd, &["config", "user.email", "t@t.test"]);
        run_git(cwd, &["config", "user.name", "Test"]);
        std::fs::write(cwd.join("README.md"), "init").unwrap();
        run_git(cwd, &["add", "README.md"]);
        run_git(cwd, &["commit", "-q", "-m", "initial"]);
    }

    /// Seed `plan_project` with the *absolute* project path. `project_dir_for`
    /// does `home.join(project)`, and `PathBuf::join` discards the base when
    /// the argument is absolute — so this resolves straight to `project_dir`.
    fn seed_project(db: &Db, plan: &str, project_dir: &Path) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_project (plan_name, project) VALUES (?1, ?2)",
            params![plan, project_dir.to_string_lossy()],
        )
        .unwrap();
    }

    fn seed_agent(db: &Db, id: &str, plan: &str, task: &str, branch: Option<&str>) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents \
                (id, session_id, cwd, status, mode, plan_name, task_id, branch, org_id) \
             VALUES (?1, ?1, '/tmp/x', 'completed', 'pty', ?2, ?3, ?4, 'default-org')",
            params![id, plan, task, branch],
        )
        .unwrap();
    }

    fn drain(rx: &mut broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    fn init_node() -> DagNode {
        DagNode {
            id: "init".to_string(),
            node_type: NodeType::Gate,
            title: "Precondition check".to_string(),
            description: String::new(),
            depends_on: Vec::new(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: false,
            gate_kind: Some(GateKind::Init),
            checks: vec![
                "git_repo".to_string(),
                "remote_configured".to_string(),
                "clean_tree".to_string(),
            ],
            workflows: Vec::new(),
            nodes: Vec::new(),
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    fn end_node() -> DagNode {
        DagNode {
            id: "end".to_string(),
            node_type: NodeType::Gate,
            title: "Final verification".to_string(),
            description: String::new(),
            depends_on: Vec::new(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: false,
            gate_kind: Some(GateKind::End),
            checks: vec![
                "all_merged".to_string(),
                "compiles".to_string(),
                "ci_green".to_string(),
            ],
            workflows: Vec::new(),
            nodes: Vec::new(),
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    fn approval_node() -> DagNode {
        DagNode {
            id: "approve".to_string(),
            node_type: NodeType::Gate,
            title: "Approval".to_string(),
            description: String::new(),
            depends_on: Vec::new(),
            file_paths: Vec::new(),
            acceptance: String::new(),
            produces_commit: false,
            gate_kind: Some(GateKind::Approval),
            checks: Vec::new(),
            workflows: Vec::new(),
            nodes: Vec::new(),
            status: None,
            status_updated_at: None,
            cost_usd: None,
        }
    }

    // ── Acceptance: init gate, all checks pass → Blocked ──────────────────

    #[tokio::test]
    async fn init_gate_with_passing_checks_returns_blocked() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        // `remote_configured` only needs `git remote` to list something; the
        // URL never has to be reachable.
        run_git(
            &project_dir,
            &["remote", "add", "origin", "https://example.com/x.git"],
        );

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        let (state, mut rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "init", &init_node()).await;

        assert!(
            matches!(outcome, GateOutcome::Blocked(_)),
            "init gate with all checks passing must block on approval, got {outcome:?}"
        );

        // The dashboard learns the gate is ready for a human.
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("\"type\":\"gate_ready_for_approval\"")),
            "expected a gate_ready_for_approval broadcast, got {events:?}"
        );
        // …and the uniform "awaiting approval" signal (carrying the gate's
        // title + gate_kind) fires for init gates too (Task 3.3).
        let awaiting = events
            .iter()
            .find(|e| e.contains("\"type\":\"gate_awaiting_approval\""))
            .expect("expected a gate_awaiting_approval broadcast for the init gate");
        assert!(
            awaiting.contains("\"title\":\"Precondition check\""),
            "gate_awaiting_approval must carry the node title, got {awaiting}"
        );
        assert!(
            awaiting.contains("\"gate_kind\":\"init\""),
            "gate_awaiting_approval must carry gate_kind, got {awaiting}"
        );
    }

    // ── Acceptance: end gate, all branches merged → Passed ────────────────

    #[tokio::test]
    async fn end_gate_with_all_branches_merged_returns_passed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir); // no .github/workflows, no branchwork.toml
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        // Two completed agents whose branches were merged (branch cleared).
        seed_agent(&db, "a1", "p", "1.1", None);
        seed_agent(&db, "a2", "p", "1.2", None);

        crate::repo_config::clear_cache_for_tests();
        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;

        assert_eq!(
            outcome,
            GateOutcome::Passed,
            "end gate with all branches merged + no checks failing must pass"
        );
    }

    // ── Init gate: a dirty tree fails the gate ────────────────────────────

    #[tokio::test]
    async fn init_gate_with_dirty_tree_returns_failed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        run_git(
            &project_dir,
            &["remote", "add", "origin", "https://example.com/x.git"],
        );
        // Modify a tracked file ⇒ porcelain reports a dirty tree.
        std::fs::write(project_dir.join("README.md"), "dirty edit").unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "init", &init_node()).await;
        match outcome {
            GateOutcome::Failed(reason) => assert!(
                reason.contains("clean_tree"),
                "expected clean_tree failure, got {reason}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Init gate: no remote fails the gate ───────────────────────────────

    #[tokio::test]
    async fn init_gate_without_remote_returns_failed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir); // no remote added
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "init", &init_node()).await;
        match outcome {
            GateOutcome::Failed(reason) => assert!(
                reason.contains("remote_configured"),
                "expected remote_configured failure, got {reason}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Init gate: a pre-existing approval short-circuits to Passed ───────

    #[tokio::test]
    async fn init_gate_with_existing_approval_returns_passed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        // Deliberately NOT a git repo — the approval short-circuit must
        // bypass the precondition checks entirely.
        std::fs::create_dir_all(&project_dir).unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO gate_approvals (plan_name, node_id, approved_by) VALUES ('p', 'init', 'alice')",
                [],
            )
            .unwrap();
        }

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "init", &init_node()).await;
        assert_eq!(outcome, GateOutcome::Passed);
    }

    // ── End gate: an unmerged branch fails all_merged ─────────────────────

    #[tokio::test]
    async fn end_gate_with_unmerged_branch_returns_failed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        seed_agent(&db, "a1", "p", "1.1", None);
        seed_agent(&db, "a2", "p", "1.2", Some("branchwork/p/1.2")); // unmerged

        crate::repo_config::clear_cache_for_tests();
        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;
        match outcome {
            GateOutcome::Failed(reason) => {
                assert!(reason.contains("all_merged"), "got {reason}");
                assert!(reason.contains("branchwork/p/1.2"), "got {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── End gate: a failing compiles check fails the gate ─────────────────

    #[tokio::test]
    async fn end_gate_with_failing_compiles_check_returns_failed() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        std::fs::write(
            project_dir.join("branchwork.toml"),
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"build\", cmd = \"exit 7\", timeout_secs = 10 },\n\
             ]\n",
        )
        .unwrap();
        crate::repo_config::clear_cache_for_tests();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;
        match outcome {
            GateOutcome::Failed(reason) => {
                assert!(reason.contains("compiles"), "got {reason}");
                assert!(reason.contains("build"), "got {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Task 3.6: structured per-check results (persist + broadcast) ──────

    /// Read the persisted per-check results for a gate node, parsed back into
    /// the typed shape for assertions.
    fn read_results(db: &Db, plan: &str, node_id: &str) -> Vec<GateCheckResult> {
        let conn = db.lock().unwrap();
        let json = crate::db::read_gate_checks_json(&conn, plan, node_id)
            .unwrap_or_else(|| panic!("no gate_checks row for {plan}/{node_id}"));
        serde_json::from_str(&json).expect("gate_checks JSON parses")
    }

    fn find_check<'a>(results: &'a [GateCheckResult], name: &str) -> &'a GateCheckResult {
        results
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no '{name}' check in {results:?}"))
    }

    /// Acceptance: a plan where everything is merged → the End gate passes and
    /// the dashboard learns every check's verdict (persisted + broadcast).
    #[tokio::test]
    async fn end_gate_passing_persists_and_broadcasts_per_check_results() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir); // no .github/workflows, no branchwork.toml
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);
        // Two completed task agents whose branches were merged (branch cleared).
        seed_agent(&db, "a1", "p", "1.1", None);
        seed_agent(&db, "a2", "p", "1.2", None);
        crate::repo_config::clear_cache_for_tests();

        let (state, mut rx) = test_state(db.clone(), plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;
        assert_eq!(outcome, GateOutcome::Passed);

        // Persisted so the GateCard survives a reload.
        let results = read_results(&db, "p", "end");
        assert_eq!(results.len(), 3, "all three checks recorded: {results:?}");
        let all_merged = find_check(&results, "all_merged");
        assert_eq!(all_merged.status, "passed");
        assert_eq!(all_merged.detail, "2/2 branches merged");
        assert_eq!(find_check(&results, "compiles").status, "skipped"); // no toml
        assert_eq!(find_check(&results, "ci_green").status, "skipped"); // no workflows

        // Broadcast for the live dashboard.
        let events = drain(&mut rx);
        let ev = events
            .iter()
            .find(|e| e.contains("\"type\":\"gate_check_results\""))
            .unwrap_or_else(|| panic!("expected gate_check_results, got {events:?}"));
        assert!(ev.contains("\"node_id\":\"end\""), "got {ev}");
        assert!(ev.contains("\"gate_kind\":\"end\""), "got {ev}");
        assert!(ev.contains("2/2 branches merged"), "got {ev}");
    }

    /// Acceptance: if compiles fails, the failure output is visible (captured
    /// as a bounded snippet on the structured result).
    #[tokio::test]
    async fn end_gate_compile_failure_records_output_snippet() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        std::fs::write(
            project_dir.join("branchwork.toml"),
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"build\", cmd = \"echo COMPILE_FAILED_MARKER; exit 7\", timeout_secs = 10 },\n\
             ]\n",
        )
        .unwrap();
        crate::repo_config::clear_cache_for_tests();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);

        let (state, _rx) = test_state(db.clone(), plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;
        assert!(matches!(outcome, GateOutcome::Failed(_)));

        let results = read_results(&db, "p", "end");
        let compiles = find_check(&results, "compiles");
        assert_eq!(compiles.status, "failed");
        assert!(compiles.detail.contains("build"), "got {}", compiles.detail);
        assert!(
            compiles.detail.contains("exit 7"),
            "got {}",
            compiles.detail
        );
        let output = compiles
            .output
            .as_deref()
            .expect("compile failure carries its output snippet");
        assert!(output.contains("COMPILE_FAILED_MARKER"), "got {output}");
        // ci_green is recorded but not run (an earlier check failed).
        assert_eq!(find_check(&results, "ci_green").status, "skipped");
    }

    /// A failing earlier check short-circuits the expensive later checks — they
    /// are recorded as `skipped` (never run), not `failed`.
    #[tokio::test]
    async fn end_gate_unmerged_branch_skips_later_checks() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        // A branchwork.toml that WOULD fail compiles — but it must not run,
        // because all_merged fails first and short-circuits.
        std::fs::write(
            project_dir.join("branchwork.toml"),
            "[auto_mode]\n\
             pre_merge_checks = [\n  \
               { name = \"build\", cmd = \"exit 1\", timeout_secs = 10 },\n\
             ]\n",
        )
        .unwrap();
        crate::repo_config::clear_cache_for_tests();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        seed_project(&db, "p", &project_dir);
        seed_agent(&db, "a1", "p", "1.1", None);
        seed_agent(&db, "a2", "p", "1.2", Some("branchwork/p/1.2"));

        let (state, _rx) = test_state(db.clone(), plans_dir);
        let outcome = execute_gate(&state, "p", "end", &end_node()).await;
        assert!(matches!(outcome, GateOutcome::Failed(_)));

        let results = read_results(&db, "p", "end");
        let all_merged = find_check(&results, "all_merged");
        assert_eq!(all_merged.status, "failed");
        assert_eq!(
            all_merged.detail,
            "1/2 branches merged — unmerged: branchwork/p/1.2"
        );
        // compiles never ran (short-circuit) ⇒ skipped, not failed.
        assert_eq!(find_check(&results, "compiles").status, "skipped");
        assert_eq!(find_check(&results, "ci_green").status, "skipped");
    }

    // ── Approval gate: blocks + broadcasts awaiting-approval ──────────────

    #[tokio::test]
    async fn approval_gate_blocks_and_broadcasts() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, mut rx) = test_state(db, plans_dir);

        let outcome = execute_gate(&state, "p", "approve", &approval_node()).await;
        assert!(matches!(outcome, GateOutcome::Blocked(_)));
        let events = drain(&mut rx);
        let awaiting = events
            .iter()
            .find(|e| e.contains("\"type\":\"gate_awaiting_approval\""))
            .unwrap_or_else(|| panic!("expected gate_awaiting_approval, got {events:?}"));
        // Task 3.3: the approval gate's event carries the node title + kind.
        assert!(
            awaiting.contains("\"title\":\"Approval\""),
            "gate_awaiting_approval must carry the node title, got {awaiting}"
        );
        assert!(
            awaiting.contains("\"gate_kind\":\"approval\""),
            "gate_awaiting_approval must carry gate_kind, got {awaiting}"
        );
    }

    #[tokio::test]
    async fn approval_gate_with_approval_returns_passed() {
        let (db, dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO gate_approvals (plan_name, node_id, approved_by) VALUES ('p', 'approve', 'bob')",
                [],
            )
            .unwrap();
        }
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "p", "approve", &approval_node()).await;
        assert_eq!(outcome, GateOutcome::Passed);
    }

    // ── Pure helper: CI aggregate mapping ─────────────────────────────────

    use crate::saas::runner_protocol::{CiAggregate, CiRunSummary};

    fn agg(status: &str, conclusion: Option<&str>, runs: Vec<CiRunSummary>) -> CiAggregate {
        CiAggregate {
            status: status.to_string(),
            conclusion: conclusion.map(String::from),
            runs,
            failing_run_id: None,
        }
    }

    fn run_summary(workflow: &str, conclusion: Option<&str>) -> CiRunSummary {
        CiRunSummary {
            run_id: "1".to_string(),
            workflow_name: workflow.to_string(),
            status: if conclusion.is_some() {
                "completed".to_string()
            } else {
                "in_progress".to_string()
            },
            conclusion: conclusion.map(String::from),
            skipped_due_to_upstream: false,
            informational: false,
        }
    }

    #[test]
    fn ci_aggregate_success_passes() {
        let a = agg("completed", Some("success"), vec![]);
        assert_eq!(evaluate_ci_aggregate(&a, &[], "abc"), GateOutcome::Passed);
    }

    #[test]
    fn ci_aggregate_failure_fails() {
        let a = agg("completed", Some("failure"), vec![]);
        assert!(matches!(
            evaluate_ci_aggregate(&a, &[], "abc"),
            GateOutcome::Failed(_)
        ));
    }

    #[test]
    fn ci_aggregate_in_progress_blocks() {
        let a = agg("in_progress", None, vec![]);
        assert!(matches!(
            evaluate_ci_aggregate(&a, &[], "abc"),
            GateOutcome::Blocked(_)
        ));
    }

    #[test]
    fn ci_aggregate_declared_workflow_filters() {
        // Declared "tests.yml" passed; an informational "deploy.yml" failed
        // but is not in the declared set ⇒ overall pass.
        let a = agg(
            "completed",
            Some("failure"),
            vec![
                run_summary("tests.yml", Some("success")),
                run_summary("deploy.yml", Some("failure")),
            ],
        );
        assert_eq!(
            evaluate_ci_aggregate(&a, &["tests.yml".to_string()], "abc"),
            GateOutcome::Passed
        );
    }

    #[test]
    fn ci_aggregate_declared_workflow_missing_blocks() {
        let a = agg(
            "completed",
            Some("success"),
            vec![run_summary("other.yml", Some("success"))],
        );
        assert!(matches!(
            evaluate_ci_aggregate(&a, &["tests.yml".to_string()], "abc"),
            GateOutcome::Blocked(_)
        ));
    }

    // ── Cross-plan artifact gating (Phase 4) ──────────────────────────────

    fn output_artifact(name: &str) -> crate::dag::PlanArtifact {
        crate::dag::PlanArtifact {
            name: name.to_string(),
            from_plan: None,
            artifact: None,
            description: None,
        }
    }

    /// A v2 plan declaring a single input from `from_plan` named `artifact`.
    const CONSUMER_YAML: &str = "schema_version: 2\n\
        title: B\n\
        inputs:\n\
        \x20 - name: schema\n\
        \x20   fromPlan: a\n\
        nodes:\n\
        \x20 - id: init\n\
        \x20   type: gate\n\
        \x20   gate_kind: init\n";

    /// Acceptance (consumer side): an Init gate with an unsatisfied cross-plan
    /// input stays Blocked with `waiting for <plan>/<artifact>`, before its own
    /// git preconditions even run.
    #[tokio::test]
    async fn init_gate_blocks_on_unsatisfied_inputs() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::write(plans_dir.join("b.yaml"), CONSUMER_YAML).unwrap();
        // A project exists, but the inputs gate fires before it's consulted.
        let project_dir = dir.path().join("bproj");
        git_init(&project_dir);
        seed_project(&db, "b", &project_dir);

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "b", "init", &init_node()).await;
        match outcome {
            GateOutcome::Blocked(reason) => assert_eq!(reason, "waiting for a/schema"),
            other => panic!("expected Blocked(waiting for a/schema), got {other:?}"),
        }
    }

    /// Acceptance (the full hand-off): once the producing plan records its
    /// output, the consumer's Init gate re-check passes the inputs gate and
    /// proceeds to its own preconditions (here: blocks awaiting approval — NOT
    /// the inputs-blocked reason).
    #[tokio::test]
    async fn init_gate_proceeds_past_inputs_after_producer_records_output() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::write(plans_dir.join("b.yaml"), CONSUMER_YAML).unwrap();
        // Clean git repo + remote so the init preconditions pass.
        let project_dir = dir.path().join("bproj");
        git_init(&project_dir);
        run_git(
            &project_dir,
            &["remote", "add", "origin", "https://example.com/x.git"],
        );
        seed_project(&db, "b", &project_dir);

        // Producer A records the output B is waiting on.
        crate::artifacts::record_output_artifacts(
            &db,
            "a",
            &[output_artifact("schema")],
            Some("sha-a"),
        );

        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "b", "init", &init_node()).await;
        match outcome {
            GateOutcome::Blocked(reason) => {
                assert!(
                    !reason.starts_with("waiting for"),
                    "inputs should be satisfied now, got {reason}"
                );
                assert!(
                    reason.contains("awaiting approval"),
                    "expected the preconditions-passed approval block, got {reason}"
                );
            }
            other => panic!("expected Blocked(awaiting approval), got {other:?}"),
        }
    }

    /// Acceptance (producer side): a passing End gate records the plan's
    /// declared outputs in `plan_artifacts`, valued at the merged HEAD SHA and
    /// stamped `satisfied_at`.
    #[tokio::test]
    async fn end_gate_passing_records_declared_outputs() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir); // HEAD commit; no workflows, no branchwork.toml
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::write(
            plans_dir.join("a.yaml"),
            "schema_version: 2\n\
             title: A\n\
             outputs:\n\
             \x20 - name: schema\n\
             nodes:\n\
             \x20 - id: end\n\
             \x20   type: gate\n\
             \x20   gate_kind: end\n",
        )
        .unwrap();
        seed_project(&db, "a", &project_dir);

        let head = crate::agents::git_head_sha(&project_dir).expect("HEAD sha");

        crate::repo_config::clear_cache_for_tests();
        let (state, _rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "a", "end", &end_node()).await;
        assert_eq!(outcome, GateOutcome::Passed, "clean end gate must pass");

        let conn = state.db.lock().unwrap();
        let (value, satisfied): (String, Option<String>) = conn
            .query_row(
                "SELECT value, satisfied_at FROM plan_artifacts \
                 WHERE plan_name='a' AND artifact_name='schema' AND direction='output'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("end gate should have recorded the output");
        assert_eq!(value, head, "output value should be the merged HEAD SHA");
        assert!(satisfied.is_some(), "output must be stamped satisfied_at");
    }

    /// Acceptance (producer side, Phase 4.2): a passing End gate broadcasts one
    /// `plan_output_produced { plan_name, artifact_name }` per declared output.
    /// The cross-plan listener reacts to these to re-advance blocked consumers
    /// — that re-advance is exercised in
    /// `crate::artifacts::tests::handle_plan_output_produced_*` and end-to-end
    /// in `tests/cross_plan_artifacts.rs`.
    #[tokio::test]
    async fn end_gate_passing_broadcasts_plan_output_produced() {
        let (db, dir) = fresh_db();
        let project_dir = dir.path().join("project");
        git_init(&project_dir);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        // Producer A declares two outputs.
        std::fs::write(
            plans_dir.join("a.yaml"),
            "schema_version: 2\n\
             title: A\n\
             outputs:\n\
             \x20 - name: schema\n\
             \x20 - name: client\n\
             nodes:\n\
             \x20 - id: end\n\
             \x20   type: gate\n\
             \x20   gate_kind: end\n",
        )
        .unwrap();
        seed_project(&db, "a", &project_dir);

        crate::repo_config::clear_cache_for_tests();
        let (state, mut rx) = test_state(db, plans_dir);
        let outcome = execute_gate(&state, "a", "end", &end_node()).await;
        assert_eq!(outcome, GateOutcome::Passed);

        let events = drain(&mut rx);
        let produced: Vec<&String> = events
            .iter()
            .filter(|e| e.contains("\"type\":\"plan_output_produced\""))
            .collect();
        assert_eq!(
            produced.len(),
            2,
            "one plan_output_produced per declared output, got {events:?}"
        );
        assert!(
            produced.iter().all(|e| e.contains("\"plan_name\":\"a\"")),
            "events must carry the producing plan name, got {produced:?}"
        );
        assert!(
            produced
                .iter()
                .any(|e| e.contains("\"artifact_name\":\"schema\"")),
            "missing the `schema` output event, got {produced:?}"
        );
        assert!(
            produced
                .iter()
                .any(|e| e.contains("\"artifact_name\":\"client\"")),
            "missing the `client` output event, got {produced:?}"
        );
    }
}
