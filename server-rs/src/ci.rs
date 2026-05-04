//! CI/CD integration: trigger pipelines after merge and report status.
//!
//! When a task branch is merged into its source branch, we push the source
//! branch to `origin` so a configured CI provider (GitHub Actions) picks up
//! the change. We record a `ci_runs` row with the merged SHA and a background
//! poller asks `gh` for status, updates the row, and broadcasts changes.
//!
//! Best-effort everywhere: no `gh`, no remote, no `.github/workflows`, or auth
//! failures all degrade to silently doing nothing. Merges still work.
//!
//! Status vocabulary exposed to the dashboard:
//!   pending | running | success | failure | cancelled | unknown

pub mod aggregate;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rusqlite::params;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::db::Db;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::{GhRun, WireMessage};
use crate::saas::runner_rpc::{RunnerRpcError, runner_request_with_registry};
use crate::saas::runner_ws::{RunnerRegistry, RunnerResponse};
use crate::ws::broadcast_event;

const POLL_INTERVAL_SECS: u64 = 30;
/// Stop polling a run after this long if we never got a terminal status back —
/// avoids polling forever for a commit that never triggered a workflow.
const MAX_RUN_AGE_SECS: i64 = 30 * 60;

// ── Detection helpers ───────────────────────────────────────────────────────

fn has_github_actions(cwd: &Path) -> bool {
    let workflows = cwd.join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&workflows) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .is_some_and(|x| x == "yml" || x == "yaml")
    })
}

fn has_remote(cwd: &Path, name: &str) -> bool {
    Command::new("git")
        .args(["remote", "get-url", name])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Decide whether `trigger_after_merge` should record a CI
/// run for a merge that landed on `target`. Pure — caller
/// resolves `default_branch` (locally or via runner RPC) and
/// passes it in.
///
/// Rule: only the canonical default branch is treated as
/// CI-watched. A merge to anything else (dropdown override,
/// stacked branch) doesn't get a row — the workflow won't
/// fire on push, so the row would just sit at "pending"
/// until MAX_RUN_AGE_SECS ages it out to "unknown".
fn should_record_ci_run(target: &str, default_branch: Option<&str>) -> bool {
    default_branch == Some(target)
}

// ── Push + record ───────────────────────────────────────────────────────────

/// Kick off CI for a just-merged task: push to `origin/<branch>`, record a
/// pending `ci_runs` row, and broadcast the initial state. Swallows all
/// failures with log lines so a merge never appears broken because of CI.
/// All inputs needed to fire a CI pipeline after a merge. Grouped because
/// clippy (correctly) doesn't love nine free-floating positional args.
pub struct TriggerArgs {
    pub db: Db,
    /// Used by [`crate::agents::git_ops::default_branch`] +
    /// [`crate::agents::git_ops::push_branch`] to dispatch through the
    /// runner in SaaS mode. Always-present even in standalone (where it's
    /// an empty registry).
    pub runners: RunnerRegistry,
    pub org_id: String,
    pub broadcast_tx: broadcast::Sender<String>,
    pub cwd: PathBuf,
    pub plan_name: String,
    pub task_number: String,
    pub agent_id: String,
    pub source_branch: String,
    pub task_branch: String,
    pub merged_sha: String,
}

pub async fn trigger_after_merge(args: TriggerArgs) {
    let TriggerArgs {
        db,
        runners,
        org_id,
        broadcast_tx,
        cwd,
        plan_name,
        task_number,
        agent_id,
        source_branch,
        task_branch,
        merged_sha,
    } = args;

    // In standalone mode, gate on local-fs detection (workflow + origin).
    // In SaaS mode, the cwd lives on the runner — these checks would
    // always return false on the SaaS server's filesystem, so we skip
    // them and trust the runner-side push to fail noisily if there's no
    // remote. Workflow absence is harmless: the ci_runs row would just
    // age out to "unknown".
    let saas_mode = org_has_runner(&db, &org_id);
    if !saas_mode {
        if !has_github_actions(&cwd) {
            return;
        }
        if !has_remote(&cwd, "origin") {
            println!(
                "[ci] no origin remote in {} — skipping CI trigger",
                cwd.display()
            );
            return;
        }
    }

    let default = match crate::agents::git_ops::default_branch(&db, &runners, &org_id, &cwd).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[ci] default-branch dispatch failed: {e} — skipping CI trigger");
            return;
        }
    };
    if !should_record_ci_run(&source_branch, default.as_deref()) {
        println!(
            "[ci] merge target `{source_branch}` is not the default \
             branch ({default:?}) — skipping push + ci_runs insert"
        );
        return;
    }

    // Push the source branch so CI on the remote can fire.
    let push =
        crate::agents::git_ops::push_branch(&db, &runners, &org_id, &cwd, &source_branch).await;
    match push {
        Ok(Ok(())) => {
            println!("[ci] pushed {source_branch} ({merged_sha}) to origin");
        }
        Ok(Err(stderr)) => {
            eprintln!("[ci] push failed for {source_branch}: {stderr}");
            return;
        }
        Err(e) => {
            eprintln!("[ci] push dispatch failed for {source_branch}: {e}");
            return;
        }
    }

    // Record pending row.
    let run_id = {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_runs \
               (plan_name, task_number, agent_id, provider, commit_sha, branch, status, org_id) \
             VALUES (?1, ?2, ?3, 'github', ?4, ?5, 'pending', ?6)",
            params![
                plan_name,
                task_number,
                agent_id,
                merged_sha,
                task_branch,
                org_id
            ],
        )
        .ok();
        conn.last_insert_rowid()
    };

    broadcast_event(
        &broadcast_tx,
        "ci_status_changed",
        serde_json::json!({
            "id": run_id,
            "plan_name": plan_name,
            "task_number": task_number,
            "status": "pending",
            "commit_sha": merged_sha,
            "run_url": serde_json::Value::Null,
            "run_id": run_id,
        }),
    );
}

