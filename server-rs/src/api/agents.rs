use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::Deserialize;

use crate::agents::git_ops;
use crate::auth::OptionalAuthUser;
use crate::saas::runner_protocol::MergeOutcome as WireMergeOutcome;
use crate::saas::runner_rpc::RunnerRpcError;
use crate::state::AppState;

/// Decide the merge target. **Pure** — all git probes happen in dispatchers
/// (the runner in SaaS, [`crate::agents::git_default_branch`] /
/// [`crate::agents::git_list_branches`] in standalone) and pass resolved
/// inputs in.
///
/// Priority: explicit (if it appears in `available_branches`) → default → "main".
///
/// `available_branches` is the runner-side branch list used to validate
/// `explicit_into`: the dropdown UI picks from this list, but the form-encoded
/// path could theoretically pass a typo. When `available_branches` is `None`
/// the validation is skipped (caller knows the input is trusted, e.g. the
/// no-override default-merge case).
///
/// The agent's stored `source_branch` is intentionally NOT consulted:
/// a stale auto-captured value (from whichever branch the user happened
/// to be on at spawn time) was the source of the wrong-target merge
/// bug. The escape hatch is the dropdown's `into` parameter, never the
/// auto-populated DB column.
fn resolve_merge_target(
    explicit_into: Option<&str>,
    default_branch: Option<&str>,
    available_branches: Option<&[String]>,
) -> String {
    if let Some(into) = explicit_into {
        let resolves = match available_branches {
            Some(branches) => branches.iter().any(|b| b == into),
            None => true,
        };
        if resolves {
            return into.to_string();
        }
    }
    if let Some(d) = default_branch {
        return d.to_string();
    }
    "main".to_string()
}

/// Map a [`RunnerRpcError`] to the canonical HTTP response. The conventions
/// (503 no_runner_connected / 504 runner_unavailable / 500 other) match what
/// `api/settings.rs::list_folders` and `api/plans.rs::create_plan` already
/// return so the dashboard can use a single error-handling path.
fn rpc_error_response(e: RunnerRpcError) -> axum::response::Response {
    match e {
        RunnerRpcError::NoConnectedRunner => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no_runner_connected" })),
        )
            .into_response(),
        RunnerRpcError::Timeout | RunnerRpcError::RunnerDisconnected => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({ "error": "runner_unavailable" })),
        )
            .into_response(),
        RunnerRpcError::InvalidRequest | RunnerRpcError::SendFailed => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "runner_request_failed" })),
        )
            .into_response(),
    }
}

/// Body for `POST /api/agents/{id}/merge`. `into` is the optional
/// dropdown override; absent means "use the canonical default branch".
#[derive(Deserialize, Default)]
pub struct MergeBody {
    #[serde(default)]
    pub into: Option<String>,
}

