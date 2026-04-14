pub mod check_agent;
pub mod pty_agent;
pub mod session_protocol;
pub mod supervisor;
pub mod terminal_ws;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Effort;
use crate::db::{Db, completed_task_numbers};
use crate::plan_parser::{self, ParsedPlan, PlanPhase, PlanTask};
use crate::ws::broadcast_event;

pub type AgentId = String;

/// Get the current HEAD commit SHA in the given directory.
/// Returns None if the directory is not a git repo or git is unavailable.
/// Ensure the given directory is a git repository. If it isn't, run
/// `git init`, stage everything, and create an initial commit so that
/// branch isolation and diffs work for this project.
///
/// Returns true if the directory was already a repo or was successfully
/// initialized. Safe to call repeatedly.
pub fn ensure_git_initialized(cwd: &std::path::Path) -> bool {
    // Already a repo?
    let in_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    if matches!(&in_repo, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
    {
        return true;
    }

    // git init
    let init = std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(cwd)
        .output();
    if !matches!(&init, Ok(o) if o.status.success()) {
        eprintln!("[orchestrAI] git init failed in {}", cwd.display());
        return false;
    }

    // Set a minimal identity if none exists globally — git commit will refuse otherwise
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "orchestrai@localhost"])
        .current_dir(cwd)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "orchestrAI"])
        .current_dir(cwd)
        .output();

    // Stage and make an empty initial commit — gives us a HEAD so branches can be created
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .output();
    let commit = std::process::Command::new("git")
        .args([
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit (orchestrAI)",
        ])
        .current_dir(cwd)
        .output();
    match commit {
        Ok(o) if o.status.success() => {
            println!("[orchestrAI] Initialized git repo in {}", cwd.display());
            true
        }
        _ => {
            eprintln!("[orchestrAI] initial commit failed in {}", cwd.display());
            false
        }
    }
}

pub fn git_head_sha(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the current branch name in the given directory.
pub fn git_current_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if name == "HEAD" {
            // Detached HEAD — not on a branch
            None
        } else {
            Some(name)
        }
    } else {
        None
    }
}

