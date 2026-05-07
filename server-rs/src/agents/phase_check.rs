//! Phase-end verify Check agent listener.
//!
//! Subscribes to the in-process `phase_completed` broadcast (fired by
//! [`crate::agents::try_auto_advance`] in Task 2.1) and, when a
//! `phase_verification` command is configured at the phase / plan / repo
//! `branchwork.toml` layer, spawns a Check agent on a fresh `git worktree`
//! at the phase merge SHA to run that command. This is the consumer side
//! of the Phase 2 gate added to `try_auto_advance`: the gate returns
//! early when verification is configured, deferring the next-phase
//! advance to this listener.
//!
//! # Lifecycle
//!
//! 1. `phase_completed{plan_name, phase_number, phase_sha}` arrives.
//! 2. Resolve `phase_verification` via [`crate::ci::resolution`]. `None`
//!    → no-op (Task 2.3 covers the no-op semantics — we just ignore the
//!    event and the auto-advance gate above lets the existing flow run).
//! 3. Create a fresh worktree at `phase_sha` under
//!    `<temp>/branchwork-phase-verify-<uuid>`.
//! 4. Spawn a Check agent rooted at the worktree, prompted to run the
//!    verification command via Bash and report a JSON verdict. The agent
//!    uses broader `Bash(*)` tool perms than [`check_agent`] (which is
//!    git-only) because the verify suite typically invokes `cargo`,
//!    `bash scripts/verify.sh`, etc.
//! 5. On verdict `passed` (or `completed`): broadcast
//!    `phase_verify_passed` and call
//!    [`crate::agents::advance_after_phase_verify`] to fall through to
//!    the next-phase scan-and-spawn that `try_auto_advance` deferred.
//! 6. On any other verdict (or no verdict produced): broadcast
//!    `phase_verify_failed` with the captured failure log, audit-log the
//!    pause, and call [`crate::db::auto_mode_pause`] so the auto-mode
//!    loop halts with a human-actionable reason.
//! 7. Worktree cleanup is unconditional — `git worktree remove --force`
//!    runs on every exit path, success or failure.
//!
//! # No phase_sha → no verification
//!
//! Manual completion paths (HTTP `PUT /tasks/:id/status`, MCP, resume)
//! pass `merged_sha=None`, which serialises as `phase_sha: null`. There
//! is no commit to verify against, so the listener silently ignores
//! these events. The auto-advance gate in `try_auto_advance` mirrors
//! this: it only defers when `merged_sha.is_some()`, so the manual flow
//! still triggers an immediate next-phase advance.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::params;
use tokio::sync::broadcast;

use crate::agents::AgentRegistry;
use crate::audit;
use crate::ci::resolution::resolve_phase_verification;
use crate::config::Effort;
use crate::db;
use crate::plan_parser::{self, ParsedPlan, PlanPhase};
use crate::repo_config;
use crate::state::AppState;
use crate::ws::broadcast_event;

/// Audit-log action constants for phase-verify transitions. Mirrors the
/// `auto_mode::actions` shape so dashboard filtering can group both under
/// the auto-mode umbrella.
pub mod actions {
    /// The verify Check agent reported success for a phase. Diff carries
    /// `{plan_name, phase_number, phase_sha, agent_id}`.
    pub const PHASE_VERIFY_PASSED: &str = "phase_verify.passed";
    /// The verify Check agent reported failure (or never produced a
    /// verdict). Diff carries `{plan_name, phase_number, phase_sha,
    /// agent_id, reason}`.
    pub const PHASE_VERIFY_FAILED: &str = "phase_verify.failed";
}

/// Parsed payload of a `phase_completed` broadcast.
#[derive(Debug, Clone)]
pub struct PhaseCompletedPayload {
    pub plan_name: String,
    pub phase_number: u32,
    /// `None` for manual completion paths (no merge happened). The
    /// listener treats this as "nothing to verify" and ignores the event.
    pub phase_sha: Option<String>,
}

