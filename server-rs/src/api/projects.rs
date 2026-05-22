//! Server-driven project records (Phase 2.1 of `runner-daemon-workspace`).
//!
//! A *project* is a host-side git repository the operator has decided to
//! work on. It pairs an operator-friendly name with a clone URL, a host
//! enum (so the future "create new remote repo" path in Phase 2.3 knows
//! which API to call), and a `workspace_path` — the absolute directory
//! the runner clones into. The default is `$HOME/<name>`; operators can
//! override per-project for the unusual case where `$HOME` is the wrong
//! parent.
//!
//! The audit verdict that landed in the plan brief (2026-05-18) decided
//! projects stay in `$HOME` and NOT in a dedicated workspace dir, so the
//! resolution lives here as a one-liner rather than a dedicated module.
//!
//! ## Endpoints
//!
//! - `POST /api/projects` — create a project row. Body: `{ name, repo_url,
//!   host?, owner?, default_credential_id?, workspace_path? }`. Returns
//!   201 with the resolved row, including a `workspace_path` defaulted to
//!   `$HOME/<name>` when the caller didn't override.
//! - `GET /api/projects` — list the caller's org's projects, ordered by
//!   most-recently-created.
//! - `DELETE /api/projects/{id}` — remove the project row. Optional query
//!   flag `?wipe_on_disk=true` ALSO removes `workspace_path` on disk;
//!   default is row-only. (Phase 2.2 + 2.3 will dispatch the actual on-
//!   disk wipe to the runner. Today the helper runs locally — fine for
//!   standalone-mode deploys; SaaS-mode falls back to "delete row only +
//!   tell the operator to clean up" via `wipeFailed: true` in the
//!   response.)
//!
//! ## Forward-looking notes
//!
//! - `default_credential_id` is currently `Option<String>` with no FK — the
//!   `credentials` table arrives in Phase 3.1.
//! - `project_id` columns landed on `agents` and `plan_project` in the
//!   db.rs migration here; today they're NULL on every existing row and
//!   the dashboard does not yet read them. Phase 2.4 (the "New Project"
//!   modal) will start populating them.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit;
use crate::auth::OptionalAuthUser;
use crate::state::AppState;

/// Host enum. Stored on disk as a string; the SQLite column has no CHECK
/// constraint so future hosts can be added without a migration. Phase 2.3
/// matches on this to dispatch to the right host API for "create new remote
/// repo" mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProjectHost {
    Github,
    Gitlab,
    Bitbucket,
    Other,
}

impl ProjectHost {
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectHost::Github => "github",
            ProjectHost::Gitlab => "gitlab",
            ProjectHost::Bitbucket => "bitbucket",
            ProjectHost::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(ProjectHost::Github),
            "gitlab" => Some(ProjectHost::Gitlab),
            "bitbucket" => Some(ProjectHost::Bitbucket),
            "other" => Some(ProjectHost::Other),
            _ => None,
        }
    }
}

/// Wire shape for a project. Field names are camelCase via serde rename.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub repo_url: String,
    pub host: String,
    pub owner: Option<String>,
    pub default_credential_id: Option<String>,
    pub workspace_path: String,
    pub org_id: String,
    pub created_at: String,
}

/// Body for `POST /api/projects`. All fields except `name` + `repo_url` are
/// optional. `host` defaults to `other` (operator can edit later); `owner`
/// is host-side context that the UI parses from `repo_url` and submits but
/// is not required by this endpoint. `workspace_path` defaults to
/// `$HOME/<name>` when absent.
#[derive(Debug, Deserialize)]
pub struct CreateProjectBody {
    pub name: String,
    pub repo_url: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, rename = "credentialId", alias = "default_credential_id")]
    pub default_credential_id: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
}

/// Query string for `DELETE /api/projects/{id}`.
#[derive(Debug, Deserialize, Default)]
pub struct DeleteProjectQuery {
    /// When `true`, ALSO remove `workspace_path` on disk. Default is
    /// row-only.
    #[serde(default)]
    pub wipe_on_disk: bool,
}

