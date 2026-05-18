//! HTTP endpoints exposing the per-branch push lock to external clients.
//!
//! Out-of-process auto-bump CI (the canonical example) holds a token
//! tied to its CI run and wants to push to `origin/master` (or wherever
//! the project's default branch lives). Without coordination, it races
//! the auto-mode loop running inside `branchwork-server`: both produce
//! a local commit, both call `git push`, exactly one wins — the loser
//! falls into the rebase-on-NFF path (Phase 1) and retries.
//!
//! These endpoints let the external client acquire the SAME advisory
//! lock the in-process auto-mode flow uses (see
//! [`crate::auto_mode::wait_for_push_lock`]). If both fire in the same
//! second, exactly one acquires first; the other waits up to
//! `wait_secs` (default 30s) for the holder to release. The lock is
//! identified by a `branch` name + a server-issued opaque `token`. The
//! holder MUST call `release` after its push completes; if it crashes
//! mid-push, the 30s TTL on the underlying row force-evicts the lock
//! so future pushes don't deadlock.
//!
//! ## Endpoints
//!
//! - `POST /api/git/push-lock` — wait+acquire, returns 200 with
//!   `{token, branch, holderKind}` or 503 with the live holder's
//!   metadata on timeout.
//! - `POST /api/git/push-lock/release` — release, returns 200 with
//!   `{released: bool}`. False means the row was already gone or the
//!   token didn't match the current holder.
//! - `GET  /api/git/push-lock/{branch}` — peek, returns 200 with the
//!   holder snapshot, or 404 when no row exists. No state change; safe
//!   to poll.
//!
//! ## Authentication
//!
//! Endpoints use `OptionalAuthUser` so external CI on the same network
//! (the canonical use case for self-hosted Branchwork) can hit them
//! without a user session. The acquire path issues an opaque token,
//! and release requires that token verbatim — so the token IS the
//! authentication for the lock-release operation. Audit rows are
//! tagged with the calling user when present, or `branchwork-ci`
//! otherwise. Operators who want stricter auth can put a reverse
//! proxy in front and require a header.
//!
//! ## Wait semantics
//!
//! The server polls the underlying DB row every 200 ms until acquire
//! succeeds or `wait_secs` elapses. On timeout, the response includes
//! the live holder's `holder_kind`, `age_secs`, and `holder_meta` so
//! the CI side can render a useful error ("auto-mode is currently
//! pushing for plan X, age 4s").

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::OptionalAuthUser;
use crate::auto_mode::{PUSH_LOCK_DEFAULT_WAIT_SECS, PushLockError, wait_for_push_lock};
use crate::db::{PushLockHolder, peek_push_lock, release_push_lock};
use crate::state::AppState;

/// Audit-log user fallback when the HTTP caller is unauthenticated
/// (typical for external auto-bump CI). Matches the convention
/// `auto_mode.rs` uses for in-process loop-driven actions
/// (`branchwork-auto-mode`).
const PUSH_LOCK_SYSTEM_USER: &str = "branchwork-ci";

/// Body for `POST /api/git/push-lock`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AcquireBody {
    /// Branch to lock — typically the repo's canonical default branch
    /// (`master` / `main`). Per-branch independence means a project on
    /// `main` and another on `master` don't contend.
    pub branch: String,
    /// Free-form tag describing the caller. Recorded on the row and
    /// surfaced in the timeout-response so the loser can see who won.
    /// Convention: `"ci"` for auto-bump CI, `"manual"` for one-shot
    /// operator scripts.
    #[serde(default = "default_holder_kind")]
    pub holder_kind: String,
    /// Optional metadata identifying the caller (e.g. CI run id, PR
    /// number). Stored verbatim; loud about its semantics so it can be
    /// JSON or a bare string. Surfaced in the timeout-response.
    pub holder_meta: Option<String>,
    /// How long to wait for the lock before returning 503. Clamped to
    /// `1..=60` seconds; defaults to [`PUSH_LOCK_DEFAULT_WAIT_SECS`].
    /// A 0-second wait is rounded up to 1 so the caller still gets one
    /// real acquire attempt.
    pub wait_secs: Option<u64>,
}

fn default_holder_kind() -> String {
    "ci".to_string()
}

/// Body for `POST /api/git/push-lock/release`.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseBody {
    pub branch: String,
    pub token: String,
}

/// Response shape for a successful acquire.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct AcquireResponse {
    branch: String,
    token: String,
    /// `holder_kind` echoed back so the client can spot a confused
    /// caller (e.g. its retry logic accidentally double-acquiring).
    holder_kind: String,
}

/// Holder-snapshot serializer for the timeout (503) response and the
/// GET peek endpoint. Wire shape is camelCase to match the rest of the
/// API.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct HolderSnapshot {
    branch: String,
    holder_kind: String,
    holder_meta: Option<String>,
    holder_pid: i64,
    taken_at: String,
    age_secs: i64,
}

