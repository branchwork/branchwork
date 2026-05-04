use axum::{Json, extract::State, response::IntoResponse};
use rusqlite::params;
use serde::Deserialize;

use crate::state::AppState;
use crate::ws::broadcast_event;

#[derive(Deserialize)]
pub struct HookEvent {
    session_id: Option<String>,
    hook_event_name: Option<String>,
    hook_type: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
}

/// POST /hooks
pub async fn receive_hook(
    State(state): State<AppState>,
    Json(event): Json<HookEvent>,
) -> impl IntoResponse {
    let session_id = event.session_id.as_deref().unwrap_or("unknown");
    let hook_type = event
        .hook_event_name
        .as_deref()
        .or(event.hook_type.as_deref())
        .unwrap_or("unknown");
    let tool_name = event.tool_name.as_deref();
    let tool_input = event.tool_input.as_ref().map(|v| v.to_string());

    // Scope the DB lock so the guard cannot survive into the await below
    // (MutexGuard<Connection> is !Send, which would make the handler future
    // !Send and break axum's Handler bound).
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "INSERT INTO hook_events (session_id, hook_type, tool_name, tool_input) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, hook_type, tool_name, tool_input],
        )
        .unwrap();

        // Update agent last_tool if we track this session
        if let Some(tn) = tool_name {
            db.execute(
                "UPDATE agents SET last_tool = ?1, last_activity_at = datetime('now') WHERE session_id = ?2 AND status IN ('starting', 'running')",
                params![tn, session_id],
            )
            .ok();
        }
    }

    // Auto-mode: a Stop hook on a live agent whose plan opted into auto-mode
    // is the trigger for the unattended graceful_exit + advance loop. The
    // path is independent of the regular telemetry below — the live feed
    // still receives the hook_event broadcast unchanged.
    if hook_type == "Stop" {
        handle_stop_hook(&state, session_id).await;
    }

    broadcast_event(
        &state.broadcast_tx,
        "hook_event",
        serde_json::json!({
            "session_id": session_id,
            "hook_type": hook_type,
            "tool_name": tool_name,
            "tool_input": event.tool_input,
        }),
    );

    Json(serde_json::json!({ "ok": true }))
}