/// Create or checkout a git branch. Returns true if successful.
/// For "start" mode: creates the branch (or checks it out if it already exists).
/// For "continue" mode: checks out the existing branch.
pub fn git_checkout_branch(cwd: &std::path::Path, branch: &str, is_continue: bool) -> bool {
    if is_continue {
        // Try to checkout the existing branch
        let status = std::process::Command::new("git")
            .args(["checkout", branch])
            .current_dir(cwd)
            .output();
        match status {
            Ok(output) if output.status.success() => {
                println!("[orchestrAI] Checked out existing branch: {branch}");
                return true;
            }
            _ => {
                // Branch doesn't exist yet — fall through to create it
                println!("[orchestrAI] Branch {branch} not found for continue, creating it");
            }
        }
    }

    // Try to create the branch
    let status = std::process::Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(cwd)
        .output();
    match status {
        Ok(output) if output.status.success() => {
            println!("[orchestrAI] Created and checked out branch: {branch}");
            true
        }
        _ => {
            // Branch already exists — just check it out
            let fallback = std::process::Command::new("git")
                .args(["checkout", branch])
                .current_dir(cwd)
                .output();
            match fallback {
                Ok(output) if output.status.success() => {
                    println!("[orchestrAI] Checked out existing branch: {branch}");
                    true
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[orchestrAI] Failed to checkout branch {branch}: {stderr}");
                    false
                }
                Err(e) => {
                    eprintln!("[orchestrAI] Failed to run git checkout: {e}");
                    false
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AgentRegistry {
    pub agents: Arc<Mutex<HashMap<AgentId, ManagedAgent>>>,
    pub db: Db,
    pub broadcast_tx: tokio::sync::broadcast::Sender<String>,
    /// Optional Slack/webhook URL. When set, agent-completion and phase-advance
    /// events fan out a POST here in addition to the in-process WS broadcast.
    pub webhook_url: Option<String>,
}

pub struct ManagedAgent {
    /// Kept alive to prevent the child process from being dropped/killed.
    #[allow(dead_code)]
    pub pty: Option<Box<dyn portable_pty::Child + Send>>,
    pub pty_writer: Option<Box<dyn std::io::Write + Send>>,
    pub pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub tmux_session: Option<String>,
    pub terminals: Vec<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
}

impl AgentRegistry {
    pub fn new(
        db: Db,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        webhook_url: Option<String>,
    ) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            db,
            broadcast_tx,
            webhook_url,
        }
    }

    /// Clean up dead agents and reattach alive ones (from previous server runs)
    pub async fn cleanup_and_reattach(&self) {
        let stale: Vec<(String, i64)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db
                .prepare("SELECT id, pid FROM agents WHERE status IN ('running', 'starting')")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .flatten()
                .collect()
        };

        for (id, pid) in stale {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                    rusqlite::params![id],
                ).ok();
                println!(
                    "[orchestrAI] Cleaned stale agent {} (pid {}) — process dead",
                    &id[..8],
                    pid
                );
                continue;
            }

            // Check if tmux session still exists
            let tmux_name = format!("oai-{}", &id[..8]);
            let tmux_exists = std::process::Command::new("tmux")
                .args(["has-session", "-t", &tmux_name])
                .status()
                .is_ok_and(|s| s.success());

            if tmux_exists {
                // Reattach!
                pty_agent::reattach_agent(self, &id, &tmux_name).await;
            } else {
                println!(
                    "[orchestrAI] Agent {} (pid {}) alive but no tmux session — detached",
                    &id[..8],
                    pid
                );
            }
        }
    }

    pub async fn kill_agent(&self, agent_id: &str) -> bool {
        // Try in-memory registry first (live agents)
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.remove(agent_id) {
            // Kill tmux session if it exists
            if let Some(ref tmux) = agent.tmux_session {
                std::process::Command::new("tmux")
                    .args(["kill-session", "-t", tmux])
                    .status()
                    .ok();
            }
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?",
                rusqlite::params![agent_id],
            )
            .ok();
            broadcast_event(
                &self.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": agent_id, "status": "killed"}),
            );
            return true;
        }
        drop(agents);

        // Fallback: try to find tmux session by naming convention
        let tmux_name = format!("oai-{}", &agent_id[..8.min(agent_id.len())]);
        let tmux_exists = std::process::Command::new("tmux")
            .args(["has-session", "-t", &tmux_name])
            .status()
            .is_ok_and(|s| s.success());

        if tmux_exists {
            std::process::Command::new("tmux")
                .args(["kill-session", "-t", &tmux_name])
                .status()
                .ok();
        } else {
            // Last resort: kill by PID
            let db = self.db.lock().unwrap();
            if let Ok(pid) = db.query_row(
                "SELECT pid FROM agents WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
                |row| row.get::<_, i64>(0),
            ) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }

        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET status = 'killed', finished_at = datetime('now') WHERE id = ?",
            rusqlite::params![agent_id],
        )
        .ok();
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({"id": agent_id, "status": "killed"}),
        );
        true
    }
}

// ── Task prompt + auto-advance ───────────────────────────────────────────────

/// Resolve the project name for a plan, preferring the DB override row (set via
/// the project endpoint) and falling back to the value parsed from the plan.
fn plan_project(db: &Db, plan_name: &str, parsed_project: Option<&str>) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT project FROM plan_project WHERE plan_name = ?1",
        rusqlite::params![plan_name],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .or_else(|| parsed_project.map(|s| s.to_string()))
}