pub async fn list_agents(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare(
            "SELECT id, session_id, pid, parent_agent_id, plan_name, task_id, \
                    cwd, status, mode, prompt, started_at, finished_at, \
                    last_tool, last_activity_at, base_commit, branch, \
                    source_branch, cost_usd, driver, merge_status, \
                    spawn_error \
             FROM agents WHERE org_id = ?1 \
             ORDER BY started_at DESC LIMIT 50",
        )
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map(params![org_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                // session_id is NULLable in the schema (db.rs:278). Local
                // PTY agents always carry one, but remote-mode tracking
                // rows inserted on the SaaS server when the runner picks
                // up a StartAgent stay NULL until the runner echoes one
                // back — which today nothing in the wire protocol does.
                // Decoding as String! made every remote-mode agent
                // silently dropped by the surrounding query_map().flatten().
                "session_id": row.get::<_, Option<String>>(1)?,
                "pid": row.get::<_, Option<i64>>(2)?,
                "parent_agent_id": row.get::<_, Option<String>>(3)?,
                "plan_name": row.get::<_, Option<String>>(4)?,
                "task_id": row.get::<_, Option<String>>(5)?,
                "cwd": row.get::<_, String>(6)?,
                "status": row.get::<_, String>(7)?,
                "mode": row.get::<_, String>(8)?,
                "prompt": row.get::<_, Option<String>>(9)?,
                "started_at": row.get::<_, String>(10)?,
                "finished_at": row.get::<_, Option<String>>(11)?,
                "last_tool": row.get::<_, Option<String>>(12)?,
                "last_activity_at": row.get::<_, Option<String>>(13)?,
                "base_commit": row.get::<_, Option<String>>(14)?,
                "branch": row.get::<_, Option<String>>(15)?,
                "source_branch": row.get::<_, Option<String>>(16)?,
                "cost_usd": row.get::<_, Option<f64>>(17)?,
                "driver": row.get::<_, Option<String>>(18)?,
                // T2.3 (Task 2.3): `merge_status='deferred_for_cadence'`
                // is the signal the dashboard reads to know which agents
                // are sitting on committed branches waiting for the
                // cadence boundary. NULL on every non-deferred agent.
                "merge_status": row.get::<_, Option<String>>(19)?,
                // Task 1.1 (runner-install-and-spawn-reliability): pre-
                // rendered "runner could not spawn: <path> (<errno_tag>)"
                // message, written by the SaaS runner_ws handler when
                // the runner reports `AgentSpawnFailed`. NULL on every
                // successful spawn — UI gates the inline error banner
                // on this field being non-null.
                "spawn_error": row.get::<_, Option<String>>(20)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    200
}

pub async fn get_agent_output(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<OutputQuery>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT id, agent_id, message_type, content, timestamp FROM agent_output WHERE agent_id = ? ORDER BY id ASC LIMIT ? OFFSET ?")
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map(params![id, q.limit, q.offset], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "message_type": row.get::<_, String>(2)?,
                "content": row.get::<_, String>(3)?,
                "timestamp": row.get::<_, String>(4)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

pub async fn kill_agent(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    match crate::agents::spawn_ops::kill_agent_dispatch(&state, &org_id, &id).await {
        Ok(true) => {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::AGENT_KILL,
                crate::audit::resources::AGENT,
                Some(&id),
                None,
            );
            (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
            .into_response(),
        Err(e) => rpc_error_response(e),
    }
}

/// Minimum runner version that handles the `AgentInput` wire variant
/// (T5.9 of the saas-folder-listing-via-runner plan). Older runners
/// (notably the v0.3.0 production runner whose `runner_protocol.rs`
/// predates the variant) silently drop the envelope — graceful exit
/// silently fails and the operator has to fall back to Kill.
///
/// The minimum is conservative: `AgentInput` actually landed during
/// v0.3.0's lifespan in commit 279407a, but we can't tell at runtime
/// which v0.3.0 build a runner came from. v0.4.0 is the first version
/// that is guaranteed to carry the handler (the variant + handler had
/// both shipped well before the 0.4.0 bump).
const MIN_RUNNER_VERSION_FOR_FINISH: (u32, u32, u32) = (0, 4, 0);

/// Lenient `<major>.<minor>.<patch>` parser. Anything after the third
/// dot or a `-`/`+` is ignored (so `0.4.0-rc1` and `0.4.0+sha` both
/// parse the same as `0.4.0`). Mirrors `runner_ws::parse_semver_lenient`
/// — duplicated here so the comparison stays local to the Finish
/// gate; the runner_ws helper is private.
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let head = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = head.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// `true` when `version` is at least [`MIN_RUNNER_VERSION_FOR_FINISH`].
/// Unparseable strings return `false` so a runner that fails to report
/// its version cleanly is treated as too old — the conservative choice
/// when the alternative is a silent 200 + dead Finish button.
fn runner_meets_min_version_for_finish(version: Option<&str>) -> bool {
    match version.and_then(parse_semver) {
        Some(v) => v >= MIN_RUNNER_VERSION_FOR_FINISH,
        None => false,
    }
}

/// Render `MIN_RUNNER_VERSION_FOR_FINISH` for body / log strings.
fn min_version_for_finish_str() -> String {
    let (a, b, c) = MIN_RUNNER_VERSION_FOR_FINISH;
    format!("{a}.{b}.{c}")
}

/// `POST /api/agents/:id/finish` — send the CLI's graceful-exit sequence
/// (e.g. `/exit` for Claude Code) so the agent shuts down cleanly. Unlike
/// Kill this preserves any in-flight commit/cleanup the agent wants to do.
///
/// **SaaS-mode fix-loud contract (Task 5.5,
/// runner-install-and-spawn-reliability plan).** Pre-fix the SaaS path
/// looked up the "most recently seen online runner" via
/// `pick_runner_for_org` and shipped a `WireMessage::AgentInput`
/// envelope to that runner via `command_tx.send`, then returned 200.
/// Three failure modes silently dropped graceful exit:
///   - **Wrong runner targeted** if the org had multiple runners — the
///     in-flight session daemon lives only on the runner that spawned
///     it, so an `AgentInput` to a different runner is a no-op (the
///     runner-side handler looks up `state.agents[id]` and bails on
///     miss).
///   - **Version too old** — runners pre-AgentInput (v0.3.x) have no
///     handler and silently drop the envelope.
///   - **Spawning runner offline** — the envelope landed on the wire
///     but the daemon was unreachable; the runner-side outbox doesn't
///     replay `AgentInput` (it's best-effort).
///
/// Post-fix Finish targets the runner pinned on `agents.runner_id` (the
/// runner that received the `StartAgent` envelope at spawn time), and
/// refuses with 409 when that runner is unavailable or below
/// [`MIN_RUNNER_VERSION_FOR_FINISH`].
pub async fn finish_agent(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Standalone fast-path: in-process registry knows how to write the
    // driver's exit sequence into the supervisor's command_tx.
    if state.registry.graceful_exit(&id).await {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::AGENT_FINISH,
            crate::audit::resources::AGENT,
            Some(&id),
            None,
        );
        return (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response();
    }

    // SaaS path: the supervisor lives in the runner container so the
    // in-process registry has nothing for `id`. Look up the agent's
    // driver and pinned runner_id, ask the driver registry for the
    // exit byte sequence, and ship it as a `WireMessage::AgentInput`
    // envelope through THE SPAWNING runner's `command_tx`.
    let default_driver = crate::persisted_settings::PersistedSettings::load(&state.settings_path)
        .default_driver()
        .to_string();
    let row: Option<(String, String, String, Option<String>)> = {
        let db = state.db.lock().unwrap();
        db.query_row(
            &format!(
                "SELECT mode, COALESCE(driver, '{}'), org_id, runner_id \
                 FROM agents WHERE id = ?",
                default_driver
            ),
            params![id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .ok()
    };
    let Some((mode, driver_name, org_id, spawning_runner_id)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
            .into_response();
    };
    if mode != "remote" {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "Agent not found or driver does not support graceful exit"
            })),
        )
            .into_response();
    }
    let exit_seq: Vec<u8> = match state.registry.drivers.exit_sequence_for(Some(&driver_name)) {
        Some(seq) => seq,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("driver '{driver_name}' has no graceful-exit sequence")
                })),
            )
                .into_response();
        }
    };

    // Resolve the runner to dispatch to. Honour `agents.runner_id` when
    // set (Task 5.5 — the runner that actually spawned the daemon), and
    // fall back to `pick_runner_for_org` only for legacy rows that
    // predate the column. The fallback is intentionally narrow: any new
    // SaaS spawn populates the column, so the fallback only runs on
    // rows that existed before this migration.
    let dispatch_target_runner_id = match spawning_runner_id {
        Some(rid) => rid,
        None => {
            let conn = state.db.lock().unwrap();
            match conn
                .query_row(
                    "SELECT id FROM runners WHERE org_id = ?1 \
                     ORDER BY (status = 'online') DESC, last_seen_at DESC LIMIT 1",
                    params![org_id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            {
                Some(rid) => rid,
                None => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"error": "no_runner_connected"})),
                    )
                        .into_response();
                }
            }
        }
    };

    // Read the target runner's status + version BEFORE acquiring the
    // (async) runners-registry lock so we can 409 with a fully-formed
    // body without holding any locks.
    let (runner_status, runner_version): (Option<String>, Option<String>) = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT status, version FROM runners WHERE id = ?1",
            params![dispatch_target_runner_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .ok()
        .unwrap_or((None, None))
    };

    // Spawning runner missing (deleted) or offline ⇒ 409
    // spawn_runner_unavailable. Pre-fix this would 200 silently after
    // shipping an envelope to a different (online) runner that had no
    // PTY for this agent.
    let is_online = runner_status.as_deref() == Some("online");
    if !is_online {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "spawn_runner_unavailable",
                "code": "spawn_runner_unavailable",
                "message": format!(
                    "the runner that spawned this agent ({dispatch_target_runner_id}) is not online — \
                     either bring it back online or kill the agent",
                ),
                "runnerId": dispatch_target_runner_id,
                "runnerStatus": runner_status.unwrap_or_else(|| "unknown".to_string()),
            })),
        )
            .into_response();
    }

    // Spawning runner online but below the minimum version ⇒ 409
    // spawn_runner_too_old. This is the production scenario the brief
    // calls out: a v0.3.0 runner whose `runner_protocol.rs` has no
    // `AgentInput` variant.
    if !runner_meets_min_version_for_finish(runner_version.as_deref()) {
        let min = min_version_for_finish_str();
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "spawn_runner_too_old",
                "code": "spawn_runner_too_old",
                "message": format!(
                    "the runner that spawned this agent ({dispatch_target_runner_id}) is too old to \
                     handle Finish — runner reports {actual} but Finish requires >= {min}; \
                     upgrade the runner or kill the agent",
                    actual = runner_version.as_deref().unwrap_or("(unknown)"),
                ),
                "runnerId": dispatch_target_runner_id,
                "runnerVersion": runner_version,
                "minimumVersion": min,
            })),
        )
            .into_response();
    }

    use base64::Engine;
    let envelope = crate::saas::runner_protocol::Envelope::best_effort(
        "server".to_string(),
        crate::saas::runner_protocol::WireMessage::AgentInput {
            agent_id: id.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(&exit_seq),
        },
    );
    let json = match serde_json::to_string(&envelope) {
        Ok(j) => j,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "envelope serialise failed"})),
            )
                .into_response();
        }
    };

    // Look up the live WS handle for the spawning runner. Status=online
    // (above) is what's in the DB, but the actual WS handle could have
    // disconnected since — the runners-registry lookup is the
    // authoritative live check.
    let runners = state.runners.lock().await;
    let Some(runner) = runners.get(&dispatch_target_runner_id) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "spawn_runner_unavailable",
                "code": "spawn_runner_unavailable",
                "message": format!(
                    "the runner that spawned this agent ({dispatch_target_runner_id}) is not currently \
                     connected — either bring it back online or kill the agent",
                ),
                "runnerId": dispatch_target_runner_id,
            })),
        )
            .into_response();
    };
    if runner.command_tx.send(json).is_err() {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({"error": "runner channel closed"})),
        )
            .into_response();
    }
    drop(runners);

    let db = state.db.lock().unwrap();
    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::AGENT_FINISH,
        crate::audit::resources::AGENT,
        Some(&id),
        None,
    );
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

