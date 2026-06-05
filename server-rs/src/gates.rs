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
/// `node` supplies the gate kind, the declared `checks`, and (for CI gates)
/// the declared `workflows`. Side effects are limited to broadcasting the
/// `gate_ready_for_approval` / `gate_awaiting_approval` WS events — node
/// status writes are the scheduler's job.
pub async fn execute_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    match node.gate_kind {
        Some(GateKind::Init) => execute_init_gate(state, plan_name, node).await,
        Some(GateKind::End) => execute_end_gate(state, plan_name, node).await,
        Some(GateKind::Ci) => execute_ci_gate(state, plan_name, node).await,
        Some(GateKind::Approval) => execute_approval_gate(state, plan_name, node).await,
        None => GateOutcome::Failed(format!("gate node '{}' has no gate_kind declared", node.id)),
    }
}

// ── Init gate ────────────────────────────────────────────────────────────

async fn execute_init_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    // Pre-approved out of band (e.g. operator approved before the gate was
    // first reached) ⇒ pass straight through.
    if gate_approved(&state.db, plan_name, &node.id) {
        return GateOutcome::Passed;
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
            "node_id": node.id,
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
            "node_id": node.id,
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

async fn execute_end_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name);

    let checks: Vec<String> = if node.checks.is_empty() {
        default_end_checks().iter().map(|s| s.to_string()).collect()
    } else {
        node.checks.clone()
    };

    // Run checks in order; a `Failed` short-circuits, a `Blocked` is
    // remembered and returned after the rest (so e.g. an unmerged branch is
    // reported before we ever wait on CI).
    let mut blocked: Option<String> = None;
    for check in &checks {
        let outcome = match check.as_str() {
            "all_merged" => check_all_merged(&state.db, plan_name),
            "compiles" => check_compiles(project_dir.as_deref()).await,
            "ci_green" => check_ci_green(state, plan_name, project_dir.as_deref(), &[]).await,
            other => {
                eprintln!("[gates] end gate: unknown check '{other}' — skipping");
                GateOutcome::Passed
            }
        };
        match outcome {
            GateOutcome::Passed => {}
            GateOutcome::Failed(reason) => return GateOutcome::Failed(reason),
            GateOutcome::Blocked(reason) => blocked = Some(reason),
        }
    }

    match blocked {
        Some(reason) => GateOutcome::Blocked(reason),
        None => GateOutcome::Passed,
    }
}

/// `all_merged`: every branch produced by the plan's agents has been merged
/// (i.e. `agents.branch` cleared to NULL by the merge path). Any non-NULL
/// branch is unmerged work that must land before the plan is "done".
fn check_all_merged(db: &Db, plan_name: &str) -> GateOutcome {
    let unmerged = unmerged_branches(db, plan_name);
    if unmerged.is_empty() {
        GateOutcome::Passed
    } else {
        GateOutcome::Failed(format!(
            "all_merged: {} unmerged branch(es): {}",
            unmerged.len(),
            unmerged.join(", ")
        ))
    }
}

/// `compiles`: run the project's `[auto_mode.pre_merge_checks]` against the
/// merged default-branch working tree. Opt-in — no config (or no project)
/// ⇒ pass.
async fn check_compiles(project_dir: Option<&Path>) -> GateOutcome {
    let Some(project_dir) = project_dir else {
        return GateOutcome::Passed;
    };
    let Some(repo_cfg) = crate::repo_config::load_for_project_dir(project_dir) else {
        return GateOutcome::Passed;
    };
    let checks = &repo_cfg.auto_mode.pre_merge_checks;
    if checks.is_empty() {
        return GateOutcome::Passed;
    }
    let total_timeout = Duration::from_secs(repo_cfg.auto_mode.pre_merge_total_timeout_secs as u64);
    match crate::auto_mode::run_pre_merge_checks_in(checks, project_dir, total_timeout).await {
        crate::auto_mode::GateOutcome::Pass => GateOutcome::Passed,
        crate::auto_mode::GateOutcome::Fail {
            check, exit_code, ..
        } => GateOutcome::Failed(format!(
            "compiles: check '{check}' failed (exit {})",
            exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "killed".to_string())
        )),
    }
}

