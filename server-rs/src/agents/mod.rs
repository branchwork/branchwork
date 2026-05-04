pub mod check_agent;
pub mod driver;
pub mod git_ops;
pub mod prompt;
pub mod pty_agent;
pub mod session_protocol;
pub mod session_settings;
pub mod spawn_ops;
pub mod supervisor;
pub mod terminal_ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;

use crate::config::Effort;
use crate::db::{Db, completed_task_numbers};
use crate::plan_parser::{self, ParsedPlan, PlanPhase, PlanTask};
use crate::ws::broadcast_event;
use driver::DriverRegistry;
use session_protocol::Message as SessionMessage;

pub type AgentId = String;

/// Cross-platform "is this PID still alive" check.
///
/// On Unix we use `kill(pid, 0)` — it sends no signal but succeeds if the
/// kernel would let us signal the process. On Windows we don't yet have a
/// cheap PID-liveness primitive wired up, so we conservatively return
/// `false` and let the caller treat the row as stale (cleanup_and_reattach
/// will mark the agent `detached` and the operator can restart it).
pub fn process_alive(pid: i64) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        let _ = pid;
        false
    }
}

/// Cross-platform "politely ask this PID to stop" (SIGTERM on Unix,
/// `taskkill` on Windows). Best-effort — failures are swallowed.
pub fn process_terminate(pid: i64) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status();
    }
}

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
        eprintln!("[Branchwork] git init failed in {}", cwd.display());
        return false;
    }

    // Set a minimal identity if none exists globally — git commit will refuse otherwise
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "branchwork@localhost"])
        .current_dir(cwd)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "Branchwork"])
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
            "Initial commit (Branchwork)",
        ])
        .current_dir(cwd)
        .output();
    match commit {
        Ok(o) if o.status.success() => {
            println!("[Branchwork] Initialized git repo in {}", cwd.display());
            true
        }
        _ => {
            eprintln!("[Branchwork] initial commit failed in {}", cwd.display());
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

// `git_default_branch` lives in `crate::git_helpers` (a leaf module shared
// with the runner binary). Re-export it here so the existing call sites in
// `pty_agent.rs` and `api/agents.rs` keep compiling without churn —
// `git_list_branches` is reached only through the dispatcher in
// `agents/git_ops.rs` so it's not re-exported.
pub use crate::git_helpers::git_default_branch;

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
                println!("[Branchwork] Checked out existing branch: {branch}");
                return true;
            }
            _ => {
                // Branch doesn't exist yet — fall through to create it
                println!("[Branchwork] Branch {branch} not found for continue, creating it");
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
            println!("[Branchwork] Created and checked out branch: {branch}");
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
                    println!("[Branchwork] Checked out existing branch: {branch}");
                    true
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    eprintln!("[Branchwork] Failed to checkout branch {branch}: {stderr}");
                    false
                }
                Err(e) => {
                    eprintln!("[Branchwork] Failed to run git checkout: {e}");
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
    /// Wrapped in `RwLock` so `PUT /api/settings` can update it live without
    /// a server restart; readers (notify call sites) take a brief read lock.
    pub webhook_url: Arc<RwLock<Option<String>>>,
    /// Directory where per-agent supervisor sockets (and their `.log` / `.pid`
    /// siblings) live. Created on startup.
    pub sockets_dir: PathBuf,
    /// Absolute path to the running server binary. Used to respawn the
    /// `session` subcommand as a detached daemon.
    pub server_exe: PathBuf,
    /// TCP port the dashboard's HTTP listener (and MCP endpoint) is bound
    /// to. Used by drivers that auto-register the Branchwork MCP server
    /// with the spawned CLI.
    pub port: u16,
    /// Available agent drivers, keyed by name. Built once at startup with
    /// [`DriverRegistry::with_defaults`]; clones share the underlying Arc.
    pub drivers: DriverRegistry,
    /// Whether to spawn agents with `--dangerously-skip-permissions` (or the
    /// driver's equivalent). Toggled live from the dashboard via
    /// `PUT /api/settings`.
    pub skip_permissions: Arc<AtomicBool>,
    /// Lazily-set reference to the global [`crate::state::AppState`].
    /// `main.rs` populates it after `AppState::new()` returns; remains
    /// unset in test fixtures that build only an `AgentRegistry`. The
    /// auto-mode completion hook reads this to dispatch the merge with
    /// the shared runner registry + broadcast/db handles.
    ///
    /// Holds `Arc<OnceLock<AppState>>` rather than `Weak`: the registry
    /// and `AppState` both live for the process lifetime, and a `Weak`
    /// would trade graceful liveness for a one-time leak that never
    /// matters. Tests that don't set this skip the auto-mode hook
    /// silently.
    pub app_state: Arc<OnceLock<crate::state::AppState>>,
}

/// In-process state for a live agent whose PTY runs inside a detached
/// session daemon. Created during [`pty_agent::start_pty_agent`] and
/// dropped on kill or PTY exit.
///
/// The socket path and daemon PID live in the DB (`agents.supervisor_socket`
/// / `agents.pid`), not here — reconnect is always DB-driven so there's no
/// benefit to duplicating them in memory.
pub struct ManagedAgent {
    /// Channel into the writer task that frames messages (`Input`, `Resize`,
    /// `Kill`) and sends them over the socket. Dropping this closes the
    /// writer side and terminates the daemon connection.
    pub command_tx: tokio::sync::mpsc::UnboundedSender<SessionMessage>,
    /// Broadcast of PTY output bytes received from the supervisor. Each
    /// attached terminal WebSocket subscribes for its own receiver.
    pub output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

impl AgentRegistry {
    pub fn new(
        db: Db,
        broadcast_tx: tokio::sync::broadcast::Sender<String>,
        webhook_url: Option<String>,
        sockets_dir: PathBuf,
        server_exe: PathBuf,
        port: u16,
        skip_permissions: bool,
    ) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            db,
            broadcast_tx,
            webhook_url: Arc::new(RwLock::new(webhook_url)),
            sockets_dir,
            server_exe,
            port,
            drivers: DriverRegistry::with_defaults(),
            skip_permissions: Arc::new(AtomicBool::new(skip_permissions)),
            app_state: Arc::new(OnceLock::new()),
        }
    }

    /// Resolve the per-agent socket path `<sockets_dir>/<id>.sock`.
    pub fn socket_for(&self, agent_id: &str) -> PathBuf {
        self.sockets_dir.join(format!("{agent_id}.sock"))
    }

    /// Resolve the per-agent MCP config path
    /// `<sockets_dir>/<id>.mcp.json`. The file is written by
    /// [`pty_agent::start_pty_agent`] when the driver declares an MCP
    /// config via [`AgentDriver::mcp_config_json`].
    pub fn mcp_config_for(&self, agent_id: &str) -> PathBuf {
        self.sockets_dir.join(format!("{agent_id}.mcp.json"))
    }

    /// Clean up dead agents and reattach alive ones (from previous server runs).
    ///
    /// Every row in `running|starting` is either reattached or marked orphaned:
    /// - **PTY mode**: if we have a `supervisor_socket`, the daemon PID is
    ///   alive, and we can reconnect, the agent is live again. Any other
    ///   outcome (no socket recorded, PID dead, connect refused) → orphaned.
    /// - **Stream-JSON mode** (check agents): the server held the child's
    ///   stdin/stdout; those handles are gone after a crash and there is no
    ///   reattach path. Always orphaned.
    ///
    /// Broadcasting `agent_stopped` for each orphan is what lets the frontend
    /// unlock task cards — without it, a server crash mid-run leaves
    /// `taskLocked` true forever because the agent row never leaves the
    /// `running`/`starting` set.
    pub async fn cleanup_and_reattach(&self) {
        let rows: Vec<(String, Option<i64>, Option<String>, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = db
                .prepare(
                    "SELECT id, pid, supervisor_socket, mode FROM agents \
                     WHERE status IN ('running', 'starting')",
                )
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .flatten()
            .collect()
        };

        for (id, pid, socket, mode) in rows {
            // Stream-JSON agents have no supervisor: the server itself held
            // their stdin/stdout. After a restart those handles are gone —
            // even if the PID happens to still be alive (or has been reused)
            // we cannot talk to it. Orphan every row unconditionally.
            if mode != "pty" {
                self.mark_orphaned(&id, "stream-json agent: IO handles lost on server restart");
                continue;
            }

            let socket_path = match socket.as_deref() {
                Some(s) => PathBuf::from(s),
                None => {
                    self.mark_orphaned(&id, "no supervisor socket recorded");
                    continue;
                }
            };

            let alive = pid.map(process_alive).unwrap_or(false);
            if !alive {
                self.mark_orphaned(&id, &format!("supervisor PID {pid:?} not alive on boot"));
                continue;
            }

            if pty_agent::reattach_agent(self, &id, &socket_path, pid.unwrap_or(0) as u32).await {
                println!(
                    "[Branchwork] Reattached agent {} → socket {}",
                    &id[..8.min(id.len())],
                    socket_path.display()
                );
            } else {
                self.mark_orphaned(
                    &id,
                    &format!("supervisor socket unreachable: {}", socket_path.display()),
                );
            }
        }

        // Second pass: normalize task_status rows. A task stuck in `checking`
        // whose check-agent is no longer running is the most common wedged
        // state — the server died mid-check, the status row is orphaned, the
        // task card is effectively frozen. Revert to whatever the previous
        // status was (fall back to deleting the row if we can't infer).
        self.reconcile_task_statuses().await;

        // Third pass: clear `agents.branch` rows whose branch no longer
        // exists in the project's git. Out-of-band `git branch -D` leaves
        // the row pointing at a ghost; the merge banner keeps offering an
        // action that can't succeed.
        self.reconcile_orphaned_branches().await;
    }

    /// Revert `task_status` rows stuck in `checking` when their check-agent
    /// is no longer alive. Called by [`cleanup_and_reattach`] on boot.
    async fn reconcile_task_statuses(&self) {
        let stuck: Vec<(String, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = match db
                .prepare("SELECT plan_name, task_number FROM task_status WHERE status = 'checking'")
            {
                Ok(s) => s,
                Err(_) => return,
            };
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect())
                .unwrap_or_default()
        };

        for (plan_name, task_number) in stuck {
            // Is there any still-running check agent for this task?
            let live: Option<String> = {
                let db = self.db.lock().unwrap();
                db.query_row(
                    "SELECT id FROM agents \
                     WHERE plan_name = ?1 AND task_id = ?2 \
                           AND status IN ('running', 'starting') \
                     LIMIT 1",
                    rusqlite::params![plan_name, task_number],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            };
            if live.is_some() {
                continue; // agent still alive — leave the status alone
            }

            // Nothing alive. Drop the checking row so the task reverts to
            // whatever its task_status update history implied (or pending
            // when there's no prior row). Simpler and more honest than
            // guessing a fallback status.
            {
                let db = self.db.lock().unwrap();
                db.execute(
                    "DELETE FROM task_status \
                     WHERE plan_name = ?1 AND task_number = ?2 AND status = 'checking'",
                    rusqlite::params![plan_name, task_number],
                )
                .ok();
            }
            broadcast_event(
                &self.broadcast_tx,
                "task_status_changed",
                serde_json::json!({
                    "plan_name": plan_name,
                    "task_number": task_number,
                    "status": serde_json::Value::Null,
                    "reason": "boot_sweep: checking status reverted, check-agent not alive",
                }),
            );
            println!(
                "[Branchwork] Reverted stuck 'checking' on {plan_name}/{task_number} — \
                 no live check-agent"
            );
        }
    }

    /// Clear `agents.branch` for rows whose branch no longer resolves in
    /// the project's git. Fast-path: if nothing has a branch, skip entirely.
    async fn reconcile_orphaned_branches(&self) {
        let rows: Vec<(String, String, String)> = {
            let db = self.db.lock().unwrap();
            let mut stmt = match db.prepare(
                "SELECT id, branch, cwd FROM agents \
                 WHERE branch IS NOT NULL AND cwd IS NOT NULL AND cwd != ''",
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
        };

        for (agent_id, branch, cwd) in rows {
            let exists = std::process::Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .current_dir(&cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if exists {
                continue;
            }
            {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET branch = NULL WHERE id = ?1",
                    rusqlite::params![agent_id],
                )
                .ok();
            }
            broadcast_event(
                &self.broadcast_tx,
                "agent_branch_cleared",
                serde_json::json!({
                    "agent_id": agent_id,
                    "branch": branch,
                    "reason": "boot_sweep: branch not present in project git",
                }),
            );
            println!(
                "[Branchwork] Cleared orphaned branch {branch} on agent {} — \
                 not in project git",
                &agent_id[..8.min(agent_id.len())]
            );
        }
    }

    /// Mark an agent `failed` + stop_reason='supervisor_unreachable' when the
    /// heartbeat loop in [`pty_agent::spawn_heartbeat_task`] sees three
    /// consecutive Pings go unanswered. Evicts the live registry entry so
    /// the UI can't pretend the agent is still there, updates the DB, and
    /// broadcasts a stop event so task cards unlock in real-time.
    pub async fn mark_supervisor_unreachable(&self, agent_id: &str) {
        // Drop the live session. Any still-open reader/writer on the same
        // ManagedAgent will wind down on their own once they hit the closed
        // channel or connection.
        self.agents.lock().await.remove(agent_id);
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', \
                     stop_reason = 'supervisor_unreachable', \
                     finished_at = datetime('now') \
                 WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": agent_id,
                "status": "failed",
                "stop_reason": "supervisor_unreachable",
                "reason": "supervisor did not respond to 3 consecutive pings (~45s)",
            }),
        );
        println!(
            "[Branchwork] Agent {} marked unreachable — heartbeat timeout",
            &agent_id[..8.min(agent_id.len())]
        );
    }

    /// Mark an agent `failed` + stop_reason='orphaned' and broadcast the
    /// change so any connected browser unlocks the task card without
    /// waiting for a manual refresh. Used by cleanup_and_reattach when the
    /// daemon/child PID is not alive — the row was from a previous server
    /// run whose supervisor did not survive.
    fn mark_orphaned(&self, agent_id: &str, detail: &str) {
        {
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', \
                     stop_reason = 'orphaned', \
                     finished_at = datetime('now') \
                 WHERE id = ? AND status IN ('running', 'starting')",
                rusqlite::params![agent_id],
            )
            .ok();
        }
        broadcast_event(
            &self.broadcast_tx,
            "agent_stopped",
            serde_json::json!({
                "id": agent_id,
                "status": "failed",
                "stop_reason": "orphaned",
                "reason": detail,
            }),
        );
        println!(
            "[Branchwork] Agent {} marked orphaned — {detail}",
            &agent_id[..8.min(agent_id.len())]
        );
    }

    /// Ask the agent's CLI to exit cleanly by sending its driver-specific
    /// exit sequence (e.g. `/exit\r` for Claude Code) through the PTY.
    /// Returns true if the sequence was sent; false if the agent is not
    /// live in-process or the driver doesn't support clean exit.
    pub async fn graceful_exit(&self, agent_id: &str) -> bool {
        // Look up the driver for this agent so we know what to send.
        let driver_name: Option<String> = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT driver FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        let driver_name = driver_name.unwrap_or_else(|| driver::DEFAULT_DRIVER.to_string());
        let exit_bytes: Vec<u8> = match self.drivers.get(&driver_name) {
            Some(d) => match d.graceful_exit_sequence() {
                Some(b) => b.to_vec(),
                None => return false,
            },
            None => return false,
        };

        let agents = self.agents.lock().await;
        match agents.get(agent_id) {
            Some(agent) => {
                let _ = agent.command_tx.send(SessionMessage::Input(exit_bytes));
                true
            }
            None => false,
        }
    }

    pub async fn kill_agent(&self, agent_id: &str) -> bool {
        // Live agent: send Kill over the control channel. The supervisor
        // propagates the kill to the PTY's child, closes the socket, and
        // the reader task we spawned in start_pty_agent will update the
        // DB + broadcast `agent_stopped` when it sees EOF.
        let maybe_agent = {
            let mut agents = self.agents.lock().await;
            agents.remove(agent_id)
        };
        if let Some(agent) = maybe_agent {
            let _ = agent.command_tx.send(SessionMessage::Kill);
            // Ensure the DB is flipped promptly even if the reader task is
            // slow to observe EOF. Also clear the branch so a killed agent
            // doesn't keep advertising itself as mergeable in the UI.
            {
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET status = 'killed', finished_at = datetime('now'), branch = NULL WHERE id = ? AND status IN ('running', 'starting')",
                    rusqlite::params![agent_id],
                )
                .ok();
            }
            broadcast_event(
                &self.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": agent_id, "status": "killed"}),
            );
            // Fall through: also try the PID-based fallback below in case
            // the daemon's socket is wedged.
            let pid = {
                let db = self.db.lock().unwrap();
                db.query_row(
                    "SELECT pid FROM agents WHERE id = ?",
                    rusqlite::params![agent_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
            };
            if let Some(p) = pid {
                process_terminate(p);
            }
            return true;
        }

        // Not in the in-process registry — still try to make sure the
        // daemon (if any) is dead, and mark the row killed.
        let (socket, pid) = {
            let db = self.db.lock().unwrap();
            db.query_row(
                "SELECT supervisor_socket, pid FROM agents WHERE id = ?",
                rusqlite::params![agent_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
            )
            .unwrap_or((None, None))
        };
        if let Some(p) = pid {
            process_terminate(p);
        }
        if let Some(s) = socket.as_deref() {
            // Clean up stale socket / pidfile / log siblings so a future
            // restart isn't confused by them. Best-effort only.
            let socket = Path::new(s);
            let _ = std::fs::remove_file(socket);
            let _ = std::fs::remove_file(supervisor::pidfile_path(socket));
        }
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET status = 'killed', finished_at = datetime('now'), branch = NULL WHERE id = ?",
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

/// Outcome of [`check_tree_clean_for_completion`]: the working tree is
/// either clean (good — we can mark `completed`), or dirty (reject the
/// status change with a helpful error), or we couldn't tell (missing
/// project dir, no git, git errored — treat as clean; the merge-time
/// empty-branch guard is the backstop).
pub enum TreeState {
    Clean,
    Dirty { files: Vec<String> },
    Unknown,
}

/// Refuse to mark a task `completed` if the project's working tree has
/// uncommitted changes. Closes the hole where an agent modifies files,
/// calls `update_task_status(completed)`, and exits — leaving the changes
/// to be silently stepped on by the next agent.
///
/// Returns `Dirty` with the first few changed paths for a useful error
/// message, `Clean` when it's safe to proceed, and `Unknown` when we
/// can't introspect (no project dir, not a git repo, etc) — the caller
/// should treat that as permissive because the merge-time empty-branch
/// guard still catches the case where nothing was ever committed.
pub fn check_tree_clean_for_completion(
    db: &Db,
    plans_dir: &std::path::Path,
    plan_name: &str,
) -> TreeState {
    let Some(cwd) = crate::ci::project_dir_for(plans_dir, db, plan_name) else {
        return TreeState::Unknown;
    };
    // `git status --porcelain` is the canonical "is this tree clean?"
    // probe. `--untracked-files=no` is the key bit: the failure mode we're
    // catching is "agent modified tracked files but didn't commit", not
    // "editor left a swap file lying around". Untracked files are noise
    // agents can't reason about — gitignore or stash is the user's call,
    // not something to block task completion on.
    let out = match std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&cwd)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return TreeState::Unknown,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return TreeState::Clean;
    }
    // Porcelain lines are "XY path" with a 3-char prefix; slice past it.
    let files: Vec<String> = lines
        .iter()
        .take(10)
        .map(|l| l.get(3..).unwrap_or(l).to_string())
        .collect();
    TreeState::Dirty { files }
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
    mcp_available: bool,
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

    // Drivers that auto-register the Branchwork MCP server (currently just
    // Claude) get a terser "call the MCP tool" instruction; others fall back
    // to a curl against the HTTP API, which is always live as a backstop.
    let status_step = if mcp_available {
        format!(
            "4. Mark the task status by calling the `update_task_status` MCP tool \
             (from the `branchwork` server) with {{\"plan\": \"{plan_name}\", \
             \"task\": \"{task_num}\", \"status\": \"completed\"}}",
            plan_name = plan.name,
            task_num = task.number,
        )
    } else {
        format!(
            "4. Mark the task status by running: curl -s -X PUT \
             http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/status \
             -H \"Content-Type: application/json\" -d '{{\"status\":\"completed\"}}'",
            port = port,
            plan_name = plan.name,
            task_num = task.number,
        )
    };

    let task_branch = format!("branchwork/{}/{}", plan.name, task.number);
    let contract = prompt::unattended_contract_block(&task_branch);

    format!(
        "{intro}\n\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Description:\n{description}\n\
         {files}{acceptance}\n\
         {context}\n\
         {instruction}\n\n\
         {contract}\n\n\
         IMPORTANT: When you think you are done, do NOT stop. Instead:\n\
         1. Summarize what you did\n\
         2. Record one short learning other tasks in this project should know (file paths established, key decisions, gotchas) by running: curl -s -X POST http://localhost:{port}/api/plans/{plan_name}/tasks/{task_num}/learnings -H \"Content-Type: application/json\" -d '{{\"learning\":\"...\"}}'\n\
         3. Commit your changes with `git add -A && git commit -m '<short summary>'`. You MUST do this before marking the task completed — merges only carry committed work, and uncommitted changes are silently dropped when the next agent checks out the task branch.\n\
         {status_step}\n\
         5. Ask the user if they need anything else\n\
         6. Only stop when the user explicitly says they are done",
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
        contract = contract,
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
/// Atomically claim a task slot for the given `(plan, task)` and gate the
/// claim on the plan being idle (no other task agent is `starting` /
/// `running`).
///
/// **Why the idle gate:** without it, two concurrent `try_auto_advance`
/// calls — one per CI-passed completion when several phase-N tasks
/// finish close together — each pick a *different* ready task in phase
/// N+1 and each win their own claim race, spawning two agents
/// simultaneously. The intra-call `break` in `spawn_ready_tasks` only
/// gates within one advance call; this `NOT EXISTS` predicate gates
/// across them.
///
/// Check agents (`mode = 'stream-json'`) are excluded — they're
/// read-only verification runs that don't touch the worktree, so they
/// shouldn't block auto-advance. Fix agents (`mode = 'pty'`,
/// `task_id` matches `<task>-fix-<n>`) **are** counted, which gives us
/// the desired "no new task spawns while a fix is in flight" behaviour
/// for free — they appear in `agents` with the same `plan_name` and a
/// `running`-class status.
///
/// The check and the claim live in one SQL statement so SQLite's
/// per-write serialisation gives us race-freedom without a tokio mutex.
fn claim_task(db: &Db, plan_name: &str, task_number: &str) -> bool {
    let conn = db.lock().unwrap();
    let updated = conn
        .execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at)
             SELECT ?1, ?2, 'in_progress', datetime('now')
             WHERE NOT EXISTS (
                 SELECT 1 FROM agents
                 WHERE plan_name = ?1
                   AND status IN ('starting', 'running')
                   AND mode != 'stream-json'
             )
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = excluded.status, updated_at = excluded.updated_at
             WHERE task_status.status IN ('pending', 'failed')
               AND NOT EXISTS (
                   SELECT 1 FROM agents
                   WHERE plan_name = ?1
                     AND status IN ('starting', 'running')
                     AND mode != 'stream-json'
               )",
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
    // Either auto-advance (phase-level opt-in) or auto-mode (the
    // merge-CI-advance loop's master gate, which already excludes paused
    // plans via paused_reason IS NULL) is enough to advance. The
    // auto-mode loop relies on this branch to spawn the next phase's
    // tasks after a green CI without requiring a separate auto_advance
    // toggle.
    if !auto_advance_enabled(&registry.db, &plan_name)
        && !crate::db::auto_mode_enabled(&registry.db, &plan_name)
    {
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

    // Done-set, work_dir, and budget are needed for both the intra-phase
    // and next-phase paths, so compute them up front. Budget check returns
    // early if exhausted — same as pre-3.1 behaviour, just hoisted.
    let done_set = {
        let conn = registry.db.lock().unwrap();
        completed_task_numbers(&conn, &plan_name)
    };

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

    // Intra-phase scan: are any tasks in the *current* phase newly ready
    // (pending/failed with all deps satisfied)? If so, spawn them and
    // broadcast `task_advanced` — no phase boundary crossed.
    let intra_ready: Vec<&PlanTask> = current_phase
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

    if !intra_ready.is_empty() {
        let spawned = spawn_ready_tasks(
            &registry,
            &plans_dir,
            &plan,
            current_phase,
            &intra_ready,
            effort,
            port,
            max_budget_usd,
            &work_dir,
        )
        .await;

        // `task_advanced` only fires if we actually spawned at least one
        // task. If every `intra_ready` candidate lost the `claim_task`
        // race (concurrent auto-advance trigger), stay quiet — the other
        // trigger already broadcast.
        if !spawned.is_empty() {
            broadcast_event(
                &registry.broadcast_tx,
                "task_advanced",
                serde_json::json!({
                    "plan": plan_name,
                    "from_task": completed_task_number,
                    "to_tasks": spawned,
                }),
            );
        }
        return;
    }

    // No ready tasks in the current phase. Either the phase is fully done
    // (every task completed/skipped) — fall through to the next-phase
    // scan — or some tasks are still in_progress (stuck dep chain),
    // in which case we wait for them to finish.
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

    spawn_ready_tasks(
        &registry,
        &plans_dir,
        &plan,
        next_phase,
        &ready_tasks,
        effort,
        port,
        max_budget_usd,
        &work_dir,
    )
    .await;

    broadcast_event(
        &registry.broadcast_tx,
        "phase_advanced",
        serde_json::json!({
            "plan_name": plan_name,
            "from_phase": current_phase.number,
            "to_phase": next_phase.number,
        }),
    );

    let webhook_snapshot = registry.webhook_url.read().unwrap().clone();
    if webhook_snapshot.is_some() {
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
        crate::notifications::notify(webhook_snapshot, msg);
    }
}

/// Spawn at most one auto-advance agent from `tasks`, a pre-filtered
/// list of ready candidates from `phase`. We iterate in order and break
/// after the first successful `claim_task` + `start_pty_agent` — this
/// is the unconditional sequential gate (Task 3.5.1): one agent per
/// `try_auto_advance` call, always. The next ready task in the chain
/// spawns when the current task's completion fires another
/// `try_auto_advance` (via `on_task_agent_completed` for auto-mode
/// plans / `task_status_changed` for auto-advance plans).
///
/// Returns the task numbers we actually claimed and spawned (length 0
/// or 1) — callers use this to gate any aggregate follow-up broadcast
/// (`task_advanced` for intra-phase advance stays quiet when every
/// candidate lost its claim race).
///
/// The `parallel: bool` per-task column (Phase 3.5.2) is what flips
/// this from sequential to fan-out, but only when both the column AND
/// the worktree cross-check (3.5.3) say yes. Today neither holds, so
/// this loop break is the load-bearing safety against parallel agents
/// stomping on a single working tree.
#[allow(clippy::too_many_arguments)]
async fn spawn_ready_tasks(
    registry: &AgentRegistry,
    plans_dir: &Path,
    plan: &ParsedPlan,
    phase: &PlanPhase,
    tasks: &[&PlanTask],
    effort: Effort,
    port: u16,
    max_budget_usd: Option<f64>,
    work_dir: &Path,
) -> Vec<String> {
    let mut spawned: Vec<String> = Vec::with_capacity(1);
    for task in tasks {
        if !claim_task(&registry.db, &plan.name, &task.number) {
            continue;
        }

        broadcast_event(
            &registry.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": plan.name,
                "task_number": task.number,
                "status": "in_progress",
            }),
        );

        let cross_ctx = build_cross_plan_context(&registry.db, plans_dir, plan, &task.number);
        // Auto-advance spawns use the default driver; check whether it
        // auto-registers MCP so the prompt picks MCP tool vs curl.
        let mcp_available = registry.drivers.injects_mcp(None, port);
        let prompt = build_task_prompt(
            plan,
            phase,
            task,
            false,
            port,
            cross_ctx.as_deref(),
            mcp_available,
        );
        let branch_name = format!("branchwork/{}/{}", plan.name, task.number);

        pty_agent::start_pty_agent(
            registry,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: work_dir,
                plan_name: Some(&plan.name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd,
                driver: None,
                user_id: None,
                org_id: None,
            },
        )
        .await;

        spawned.push(task.number.clone());
        // Sequential gate: one agent per `try_auto_advance` call.
        // See doc comment above for the full rationale.
        break;
    }
    spawned
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

    /// Regression: two CI-passed completions arriving close together in
    /// the auto-mode loop both reach `try_auto_advance` after their
    /// respective tasks completed. Without this gate, each call picks a
    /// *different* ready task in the next phase and both spawn — two
    /// concurrent agents on a single working tree. The DB-side
    /// `NOT EXISTS` predicate makes the second call lose the claim
    /// attempt without needing a tokio mutex around the whole advance.
    fn insert_running_agent(db: &Db, id: &str, plan_name: &str, task_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES (?1, '/tmp/proj', 'running', 'pty', ?2, ?3)",
            params![id, plan_name, task_id],
        )
        .unwrap();
    }

    #[test]
    fn claim_task_defers_when_sibling_agent_is_running_for_same_plan() {
        let (db, _dir) = fresh_db();
        insert_running_agent(&db, "agent-A", "p1", "1.1");

        // Sibling task in the same plan must NOT claim while agent-A
        // runs — the per-plan idle gate is what keeps auto-advance
        // strictly sequential across concurrent advance triggers.
        assert!(!claim_task(&db, "p1", "1.2"));

        // Different plan is unaffected — the gate is per-plan.
        assert!(claim_task(&db, "p2", "1.1"));

        // Once agent-A is no longer 'running'/'starting', sibling
        // claims succeed.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = 'completed' WHERE id = 'agent-A'",
                [],
            )
            .unwrap();
        }
        assert!(claim_task(&db, "p1", "1.2"));
    }

    #[test]
    fn claim_task_ignores_check_agents_in_idle_gate() {
        let (db, _dir) = fresh_db();
        // Check agents (mode='stream-json') are read-only verification
        // runs that don't touch the worktree, so they shouldn't gate
        // auto-advance — assert the predicate's `mode != 'stream-json'`
        // exclusion holds.
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES ('check-1', '/tmp/proj', 'running', 'stream-json', 'p1', '1.1')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(claim_task(&db, "p1", "1.2"));
    }

    #[test]
    fn claim_task_defers_for_starting_agents_too_not_just_running() {
        let (db, _dir) = fresh_db();
        // The window between agent-row insert and the spawn-side
        // status flip to 'running' is short but real; the gate must
        // see 'starting' as in-flight too, otherwise two advances can
        // both squeeze through during that window.
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, plan_name, task_id)
             VALUES ('agent-A', '/tmp/proj', 'starting', 'pty', 'p1', '1.1')",
            [],
        )
        .unwrap();
        drop(conn);

        assert!(!claim_task(&db, "p1", "1.2"));
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

    fn sample_plan_for_prompt() -> (ParsedPlan, PlanPhase, PlanTask) {
        let task = PlanTask {
            number: "2.6".to_string(),
            title: "Update agent prompts".to_string(),
            description: "Replace curl with MCP tools.".to_string(),
            file_paths: vec![],
            acceptance: String::new(),
            dependencies: vec![],
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        };
        let phase = PlanPhase {
            number: 2,
            title: "MCP Server".to_string(),
            description: String::new(),
            tasks: vec![task.clone()],
        };
        let plan = ParsedPlan {
            name: "portable-agents".to_string(),
            file_path: String::new(),
            title: "Portable agents".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![phase.clone()],
            verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        };
        (plan, phase, task)
    }

    #[test]
    fn build_task_prompt_uses_mcp_tool_when_available() {
        let (plan, phase, task) = sample_plan_for_prompt();
        let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, true);
        // MCP branch: tool name appears, curl PUT does not.
        assert!(
            prompt.contains("update_task_status"),
            "expected MCP tool mention, got: {prompt}"
        );
        assert!(
            !prompt.contains("curl -s -X PUT"),
            "curl PUT should be omitted when MCP is available: {prompt}"
        );
        // Learnings curl (step 2) is still there regardless of MCP.
        assert!(
            prompt.contains("curl -s -X POST"),
            "learnings curl should remain: {prompt}"
        );
    }

    #[test]
    fn build_task_prompt_falls_back_to_curl_when_mcp_unavailable() {
        let (plan, phase, task) = sample_plan_for_prompt();
        let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, false);
        // Curl branch: PUT appears, MCP tool does not.
        assert!(
            prompt.contains("curl -s -X PUT"),
            "expected curl PUT fallback, got: {prompt}"
        );
        assert!(
            !prompt.contains("update_task_status"),
            "MCP tool should not be mentioned when unavailable: {prompt}"
        );
    }

    #[test]
    fn build_task_prompt_embeds_unattended_contract_block() {
        let (plan, phase, task) = sample_plan_for_prompt();
        // Both MCP and curl branches must carry the contract — auto-mode
        // can spawn either flavour and the no-push / no-ask rules apply
        // uniformly.
        for mcp in [true, false] {
            let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, mcp);
            assert!(
                prompt.contains("Unattended-execution contract"),
                "contract heading missing (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("branchwork/portable-agents/2.6"),
                "task branch must be interpolated literally (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("Do not run `git push`"),
                "no-push rule missing (mcp={mcp}): {prompt}"
            );
            assert!(
                prompt.contains("Do not ask the user"),
                "no-ask rule missing (mcp={mcp}): {prompt}"
            );
        }
    }

    #[test]
    fn build_task_prompt_mandates_commit_before_status_update() {
        let (plan, phase, task) = sample_plan_for_prompt();
        for mcp in [true, false] {
            let prompt = build_task_prompt(&plan, &phase, &task, false, 3100, None, mcp);
            assert!(
                prompt.contains("git add -A && git commit"),
                "expected commit mandate (mcp={mcp}): {prompt}"
            );
            let commit_idx = prompt.find("git add -A && git commit").unwrap();
            let status_idx = prompt
                .find("Mark the task status")
                .expect("status step present");
            assert!(
                commit_idx < status_idx,
                "commit step must precede status step (mcp={mcp})"
            );
        }
    }

    /// Build a registry wired to a fresh DB + an in-memory broadcast channel
    /// so cleanup_and_reattach has everything it needs to run without
    /// touching the filesystem or network.
    fn test_registry(db: Db) -> (AgentRegistry, tokio::sync::broadcast::Receiver<String>) {
        let (tx, rx) = tokio::sync::broadcast::channel::<String>(32);
        let registry = AgentRegistry::new(
            db,
            tx,
            None,
            PathBuf::from("/tmp/branchwork-test-sockets"),
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        (registry, rx)
    }

    fn insert_agent(
        db: &Db,
        id: &str,
        mode: &str,
        status: &str,
        pid: Option<i64>,
        socket: Option<&str>,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, mode, pid, supervisor_socket) \
             VALUES (?1, '/tmp', ?2, ?3, ?4, ?5)",
            params![id, status, mode, pid, socket],
        )
        .unwrap();
    }

    fn agent_status(db: &Db, id: &str) -> (String, Option<String>) {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status, stop_reason FROM agents WHERE id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap()
    }

    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    #[tokio::test]
    async fn cleanup_orphans_pty_row_without_supervisor_socket() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // PTY agent that never recorded a socket (pre-upgrade / crashed mid-spawn).
        insert_agent(&db, "agent-no-sock", "pty", "running", Some(1), None);
        registry.cleanup_and_reattach().await;

        let (status, reason) = agent_status(&db, "agent-no-sock");
        assert_eq!(status, "failed", "expected status=failed");
        assert_eq!(reason.as_deref(), Some("orphaned"));

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains("agent-no-sock")
                && e.contains("orphaned")),
            "expected agent_stopped/orphaned broadcast, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_orphans_stream_json_agent_regardless_of_pid() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Use PID 1 (init) so process_alive returns true on Unix — we want to
        // prove stream-json rows are orphaned even when the PID is alive,
        // because the server lost the stdin/stdout handles.
        insert_agent(&db, "check-alive", "stream-json", "running", Some(1), None);
        // And a starting row with a dead PID, for good measure.
        insert_agent(
            &db,
            "check-dead",
            "stream-json",
            "starting",
            Some(0x7fff_ffff),
            None,
        );

        registry.cleanup_and_reattach().await;

        for id in ["check-alive", "check-dead"] {
            let (status, reason) = agent_status(&db, id);
            assert_eq!(status, "failed", "{id}: expected failed");
            assert_eq!(
                reason.as_deref(),
                Some("orphaned"),
                "{id}: expected orphaned"
            );
        }

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("check-alive") && e.contains("orphaned")),
            "missing orphan broadcast for check-alive: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.contains("check-dead") && e.contains("orphaned")),
            "missing orphan broadcast for check-dead: {events:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_orphans_pty_row_with_dead_supervisor_pid() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Socket present but PID very unlikely to be alive. i32::MAX is above
        // the typical pid_max; kill(0) will return ESRCH.
        insert_agent(
            &db,
            "pty-dead",
            "pty",
            "running",
            Some(i32::MAX as i64),
            Some("/tmp/branchwork-test-sockets/does-not-exist.sock"),
        );

        registry.cleanup_and_reattach().await;

        let (status, reason) = agent_status(&db, "pty-dead");
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("orphaned"));

        let events = drain_events(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e.contains("pty-dead") && e.contains("orphaned")),
            "missing orphan broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn mark_supervisor_unreachable_flips_running_row_and_broadcasts() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        insert_agent(
            &db,
            "wedged",
            "pty",
            "running",
            Some(1),
            Some("/tmp/w.sock"),
        );
        registry.mark_supervisor_unreachable("wedged").await;

        let (status, reason) = agent_status(&db, "wedged");
        assert_eq!(status, "failed");
        assert_eq!(reason.as_deref(), Some("supervisor_unreachable"));

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| e.contains("agent_stopped")
                && e.contains("wedged")
                && e.contains("supervisor_unreachable")),
            "expected agent_stopped/supervisor_unreachable broadcast: {events:?}"
        );
    }

    #[tokio::test]
    async fn mark_supervisor_unreachable_leaves_terminal_rows_alone() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Already-killed agent: the heartbeat shouldn't overwrite the row.
        insert_agent(&db, "already-killed", "pty", "killed", Some(1), None);
        registry.mark_supervisor_unreachable("already-killed").await;

        let (status, reason) = agent_status(&db, "already-killed");
        assert_eq!(status, "killed", "terminal status must not be overwritten");
        assert_eq!(reason.as_deref(), None);

        // The broadcast still fires (the method doesn't read back the DB to
        // decide), but what matters is that the row survives. Drain to keep
        // the channel tidy.
        let _ = drain_events(&mut rx);
    }

    #[tokio::test]
    async fn cleanup_leaves_non_running_rows_alone() {
        let (db, _dir) = fresh_db();
        let (registry, mut rx) = test_registry(db.clone());

        // Rows already in a terminal state must not be rewritten.
        insert_agent(&db, "done", "pty", "completed", Some(1), None);
        insert_agent(&db, "killed", "pty", "killed", Some(1), None);

        registry.cleanup_and_reattach().await;

        assert_eq!(agent_status(&db, "done").0, "completed");
        assert_eq!(agent_status(&db, "killed").0, "killed");
        // reconcile passes run and may emit unrelated events; what matters is
        // that neither completed nor killed got touched.
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| (e.contains("\"id\":\"done\"")
                || e.contains("\"id\":\"killed\""))
                && e.contains("agent_stopped")),
            "terminal rows re-broadcast: {events:?}"
        );
    }

    // `git_default_branch` / `git_list_branches` tests live alongside the
    // implementations in `crate::git_helpers` — they moved out with the
    // helpers when the leaf module was carved out for the runner binary.

    // ── try_auto_advance tests (Task 3.4) ─────────────────────────────
    //
    // Pin the post-3.1/3.2 contract: `try_auto_advance` scans the
    // current phase first (intra-phase advance) and only crosses a
    // phase boundary when the current phase is fully done.
    //
    // We let `start_pty_agent` run end-to-end through `test_registry`:
    // the supervisor spawn fails fast (server_exe is /nonexistent),
    // but the `agents` row is INSERTed before that point — same
    // insert-then-fail pattern the recovery / Phase 1 integration
    // tests rely on. Each spawn writes a per-session
    // `~/.claude/sessions/<session_id>.settings.json` (claude is the
    // test_registry default driver) which we clean up via
    // `SettingsCleanup`'s Drop guard so test runs don't accumulate
    // stale files in the developer's $HOME.

    /// Insert (or upsert) a `task_status` row to a fixed status.
    /// Bypasses `claim_task`'s `IN ('pending','failed')` gate so a
    /// test can pre-stage states the loop normally arrives at via
    /// spawn (in_progress, completed).
    fn set_task_status(db: &Db, plan_name: &str, task_number: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, updated_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, task_number) DO UPDATE SET \
                status = excluded.status, updated_at = excluded.updated_at",
            params![plan_name, task_number, status],
        )
        .unwrap();
    }

    fn enable_auto_mode(db: &Db, plan_name: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled, paused_reason, paused_at) \
             VALUES (?1, 1, NULL, NULL)",
            params![plan_name],
        )
        .unwrap();
    }

    fn enable_auto_advance(db: &Db, plan_name: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
            params![plan_name],
        )
        .unwrap();
    }

    /// Distinct `task_id` values currently on `agents` rows for
    /// `plan_name`. Each successful spawn through `start_pty_agent`
    /// inserts one row, so this is the captured "invocation list" for
    /// `spawn_ready_tasks`.
    fn agent_task_ids(db: &Db, plan_name: &str) -> std::collections::HashSet<String> {
        let conn = db.lock().unwrap();
        conn.prepare(
            "SELECT task_id FROM agents \
             WHERE plan_name = ?1 AND task_id IS NOT NULL",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))?
                .collect::<Result<std::collections::HashSet<_>, _>>()
        })
        .unwrap_or_default()
    }

    /// `try_auto_advance` calls `start_pty_agent`, which writes a
    /// per-session `~/.claude/sessions/<session_id>.settings.json` for
    /// the claude driver. The supervisor spawn then fails (server_exe
    /// is /nonexistent), so `on_agent_exit` never fires to clean up.
    /// This Drop covers the leak so test runs don't accumulate stale
    /// settings files in the developer's $HOME.
    struct SettingsCleanup {
        db: Db,
        plan_name: String,
    }

    impl Drop for SettingsCleanup {
        fn drop(&mut self) {
            let Some(home) = dirs::home_dir() else {
                return;
            };
            let session_ids: Vec<String> = {
                let Ok(conn) = self.db.lock() else { return };
                conn.prepare(
                    "SELECT session_id FROM agents \
                     WHERE plan_name = ?1 AND session_id IS NOT NULL",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(params![&self.plan_name], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_default()
            };
            for sid in session_ids {
                let _ = session_settings::delete_for_agent_with_home(&home, &sid);
            }
        }
    }

    /// Write a YAML plan whose `project` field resolves (via
    /// `dirs::home_dir().join(...)` inside `try_auto_advance`) to a
    /// path that is NOT a real git working tree —
    /// `git_checkout_branch` inside `start_pty_agent` then fails
    /// harmlessly instead of polluting the test runner's repo.
    fn write_advance_plan(plans_dir: &Path, plan_name: &str, phases_yaml: &str) {
        let path = plans_dir.join(format!("{plan_name}.yaml"));
        let yaml = format!(
            "title: \"{plan_name} title\"\n\
             project: branchwork-no-such-dir-{plan_name}\n\
             phases:\n{phases_yaml}"
        );
        std::fs::write(path, yaml).unwrap();
    }

    fn has_task_advanced(events: &[String], plan_name: &str) -> bool {
        events.iter().any(|e| {
            e.contains("\"type\":\"task_advanced\"")
                && e.contains(&format!("\"plan\":\"{plan_name}\""))
        })
    }

    fn has_phase_advanced(events: &[String], plan_name: &str) -> bool {
        events.iter().any(|e| {
            e.contains("\"type\":\"phase_advanced\"")
                && e.contains(&format!("\"plan_name\":\"{plan_name}\""))
        })
    }

    /// Acceptance-criteria spec: a 5-task linear chain (a→b→c→d→e)
    /// advances exactly one task per `try_auto_advance` invocation.
    /// Pre-stage `a=completed`; each tick must spawn exactly the next
    /// link, broadcast `task_advanced`, and never `phase_advanced`.
    #[tokio::test]
    async fn linear_intra_phase_chain_completes_one_task_per_completion() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "lin-chain";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"b\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"c\"]\n\
             \x20     - number: \"e\"\n\
             \x20       title: E\n\
             \x20       dependencies: [\"d\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // a was completed externally; the loop must spawn exactly b on
        // the next tick, then c, d, e — one new spawn per completion.
        set_task_status(&db, plan, "a", "completed");

        let chain = [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")];
        let mut expected: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut prior_total = 0usize;

        for (from, to) in chain {
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
            )
            .await;

            expected.insert(to.to_string());
            let actual = agent_task_ids(&db, plan);
            assert_eq!(
                actual, expected,
                "after completing {from}, agents rows should be {expected:?}, got {actual:?}",
            );
            let total = actual.len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "exactly one new spawn at {from}->{to} (delta {delta})",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            assert!(
                has_task_advanced(&events, plan),
                "expected task_advanced after {from}->{to}, events: {events:?}",
            );
            assert!(
                !has_phase_advanced(&events, plan),
                "phase_advanced should not fire intra-phase ({from}->{to}): {events:?}",
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.contains(&format!("\"to_tasks\":[\"{to}\"]"))),
                "task_advanced payload should list {to:?}: {events:?}",
            );

            // Mark `to` completed so the next tick treats it as done.
            set_task_status(&db, plan, to, "completed");
        }
    }

    /// Eligibility filter: when `b` completes but `e` still has an
    /// in-progress blocker (`d`), the intra-phase scan must not spawn
    /// `e`. Once `d` completes too, the next tick spawns `e` and only
    /// `e`. (Fan-out where multiple siblings become ready at the same
    /// tick is covered by Phase 3.5 — kept out of this test.)
    #[tokio::test]
    async fn mixed_deps_within_phase_only_spawns_when_blockers_done() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "mixed-deps";
        // Deps: a -> b; c -> d; e depends on {b, d}.
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"c\"]\n\
             \x20     - number: \"e\"\n\
             \x20       title: E\n\
             \x20       dependencies: [\"b\", \"d\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Pre-stage so the first try_auto_advance call has nothing
        // ready (a/b/c done, d in_progress, e blocked on d). a/c must
        // be past pending so they aren't candidates either — net spawn
        // count = 0 on the b-completion tick.
        set_task_status(&db, plan, "a", "completed");
        set_task_status(&db, plan, "b", "completed");
        set_task_status(&db, plan, "c", "completed");
        set_task_status(&db, plan, "d", "in_progress");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "b".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        assert!(
            agent_task_ids(&db, plan).is_empty(),
            "no spawn expected — e is still blocked on d (in_progress)",
        );
        let events = drain_events(&mut rx);
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must not fire when nothing was spawned: {events:?}",
        );
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must not fire (phase still has in_progress + pending tasks): {events:?}",
        );

        // d completes → e becomes ready.
        set_task_status(&db, plan, "d", "completed");
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "d".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["e".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "e should spawn once both b and d are done, got {actual:?}",
        );
        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced expected for e: {events:?}",
        );
        assert!(
            events.iter().any(|e| e.contains("\"to_tasks\":[\"e\"]")),
            "task_advanced payload should list e: {events:?}",
        );
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced should not fire intra-phase: {events:?}",
        );
    }

    /// Phase boundary still uses the next-phase path: with phase 1
    /// fully done, the tick spawns 2.1 and broadcasts
    /// `phase_advanced`. `task_advanced` must NOT fire on this path —
    /// the two events are mutually exclusive per ADR 0003 §3.
    #[tokio::test]
    async fn phase_boundary_still_fires_phase_advanced() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "phase-boundary";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: One-Two\n\
             \x20       dependencies: [\"1.1\"]\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20       dependencies: [\"2.1\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Phase 1 fully done: 1.1 + 1.2 completed.
        set_task_status(&db, plan, "1.1", "completed");
        set_task_status(&db, plan, "1.2", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.2".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        // 2.1 spawns (no deps); 2.2 stays blocked on 2.1.
        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["2.1".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "phase boundary should spawn 2.1 only (2.2 still blocked on 2.1), got {actual:?}",
        );

        let events = drain_events(&mut rx);
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced expected on phase boundary: {events:?}",
        );
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must not fire on the phase-boundary path: {events:?}",
        );
    }

    /// Two-opt-in independence (non-negotiable #10): with
    /// `plan_auto_advance.enabled=1` and no `plan_auto_mode` row,
    /// intra-phase advance must still fire. Pins that the gate at the
    /// top of `try_auto_advance` is OR, not AND, of the two opt-ins.
    #[tokio::test]
    async fn auto_advance_only_no_auto_mode_still_advances_intra_phase() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "advance-only";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n",
        );
        // Critical: enable plan_auto_advance only — no plan_auto_mode
        // row. The two opt-ins must remain independent.
        enable_auto_advance(&db, plan);
        assert!(
            auto_advance_enabled(&db, plan),
            "auto_advance should be on for this test",
        );
        assert!(
            !crate::db::auto_mode_enabled(&db, plan),
            "auto_mode must NOT be enabled for this test",
        );

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["b".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "auto_advance alone should spawn b after a completes, got {actual:?}",
        );
        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced expected with auto_advance-only opt-in: {events:?}",
        );
    }

    /// Task 3.5.1 — sequential gate: when several siblings become
    /// ready at the same tick (fan-out), `spawn_ready_tasks` must
    /// break after the first successful claim. The remaining ready
    /// tasks spawn on subsequent `try_auto_advance` ticks driven by
    /// the first task's completion. This is the load-bearing safety
    /// today (no `parallel:` column or worktree cross-check yet) so
    /// pin it explicitly: one agent per call, regardless of how many
    /// candidates the eligibility filter produced.
    #[tokio::test]
    async fn fan_out_only_spawns_one_task_per_tick() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "fan-out";
        // Three siblings (b, c, d) all depend on a — completing a
        // makes all three ready at the same tick.
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"a\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        // Exactly one of {b, c, d} spawned — the order is the YAML
        // declaration order so `b` is the deterministic pick, but we
        // assert on the count and the membership to keep the test
        // robust against future ordering tweaks in the eligibility
        // filter.
        let actual = agent_task_ids(&db, plan);
        assert_eq!(
            actual.len(),
            1,
            "exactly one task must spawn per tick (fan-out gate), got {actual:?}",
        );
        assert!(
            actual.iter().all(|t| ["b", "c", "d"].contains(&t.as_str())),
            "spawned task must be one of the ready siblings, got {actual:?}",
        );

        let events = drain_events(&mut rx);
        assert!(
            has_task_advanced(&events, plan),
            "task_advanced must fire for the one spawned task: {events:?}",
        );
        // `to_tasks` payload is a 1-element array — never multiple
        // siblings at once. Match exactly one of the singleton-array
        // shapes; reject any 2+ element variant.
        let task_advanced = events
            .iter()
            .find(|e| e.contains("\"type\":\"task_advanced\""))
            .expect("task_advanced event present");
        let one_of_singletons = ["[\"b\"]", "[\"c\"]", "[\"d\"]"]
            .iter()
            .any(|s| task_advanced.contains(s));
        assert!(
            one_of_singletons,
            "to_tasks must be a 1-element array, event was: {task_advanced}",
        );
    }

    // ── Task 3.5.5 — sequential gate under fan-out + ignored toggle ──
    //
    // Three regression tests pinning that the 3.5.1 break in
    // `spawn_ready_tasks` is the load-bearing safety today:
    //   1. The phase-boundary path also spawns one-per-tick when
    //      multiple dep-free tasks land in the next phase.
    //   2. Forcing `plan_auto_mode.parallel=1` via direct SQL
    //      (bypassing the API gate) does not relax the loop —
    //      the runtime ignores the column for now, and that is
    //      exactly what keeps a single working tree safe from
    //      racing agents until worktree isolation ships.
    //   3. A linear chain crossing a phase boundary calls
    //      `claim_task` exactly once per advance — proxy: agents
    //      row delta of 1 per `try_auto_advance`. Pins that the
    //      intra-phase and cross-phase paths share the gate.
    //
    // The PUT-side rejection test (412 + audit row) lives with
    // 3.5.3 in `tests/plan_config.rs` since it's HTTP-shaped:
    //   - put_parallel_true_returns_412_when_worktrees_not_shipped
    //   - put_parallel_true_audits_refused_attempt
    //
    // That HTTP audit row IS the post-mortem trail the brief
    // refers to. The SQL-bypass scenario in #2 below
    // intentionally produces no audit row of its own (no API
    // call ran); its job is to prove the sequential break holds
    // even when the audit trail is missing.

    /// Task 3.5.5 #1 — sequential gate also holds on the
    /// phase-boundary path. With phase 1 fully done and phase 2
    /// holding 5 dep-free tasks, the first
    /// `try_auto_advance` tick spawns only `2.1` (YAML
    /// declaration order); the other four stay `pending`. Each
    /// subsequent completion advances exactly one more task —
    /// five sequential spawns, never five in parallel.
    #[tokio::test]
    async fn phase_boundary_with_5_ready_tasks_spawns_only_one() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "phase-boundary-fanout";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20     - number: \"2.3\"\n\
             \x20       title: Two-Three\n\
             \x20     - number: \"2.4\"\n\
             \x20       title: Two-Four\n\
             \x20     - number: \"2.5\"\n\
             \x20       title: Two-Five\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        // Phase 1 fully done: only 1.1 in it, mark it
        // completed externally (simulating the user/agent that
        // closed it out).
        set_task_status(&db, plan, "1.1", "completed");

        // Tick 1 — phase boundary. Five dep-free tasks become
        // ready in phase 2; only 2.1 (YAML-first) spawns.
        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "1.1".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        let actual = agent_task_ids(&db, plan);
        let expected: std::collections::HashSet<String> = ["2.1".to_string()].into_iter().collect();
        assert_eq!(
            actual, expected,
            "phase boundary fan-out: only 2.1 should spawn on tick 1, got {actual:?}",
        );

        // 2.2..2.5 must still be pending — no `task_status`
        // row at all is equivalent to pending in this schema.
        for tid in ["2.2", "2.3", "2.4", "2.5"] {
            let conn = db.lock().unwrap();
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
                    params![plan, tid],
                    |row| row.get(0),
                )
                .ok();
            assert!(
                status.is_none() || status.as_deref() == Some("pending"),
                "{tid} should still be pending after tick 1, got {status:?}",
            );
        }

        let events = drain_events(&mut rx);
        assert!(
            has_phase_advanced(&events, plan),
            "phase_advanced expected on tick 1: {events:?}",
        );
        assert!(
            !has_task_advanced(&events, plan),
            "task_advanced must NOT fire on the phase-boundary tick: {events:?}",
        );

        // Drive the remaining four spawns. Each is intra-phase
        // (we're already in phase 2 after tick 1), so every
        // tick must emit `task_advanced`, never
        // `phase_advanced`.
        let chain = [
            ("2.1", "2.2"),
            ("2.2", "2.3"),
            ("2.3", "2.4"),
            ("2.4", "2.5"),
        ];
        let mut prior_total = 1usize;
        for (from, to) in chain {
            set_task_status(&db, plan, from, "completed");
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
            )
            .await;
            let total = agent_task_ids(&db, plan).len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "exactly one new spawn at {from}->{to} (delta {delta})",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            assert!(
                has_task_advanced(&events, plan),
                "intra-phase {from}->{to}: task_advanced expected, events: {events:?}",
            );
            assert!(
                !has_phase_advanced(&events, plan),
                "intra-phase {from}->{to}: phase_advanced must NOT fire, events: {events:?}",
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.contains(&format!("\"to_tasks\":[\"{to}\"]"))),
                "task_advanced payload must list {to:?}: {events:?}",
            );
        }

        // End state: all 5 phase-2 tasks have been spawned —
        // but across 5 sequential ticks, never 5 in parallel.
        assert_eq!(
            agent_task_ids(&db, plan).len(),
            5,
            "all 5 phase-2 tasks must be spawned across 5 ticks",
        );
    }

    /// Task 3.5.5 #2 — `plan_auto_mode.parallel=1` set
    /// directly via SQL (bypassing the API PUT-time gate from
    /// 3.5.3) does NOT relax the sequential break in
    /// `spawn_ready_tasks`. The 3.5.1 break is unconditional —
    /// the loop does not consult the column at all today, so a
    /// hand-edited DB or a stuck row from a future migration
    /// glitch cannot accidentally fan out parallel agents onto
    /// a single working tree.
    ///
    /// The HTTP-shaped audit trail
    /// (`config.parallel_refused`) lives in
    /// `tests/plan_config.rs`. This test deliberately bypasses
    /// the PUT path so no audit row is written here — the
    /// behavioural property under test is "gate held without
    /// audit too".
    #[tokio::test]
    async fn parallel_true_db_with_worktrees_off_still_sequential() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "parallel-bypass";
        // Three siblings (b, c, d) all depend on a — completing
        // a makes all three ready at the same tick (fan-out
        // shape).
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"a\"\n\
             \x20       title: A\n\
             \x20     - number: \"b\"\n\
             \x20       title: B\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"c\"\n\
             \x20       title: C\n\
             \x20       dependencies: [\"a\"]\n\
             \x20     - number: \"d\"\n\
             \x20       title: D\n\
             \x20       dependencies: [\"a\"]\n",
        );

        // Force `enabled=1, parallel=1` directly. The API PUT
        // path would refuse `parallel=true` with 412
        // (worktrees_required) and audit
        // `config.parallel_refused`; this test simulates a
        // post-rollback / hand-edit scenario where the column
        // is set but the runtime gate must still hold.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled, parallel, paused_reason) \
                 VALUES (?1, 1, 1, NULL) \
                 ON CONFLICT(plan_name) DO UPDATE SET \
                     enabled = excluded.enabled, \
                     parallel = excluded.parallel, \
                     paused_reason = excluded.paused_reason",
                params![plan],
            )
            .unwrap();
        }
        // Sanity: confirm the row really has parallel=1 so a
        // future migration that drops the column can't quietly
        // make this test pass for the wrong reason.
        {
            let conn = db.lock().unwrap();
            let (enabled, parallel): (i64, i64) = conn
                .query_row(
                    "SELECT enabled, parallel FROM plan_auto_mode WHERE plan_name = ?1",
                    params![plan],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(enabled, 1, "auto-mode must be enabled");
            assert_eq!(parallel, 1, "parallel column must be set in DB");
        }

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "a", "completed");

        let _ = drain_events(&mut rx);
        try_auto_advance(
            registry.clone(),
            plans_dir.clone(),
            plan.to_string(),
            "a".to_string(),
            Effort::Medium,
            registry.port,
        )
        .await;

        // Despite `parallel=1` in the DB, the sequential break
        // holds: exactly one of {b, c, d} spawns, never
        // multiple. The runtime gate ignores the column until
        // worktree isolation ships — that's the whole point of
        // 3.5.3's WORKTREES_SHIPPED const + this regression.
        let actual = agent_task_ids(&db, plan);
        assert_eq!(
            actual.len(),
            1,
            "parallel=1 in DB must NOT relax the sequential gate, got {actual:?}",
        );
        assert!(
            actual.iter().all(|t| ["b", "c", "d"].contains(&t.as_str())),
            "spawned task must be one of the ready siblings, got {actual:?}",
        );

        let events = drain_events(&mut rx);
        let task_advanced = events
            .iter()
            .find(|e| e.contains("\"type\":\"task_advanced\""))
            .expect("task_advanced event present");
        let one_of_singletons = ["[\"b\"]", "[\"c\"]", "[\"d\"]"]
            .iter()
            .any(|s| task_advanced.contains(s));
        assert!(
            one_of_singletons,
            "to_tasks must be a 1-element array even with parallel=1, event was: {task_advanced}",
        );

        // Defensive: no `phase_advanced` here — we're still in
        // phase 1 after tick 1 because b/c/d haven't all
        // completed.
        assert!(
            !has_phase_advanced(&events, plan),
            "phase_advanced must not fire when only one task spawned: {events:?}",
        );
    }

    /// Task 3.5.5 #3 — pin that the same sequential gate
    /// covers BOTH intra-phase and cross-phase advances. Drive
    /// a 5-task linear chain that straddles a phase boundary
    /// (phase 1: 1.1→1.2→1.3, phase 2: 2.1→2.2 with
    /// 2.1 depending on 1.3); each `try_auto_advance` call
    /// must result in exactly one new `claim_task` success.
    /// Counter proxy: agents-row delta is 1 per tick (each
    /// successful claim leads to a `start_pty_agent` insert),
    /// so a delta of 1 on every tick is the load-bearing
    /// assertion that 3.5.1's break path was taken on each
    /// path.
    ///
    /// Tick layout:
    ///   - Tick 1 (after 1.1): intra-phase — spawn 1.2 (`task_advanced`)
    ///   - Tick 2 (after 1.2): intra-phase — spawn 1.3 (`task_advanced`)
    ///   - Tick 3 (after 1.3): cross-phase — spawn 2.1 (`phase_advanced`)
    ///   - Tick 4 (after 2.1): intra-phase — spawn 2.2 (`task_advanced`)
    #[tokio::test]
    async fn linear_chain_completes_one_per_completion() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (registry, mut rx) = test_registry(db.clone());

        let plan = "lin-chain-cross-phase";
        write_advance_plan(
            &plans_dir,
            plan,
            "  - number: 1\n\
             \x20   title: P1\n\
             \x20   tasks:\n\
             \x20     - number: \"1.1\"\n\
             \x20       title: One-One\n\
             \x20     - number: \"1.2\"\n\
             \x20       title: One-Two\n\
             \x20       dependencies: [\"1.1\"]\n\
             \x20     - number: \"1.3\"\n\
             \x20       title: One-Three\n\
             \x20       dependencies: [\"1.2\"]\n\
             \x20 - number: 2\n\
             \x20   title: P2\n\
             \x20   tasks:\n\
             \x20     - number: \"2.1\"\n\
             \x20       title: Two-One\n\
             \x20       dependencies: [\"1.3\"]\n\
             \x20     - number: \"2.2\"\n\
             \x20       title: Two-Two\n\
             \x20       dependencies: [\"2.1\"]\n",
        );
        enable_auto_mode(&db, plan);

        let _cleanup = SettingsCleanup {
            db: db.clone(),
            plan_name: plan.to_string(),
        };

        set_task_status(&db, plan, "1.1", "completed");

        // (from, to, is_phase_boundary). The "from"
        // completed-task arg is metadata used only to locate
        // the current phase; the actual spawn decision is
        // driven by the `done_set` snapshot.
        let chain = [
            ("1.1", "1.2", false),
            ("1.2", "1.3", false),
            ("1.3", "2.1", true),
            ("2.1", "2.2", false),
        ];
        let mut prior_total = 0usize;
        for (from, to, is_phase_boundary) in chain {
            let _ = drain_events(&mut rx);
            try_auto_advance(
                registry.clone(),
                plans_dir.clone(),
                plan.to_string(),
                from.to_string(),
                Effort::Medium,
                registry.port,
            )
            .await;

            // Counter for `claim_task`: agents-row delta is 1
            // per successful claim (each claim leads to a
            // `start_pty_agent` row insert before the spawn
            // fails on the test_registry's fake server_exe).
            // A delta of 1 on every tick — both intra-phase
            // and cross-phase — proves both paths ride the
            // 3.5.1 break.
            let total = agent_task_ids(&db, plan).len();
            let delta = total - prior_total;
            assert_eq!(
                delta, 1,
                "claim_task must succeed exactly once at {from}->{to} (delta {delta}, intra+cross share the gate)",
            );
            prior_total = total;

            let events = drain_events(&mut rx);
            if is_phase_boundary {
                assert!(
                    has_phase_advanced(&events, plan),
                    "phase boundary {from}->{to}: phase_advanced expected, events: {events:?}",
                );
                assert!(
                    !has_task_advanced(&events, plan),
                    "phase boundary {from}->{to}: task_advanced must NOT fire, events: {events:?}",
                );
            } else {
                assert!(
                    has_task_advanced(&events, plan),
                    "intra-phase {from}->{to}: task_advanced expected, events: {events:?}",
                );
                assert!(
                    !has_phase_advanced(&events, plan),
                    "intra-phase {from}->{to}: phase_advanced must NOT fire, events: {events:?}",
                );
            }

            // Mark the spawned task completed so the next tick
            // sees it as done in `status_map` (the loop's
            // claim flipped it to `in_progress`; a real agent
            // would close it out as `completed`).
            set_task_status(&db, plan, to, "completed");
        }

        // Five-task chain, 1.1 pre-staged + 4 spawned by the
        // loop = 4 successful claims total. Never more.
        assert_eq!(
            agent_task_ids(&db, plan).len(),
            4,
            "linear chain across phase boundary: 4 claims total",
        );
    }
}