pub async fn get_agent_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up the agent's cwd and base_commit
    let (cwd, base_commit): (String, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, base_commit FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let base = match base_commit {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": "No base commit recorded (not a git repo?)"
            }))
            .into_response();
        }
    };

    // Run git diff
    let diff_output = std::process::Command::new("git")
        .args(["diff", &base, "--no-color"])
        .current_dir(&cwd)
        .output();

    let diff = match diff_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": format!("git diff failed: {stderr}")
            }))
            .into_response();
        }
        Err(e) => {
            return Json(serde_json::json!({
                "diff": "",
                "files": [],
                "error": format!("Failed to run git: {e}")
            }))
            .into_response();
        }
    };

    // Get list of changed files with stats
    let stat_output = std::process::Command::new("git")
        .args(["diff", &base, "--stat", "--no-color"])
        .current_dir(&cwd)
        .output();

    let stat = match stat_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        _ => String::new(),
    };

    // Get changed file names
    let name_output = std::process::Command::new("git")
        .args(["diff", &base, "--name-only"])
        .current_dir(&cwd)
        .output();

    let files: Vec<String> = match name_output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        _ => Vec::new(),
    };

    Json(serde_json::json!({
        "diff": diff,
        "stat": stat,
        "files": files,
        "base_commit": base,
    }))
    .into_response()
}

/// `GET /api/agents/:id/merge-targets` — list candidate merge targets
/// for the dropdown next to the Merge button.
///
/// Response: `{ "default": "master" | null, "available": [...] }`.
/// `default` is the canonical default branch (highlighted in the UI);
/// `available` is the alternatives — local branches except the default
/// itself (it's the implicit choice) and the agent's own task branch
/// (merging into yourself is nonsense and the empty-branch guard would
/// 409 anyway).
///
/// SaaS path (future): the handler dispatches to the runner via two
/// RPCs (`GetDefaultBranch`, `ListBranches`) and assembles the same
/// JSON. The dashboard never knows the difference — keep the response
/// shape frozen so the runner refactor stays an in-place swap.
pub async fn list_merge_targets(
    State(state): State<AppState>,
    _auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (cwd, task_branch, org_id): (String, Option<String>, String) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, org_id FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let cwd_path = std::path::Path::new(&cwd);
    let default = match git_ops::default_branch(&state.db, &state.runners, &org_id, cwd_path).await
    {
        Ok(d) => d,
        Err(e) => return rpc_error_response(e),
    };
    let mut branches =
        match git_ops::list_branches(&state.db, &state.runners, &org_id, cwd_path).await {
            Ok(b) => b,
            Err(e) => return rpc_error_response(e),
        };

    if let Some(task) = task_branch.as_deref() {
        branches.retain(|b| b != task);
    }
    if let Some(d) = default.as_deref() {
        branches.retain(|b| b != d);
    }

    Json(serde_json::json!({
        "default": default,
        "available": branches,
    }))
    .into_response()
}