// ── Polling ─────────────────────────────────────────────────────────────────

/// Normalize GitHub Actions status + conclusion to our vocabulary.
fn normalize(status: Option<&str>, conclusion: Option<&str>) -> &'static str {
    match status {
        Some("queued") | Some("waiting") | Some("pending") | Some("requested") => "pending",
        Some("in_progress") => "running",
        Some("completed") => match conclusion {
            Some("success") => "success",
            Some("failure")
            | Some("timed_out")
            | Some("startup_failure")
            | Some("action_required") => "failure",
            Some("cancelled") | Some("skipped") | Some("neutral") | Some("stale") => "cancelled",
            _ => "unknown",
        },
        _ => "pending",
    }
}

fn terminal(status: &str) -> bool {
    matches!(status, "success" | "failure" | "cancelled" | "unknown")
}

/// Ask `gh` for the most recent workflow run against a given commit.
/// Runs in a blocking thread because `Command` is sync.
///
/// Local (standalone) implementation. The SaaS path goes through
/// [`fetch_run`] which dispatches via the runner. Implementation lives in
/// `crate::git_helpers::gh_run_list_local` so the runner binary reuses it.
async fn fetch_run_local(cwd: PathBuf, sha: String) -> Option<GhRun> {
    tokio::task::spawn_blocking(move || crate::git_helpers::gh_run_list_local(&cwd, &sha))
        .await
        .ok()
        .flatten()
}

/// Mode-aware dispatcher for [`fetch_run_local`].
///
/// - SaaS path (org has any `runners` row): dispatch
///   [`WireMessage::GhRunList`] to a connected runner. The poll cadence
///   is ~30s and the next pass will retry, so a longer-than-read timeout
///   is fine.
/// - Standalone: shell out via [`fetch_run_local`].
///
/// Outer `Result` is `Err` only when the SaaS path failed to reach the
/// runner (caller logs and skips this pass — does NOT age out the row).
/// Inner `Option<GhRun>` is `None` when no workflow has fired yet for
/// the commit, or `gh` is unavailable on the runner.
pub async fn fetch_run(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    cwd: &Path,
    sha: &str,
) -> Result<Option<GhRun>, RunnerRpcError> {
    if org_has_runner(db, org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::GhRunList {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
            sha: sha.to_string(),
        };
        match runner_request_with_registry(db, runners, org_id, msg, Duration::from_secs(15))
            .await?
        {
            RunnerResponse::GhRunListed(run) => Ok(run),
            other => {
                eprintln!("[ci] expected gh_run_listed, got {other:?}");
                Err(RunnerRpcError::InvalidRequest)
            }
        }
    } else {
        Ok(fetch_run_local(cwd.to_path_buf(), sha.to_string()).await)
    }
}