/// Outcome reported by the verify Check agent. Maps to the dispatch
/// branches in [`run_verification`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Agent exited cleanly and produced a `passed` / `completed`
    /// verdict — phase is verified, fall through to next-phase advance.
    Passed,
    /// Agent exited but the verdict was missing, malformed, or non-
    /// passing. `reason` carries a short human-readable summary
    /// (verdict reason if available, else exit code, else
    /// "no_verdict").
    Failed { reason: String },
}

/// Spawn the per-process listener task. Subscribes to the dashboard
/// broadcast channel, filters for `phase_completed` envelopes, and
/// dispatches each one onto its own tokio task so a slow verify run can't
/// block other events.
pub fn spawn_listener(state: AppState) {
    let mut rx = state.broadcast_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if let Some(payload) = parse_phase_completed(&msg) {
                        let state = state.clone();
                        tokio::spawn(async move {
                            handle_phase_completed(&state, payload).await;
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[phase_check] listener lagged, skipped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Pull a `phase_completed` payload out of a broadcast envelope. Returns
/// `None` for any other event type. The cheap string contains check
/// short-circuits before the JSON parse so the hot path stays cheap.
pub fn parse_phase_completed(msg: &str) -> Option<PhaseCompletedPayload> {
    if !msg.contains("\"type\":\"phase_completed\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(msg).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("phase_completed") {
        return None;
    }
    let data = v.get("data")?;
    Some(PhaseCompletedPayload {
        plan_name: data.get("plan_name").and_then(|s| s.as_str())?.to_string(),
        phase_number: data.get("phase_number").and_then(|n| n.as_u64())? as u32,
        phase_sha: data
            .get("phase_sha")
            .and_then(|s| s.as_str())
            .map(String::from),
    })
}

/// Resolve the verify command and project dir for the just-completed
/// phase, then dispatch to [`run_verification`]. No-op when:
/// - `phase_sha` is `None` (manual completion — nothing to verify)
/// - the plan can't be loaded
/// - the phase number doesn't exist in the plan
/// - `resolve_phase_verification` returns `None` (Task 2.3 contract)
/// - the plan has no `project` configured (verification needs a worktree)
pub async fn handle_phase_completed(state: &AppState, payload: PhaseCompletedPayload) {
    let phase_sha = match payload.phase_sha.as_deref() {
        Some(s) => s.to_string(),
        None => return,
    };
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &payload.plan_name) {
        Some(p) => p,
        None => return,
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => return,
    };
    let phase_idx = match plan
        .phases
        .iter()
        .position(|p| p.number == payload.phase_number)
    {
        Some(i) => i,
        None => return,
    };

    let project_dir = match plan.project.as_ref() {
        Some(p) => match dirs::home_dir() {
            Some(h) => h.join(p),
            None => return,
        },
        None => {
            eprintln!(
                "[phase_check] plan {} has no `project` configured — cannot run verification",
                payload.plan_name
            );
            return;
        }
    };

    let repo_cfg = repo_config::load_for_project_dir(&project_dir);
    let verify_cmd =
        match resolve_phase_verification(&plan, &plan.phases[phase_idx], repo_cfg.as_ref()) {
            Some(c) => c,
            None => return,
        };

    run_verification(
        state,
        &plan,
        phase_idx,
        &project_dir,
        &phase_sha,
        &verify_cmd,
    )
    .await;
}

/// End-to-end verify run: create a fresh worktree at `phase_sha`, spawn
/// the Check agent, wait for it to exit, parse the verdict, clean up the
/// worktree, then either advance to the next phase (on pass) or pause
/// auto-mode + broadcast the failure log (on fail).
///
/// `phase_idx` is the index into `plan.phases` rather than the phase
/// number — keeps the borrow-checker happy when we want both the
/// `&PlanPhase` and an owned `u32` later (the index lookup is O(1)
/// against the small phases vec).
pub async fn run_verification(
    state: &AppState,
    plan: &ParsedPlan,
    phase_idx: usize,
    project_dir: &Path,
    phase_sha: &str,
    verify_cmd: &str,
) {
    let phase = &plan.phases[phase_idx];
    let plan_name = plan.name.clone();
    let phase_number = phase.number;

    let worktree_dir = match create_worktree(project_dir, phase_sha) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("worktree_create_failed: {e}");
            on_verify_failed(state, &plan_name, phase_number, phase_sha, None, &reason).await;
            return;
        }
    };

    let prompt = build_verify_prompt(&plan_name, phase, phase_sha, verify_cmd);
    let effort = *state.effort.lock().unwrap();

    let outcome = spawn_and_await_check_agent(
        &state.registry,
        prompt,
        &worktree_dir,
        &plan_name,
        phase_number,
        effort,
    )
    .await;

    cleanup_worktree(project_dir, &worktree_dir);

    match outcome {
        SpawnResult {
            agent_id,
            outcome: VerifyOutcome::Passed,
        } => {
            on_verify_passed(state, &plan_name, phase_number, phase_sha, &agent_id).await;
        }
        SpawnResult {
            agent_id,
            outcome: VerifyOutcome::Failed { reason },
        } => {
            on_verify_failed(
                state,
                &plan_name,
                phase_number,
                phase_sha,
                Some(&agent_id),
                &reason,
            )
            .await;
        }
    }
}

/// Result of a single Check agent run: the agent_id (so callers can cite
/// it in audit + broadcast payloads, even on failure) plus the
/// pass/fail outcome.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub agent_id: String,
    pub outcome: VerifyOutcome,
}