/// Outcome of [`merge_agent_branch_inner`]. Used by the HTTP wrapper at
/// [`merge_agent_branch`] (mapped onto the existing JSON response shape)
/// and by background callers like the auto-mode loop in T1.2 (which
/// reacts to `had_conflict` and `merged_sha.is_some()` to decide whether
/// to advance or pause).
///
/// `task_branch` is included alongside `target_branch` so the HTTP
/// wrapper can echo it in the `merged` / `branch` JSON fields without a
/// second DB lookup. Both are empty for early-return failures (agent
/// not found, no task branch — before target resolution).
pub struct MergeOutcome {
    /// Merge commit SHA on `target_branch`. `None` for any failure.
    pub merged_sha: Option<String>,
    /// Branch the merge targeted (resolved canonical default or the
    /// validated `into` override).
    pub target_branch: String,
    /// The agent's task branch — the one that was merged.
    pub task_branch: String,
    /// `git merge` reported a conflict; the working tree is clean
    /// (the runner ran `git merge --abort`). Auto-mode pauses the
    /// plan when this is true.
    pub had_conflict: bool,
    /// User-facing error message, if any. Sentinel values the HTTP
    /// wrapper recognizes for status-code mapping:
    /// - `"Agent not found"` → 404
    /// - `"Agent has no task branch"` → 400
    /// - starts with `"task branch has no commits"` → 409 (empty branch)
    /// - `"no_runner_connected"` → 503
    /// - `"runner_unavailable"` → 504
    /// - `"runner_request_failed"` → 500
    /// - other (checkout/merge failures) → 500
    pub error: Option<String>,
    /// Phase 3.3: what the conflict arm did with an at-merge resolver.
    /// Only ever `Some` together with `had_conflict: true`, and only when
    /// the resolver path applied (standalone + worktrees + auto-mode). The
    /// auto-mode loop ([`crate::auto_mode::run_merge_step`]) reads this to
    /// decide whether to leave the plan running (a resolver was spawned)
    /// or pause it with `merge_conflict_unresolved` (the cap is spent).
    /// `None` for every non-conflict outcome and for conflicts where no
    /// resolver applies (manual merge / SaaS / no worktree / non-auto-mode)
    /// — there the loop keeps its pre-3.3 `merge_conflict` pause.
    pub resolver: Option<crate::agents::ResolverDisposition>,
}

