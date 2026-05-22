//! Server-driven project records (Phase 2.1 + 2.3 of
//! `runner-daemon-workspace`).
//!
//! A *project* is a host-side git repository the operator has decided to
//! work on. It pairs an operator-friendly name with a clone URL, a host
//! enum (so the "create new remote repo" path in Phase 2.3 knows which
//! API to call), and a `workspace_path` — the absolute directory the
//! runner clones into. The default is `$HOME/<name>`; operators can
//! override per-project for the unusual case where `$HOME` is the wrong
//! parent.
//!
//! The audit verdict that landed in the plan brief (2026-05-18) decided
//! projects stay in `$HOME` and NOT in a dedicated workspace dir, so the
//! resolution lives here as a one-liner rather than a dedicated module.
//!
//! ## Endpoints
//!
//! `POST /api/projects` — create a project row. Body shape depends on
//! `mode`:
//!
//! - `mode: "clone"` (default): `{ name, repo_url, host?, owner?,
//!   credentialId?, workspace_path? }`. The caller already knows the
//!   remote URL; we just record the row.
//! - `mode: "create"`: `{ name, host, private?, owner?, credentialId,
//!   workspace_path? }`. We resolve the credential, dispatch to the
//!   host API (GitHub `POST /user/repos` or `POST /orgs/{owner}/repos`;
//!   GitLab `POST /projects`), and on success record the project row
//!   with the host-reported `clone_url`. On host-API failure NO
//!   project row is created; the response is a structured `{ code,
//!   message, host_response_excerpt }` 4xx/5xx so the dashboard can
//!   surface the upstream error verbatim.
//!
//! Either mode returns 201 with the resolved row, including a
//! `workspace_path` defaulted to `$HOME/<name>` when the caller didn't
//! override. In `create` mode the clone RPC fires asynchronously
//! (fire-and-forget `tokio::spawn`) so the operator can move on while
//! the runner pulls the (still-empty) repo.
//!
//! `GET /api/projects` — list the caller's org's projects, ordered by
//! most-recently-created.
//!
//! `DELETE /api/projects/{id}` — remove the project row. Optional query
//! flag `?wipe_on_disk=true` ALSO removes `workspace_path` on disk;
//! default is row-only. (Phase 2.2 + 2.3 will dispatch the actual
//! on-disk wipe to the runner. Today the helper runs locally — fine
//! for standalone-mode deploys; SaaS-mode falls back to "delete row
//! only + tell the operator to clean up" via `wipeFailed: true` in
//! the response.)
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

/// Body for `POST /api/projects`.
///
/// `mode` (default `"clone"`) selects which sub-path the handler runs:
/// - `clone`: `repo_url` is required; we just record the row.
/// - `create`: `host` + `credentialId` are required; we dispatch to the
///   host API to create the remote, then record the row with the
///   host-reported `clone_url`. `repo_url`, when present, is ignored —
///   the host API is authoritative.
///
/// `host` defaults to `other` for clone mode (operator can edit later).
/// `owner` is host-side context: for `create` mode it picks `POST
/// /orgs/{owner}/repos` (GitHub) or resolves a GitLab namespace; for
/// `clone` mode it's parsed by the UI from `repo_url` and round-trips
/// through the row. `workspace_path` defaults to `$HOME/<name>` when
/// absent. `private` only takes effect for `create` mode (defaults to
/// `true` so the remote is private unless the operator opted into
/// public).
#[derive(Debug, Default, Deserialize)]
pub struct CreateProjectBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, rename = "credentialId", alias = "default_credential_id")]
    pub default_credential_id: Option<String>,
    #[serde(default)]
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub private: Option<bool>,
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
/// Dispatches on `mode` (default `"clone"`):
/// - `clone`: just records the row from the supplied `repo_url`.
/// - `create`: dispatches to the host API (GitHub `POST /user/repos` or
///   `POST /orgs/{owner}/repos`; GitLab `POST /projects`) to create the
///   remote, then records the row with the host-reported `clone_url`.
///
/// Shared validation:
/// - `name` must be non-empty, ≤64 chars, and slug-shaped
///   (`[A-Za-z0-9._-]+`). The default `workspace_path` interpolates
///   `name` directly into a filesystem path, so we refuse `/`, `..`,
///   and other separators here rather than have a path-injection
///   surface.
/// - `host`, when present, must be one of the [`ProjectHost`] variants.
/// - `(org_id, name)` is UNIQUE; collisions return 409.
///
/// Per-mode error shapes diverge: `clone` mode uses the existing
/// `{ error, message? }` shape (`name_required`, `repo_url_required`,
/// `invalid_host`, `name_taken`). `create` mode adds
/// `{ code, message, host_response_excerpt }` for host-API rejections
/// so the dashboard can surface the upstream error verbatim.
pub async fn create_project(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Json(body): Json<CreateProjectBody>,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();

    // ── name validation (shared) ────────────────────────────────────────
    let name = body.name.as_deref().unwrap_or("").trim().to_string();
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

    // ── mode dispatch ───────────────────────────────────────────────────
    let mode = body.mode.as_deref().unwrap_or("clone").trim();
    match mode {
        "clone" => create_project_clone_mode(state, auth, body, name, org_id).await,
        "create" => create_project_create_mode(state, auth, body, name, org_id).await,
        other => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_mode",
                "message": format!("mode must be 'clone' or 'create', got '{other}'"),
            })),
        )
            .into_response(),
    }
}

