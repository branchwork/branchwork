pub mod check_agent;
pub mod driver;
pub mod git_ops;
pub mod pty_agent;
pub mod session_protocol;
pub mod supervisor;
pub mod terminal_ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Resolve the canonical default branch for the repo at `cwd`.
/// Tries `origin/HEAD` first, then falls back to local `master` / `main`.
/// Returns `None` if nothing resolves. Local-only — never fetches.
///
/// When git ops move to the runner, this helper's call site moves too —
/// keep it pure git so the runner-side implementation can copy it verbatim.
pub fn git_default_branch(cwd: &std::path::Path) -> Option<String> {
    // Step 1: origin/HEAD via symbolic-ref (set by `git clone` and
    // `git remote set-head --auto`). Exits 128, not 1, when absent —
    // gate on status.success() rather than matching exit codes.
    let out = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(cwd)
        .output();
    if let Ok(o) = out
        && o.status.success()
    {
        let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if let Some(name) = raw.strip_prefix("origin/")
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }

    // Step 2: probe local master, then main. --quiet suppresses the
    // "Needed a single revision" stderr that rev-parse writes on miss.
    // Note: a freshly `git init -b master`d repo with no commits
    // returns failure here — the symbolic HEAD exists but no ref does.
    for name in ["master", "main"] {
        let ok = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(name.to_string());
        }
    }

    None
}

/// List local branches in the repo at `cwd` (no remotes).
/// Sorted alphabetically. Empty `Vec` if `git` fails. Local-
/// only — same SaaS swappability rules as `git_default_branch`.
pub fn git_list_branches(cwd: &std::path::Path) -> Vec<String> {
    let Ok(output) = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut branches: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    branches.sort();
    branches
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
        // Auto-advance spawns use the default driver; check whether it
        // auto-registers MCP so the prompt picks MCP tool vs curl.
        let mcp_available = registry.drivers.injects_mcp(None, port);
        let prompt = build_task_prompt(
            &plan,
            next_phase,
            task,
            false,
            port,
            cross_ctx.as_deref(),
            mcp_available,
        );
        let branch_name = format!("branchwork/{}/{}", plan_name, task.number);

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
                driver: None,
                user_id: None,
                org_id: None,
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

    fn git_init_with_commit(dir: &std::path::Path, initial_branch: &str) {
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        run(&["init", "-b", initial_branch]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["commit", "--allow-empty", "-m", "init"]);
    }

    #[test]
    fn git_default_branch_master_via_local_probe() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        assert_eq!(git_default_branch(dir.path()), Some("master".to_string()));
    }

    #[test]
    fn git_default_branch_main_via_local_probe() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "main");
        assert_eq!(git_default_branch(dir.path()), Some("main".to_string()));
    }

    #[test]
    fn git_default_branch_none_when_no_commits() {
        let dir = TempDir::new().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-b", "master"])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok);
        // No commit yet — `master` is the symbolic HEAD but no ref exists,
        // so rev-parse --verify --quiet fails on both probes.
        assert_eq!(git_default_branch(dir.path()), None);
    }

    #[test]
    fn git_default_branch_uses_origin_head_when_set() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        // Seed a fake remote-tracking ref and point origin/HEAD at a
        // non-trunk branch. No clone or fetch needed.
        let head_sha = git_head_sha(dir.path()).unwrap();
        let refs_dir = dir.path().join(".git/refs/remotes/origin");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(refs_dir.join("trunk"), format!("{head_sha}\n")).unwrap();
        let ok = std::process::Command::new("git")
            .args([
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "failed to set origin/HEAD symref");
        assert_eq!(git_default_branch(dir.path()), Some("trunk".to_string()));
    }

    #[test]
    fn git_list_branches_single_master() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        assert_eq!(git_list_branches(dir.path()), vec!["master".to_string()]);
    }

    #[test]
    fn git_list_branches_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        run(&["branch", "feature/x"]);
        run(&["branch", "bw/1.1"]);
        assert_eq!(
            git_list_branches(dir.path()),
            vec![
                "bw/1.1".to_string(),
                "feature/x".to_string(),
                "master".to_string(),
            ]
        );
    }

    #[test]
    fn git_list_branches_empty_when_not_a_git_repo() {
        let dir = TempDir::new().unwrap();
        assert_eq!(git_list_branches(dir.path()), Vec::<String>::new());
    }
}