/// One pass: look up every pending/running `ci_runs` row, query `gh`, and
/// update rows + broadcast when status changes. Rows older than
/// `MAX_RUN_AGE_SECS` with no success are marked `unknown` so the dashboard
/// doesn't show a permanent spinner for a commit that never kicked off CI.
///
/// SaaS-aware: each row's `org_id` is resolved by joining through
/// `agents.id` (`ci_runs.agent_id`), and the gh shell-out is dispatched
/// through the runner. A `RunnerRpcError::NoConnectedRunner` (or any other
/// transport failure) logs `runner offline, retrying` and **skips the row
/// without aging it** — a brief reconnect window must not flip rows to
/// `unknown` just because the runner was disconnected for a few minutes.
async fn poll_once(
    db: &Db,
    runners: &RunnerRegistry,
    broadcast_tx: &broadcast::Sender<String>,
    project_dirs: &std::collections::HashMap<String, PathBuf>,
) {
    // Snapshot open rows — hold the lock only briefly. Joining through
    // agents to pick up org_id; rows with NULL agent_id (legacy/manual
    // inserts) get NULL here and we skip them.
    struct Row {
        id: i64,
        plan_name: String,
        task_number: String,
        commit_sha: Option<String>,
        status: String,
        age_secs: i64,
        org_id: Option<String>,
    }
    let rows: Vec<Row> = {
        let conn = db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT c.id, c.plan_name, c.task_number, c.commit_sha, c.status, \
                    CAST(strftime('%s','now') - strftime('%s', c.created_at) AS INTEGER), \
                    a.org_id \
             FROM ci_runs c \
             LEFT JOIN agents a ON c.agent_id = a.id \
             WHERE c.status IN ('pending','running') \
             ORDER BY c.id ASC",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                plan_name: r.get(1)?,
                task_number: r.get(2)?,
                commit_sha: r.get(3)?,
                status: r.get(4)?,
                age_secs: r.get(5)?,
                org_id: r.get(6)?,
            })
        })
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    };

    if rows.is_empty() {
        return;
    }

    for row in rows {
        let Some(sha) = row.commit_sha.clone() else {
            continue;
        };
        let Some(cwd) = project_dirs.get(&row.plan_name).cloned() else {
            // Plan has no known project dir — age it out eventually.
            if row.age_secs > MAX_RUN_AGE_SECS {
                update_row(
                    db,
                    broadcast_tx,
                    row.id,
                    &row.plan_name,
                    &row.task_number,
                    "unknown",
                    None,
                    None,
                    None,
                );
            }
            continue;
        };
        // Default to 'default-org' for legacy rows where the JOIN returned
        // NULL — matches the column default that pre-multi-tenancy inserts
        // would have hit.
        let org_id = row.org_id.clone().unwrap_or_else(|| "default-org".into());

        let run = match fetch_run(db, runners, &org_id, &cwd, &sha).await {
            Ok(r) => r,
            Err(e) => {
                // Runner offline / timeout / disconnect — skip this pass
                // and try again next cycle. Crucially, do NOT age out the
                // row: the runner could be back in seconds and we'd lose
                // a real success/failure status.
                eprintln!(
                    "[ci] runner offline, retrying next cycle (plan={}, task={}): {e}",
                    row.plan_name, row.task_number
                );
                continue;
            }
        };
        match run {
            Some(r) => {
                let new_status = normalize(r.status.as_deref(), r.conclusion.as_deref());
                if new_status != row.status {
                    update_row(
                        db,
                        broadcast_tx,
                        row.id,
                        &row.plan_name,
                        &row.task_number,
                        new_status,
                        r.conclusion.as_deref(),
                        r.url.as_deref(),
                        r.database_id.map(|i| i.to_string()).as_deref(),
                    );
                }
            }
            None => {
                // No run found yet. If it's been too long, mark unknown.
                if row.age_secs > MAX_RUN_AGE_SECS {
                    update_row(
                        db,
                        broadcast_tx,
                        row.id,
                        &row.plan_name,
                        &row.task_number,
                        "unknown",
                        None,
                        None,
                        None,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_row(
    db: &Db,
    broadcast_tx: &broadcast::Sender<String>,
    id: i64,
    plan_name: &str,
    task_number: &str,
    status: &str,
    conclusion: Option<&str>,
    run_url: Option<&str>,
    run_id: Option<&str>,
) {
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE ci_runs SET status = ?1, conclusion = ?2, run_url = ?3, run_id = ?4, updated_at = datetime('now') \
             WHERE id = ?5",
            params![status, conclusion, run_url, run_id, id],
        )
        .ok();
    }
    broadcast_event(
        broadcast_tx,
        "ci_status_changed",
        serde_json::json!({
            "id": id,
            "plan_name": plan_name,
            "task_number": task_number,
            "status": status,
            "conclusion": conclusion,
            "run_url": run_url,
            "run_id": run_id,
        }),
    );
    let note = if terminal(status) { " (final)" } else { "" };
    println!("[ci] {plan_name}/{task_number} → {status}{note}");
}

/// Spawn the background poller. Runs forever; cancellation happens on process
/// exit. Safe to call once from main.
///
/// `has_gh()` is intentionally NOT a gate here: in SaaS deployments the gh
/// shell-out happens on the runner, not the server, so the server's `$PATH`
/// is irrelevant. Standalone deployments without `gh` installed still spin
/// the poller but every dispatch returns `Ok(None)` (the runner-less branch
/// of `fetch_run` calls `fetch_run_local` which fails fast on a missing gh
/// binary), so there's no harm.
pub fn spawn_poller(
    db: Db,
    runners: RunnerRegistry,
    broadcast_tx: broadcast::Sender<String>,
    plans_dir: PathBuf,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            // Resolve plan_name -> project dir by re-reading plan files. Cheap
            // enough given the poll cadence, and avoids caching that could go
            // stale when the user edits a plan's `project` field.
            let project_dirs = resolve_project_dirs(&plans_dir, &db);
            poll_once(&db, &runners, &broadcast_tx, &project_dirs).await;
        }
    });
}