/// Build a prompt fragment listing completed tasks from sibling plans (and the
/// current plan's earlier tasks) in the same project, so an agent picking up a
/// task inherits what its predecessors established.
///
/// `current_task` is excluded from the listing (it's the task being worked on).
/// Returns `None` when there's nothing useful to share.
pub fn build_cross_plan_context(
    db: &Db,
    plans_dir: &std::path::Path,
    current_plan: &ParsedPlan,
    current_task_number: &str,
) -> Option<String> {
    let project = plan_project(db, &current_plan.name, current_plan.project.as_deref())?;
    let home = dirs::home_dir().unwrap_or_default();

    // Every plan mapped to this project (DB override + parsed value).
    let summaries = plan_parser::list_plans(plans_dir);
    let project_overrides: HashMap<String, String> = {
        let conn = db.lock().unwrap();
        conn.prepare("SELECT plan_name, project FROM plan_project")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()
            })
            .unwrap_or_default()
    };

    let mut entries: Vec<String> = Vec::new();

    for summary in summaries {
        let plan_project_name = project_overrides
            .get(&summary.name)
            .cloned()
            .or(summary.project.clone());
        if plan_project_name.as_deref() != Some(project.as_str()) {
            continue;
        }

        let plan = if summary.name == current_plan.name {
            current_plan.clone()
        } else {
            match plan_parser::find_plan_file(plans_dir, &summary.name)
                .and_then(|p| plan_parser::parse_plan_file(&p).ok())
            {
                Some(p) => p,
                None => continue,
            }
        };

        // Completed/skipped tasks for this plan.
        let status_map: HashMap<String, String> = {
            let conn = db.lock().unwrap();
            conn.prepare(
                "SELECT task_number, status FROM task_status \
                 WHERE plan_name = ?1 AND status IN ('completed', 'skipped')",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![summary.name], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<_, _>>()
            })
            .unwrap_or_default()
        };

        for phase in &plan.phases {
            for task in &phase.tasks {
                // Never include the task the agent is about to work on.
                if plan.name == current_plan.name && task.number == current_task_number {
                    continue;
                }
                if !status_map.contains_key(&task.number) {
                    continue;
                }

                let learnings = {
                    let conn = db.lock().unwrap();
                    crate::db::task_learnings(&conn, &plan.name, &task.number)
                };

                let source = if plan.name == current_plan.name {
                    format!("Task {}", task.number)
                } else {
                    format!("Task {} ({})", task.number, plan.title)
                };

                let mut line = format!("- {source}: {}", task.title);
                if !task.file_paths.is_empty() {
                    line.push_str(" — files: ");
                    line.push_str(&task.file_paths.join(", "));
                }
                entries.push(line);

                for l in learnings {
                    entries.push(format!("    • {l}"));
                }
            }
        }
    }

    // Reference home to silence the unused warning in case we later expand this
    // with project-directory–relative grounding; keep the import useful.
    let _ = home;

    if entries.is_empty() {
        None
    } else {
        Some(format!(
            "Related work already completed in this project:\n{}",
            entries.join("\n")
        ))
    }
}