/// Run the merge flow for an agent's task branch — the body of
/// [`merge_agent_branch`] with the HTTP/auth/audit shell peeled off so
/// background callers (T1.2 auto-mode loop) can invoke it directly.
///
/// `into = None` selects the canonical default branch; `into = Some(b)`
/// is the dropdown override. An empty string is treated like `None`. An
/// unresolvable override falls through to the default (matches the HTTP
/// behaviour codified by `merge_with_unresolvable_into_body_falls_back_to_default`).
///
/// `trigger_ci = true` (the default for user-driven merges + the *final*
/// merge in an auto-mode cadence batch) spawns
/// [`crate::ci::trigger_after_merge`], which pushes the resulting
/// trunk SHA to origin and inserts the `ci_runs` row. `trigger_ci =
/// false` is used by the auto-mode loop when draining a cadence batch:
/// the intermediate merges land locally but don't push, so the cadence
/// boundary produces a single push containing every batched merge in
/// order (Task 2.2 of the ci-cadence-build-vs-test-configurable plan).
///
/// Side effects on success:
/// - Clears `branch` in the `agents` table for *every* row matching the
///   merged branch (siblings — killed retries, check agents — must stop
///   advertising the merged ref).
/// - Broadcasts an `agent_branch_merged` event so connected dashboards
///   refresh immediately.
/// - When `trigger_ci=true`: spawns [`crate::ci::trigger_after_merge`]
///   if the agent has a plan/task and the merge SHA is non-empty.
///
/// Audit logging is **not** done here — the caller knows whose action
/// this is. The HTTP wrapper logs the user; the auto-mode loop will log
/// a system actor.
pub async fn merge_agent_branch_inner(
    state: &AppState,
    agent_id: &str,
    into: Option<&str>,
    trigger_ci: bool,
) -> MergeOutcome {
    // Look up agent details (need plan/task for CI bookkeeping too).
    // org_id picks the runner in SaaS mode.
    let (cwd, branch, plan_name, task_id, org_id): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, plan_name, task_id, org_id FROM agents WHERE id = ?",
            params![agent_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        ) {
            Ok(r) => r,
            Err(_) => {
                return MergeOutcome {
                    merged_sha: None,
                    target_branch: String::new(),
                    task_branch: String::new(),
                    had_conflict: false,
                    error: Some("Agent not found".to_string()),
                    resolver: None,
                };
            }
        }
    };

    let task_branch = match branch {
        Some(b) => b,
        None => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: String::new(),
                task_branch: String::new(),
                had_conflict: false,
                error: Some("Agent has no task branch".to_string()),
                resolver: None,
            };
        }
    };

    let cwd_path = std::path::Path::new(&cwd);

    // Resolve canonical default + (only when needed) available branches via
    // dispatcher. SaaS mode round-trips to the runner; standalone shells out.
    let default = match git_ops::default_branch(&state.db, &state.runners, &org_id, cwd_path).await
    {
        Ok(d) => d,
        Err(e) => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: String::new(),
                task_branch,
                had_conflict: false,
                error: Some(rpc_error_string(e)),
                resolver: None,
            };
        }
    };

    // Treat `Some("")` the same as `None` — an empty `into` means
    // "no override, use the canonical default". Defensive against a
    // future form-encoded path where empty strings sneak through; the
    // current JSON UI already coerces empty selections to `null`.
    let explicit = into.filter(|s| !s.is_empty());

    // Validate explicit only when it's actually provided — saves a
    // round-trip in the common no-override case.
    let available = if explicit.is_some() {
        match git_ops::list_branches(&state.db, &state.runners, &org_id, cwd_path).await {
            Ok(b) => Some(b),
            Err(e) => {
                return MergeOutcome {
                    merged_sha: None,
                    target_branch: String::new(),
                    task_branch,
                    had_conflict: false,
                    error: Some(rpc_error_string(e)),
                    resolver: None,
                };
            }
        }
    } else {
        None
    };
    let target = resolve_merge_target(explicit, default.as_deref(), available.as_deref());

    // Dispatch the merge. SaaS: runner runs the five-step sequence and
    // replies with a WireMergeOutcome. Standalone: run the five steps locally.
    let wire_outcome = match git_ops::merge_branch(
        &state.db,
        &state.runners,
        &org_id,
        cwd_path,
        &target,
        &task_branch,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: target,
                task_branch,
                had_conflict: false,
                error: Some(rpc_error_string(e)),
                resolver: None,
            };
        }
    };

    let merged_sha = match wire_outcome {
        WireMergeOutcome::Ok { merged_sha } => merged_sha,
        WireMergeOutcome::EmptyBranch => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: target,
                task_branch,
                had_conflict: false,
                error: Some(
                    "task branch has no commits — agent exited without committing".to_string(),
                ),
                resolver: None,
            };
        }
        WireMergeOutcome::CheckoutFailed { stderr } => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: target.clone(),
                task_branch,
                had_conflict: false,
                error: Some(format!("Failed to checkout {target}: {stderr}")),
                resolver: None,
            };
        }
        WireMergeOutcome::Conflict {
            conflicted_paths,
            our_diff,
            their_diff,
        } => {
            // Phase 3.2: hand the conflicted worktree off to an at-merge
            // resolver agent instead of just surfacing the conflict. The
            // helper gates on standalone + worktrees + auto-mode (and the
            // fix-attempt cap), so a SaaS / shared-tree / non-auto-mode
            // merge returns `None` and the conflict falls through to the
            // pre-3.3 `merge_conflict` pause unchanged. Phase 3.3: the
            // returned [`ResolverDisposition`] is surfaced on the outcome so
            // `run_merge_step` can leave the plan running while the resolver
            // works (`Spawned`) or pause it with `merge_conflict_unresolved`
            // when the cap is spent (`CapHit`). `had_conflict: true` either
            // way.
            let conflict = crate::agents::ConflictPayload {
                conflicted_paths: conflicted_paths.clone(),
                our_diff,
                their_diff,
            };
            let resolver = maybe_spawn_conflict_resolver(
                state,
                agent_id,
                plan_name.as_deref(),
                task_id.as_deref(),
                &org_id,
                &target,
                cwd_path,
                conflict,
            )
            .await;
            return MergeOutcome {
                merged_sha: None,
                target_branch: target,
                task_branch,
                had_conflict: true,
                error: Some(format!(
                    "Merge conflict in: {}",
                    conflicted_paths.join(", ")
                )),
                resolver,
            };
        }
        WireMergeOutcome::Other { stderr } => {
            return MergeOutcome {
                merged_sha: None,
                target_branch: target,
                task_branch,
                had_conflict: false,
                error: Some(format!("Failed to run git merge: {stderr}")),
                resolver: None,
            };
        }
    };

    // Clear branch in DB for ALL agents with this branch — siblings
    // (killed retries, check agents) shouldn't keep advertising it.
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET branch = NULL WHERE branch = ?",
            params![task_branch],
        )
        .ok();
    }

    // Broadcast so connected dashboards update immediately
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "agent_branch_merged",
        serde_json::json!({
            "id": agent_id,
            "merged": task_branch,
            "into": target,
        }),
    );

    // With worktrees on, the merge ran in the *main* worktree (resolved inside
    // `merge_branch_local`), not the agent's linked `cwd`. Resolve the same
    // main tree here so the post-merge push and branch cleanup run where the
    // merged trunk actually lives — the agent's worktree is on `task_branch`
    // (wrong HEAD for `push_branch`'s rebase path) and is about to be removed.
    // A non-worktree / legacy `cwd` resolves to itself, so this is a no-op
    // there.
    let agent_cwd = std::path::Path::new(&cwd);
    let main_tree = crate::git_helpers::main_worktree(agent_cwd);

    // Kick off CI pipeline (push to origin, record pending run).
    // Only possible when we know which task this agent was for, and when
    // the merged SHA is non-empty (runner-side cleanup may leave it blank
    // on edge cases). Suppressed when the auto-mode batch drain is
    // running (`trigger_ci=false`); the final merge in the batch is
    // what pushes the accumulated trunk in one go.
    if trigger_ci
        && let (Some(plan), Some(task)) = (plan_name, task_id)
        && !merged_sha.is_empty()
    {
        tokio::spawn(crate::ci::trigger_after_merge(crate::ci::TriggerArgs {
            db: state.db.clone(),
            runners: state.runners.clone(),
            org_id: org_id.clone(),
            broadcast_tx: state.broadcast_tx.clone(),
            cwd: main_tree.clone(),
            plan_name: plan,
            task_number: task,
            agent_id: agent_id.to_string(),
            source_branch: target.clone(),
            task_branch: task_branch.clone(),
            merged_sha: merged_sha.clone(),
        }));
    }

    // Worktree-per-agent cleanup (Task 2.3). The branch landed cleanly, so the
    // agent's isolated worktree has done its job. Best-effort + self-gating:
    // a no-op when worktrees are disabled (`BRANCHWORK_USE_WORKTREES=0`) or
    // when `cwd` is the shared project tree (SaaS / legacy in-place spawn).
    // Reached ONLY on merge success — the conflict / failure arms above all
    // return early, so a conflicted worktree is left in place for the Phase 3
    // at-merge resolver. Runs for every merge route (manual dashboard merge,
    // auto-mode loop, cadence drain) because they all converge here.
    crate::agents::remove_agent_worktree(agent_cwd);

    // Drop the merged task branch. `merge_branch_local`'s own `git branch -d`
    // (step 4) could not while the branch was checked out in the agent's
    // linked worktree; now that the worktree is gone, delete it from the main
    // tree so the branch list / merge dropdown stops advertising a dead ref.
    // Best-effort + idempotent: a legacy merge already deleted it (this is a
    // harmless no-op), and a lingering ref is cosmetic, not corrupting.
    crate::git_helpers::delete_local_branch(&main_tree, &task_branch);

    MergeOutcome {
        merged_sha: Some(merged_sha),
        target_branch: target,
        task_branch,
        had_conflict: false,
        error: None,
        resolver: None,
    }
}