/// Resolve the on-disk directory for a single plan. Mirrors the per-plan
/// lookup inside [`resolve_project_dirs`] but cheaper when the caller only
/// needs one plan — used by the failure-log endpoint.
pub fn project_dir_for(plans_dir: &Path, db: &Db, plan_name: &str) -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let override_project: Option<String> = {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT project FROM plan_project WHERE plan_name = ?1",
            rusqlite::params![plan_name],
            |r| r.get::<_, String>(0),
        )
        .ok()
    };
    if let Some(p) = override_project {
        return Some(home.join(p));
    }
    let summaries = crate::plan_parser::list_plans(plans_dir);
    summaries
        .into_iter()
        .find(|s| s.name == plan_name)
        .and_then(|s| s.project.map(|p| home.join(p)))
}

/// Fetch the failure log for a CI run via `gh run view --log-failed`.
/// Capped at ~8 KB (last slice — failures usually accumulate at the tail),
/// cached back into `ci_runs.failure_log` so repeat fetches don't re-hit
/// GitHub. Returns `None` when the run is still pending, the project has
/// no remote, `gh` isn't installed, or the SaaS runner is unreachable.
///
/// Mode-aware: SaaS deployments dispatch the actual `gh` shell-out to a
/// connected runner via [`WireMessage::GhFailureLog`]; standalone runs the
/// shell-out locally. Cache reads/writes always happen on the server.
pub async fn fetch_failure_log(
    db: &Db,
    runners: &RunnerRegistry,
    plans_dir: PathBuf,
    ci_run_id: i64,
) -> Option<String> {
    // Cache hit?
    let cached: Option<String> = {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT failure_log FROM ci_runs WHERE id = ?1",
            rusqlite::params![ci_run_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };
    if cached.is_some() {
        return cached;
    }

    // Lookup provider run_id + plan so we can shell out in the right cwd.
    let (provider_run_id, plan_name): (Option<String>, String) = {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT run_id, plan_name FROM ci_runs WHERE id = ?1",
            rusqlite::params![ci_run_id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
        )
        .ok()?
    };
    let run_id = provider_run_id?;
    let cwd = project_dir_for(&plans_dir, db, &plan_name)?;

    // Resolve the org_id by joining through agents — same JOIN-via-agent_id
    // pattern that poll_once uses, since ci_runs.org_id is left at its
    // 'default-org' default when trigger_after_merge inserts the row.
    let org_id = ci_run_org_id(db, ci_run_id)?;

    let log = if org_has_runner(db, &org_id) {
        let req_id = Uuid::new_v4().to_string();
        let msg = WireMessage::GhFailureLog {
            req_id,
            cwd: cwd.to_string_lossy().to_string(),
            run_id: run_id.clone(),
        };
        match runner_request_with_registry(db, runners, &org_id, msg, Duration::from_secs(30)).await
        {
            Ok(RunnerResponse::GhFailureLogFetched(log)) => log?,
            Ok(other) => {
                eprintln!("[ci] expected gh_failure_log_fetched, got {other:?}");
                return None;
            }
            Err(e) => {
                eprintln!("[ci] failure-log dispatch failed: {e}");
                return None;
            }
        }
    } else {
        fetch_failure_log_local(cwd, run_id).await?
    };

    // Write-through cache so the next call is free.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE ci_runs SET failure_log = ?1 WHERE id = ?2",
            rusqlite::params![log, ci_run_id],
        )
        .ok();
    }
    Some(log)
}