/// `POST /api/projects` — create a project row.
///
/// Validation:
/// - `name` must be non-empty, ≤64 chars, and slug-shaped
///   (`[A-Za-z0-9._-]+`). The default `workspace_path` interpolates
///   `name` directly into a filesystem path, so we refuse `/`, `..`, and
///   other separators here rather than have a path-injection surface.
/// - `repo_url` must be non-empty.
/// - `host`, when present, must be one of the [`ProjectHost`] variants.
/// - `(org_id, name)` is UNIQUE; collisions return 409.
pub async fn create_project(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Json(body): Json<CreateProjectBody>,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();

    // ── name validation ─────────────────────────────────────────────────
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name_required" })),
        )
            .into_response();
    }
    if !is_valid_project_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_name",
                "message": "name must be 1-64 chars, [A-Za-z0-9._-], with no leading dot or path separator",
            })),
        )
            .into_response();
    }

    // ── repo_url validation ─────────────────────────────────────────────
    let repo_url = body.repo_url.trim().to_string();
    if repo_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "repo_url_required" })),
        )
            .into_response();
    }

    // ── host validation ─────────────────────────────────────────────────
    let host = match body.host.as_deref() {
        None => ProjectHost::Other,
        Some(s) => match ProjectHost::parse(s) {
            Some(h) => h,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_host",
                        "message": "host must be one of: github, gitlab, bitbucket, other",
                    })),
                )
                    .into_response();
            }
        },
    };

    // ── workspace_path resolution ───────────────────────────────────────
    let workspace_path = resolve_workspace_path(body.workspace_path.as_deref(), &name);

    // ── insert ──────────────────────────────────────────────────────────
    let id = Uuid::new_v4().to_string();
    let row = ProjectRow {
        id: id.clone(),
        name: name.clone(),
        repo_url: repo_url.clone(),
        host: host.as_str().to_string(),
        owner: body.owner.clone(),
        default_credential_id: body.default_credential_id.clone(),
        workspace_path: workspace_path.clone(),
        org_id: org_id.clone(),
        created_at: String::new(), // backfilled by SQLite default
    };

    let insert_result = {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO projects \
                 (id, name, repo_url, host, owner, default_credential_id, workspace_path, org_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id,
                row.name,
                row.repo_url,
                row.host,
                row.owner,
                row.default_credential_id,
                row.workspace_path,
                row.org_id,
            ],
        )
    };

    match insert_result {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "name_taken",
                    "message": format!("a project named '{name}' already exists in this org"),
                })),
            )
                .into_response();
        }
        Err(e) => {
            eprintln!("[projects] insert failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "insert_failed" })),
            )
                .into_response();
        }
    }

    // Read back created_at so the response carries the authoritative
    // timestamp the DB assigned.
    let created_at: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT created_at FROM projects WHERE id = ?1",
            params![row.id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };

    let mut row = row;
    row.created_at = created_at;

    // ── audit ───────────────────────────────────────────────────────────
    let user = auth.0.as_ref();
    let diff = serde_json::json!({
        "project_id": row.id,
        "name": row.name,
        "repo_url": row.repo_url,
        "host": row.host,
        "workspace_path": row.workspace_path,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            user.map(|u| u.id.as_str()),
            user.map(|u| u.email.as_str()),
            audit::actions::PROJECT_CREATE,
            audit::resources::PROJECT,
            Some(&row.id),
            Some(&diff),
        );
    }

    (StatusCode::CREATED, Json(row)).into_response()
}

/// `GET /api/projects` — list projects for the caller's org, newest first.
pub async fn list_projects(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let rows: Vec<ProjectRow> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, name, repo_url, host, owner, default_credential_id, \
                    workspace_path, org_id, created_at \
               FROM projects \
              WHERE org_id = ?1 \
              ORDER BY created_at DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[projects] list prepare failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "list_failed" })),
                )
                    .into_response();
            }
        };
        match stmt.query_map(params![org_id], |r| {
            Ok(ProjectRow {
                id: r.get(0)?,
                name: r.get(1)?,
                repo_url: r.get(2)?,
                host: r.get(3)?,
                owner: r.get(4)?,
                default_credential_id: r.get(5)?,
                workspace_path: r.get(6)?,
                org_id: r.get(7)?,
                created_at: r.get(8)?,
            })
        }) {
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                eprintln!("[projects] list query failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "list_failed" })),
                )
                    .into_response();
            }
        }
    };

    Json(serde_json::json!({ "projects": rows })).into_response()
}

