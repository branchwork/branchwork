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
pub mod resolution;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rusqlite::params;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::db::Db;
use crate::saas::dispatch::{CiStatusError, get_ci_run_status_dispatch, org_has_runner};
use crate::saas::runner_protocol::WireMessage;
use crate::saas::runner_rpc::runner_request_with_registry;
use crate::saas::runner_ws::{RunnerRegistry, RunnerResponse};
use crate::state::AppState;
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

/// One pass: look up every pending/running `ci_runs` row, ask the
/// aggregate-aware dispatch for the per-SHA verdict, and update rows +
/// broadcast when status changes. Rows older than `MAX_RUN_AGE_SECS` with
/// no success are marked `unknown` so the dashboard doesn't show a
/// permanent spinner for a commit that never kicked off CI.
///
/// Why aggregate-aware: a SHA can have multiple workflow runs (CI, Docker,
/// CodeQL, …). The legacy single-run path used `gh run list -L 1`, which
/// returned whichever run gh sorted to the top — often a passing Docker
/// build while the blocking CI run had failed. The dispatch path applies
/// the per-plan blocking-workflow filter (levels 1-4) and returns a
/// [`crate::saas::runner_protocol::CiAggregate`] whose `conclusion`
/// reflects only the blocking subset and whose `failing_run_id`
/// deep-links to the chronologically-earliest blocking failure (the
/// root cause).
///
/// SaaS-aware: each row's `org_id` is resolved by joining through
/// `agents.id` (`ci_runs.agent_id`), and the gh shell-out is dispatched
/// through the runner. A [`CiStatusError::Rpc`] logs `runner offline,
/// retrying` and **skips the row without aging it** — a brief reconnect
/// window must not flip rows to `unknown` just because the runner was
/// disconnected for a few minutes. [`CiStatusError::InvalidResponse`] is
/// programmer error / schema drift; we log and skip too.
async fn poll_once(state: &AppState, project_dirs: &std::collections::HashMap<String, PathBuf>) {
    let db = &state.db;
    let broadcast_tx = &state.broadcast_tx;

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

        let aggregate = match get_ci_run_status_dispatch(
            state,
            &org_id,
            &row.plan_name,
            &row.task_number,
            &sha,
        )
        .await
        {
            Ok(a) => a,
            Err(CiStatusError::Rpc(e)) => {
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
            Err(CiStatusError::InvalidResponse) => {
                // Schema drift / programmer error: skip this pass. Do not
                // age out — the next cycle may catch up.
                eprintln!(
                    "[ci] dispatch returned an unexpected reply variant (plan={}, task={})",
                    row.plan_name, row.task_number
                );
                continue;
            }
        };

        match aggregate {
            Some(agg) => {
                let new_status = normalize(Some(&agg.status), agg.conclusion.as_deref());
                if new_status != row.status {
                    let run_url = agg
                        .failing_run_id
                        .as_deref()
                        .and_then(|id| derive_run_url(&cwd, id));
                    update_row(
                        db,
                        broadcast_tx,
                        row.id,
                        &row.plan_name,
                        &row.task_number,
                        new_status,
                        agg.conclusion.as_deref(),
                        run_url.as_deref(),
                        agg.failing_run_id.as_deref(),
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

/// Build `https://github.com/<owner>/<repo>/actions/runs/<run_id>` from a
/// local checkout's `origin` remote. Returns `None` when the remote is
/// missing, isn't a github URL, or the cwd doesn't exist (typical SaaS
/// path — `cwd` is the runner's filesystem, not the server's). The
/// dashboard already handles `run_url == None` so this is a soft signal,
/// not a hard error.
fn derive_run_url(cwd: &Path, run_id: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    let (owner, repo) = parse_github_owner_repo(url.trim())?;
    Some(format!(
        "https://github.com/{owner}/{repo}/actions/runs/{run_id}"
    ))
}

/// Parse `<owner>/<repo>` out of a github clone URL. Handles the common
/// shapes: `git@github.com:o/r.git`, `https://github.com/o/r.git`,
/// `https://github.com/o/r`, and the `git://` form. Returns `None` for
/// non-github remotes (gitlab, self-hosted) — the run_url derivation is
/// best-effort, and a None falls through to the existing dashboard
/// "no link" rendering.
fn parse_github_owner_repo(url: &str) -> Option<(&str, &str)> {
    // Strip the host prefix in any of: git@host:, https://host/, git://host/, ssh://host/
    let without_host = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;

    // Trim trailing `.git` and any trailing slashes/whitespace.
    let trimmed = without_host
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = trimmed.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
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
/// of `get_ci_run_status_dispatch` shells out via `gh_run_list_full_local`
/// which fails fast on a missing `gh` binary), so there's no harm.
pub fn spawn_poller(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            // Resolve plan_name -> project dir by re-reading plan files. Cheap
            // enough given the poll cadence, and avoids caching that could go
            // stale when the user edits a plan's `project` field.
            let project_dirs = resolve_project_dirs(&state.plans_dir, &state.db);
            poll_once(&state, &project_dirs).await;
        }
    });
}

// ── Backfill ────────────────────────────────────────────────────────────────

/// Settings flag set after the first aggregate-aware backfill pass; further
/// boots short-circuit on this without re-polling. Bump the suffix (`_v2_`,
/// …) only when a future schema change requires re-running the backfill on
/// already-migrated databases.
const BACKFILL_GATE_KEY: &str = "ci_backfill_v1_done";

/// Spawn a one-shot startup backfill that re-evaluates every `ci_runs` row
/// with a SHA against the aggregate-aware dispatch path introduced in T1.3.
/// Detached via `tokio::spawn` so the HTTP listener readiness probe is not
/// blocked: the backfill races against the first poller tick, and either
/// completes within the first 30 s window or trickles in alongside live
/// polling work.
///
/// Idempotent and gated by [`BACKFILL_GATE_KEY`] in the `settings` table —
/// the second boot logs `[ci-backfill] already applied, skipping` and returns.
pub fn spawn_backfill(state: AppState) {
    tokio::spawn(async move {
        backfill_aggregates(state).await;
    });
}

/// Re-poll every `ci_runs` row whose `commit_sha IS NOT NULL` against the
/// aggregate-aware dispatch (T1.3) so legacy single-run-path verdicts that
/// stored a passing Docker badge over a failing CI run get flipped to the
/// correct aggregate verdict. Logs one line per row (even no-ops) so an
/// operator can tell at a glance how many rows flipped, then sets the
/// [`BACKFILL_GATE_KEY`] settings row so subsequent boots skip the routine.
///
/// Per-row failures (runner offline, malformed reply, missing aggregate)
/// leave the row untouched and are logged. The gate is set unconditionally
/// at the end of the pass — re-running after a transient outage requires
/// flipping the flag manually (or wiping the row). This trade-off is
/// documented in the task description: the goal is one backfill per
/// database, not one backfill per online runner.
pub async fn backfill_aggregates(state: AppState) {
    if backfill_gate_is_set(&state.db) {
        eprintln!("[ci-backfill] already applied, skipping");
        return;
    }

    let project_dirs = resolve_project_dirs(&state.plans_dir, &state.db);

    struct Row {
        id: i64,
        plan_name: String,
        task_number: String,
        commit_sha: String,
        org_id: Option<String>,
        old_status: String,
        old_run_id: Option<String>,
    }
    let rows: Vec<Row> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT c.id, c.plan_name, c.task_number, c.commit_sha, a.org_id, \
                    c.status, c.run_id \
             FROM ci_runs c \
             LEFT JOIN agents a ON c.agent_id = a.id \
             WHERE c.commit_sha IS NOT NULL \
             ORDER BY c.id ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[ci-backfill] prepare failed: {e}");
                return;
            }
        };
        stmt.query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                plan_name: r.get(1)?,
                task_number: r.get(2)?,
                commit_sha: r.get(3)?,
                org_id: r.get(4)?,
                old_status: r.get(5)?,
                old_run_id: r.get(6)?,
            })
        })
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    };

    let total = rows.len();
    eprintln!("[ci-backfill] starting: {total} rows to re-poll");
    let mut flipped = 0usize;
    let mut offline = 0usize;

    for row in rows {
        // JOIN may have returned NULL when the agent row is gone or when
        // ci_runs.agent_id was never set (legacy/manual inserts). Match
        // poll_once's behaviour by defaulting to `default-org`.
        let org_id = row.org_id.clone().unwrap_or_else(|| "default-org".into());

        let aggregate = match get_ci_run_status_dispatch(
            &state,
            &org_id,
            &row.plan_name,
            &row.task_number,
            &row.commit_sha,
        )
        .await
        {
            Ok(a) => a,
            Err(CiStatusError::Rpc(e)) => {
                // Runner offline / timeout — leave the row alone. The next
                // run only happens if someone manually flips the gate, so
                // log loudly enough that an operator can spot the gap.
                eprintln!(
                    "[ci-backfill] {}/{}: runner offline, leaving row untouched: {e}",
                    row.plan_name, row.task_number
                );
                offline += 1;
                continue;
            }
            Err(CiStatusError::InvalidResponse) => {
                eprintln!(
                    "[ci-backfill] {}/{}: dispatch returned an unexpected reply variant",
                    row.plan_name, row.task_number
                );
                continue;
            }
        };

        let Some(agg) = aggregate else {
            // No runs found for the SHA. Either the legacy row pointed at a
            // SHA that never triggered CI, or `gh` is unavailable. Nothing
            // to write; log so the operator can see the no-op.
            eprintln!(
                "[ci-backfill] {}/{}: {}:{} → no_runs",
                row.plan_name,
                row.task_number,
                row.old_status,
                row.old_run_id.as_deref().unwrap_or("-"),
            );
            continue;
        };

        let new_status = normalize(Some(&agg.status), agg.conclusion.as_deref());
        let new_run_id = agg.failing_run_id.as_deref();
        let run_url = new_run_id.and_then(|id| {
            project_dirs
                .get(&row.plan_name)
                .and_then(|d| derive_run_url(d, id))
        });

        let changed = new_status != row.old_status || new_run_id != row.old_run_id.as_deref();
        eprintln!(
            "[ci-backfill] {}/{}: {}:{} → {}:{}{}",
            row.plan_name,
            row.task_number,
            row.old_status,
            row.old_run_id.as_deref().unwrap_or("-"),
            new_status,
            new_run_id.unwrap_or("-"),
            if changed { " (flipped)" } else { "" },
        );
        if changed {
            flipped += 1;
        }

        // Unconditionally write back: even when `new_status` matches the
        // stored value, `run_url` and `run_id` may have changed (the
        // canonical case is a stale Docker URL on a previously-stored
        // green that is actually a CI failure on the same SHA).
        update_row(
            &state.db,
            &state.broadcast_tx,
            row.id,
            &row.plan_name,
            &row.task_number,
            new_status,
            agg.conclusion.as_deref(),
            run_url.as_deref(),
            new_run_id,
        );
    }

    set_backfill_gate(&state.db);
    eprintln!(
        "[ci-backfill] done: {flipped}/{total} rows flipped, {offline} skipped (runner offline)"
    );
}