/// Local `gh run view --log-failed` shell-out. Tail-trimmed at 8 KB.
/// Implementation lives in `crate::git_helpers::gh_failure_log_local` so
/// the runner binary reuses it.
async fn fetch_failure_log_local(cwd: PathBuf, run_id: String) -> Option<String> {
    tokio::task::spawn_blocking(move || crate::git_helpers::gh_failure_log_local(&cwd, &run_id))
        .await
        .ok()
        .flatten()
}

/// Resolve `org_id` for a `ci_runs` row by joining through `agents.agent_id`.
/// Returns `None` when `ci_runs.agent_id` is NULL or the agent row is gone —
/// in that case the caller skips the row rather than guessing.
fn ci_run_org_id(db: &Db, ci_run_id: i64) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT a.org_id FROM ci_runs c INNER JOIN agents a ON c.agent_id = a.id \
         WHERE c.id = ?1",
        rusqlite::params![ci_run_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn resolve_project_dirs(plans_dir: &Path, db: &Db) -> std::collections::HashMap<String, PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let summaries = crate::plan_parser::list_plans(plans_dir);

    // DB overrides
    let mut overrides = std::collections::HashMap::new();
    {
        let conn = db.lock().unwrap();
        if let Ok(mut stmt) = conn.prepare("SELECT plan_name, project FROM plan_project")
            && let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for row in rows.flatten() {
                overrides.insert(row.0, row.1);
            }
        }
    }

    summaries
        .into_iter()
        .filter_map(|s| {
            let project = overrides.get(&s.name).cloned().or(s.project)?;
            Some((s.name, home.join(project)))
        })
        .collect()
}

// ── Public read helpers ─────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CiStatus {
    /// `ci_runs.id` — the dashboard's internal row id, needed by the
    /// failure-log / fix-CI endpoints so the frontend can refer back to
    /// this specific run without guessing.
    pub id: i64,
    pub status: String,
    pub conclusion: Option<String>,
    pub run_url: Option<String>,
    pub commit_sha: Option<String>,
    pub updated_at: String,
    /// Set when the picked CI row's `task_number` is `<task>-fix-<N>` rather
    /// than the canonical task number — i.e. a fix-attempt's CI is what's
    /// surfaced on the original task's badge. UI may render
    /// "green via fix attempt N". `None` for a row that belongs to the
    /// canonical task itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_fix_attempt: Option<u32>,
}

