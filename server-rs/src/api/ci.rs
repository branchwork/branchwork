//! CI-related HTTP endpoints.
//!
//! Separate from `ci.rs` (the polling / trigger logic) so the API surface
//! stays easy to scan alongside the other `api/*` modules.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::agents::pty_agent;
use crate::state::AppState;

/// GET /api/ci/{run_id}/failure-log
///
/// Returns the cached-or-freshly-fetched failure log for a CI run. The
/// first call shells out to `gh run view --log-failed` in the project's
/// directory, stores the tail in `ci_runs.failure_log`, and returns it.
/// Subsequent calls serve from the cache.
pub async fn failure_log(
    State(state): State<AppState>,
    Path(ci_run_id): Path<i64>,
) -> impl IntoResponse {
    match crate::ci::fetch_failure_log(&state.db, state.plans_dir.clone(), ci_run_id).await {
        Some(log) => Json(serde_json::json!({
            "ciRunId": ci_run_id,
            "log": log,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "failure log unavailable — run may still be pending, \
                          have no remote, or `gh` is not installed"
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixCiBody {
    pub plan_name: String,
    pub task_number: String,
    pub ci_run_id: i64,
    /// Driver override. Defaults to the server's default driver.
    pub driver: Option<String>,
}

/// POST /api/actions/fix-ci
///
/// Spawn an agent on a recovery branch off the failing commit, with the
/// CI failure log baked into the prompt and a hard pre-commit requirement.
/// The agent commits its fix on `orchestrai/fix/<plan>/<task>/<run_id>`;
/// the user merges it through the existing banner UX, which creates a
/// fresh `ci_runs` row that eventually supersedes the red badge.
pub async fn fix_ci(
    State(state): State<AppState>,
    Json(body): Json<FixCiBody>,
) -> impl IntoResponse {
    // 1. Load the failing run's SHA + status. Bail if the run isn't a
    //    failure — nothing to fix.
    let (status, commit_sha): (String, Option<String>) = {
        let conn = state.db.lock().unwrap();
        match conn.query_row(
            "SELECT status, commit_sha FROM ci_runs WHERE id = ?1",
            rusqlite::params![body.ci_run_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "CI run not found"})),
                )
                    .into_response();
            }
        }
    };
    if status != "failure" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("CI run is '{status}', not 'failure' — nothing to fix"),
            })),
        )
            .into_response();
    }
    let Some(commit_sha) = commit_sha else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "CI run has no commit SHA — cannot branch off it"
            })),
        )
            .into_response();
    };

    // 2. Resolve the plan + task so the agent gets task context in its prompt.
    let plan_path = match crate::plan_parser::find_plan_file(&state.plans_dir, &body.plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match crate::plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "plan not parseable"})),
            )
                .into_response();
        }
    };
    let Some((phase, task)) = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter().map(move |t| (p, t)))
        .find(|(_, t)| t.number == body.task_number)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "task not found in plan"})),
        )
            .into_response();
    };

    // 3. Locate the project dir so we know where to branch and spawn the
    //    agent. Fix-CI only makes sense for plans that map to a real repo.
    let Some(cwd) = crate::ci::project_dir_for(&state.plans_dir, &state.db, &body.plan_name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "plan has no resolvable project directory"
            })),
        )
            .into_response();
    };

    // 4. Fetch the failure log (cached if available). A missing log is a
    //    soft failure — we still spawn the agent with a stub, so the user
    //    can pick up from the task card even if `gh` isn't available.
    let failure_log =
        crate::ci::fetch_failure_log(&state.db, state.plans_dir.clone(), body.ci_run_id)
            .await
            .unwrap_or_else(|| "(no failure log available — check the CI run URL)".to_string());

    // 5. Pre-create the recovery branch pointing at the failing SHA. The
    //    spawn path's `git_checkout_branch(..., is_continue=true)` will
    //    then just check it out.
    let fix_branch = format!(
        "orchestrai/fix/{plan}/{task}/{run}",
        plan = body.plan_name,
        task = body.task_number,
        run = body.ci_run_id,
    );
    let checkout = std::process::Command::new("git")
        .args(["checkout", "-b", &fix_branch, &commit_sha])
        .current_dir(&cwd)
        .output();
    match checkout {
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Benign if it already exists from a prior attempt — fall through.
            if !stderr.contains("already exists") {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("git checkout -b failed: {stderr}"),
                    })),
                )
                    .into_response();
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("git not runnable: {e}")})),
            )
                .into_response();
        }
        _ => {}
    }

    // 6. Build the prompt: reuse the canonical task prompt, then append a
    //    CI-failure section with the log tail and a hard requirement to run
    //    the project's local checks before committing. The pre-commit
    //    language intentionally duplicates build_task_prompt step 3 so a
    //    reader skimming the "CI is failing" section gets it in context.
    let port = state.config_port();
    let mcp_available = state
        .registry
        .drivers
        .injects_mcp(body.driver.as_deref(), port);
    let cross_ctx =
        crate::agents::build_cross_plan_context(&state.db, &state.plans_dir, &plan, &task.number);
    let base = crate::agents::build_task_prompt(
        &plan,
        phase,
        task,
        /* is_continue */ true,
        port,
        cross_ctx.as_deref(),
        mcp_available,
    );
    let prompt = format!(
        "{base}\n\n\
         --- CI IS FAILING ---\n\
         The last merge for this task (commit {sha}) broke CI. You are on a recovery \
         branch `{fix_branch}` off that commit. Fix whatever the log below reports, \
         then commit and push so the merge supersedes the failing run.\n\n\
         Failure log (tail, ~8 KB):\n{log}\n\n\
         Before you commit, you MUST run:\n  \
         cargo fmt && cargo clippy --release --all-targets -- -D warnings && cargo test --release\n\
         in the project root. If any of these fail, fix them before committing. The \
         previous agent skipped these and that is exactly why we're here.",
        base = base,
        sha = commit_sha,
        fix_branch = fix_branch,
        log = failure_log,
    );

    // 7. Spawn the agent. `is_continue=true` so start_pty_agent just
    //    checks out the (now-existing) fix branch instead of trying to
    //    create it again.
    let effort = *state.effort.lock().unwrap();
    let agent_id = pty_agent::start_pty_agent(
        &state.registry,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &cwd,
            plan_name: Some(&body.plan_name),
            task_id: Some(&body.task_number),
            effort,
            branch: Some(&fix_branch),
            is_continue: true,
            max_budget_usd: None,
            driver: body.driver.as_deref(),
        },
    )
    .await;

    Json(serde_json::json!({
        "agentId": agent_id,
        "branch": fix_branch,
        "ciRunId": body.ci_run_id,
    }))
    .into_response()
}