/// Existing "record the row from the supplied `repo_url`" path. No host
/// API dispatch — the operator already knows the remote URL.
async fn create_project_clone_mode(
    state: AppState,
    auth: OptionalAuthUser,
    body: CreateProjectBody,
    name: String,
    org_id: String,
) -> axum::response::Response {
    // ── repo_url validation ─────────────────────────────────────────────
    let repo_url = body.repo_url.as_deref().unwrap_or("").trim().to_string();
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

    let workspace_path = resolve_workspace_path(body.workspace_path.as_deref(), &name);

    insert_project_row(
        &state,
        &auth,
        InsertProjectArgs {
            name,
            repo_url,
            host,
            owner: body.owner.clone(),
            default_credential_id: body.default_credential_id.clone(),
            workspace_path,
            org_id,
        },
        /* fire_clone_after */ false,
    )
    .await
}

/// New "create the remote first, then record the row" path. Resolves
/// the credential, dispatches to the host API, and only records the
/// row on host-API success. On failure NO row is created and the
/// response is a structured `{ code, message, host_response_excerpt }`.
async fn create_project_create_mode(
    state: AppState,
    auth: OptionalAuthUser,
    body: CreateProjectBody,
    name: String,
    org_id: String,
) -> axum::response::Response {
    // ── host validation ─────────────────────────────────────────────────
    let Some(host_str) = body.host.as_deref().map(str::trim) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "host_required",
                "message": "create mode requires host: 'github' or 'gitlab'",
            })),
        )
            .into_response();
    };
    let host = match ProjectHost::parse(host_str) {
        Some(h @ ProjectHost::Github) | Some(h @ ProjectHost::Gitlab) => h,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "unsupported_host_for_create",
                    "message": "create mode supports only host: github or gitlab",
                })),
            )
                .into_response();
        }
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
    };

    // ── credential required ─────────────────────────────────────────────
    let Some(credential_id) = body
        .default_credential_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "credential_required",
                "message": "create mode requires credentialId to authenticate against the host API",
            })),
        )
            .into_response();
    };

    // ── owner validation (host-side names are ASCII slug-ish) ───────────
    let owner = body
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    if let Some(ref o) = owner
        && !is_valid_host_owner(o)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_owner",
                "message": "owner must be [A-Za-z0-9._-] only",
            })),
        )
            .into_response();
    }

    // ── load credential (decrypted PAT) ─────────────────────────────────
    let master_key = match load_master_key(&state) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[projects] master key load failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "master_key_unavailable",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };
    let cred = match crate::db::get_credential(&state.db, &master_key, credential_id) {
        Ok(c) => c,
        Err(crate::db::CredentialError::NotFound) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "credential_not_found",
                    "message": "credentialId did not resolve to an active credential row",
                })),
            )
                .into_response();
        }
        Err(e) => {
            eprintln!("[projects] credential load failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "credential_unavailable",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    // Owner-scope check: when an authenticated user is on the wire, the
    // credential MUST belong to them. Anonymous standalone deploys
    // (where `auth.0` is None) skip the check — there is no concept of
    // user-owned credentials in that mode.
    if let Some(u) = auth.0.as_ref()
        && cred.owner_user_id != u.id
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "credential_not_owned",
                "message": "credentialId belongs to a different user",
            })),
        )
            .into_response();
    }

    // Kind-shape check: GitHub needs a PAT (gh_pat), GitLab needs a
    // PAT (gitlab_pat). SSH-key credentials are not usable for the
    // host-API create flow.
    let expected_kind = match host {
        ProjectHost::Github => crate::db::CredentialKind::GhPat,
        ProjectHost::Gitlab => crate::db::CredentialKind::GitlabPat,
        _ => unreachable!("filtered above"),
    };
    if cred.kind != expected_kind {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "credential_kind_mismatch",
                "message": format!(
                    "host '{}' requires a credential of kind '{}', got '{}'",
                    host.as_str(),
                    expected_kind.as_str(),
                    cred.kind.as_str(),
                ),
            })),
        )
            .into_response();
    }

    let pat = String::from_utf8(cred.secret_plaintext).unwrap_or_default();
    if pat.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "credential_invalid",
                "message": "decrypted credential is empty or not valid UTF-8",
            })),
        )
            .into_response();
    }

    let private = body.private.unwrap_or(true);

    // ── dispatch to host API ────────────────────────────────────────────
    let (clone_url, html_url, ssh_url) = match host {
        ProjectHost::Github => {
            let outcome = crate::api::github::create_repo(crate::api::github::CreateRepoRequest {
                name: &name,
                private,
                owner: owner.as_deref(),
                pat: &pat,
            })
            .await;
            match outcome {
                crate::api::github::CreateRepoOutcome::Created {
                    clone_url,
                    html_url,
                    ssh_url,
                } => (clone_url, html_url, ssh_url),
                crate::api::github::CreateRepoOutcome::Failed {
                    http_status,
                    message,
                    body_excerpt,
                } => return host_failure_response("github", http_status, message, body_excerpt),
                crate::api::github::CreateRepoOutcome::Transport { error } => {
                    return host_transport_response("github", error);
                }
            }
        }
        ProjectHost::Gitlab => {
            let outcome = crate::api::gitlab::create_repo(crate::api::gitlab::CreateRepoRequest {
                name: &name,
                private,
                owner: owner.as_deref(),
                pat: &pat,
            })
            .await;
            match outcome {
                crate::api::gitlab::CreateRepoOutcome::Created {
                    clone_url,
                    html_url,
                    ssh_url,
                } => (clone_url, html_url, ssh_url),
                crate::api::gitlab::CreateRepoOutcome::Failed {
                    http_status,
                    message,
                    body_excerpt,
                } => return host_failure_response("gitlab", http_status, message, body_excerpt),
                crate::api::gitlab::CreateRepoOutcome::Transport { error } => {
                    return host_transport_response("gitlab", error);
                }
            }
        }
        _ => unreachable!("filtered above"),
    };

    let _ = (html_url, ssh_url); // recorded in audit diff below; row carries only clone_url

    let workspace_path = resolve_workspace_path(body.workspace_path.as_deref(), &name);

    insert_project_row(
        &state,
        &auth,
        InsertProjectArgs {
            name,
            repo_url: clone_url,
            host,
            owner,
            default_credential_id: Some(credential_id.to_string()),
            workspace_path,
            org_id,
        },
        /* fire_clone_after */ true,
    )
    .await
}