/// Build the verify prompt that gets sent to the Check agent. The prompt
/// is intentionally terse — the agent is a glorified subprocess wrapper
/// here; we want it to run the command, capture exit code, and report a
/// JSON verdict. Mirrors the verdict shape parsed by
/// [`crate::agents::check_agent`]: `{"status": "...", "reason": "..."}`,
/// but we accept `passed`/`completed` as positive and treat anything else
/// as failure (the existing check_agent only accepts `completed`/`in_progress`/`pending`,
/// but those are task-status semantics — for phase verify we want a
/// broader yes/no answer).
fn build_verify_prompt(
    plan_name: &str,
    phase: &PlanPhase,
    phase_sha: &str,
    verify_cmd: &str,
) -> String {
    format!(
        "You are a phase-end verification agent for the Branchwork plan `{plan_name}` at \
         phase {phase_number} (\"{phase_title}\"). The phase merge commit is `{phase_sha}` \
         and you are running in a fresh git worktree checked out at that SHA.\n\
         \n\
         Your job is to run the verification command exactly once via the Bash tool, capture \
         its exit code and the last few lines of its output, and report a JSON verdict.\n\
         \n\
         Verification command:\n\
         ```\n\
         {verify_cmd}\n\
         ```\n\
         \n\
         When the command's exit code is 0 (success), respond with EXACTLY this JSON on the \
         final line of your reply (no prose after it):\n\
         ```\n\
         {{\"status\": \"passed\", \"reason\": \"verification command exited 0\"}}\n\
         ```\n\
         \n\
         When the command's exit code is non-zero (failure), respond with EXACTLY this JSON on \
         the final line, with `reason` set to a short summary of the failure (the failing \
         step, last error line, etc.):\n\
         ```\n\
         {{\"status\": \"failed\", \"reason\": \"<one-line failure summary>\"}}\n\
         ```\n\
         \n\
         Do NOT modify any files in the worktree. Do NOT commit anything. Do NOT push. Just \
         run the command, observe, and report.\n",
        plan_name = plan_name,
        phase_number = phase.number,
        phase_title = phase.title.replace('"', "\\\""),
        phase_sha = phase_sha,
        verify_cmd = verify_cmd,
    )
}