/// `DELETE /api/projects/{id}` — remove the project row.
///
/// Optional `?wipe_on_disk=true` flag ALSO removes `workspace_path` on
/// disk (best-effort: failure is reported in the response body but does
/// not fail the DB delete). Default is row-only.
pub async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteProjectQuery>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();

    // Look up the row first so we can audit + (optionally) wipe the path
    // even when the DELETE succeeds.
    let existing: Option<(String, String, String)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT name, repo_url, workspace_path FROM projects \
              WHERE id = ?1 AND org_id = ?2",
            params![id, org_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .ok()
    };

    let Some((name, repo_url, workspace_path)) = existing else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project_not_found" })),
        )
            .into_response();
    };

    let deleted = {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "DELETE FROM projects WHERE id = ?1 AND org_id = ?2",
            params![id, org_id],
        )
        .unwrap_or(0)
    };

    if deleted == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project_not_found" })),
        )
            .into_response();
    }

    let mut wipe_attempted = false;
    let mut wipe_failed: Option<String> = None;
    if q.wipe_on_disk {
        wipe_attempted = true;
        // Best-effort wipe. We intentionally do NOT propagate the error to
        // the DB delete: the row is gone either way, and the response
        // surfaces `wipeFailed` so the operator can clean up by hand if
        // needed. In SaaS mode the runner owns the filesystem — Phase 2.x
        // will dispatch this through the runner; today the helper runs
        // locally, which is the right behavior for standalone deploys.
        if !workspace_path.is_empty() {
            let path = std::path::PathBuf::from(&workspace_path);
            if path.exists()
                && let Err(e) = std::fs::remove_dir_all(&path)
            {
                wipe_failed = Some(format!("{e}"));
            }
        }
    }

    // ── audit ───────────────────────────────────────────────────────────
    let user = auth.0.as_ref();
    let diff = serde_json::json!({
        "project_id": id,
        "name": name,
        "repo_url": repo_url,
        "workspace_path": workspace_path,
        "wipe_on_disk": q.wipe_on_disk,
        "wipe_failed": wipe_failed,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            user.map(|u| u.id.as_str()),
            user.map(|u| u.email.as_str()),
            audit::actions::PROJECT_DELETE,
            audit::resources::PROJECT,
            Some(&id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "deleted": true,
        "id": id,
        "wipeAttempted": wipe_attempted,
        "wipeFailed": wipe_failed,
    }))
    .into_response()
}

/// Resolve `workspace_path` for a new project.
///
/// - Explicit `workspace_path` from the body wins (whatever the caller
///   passed, trimmed). Empty string is treated as absent.
/// - Otherwise default to `$HOME/<name>`. `dirs::home_dir()` is the same
///   helper the rest of the server uses for resolving `$HOME` — on
///   Windows it reads `SHGetKnownFolderPath(FOLDERID_Profile)` and
///   ignores the `HOME` env var (same gotcha that affects
///   `tests/folders.rs`).
fn resolve_workspace_path(override_path: Option<&str>, name: &str) -> String {
    if let Some(p) = override_path
        && !p.trim().is_empty()
    {
        return p.trim().to_string();
    }
    let home = dirs::home_dir().unwrap_or_default();
    home.join(name).to_string_lossy().to_string()
}

/// `POST /api/projects/{id}/clone` — clone the project's `repo_url` into
/// `workspace_path` via the runner (SaaS mode) or locally (standalone).
///
/// On success the runner-resolved absolute path is written back to
/// `projects.workspace_path` so callers reading the row see the canonical
/// location (which may differ from the originally-requested path because
/// the runner resolves `~`-prefixes and bare names against `$HOME`).
///
/// Status codes:
/// - 200: clone succeeded; body includes the resolved path.
/// - 404: project not found in the caller's org.
/// - 409: a clone already happened (workspace_path already exists in the
///   filesystem) — the operator must `DELETE …?wipe_on_disk=true` first
///   if they want to re-clone.
/// - 500: dispatcher returned `RpcFailed` (no runner connected, runner
///   timed out, etc.) OR `CloneFailed` (git exited non-zero).
pub async fn clone_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();

    // Fetch the project row scoped to the caller's org. Cross-org lookups
    // return 404 (same shape as delete_project) so a leak doesn't expose
    // project existence to other tenants.
    let existing: Option<(String, String, String, Option<String>)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT name, repo_url, workspace_path, default_credential_id \
               FROM projects WHERE id = ?1 AND org_id = ?2",
            params![id, org_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .ok()
    };

    let Some((name, repo_url, workspace_path, credential_id)) = existing else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "project_not_found" })),
        )
            .into_response();
    };

    let outcome = crate::saas::dispatch::clone_project_dispatch(
        &state,
        &org_id,
        &repo_url,
        &workspace_path,
        credential_id.as_deref(),
    )
    .await;

    let (status, resolved_path, error) = match &outcome {
        crate::saas::dispatch::CloneDispatchOutcome::Ok { resolved_path } => {
            // Persist the runner-resolved path so future reads of the
            // project row see the canonical location (which may differ
            // from the originally-requested `workspace_path` after `~`
            // expansion / bare-name resolution).
            let conn = state.db.lock().unwrap();
            let _ = conn.execute(
                "UPDATE projects SET workspace_path = ?1 \
                  WHERE id = ?2 AND org_id = ?3",
                params![resolved_path, id, org_id],
            );
            ("ok".to_string(), Some(resolved_path.clone()), None)
        }
        crate::saas::dispatch::CloneDispatchOutcome::CloneFailed { error } => {
            ("clone_failed".to_string(), None, Some(error.clone()))
        }
        crate::saas::dispatch::CloneDispatchOutcome::RpcFailed { error } => {
            ("rpc_failed".to_string(), None, Some(error.clone()))
        }
    };

    // ── audit ───────────────────────────────────────────────────────────
    let user = auth.0.as_ref();
    let diff = serde_json::json!({
        "project_id": id,
        "name": name,
        "repo_url": repo_url,
        "workspace_path": workspace_path,
        "resolved_path": resolved_path,
        "outcome": status,
        "error": error,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            user.map(|u| u.id.as_str()),
            user.map(|u| u.email.as_str()),
            audit::actions::PROJECT_CLONE,
            audit::resources::PROJECT,
            Some(&id),
            Some(&diff),
        );
    }

    match outcome {
        crate::saas::dispatch::CloneDispatchOutcome::Ok { resolved_path } => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "id": id,
                "resolvedPath": resolved_path,
            })),
        )
            .into_response(),
        crate::saas::dispatch::CloneDispatchOutcome::CloneFailed { error } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "clone_failed",
                "message": error,
            })),
        )
            .into_response(),
        crate::saas::dispatch::CloneDispatchOutcome::RpcFailed { error } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "runner_rpc_failed",
                "message": error,
            })),
        )
            .into_response(),
    }
}