/// Handle the auto-mode side-effects of a `Stop` hook.
///
/// Returns silently in any of these cases (still no-op telemetry-wise):
///   - the session_id doesn't match a known agent (stale hook),
///   - the agent isn't `running` (already finishing — debounce against
///     the second Stop the CLI fires after we send `/exit`; see Task 2.4),
///   - the agent has no plan attached, or
///   - the plan's auto-mode is off (or self-paused).
///
/// If auto-mode is on and the project tree is dirty, pauses the plan with
/// reason `agent_left_uncommitted_work` (broadcast + audit identical to
/// other auto-mode pause paths).
///
/// Otherwise spawns `registry.graceful_exit` so the HTTP response doesn't
/// await the PTY write, then audits `AGENT_AUTO_FINISH` and broadcasts
/// `auto_finish_triggered` with `{trigger: "stop_hook"}`.
async fn handle_stop_hook(state: &AppState, session_id: &str) {
    // (id, status, plan_name, task_id, org_id) for the agent matching the
    // hook's session_id; None means the session is stale.
    type AgentLookup = (String, String, Option<String>, Option<String>, String);

    // Single DB lookup pulls every field the decision tree needs. Keeping
    // it in one query avoids lock thrash inside the hot hook path.
    let lookup: Option<AgentLookup> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT id, status, plan_name, task_id, org_id \
             FROM agents WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .ok()
    };

    let Some((agent_id, status, plan_name, task_id, org_id)) = lookup else {
        eprintln!("[hooks] stale Stop ignored: unknown session {session_id}");
        return;
    };
    if status != "running" {
        eprintln!("[hooks] stale Stop ignored: agent {agent_id} not running (status={status})");
        return;
    }
    let Some(plan_name) = plan_name else {
        eprintln!("[hooks] auto-mode off, no auto-finish: agent {agent_id} has no plan");
        return;
    };
    if !crate::db::auto_mode_enabled(&state.db, &plan_name) {
        eprintln!("[hooks] auto-mode off, no auto-finish: plan={plan_name}");
        return;
    }

    // Tree-clean check is the same one task-completion uses. Dirty pauses
    // the loop with a reason the dashboard can render; Clean / Unknown
    // proceed (Unknown is permissive on purpose — see the helper docs).
    //
    // The dirty-tree branch self-debounces against repeat Stops because
    // `auto_mode_pause` flips `paused_reason`, which makes the next
    // `auto_mode_enabled` lookup return false above. Only the
    // clean / unknown branch needs the dedupe set: it leaves the DB
    // state untouched (auto-mode stays enabled, agent status stays
    // running until `on_agent_exit` runs after the PTY closes), so the
    // second Stop would otherwise re-fire `graceful_exit` + log a
    // duplicate audit row + broadcast a redundant event.
    match crate::agents::check_tree_clean_for_completion(&state.db, &state.plans_dir, &plan_name) {
        crate::agents::TreeState::Dirty { files } => {
            let trimmed: Vec<&String> = files.iter().take(5).collect();
            crate::db::auto_mode_pause(&state.db, &plan_name, "agent_left_uncommitted_work");
            let payload = serde_json::json!({
                "plan": plan_name,
                "task": task_id,
                "reason": "agent_left_uncommitted_work",
                "files": trimmed,
            });
            broadcast_event(&state.broadcast_tx, "auto_mode_paused", payload.clone());
            let conn = state.db.lock().unwrap();
            crate::audit::log(
                &conn,
                &org_id,
                None,
                Some("branchwork-auto-mode"),
                crate::auto_mode::actions::AUTO_MODE_PAUSED,
                crate::audit::resources::PLAN,
                Some(&plan_name),
                Some(&payload.to_string()),
            );
            return;
        }
        crate::agents::TreeState::Clean | crate::agents::TreeState::Unknown => {}
    }

    // Dedupe: the first Stop wins. `HashSet::insert` returns true on the
    // first insert and false on subsequent ones — flip it into a "first
    // call" gate. See the field doc on `AppState::auto_finish_dedupe`
    // for why this is necessary.
    let first_call = state
        .auto_finish_dedupe
        .lock()
        .unwrap()
        .insert(agent_id.clone());
    if !first_call {
        eprintln!("[hooks] dedupe: ignoring duplicate Stop for agent {agent_id}");
        return;
    }

    // Spawn graceful_exit so we don't block the HTTP response on the PTY
    // write. The registry handle is a cheap clone (Arc inside).
    let registry = state.registry.clone();
    let agent_id_for_spawn = agent_id.clone();
    tokio::spawn(async move {
        registry.graceful_exit(&agent_id_for_spawn).await;
    });

    // Audit row is per-agent (resources::AGENT). Diff carries the
    // discriminator so a later idle-timeout trigger can write the same
    // action with `{trigger: "idle_timeout"}` and reuse this filter.
    {
        let conn = state.db.lock().unwrap();
        crate::audit::log(
            &conn,
            &org_id,
            None,
            Some("branchwork-auto-mode"),
            crate::audit::actions::AGENT_AUTO_FINISH,
            crate::audit::resources::AGENT,
            Some(&agent_id),
            Some(&serde_json::json!({ "trigger": "stop_hook" }).to_string()),
        );
    }
    broadcast_event(
        &state.broadcast_tx,
        "auto_finish_triggered",
        serde_json::json!({
            "agent_id": agent_id,
            "plan": plan_name,
            "task": task_id,
            "trigger": "stop_hook",
        }),
    );
}

#[cfg(test)]
mod tests {
    //! Tests drive `handle_stop_hook` directly with a synthesized
    //! `AppState`. The function is the entire decision tree the brief
    //! covers; the public `receive_hook` is a thin wrapper that always
    //! calls it on `Stop` events, so testing the helper is equivalent.

    use super::*;

    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use rusqlite::params;
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use crate::config::Effort;
    use crate::db::Db;
    use crate::saas::runner_ws::new_runner_registry;