pub fn build_task_prompt(
    plan: &ParsedPlan,
    phase: &PlanPhase,
    task: &PlanTask,
    is_continue: bool,
    port: u16,
    cross_plan_context: Option<&str>,
) -> String {
    let files_section = if !task.file_paths.is_empty() {
        format!(
            "\nFiles involved:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        String::new()
    };

    let acceptance_section = if !task.acceptance.is_empty() {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    } else {
        String::new()
    };

    let context_section = cross_plan_context
        .map(|c| format!("\n{c}\n"))
        .unwrap_or_default();

    format!(
        "{intro}\n\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Description:\n{description}\n\
         {files}{acceptance}\n\
         {context}\n\
         {instruction}\n\n\
         IMPORTANT: When you think you are done, do NOT stop. Instead:\n\
         1. Summarize what you did\n\
         2. Record one short learning other tasks in this project should know (file paths established, key decisions, gotchas) by running: curl -s -X POST http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/learnings -H \"Content-Type: application/json\" -d '{{\"learning\":\"...\"}}'\n\
         3. Mark the task status by running: curl -s -X PUT http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/status -H \"Content-Type: application/json\" -d '{{\"status\":\"completed\"}}'\n\
         4. Ask the user if they need anything else\n\
         5. Only stop when the user explicitly says they are done",
        intro = if is_continue {
            "You are continuing work on a partially implemented task. Some parts may already exist — check the current state of the code before making changes."
        } else {
            "You are working on the following task from a plan."
        },
        plan_title = plan.title,
        phase_num = phase.number,
        phase_title = phase.title,
        task_num = task.number,
        task_title = task.title,
        description = task.description,
        files = files_section,
        acceptance = acceptance_section,
        context = context_section,
        instruction = if is_continue {
            "First, read the relevant files to understand what has already been done. Then complete the remaining work."
        } else {
            "Please implement this task. When done, summarize what you changed."
        },
        plan_name = plan.name,
    )
}

/// Whether auto-advance is enabled for `plan_name` (opt-in, default off).
pub fn auto_advance_enabled(db: &Db, plan_name: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled FROM plan_auto_advance WHERE plan_name = ?1",
        rusqlite::params![plan_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

/// Compute the remaining budget for a plan, or `Err((spent, max))` if the
/// budget is already exhausted. Mirrors `plan_remaining_budget` in the API
/// module so we don't have to thread it through here.
fn remaining_budget(db: &Db, plan_name: &str) -> Result<Option<f64>, (f64, f64)> {
    let conn = db.lock().unwrap();
    let max: Option<f64> = conn
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?1",
            rusqlite::params![plan_name],
            |row| row.get::<_, f64>(0),
        )
        .ok();
    match max {
        Some(m) => {
            let spent: f64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
                     WHERE plan_name = ?1 AND cost_usd IS NOT NULL",
                    rusqlite::params![plan_name],
                    |row| row.get::<_, f64>(0),
                )
                .unwrap_or(0.0);
            if spent >= m {
                Err((spent, m))
            } else {
                Ok(Some((m - spent).max(0.0)))
            }
        }
        None => Ok(None),
    }
}

/// Atomically claim a task for spawning: flip its row to `in_progress` only if
/// it's currently `pending` or `failed`. Returns true if we won the claim.
fn claim_task(db: &Db, plan_name: &str, task_number: &str) -> bool {
    let conn = db.lock().unwrap();
    let updated = conn
        .execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at)
             VALUES (?1, ?2, 'in_progress', datetime('now'))
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at
             WHERE task_status.status IN ('pending', 'failed')",
            rusqlite::params![plan_name, task_number],
        )
        .unwrap_or(0);
    updated > 0
}

