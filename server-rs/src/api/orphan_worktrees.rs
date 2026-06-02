//! Orphan-worktree detection + read API (Phase 5, Task 5.1 of
//! worktree-per-agent-isolation).
//!
//! A per-agent worktree under `<base>/<slug>/` that no *live* agent
//! (`finished_at IS NULL`) claims is a leak — the normal cleanup on agent
//! completion / kill (Tasks 2.3 / 2.4) did not run, almost always because
//! the server was `SIGKILL`'d (crash, OOM, operator kill) before it could
//! remove the worktree. This module reaps those leaks two ways:
//!
//! 1. [`run_startup_sweep`] — runs once at boot (after
//!    `cleanup_and_reattach` has flipped dead agents to terminal, so their
//!    `finished_at` is set and they no longer count as live). For each
//!    project that has agent rows it asks the mode-aware worktree manager
//!    which worktrees no live agent claims and records each into the
//!    `worktree_orphans` table (idempotent on the `path` PK, so a
//!    re-detected orphan keeps its original `first_detected`).
//! 2. [`list_orphan_worktrees`] — `GET /api/orgs/:slug/orphan-worktrees`,
//!    admin-only, returns the recorded set so an operator can inspect /
//!    reclaim leaked disk.
//!
//! The `worktree_orphans` table carries no `org_id` (orphan worktrees are a
//! deployment-host concern, not org-partitioned data), so the endpoint
//! gates on an admin role for the named org but returns the full set.

use std::collections::HashSet;
use std::path::PathBuf;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::params;

use crate::auth::AuthUser;
use crate::auth::orgs::{ROLE_ADMIN, ROLE_OWNER};
use crate::state::AppState;

// ── auth helpers (mirror `api::billing`) ─────────────────────────────────

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Resolve org_id from slug and verify the caller is a member. Returns
/// `(org_id, caller_role)` on success. Byte-for-byte the gate the billing
/// endpoints use, so org-scoped auth stays consistent across the API.
fn resolve_org(
    state: &AppState,
    user: &AuthUser,
    slug: &str,
) -> Result<(String, String), Box<Response>> {
    let conn = state.db.lock().unwrap();
    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .ok();
    let org_id = match org_id {
        Some(id) => id,
        None => return Err(Box::new(err(StatusCode::NOT_FOUND, "org_not_found"))),
    };
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |row| row.get(0),
        )
        .ok();
    match role {
        Some(r) => Ok((org_id, r)),
        None => Err(Box::new(err(StatusCode::FORBIDDEN, "not_a_member"))),
    }
}

fn require_admin(role: &str) -> Result<(), Box<Response>> {
    if role == ROLE_OWNER || role == ROLE_ADMIN {
        Ok(())
    } else {
        Err(Box::new(err(StatusCode::FORBIDDEN, "admin_required")))
    }
}

// ── GET /api/orgs/:slug/orphan-worktrees ─────────────────────────────────

/// Admin-only list of leaked worktrees recorded by the boot sweep.
pub async fn list_orphan_worktrees(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let (_org_id, role) = match resolve_org(&state, &user, &slug) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(r) = require_admin(&role) {
        return *r;
    }
    let orphans = {
        let conn = state.db.lock().unwrap();
        crate::db::list_recorded_orphans(&conn)
    };
    Json(serde_json::json!({ "orphans": orphans })).into_response()
}

// ── server-boot sweep ────────────────────────────────────────────────────

/// Spawn the orphan sweep as a detached task so it never blocks the HTTP
/// listener readiness probe. Wired into `main::run_server` after
/// `cleanup_and_reattach` has settled the agent rows.
pub fn spawn_startup_sweep(state: AppState) {
    tokio::spawn(run_startup_sweep(state));
}

/// One-shot startup sweep. See module docs for the full contract.
pub async fn run_startup_sweep(state: AppState) {
    // Distinct (cwd, org_id) anchors. We need a still-existing git working
    // tree to derive each project's stable main-worktree root; both live
    // and finished agents contribute a candidate cwd (a leaked worktree's
    // own — now finished — agent row is often the only anchor pointing at
    // it).
    let anchors: Vec<(String, String)> = {
        let conn = state.db.lock().unwrap();
        let mut stmt =
            match conn.prepare("SELECT DISTINCT cwd, org_id FROM agents WHERE cwd IS NOT NULL") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[orphan-sweep] agents query failed: {e}");
                    return;
                }
            };
        let rows = stmt.query_map([], |row| {
            let cwd: String = row.get(0)?;
            let org_id: Option<String> = row.get(1)?;
            Ok((cwd, org_id.unwrap_or_else(|| "default-org".to_string())))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => return,
        }
    };
    if anchors.is_empty() {
        return;
    }

    // Live cwds: agents that have NOT finished. Their worktrees must never
    // be reaped. A global set is correct — `list_orphans` scopes by slug,
    // and a live agent in any project should never be reported.
    let live_cwds: HashSet<PathBuf> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT cwd FROM agents WHERE finished_at IS NULL AND cwd IS NOT NULL")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        match rows {
            Ok(it) => it.flatten().map(PathBuf::from).collect(),
            Err(_) => return,
        }
    };

    // Resolve distinct (project_dir, org_id) from the anchors. The
    // standalone path derives the root via a local `git worktree list`
    // shell-out; in SaaS mode the cwd lives on the runner host, so the
    // shell-out returns `None` and the project is skipped — orphan reaping
    // for SaaS needs the dedicated `agents.project_dir` column the task
    // defers. The dispatcher call below is wired and ready for that.
    let mut projects: HashSet<(PathBuf, String)> = HashSet::new();
    for (cwd, org_id) in anchors {
        let cwd = PathBuf::from(cwd);
        if let Some(project_dir) = crate::agents::main_worktree_root(&cwd) {
            projects.insert((project_dir, org_id));
        }
    }

    for (project_dir, org_id) in projects {
        let project_slug = crate::agents::pty_agent::project_slug_for_worktree(&project_dir);
        let orphans = match crate::agents::git_ops::list_worktree_orphans(
            &state.db,
            &state.runners,
            &org_id,
            &project_dir,
            &project_slug,
            &live_cwds,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!(
                    "[orphan-sweep] list_orphans failed for {} (org {org_id}): {e}",
                    project_dir.display()
                );
                continue;
            }
        };
        if orphans.is_empty() {
            continue;
        }

        let mut new_count = 0u32;
        {
            let conn = state.db.lock().unwrap();
            for o in &orphans {
                let last_modified_secs = o
                    .last_modified
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let path_str = o.path.to_string_lossy();
                if crate::db::record_worktree_orphan(
                    &conn,
                    &project_slug,
                    &path_str,
                    o.branch.as_deref(),
                    last_modified_secs,
                ) {
                    new_count += 1;
                }
            }
        }
        eprintln!(
            "[orphan-sweep] {project_slug}: {} orphan(s) under {} ({new_count} newly recorded)",
            orphans.len(),
            project_dir.display()
        );
    }
}
