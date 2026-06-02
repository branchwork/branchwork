//! Per-project worktree disk-usage read API (Phase 6, Task 6.2 of
//! worktree-per-agent-isolation).
//!
//! `GET /api/orgs/:slug/worktree-disk-usage` reports `du`-measured size for
//! each project's worktree tree under the per-agent worktree base
//! (`<base>/<project-slug>/`). The dashboard sidebar renders one line per
//! project so an operator can see which projects accrete disk.
//!
//! Two contracts make a slow `du` invisible to the UI:
//!   1. **In-process cache, 60s TTL** keyed by org — repeated dashboard loads
//!      within a minute reuse the same result.
//!   2. **Stale-while-revalidate, background refresh** — the HTTP handler
//!      NEVER runs `du` inline. It returns whatever is cached (empty on a cold
//!      start) and, when the entry is stale/absent, spawns a background task to
//!      recompute via [`crate::agents::git_ops::disk_usage`]. The next load
//!      picks up the fresh value. An in-flight guard dedups concurrent
//!      refreshes for the same org.
//!
//! SaaS mode: the worktree base lives on the runner, so the dispatcher relays
//! `WireMessage::DiskUsage` and the runner shells out; standalone runs `du`
//! locally. Both go through `agents::git_ops::disk_usage`.
//!
//! Auth: member-level (NOT admin). Disk usage is read-only informational and
//! shown to every dashboard user — unlike the orphan-worktree reaper, which is
//! a destructive admin action.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::params;

use crate::auth::AuthUser;
use crate::saas::runner_protocol::ProjectDiskUsage;
use crate::state::AppState;

/// How long a computed snapshot is served before a background refresh fires.
const CACHE_TTL: Duration = Duration::from_secs(60);

struct CacheEntry {
    computed_at: Instant,
    projects: Vec<ProjectDiskUsage>,
}

/// Per-org disk-usage snapshots. Keyed by org_id so each org's runner (SaaS)
/// or base (standalone) caches independently.
fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-org in-flight guard so concurrent dashboard loads don't each spawn a
/// background `du` pass for the same org.
fn refreshing() -> &'static Mutex<HashSet<String>> {
    static REFRESHING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REFRESHING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Resolve org_id from slug and verify the caller is a member. Member-level
/// (not admin): disk usage is read-only and shown to every dashboard user.
/// `Err` boxes the `Response` (it is large) to dodge `clippy::result_large_err`
/// — same convention as `api::orphan_worktrees::resolve_org`.
fn resolve_member_org(
    state: &AppState,
    user: &AuthUser,
    slug: &str,
) -> Result<String, Box<Response>> {
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
    let is_member: bool = conn
        .query_row(
            "SELECT 1 FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if is_member {
        Ok(org_id)
    } else {
        Err(Box::new(err(StatusCode::FORBIDDEN, "not_a_member")))
    }
}

/// `GET /api/orgs/:slug/worktree-disk-usage`.
///
/// Returns `{ "projects": [{ slug, size_bytes, size_human }] }` from the
/// in-process cache, never blocking on `du`. Spawns a background refresh when
/// the cached entry is stale or missing (see module docs).
pub async fn worktree_disk_usage(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let org_id = match resolve_member_org(&state, &user, &slug) {
        Ok(id) => id,
        Err(r) => return *r,
    };

    // Snapshot the cached value + freshness without holding the lock across an
    // await. A cold cache reads as `(empty, stale)`.
    let (projects, fresh) = {
        let now = Instant::now();
        let guard = cache().lock().unwrap();
        match guard.get(&org_id) {
            Some(e) => (
                e.projects.clone(),
                now.duration_since(e.computed_at) < CACHE_TTL,
            ),
            None => (Vec::new(), false),
        }
    };

    // Stale / cold → kick off a background refresh (deduped per org). The
    // handler returns the (possibly empty / stale) snapshot immediately so a
    // slow `du` never blocks the dashboard.
    if !fresh {
        let should_spawn = refreshing().lock().unwrap().insert(org_id.clone());
        if should_spawn {
            spawn_refresh(state.clone(), org_id);
        }
    }

    Json(serde_json::json!({ "projects": projects })).into_response()
}

/// Recompute the cache entry for `org_id` in the background, then clear the
/// in-flight guard. Always runs to completion (the dispatcher never panics;
/// `unwrap_or_default` absorbs an RPC failure), so the guard is never leaked.
fn spawn_refresh(state: AppState, org_id: String) {
    tokio::spawn(async move {
        let projects = crate::agents::git_ops::disk_usage(&state.db, &state.runners, &org_id)
            .await
            .unwrap_or_default();
        cache().lock().unwrap().insert(
            org_id.clone(),
            CacheEntry {
                computed_at: Instant::now(),
                projects,
            },
        );
        refreshing().lock().unwrap().remove(&org_id);
    });
}
