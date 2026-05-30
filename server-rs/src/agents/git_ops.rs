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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use uuid::Uuid;

use crate::agents::worktree::{OrphanWorktree, RemoveOutcome};
use crate::db::Db;
use crate::git_helpers::{PushError, PushReport};
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::{MergeOutcome, WireMessage, WorktreeOrphan};
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

/// Timeout for worktree dispatchers (setup / remove / list). `git worktree
/// add` checks out a tree, comparable to the other git shell-outs; the runner
/// side caps the same op at 30s, so both ends use 30s.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(30);

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
/// Inner `Result<PushReport, PushError>` is `Ok(PushReport { retries })` on a
/// successful push (with the rebase-retry history; empty vec when the very
/// first attempt landed cleanly) and `Err(PushError)` when the push itself
/// failed. The structured [`PushError::RebaseConflict`] variant is what the
/// auto-mode caller (`ci::trigger_after_merge`) branches on to pause the
/// plan with reason `auto_push_rebase_conflict`.
///
/// SaaS-mode visibility is a known gap on two axes: (a) rebase-retry
/// history is lost because `WireMessage::PushResult` today carries only
/// `{ok, stderr}`, and (b) rebase conflicts come back as generic
/// `PushError::Other { stderr }` since the runner can't transmit the
/// structured `RebaseConflict { files }` over the existing wire shape.
/// Extending the wire protocol to carry both is tracked as a follow-up;
/// for now, only the standalone path produces the
/// `auto_push_rebase_retry` audit + `auto_push_rebased` broadcast and
/// the `auto_push_rebase_conflict` pause.
pub async fn push_branch(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
    branch: &str,
) -> Result<Result<PushReport, PushError>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::PushBranch {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
            branch: branch.to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, WRITE_TIMEOUT).await? {
            RunnerResponse::PushResult { ok: true, .. } => Ok(Ok(PushReport::default())),
            RunnerResponse::PushResult { ok: false, stderr } => Ok(Err(PushError::Other {
                stderr: stderr.unwrap_or_else(|| "push failed".to_string()),
            })),
            other => unexpected_response("push_result", &other),
        }
    } else {
        Ok(push_branch_local(cwd, branch))
    }
}

/// Local implementation of `git push origin <branch>`. Implementation lives
/// in `crate::git_helpers` so the runner binary can reuse it.
pub use crate::git_helpers::push_branch_local;

// ── worktree dispatchers (Phase 0.4: runner owns the FS in SaaS) ─────────────

/// Set up (or, on continue, re-attach) the per-agent git worktree and return
/// its absolute on-disk path.
/// - SaaS path: dispatch [`WireMessage::SetupWorktree`] to the runner, which
///   resolves its own `$BRANCHWORK_WORKTREE_BASE` and replies with the path it
///   created (`WorktreeCreated`) or the failure (`WorktreeSetupFailed`).
/// - Standalone: delegate to [`crate::agents::setup_agent_worktree`] (env-based
///   base) or [`crate::agents::setup_agent_worktree_in`] (test base override).
///
/// `plan_name` / `task_id` accept `None`; the freestanding-agent placeholders
/// are substituted here for the wire (the runner has no access to the
/// constants) and inside `setup_agent_worktree*` for the local path, so both
/// modes produce the same path shape.
///
/// Outer `Result` is `Err` only when the SaaS path failed to reach the runner.
/// Inner `Result<PathBuf, String>` is `Ok(path)` on success / `Err(msg)` when
/// the worktree setup itself failed (git error).
#[allow(clippy::too_many_arguments)]
pub async fn setup_worktree(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    project_dir: &Path,
    project_slug: &str,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    agent_id: &str,
    branch: &str,
    base_branch: Option<&str>,
    is_continue: bool,
    base_override: Option<&Path>,
) -> Result<Result<PathBuf, String>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::SetupWorktree {
            req_id,
            project_dir: project_dir.to_string_lossy().to_string(),
            project_slug: project_slug.to_string(),
            plan: plan_name
                .unwrap_or(crate::agents::NO_PLAN_SEGMENT)
                .to_string(),
            task: task_id
                .unwrap_or(crate::agents::NO_TASK_SEGMENT)
                .to_string(),
            agent_id: agent_id.to_string(),
            branch: branch.to_string(),
            base_branch: base_branch.map(str::to_string),
            is_continue,
        };
        match runner_request_with_registry(db, runners, org_id, msg, WORKTREE_TIMEOUT).await? {
            RunnerResponse::WorktreeSetup(result) => Ok(result.map(PathBuf::from)),
            other => unexpected_response("worktree_setup", &other),
        }
    } else {
        let result = match base_override {
            Some(base) => crate::agents::setup_agent_worktree_in(
                base,
                project_dir,
                project_slug,
                plan_name,
                task_id,
                agent_id,
                branch,
                base_branch,
                is_continue,
            ),
            None => crate::agents::setup_agent_worktree(
                project_dir,
                project_slug,
                plan_name,
                task_id,
                agent_id,
                branch,
                base_branch,
                is_continue,
            ),
        };
        Ok(result.map_err(|e| e.to_string()))
    }
}