    fn fresh_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("branchwork.db");
        (crate::db::init(&path), dir)
    }

    fn test_app_state(db: Db, plans_dir: PathBuf) -> (AppState, broadcast::Receiver<String>) {
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
        let state = AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(Effort::Medium)),
            broadcast_tx,
            registry,
            runners: new_runner_registry(),
            settings_path: PathBuf::from("/tmp/branchwork-test-hooks-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
        };
        (state, rx)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        if !out.status.success() {
            panic!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }

    fn git_init_with_clean_tree(cwd: &Path) {
        std::fs::create_dir_all(cwd).unwrap();
        run_git(cwd, &["init", "-q", "-b", "master"]);
        run_git(cwd, &["config", "user.email", "t@t.test"]);
        run_git(cwd, &["config", "user.name", "Test"]);
        std::fs::write(cwd.join("README.md"), "init").unwrap();
        run_git(cwd, &["add", "README.md"]);
        run_git(cwd, &["commit", "-q", "-m", "initial"]);
    }

    fn enable_auto_mode(db: &Db, plan: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1) \
             ON CONFLICT(plan_name) DO UPDATE SET enabled = 1, paused_reason = NULL",
            params![plan],
        )
        .unwrap();
    }

    fn map_plan_to_project(db: &Db, plan: &str, project: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_project (plan_name, project) VALUES (?1, ?2) \
             ON CONFLICT(plan_name) DO UPDATE SET project = excluded.project",
            params![plan, project],
        )
        .unwrap();
    }

    fn seed_running_agent(db: &Db, id: &str, session_id: &str, cwd: &Path, plan: &str, task: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents \
                (id, session_id, cwd, status, mode, plan_name, task_id, org_id) \
             VALUES (?1, ?2, ?3, 'running', 'pty', ?4, ?5, 'default-org')",
            params![id, session_id, cwd.to_string_lossy(), plan, task],
        )
        .unwrap();
    }

    fn paused_reason(db: &Db, plan: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
            params![plan],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn audit_actions_for(db: &Db, resource_id: &str) -> Vec<String> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT action FROM audit_logs WHERE resource_id = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map(params![resource_id], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    }

    fn audit_diff_for(db: &Db, resource_id: &str, action: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT diff FROM audit_logs WHERE resource_id = ?1 AND action = ?2 ORDER BY id LIMIT 1",
            params![resource_id, action],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    fn drain_event_types(rx: &mut broadcast::Receiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && let Some(t) = v.get("type").and_then(|t| t.as_str())
            {
                out.push(t.to_string());
            }
        }
        out
    }

    fn drain_events(rx: &mut broadcast::Receiver<String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                out.push(v);
            }
        }
        out
    }

    /// How many `agent.auto_finish` audit rows are recorded for `agent_id`.
    /// Used by the dedupe tests as the side-channel proxy for "how many
    /// times did `graceful_exit` fire?". The handler unconditionally
    /// writes an `AGENT_AUTO_FINISH` audit row in the same gated block
    /// that spawns `graceful_exit`, so the audit count and the
    /// graceful-exit count are 1-to-1.
    fn auto_finish_audit_count(db: &Db, agent_id: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM audit_logs WHERE resource_id = ?1 AND action = ?2",
            params![agent_id, crate::audit::actions::AGENT_AUTO_FINISH],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    }

    /// How many `auto_finish_triggered` broadcasts have been emitted on
    /// `rx` so far. Counterpart to [`auto_finish_audit_count`] for the
    /// in-process WS broadcast side of the same gate.
    fn auto_finish_broadcast_count(rx: &mut broadcast::Receiver<String>) -> usize {
        drain_event_types(rx)
            .iter()
            .filter(|t| t.as_str() == "auto_finish_triggered")
            .count()
    }

    #[tokio::test]
    async fn unknown_session_is_a_silent_no_op() {
        let (db, dir) = fresh_db();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let (state, mut rx) = test_app_state(db.clone(), plans_dir);

        handle_stop_hook(&state, "no-such-session").await;

        // No auto_finish_triggered or auto_mode_paused broadcast.
        let events = drain_event_types(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e == "auto_finish_triggered" || e == "auto_mode_paused"),
            "no auto-mode events expected, got: {events:?}"
        );
        // No audit rows.
        let conn = db.lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    /// Brief acceptance #3: `stop_on_non_running_agent_is_noop`.
    /// Agent row is `completed` when the Stop arrives — the Stop event
    /// itself still records in `hook_events` (handled unconditionally
    /// by the `receive_hook` wrapper, not exercised here), but the
    /// auto-mode side of `handle_stop_hook` early-returns at the
    /// `status != "running"` guard. No audit row, no broadcast, no
    /// graceful_exit attempt.
    #[tokio::test]
    async fn stop_on_non_running_agent_is_noop() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_with_clean_tree(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        // Seed an agent then mark it completed — the second Stop coming in
        // after we sent /exit must not re-fire the auto-finish path.
        seed_running_agent(&db, "a-1", "s-1", &cwd, "p", "1.1");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET status = 'completed' WHERE id = 'a-1'",
                [],
            )
            .unwrap();
        }
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), plans_dir);
        handle_stop_hook(&state, "s-1").await;

        let events = drain_event_types(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e == "auto_finish_triggered" || e == "auto_mode_paused"),
            "no auto-mode events expected, got: {events:?}"
        );
        assert!(audit_actions_for(&db, "a-1").is_empty());
        assert!(audit_actions_for(&db, "p").is_empty());
    }

    /// Brief acceptance #4: `stop_on_agent_with_auto_mode_off_is_telemetry_only`.
    /// Plan has no `plan_auto_mode` row (auto-mode never opted in), so
    /// `handle_stop_hook` exits at the `auto_mode_enabled` gate. The
    /// outer `receive_hook` wrapper still inserts the `hook_events`
    /// telemetry row unconditionally — that path is exercised by the
    /// integration shape (we drive the helper directly here, but the
    /// wrapper's `db.execute("INSERT INTO hook_events ...")` runs
    /// before `handle_stop_hook` and is independent of the auto-mode
    /// branch).
    #[tokio::test]
    async fn stop_on_agent_with_auto_mode_off_is_telemetry_only() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_with_clean_tree(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent(&db, "a-1", "s-1", &cwd, "p", "1.1");
        // No enable_auto_mode — gate stays false.
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), plans_dir);
        handle_stop_hook(&state, "s-1").await;

        let events = drain_event_types(&mut rx);
        assert!(
            !events
                .iter()
                .any(|e| e == "auto_finish_triggered" || e == "auto_mode_paused"),
            "no auto-mode events expected, got: {events:?}"
        );
        assert!(audit_actions_for(&db, "a-1").is_empty());
    }

    /// Brief acceptance #2: `stop_with_dirty_tree_pauses_plan`. Tracked
    /// file with uncommitted changes ⇒ `paused_reason =
    /// "agent_left_uncommitted_work"`, no graceful-exit fired (no
    /// `AGENT_AUTO_FINISH` audit row, no `auto_finish_triggered`
    /// broadcast). The `auto_mode_paused` broadcast and audit row land
    /// instead, with a trimmed `files` list for the dashboard.
    #[tokio::test]
    async fn stop_with_dirty_tree_pauses_plan() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_with_clean_tree(&cwd);
        // Modify a tracked file without committing — porcelain reports it.
        std::fs::write(cwd.join("README.md"), "modified but not committed").unwrap();

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent(&db, "a-1", "s-1", &cwd, "p", "1.1");
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), plans_dir);
        handle_stop_hook(&state, "s-1").await;

        // Plan paused with the uncommitted-work reason.
        assert_eq!(
            paused_reason(&db, "p").as_deref(),
            Some("agent_left_uncommitted_work"),
        );

        // auto_mode_paused broadcast went out with the matching payload.
        let evs = drain_events(&mut rx);
        let paused = evs
            .iter()
            .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_mode_paused"))
            .expect("expected auto_mode_paused broadcast");
        let data = paused.get("data").unwrap();
        assert_eq!(data.get("plan").and_then(|v| v.as_str()), Some("p"));
        assert_eq!(data.get("task").and_then(|v| v.as_str()), Some("1.1"));
        assert_eq!(
            data.get("reason").and_then(|v| v.as_str()),
            Some("agent_left_uncommitted_work")
        );
        let files = data.get("files").and_then(|v| v.as_array()).unwrap();
        assert!(!files.is_empty(), "expected dirty file list, got empty");

        // No auto_finish_triggered fired.
        assert!(
            !evs.iter()
                .any(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_finish_triggered")),
            "auto_finish_triggered must not fire when tree is dirty"
        );

        // Audit row landed for AUTO_MODE_PAUSED on the plan resource.
        let actions = audit_actions_for(&db, "p");
        assert!(
            actions
                .iter()
                .any(|a| a == crate::auto_mode::actions::AUTO_MODE_PAUSED),
            "expected {} in {actions:?}",
            crate::auto_mode::actions::AUTO_MODE_PAUSED
        );

        // No AGENT_AUTO_FINISH on the agent.
        let agent_actions = audit_actions_for(&db, "a-1");
        assert!(
            !agent_actions
                .iter()
                .any(|a| a == crate::audit::actions::AGENT_AUTO_FINISH),
            "AGENT_AUTO_FINISH must not fire when tree is dirty"
        );
    }

    /// Brief acceptance #1:
    /// `stop_on_running_auto_mode_agent_with_clean_tree_triggers_graceful_exit`.
    /// Fires two Stops for the same session and asserts exactly-once
    /// `graceful_exit` semantics (`AGENT_AUTO_FINISH` count == 1,
    /// `auto_finish_triggered` broadcast count == 1). The second
    /// Stop is gated by `auto_finish_dedupe` because the agent's
    /// status is still `running` (the row only flips to `completed`
    /// inside `on_agent_exit`, which runs after the PTY actually
    /// closes — we don't drive a real PTY here).
    #[tokio::test]
    async fn stop_on_running_auto_mode_agent_with_clean_tree_triggers_graceful_exit() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_with_clean_tree(&cwd);

        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent(&db, "a-1", "s-1", &cwd, "p", "1.1");
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), plans_dir);

        // Two Stops for the same session in quick succession. The agent
        // row stays `running` for both — graceful_exit must only fire
        // for the first.
        handle_stop_hook(&state, "s-1").await;
        handle_stop_hook(&state, "s-1").await;
        // Give the spawned graceful_exit task a tick to run (it will no-op
        // because no live PTY is attached, but we want to make sure the
        // spawn itself doesn't panic).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Plan stays unpaused on the clean path.
        assert!(paused_reason(&db, "p").is_none());

        // Counter assertion #1: the audit log proxy. AGENT_AUTO_FINISH is
        // written inside the same gated block that spawns graceful_exit,
        // so audit count == graceful_exit call count.
        assert_eq!(
            auto_finish_audit_count(&db, "a-1"),
            1,
            "graceful_exit must fire exactly once across two Stop hooks"
        );

        // auto_finish_triggered broadcast carries the expected fields,
        // and only one was emitted.
        let evs = drain_events(&mut rx);
        let triggered: Vec<&serde_json::Value> = evs
            .iter()
            .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("auto_finish_triggered"))
            .collect();
        assert_eq!(
            triggered.len(),
            1,
            "expected exactly one auto_finish_triggered broadcast, got {}",
            triggered.len()
        );
        let data = triggered[0].get("data").unwrap();
        assert_eq!(data.get("agent_id").and_then(|v| v.as_str()), Some("a-1"));
        assert_eq!(data.get("plan").and_then(|v| v.as_str()), Some("p"));
        assert_eq!(data.get("task").and_then(|v| v.as_str()), Some("1.1"));
        assert_eq!(
            data.get("trigger").and_then(|v| v.as_str()),
            Some("stop_hook")
        );

        // Audit row carries the trigger discriminator in the diff.
        let diff = audit_diff_for(&db, "a-1", crate::audit::actions::AGENT_AUTO_FINISH)
            .expect("AGENT_AUTO_FINISH should have a diff");
        assert!(
            diff.contains("\"trigger\":\"stop_hook\""),
            "diff should pin trigger to stop_hook, got: {diff}"
        );

        // No pause-side audit on the plan.
        let plan_actions = audit_actions_for(&db, "p");
        assert!(
            !plan_actions
                .iter()
                .any(|a| a == crate::auto_mode::actions::AUTO_MODE_PAUSED),
            "AUTO_MODE_PAUSED must not fire on the clean path"
        );

        // Dedupe set has the agent recorded so a third Stop would also
        // be debounced.
        assert!(
            state.auto_finish_dedupe.lock().unwrap().contains("a-1"),
            "agent_id should be retained in auto_finish_dedupe"
        );
    }

    /// Brief acceptance #5: `two_stops_in_quick_succession_call_graceful_exit_once`.
    /// Same end behaviour as the test above, but factored into its own
    /// function with a name that grep-matches the acceptance criteria
    /// readout. Asserts only the counters (audit row count and
    /// broadcast count), keeping the body minimal.
    #[tokio::test]
    async fn two_stops_in_quick_succession_call_graceful_exit_once() {
        let (db, dir) = fresh_db();
        let cwd = dir.path().join("project");
        git_init_with_clean_tree(&cwd);
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();

        seed_running_agent(&db, "a-1", "s-1", &cwd, "p", "1.1");
        enable_auto_mode(&db, "p");
        map_plan_to_project(&db, "p", &cwd.to_string_lossy());

        let (state, mut rx) = test_app_state(db.clone(), plans_dir);

        // Fire twice; the second call must be a dedupe no-op because the
        // first call hasn't yet flipped `agents.status` away from
        // `running` (that flip happens inside `on_agent_exit`, after the
        // PTY closes).
        handle_stop_hook(&state, "s-1").await;
        handle_stop_hook(&state, "s-1").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            auto_finish_audit_count(&db, "a-1"),
            1,
            "AGENT_AUTO_FINISH must be written exactly once"
        );
        assert_eq!(
            auto_finish_broadcast_count(&mut rx),
            1,
            "auto_finish_triggered must be broadcast exactly once"
        );
    }
}