impl HolderSnapshot {
    fn from(branch: &str, h: PushLockHolder) -> Self {
        Self {
            branch: branch.to_string(),
            holder_kind: h.holder_kind,
            holder_meta: h.holder_meta,
            holder_pid: h.holder_pid,
            taken_at: h.taken_at,
            age_secs: h.age_secs,
        }
    }
}

/// `POST /api/git/push-lock` — wait + acquire.
///
/// On success returns `200` + `{branch, token, holderKind}`. The client
/// MUST call the release endpoint with the same `(branch, token)` after
/// its push completes. If it crashes, the 30s TTL on the underlying
/// row force-evicts the lock so other writers don't deadlock.
///
/// On timeout returns `503 SERVICE UNAVAILABLE` + the live holder's
/// snapshot so the caller can render a diagnostic and retry later.
pub async fn acquire(
    State(state): State<AppState>,
    user: OptionalAuthUser,
    Json(body): Json<AcquireBody>,
) -> impl IntoResponse {
    if body.branch.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "branch is required" })),
        )
            .into_response();
    }
    let wait_secs = body
        .wait_secs
        .unwrap_or(PUSH_LOCK_DEFAULT_WAIT_SECS)
        .clamp(1, 60);

    let result = wait_for_push_lock(
        &state.db,
        &body.branch,
        &body.holder_kind,
        std::process::id() as i64,
        body.holder_meta.as_deref(),
        Duration::from_secs(wait_secs),
    )
    .await;

    match result {
        Ok(guard) => {
            // Take ownership of the release — the HTTP client is now
            // responsible. Drop semantics are disabled via `forget()`.
            let token = guard.forget();

            // Audit row so an admin can see every acquire/release for
            // a branch. `holder_meta` lives in the diff payload so an
            // operator can correlate a stuck CI run to its lock acquire.
            let user_id_owned = user.0.as_ref().map(|u| u.id.clone());
            let user_email_owned = user.0.as_ref().map(|u| u.email.clone());
            let conn = state.db.lock().unwrap();
            audit::log(
                &conn,
                user.org_id(),
                user_id_owned.as_deref(),
                Some(user_email_owned.as_deref().unwrap_or(PUSH_LOCK_SYSTEM_USER)),
                audit::actions::PUSH_LOCK_ACQUIRED,
                audit::resources::PLAN,
                Some(&body.branch),
                Some(
                    &serde_json::json!({
                        "branch": body.branch,
                        "holder_kind": body.holder_kind,
                        "holder_meta": body.holder_meta,
                        "wait_secs": wait_secs,
                    })
                    .to_string(),
                ),
            );

            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(AcquireResponse {
                        branch: body.branch,
                        token,
                        holder_kind: body.holder_kind,
                    })
                    .unwrap_or(serde_json::Value::Null),
                ),
            )
                .into_response()
        }
        Err(PushLockError::Timeout(holder)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "push_lock_busy",
                "branch": body.branch,
                "wait_secs": wait_secs,
                "holder": HolderSnapshot::from(&body.branch, holder),
            })),
        )
            .into_response(),
    }
}

/// `POST /api/git/push-lock/release` — release the lock IFF the token
/// matches the current holder. Returns `200` with `{released: bool}`.
/// `released=false` covers both "wrong token" and "row already gone":
/// the caller doesn't need to distinguish, and forging a "true"
/// success when the lock has actually moved on would be misleading.
pub async fn release(
    State(state): State<AppState>,
    user: OptionalAuthUser,
    Json(body): Json<ReleaseBody>,
) -> impl IntoResponse {
    if body.branch.trim().is_empty() || body.token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "branch and token are required"
            })),
        )
            .into_response();
    }

    let released = release_push_lock(&state.db, &body.branch, &body.token);

    // Audit-log the release attempt. We log every call (incl. wrong-
    // token / row-gone) so a malicious / buggy client can be traced.
    let user_id_owned = user.0.as_ref().map(|u| u.id.clone());
    let user_email_owned = user.0.as_ref().map(|u| u.email.clone());
    let conn = state.db.lock().unwrap();
    audit::log(
        &conn,
        user.org_id(),
        user_id_owned.as_deref(),
        Some(user_email_owned.as_deref().unwrap_or(PUSH_LOCK_SYSTEM_USER)),
        audit::actions::PUSH_LOCK_RELEASED,
        audit::resources::PLAN,
        Some(&body.branch),
        Some(
            &serde_json::json!({
                "branch": body.branch,
                "released": released,
            })
            .to_string(),
        ),
    );

    Json(serde_json::json!({ "released": released })).into_response()
}

/// `GET /api/git/push-lock/{branch}` — peek at the current holder of
/// the push lock for `branch`. Returns `200` with a holder snapshot or
/// `404` when no row exists. Read-only; safe to poll.
pub async fn peek(
    State(state): State<AppState>,
    _user: OptionalAuthUser,
    Path(branch): Path<String>,
) -> impl IntoResponse {
    match peek_push_lock(&state.db, &branch) {
        Some(h) => Json(
            serde_json::to_value(HolderSnapshot::from(&branch, h))
                .unwrap_or(serde_json::Value::Null),
        )
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no_lock", "branch": branch })),
        )
            .into_response(),
    }
}