/// Phase 3.2/3.3 at-merge conflict resolver hand-off, called from
/// [`merge_agent_branch_inner`]'s conflict arm. Returns the
/// [`ResolverDisposition`] for the auto-mode caller (`run_merge_step`) to
/// react to:
/// - `Some(Spawned{..})` — a resolver agent was spawned in the conflicted
///   worktree and the original agent was marked
///   [`crate::agents::MERGE_STATUS_HELD_FOR_RESOLVER`] (so termination
///   cleanup leaves the tree in place). The loop must **not** pause.
/// - `Some(CapHit{..})` — the shared fix-attempt cap is spent; the loop
///   pauses with `merge_conflict_unresolved`, leaving the worktree on disk.
/// - `None` — no resolver path applied; the loop keeps its pre-3.3
///   `merge_conflict` pause. Returned when any precondition fails:
///   - the agent has no plan/task (can't scope a resolver to a task),
///   - worktrees are disabled (no isolated tree to resolve in safely),
///   - SaaS mode (the worktree lives on the runner; resolver dispatch
///     there is a follow-up — `start_pty_agent` is the standalone path),
///   - auto-mode isn't enabled for the plan (manual merges of non-auto-mode
///     plans keep their old behaviour),
///   - no parent task agent row could be found
///     ([`crate::agents::SpawnError::NoParentAgent`] — defensive).
#[allow(clippy::too_many_arguments)]
async fn maybe_spawn_conflict_resolver(
    state: &AppState,
    original_agent_id: &str,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    org_id: &str,
    target: &str,
    worktree_path: &std::path::Path,
    conflict: crate::agents::ConflictPayload,
) -> Option<crate::agents::ResolverDisposition> {
    let (Some(plan), Some(task)) = (plan_name, task_id) else {
        return None;
    };
    // Only the ORIGINAL task merge gets a resolver. A CI-fix / gate-fix
    // merge (`<task>-fix-<n>`) lands its own branch on trunk and is paused
    // independently by `on_fix_agent_completed` (`fix_merge_failed`); a
    // resolver merge never reaches here (the re-merge runs on the original
    // task agent, whose `task_id` is the bare number). Spawning a resolver
    // for a suffixed task id would burn budget on a branch whose completion
    // handler ignores the resolution. The resolver re-merge path itself uses
    // the bare original task id, so this guard does not block the recursion.
    if task.contains("-fix-") || task.contains("-resolve-") {
        return None;
    }
    if !crate::agents::pty_agent::worktrees_enabled() {
        return None;
    }
    if crate::saas::dispatch::org_has_runner(&state.db, org_id) {
        return None; // SaaS: the worktree lives on the runner — follow-up.
    }
    if !crate::db::auto_mode_enabled(&state.db, plan) {
        return None;
    }

    let attempt = crate::db::task_fix_attempt_count(&state.db, plan, task) + 1;
    match crate::agents::spawn_conflict_resolver(
        &state.registry,
        plan,
        task,
        target,
        &conflict,
        attempt,
        worktree_path,
    )
    .await
    {
        Ok(resolver_id) => {
            // Hold the original agent's worktree for the resolver (Task 2.4
            // hand-off contract): termination cleanup skips a row carrying this
            // marker, so the conflicted tree is never yanked out from under the
            // resolver while it works.
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE agents SET merge_status = ?2 WHERE id = ?1",
                    params![
                        original_agent_id,
                        crate::agents::MERGE_STATUS_HELD_FOR_RESOLVER
                    ],
                )
                .ok();
            }
            eprintln!(
                "[merge] conflict on {plan}/{task} → spawned resolver {resolver_id} (attempt {attempt})"
            );
            Some(crate::agents::ResolverDisposition::Spawned {
                resolver_agent_id: resolver_id,
                attempt,
            })
        }
        Err(crate::agents::SpawnError::CapHit) => {
            eprintln!(
                "[merge] conflict on {plan}/{task} → resolver cap hit (attempt {attempt}); \
                 pausing with merge_conflict_unresolved"
            );
            Some(crate::agents::ResolverDisposition::CapHit {
                attempt,
                conflicted_paths: conflict.conflicted_paths,
            })
        }
        Err(e) => {
            eprintln!(
                "[merge] conflict on {plan}/{task} → resolver not spawned: {e}; \
                 falling through to merge_conflict pause"
            );
            None
        }
    }
}

/// Translate a [`RunnerRpcError`] into the sentinel string the HTTP
/// wrapper recognizes for status-code mapping. Keeps [`MergeOutcome`]
/// transport-agnostic — background callers see a plain `String` and
/// don't need to import [`RunnerRpcError`].
fn rpc_error_string(e: RunnerRpcError) -> String {
    match e {
        RunnerRpcError::NoConnectedRunner => "no_runner_connected".to_string(),
        RunnerRpcError::Timeout | RunnerRpcError::RunnerDisconnected => {
            "runner_unavailable".to_string()
        }
        RunnerRpcError::InvalidRequest | RunnerRpcError::SendFailed => {
            "runner_request_failed".to_string()
        }
    }
}