/// Slug check for project names.
///
/// Accepts `[A-Za-z0-9._-]`, 1..=64 chars, with no leading dot and no
/// path separators. Tight enough to safely interpolate into the default
/// `workspace_path` without escaping.
fn is_valid_project_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    if name.starts_with('.') {
        return false;
    }
    if name == ".." {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_project_name_accepts_typical_slugs() {
        assert!(is_valid_project_name("my-project"));
        assert!(is_valid_project_name("foo_bar"));
        assert!(is_valid_project_name("a"));
        assert!(is_valid_project_name("project.v2"));
        assert!(is_valid_project_name("UPPER-case"));
    }

    #[test]
    fn is_valid_project_name_rejects_path_traversal_and_dotfiles() {
        assert!(!is_valid_project_name(""));
        assert!(!is_valid_project_name(".hidden"));
        assert!(!is_valid_project_name(".."));
        assert!(!is_valid_project_name("../escape"));
        assert!(!is_valid_project_name("foo/bar"));
        assert!(!is_valid_project_name("foo\\bar"));
        assert!(!is_valid_project_name("with space"));
    }

    #[test]
    fn is_valid_project_name_caps_at_64_chars() {
        assert!(is_valid_project_name(&"a".repeat(64)));
        assert!(!is_valid_project_name(&"a".repeat(65)));
    }

    #[test]
    fn resolve_workspace_path_uses_explicit_override() {
        assert_eq!(
            resolve_workspace_path(Some("/srv/projects/x"), "my-project"),
            "/srv/projects/x"
        );
    }

    #[test]
    fn resolve_workspace_path_trims_override() {
        assert_eq!(
            resolve_workspace_path(Some("  /srv/x  "), "my-project"),
            "/srv/x"
        );
    }

    #[test]
    fn resolve_workspace_path_empty_override_falls_through_to_home() {
        let resolved = resolve_workspace_path(Some("   "), "my-project");
        // Resolved must end with `my-project` (under whatever home_dir
        // resolves to on this host).
        assert!(
            resolved.ends_with("my-project"),
            "expected '{resolved}' to end with 'my-project'"
        );
    }

    #[test]
    fn resolve_workspace_path_defaults_to_home_when_absent() {
        let resolved = resolve_workspace_path(None, "my-project");
        let home = dirs::home_dir().unwrap_or_default();
        let expected = home.join("my-project").to_string_lossy().to_string();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn project_host_round_trip() {
        for host in [
            ProjectHost::Github,
            ProjectHost::Gitlab,
            ProjectHost::Bitbucket,
            ProjectHost::Other,
        ] {
            let s = host.as_str();
            assert_eq!(ProjectHost::parse(s), Some(host));
        }
        assert_eq!(ProjectHost::parse("unknown"), None);
        assert_eq!(ProjectHost::parse(""), None);
    }
}