fn backfill_gate_is_set(db: &Db) -> bool {
    let conn = db.lock().unwrap();
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![BACKFILL_GATE_KEY],
            |r| r.get(0),
        )
        .ok();
    matches!(v.as_deref(), Some("1"))
}

fn set_backfill_gate(db: &Db) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, '1') \
         ON CONFLICT(key) DO UPDATE SET value = '1'",
        params![BACKFILL_GATE_KEY],
    )
    .ok();
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

    #[test]
    fn parse_github_owner_repo_accepts_common_url_forms() {
        // git@host SSH alias form (default for `gh repo clone`).
        assert_eq!(
            parse_github_owner_repo("git@github.com:rust-lang/rust.git"),
            Some(("rust-lang", "rust"))
        );
        // HTTPS clone URL with .git suffix.
        assert_eq!(
            parse_github_owner_repo("https://github.com/cep/cep.git"),
            Some(("cep", "cep"))
        );
        // HTTPS without .git suffix (browser address-bar copy).
        assert_eq!(
            parse_github_owner_repo("https://github.com/cep/cep"),
            Some(("cep", "cep"))
        );
        // git:// (read-only) form.
        assert_eq!(
            parse_github_owner_repo("git://github.com/cep/cep.git"),
            Some(("cep", "cep"))
        );
        // Trailing slash.
        assert_eq!(
            parse_github_owner_repo("https://github.com/cep/cep/"),
            Some(("cep", "cep"))
        );
    }

    #[test]
    fn parse_github_owner_repo_rejects_non_github_remotes() {
        assert_eq!(parse_github_owner_repo("https://gitlab.com/cep/cep"), None);
        assert_eq!(
            parse_github_owner_repo("git@bitbucket.org:cep/cep.git"),
            None
        );
        // Empty owner / repo.
        assert_eq!(parse_github_owner_repo("https://github.com//"), None);
        assert_eq!(parse_github_owner_repo("https://github.com/cep"), None);
    }

    #[test]
    fn derive_run_url_uses_local_origin_remote() {
        // Set up a real-but-tiny git repo with an origin remote we control.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["remote", "add", "origin", "https://github.com/cep/cep.git"]);

        let url = derive_run_url(cwd, "12345").unwrap();
        assert_eq!(url, "https://github.com/cep/cep/actions/runs/12345");
    }

    #[test]
    fn derive_run_url_returns_none_for_missing_remote() {
        // Bare directory — `git remote get-url origin` exits non-zero.
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(derive_run_url(tmp.path(), "999"), None);
    }

    #[test]
    fn derive_run_url_returns_none_for_non_github_remote() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["remote", "add", "origin", "https://gitlab.com/cep/cep.git"]);
        assert_eq!(derive_run_url(cwd, "999"), None);
    }

    // ── Multi-workflow regression: poll_once writes the CI failure, not
    // the Docker success. Live evidence cep sha
    // 12d31ceb6a9a93d906a017a9a7a85b269361ab91 had Docker:success +
    // CI:failure on the same SHA; the legacy `gh run list -L 1` path
    // stored Docker. The aggregate-aware path stores CI.

    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Instant;

    use rusqlite::Connection;
    use tokio::sync::{Mutex, mpsc, oneshot};

    use crate::saas::runner_protocol::{CiAggregate, CiRunSummary, Envelope};
    use crate::saas::runner_ws::{ConnectedRunner, new_runner_registry};

    /// Seed a multi-tenant SQLite DB with the schema poll_once + the
    /// dispatch shim need: agents (org_id, cwd), ci_runs (with
    /// commit_sha + status), runners (online row makes
    /// org_has_runner true). Also creates the `settings` table that
    /// `backfill_aggregates` reads/writes its idempotency gate from.
    fn poll_test_db(org_id: &str, runner_id: &str, plan_name: &str, sha: &str) -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, name TEXT, org_id TEXT, status TEXT, \
               hostname TEXT, version TEXT, last_seen_at TEXT, \
               created_at TEXT \
             ); \
             CREATE TABLE agents ( \
               id TEXT PRIMARY KEY, plan_name TEXT, task_id TEXT, \
               org_id TEXT, cwd TEXT \
             ); \
             CREATE TABLE ci_runs ( \
               id INTEGER PRIMARY KEY AUTOINCREMENT, plan_name TEXT, \
               task_number TEXT, agent_id TEXT, provider TEXT, \
               commit_sha TEXT, branch TEXT, status TEXT, \
               conclusion TEXT, run_url TEXT, run_id TEXT, \
               created_at TEXT DEFAULT (datetime('now')), \
               updated_at TEXT DEFAULT (datetime('now')), \
               failure_log TEXT, dismissed_at TEXT, org_id TEXT \
             ); \
             CREATE TABLE settings ( \
               key TEXT PRIMARY KEY, value TEXT \
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runners (id, org_id, status, last_seen_at) \
             VALUES (?1, ?2, 'online', datetime('now'))",
            params![runner_id, org_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, plan_name, task_id, org_id, cwd) \
             VALUES ('agent-1', ?1, '1.3', ?2, '/tmp/cwd')",
            params![plan_name, org_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, agent_id, \
                                  provider, commit_sha, branch, status, org_id) \
             VALUES (?1, '1.3', 'agent-1', 'github', ?2, 'master', 'pending', ?3)",
            params![plan_name, sha, org_id],
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    fn poll_test_app_state(db: Db, runners: RunnerRegistry, plans_dir: PathBuf) -> AppState {
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/tmp/branchwork-test-claude"),
            0,
            true,
        );
        AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: Instant::now(),
        }
    }

    /// Wire an in-memory runner that replies with the supplied
    /// [`CiAggregate`] for any `GetCiRunStatus` envelope. Borrowed from
    /// the `saas::dispatch` test pattern.
    async fn install_aggregate_responder(
        registry: &RunnerRegistry,
        runner_id: &str,
        aggregate: CiAggregate,
    ) {
        let pending: Arc<
            Mutex<std::collections::HashMap<String, oneshot::Sender<RunnerResponse>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pending_clone = pending.clone();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(payload) = cmd_rx.recv().await {
                let envelope: Envelope = match serde_json::from_str(&payload) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if let WireMessage::GetCiRunStatus { req_id, .. } = envelope.message
                    && let Some(tx) = pending_clone.lock().await.remove(&req_id)
                {
                    let _ = tx.send(RunnerResponse::CiRunStatusResolved(Some(aggregate.clone())));
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
            },
        );
    }

    /// Build the `Docker:success + CI:failure` aggregate the way the
    /// runner produces it for cep sha 12d31ceb6a9a93d906a017a9a7a85b269361ab91:
    /// the CI run failed first (run id 25520540172) so it's the
    /// `failing_run_id`; Docker's later success (25520540186) does NOT
    /// flip the verdict.
    fn cep_multi_workflow_aggregate() -> CiAggregate {
        CiAggregate {
            status: "completed".into(),
            conclusion: Some("failure".into()),
            failing_run_id: Some("25520540172".into()),
            runs: vec![
                CiRunSummary {
                    run_id: "25520540172".into(),
                    workflow_name: "CI".into(),
                    status: "completed".into(),
                    conclusion: Some("failure".into()),
                    skipped_due_to_upstream: false,
                    informational: false,
                },
                CiRunSummary {
                    run_id: "25520540186".into(),
                    workflow_name: "Docker".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    skipped_due_to_upstream: false,
                    informational: true,
                },
            ],
        }
    }

    /// Acceptance: when dispatch reports `Docker:success + CI:failure`
    /// for a SHA, the poller writes `conclusion=failure` (NOT success)
    /// and the run_url + run_id point at the CI run, NOT the Docker
    /// badge.
    #[tokio::test]
    async fn polls_aggregates_to_failure_when_blocking_run_fails() {
        let org_id = "org-cep";
        let runner_id = "cep-runner";
        let plan_name = "varpulis-option-b-cep";
        let sha = "12d31ceb6a9a93d906a017a9a7a85b269361ab91";

        let db = poll_test_db(org_id, runner_id, plan_name, sha);
        let runners = new_runner_registry();
        install_aggregate_responder(&runners, runner_id, cep_multi_workflow_aggregate()).await;

        // Set up a real cwd with a github origin so derive_run_url
        // produces the `cep/cep` deep link the regression pins.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(cwd)
            .status()
            .unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", "https://github.com/cep/cep.git"])
            .current_dir(cwd)
            .status()
            .unwrap();

        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans-poll");
        let state = poll_test_app_state(db.clone(), runners, plans_dir);
        let mut project_dirs = std::collections::HashMap::new();
        project_dirs.insert(plan_name.to_string(), cwd.to_path_buf());

        poll_once(&state, &project_dirs).await;

        // Read the row back and assert the verdict + URL deep-links to
        // the failing CI run, NOT the latest gh run.
        let conn = db.lock().unwrap();
        let (status, conclusion, run_url, run_id): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, conclusion, run_url, run_id FROM ci_runs WHERE plan_name = ?1",
                params![plan_name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();

        assert_eq!(
            status, "failure",
            "blocking CI failure must beat Docker success"
        );
        assert_eq!(conclusion.as_deref(), Some("failure"));
        assert_eq!(
            run_url.as_deref(),
            Some("https://github.com/cep/cep/actions/runs/25520540172"),
            "run_url must deep-link to the failing CI run, not the Docker badge"
        );
        assert_eq!(run_id.as_deref(), Some("25520540172"));
    }

    /// Acceptance: success-with-no-failing_run_id (every blocking
    /// workflow passed) writes `run_url = NULL`. The dashboard already
    /// handles None gracefully; the existing test at
    /// `plan_curate.rs:668` exercises the round-trip of an explicit
    /// URL so we don't need to assert the round-trip shape here, only
    /// that the None case is preserved.
    #[tokio::test]
    async fn polls_clears_run_url_on_blocking_success() {
        let org_id = "org-cep";
        let runner_id = "cep-runner";
        let plan_name = "successful-plan";
        let sha = "0000000000000000000000000000000000000000";
        let db = poll_test_db(org_id, runner_id, plan_name, sha);
        let runners = new_runner_registry();
        install_aggregate_responder(
            &runners,
            runner_id,
            CiAggregate {
                status: "completed".into(),
                conclusion: Some("success".into()),
                failing_run_id: None,
                runs: vec![CiRunSummary {
                    run_id: "1".into(),
                    workflow_name: "CI".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    skipped_due_to_upstream: false,
                    informational: false,
                }],
            },
        )
        .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans-poll-success");
        let state = poll_test_app_state(db.clone(), runners, plans_dir);
        let mut project_dirs = std::collections::HashMap::new();
        project_dirs.insert(plan_name.to_string(), tmp.path().to_path_buf());

        poll_once(&state, &project_dirs).await;

        let conn = db.lock().unwrap();
        let (status, conclusion, run_url, run_id): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, conclusion, run_url, run_id FROM ci_runs WHERE plan_name = ?1",
                params![plan_name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();

        assert_eq!(status, "success");
        assert_eq!(conclusion.as_deref(), Some("success"));
        assert_eq!(run_url, None);
        assert_eq!(run_id, None);
    }

    // ── Backfill regression: legacy ci_runs rows written by the
    // single-run path (`gh run list -L 1`) get re-evaluated against the
    // aggregate-aware dispatch on first server startup. The pass is
    // gated by a `settings` row so subsequent boots are no-ops.

    /// Seed a `ci_runs` row in a terminal state — mirrors what the
    /// legacy single-run path would have stored before T1.3 landed.
    /// Returns the row id so the caller can read it back.
    fn seed_terminal_ci_row(
        db: &Db,
        plan_name: &str,
        sha: &str,
        org_id: &str,
        status: &str,
        run_id: &str,
        run_url: &str,
    ) -> i64 {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_runs (plan_name, task_number, agent_id, \
                                  provider, commit_sha, branch, status, \
                                  conclusion, run_url, run_id, org_id) \
             VALUES (?1, '1.3', 'agent-1', 'github', ?2, 'master', ?3, \
                     ?3, ?4, ?5, ?6)",
            params![plan_name, sha, status, run_url, run_id, org_id],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Acceptance: legacy `success` row for a SHA whose blocking run
    /// actually failed gets flipped to `failure`, with `run_url` and
    /// `run_id` deep-linking to the failing CI run rather than the
    /// stored Docker badge. Settings flag is set after the pass.
    #[tokio::test]
    async fn backfill_flips_legacy_docker_success_to_aggregate_failure() {
        let org_id = "org-cep";
        let runner_id = "cep-runner";
        let plan_name = "varpulis-option-b-cep";
        let sha = "12d31ceb6a9a93d906a017a9a7a85b269361ab91";

        let db = poll_test_db(org_id, runner_id, plan_name, sha);
        // The default poll_test_db row is status='pending'; replace it
        // with a stored Docker:success row that the legacy single-run
        // path would have written. The poll_once short-circuit on
        // status NOT IN (pending,running) means this row is invisible
        // to the live poller — only the backfill can rewrite it.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM ci_runs WHERE plan_name = ?1",
                params![plan_name],
            )
            .unwrap();
        }
        let row_id = seed_terminal_ci_row(
            &db,
            plan_name,
            sha,
            org_id,
            "success",
            "25520540186", // legacy Docker run id
            "https://github.com/cep/cep/actions/runs/25520540186",
        );

        let runners = new_runner_registry();
        install_aggregate_responder(&runners, runner_id, cep_multi_workflow_aggregate()).await;

        // Real cwd with a github origin so derive_run_url produces the
        // failing-CI deep link the regression pins.
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(cwd)
            .status()
            .unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", "https://github.com/cep/cep.git"])
            .current_dir(cwd)
            .status()
            .unwrap();

        // resolve_project_dirs needs to find this plan's cwd; the
        // simplest setup is to seed a plan_project row in a real
        // plan_project table. We don't have that table in
        // poll_test_db, so write a plan YAML in plans_dir instead and
        // let the canonical resolver pick it up.
        let plans_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            plans_dir.path().join(format!("{plan_name}.yaml")),
            format!(
                "name: {plan_name}\ntitle: t\nproject: {}\nphases:\n  - title: P1\n    tasks:\n      - number: '1.3'\n        title: t\n",
                cwd.strip_prefix(dirs::home_dir().unwrap_or_default())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| cwd.display().to_string())
            ),
        )
        .unwrap();

        let state = poll_test_app_state(db.clone(), runners, plans_dir.path().to_path_buf());

        backfill_aggregates(state).await;

        // Row flipped to failure, pointing at the CI run, NOT the
        // legacy Docker run.
        let conn = db.lock().unwrap();
        let (status, conclusion, run_id): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, conclusion, run_id FROM ci_runs WHERE id = ?1",
                params![row_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            status, "failure",
            "legacy Docker:success must flip to failure"
        );
        assert_eq!(conclusion.as_deref(), Some("failure"));
        assert_eq!(run_id.as_deref(), Some("25520540172"));

        // Gate set so subsequent boots skip the backfill.
        let gate: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![BACKFILL_GATE_KEY],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(gate.as_deref(), Some("1"));
    }

    /// Acceptance: a pre-existing `ci_backfill_v1_done='1'` settings
    /// row makes the backfill a no-op. The responder, if installed,
    /// would flip the row — assert that the row is left alone.
    #[tokio::test]
    async fn backfill_skips_when_gate_already_set() {
        let org_id = "org-cep";
        let runner_id = "cep-runner";
        let plan_name = "already-applied";
        let sha = "deadbeefcafebabe000000000000000000000000";

        let db = poll_test_db(org_id, runner_id, plan_name, sha);
        // Pre-set the gate.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, '1')",
                params![BACKFILL_GATE_KEY],
            )
            .unwrap();
        }
        // Update the seeded row to a terminal-but-stale state. If the
        // backfill ran, it would flip this to failure via the
        // installed responder — so an unchanged status proves the
        // gate skipped the pass.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE ci_runs SET status = 'success', run_id = 'legacy-id' \
                 WHERE plan_name = ?1",
                params![plan_name],
            )
            .unwrap();
        }

        let runners = new_runner_registry();
        install_aggregate_responder(&runners, runner_id, cep_multi_workflow_aggregate()).await;

        let plans_dir = tempfile::TempDir::new().unwrap();
        let state = poll_test_app_state(db.clone(), runners, plans_dir.path().to_path_buf());

        backfill_aggregates(state).await;

        let conn = db.lock().unwrap();
        let (status, run_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, run_id FROM ci_runs WHERE plan_name = ?1",
                params![plan_name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "success", "row must be untouched when gate is set");
        assert_eq!(run_id.as_deref(), Some("legacy-id"));
    }

    /// Acceptance: a runner-offline pass leaves rows untouched but
    /// STILL sets the gate. The next boot, with the responder finally
    /// available, must NOT re-poll — flipping the gate manually is
    /// the only way to retry. This is the explicit trade-off
    /// documented in the task description.
    #[tokio::test]
    async fn backfill_runner_offline_leaves_rows_untouched_and_sets_gate() {
        let org_id = "org-cep";
        let runner_id = "cep-runner";
        let plan_name = "offline-during-backfill";
        let sha = "0000000000000000000000000000000000000000";

        let db = poll_test_db(org_id, runner_id, plan_name, sha);
        // Replace the seeded row with a terminal `success` row that
        // we'd want flipped if the runner were online. The runners
        // table still says 'online' (poll_test_db inserts that), so
        // org_has_runner returns true and dispatch will try the
        // runner branch — but we never install_aggregate_responder,
        // so runner_request_with_registry returns NoConnectedRunner.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM ci_runs WHERE plan_name = ?1",
                params![plan_name],
            )
            .unwrap();
        }
        let row_id = seed_terminal_ci_row(
            &db,
            plan_name,
            sha,
            org_id,
            "success",
            "legacy-docker-id",
            "https://github.com/x/y/actions/runs/legacy-docker-id",
        );

        let runners = new_runner_registry();
        // INTENTIONALLY no install_aggregate_responder — registry
        // empty produces RunnerRpcError::NoConnectedRunner promptly.

        let plans_dir = tempfile::TempDir::new().unwrap();
        let state =
            poll_test_app_state(db.clone(), runners.clone(), plans_dir.path().to_path_buf());

        backfill_aggregates(state.clone()).await;

        // Row is untouched — runner-offline did NOT corrupt it.
        let (status, run_id): (String, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, run_id FROM ci_runs WHERE id = ?1",
                params![row_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(status, "success", "offline pass must not corrupt the row");
        assert_eq!(run_id.as_deref(), Some("legacy-docker-id"));

        // Gate IS set, even though we made no useful progress.
        let gate: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![BACKFILL_GATE_KEY],
                |r| r.get(0),
            )
            .ok()
        };
        assert_eq!(
            gate.as_deref(),
            Some("1"),
            "gate must be set so subsequent boots don't retry the offline pass"
        );

        // Now that the responder IS installed, re-running the
        // backfill must STILL leave the row alone — the gate
        // short-circuits the dispatch entirely. This pins the
        // documented trade-off: retry requires manual gate flip.
        install_aggregate_responder(&runners, runner_id, cep_multi_workflow_aggregate()).await;
        backfill_aggregates(state).await;

        let conn = db.lock().unwrap();
        let (status, run_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, run_id FROM ci_runs WHERE id = ?1",
                params![row_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "success",
            "second pass must not run while gate is set"
        );
        assert_eq!(run_id.as_deref(), Some("legacy-docker-id"));
    }
}