/// Spawn the Check agent (mirroring [`crate::agents::check_agent::start_check_agent`]
/// but with broader `Bash(*)` perms so the verify suite can invoke
/// arbitrary tooling) and synchronously await its exit. Returns the
/// agent_id + parsed verdict.
///
/// The agent runs in stream-json mode just like the existing per-task
/// Check agent — that gives the dashboard live output, an agent_output
/// row trail, and the same agent_started / agent_stopped broadcast
/// envelope shape so existing UI code "just works".
async fn spawn_and_await_check_agent(
    registry: &AgentRegistry,
    prompt: String,
    cwd: &Path,
    plan_name: &str,
    phase_number: u32,
    effort: Effort,
) -> SpawnResult {
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let base_commit = crate::agents::git_head_sha(cwd);

    {
        let conn = registry.db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt, base_commit)
             VALUES (?1, ?2, ?3, 'starting', 'stream-json', ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                cwd.to_str().unwrap_or(""),
                plan_name,
                // task_id encodes the phase number with a `phase-` prefix
                // so the agents row remains queryable per-phase. Not a
                // real task number — `task_status` writes are gated on
                // numeric task ids elsewhere, so this won't accidentally
                // mutate the task_status table.
                Some(format!("phase-{phase_number}-verify")),
                prompt,
                base_commit,
            ],
        )
        .ok();
    }

    let mut child = match Command::new("claude")
        .args([
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--session-id",
            &session_id,
            "--add-dir",
            &cwd.to_string_lossy(),
            "--permission-mode",
            "acceptEdits",
            "--allowedTools",
            "Read,Glob,Grep,Bash",
            "--effort",
            &effort.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[phase_check {}] failed to spawn claude: {e}", &id[..8]);
            let conn = registry.db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                params![id],
            )
            .ok();
            broadcast_event(
                &registry.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": id, "status": "failed", "error": e.to_string()}),
            );
            return SpawnResult {
                agent_id: id,
                outcome: VerifyOutcome::Failed {
                    reason: format!("spawn_failed: {e}"),
                },
            };
        }
    };

    let pid = child.id();

    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        let msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}]
            }
        });
        writeln!(stdin, "{msg}").ok();
        drop(child.stdin.take());
    }

    {
        let conn = registry.db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET pid = ?1, status = 'running' WHERE id = ?2",
            params![pid as i64, id],
        )
        .ok();
    }

    broadcast_event(
        &registry.broadcast_tx,
        "agent_started",
        serde_json::json!({
            "id": id,
            "sessionId": session_id,
            "planName": plan_name,
            "taskId": format!("phase-{phase_number}-verify"),
            "pid": pid,
            "mode": "stream-json"
        }),
    );

    // Hand off the child to a blocking thread so we can synchronously
    // drain its stdout to agent_output AND wait for exit. The async
    // wrapper just awaits the JoinHandle.
    let db = registry.db.clone();
    let tx = registry.broadcast_tx.clone();
    let id_clone = id.clone();

    let handle = tokio::task::spawn_blocking(move || -> (i32, Option<String>) {
        let stdout = child.stdout.take().unwrap();
        let mut last_text_lines: Vec<String> = Vec::new();
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let msg_type = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("type")?.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "raw".to_string());

            {
                let conn = db.lock().unwrap();
                conn.execute(
                    "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, ?2, ?3)",
                    params![id_clone, msg_type, line],
                )
                .ok();
            }
            broadcast_event(
                &tx,
                "agent_output",
                serde_json::json!({
                    "agent_id": id_clone,
                    "message_type": msg_type,
                }),
            );
            last_text_lines.push(line);
        }

        let exit = child.wait().ok();
        let exit_code = exit.and_then(|s| s.code()).unwrap_or(-1);

        // Pull the verdict text out of agent_output rows. Same parsing
        // strategy as `check_agent` but accepts `passed` / `completed`
        // for success and `failed` for explicit failure.
        let verdict_reason: Option<String> = {
            let conn = db.lock().unwrap();
            extract_verdict_text(&conn, &id_clone)
        };

        let agent_status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = ?1, finished_at = datetime('now') WHERE id = ?2",
                params![agent_status, id_clone],
            )
            .ok();
        }
        broadcast_event(
            &tx,
            "agent_stopped",
            serde_json::json!({"id": id_clone, "status": agent_status, "exit_code": exit_code}),
        );

        (exit_code, verdict_reason)
    });

    let (exit_code, verdict_text) = match handle.await {
        Ok(t) => t,
        Err(e) => {
            return SpawnResult {
                agent_id: id,
                outcome: VerifyOutcome::Failed {
                    reason: format!("agent_join_failed: {e}"),
                },
            };
        }
    };

    let outcome = classify_outcome(exit_code, verdict_text.as_deref());
    SpawnResult {
        agent_id: id,
        outcome,
    }
}