pub async fn merge_agent_branch(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
    body: Option<Json<MergeBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let outcome = merge_agent_branch_inner(&state, &id, body.into.as_deref(), true).await;

    if outcome.merged_sha.is_some() {
        // Audit-log the user-initiated merge. Re-fetch plan/task — the
        // helper consumed them when spawning the CI trigger and doesn't
        // expose them on the outcome (auto-mode tracks its own context).
        let (plan_name, task_id): (Option<String>, Option<String>) = {
            let db = state.db.lock().unwrap();
            db.query_row(
                "SELECT plan_name, task_id FROM agents WHERE id = ?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or((None, None))
        };

        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::BRANCH_MERGE,
            crate::audit::resources::AGENT,
            Some(&id),
            Some(
                &serde_json::json!({
                    "branch": outcome.task_branch,
                    "into": outcome.target_branch,
                    "plan": plan_name,
                    "task": task_id,
                })
                .to_string(),
            ),
        );
        drop(db);

        return Json(serde_json::json!({
            "ok": true,
            "merged": outcome.task_branch,
            "into": outcome.target_branch,
        }))
        .into_response();
    }

    if outcome.had_conflict {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": outcome.error.unwrap_or_default(),
                "branch": outcome.task_branch,
                "target": outcome.target_branch,
            })),
        )
            .into_response();
    }

    let error = outcome.error.unwrap_or_default();
    let (status, json_body) = match error.as_str() {
        "Agent not found" => (StatusCode::NOT_FOUND, serde_json::json!({ "error": error })),
        "Agent has no task branch" => (
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "error": error }),
        ),
        "no_runner_connected" => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "error": error }),
        ),
        "runner_unavailable" => (
            StatusCode::GATEWAY_TIMEOUT,
            serde_json::json!({ "error": error }),
        ),
        s if s.starts_with("task branch has no commits") => (
            StatusCode::CONFLICT,
            serde_json::json!({
                "error": error,
                "branch": outcome.task_branch,
                "target": outcome.target_branch,
            }),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": error }),
        ),
    };
    (status, Json(json_body)).into_response()
}

pub async fn discard_agent_branch(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up agent details
    let (cwd, branch, org_id): (String, Option<String>, String) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, org_id FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
                )
                    .into_response();
            }
        }
    };

    let task_branch = match branch {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Agent has no task branch"})),
            )
                .into_response();
        }
    };

    // SaaS path: dispatch the discard via the runner RPC pair (T5.7).
    // Pre-T5.7 this branch returned 503
    // `discard_not_supported_for_saas_runners`; with the new
    // `WireMessage::DiscardBranch` / `BranchDiscarded` pair we hand the
    // cwd+target+task_branch to the runner, which runs the same
    // `git checkout && git branch -D` sequence the standalone branch
    // below performs locally. Server-side bookkeeping (DB clear, audit,
    // broadcast) still happens here on success.
    if crate::saas::dispatch::org_has_runner(&state.db, &org_id) {
        // Resolve the target branch via the runner-aware default-branch
        // helper so we hand a real branch name to the discard RPC. A
        // missing default falls back to `main` — same fallback
        // `resolve_merge_target(None, None, None)` would have produced
        // before T5.7.
        let cwd_path = std::path::Path::new(&cwd);
        let default = git_ops::default_branch(&state.db, &state.runners, &org_id, cwd_path)
            .await
            .unwrap_or_default();
        let target = resolve_merge_target(None, default.as_deref(), None);

        let req_id = uuid::Uuid::new_v4().to_string();
        let msg = crate::saas::runner_protocol::WireMessage::DiscardBranch {
            req_id: req_id.clone(),
            cwd: cwd.clone(),
            target: target.clone(),
            task_branch: task_branch.clone(),
        };
        match crate::saas::runner_rpc::runner_request(
            &state,
            &org_id,
            msg,
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(crate::saas::runner_ws::RunnerResponse::BranchDiscarded { ok: true, .. }) => {
                return discard_finalize(&state, auth, &id, &task_branch, &target).await;
            }
            Ok(crate::saas::runner_ws::RunnerResponse::BranchDiscarded { ok: false, error }) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": error.unwrap_or_else(|| "discard failed".into())
                    })),
                )
                    .into_response();
            }
            Ok(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "runner returned unexpected reply variant"
                    })),
                )
                    .into_response();
            }
            Err(RunnerRpcError::NoConnectedRunner) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "no_runner_connected"})),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({"error": format!("runner RPC: {e}")})),
                )
                    .into_response();
            }
        }
    }

    // Discard has no `into` body field today (future "discard and switch
    // to X" UI hooks here). Passing `None` falls through to the canonical
    // default branch.
    let cwd_path = std::path::Path::new(&cwd);
    let default = crate::agents::git_default_branch(cwd_path);
    let target = resolve_merge_target(None, default.as_deref(), None);

    // Checkout the target branch first
    let checkout = std::process::Command::new("git")
        .args(["checkout", &target])
        .current_dir(&cwd)
        .output();

    if let Ok(output) = checkout
        && !output.status.success()
    {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to checkout {target}: {stderr}")
            })),
        )
            .into_response();
    }

    // Force-delete the task branch
    let delete = std::process::Command::new("git")
        .args(["branch", "-D", &task_branch])
        .current_dir(&cwd)
        .output();

    match delete {
        Ok(output) if output.status.success() => {
            // Clear branch in DB for ALL agents with this branch
            {
                let db = state.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET branch = NULL WHERE branch = ?",
                    params![task_branch],
                )
                .ok();
                crate::audit::log(
                    &db,
                    auth.org_id(),
                    auth.0.as_ref().map(|u| u.id.as_str()),
                    auth.0.as_ref().map(|u| u.email.as_str()),
                    crate::audit::actions::BRANCH_DISCARD,
                    crate::audit::resources::AGENT,
                    Some(&id),
                    Some(&serde_json::json!({"branch": task_branch}).to_string()),
                );
            }

            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "agent_branch_discarded",
                serde_json::json!({
                    "id": id,
                    "deleted": task_branch,
                }),
            );

            Json(serde_json::json!({
                "ok": true,
                "deleted": task_branch,
                "switched_to": target,
            }))
            .into_response()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to delete branch: {stderr}")
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to run git: {e}")
            })),
        )
            .into_response(),
    }
}