/// Latest CI run per task for the given plan, rolling up `<task>-fix-<N>`
/// attempts onto the canonical task. Caller passes the canonical task
/// numbers from the parsed plan; we don't infer them from the DB because
/// the canonical set lives in `plan.yaml`, not in `ci_runs`.
///
/// For each requested `task_number`, picks the latest non-dismissed row
/// whose `task_number` is either `?` exactly or `?-fix-*`. The explicit
/// `-fix-` infix in the GLOB means task `1.3` does not collide with
/// `1.30-fix-1` (different canonical task).
pub fn latest_per_task(
    conn: &rusqlite::Connection,
    plan_name: &str,
    task_numbers: &[&str],
) -> std::collections::HashMap<String, CiStatus> {
    // Per-task lookup. Cheap on SQLite (in-process, prepared once,
    // O(log N) on the (plan_name, task_number) index) and the alternative
    // — passing the canonical set into a single SQL — would mean either
    // a giant IN-list or a temp table per call.
    let mut stmt = match conn.prepare(
        "SELECT c.id, c.task_number, c.status, c.conclusion, c.run_url, c.commit_sha, c.updated_at \
         FROM ci_runs c \
         WHERE c.plan_name = ?1 \
           AND (c.task_number = ?2 OR c.task_number GLOB ?2 || '-fix-*') \
           AND c.dismissed_at IS NULL \
         ORDER BY c.id DESC \
         LIMIT 1",
    ) {
        Ok(s) => s,
        Err(_) => return Default::default(),
    };

    let mut out: std::collections::HashMap<String, CiStatus> =
        std::collections::HashMap::with_capacity(task_numbers.len());
    for &task_number in task_numbers {
        let row = stmt.query_row(params![plan_name, task_number], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        });
        if let Ok((id, picked, status, conclusion, run_url, commit_sha, updated_at)) = row {
            let via_fix_attempt = if picked == task_number {
                None
            } else {
                parse_fix_attempt_suffix(&picked, task_number)
            };
            out.insert(
                task_number.to_string(),
                CiStatus {
                    id,
                    status,
                    conclusion,
                    run_url,
                    commit_sha,
                    updated_at,
                    via_fix_attempt,
                },
            );
        }
    }
    out
}