/// Walk `agent_output` for `agent_id` newest-first, looking for a
/// `{"status": "..."}` JSON blob in any text content. Returns the raw
/// blob text on first hit (so callers can parse it themselves), `None`
/// when the agent never produced one. Mirrors the search loop in
/// [`crate::agents::check_agent`].
fn extract_verdict_text(conn: &rusqlite::Connection, agent_id: &str) -> Option<String> {
    let mut stmt = conn
        .prepare("SELECT content FROM agent_output WHERE agent_id = ? ORDER BY id DESC")
        .ok()?;
    let rows: Vec<String> = stmt
        .query_map(params![agent_id], |row| row.get::<_, String>(0))
        .ok()?
        .flatten()
        .collect();

    for row in rows {
        if let Ok(outer) = serde_json::from_str::<serde_json::Value>(&row) {
            let mut text = String::new();
            if let Some(result) = outer.get("result").and_then(|v| v.as_str()) {
                text = result.to_string();
            } else if let Some(content) = outer
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(t) = block.get("text").and_then(|t| t.as_str())
                    {
                        text.push_str(t);
                    }
                }
            }

            if let Some(start) = text.find(r#""status""#) {
                let json_start = text[..start].rfind('{').unwrap_or(start);
                let remainder = &text[json_start..];
                let verdict_json = (1..=remainder.len())
                    .filter(|&i| remainder.as_bytes().get(i - 1) == Some(&b'}'))
                    .find_map(|i| serde_json::from_str::<serde_json::Value>(&remainder[..i]).ok());
                if let Some(v) = verdict_json {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Map the agent's process exit code + parsed verdict text to the
/// pass/fail outcome we report up. Pass = verdict status is one of
/// `passed` / `completed` (we accept both since the verify prompt asks
/// for `passed` but the existing check_agent vocabulary uses
/// `completed`); anything else is a failure with the most informative
/// reason we have.
fn classify_outcome(exit_code: i32, verdict_text: Option<&str>) -> VerifyOutcome {
    if let Some(text) = verdict_text
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(text)
    {
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if status == "passed" || status == "completed" {
            return VerifyOutcome::Passed;
        }
        let reason = v
            .get("reason")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("verdict_status: {status}"));
        return VerifyOutcome::Failed { reason };
    }
    if exit_code == 0 {
        VerifyOutcome::Failed {
            reason: "no_verdict".to_string(),
        }
    } else {
        VerifyOutcome::Failed {
            reason: format!("agent_exit_{exit_code}_no_verdict"),
        }
    }
}

/// Pass branch: broadcast `phase_verify_passed`, audit-log it, then
/// trigger the next-phase advance that `try_auto_advance` deferred when
/// it saw the verification gate.
async fn on_verify_passed(
    state: &AppState,
    plan_name: &str,
    phase_number: u32,
    phase_sha: &str,
    agent_id: &str,
) {
    let payload = serde_json::json!({
        "plan_name": plan_name,
        "phase_number": phase_number,
        "phase_sha": phase_sha,
        "agent_id": agent_id,
    });
    broadcast_event(&state.broadcast_tx, "phase_verify_passed", payload.clone());

    if let Some(org_id) = lookup_org_for_plan(state, plan_name) {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            None,
            Some("branchwork-phase-verify"),
            actions::PHASE_VERIFY_PASSED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }

    let registry = state.registry.clone();
    let plans_dir = state.plans_dir.clone();
    let plan_name_owned = plan_name.to_string();
    let effort = *state.effort.lock().unwrap();
    let port = state.config_port();
    crate::agents::advance_after_phase_verify(
        registry,
        plans_dir,
        plan_name_owned,
        phase_number,
        effort,
        port,
    )
    .await;
}

/// Fail branch: broadcast `phase_verify_failed` carrying the reason +
/// agent_id (so the dashboard can deep-link to the agent's output),
/// audit-log the pause, and pause auto-mode for the plan with reason
/// `phase_verify_failed:<short>`. Auto-advance for the next phase is
/// blocked until the user clicks Resume.
async fn on_verify_failed(
    state: &AppState,
    plan_name: &str,
    phase_number: u32,
    phase_sha: &str,
    agent_id: Option<&str>,
    reason: &str,
) {
    let payload = serde_json::json!({
        "plan_name": plan_name,
        "phase_number": phase_number,
        "phase_sha": phase_sha,
        "agent_id": agent_id,
        "reason": reason,
    });
    broadcast_event(&state.broadcast_tx, "phase_verify_failed", payload.clone());

    let pause_reason = format!("phase_verify_failed: {reason}");
    db::auto_mode_pause(&state.db, plan_name, &pause_reason);

    if let Some(org_id) = lookup_org_for_plan(state, plan_name) {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            None,
            Some("branchwork-phase-verify"),
            actions::PHASE_VERIFY_FAILED,
            audit::resources::PLAN,
            Some(plan_name),
            Some(&payload.to_string()),
        );
    }
}

/// Best-effort lookup of an `org_id` to attribute audit rows to. Returns
/// the org of the most recent agent for `plan_name` if any exists,
/// otherwise `None` — audit-log writes are skipped in that case (they're
/// org-scoped). Tests that don't seed any agents row will land on the
/// `None` branch and skip the audit write, which is fine.
fn lookup_org_for_plan(state: &AppState, plan_name: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT org_id FROM agents WHERE plan_name = ?1 ORDER BY started_at DESC LIMIT 1",
        params![plan_name],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// Create a fresh `git worktree` at `phase_sha` under a per-run temp dir.
/// `git worktree add` requires the path to NOT exist, so we use a UUID
/// suffix + the system temp dir for collision avoidance + auto-cleanup
/// on host reboot. Returns the worktree path on success.
fn create_worktree(project_dir: &Path, phase_sha: &str) -> Result<PathBuf, String> {
    let suffix = uuid::Uuid::new_v4().to_string();
    let path = std::env::temp_dir().join(format!("branchwork-phase-verify-{suffix}"));
    let out = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            path.to_str()
                .ok_or_else(|| "non-utf8 worktree path".to_string())?,
            phase_sha,
        ])
        .current_dir(project_dir)
        .output()
        .map_err(|e| format!("git worktree add spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(path)
}

/// Best-effort worktree cleanup. `git worktree remove --force` is the
/// canonical path; if it fails (worktree row corrupted, git absent, etc.)
/// fall back to a plain `remove_dir_all` so we don't leak the directory
/// on disk. Both failures are logged — the calling flow continues
/// regardless because the verify outcome already drove dispatch.
fn cleanup_worktree(project_dir: &Path, worktree_path: &Path) {
    let path_str = match worktree_path.to_str() {
        Some(s) => s,
        None => {
            eprintln!(
                "[phase_check] non-utf8 worktree path during cleanup: {}",
                worktree_path.display()
            );
            return;
        }
    };
    let out = Command::new("git")
        .args(["worktree", "remove", "--force", path_str])
        .current_dir(project_dir)
        .output();
    match out {
        Ok(o) if o.status.success() => return,
        Ok(o) => {
            eprintln!(
                "[phase_check] git worktree remove failed ({}): {}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[phase_check] git worktree remove spawn failed: {e}");
        }
    }
    if let Err(e) = std::fs::remove_dir_all(worktree_path) {
        eprintln!(
            "[phase_check] fallback remove_dir_all failed for {}: {e}",
            worktree_path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_phase_completed ───────────────────────────────────────────

    fn make_envelope(event: &str, data: serde_json::Value) -> String {
        serde_json::json!({
            "type": event,
            "data": data,
            "timestamp": "2026-05-07T00:00:00Z",
        })
        .to_string()
    }

    #[test]
    fn parse_extracts_well_formed_phase_completed_payload() {
        let msg = make_envelope(
            "phase_completed",
            serde_json::json!({
                "plan_name": "p1",
                "phase_number": 2,
                "phase_sha": "deadbeef",
            }),
        );
        let payload = parse_phase_completed(&msg).expect("should parse");
        assert_eq!(payload.plan_name, "p1");
        assert_eq!(payload.phase_number, 2);
        assert_eq!(payload.phase_sha.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn parse_handles_null_phase_sha() {
        let msg = make_envelope(
            "phase_completed",
            serde_json::json!({
                "plan_name": "p1",
                "phase_number": 0,
                "phase_sha": null,
            }),
        );
        let payload = parse_phase_completed(&msg).expect("should parse");
        assert!(payload.phase_sha.is_none());
    }

    #[test]
    fn parse_returns_none_for_unrelated_event_types() {
        let msg = make_envelope(
            "task_advanced",
            serde_json::json!({"plan": "p1", "to_tasks": ["1.1"]}),
        );
        assert!(parse_phase_completed(&msg).is_none());
    }

    #[test]
    fn parse_returns_none_for_malformed_json() {
        // Contains the substring but isn't valid JSON — guard against
        // false positives from the cheap `contains` short-circuit.
        let msg = r#"{"type":"phase_completed","data":not-json}"#;
        assert!(parse_phase_completed(msg).is_none());
    }

    #[test]
    fn parse_returns_none_when_required_fields_missing() {
        // No phase_number → payload is unusable.
        let msg = make_envelope(
            "phase_completed",
            serde_json::json!({"plan_name": "p1", "phase_sha": "x"}),
        );
        assert!(parse_phase_completed(&msg).is_none());

        // No plan_name → payload is unusable.
        let msg2 = make_envelope(
            "phase_completed",
            serde_json::json!({"phase_number": 1, "phase_sha": "x"}),
        );
        assert!(parse_phase_completed(&msg2).is_none());
    }

    // ── classify_outcome ────────────────────────────────────────────────

    #[test]
    fn classify_passed_when_verdict_status_is_passed() {
        let v = r#"{"status":"passed","reason":"ok"}"#;
        assert_eq!(classify_outcome(0, Some(v)), VerifyOutcome::Passed);
        // Even if exit code claims failure, an explicit `passed` verdict
        // wins — the agent saw the command succeed and reported it
        // (defensive: the agent process itself can crash post-verdict).
        assert_eq!(classify_outcome(1, Some(v)), VerifyOutcome::Passed);
    }

    #[test]
    fn classify_passed_when_verdict_status_is_completed() {
        // We accept `completed` for compatibility with the existing
        // check_agent verdict vocabulary — agents that re-use the
        // task-status verdict shape should still be treated as passing.
        let v = r#"{"status":"completed","reason":"all green"}"#;
        assert_eq!(classify_outcome(0, Some(v)), VerifyOutcome::Passed);
    }

    #[test]
    fn classify_failed_with_verdict_reason_when_status_is_failed() {
        let v = r#"{"status":"failed","reason":"clippy: unused import"}"#;
        match classify_outcome(0, Some(v)) {
            VerifyOutcome::Failed { reason } => {
                assert_eq!(reason, "clippy: unused import");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_failed_when_no_verdict_text() {
        // Exit 0 + no verdict = "no_verdict" (agent finished cleanly
        // but never produced the JSON we asked for — treat as failure
        // so we don't silently advance on missing data).
        match classify_outcome(0, None) {
            VerifyOutcome::Failed { reason } => assert_eq!(reason, "no_verdict"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_failed_carries_exit_code_when_no_verdict_and_nonzero_exit() {
        match classify_outcome(7, None) {
            VerifyOutcome::Failed { reason } => assert_eq!(reason, "agent_exit_7_no_verdict"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_failed_with_unknown_status_synthesises_reason() {
        // Empty-reason verdict with unknown status falls back to the
        // synthesised "verdict_status: <s>" so the dashboard still shows
        // something actionable.
        let v = r#"{"status":"weird","reason":""}"#;
        match classify_outcome(0, Some(v)) {
            VerifyOutcome::Failed { reason } => assert_eq!(reason, "verdict_status: weird"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ── build_verify_prompt ─────────────────────────────────────────────

    #[test]
    fn prompt_contains_plan_phase_sha_and_command() {
        let phase = PlanPhase {
            number: 3,
            title: "P3".into(),
            description: String::new(),
            tasks: vec![],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let p = build_verify_prompt("my-plan", &phase, "abc123", "bash scripts/verify.sh");
        assert!(p.contains("my-plan"));
        assert!(p.contains("phase 3"));
        assert!(p.contains("P3"));
        assert!(p.contains("abc123"));
        assert!(p.contains("bash scripts/verify.sh"));
        // Verdict instruction must be present so the agent knows the
        // expected JSON shape — `classify_outcome` depends on it.
        assert!(p.contains("\"status\": \"passed\""));
        assert!(p.contains("\"status\": \"failed\""));
    }

    #[test]
    fn prompt_escapes_double_quotes_in_phase_title() {
        // Phase title with embedded quotes must not break the prompt's
        // surrounding quoted string (defence against YAML titles that
        // legitimately contain quote chars).
        let phase = PlanPhase {
            number: 1,
            title: r#"with "quotes""#.into(),
            description: String::new(),
            tasks: vec![],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let p = build_verify_prompt("p", &phase, "sha", "true");
        assert!(p.contains(r#"with \"quotes\""#));
    }

    // ── create_worktree / cleanup_worktree ──────────────────────────────

    /// End-to-end worktree create + cleanup against a real local git
    /// repo. Skips on hosts without git installed (CI without git is
    /// vanishingly unlikely; the test prefers visibility over silently
    /// passing). Pinning the worktree create + remove cycle catches
    /// regressions in the args we pass to `git worktree`.
    #[test]
    fn create_and_cleanup_worktree_round_trip() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("[phase_check::tests] git not installed — skipping");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path();
        // Init repo + first commit so we have a SHA to check out.
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.email", "t@t.local"][..],
            &["config", "user.name", "T"][..],
        ] {
            let out = Command::new("git")
                .args(args)
                .current_dir(project_dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        }
        std::fs::write(project_dir.join("README.md"), "hi\n").unwrap();
        for args in [&["add", "."][..], &["commit", "-m", "init"][..]] {
            let out = Command::new("git")
                .args(args)
                .current_dir(project_dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        }
        let sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(project_dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let wt = create_worktree(project_dir, &sha).expect("worktree create");
        assert!(wt.exists(), "worktree dir should exist after create");
        assert!(
            wt.join("README.md").exists(),
            "checked-out file should be present in the worktree",
        );

        cleanup_worktree(project_dir, &wt);
        assert!(!wt.exists(), "worktree dir should be gone after cleanup");
    }

    #[test]
    fn create_worktree_returns_err_on_invalid_sha() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("[phase_check::tests] git not installed — skipping");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path();
        for args in [
            &["init", "--initial-branch=main"][..],
            &["config", "user.email", "t@t.local"][..],
            &["config", "user.name", "T"][..],
        ] {
            Command::new("git")
                .args(args)
                .current_dir(project_dir)
                .output()
                .unwrap();
        }
        std::fs::write(project_dir.join("a"), "a").unwrap();
        for args in [&["add", "."][..], &["commit", "-m", "i"][..]] {
            Command::new("git")
                .args(args)
                .current_dir(project_dir)
                .output()
                .unwrap();
        }
        let res = create_worktree(project_dir, "0000000000000000000000000000000000000000");
        assert!(res.is_err(), "non-existent SHA must produce Err");
    }
}
