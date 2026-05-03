//! Git operation dispatchers: route through a connected runner in SaaS mode,
//! or shell out locally in standalone mode.
//!
//! `org_has_runner(db, org_id)` is the boolean mode switch:
//! - **standalone** (no `runners` row): every helper shells out via the local
//!   `git` binary, exactly as the pre-runner code did.
//! - **SaaS** (any `runners` row): every helper enqueues a [`WireMessage`]
//!   request, awaits the runner's reply with a short timeout, and propagates
//!   the result. The server runs no `git` subprocess in this mode.
//!
//! HTTP-driven callers (merge, list-targets) get an explicit `Result` so they
//! can map [`RunnerRpcError`] to 503/504/500. The CI poller — which is not
//! tied to a live HTTP request — uses the same dispatchers but downgrades
//! `Err(_)` to "skip this pass and try again next cycle" so a transient
//! disconnect doesn't age out rows.
//!
//! See `docs/architecture/protocols.md#requestresponse-frames` for the wire
//! protocol; see this plan's Phase 5.7 for the runner-side implementation
//! that actually executes the git commands.

use std::path::Path;
use std::time::Duration;

use uuid::Uuid;

use crate::db::Db;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::{MergeOutcome, WireMessage};
use crate::saas::runner_rpc::{RunnerRpcError, runner_request_with_registry};
use crate::saas::runner_ws::{RunnerRegistry, RunnerResponse};

/// Default timeout for read-side dispatchers (default-branch, list-branches).
/// Generous enough to cover the WS round-trip + a `git` shell-out on the
/// runner, tight enough that an unresponsive runner doesn't stall the UI.
const READ_TIMEOUT: Duration = Duration::from_secs(8);

/// Default timeout for write-side dispatchers (merge, push). Longer than
/// reads because `git merge` and `git push` can take several seconds on
/// large repos.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

// ── default_branch ──────────────────────────────────────────────────────────

/// Resolve the canonical default branch.
/// - SaaS path: dispatch [`WireMessage::GetDefaultBranch`] to the runner.
/// - Standalone: shell out via [`crate::git_helpers::git_default_branch`].
///
/// Inner `Option<String>` is `None` when no candidate resolved (no
/// `origin/HEAD`, no local `master`/`main`). Outer `Result` is `Err` only
/// when the SaaS path failed to reach the runner.
pub async fn default_branch(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
) -> Result<Option<String>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::GetDefaultBranch {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, READ_TIMEOUT).await? {
            RunnerResponse::DefaultBranchResolved(branch) => Ok(branch),
            other => unexpected_response("default_branch_resolved", &other),
        }
    } else {
        Ok(crate::git_helpers::git_default_branch(cwd))
    }
}

// ── list_branches ───────────────────────────────────────────────────────────

/// List local branches (sorted, no remotes).
/// - SaaS path: dispatch [`WireMessage::ListBranches`] to the runner.
/// - Standalone: shell out via [`crate::git_helpers::git_list_branches`].
pub async fn list_branches(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
) -> Result<Vec<String>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::ListBranches {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, READ_TIMEOUT).await? {
            RunnerResponse::BranchesListed(branches) => Ok(branches),
            other => unexpected_response("branches_listed", &other),
        }
    } else {
        Ok(crate::git_helpers::git_list_branches(cwd))
    }
}

// ── merge_branch ────────────────────────────────────────────────────────────

/// Merge `task_branch` into `target` in `cwd`.
/// - SaaS path: dispatch [`WireMessage::MergeBranch`] to the runner. The
///   runner runs the same five-step sequence as `merge_branch_local` and
///   replies with a [`MergeOutcome`].
/// - Standalone: run [`merge_branch_local`] directly.
pub async fn merge_branch(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
    target: &str,
    task_branch: &str,
) -> Result<MergeOutcome, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::MergeBranch {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
            target: target.to_string(),
            task_branch: task_branch.to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, WRITE_TIMEOUT).await? {
            RunnerResponse::MergeResult(outcome) => Ok(outcome),
            other => unexpected_response("merge_result", &other),
        }
    } else {
        Ok(merge_branch_local(cwd, target, task_branch))
    }
}

/// Run the five-step merge sequence locally. Implementation lives in
/// `crate::git_helpers` so the runner binary can pull it in via `#[path]`
/// and call the exact same logic on the runner side.
pub use crate::git_helpers::merge_branch_local;

// ── push_branch ─────────────────────────────────────────────────────────────

/// `git push origin <branch>` in `cwd`.
/// - SaaS path: dispatch [`WireMessage::PushBranch`] to the runner.
/// - Standalone: run [`push_branch_local`] directly.
///
/// Outer `Result` is `Err` only when the SaaS path failed to reach the runner.
/// Inner `Result<(), String>` is `Ok(())` on a successful push and
/// `Err(stderr)` when the push itself failed (no remote, auth error, etc).
pub async fn push_branch(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
    branch: &str,
) -> Result<Result<(), String>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::PushBranch {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
            branch: branch.to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, WRITE_TIMEOUT).await? {
            RunnerResponse::PushResult { ok: true, .. } => Ok(Ok(())),
            RunnerResponse::PushResult { ok: false, stderr } => {
                Ok(Err(stderr.unwrap_or_else(|| "push failed".to_string())))
            }
            other => unexpected_response("push_result", &other),
        }
    } else {
        Ok(push_branch_local(cwd, branch))
    }
}

/// Local implementation of `git push origin <branch>`. Implementation lives
/// in `crate::git_helpers` so the runner binary can reuse it.
pub use crate::git_helpers::push_branch_local;

// ── helpers ─────────────────────────────────────────────────────────────────

/// Map an unexpected reply variant to `RunnerRpcError::InvalidRequest`. The
/// only way this can happen is a runner that violates the protocol — in which
/// case we want the caller to bubble up a 500-equivalent rather than silently
/// returning a default value.
fn unexpected_response<T>(expected: &str, got: &RunnerResponse) -> Result<T, RunnerRpcError> {
    eprintln!("[git_ops] expected {expected}, got {got:?}");
    Err(RunnerRpcError::InvalidRequest)
}