/// Parse `<canonical>-fix-<N>` and return `N`. Returns `None` for unexpected
/// shapes (e.g. non-numeric suffix) — defensive against future task-number
/// schemes; the caller treats `None` as "row belongs to canonical task" and
/// would have already returned by then, so we only land here on real fix
/// matches.
fn parse_fix_attempt_suffix(picked: &str, canonical: &str) -> Option<u32> {
    let prefix = format!("{canonical}-fix-");
    let rest = picked.strip_prefix(&prefix)?;
    rest.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_covers_main_transitions() {
        assert_eq!(normalize(Some("queued"), None), "pending");
        assert_eq!(normalize(Some("in_progress"), None), "running");
        assert_eq!(normalize(Some("completed"), Some("success")), "success");
        assert_eq!(normalize(Some("completed"), Some("failure")), "failure");
        assert_eq!(normalize(Some("completed"), Some("cancelled")), "cancelled");
        assert_eq!(normalize(Some("completed"), Some("skipped")), "cancelled");
        assert_eq!(normalize(Some("completed"), Some("weird")), "unknown");
        assert_eq!(normalize(None, None), "pending");
    }

    #[test]
    fn terminal_matches_only_terminal_statuses() {
        assert!(terminal("success"));
        assert!(terminal("failure"));
        assert!(terminal("cancelled"));
        assert!(terminal("unknown"));
        assert!(!terminal("pending"));
        assert!(!terminal("running"));
    }

    #[test]
    fn should_record_ci_run_true_when_target_matches_default() {
        assert!(should_record_ci_run("master", Some("master")));
    }

    #[test]
    fn should_record_ci_run_false_when_default_is_different_canonical() {
        // Repo's canonical default is `main` but the merge landed on `master`
        // (e.g. a stale local branch). Do not record CI.
        assert!(!should_record_ci_run("master", Some("main")));
    }

    #[test]
    fn should_record_ci_run_false_for_non_default_target() {
        assert!(!should_record_ci_run("feature/x", Some("master")));
    }

    #[test]
    fn should_record_ci_run_false_when_default_unknown() {
        // No origin/HEAD and no master/main probe hit — be conservative and
        // skip the CI insert rather than guessing.
        assert!(!should_record_ci_run("master", None));
    }

    #[test]
    fn has_github_actions_detects_yml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        assert!(!has_github_actions(dir.path())); // empty
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        assert!(has_github_actions(dir.path()));
    }

    fn ci_runs_schema() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ci_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, \
              plan_name TEXT, task_number TEXT, status TEXT, conclusion TEXT, \
              run_url TEXT, commit_sha TEXT, updated_at TEXT DEFAULT (datetime('now')), \
              dismissed_at TEXT);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn latest_per_task_returns_most_recent_row() {
        let conn = ci_runs_schema();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.1','pending')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO ci_runs (plan_name, task_number, status, run_url, commit_sha) VALUES ('p','1.1','success','https://x','abc')", []).unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.2','failure')",
            [],
        )
        .unwrap();

        let got = latest_per_task(&conn, "p", &["1.1", "1.2"]);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("1.1").unwrap().status, "success");
        assert_eq!(
            got.get("1.1").unwrap().run_url.as_deref(),
            Some("https://x")
        );
        assert_eq!(got.get("1.1").unwrap().via_fix_attempt, None);
        assert_eq!(got.get("1.2").unwrap().status, "failure");
    }

    #[test]
    fn latest_per_task_rolls_up_fix_attempt_onto_canonical_task() {
        // Original CI failed on task 1.3, then a fix-1 attempt landed green.
        // The badge for canonical task 1.3 should surface the fix-1 row.
        let conn = ci_runs_schema();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status, conclusion, commit_sha) \
             VALUES ('p','1.3','failure','failure','sha-orig')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status, conclusion, commit_sha) \
             VALUES ('p','1.3-fix-1','success','success','sha-fix1')",
            [],
        )
        .unwrap();

        let got = latest_per_task(&conn, "p", &["1.3"]);
        let ci = got.get("1.3").expect("rollup must populate canonical task");
        assert_eq!(ci.status, "success");
        assert_eq!(ci.conclusion.as_deref(), Some("success"));
        assert_eq!(ci.commit_sha.as_deref(), Some("sha-fix1"));
        assert_eq!(ci.via_fix_attempt, Some(1));
    }

    #[test]
    fn latest_per_task_does_not_collide_across_similarly_numbered_tasks() {
        // Task 1.3 has a failing run. Separately, task 1.30 has a green
        // fix-1 attempt — its row's task_number is `1.30-fix-1`, which
        // must NOT match `1.3-fix-*` (the explicit `-fix-` infix in the
        // GLOB pattern is what enforces this). Projecting task 1.3 must
        // keep returning the failure, not leak in the unrelated 1.30
        // success.
        let conn = ci_runs_schema();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status, conclusion, commit_sha) \
             VALUES ('p','1.3','failure','failure','sha-orig')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status, conclusion, commit_sha) \
             VALUES ('p','1.30-fix-1','success','success','sha-30fix1')",
            [],
        )
        .unwrap();

        let got = latest_per_task(&conn, "p", &["1.3"]);
        let ci = got.get("1.3").expect("task 1.3 must keep its own row");
        assert_eq!(ci.status, "failure");
        assert_eq!(ci.commit_sha.as_deref(), Some("sha-orig"));
        assert_eq!(ci.via_fix_attempt, None);
    }

    #[test]
    fn latest_per_task_picks_highest_fix_attempt() {
        // Three rows: original failure, fix-1 failure, fix-2 success.
        // The id-DESC + LIMIT 1 picks the latest by insertion order, which
        // (with AUTOINCREMENT) is the fix-2 row. via_fix_attempt = 2.
        let conn = ci_runs_schema();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.3','failure')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.3-fix-1','failure')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.3-fix-2','success')",
            [],
        )
        .unwrap();

        let got = latest_per_task(&conn, "p", &["1.3"]);
        let ci = got.get("1.3").unwrap();
        assert_eq!(ci.status, "success");
        assert_eq!(ci.via_fix_attempt, Some(2));
    }

    #[test]
    fn latest_per_task_skips_dismissed_fix_rows() {
        // A green fix-1 run that's been dismissed must not surface on the
        // canonical task — the lookup falls back to the next non-dismissed
        // row, which here is the original failure.
        let conn = ci_runs_schema();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status) VALUES ('p','1.3','failure')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, status, dismissed_at) \
             VALUES ('p','1.3-fix-1','success', datetime('now'))",
            [],
        )
        .unwrap();

        let got = latest_per_task(&conn, "p", &["1.3"]);
        let ci = got.get("1.3").unwrap();
        assert_eq!(ci.status, "failure");
        assert_eq!(ci.via_fix_attempt, None);
    }

    #[test]
    fn parse_fix_attempt_suffix_handles_well_formed_and_malformed() {
        assert_eq!(parse_fix_attempt_suffix("1.3-fix-1", "1.3"), Some(1));
        assert_eq!(parse_fix_attempt_suffix("1.3-fix-42", "1.3"), Some(42));
        // Wrong canonical: rejected.
        assert_eq!(parse_fix_attempt_suffix("1.30-fix-1", "1.3"), None);
        // Non-numeric suffix: defensive None.
        assert_eq!(parse_fix_attempt_suffix("1.3-fix-x", "1.3"), None);
        // No suffix: defensive None.
        assert_eq!(parse_fix_attempt_suffix("1.3", "1.3"), None);
    }
}