/// Arguments to [`insert_project_row`]. Centralises the field set the
/// two mode-specific paths converge on so we don't drift if a future
/// column lands on `projects`.
struct InsertProjectArgs {
    name: String,
    repo_url: String,
    host: ProjectHost,
    owner: Option<String>,
    default_credential_id: Option<String>,
    workspace_path: String,
    org_id: String,
}

/// Shared INSERT + audit step the two mode handlers converge on.
///
/// When `fire_clone_after` is true we spawn a background
/// [`crate::saas::dispatch::clone_project_dispatch`] tokio task before
/// returning so the runner clone RPC fires without blocking the 201
/// response. The clone outcome is recorded via the existing
/// `project.clone` audit row + workspace_path UPDATE.
async fn insert_project_row(
    state: &AppState,
    auth: &OptionalAuthUser,
    args: InsertProjectArgs,
    fire_clone_after: bool,
) -> axum::response::Response {
    let InsertProjectArgs {
        name,
        repo_url,
        host,
        owner,
        default_credential_id,
        workspace_path,
        org_id,
    } = args;

    let id = Uuid::new_v4().to_string();
    let row = ProjectRow {
        id: id.clone(),
        name: name.clone(),
        repo_url: repo_url.clone(),
        host: host.as_str().to_string(),
        owner: owner.clone(),
        default_credential_id: default_credential_id.clone(),
        workspace_path: workspace_path.clone(),
        org_id: org_id.clone(),
        created_at: String::new(),
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

    // Fire the clone RPC asynchronously so the operator gets a 201 the
    // moment the remote exists; the runner pulls the (still-empty) repo
    // in the background. The existing `project.clone` audit row + the
    // `workspace_path` UPDATE on success/failure surface the outcome
    // to the dashboard via the audit feed and a subsequent
    // `GET /api/projects/{id}` read.
    if fire_clone_after {
        let state_clone = state.clone();
        let project_id = row.id.clone();
        let repo_url_clone = row.repo_url.clone();
        let workspace_clone = row.workspace_path.clone();
        let credential_clone = row.default_credential_id.clone();
        let org_id_clone = org_id.clone();
        let user_id = user.map(|u| u.id.clone());
        let user_email = user.map(|u| u.email.clone());
        let project_name_clone = row.name.clone();
        tokio::spawn(async move {
            let outcome = crate::saas::dispatch::clone_project_dispatch(
                &state_clone,
                &org_id_clone,
                &repo_url_clone,
                &workspace_clone,
                credential_clone.as_deref(),
            )
            .await;

            let (status_label, resolved_path, error_str) = match &outcome {
                crate::saas::dispatch::CloneDispatchOutcome::Ok { resolved_path } => {
                    let conn = state_clone.db.lock().unwrap();
                    let _ = conn.execute(
                        "UPDATE projects SET workspace_path = ?1 \
                          WHERE id = ?2 AND org_id = ?3",
                        params![resolved_path, project_id, org_id_clone],
                    );
                    ("ok", Some(resolved_path.clone()), None)
                }
                crate::saas::dispatch::CloneDispatchOutcome::CloneFailed { error } => {
                    ("clone_failed", None, Some(error.clone()))
                }
                crate::saas::dispatch::CloneDispatchOutcome::RpcFailed { error } => {
                    ("rpc_failed", None, Some(error.clone()))
                }
            };

            let diff = serde_json::json!({
                "project_id": project_id,
                "name": project_name_clone,
                "repo_url": repo_url_clone,
                "workspace_path": workspace_clone,
                "resolved_path": resolved_path,
                "outcome": status_label,
                "error": error_str,
                "trigger": "create_mode",
            })
            .to_string();
            let conn = state_clone.db.lock().unwrap();
            audit::log(
                &conn,
                &org_id_clone,
                user_id.as_deref(),
                user_email.as_deref(),
                audit::actions::PROJECT_CLONE,
                audit::resources::PROJECT,
                Some(&project_id),
                Some(&diff),
            );
        });
    }

    (StatusCode::CREATED, Json(row)).into_response()
}

/// Render the structured host-API failure response. Status code is
/// 502 (Bad Gateway) for 5xx upstream errors, 422 (Unprocessable
/// Entity) for 4xx — the host-API result is "the dashboard's request
/// was syntactically fine but the upstream said no", which is the
/// canonical 422 use case (`POST /user/repos` returns 422 on a name
/// collision). Anything else maps to 502.
fn host_failure_response(
    host: &str,
    http_status: u16,
    message: String,
    body_excerpt: String,
) -> axum::response::Response {
    let status = if (400..500).contains(&http_status) {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::BAD_GATEWAY
    };
    (
        status,
        Json(serde_json::json!({
            "error": "host_api_failed",
            "code": format!("{host}_{http_status}"),
            "message": message,
            "host_response_excerpt": body_excerpt,
        })),
    )
        .into_response()
}

/// Render the transport-failure response for host-API dispatch (DNS,
/// TLS, connection refused). Status code is 502.
fn host_transport_response(host: &str, error: String) -> axum::response::Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": "host_api_unreachable",
            "code": format!("{host}_transport_error"),
            "message": error,
            "host_response_excerpt": "",
        })),
    )
        .into_response()
}

/// Load the AES-GCM master key from disk. Mirrors the same resolution
/// rules `api::credentials::load_master_key` uses — `$BRANCHWORK_DATA_DIR`
/// when set, otherwise the server's `claude_dir` (derived from
/// `state.settings_path.parent()`).
fn load_master_key(
    state: &AppState,
) -> Result<crate::crypto::MasterKey, crate::crypto::CryptoError> {
    let claude_dir = state
        .settings_path
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = crate::crypto::resolve_master_key_path(&claude_dir);
    crate::crypto::load_or_generate_master_key(&path)
}

/// Validate a host-side owner/org/group name. Accepts `[A-Za-z0-9._-]`
/// only — tight enough to interpolate into the host API URL without
/// escaping. GitHub's actual rules are a strict subset
/// (alphanumeric + hyphen, no leading hyphen) but we trade leniency
/// for a single shared validator; the host will reject malformed
/// owner names downstream regardless.
fn is_valid_host_owner(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
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