/// If the just-completed task finished the last open task in its phase, and
/// auto-advance is enabled for the plan, spawn agents for ready tasks of the
/// next phase that has any. No-op otherwise.
///
/// Runs in the background (caller `tokio::spawn`s it) so it can't block the
/// HTTP response that triggered it.
pub async fn try_auto_advance(
    registry: AgentRegistry,
    plans_dir: PathBuf,
    plan_name: String,
    completed_task_number: String,
    effort: Effort,
    port: u16,
) {
    if !auto_advance_enabled(&registry.db, &plan_name) {
        return;
    }

    let plan_path = match plan_parser::find_plan_file(&plans_dir, &plan_name) {
        Some(p) => p,
        None => return,
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Find the phase that owns the just-completed task.
    let current_phase = match plan
        .phases
        .iter()
        .find(|p| p.tasks.iter().any(|t| t.number == completed_task_number))
    {
        Some(p) => p,
        None => return,
    };

    // Snapshot all task statuses for this plan so we can decide locally.
    let status_map: HashMap<String, String> = {
        let conn = registry.db.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?1")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map(rusqlite::params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    };

    let phase_done = current_phase.tasks.iter().all(|t| {
        matches!(
            status_map.get(&t.number).map(String::as_str),
            Some("completed") | Some("skipped")
        )
    });
    if !phase_done {
        return;
    }

    // Find the next sequentially-numbered phase that has at least one ready
    // task. Skip phases that are already fully done.
    let done_set = {
        let conn = registry.db.lock().unwrap();
        completed_task_numbers(&conn, &plan_name)
    };

    let next_phase = plan
        .phases
        .iter()
        .filter(|p| p.number > current_phase.number)
        .find(|p| {
            p.tasks.iter().any(|t| {
                let status = status_map
                    .get(&t.number)
                    .map(String::as_str)
                    .unwrap_or("pending");
                (status == "pending" || status == "failed")
                    && t.dependencies.iter().all(|d| done_set.contains(d))
            })
        });
    let next_phase = match next_phase {
        Some(p) => p,
        None => return,
    };

    // Budget check — if exhausted, give up silently. The plan's already had
    // its other agents killed by the budget enforcement in pty_agent.
    let max_budget_usd = match remaining_budget(&registry.db, &plan_name) {
        Ok(b) => b,
        Err(_) => return,
    };

    let work_dir: PathBuf = {
        let home = dirs::home_dir().unwrap();
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    };

    let ready_tasks: Vec<&PlanTask> = next_phase
        .tasks
        .iter()
        .filter(|t| {
            let status = status_map
                .get(&t.number)
                .map(String::as_str)
                .unwrap_or("pending");
            (status == "pending" || status == "failed")
                && t.dependencies.iter().all(|d| done_set.contains(d))
        })
        .collect();

    println!(
        "[auto-advance] {plan_name}: phase {} done -> spawning {} task(s) in phase {}",
        current_phase.number,
        ready_tasks.len(),
        next_phase.number
    );

    for task in ready_tasks {
        // Race guard: only spawn if we successfully claim the task.
        if !claim_task(&registry.db, &plan_name, &task.number) {
            continue;
        }

        broadcast_event(
            &registry.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": plan_name,
                "task_number": task.number,
                "status": "in_progress",
            }),
        );

        let cross_ctx = build_cross_plan_context(&registry.db, &plans_dir, &plan, &task.number);
        let prompt = build_task_prompt(&plan, next_phase, task, false, port, cross_ctx.as_deref());
        let branch_name = format!("orchestrai/{}/{}", plan_name, task.number);

        pty_agent::start_pty_agent(
            &registry,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: &work_dir,
                plan_name: Some(&plan_name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd,
            },
        )
        .await;
    }

    broadcast_event(
        &registry.broadcast_tx,
        "phase_advanced",
        serde_json::json!({
            "plan_name": plan_name,
            "from_phase": current_phase.number,
            "to_phase": next_phase.number,
        }),
    );

    if registry.webhook_url.is_some() {
        let msg = crate::notifications::phase_advance_message(
            &plan_name,
            current_phase.number,
            next_phase.number,
            next_phase
                .tasks
                .iter()
                .filter(|t| {
                    let s = status_map
                        .get(&t.number)
                        .map(String::as_str)
                        .unwrap_or("pending");
                    s == "pending" || s == "failed"
                })
                .count(),
        );
        crate::notifications::notify(registry.webhook_url.clone(), msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.db");
        (crate::db::init(&path), dir)
    }

    #[test]
    fn auto_advance_default_off() {
        let (db, _dir) = fresh_db();
        assert!(!auto_advance_enabled(&db, "p1"));
    }

    #[test]
    fn auto_advance_toggle() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(auto_advance_enabled(&db, "p1"));
        assert!(!auto_advance_enabled(&db, "p2"));

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE plan_auto_advance SET enabled = 0 WHERE plan_name = ?1",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(!auto_advance_enabled(&db, "p1"));
    }

    #[test]
    fn claim_task_only_first_caller_wins() {
        let (db, _dir) = fresh_db();
        // No row exists yet — first claim creates as in_progress.
        assert!(claim_task(&db, "p1", "1.1"));
        // Second claim sees in_progress, refuses.
        assert!(!claim_task(&db, "p1", "1.1"));
    }

    #[test]
    fn claim_task_reclaims_failed() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'failed')",
                params!["p1", "1.1"],
            )
            .unwrap();
        }
        // Failed tasks can be re-spawned.
        assert!(claim_task(&db, "p1", "1.1"));
        assert!(!claim_task(&db, "p1", "1.1"));
    }

    fn write_yaml_plan(dir: &std::path::Path, name: &str, project: &str, body: &str) {
        let path = dir.join(format!("{name}.yaml"));
        let yaml = format!("title: \"{name} title\"\nproject: {project}\nphases:\n{body}");
        std::fs::write(path, yaml).unwrap();
    }

    #[test]
    fn cross_plan_context_includes_sibling_completed_tasks() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // Sibling plan in the same project with a completed auth task.
        write_yaml_plan(
            &plans_dir,
            "auth-plan",
            "demo-proj",
            "  - number: 1\n\
             \x20   title: \"Auth\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Build auth middleware\"\n\
             \x20       file_paths: [\"src/middleware/auth.ts\"]\n",
        );

        // Current plan in the same project. Task 2.1 is what we'll be working on.
        write_yaml_plan(
            &plans_dir,
            "checkout-plan",
            "demo-proj",
            "  - number: 2\n\
             \x20   title: \"Checkout\"\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: \"Add checkout endpoint\"\n",
        );

        // Mark sibling task complete and store a learning on it.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'completed')",
                params!["auth-plan", "1.1"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
                params!["auth-plan", "1.1", "Uses JWT with refresh tokens"],
            )
            .unwrap();
        }

        let current = plan_parser::parse_plan_file(&plans_dir.join("checkout-plan.yaml")).unwrap();
        let ctx = build_cross_plan_context(&db, &plans_dir, &current, "2.1")
            .expect("expected sibling context");

        assert!(ctx.contains("Build auth middleware"), "got: {ctx}");
        assert!(ctx.contains("src/middleware/auth.ts"), "got: {ctx}");
        assert!(ctx.contains("auth-plan title"), "got: {ctx}");
        assert!(ctx.contains("Uses JWT with refresh tokens"), "got: {ctx}");
    }

    #[test]
    fn cross_plan_context_excludes_other_projects_and_self() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        write_yaml_plan(
            &plans_dir,
            "unrelated",
            "other-proj",
            "  - number: 1\n\
             \x20   title: \"Unrelated\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Should not appear\"\n",
        );

        write_yaml_plan(
            &plans_dir,
            "current",
            "demo-proj",
            "  - number: 1\n\
             \x20   title: \"Cur\"\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: \"Already done in this plan\"\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: \"The task being worked on\"\n",
        );

        // Both unrelated/1.1 and current/1.1 are completed; only current/1.1 should
        // appear (same project) and current/1.2 (the task being worked on) must
        // never appear.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('unrelated', '1.1', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('current', '1.1', 'completed')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES ('current', '1.2', 'completed')",
                [],
            )
            .unwrap();
        }

        let current = plan_parser::parse_plan_file(&plans_dir.join("current.yaml")).unwrap();
        let ctx = build_cross_plan_context(&db, &plans_dir, &current, "1.2")
            .expect("expected own-plan context");

        assert!(ctx.contains("Already done in this plan"), "got: {ctx}");
        assert!(
            !ctx.contains("Should not appear"),
            "leaked unrelated project: {ctx}"
        );
        assert!(
            !ctx.contains("The task being worked on"),
            "leaked current task into its own context: {ctx}",
        );
    }

    #[test]
    fn cross_plan_context_none_when_no_project() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // No `project:` field — plan can't be cross-referenced.
        let path = plans_dir.join("orphan.yaml");
        std::fs::write(
            &path,
            "title: Orphan\nphases:\n  - number: 1\n    title: Solo\n    tasks:\n      - number: \"1.1\"\n        title: Lonely\n",
        )
        .unwrap();

        let current = plan_parser::parse_plan_file(&path).unwrap();
        assert!(build_cross_plan_context(&db, &plans_dir, &current, "1.1").is_none());
    }

    #[test]
    fn claim_task_skips_completed() {
        let (db, _dir) = fresh_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, 'completed')",
                params!["p1", "1.1"],
            )
            .unwrap();
        }
        // Completed tasks are never re-spawned.
        assert!(!claim_task(&db, "p1", "1.1"));
    }
}