/// Remove a per-agent worktree.
/// - SaaS path: dispatch [`WireMessage::RemoveWorktree`] to the runner.
/// - Standalone: run [`crate::agents::worktree::remove_worktree`] directly.
///
/// Inner `Result<RemoveOutcome, String>` mirrors the local function: `Ok` with
/// the escalation tier that succeeded, or `Err(msg)` when even the `rm -rf`
/// fallback failed.
#[allow(dead_code)] // cleanup-path consumer (kill/fail/merge sweep) lands in a later phase
pub async fn remove_worktree(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    project_dir: &Path,
    path: &Path,
) -> Result<Result<RemoveOutcome, String>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::RemoveWorktree {
            req_id,
            project_dir: project_dir.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, WORKTREE_TIMEOUT).await? {
            RunnerResponse::WorktreeRemoved(outcome) => Ok(parse_remove_outcome(&outcome)),
            other => unexpected_response("worktree_removed", &other),
        }
    } else {
        Ok(crate::agents::worktree::remove_worktree(project_dir, path).map_err(|e| e.to_string()))
    }
}

/// List orphaned worktrees under `<base>/<project_slug>/` that no live agent
/// claims (`live_cwds`).
/// - SaaS path: dispatch [`WireMessage::ListWorktreeOrphans`] to the runner.
/// - Standalone: run [`crate::agents::worktree::list_orphans`] directly.
#[allow(dead_code)] // startup orphan-sweep consumer lands in a later phase
pub async fn list_worktree_orphans(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    project_dir: &Path,
    project_slug: &str,
    live_cwds: &HashSet<PathBuf>,
) -> Result<Vec<OrphanWorktree>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::ListWorktreeOrphans {
            req_id,
            project_dir: project_dir.to_string_lossy().to_string(),
            project_slug: project_slug.to_string(),
            live_cwds: live_cwds
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, WORKTREE_TIMEOUT).await? {
            RunnerResponse::WorktreeOrphansListed(orphans) => {
                Ok(orphans.into_iter().map(wire_orphan_to_local).collect())
            }
            other => unexpected_response("worktree_orphans_listed", &other),
        }
    } else {
        Ok(crate::agents::worktree::list_orphans(
            project_dir,
            project_slug,
            live_cwds,
        ))
    }
}

/// Parse the runner's `WorktreeRemoved.outcome` label back into a
/// [`RemoveOutcome`]. Unknown / `error: …` labels collapse to `Err(msg)` so
/// the SaaS path mirrors the local `Result<RemoveOutcome, WorktreeError>`.
fn parse_remove_outcome(outcome: &str) -> Result<RemoveOutcome, String> {
    match outcome {
        "removed" => Ok(RemoveOutcome::Removed),
        "force_removed" => Ok(RemoveOutcome::ForceRemoved),
        "manually_removed" => Ok(RemoveOutcome::ManuallyRemoved),
        other => Err(other.strip_prefix("error: ").unwrap_or(other).to_string()),
    }
}

