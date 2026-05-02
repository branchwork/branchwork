use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::Deserialize;

use crate::auth::OptionalAuthUser;
use crate::state::AppState;

/// Decide the merge target. Pure — git probes happen in the caller
/// (the runner in SaaS, the local helper in standalone) and pass
/// results in. `explicit_into` is the dropdown selection from the
/// merge endpoint's `into` body field; `default_branch` is the
/// canonical-default-branch resolution (see
/// [`crate::agents::git_default_branch`]).
///
/// Priority: explicit (if it resolves) → default → "main".
///
/// The agent's stored `source_branch` is intentionally NOT consulted:
/// a stale auto-captured value (from whichever branch the user happened
/// to be on at spawn time) was the source of the wrong-target merge
/// bug. The escape hatch is the dropdown's `into` parameter, never the
/// auto-populated DB column.
fn resolve_merge_target(
    explicit_into: Option<&str>,
    default_branch: Option<&str>,
    cwd: &std::path::Path,
) -> String {
    let resolves = |name: &str| -> bool {
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", name])
            .current_dir(cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    if let Some(into) = explicit_into
        && resolves(into)
    {
        return into.to_string();
    }
    if let Some(d) = default_branch {
        return d.to_string();
    }
    "main".to_string()
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
                    source_branch, cost_usd, driver \
             FROM agents WHERE org_id = ?1 \
             ORDER BY started_at DESC LIMIT 50",
        )
        .unwrap();

    let rows: Vec<serde_json::Value> = stmt
        .query_map(params![org_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "session_id": row.get::<_, String>(1)?,
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
    if state.registry.kill_agent(&id).await {
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
        (StatusCode::OK, Json(serde_json::json!({"ok": true})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
    }
}

/// `POST /api/agents/:id/finish` — send the CLI's graceful-exit sequence
/// (e.g. `/exit` for Claude Code) so the agent shuts down cleanly. Unlike
/// Kill this preserves any in-flight commit/cleanup the agent wants to do.
pub async fn finish_agent(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
        (StatusCode::OK, Json(serde_json::json!({"ok": true})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "Agent not found or driver does not support graceful exit"
            })),
        )
    }
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
    let (cwd, task_branch): (String, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch FROM agents WHERE id = ?",
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

    let cwd_path = std::path::Path::new(&cwd);
    let default = crate::agents::git_default_branch(cwd_path);
    let mut branches = crate::agents::git_list_branches(cwd_path);

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

pub async fn merge_agent_branch(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
    body: Option<Json<MergeBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Look up agent details (need plan/task for CI bookkeeping too)
    let (cwd, branch, plan_name, task_id): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch, plan_name, task_id FROM agents WHERE id = ?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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

    let cwd_path = std::path::Path::new(&cwd);
    let default = crate::agents::git_default_branch(cwd_path);
    // Treat `Some("")` the same as `None` — an empty `into` means
    // "no override, use the canonical default". Defensive against a
    // future form-encoded path where empty strings sneak through; the
    // current JSON UI already coerces empty selections to `null`.
    let explicit = body.into.filter(|s| !s.is_empty());
    let target = resolve_merge_target(explicit.as_deref(), default.as_deref(), cwd_path);

    // Guard: refuse to merge a branch with no commits ahead of its source.
    // Agents that exit before committing leave a branch that points at the
    // same SHA as the source — merging it is a silent no-op that hides the
    // real problem (the agent did no work). Return 409 so the UI can tell
    // the user to retry or commit manually.
    //
    // If git can't resolve the range (stale branch reference, missing
    // trunk, detached HEAD, etc) we fall through permissively — the
    // actual `git merge` below will fail with its own clear error and
    // that's better than blocking the user on an inscrutable guard.
    let revlist = std::process::Command::new("git")
        .args(["rev-list", "--count", &format!("{target}..{task_branch}")])
        .current_dir(&cwd)
        .output();

    match revlist {
        Ok(output) if output.status.success() => {
            let count: u64 = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            if count == 0 {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "task branch has no commits — agent exited without committing",
                        "branch": task_branch,
                        "target": target,
                    })),
                )
                    .into_response();
            }
        }
        Ok(output) => {
            // `rev-list` failed — most likely one of the refs doesn't resolve
            // (deleted branch, typo, detached HEAD). Log and proceed; `git
            // merge` below will return the same or better error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "[merge] rev-list {target}..{task_branch} failed, skipping empty-branch guard: {stderr}"
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to run git rev-list: {e}")
                })),
            )
                .into_response();
        }
    }

    // Checkout the target branch
    let checkout = std::process::Command::new("git")
        .args(["checkout", &target])
        .current_dir(&cwd)
        .output();

    match checkout {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to checkout {target}: {stderr}")
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("Failed to run git: {e}")
                })),
            )
                .into_response();
        }
        _ => {}
    }

    // Merge the task branch
    let merge = std::process::Command::new("git")
        .args(["merge", &task_branch, "--no-edit"])
        .current_dir(&cwd)
        .output();

    match merge {
        Ok(output) if output.status.success() => {
            // Delete the task branch after successful merge
            std::process::Command::new("git")
                .args(["branch", "-d", &task_branch])
                .current_dir(&cwd)
                .output()
                .ok();

            // Capture merged SHA for CI tracking before we kick off side work
            let merged_sha = crate::agents::git_head_sha(std::path::Path::new(&cwd));

            // Clear branch in DB for ALL agents with this branch — siblings
            // (killed retries, check agents) shouldn't keep advertising it.
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
                    crate::audit::actions::BRANCH_MERGE,
                    crate::audit::resources::AGENT,
                    Some(&id),
                    Some(
                        &serde_json::json!({
                            "branch": task_branch,
                            "into": target,
                            "plan": plan_name,
                            "task": task_id,
                        })
                        .to_string(),
                    ),
                );
            }

            // Broadcast so connected dashboards update immediately
            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "agent_branch_merged",
                serde_json::json!({
                    "id": id,
                    "merged": task_branch,
                    "into": target,
                }),
            );

            // Kick off CI pipeline (push to origin, record pending run).
            // Only possible when we know which task this agent was for.
            if let (Some(plan), Some(task), Some(sha)) =
                (plan_name.clone(), task_id.clone(), merged_sha)
            {
                tokio::spawn(crate::ci::trigger_after_merge(crate::ci::TriggerArgs {
                    db: state.db.clone(),
                    broadcast_tx: state.broadcast_tx.clone(),
                    cwd: std::path::PathBuf::from(&cwd),
                    plan_name: plan,
                    task_number: task,
                    agent_id: id.clone(),
                    source_branch: target.clone(),
                    task_branch: task_branch.clone(),
                    merged_sha: sha,
                }));
            }

            Json(serde_json::json!({
                "ok": true,
                "merged": task_branch,
                "into": target,
            }))
            .into_response()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Abort the failed merge
            std::process::Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(&cwd)
                .output()
                .ok();
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Merge conflict: {stderr}"),
                    "branch": task_branch,
                    "target": target,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to run git merge: {e}")
            })),
        )
            .into_response(),
    }
}