/// Server-side bookkeeping after a successful discard (either path).
/// Clears `agents.branch` for every agent row that was on this branch,
/// writes the audit row, broadcasts `agent_branch_discarded`, and
/// returns the canonical 200 body. Factored out of
/// `discard_agent_branch` so the SaaS runner-RPC path doesn't have to
/// inline-duplicate it from the standalone block below.
async fn discard_finalize(
    state: &AppState,
    auth: OptionalAuthUser,
    agent_id: &str,
    task_branch: &str,
    target: &str,
) -> axum::response::Response {
    {
        let db = state.db.lock().unwrap();
        // Clear `merge_status` alongside `branch`: discarding the branch
        // means there's nothing left to merge, so a lingering
        // `merge_status='deferred_for_cadence'` would keep the row counted as
        // "awaiting cadence" with no branch to flush/merge/dismiss.
        db.execute(
            "UPDATE agents SET branch = NULL, merge_status = NULL WHERE branch = ?",
            params![task_branch],
        )
        .ok();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::BRANCH_DISCARD,
            crate::audit::resources::AGENT,
            Some(agent_id),
            Some(&serde_json::json!({"branch": task_branch}).to_string()),
        );
    }
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "agent_branch_discarded",
        serde_json::json!({
            "id": agent_id,
            "deleted": task_branch,
        }),
    );
    Json(serde_json::json!({
        "ok": true,
        "deleted": task_branch,
        "switched_to": target,
    }))
    .into_response()
}

/// Query parameters for `GET /api/drivers`. Optional `runner_id` selects
/// which runner's inventory to return in SaaS mode; absent means "the
/// most recently-seen runner in the org" (matches the dispatch
/// convention used by `runner_request`).
#[derive(Deserialize)]
pub struct ListDriversQuery {
    pub runner_id: Option<String>,
}

/// `GET /api/drivers` — list agent drivers + auth status.
///
/// In standalone deployments this returns the server's local registry —
/// the historical contract. In SaaS deployments (`org_has_runner` is
/// true) it returns the **runner's** driver inventory, because agents
/// run there and the dashboard host has no agent CLIs. The mode switch
/// is hidden in [`crate::saas::dispatch::list_drivers_dispatch`]. The
/// optional `?runner_id=<id>` query selects which runner to inspect when
/// the user has multiple; falls back to the most recently-seen runner.
///
/// Response shape (frozen for the dashboard):
/// `{ drivers: [{ name, binary, capabilities, auth_status }], default,
///    runner_id?, runner_status? }`. The two `runner_*` fields are
/// SaaS-only and let the dashboard render an offline / never-reported
/// chip when the latest data came from the DB rather than a live runner.
pub async fn list_drivers(
    State(state): State<AppState>,
    user: OptionalAuthUser,
    Query(q): Query<ListDriversQuery>,
) -> impl IntoResponse {
    let org_id = user
        .0
        .as_ref()
        .map(|u| u.org_id.as_str())
        .unwrap_or("default-org");
    let body =
        crate::saas::dispatch::list_drivers_dispatch(&state, org_id, q.runner_id.as_deref()).await;
    Json(body)
}

pub async fn get_events(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT * FROM hook_events ORDER BY id DESC LIMIT 50")
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, i64>(0)?,
                "session_id": row.get::<_, String>(1)?,
                "hook_type": row.get::<_, String>(2)?,
                "tool_name": row.get::<_, Option<String>>(3)?,
                "tool_input": row.get::<_, Option<String>>(4)?,
                "timestamp": row.get::<_, String>(5)?,
            }))
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

#[cfg(test)]
mod finish_version_gate_tests {
    use super::*;

    /// Pinned by Task 5.5. The exact constant matters for the wire
    /// contract: the 409 response body advertises this value as
    /// `minimumVersion`, and the dashboard's upgrade banner reads it.
    /// A future bump (e.g. once an Aider/Codex Finish path needs a
    /// newer minimum) MUST update tests/finish_agent_spawn_runner.rs
    /// at the same time.
    #[test]
    fn minimum_version_is_0_4_0() {
        assert_eq!(MIN_RUNNER_VERSION_FOR_FINISH, (0, 4, 0));
        assert_eq!(min_version_for_finish_str(), "0.4.0");
    }

    #[test]
    fn parses_simple_semver() {
        assert_eq!(parse_semver("0.5.70"), Some((0, 5, 70)));
        assert_eq!(parse_semver("1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_semver("0.4.0"), Some((0, 4, 0)));
    }

    #[test]
    fn parses_two_component_semver_as_patch_zero() {
        // The classifier permits "0.5" as a shorthand — same as
        // `runner_ws::parse_semver_lenient` so a Cargo.toml-style
        // `version = "0.5"` doesn't accidentally trigger too_old.
        assert_eq!(parse_semver("0.5"), Some((0, 5, 0)));
    }

    #[test]
    fn strips_pre_release_and_build_metadata() {
        // `0.5.70-rc1` and `0.5.70+sha1234` must compare as `0.5.70`,
        // mirroring the upstream version classifier so a runner built
        // from a tagged pre-release isn't refused on Finish.
        assert_eq!(parse_semver("0.5.70-rc1"), Some((0, 5, 70)));
        assert_eq!(parse_semver("0.5.70+sha1234"), Some((0, 5, 70)));
    }

    #[test]
    fn rejects_unparseable_strings() {
        assert_eq!(parse_semver("custom-build"), None);
        assert_eq!(parse_semver("v0.5.70"), None); // leading `v` not stripped
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn meets_min_version_when_above_minimum() {
        assert!(runner_meets_min_version_for_finish(Some("0.4.0")));
        assert!(runner_meets_min_version_for_finish(Some("0.4.1")));
        assert!(runner_meets_min_version_for_finish(Some("0.5.70")));
        assert!(runner_meets_min_version_for_finish(Some("1.0.0")));
    }

    #[test]
    fn does_not_meet_min_version_when_below_minimum() {
        // The headline regression: v0.3.0 (the prod runner that
        // surfaced the 2026-05-19 silent-200 bug) must NOT meet the
        // minimum so Finish refuses to dispatch.
        assert!(!runner_meets_min_version_for_finish(Some("0.3.0")));
        assert!(!runner_meets_min_version_for_finish(Some("0.3.99")));
        assert!(!runner_meets_min_version_for_finish(Some("0.2.0")));
    }

    #[test]
    fn does_not_meet_min_version_for_unparseable() {
        // Conservative: an unparseable version string is treated as
        // too old. Better a 409 + upgrade prompt than a silent 200
        // when we can't be sure the runner has the AgentInput handler.
        assert!(!runner_meets_min_version_for_finish(Some("custom-build")));
        assert!(!runner_meets_min_version_for_finish(Some("")));
        assert!(!runner_meets_min_version_for_finish(None));
    }
}