/// Convert a wire [`WorktreeOrphan`] back into the local [`OrphanWorktree`]
/// shape (epoch seconds → `SystemTime`).
fn wire_orphan_to_local(o: WorktreeOrphan) -> OrphanWorktree {
    OrphanWorktree {
        path: PathBuf::from(o.path),
        branch: o.branch,
        last_modified: o
            .last_modified_secs
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs)),
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Map an unexpected reply variant to `RunnerRpcError::InvalidRequest`. The
/// only way this can happen is a runner that violates the protocol — in which
/// case we want the caller to bubble up a 500-equivalent rather than silently
/// returning a default value.
fn unexpected_response<T>(expected: &str, got: &RunnerResponse) -> Result<T, RunnerRpcError> {
    eprintln!("[git_ops] expected {expected}, got {got:?}");
    Err(RunnerRpcError::InvalidRequest)
}

#[cfg(test)]
mod worktree_dispatch_tests {
    //! Round-trip the worktree dispatchers (Task 2.7). The SaaS path is driven
    //! by an in-test echo runner that captures the outgoing `WireMessage` and
    //! replies via the runner's `pending` map — the same shape `runner_request`
    //! resolves against in production. The standalone path drives a real git
    //! repo + tempdir base override.

    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};

    use rusqlite::{Connection, params};
    use tokio::sync::{Mutex, mpsc, oneshot};

    use crate::saas::runner_protocol::Envelope;
    use crate::saas::runner_ws::{ConnectedRunner, new_runner_registry};

    /// Test-local `req_id` extractor for the worktree request variants the
    /// dispatch tests send (the production `runner_rpc::req_id_for` is private;
    /// the dispatch.rs tests follow the same inline-copy convention).
    fn req_id_for_inline(msg: &WireMessage) -> Option<&str> {
        match msg {
            WireMessage::SetupWorktree { req_id, .. }
            | WireMessage::RemoveWorktree { req_id, .. }
            | WireMessage::ListWorktreeOrphans { req_id, .. } => Some(req_id),
            _ => None,
        }
    }

    fn db_with_online_runner(runner_id: &str, org_id: &str) -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, name TEXT, org_id TEXT, status TEXT, \
               hostname TEXT, version TEXT, last_seen_at TEXT, created_at TEXT \
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
             VALUES (?1, 'test', ?2, 'online', datetime('now'))",
            params![runner_id, org_id],
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    fn empty_runner_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, org_id TEXT, status TEXT, last_seen_at TEXT \
             );",
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    /// Register a runner whose `command_tx` parses each outgoing envelope, hands
    /// the message to `respond`, and resolves the matching pending entry.
    async fn install_echo_runner<F>(registry: &RunnerRegistry, runner_id: &str, respond: F)
    where
        F: Fn(WireMessage) -> Option<RunnerResponse> + Send + Sync + 'static,
    {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(payload) = cmd_rx.recv().await {
                let env: Envelope = match serde_json::from_str(&payload) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let req_id = match req_id_for_inline(&env.message) {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                if let Some(reply) = respond(env.message)
                    && let Some(tx) = pending_clone.lock().await.remove(&req_id)
                {
                    let _ = tx.send(reply);
                }
            }
        });
        registry.lock().await.insert(
            runner_id.to_string(),
            ConnectedRunner {
                command_tx: cmd_tx,
                hostname: None,
                version: None,
                drivers: None,
                pending,
                server_url: "http://localhost:3100".to_string(),
            },
        );
    }

    #[tokio::test]
    async fn setup_worktree_saas_dispatches_and_resolves_path() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |msg| match msg {
            WireMessage::SetupWorktree { .. } => Some(RunnerResponse::WorktreeSetup(Ok(
                "/home/r/.branchwork/worktrees/proj/p/0.1-abcdef12".into(),
            ))),
            _ => None,
        })
        .await;

        let result = setup_worktree(
            &db,
            &registry,
            "org-1",
            Path::new("/runner/proj"),
            "proj",
            Some("p"),
            Some("0.1"),
            "abcdef1234",
            "branchwork/p/0.1",
            Some("master"),
            false,
            None,
        )
        .await
        .expect("dispatch should reach the runner");
        assert_eq!(
            result,
            Ok(PathBuf::from(
                "/home/r/.branchwork/worktrees/proj/p/0.1-abcdef12"
            ))
        );
    }

    #[tokio::test]
    async fn setup_worktree_saas_failure_propagates_inner_err() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |msg| match msg {
            WireMessage::SetupWorktree { .. } => Some(RunnerResponse::WorktreeSetup(Err(
                "git worktree add failed: already exists".into(),
            ))),
            _ => None,
        })
        .await;

        let result = setup_worktree(
            &db,
            &registry,
            "org-1",
            Path::new("/runner/proj"),
            "proj",
            None,
            None,
            "id",
            "b",
            None,
            true,
            None,
        )
        .await
        .expect("dispatch should reach the runner");
        match result {
            Err(msg) => assert!(msg.contains("already exists")),
            Ok(p) => panic!("expected inner Err, got Ok({})", p.display()),
        }
    }

    #[tokio::test]
    async fn setup_worktree_no_runner_is_rpc_err() {
        // SaaS routing requested (db has a runner row) but the in-memory
        // registry is empty (post-restart) → NoConnectedRunner.
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        let err = setup_worktree(
            &db,
            &registry,
            "org-1",
            Path::new("/runner/proj"),
            "proj",
            None,
            None,
            "id",
            "b",
            None,
            false,
            None,
        )
        .await
        .expect_err("no connected runner");
        assert!(matches!(err, RunnerRpcError::NoConnectedRunner));
    }

    #[tokio::test]
    async fn setup_worktree_standalone_creates_local_worktree() {
        // No runner for the org → local shell-out, honouring the base override.
        let db = empty_runner_db();
        let registry = new_runner_registry();
        let project = tempfile::TempDir::new().unwrap();
        let base = tempfile::TempDir::new().unwrap();
        init_repo(project.path());

        let result = setup_worktree(
            &db,
            &registry,
            "org-standalone",
            project.path(),
            "proj",
            Some("p"),
            Some("0.1"),
            "abcdef1234",
            "branchwork/p/0.1",
            None,
            false,
            Some(base.path()),
        )
        .await
        .expect("standalone never errors at the RPC layer");
        let path = result.expect("worktree should be created");
        assert!(path.exists());
        assert!(path.starts_with(base.path()));
    }

    #[tokio::test]
    async fn remove_worktree_saas_parses_outcome() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |msg| match msg {
            WireMessage::RemoveWorktree { .. } => {
                Some(RunnerResponse::WorktreeRemoved("force_removed".into()))
            }
            _ => None,
        })
        .await;

        let result = remove_worktree(
            &db,
            &registry,
            "org-1",
            Path::new("/runner/proj"),
            Path::new("/home/r/.branchwork/worktrees/proj/p/0.1-id"),
        )
        .await
        .expect("dispatch should reach the runner");
        assert_eq!(result, Ok(RemoveOutcome::ForceRemoved));
    }

    #[tokio::test]
    async fn list_worktree_orphans_saas_converts_wire_orphans() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |msg| match msg {
            WireMessage::ListWorktreeOrphans { .. } => {
                Some(RunnerResponse::WorktreeOrphansListed(vec![
                    WorktreeOrphan {
                        path: "/home/r/.branchwork/worktrees/proj/p/1.2-bbbb".into(),
                        branch: Some("branchwork/p/1.2".into()),
                        last_modified_secs: Some(1_700_000_000),
                    },
                ]))
            }
            _ => None,
        })
        .await;

        let orphans = list_worktree_orphans(
            &db,
            &registry,
            "org-1",
            Path::new("/runner/proj"),
            "proj",
            &HashSet::new(),
        )
        .await
        .expect("dispatch should reach the runner");
        assert_eq!(orphans.len(), 1);
        assert_eq!(
            orphans[0].path,
            PathBuf::from("/home/r/.branchwork/worktrees/proj/p/1.2-bbbb")
        );
        assert_eq!(orphans[0].branch.as_deref(), Some("branchwork/p/1.2"));
        assert_eq!(
            orphans[0]
                .last_modified
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            Some(1_700_000_000)
        );
    }

    fn init_repo(dir: &Path) {
        for args in [
            &["init", "-q", "-b", "master"][..],
            &["config", "user.email", "t@t"][..],
            &["config", "user.name", "t"][..],
            &["commit", "--allow-empty", "-q", "-m", "init"][..],
        ] {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        }
    }
}
