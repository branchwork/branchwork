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

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::db::Db;
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

fn has_gh() -> bool {
    // Try `gh --version` to confirm the binary works.
    Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_remote(cwd: &Path, name: &str) -> bool {
    Command::new("git")
        .args(["remote", "get-url", name])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Push + record ───────────────────────────────────────────────────────────

/// Kick off CI for a just-merged task: push to `origin/<branch>`, record a
/// pending `ci_runs` row, and broadcast the initial state. Swallows all
/// failures with log lines so a merge never appears broken because of CI.
/// All inputs needed to fire a CI pipeline after a merge. Grouped because
/// clippy (correctly) doesn't love nine free-floating positional args.
pub struct TriggerArgs {
    pub db: Db,
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
        broadcast_tx,
        cwd,
        plan_name,
        task_number,
        agent_id,
        source_branch,
        task_branch,
        merged_sha,
    } = args;
    // Detect: need GitHub Actions workflows, an `origin` remote, and `gh`.
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

    // Push the source branch so CI on the remote can fire.
    let push = Command::new("git")
        .args(["push", "origin", &source_branch])
        .current_dir(&cwd)
        .output();

    match push {
        Ok(out) if out.status.success() => {
            println!("[ci] pushed {source_branch} ({merged_sha}) to origin");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("[ci] push failed for {source_branch}: {stderr}");
            return;
        }
        Err(e) => {
            eprintln!("[ci] failed to run git push: {e}");
            return;
        }
    }

    // Record pending row.
    let run_id = {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_runs \
               (plan_name, task_number, agent_id, provider, commit_sha, branch, status) \
             VALUES (?1, ?2, ?3, 'github', ?4, ?5, 'pending')",
            params![plan_name, task_number, agent_id, merged_sha, task_branch],
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

#[derive(Deserialize)]
struct GhRun {
    #[serde(rename = "databaseId")]
    database_id: Option<i64>,
    status: Option<String>,
    conclusion: Option<String>,
    url: Option<String>,
}

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
async fn fetch_run(cwd: PathBuf, sha: String) -> Option<GhRun> {
    tokio::task::spawn_blocking(move || {
        let out = Command::new("gh")
            .args([
                "run",
                "list",
                "--commit",
                &sha,
                "-L",
                "1",
                "--json",
                "databaseId,status,conclusion,url",
            ])
            .current_dir(&cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let runs: Vec<GhRun> = serde_json::from_slice(&out.stdout).ok()?;
        runs.into_iter().next()
    })
    .await
    .ok()
    .flatten()
}

/// One pass: look up every pending/running `ci_runs` row, query `gh`, and
/// update rows + broadcast when status changes. Rows older than
/// `MAX_RUN_AGE_SECS` with no success are marked `unknown` so the dashboard
/// doesn't show a permanent spinner for a commit that never kicked off CI.
async fn poll_once(
    db: &Db,
    broadcast_tx: &broadcast::Sender<String>,
    project_dirs: &std::collections::HashMap<String, PathBuf>,
) {
    // Snapshot open rows — hold the lock only briefly.
    struct Row {
        id: i64,
        plan_name: String,
        task_number: String,
        commit_sha: Option<String>,
        status: String,
        age_secs: i64,
    }
    let rows: Vec<Row> = {
        let conn = db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, plan_name, task_number, commit_sha, status, \
                    CAST(strftime('%s','now') - strftime('%s', created_at) AS INTEGER) \
             FROM ci_runs WHERE status IN ('pending','running') \
             ORDER BY id ASC",
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

        let run = fetch_run(cwd.clone(), sha.clone()).await;
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
pub fn spawn_poller(db: Db, broadcast_tx: broadcast::Sender<String>, plans_dir: PathBuf) {
    if !has_gh() {
        println!("[ci] `gh` CLI not available — CI status polling disabled");
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            // Resolve plan_name -> project dir by re-reading plan files. Cheap
            // enough given the poll cadence, and avoids caching that could go
            // stale when the user edits a plan's `project` field.
            let project_dirs = resolve_project_dirs(&plans_dir, &db);
            poll_once(&db, &broadcast_tx, &project_dirs).await;
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
/// no remote, or `gh` isn't installed.
pub async fn fetch_failure_log(db: &Db, plans_dir: PathBuf, ci_run_id: i64) -> Option<String> {
    const CAP_BYTES: usize = 8 * 1024;

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

    let log = tokio::task::spawn_blocking(move || {
        let out = Command::new("gh")
            .args(["run", "view", &run_id, "--log-failed"])
            .current_dir(&cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // `--log-failed` can be hundreds of KB; keep the tail (failures
        // accumulate there) and decode lossily so stray non-UTF8 doesn't
        // drop the whole buffer.
        let raw = out.stdout;
        let start = raw.len().saturating_sub(CAP_BYTES);
        Some(String::from_utf8_lossy(&raw[start..]).into_owned())
    })
    .await
    .ok()
    .flatten()?;

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
}

/// Latest CI run per task number for the given plan.
pub fn latest_per_task(
    conn: &rusqlite::Connection,
    plan_name: &str,
) -> std::collections::HashMap<String, CiStatus> {
    // Pick row with max(id) per (plan_name, task_number).
    let mut stmt = match conn.prepare(
        "SELECT c.task_number, c.id, c.status, c.conclusion, c.run_url, c.commit_sha, c.updated_at \
         FROM ci_runs c \
         INNER JOIN (SELECT task_number, MAX(id) AS max_id FROM ci_runs \
                     WHERE plan_name = ?1 GROUP BY task_number) m \
           ON c.id = m.max_id \
         WHERE c.plan_name = ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Default::default(),
    };
    let rows = stmt
        .query_map(params![plan_name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                CiStatus {
                    id: r.get(1)?,
                    status: r.get(2)?,
                    conclusion: r.get(3)?,
                    run_url: r.get(4)?,
                    commit_sha: r.get(5)?,
                    updated_at: r.get(6)?,
                },
            ))
        })
        .ok();
    rows.map(|it| it.flatten().collect()).unwrap_or_default()
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
    fn has_github_actions_detects_yml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        assert!(!has_github_actions(dir.path())); // empty
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        assert!(has_github_actions(dir.path()));
    }

    #[test]
    fn latest_per_task_returns_most_recent_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ci_runs (id INTEGER PRIMARY KEY AUTOINCREMENT, \
              plan_name TEXT, task_number TEXT, status TEXT, conclusion TEXT, \
              run_url TEXT, commit_sha TEXT, updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
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

        let got = latest_per_task(&conn, "p");
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("1.1").unwrap().status, "success");
        assert_eq!(
            got.get("1.1").unwrap().run_url.as_deref(),
            Some("https://x")
        );
        assert_eq!(got.get("1.2").unwrap().status, "failure");
    }
}