pub async fn discard_agent_branch(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Look up agent details
    let (cwd, branch): (String, Option<String>) = {
        let db = state.db.lock().unwrap();
        match db.query_row(
            "SELECT cwd, branch FROM agents WHERE id = ?",
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

    // Discard has no `into` body field today (future "discard and switch
    // to X" UI hooks here). Passing `None` falls through to the canonical
    // default branch.
    let cwd_path = std::path::Path::new(&cwd);
    let default = crate::agents::git_default_branch(cwd_path);
    let target = resolve_merge_target(None, default.as_deref(), cwd_path);

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

/// `GET /api/drivers` — list agent drivers the server knows about.
///
/// Response: `{ drivers: [{ name, binary }], default: "claude" }`. The
/// first entry is always [`crate::agents::driver::DEFAULT_DRIVER`] so UIs
/// have a stable fallback to pre-select.
pub async fn list_drivers(State(state): State<AppState>) -> impl IntoResponse {
    let reg = &state.registry.drivers;
    let entries: Vec<serde_json::Value> = reg
        .names()
        .into_iter()
        .map(|name| {
            let driver = reg.get(&name);
            let binary = driver
                .as_ref()
                .map(|d| d.binary().to_string())
                .unwrap_or_default();
            let caps = driver
                .as_ref()
                .map(|d| d.capabilities())
                .unwrap_or_default();
            let auth = driver
                .as_ref()
                .map(|d| d.auth_status())
                .unwrap_or(crate::agents::driver::AuthStatus::Unknown);
            serde_json::json!({
                "name": name,
                "binary": binary,
                "capabilities": caps,
                "auth_status": auth,
            })
        })
        .collect();
    Json(serde_json::json!({
        "drivers": entries,
        "default": crate::agents::driver::DEFAULT_DRIVER,
    }))
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