// ── CI gate ──────────────────────────────────────────────────────────────

async fn execute_ci_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, plan_name);
    check_ci_green(state, plan_name, project_dir.as_deref(), &node.workflows).await
}

/// Poll GitHub Actions on the project HEAD and map the aggregate to a gate
/// outcome. Reuses [`crate::saas::dispatch::get_ci_run_status_dispatch`]
/// (mode-aware: SaaS dispatches to the runner, standalone shells out to
/// `gh`).
///
/// `declared` filters the aggregate to a specific set of workflows (the CI
/// gate's `workflows:`); empty ⇒ evaluate the full blocking subset.
///
/// A project with no `.github/workflows` is vacuously green (nothing to
/// wait for). No CI runs yet for the HEAD SHA ⇒ `Blocked` (pending). A
/// transient dispatch error ⇒ `Blocked` (retry next tick).
async fn check_ci_green(
    state: &AppState,
    plan_name: &str,
    project_dir: Option<&Path>,
    declared: &[String],
) -> GateOutcome {
    let Some(project_dir) = project_dir else {
        // No project dir resolvable on the server (SaaS, or unconfigured) —
        // can't probe workflows locally. Treat as vacuously green so a plan
        // without server-visible CI isn't stuck forever.
        return GateOutcome::Passed;
    };
    if !crate::ci::has_github_actions(project_dir) {
        return GateOutcome::Passed;
    }
    let Some(sha) = crate::agents::git_head_sha(project_dir) else {
        return GateOutcome::Blocked("ci_green: cannot resolve project HEAD".to_string());
    };
    let org = org_for_plan(&state.db, plan_name);
    // `task_number` is wire-only correlation metadata for the dispatch; a
    // gate isn't a task, so pass an empty marker.
    match crate::saas::dispatch::get_ci_run_status_dispatch(state, &org, plan_name, "", &sha).await
    {
        Ok(Some(agg)) => evaluate_ci_aggregate(&agg, declared, &sha),
        Ok(None) => GateOutcome::Blocked(format!("ci_green: no CI runs yet for {sha}")),
        Err(e) => GateOutcome::Blocked(format!("ci_green: CI status unavailable ({e})")),
    }
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

async fn execute_approval_gate(state: &AppState, plan_name: &str, node: &DagNode) -> GateOutcome {
    if gate_approved(&state.db, plan_name, &node.id) {
        return GateOutcome::Passed;
    }
    broadcast_event(
        &state.broadcast_tx,
        "gate_awaiting_approval",
        json!({
            "plan_name": plan_name,
            "node_id": node.id,
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
        let outcome = execute_gate(&state, "p", &init_node()).await;

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
        let outcome = execute_gate(&state, "p", &end_node()).await;

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
        let outcome = execute_gate(&state, "p", &init_node()).await;
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
        let outcome = execute_gate(&state, "p", &init_node()).await;
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
        let outcome = execute_gate(&state, "p", &init_node()).await;
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
        let outcome = execute_gate(&state, "p", &end_node()).await;
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
        let outcome = execute_gate(&state, "p", &end_node()).await;
        match outcome {
            GateOutcome::Failed(reason) => {
                assert!(reason.contains("compiles"), "got {reason}");
                assert!(reason.contains("build"), "got {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── Approval gate: blocks + broadcasts awaiting-approval ──────────────

    #[tokio::test]
    async fn approval_gate_blocks_and_broadcasts() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, mut rx) = test_state(db, plans_dir);

        let outcome = execute_gate(&state, "p", &approval_node()).await;
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
        let outcome = execute_gate(&state, "p", &approval_node()).await;
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
}
