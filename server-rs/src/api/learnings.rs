//! REST surface for the org-shared learnings store.
//!
//! Two families of endpoints live in this module:
//!
//! Phase 1 (pending-learning queue):
//! - `GET  /api/learnings/pending`             — list `ci_failure_events`
//!   rows with `resolved_at IS NULL` (the Learnings due panel).
//! - `GET  /api/learnings/pending/{id}/log`    — failure-log tail for one
//!   pending row (drilldown).
//!
//! Phase 2.2 (org-shared learnings — append-only):
//! - `GET  /api/learnings`                     — list active learnings,
//!   filterable by `?category=` and `?kind=`. Pass `?include=archived` to
//!   surface tombstones for the Activity tab.
//! - `GET  /api/learnings/{id}`                — single learning with full
//!   `bodyMd`. Returns archived rows too so the audit view can render
//!   them; callers that want only active should check
//!   `archivedAt == null`.
//! - `POST /api/learnings/{id}/archive`        — soft-delete with required
//!   `{ reason: string }`. Idempotent at the row level (already-archived
//!   returns 410 Gone, not 200, so the operator can tell their action
//!   was a no-op). Editing is intentionally out of scope: corrections
//!   happen by archiving and writing a new entry, preserving the audit
//!   trail.
//!
//! All endpoints are org-scoped via [`OptionalAuthUser::org_id`] —
//! anonymous callers in standalone mode resolve to the default org, the
//! same convention the rest of `api/` uses. Writes (archive) emit an
//! [`audit::actions::LEARNING_ARCHIVE`] row on
//! [`audit::resources::LEARNING`]; refusal paths (422, 410) DO NOT
//! write audit rows so the trail records only consequential transitions.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::api::github;
use crate::audit;
use crate::auth::OptionalAuthUser;
use crate::crypto::{self, MasterKey, resolve_master_key_path};
use crate::db::{CredentialKind, Learning, LearningError, LearningKind, PromotionCandidate};
use crate::state::AppState;

/// A single pending CI-failure waiting on a learning capture. Wire shape
/// is the camelCase superset of `db::PendingCiFailure` plus the
/// authoring metadata the dashboard needs to render the row (agent +
/// plan + task + branch). All `Option<...>` fields collapse to `null` on
/// the wire so the frontend can treat missing data uniformly.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLearningRow {
    /// `ci_failure_events.id` — also the path parameter for the log
    /// drilldown endpoint.
    pub id: i64,
    pub agent_id: String,
    /// Current `agents.status` (running / starting / completed / failed /
    /// killed) so the panel can show the live state of the blocked
    /// agent. `null` when the agent row is gone (defensive — should not
    /// happen, since `record_ci_failure_observed` only inserts when a
    /// live agent owns the branch).
    pub agent_status: Option<String>,
    pub plan_name: String,
    pub task_number: Option<String>,
    pub branch: String,
    pub run_id: String,
    pub run_url: Option<String>,
    pub workflow: Option<String>,
    pub conclusion: Option<String>,
    pub failed_job: Option<String>,
    pub summary: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingLearningsResponse {
    pub items: Vec<PendingLearningRow>,
}

/// GET /api/learnings/pending
///
/// Org-scoped list of `ci_failure_events` rows where `resolved_at IS
/// NULL`. Joined to `agents` so the dashboard can render the live
/// status of the blocked agent inline. Sorted oldest-first so the
/// dashboard's drilldown order matches the production order the agent
/// would see them in.
pub async fn list_pending(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let items = fetch_pending(&state, &org_id);
    Json(PendingLearningsResponse { items })
}

fn fetch_pending(state: &AppState, org_id: &str) -> Vec<PendingLearningRow> {
    let conn = state.db.lock().unwrap();
    let Ok(mut stmt) = conn.prepare(
        "SELECT e.id, e.agent_id, a.status, e.plan_name, e.task_number, e.branch, \
                e.run_id, e.run_url, e.workflow, e.conclusion, e.failed_job, e.summary, \
                e.observed_at \
           FROM ci_failure_events e \
           LEFT JOIN agents a ON a.id = e.agent_id \
          WHERE e.org_id = ?1 \
            AND e.resolved_at IS NULL \
          ORDER BY e.observed_at ASC, e.id ASC",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![org_id], |row| {
        Ok(PendingLearningRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            agent_status: row.get(2)?,
            plan_name: row.get(3)?,
            task_number: row.get(4)?,
            branch: row.get(5)?,
            run_id: row.get(6)?,
            run_url: row.get(7)?,
            workflow: row.get(8)?,
            conclusion: row.get(9)?,
            failed_job: row.get(10)?,
            summary: row.get(11)?,
            observed_at: row.get(12)?,
        })
    })
    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

/// GET /api/learnings/pending/{event_id}/log
///
/// Returns the cached-or-freshly-fetched failure log for the failing
/// GitHub Actions run referenced by a pending learning row, as
/// `text/plain`. Tail-trimmed to ~8 KB (the gh shell-out cap; the panel
/// renders the last 100 lines which is well inside that budget).
///
/// Pipeline:
/// 1. Look up the `ci_failure_events` row by id, gated on `org_id` so a
///    cross-org id leak returns 404.
/// 2. JOIN `ci_runs` on `(plan_name, run_id)` to recover the
///    autoincrement PK that [`crate::ci::fetch_failure_log`] uses as
///    its cache key.
/// 3. Delegate to that helper, which is already mode-aware (standalone
///    shells out locally, SaaS dispatches to the connected runner).
///
/// 404 paths: unknown event id, cross-org, no matching `ci_runs` row
/// (record was inserted by a path that didn't go through
/// `trigger_after_merge`), `gh` unavailable or run still pending.
pub async fn fetch_log(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(event_id): Path<i64>,
) -> Response {
    let org_id = auth.org_id().to_string();

    // Step 1+2: look up the event + its matching ci_runs.id in one SQL.
    // LEFT JOIN so a missing ci_runs row returns Some((..., None)) and
    // we can distinguish "event doesn't exist or wrong org" (404
    // event_not_found) from "event exists but no cached ci_runs row to
    // fetch from" (404 log_unavailable).
    let resolved: Option<(String, String, Option<i64>)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT e.plan_name, e.run_id, c.id \
               FROM ci_failure_events e \
               LEFT JOIN ci_runs c \
                      ON c.plan_name = e.plan_name AND c.run_id = e.run_id \
              WHERE e.id = ?1 AND e.org_id = ?2 \
              LIMIT 1",
            params![event_id, org_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .ok()
    };

    let Some((plan_name, run_id, ci_runs_id)) = resolved else {
        return not_found("event_not_found");
    };
    let Some(ci_runs_id) = ci_runs_id else {
        // Event exists but no ci_runs row to anchor the fetch. Surface
        // a distinct error code so the dashboard can copy-paste the
        // run_url instead of waiting on a refetch.
        return not_found_with_url(
            "log_unavailable",
            "no matching ci_runs row — open the run in GitHub directly",
        );
    };

    // Step 3: delegate. Mode-aware: standalone shells out locally,
    // SaaS dispatches to the connected runner.
    let _ = plan_name; // recovered for the JOIN, no longer needed here.
    let _ = run_id;
    match crate::ci::fetch_failure_log(
        &state.db,
        &state.runners,
        state.plans_dir.clone(),
        ci_runs_id,
    )
    .await
    {
        Some(log) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            log,
        )
            .into_response(),
        None => not_found_with_url(
            "log_unavailable",
            "failure log unavailable — run may still be pending, have no \
             remote, or `gh` is not installed",
        ),
    }
}

fn not_found(code: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": code})),
    )
        .into_response()
}

fn not_found_with_url(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": code, "message": message})),
    )
        .into_response()
}

// ── Org-shared learnings (Phase 2.2) ────────────────────────────────────────

/// Wrapper for `GET /api/learnings`. A bare array would force a future
/// envelope (paging, totals, kind facets) to be a breaking change;
/// `{ items: [...] }` keeps the surface forward-extensible the way the
/// pending list ([`PendingLearningsResponse`]) already is.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LearningsResponse {
    pub items: Vec<Learning>,
}

/// Query parameters for `GET /api/learnings`. All fields are optional;
/// unknown values for `kind` collapse to "no filter" rather than 400 —
/// the dashboard sends raw user input and a typo should yield an empty
/// list, not a fatal client error.
#[derive(Debug, Deserialize, Default)]
pub struct ListLearningsQuery {
    /// Restrict to one [`LearningKind`] wire form (`feedback` | `project`
    /// | `reference`). Unknown values are treated as "no filter".
    pub kind: Option<String>,
    /// Restrict to one category. Case-sensitive exact match — the
    /// dashboard's category dropdown is populated from the existing
    /// rows so casing matches by construction.
    pub category: Option<String>,
    /// Pass `?include=archived` to include soft-deleted rows in the
    /// response. Any other value (or absent) hides them. The archive
    /// view in the Activity tab is the only intended consumer.
    pub include: Option<String>,
}

/// `GET /api/learnings`
///
/// Org-scoped list of active learnings, newest first. Filterable by
/// `?kind=<wire>` and `?category=<exact>`. Pass `?include=archived` to
/// surface tombstones alongside active rows (the Activity tab's
/// archive view).
///
/// Unknown `kind` values fall through to "no filter" (the dashboard's
/// category dropdown is the only authority for valid values; a typo
/// returning an empty list would be misleading). Unknown `include`
/// values are treated as "active only" so a future `?include=foo`
/// doesn't accidentally surface archived rows.
pub async fn list_learnings(
    State(state): State<AppState>,
    Query(q): Query<ListLearningsQuery>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let kind_filter = q.kind.as_deref().and_then(LearningKind::parse);
    let include_archived = q.include.as_deref() == Some("archived");
    let mut items =
        crate::db::list_learnings_for_org(&state.db, &org_id, kind_filter, include_archived);
    if let Some(cat) = q.category.as_deref() {
        items.retain(|l| l.category == cat);
    }
    Json(LearningsResponse { items })
}

/// `GET /api/learnings/{id}`
///
/// Single learning with full `bodyMd`. Returns archived rows too so the
/// audit view can render them; callers that want only active should
/// check `archivedAt == null`.
///
/// Status codes:
/// - 200: row found in the caller's org.
/// - 404: id does not exist, OR belongs to a different org.
///
/// The cross-org case collapses to a generic 404 so the absence of a
/// row in this org can't be distinguished from "no such row anywhere"
/// — same convention `delete_credential` uses for its
/// `credential_not_found` branch.
pub async fn get_learning(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: OptionalAuthUser,
) -> Response {
    let org_id = auth.org_id().to_string();
    match crate::db::get_learning(&state.db, &id) {
        Ok(row) if row.org_id == org_id => Json(row).into_response(),
        Ok(_) => not_found("learning_not_found"),
        Err(LearningError::NotFound) => not_found("learning_not_found"),
        Err(LearningError::SlugCollision) => {
            // SlugCollision is structurally impossible on a read path
            // (the helper never inserts), but the From<rusqlite::Error>
            // shim could theoretically classify a stray constraint
            // error this way. Map to a 500 rather than 404 so a corrupt
            // row surfaces loudly instead of silently disappearing.
            eprintln!("[learnings] unexpected SlugCollision from get_learning({id})");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "lookup_failed" })),
            )
                .into_response()
        }
        Err(LearningError::Db(e)) => {
            eprintln!("[learnings] get_learning({id}) db error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "lookup_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Body for `POST /api/learnings/{id}/archive`. `reason` is required —
/// the audit trail's whole purpose is to record WHY a once-canonical
/// lesson is no longer active, so we reject empty/missing here rather
/// than letting the DB write a NULL.
#[derive(Debug, Deserialize)]
pub struct ArchiveLearningBody {
    pub reason: Option<String>,
}

/// `POST /api/learnings/{id}/archive`
///
/// Soft-delete a learning with an operator-supplied `reason`. The row
/// stays in the DB; only `archived_at` / `archived_reason` flip. List
/// endpoints hide archived rows by default; pass `?include=archived` to
/// see them.
///
/// Status codes:
/// - 200: row archived. Body: `{ ok: true, id, archivedAt }`.
/// - 404: id not found in caller's org (same 404-on-cross-org masking
///   convention as `get_learning`).
/// - 410: already archived. Body includes the original `archivedAt` +
///   `archivedReason` so the operator can see what happened.
/// - 422: `reason` missing / empty after trim.
///
/// Refusal paths (404 / 410 / 422) DO NOT write an audit row — the
/// trail records only consequential transitions.
pub async fn archive_learning(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: OptionalAuthUser,
    Json(body): Json<ArchiveLearningBody>,
) -> Response {
    let reason_raw = body.reason.unwrap_or_default();
    let reason = reason_raw.trim();
    if reason.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "reason_required",
                "message": "archive requires a non-empty `reason`",
            })),
        )
            .into_response();
    }

    // Pre-fetch the row scoped to the caller's org. This serves three
    // purposes: (a) 404 on unknown / cross-org id without giving the
    // archive helper a chance to flip the wrong row, (b) capture the
    // kind/slug/category for the audit diff, (c) distinguish "already
    // archived" (410) from "freshly archived" (200) without relying on
    // the helper's bool return.
    let org_id = auth.org_id().to_string();
    let existing = match crate::db::get_learning(&state.db, &id) {
        Ok(row) if row.org_id == org_id => row,
        Ok(_) => return not_found("learning_not_found"),
        Err(LearningError::NotFound) => return not_found("learning_not_found"),
        Err(e) => {
            eprintln!("[learnings] archive lookup({id}) failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "archive_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    if existing.archived_at.is_some() {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "already_archived",
                "message": "learning is already archived",
                "id": existing.id,
                "archivedAt": existing.archived_at,
                "archivedReason": existing.archived_reason,
            })),
        )
            .into_response();
    }

    // Flip the row. The helper is itself idempotent (returns Ok(false)
    // if already archived), but we already handled that case above; a
    // concurrent caller racing us through the gap converges on the
    // first-writer-wins outcome — both operators see the same final
    // state.
    let flipped = match crate::db::archive_learning(&state.db, &id, reason) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[learnings] archive_learning({id}) failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "archive_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    if !flipped {
        // Race: another caller archived between our SELECT and UPDATE.
        // Re-fetch so the response carries their archivedAt/reason, not
        // ours. Matches the 410 convergence above.
        let now_archived = crate::db::get_learning(&state.db, &id).ok();
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "already_archived",
                "message": "learning was archived by another caller",
                "id": id,
                "archivedAt": now_archived.as_ref().and_then(|r| r.archived_at.clone()),
                "archivedReason": now_archived.as_ref().and_then(|r| r.archived_reason.clone()),
            })),
        )
            .into_response();
    }

    // Re-read so the response (and the audit diff) carries the actual
    // archivedAt the DB stamped, not our local clock.
    let archived_at = crate::db::get_learning(&state.db, &id)
        .ok()
        .and_then(|r| r.archived_at);

    let diff = serde_json::json!({
        "learning_id": existing.id,
        "kind": existing.kind,
        "slug": existing.slug,
        "category": existing.category,
        "reason": reason,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            audit::actions::LEARNING_ARCHIVE,
            audit::resources::LEARNING,
            Some(&existing.id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "ok": true,
        "id": existing.id,
        "archivedAt": archived_at,
    }))
    .into_response()
}

// ── Promotion to CLAUDE.md (Phase 4, Task 4.2) ──────────────────────────────

/// One promotion candidate plus the exact markdown block the promote
/// flow would append under `## Other rules` in `CLAUDE.md`. The
/// `appendPreview` lets the dashboard render the diff preview ("here is
/// what would be appended") without any GitHub round trip — it is built
/// purely from the learning body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionCandidateRow {
    #[serde(flatten)]
    pub candidate: PromotionCandidate,
    pub append_preview: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionCandidatesResponse {
    pub items: Vec<PromotionCandidateRow>,
}

/// `GET /api/learnings/promotion-candidates`
///
/// Org-scoped list of learnings that have crossed the promotion
/// threshold, newest first, each with the rule block that would be
/// appended to `CLAUDE.md`. Already-promoted candidates stay in the list
/// (carrying their `promotedPrUrl`) so the dashboard can render the PR
/// link rather than dropping the row.
pub async fn list_promotion_candidates(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let items = crate::db::list_promotion_candidates(&state.db, &org_id)
        .into_iter()
        .map(|candidate| {
            let append_preview = format_rule_block(&candidate);
            PromotionCandidateRow {
                candidate,
                append_preview,
            }
        })
        .collect();
    Json(PromotionCandidatesResponse { items })
}

/// Body for `POST /api/learnings/{id}/promote`. `projectId` names the
/// repo to open the PR against (its `repo_url` + `host` + default
/// credential). `credentialId` overrides the project's default credential
/// (must be a `gh_pat`); when omitted the project's
/// `default_credential_id` is used.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromoteBody {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub credential_id: Option<String>,
}

/// `POST /api/learnings/{id}/promote`
///
/// Approve a promotion candidate: open a PR against the project repo that
/// appends the rule (wrapped in an H3 heading) under `## Other rules` in
/// `CLAUDE.md`, then stamp the candidate row with the PR URL.
///
/// Status codes:
/// - 200: PR opened. Body `{ ok, prUrl, prNumber }`.
/// - 400: `projectId` missing.
/// - 404: learning is not a promotion candidate (or cross-org / archived);
///   project not found in the caller's org.
/// - 409: already promoted (body carries the existing `prUrl`).
/// - 422: unsupported host, unparseable repo URL, no usable PAT credential.
/// - 502: the GitHub API rejected one of the calls (body carries `step`
///   + the upstream message).
///
/// Only a successfully-opened PR writes the audit row + broadcasts
/// `learning_promoted`; every refusal path leaves state untouched.
pub async fn promote_learning(
    State(state): State<AppState>,
    Path(learning_id): Path<String>,
    auth: OptionalAuthUser,
    Json(body): Json<PromoteBody>,
) -> Response {
    let org_id = auth.org_id().to_string();

    // Must be a live candidate in this org.
    let Some(candidate) = crate::db::get_promotion_candidate(&state.db, &learning_id, &org_id)
    else {
        return json_err(
            StatusCode::NOT_FOUND,
            "not_a_candidate",
            "learning is not a promotion candidate",
        );
    };
    if let Some(existing) = candidate.promoted_pr_url.as_deref() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "already_promoted",
                "message": "this learning was already promoted",
                "prUrl": existing,
            })),
        )
            .into_response();
    }

    // Resolve the target repo from the project.
    let Some(project_id) = body
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "project_required",
            "projectId is required",
        );
    };
    let project: Option<(String, String, Option<String>)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT repo_url, host, default_credential_id FROM projects \
              WHERE id = ?1 AND org_id = ?2",
            params![project_id, org_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()
    };
    let Some((repo_url, host, default_credential_id)) = project else {
        return json_err(
            StatusCode::NOT_FOUND,
            "project_not_found",
            "no such project in this org",
        );
    };
    if host != "github" {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_host",
            "promotion currently supports GitHub repos only",
        );
    }
    let Some((owner, repo)) = github::parse_owner_repo(&repo_url) else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unparseable_repo_url",
            "could not parse owner/repo from the project's repo_url",
        );
    };

    // Resolve a PAT credential: explicit override, else project default.
    let Some(cred_id) = body
        .credential_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(default_credential_id)
    else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "credential_required",
            "no credential selected and the project has no default credential",
        );
    };

    let key = match load_master_key(&state) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[learnings] master key load failed: {e}");
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "master_key_unavailable",
                &e.to_string(),
            );
        }
    };
    let cred = match crate::db::get_credential(&state.db, &key, &cred_id) {
        Ok(c) => c,
        Err(crate::db::CredentialError::NotFound) => {
            return json_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "credential_not_found",
                "the selected credential does not exist",
            );
        }
        Err(crate::db::CredentialError::Crypto(_)) => {
            return json_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "credential_sealed",
                "the credential is sealed by an older master key — re-enter it",
            );
        }
        Err(crate::db::CredentialError::Db(e)) => {
            eprintln!("[learnings] credential lookup failed: {e}");
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "credential_lookup_failed",
                &e.to_string(),
            );
        }
    };
    // Ownership guard: an authenticated caller may only use their own
    // credentials (credentials are user-scoped). Anonymous standalone
    // callers have no credentials, so this is moot for them.
    if let Some(user) = auth.0.as_ref()
        && cred.owner_user_id != user.id
    {
        return json_err(
            StatusCode::FORBIDDEN,
            "credential_forbidden",
            "that credential belongs to another user",
        );
    }
    if cred.kind != CredentialKind::GhPat {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "credential_not_pat",
            "promotion needs a GitHub PAT credential (SSH keys cannot open PRs via the API)",
        );
    }
    let Ok(pat) = String::from_utf8(cred.secret_plaintext) else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "credential_invalid",
            "the credential secret is not valid UTF-8",
        );
    };

    // Build the rule block + PR metadata.
    let rule_block = format_rule_block(&candidate);
    let title = humanize_slug(&candidate.slug);
    let branch = format!(
        "branchwork/promote-learning/{}-{}",
        slug_for_branch(&candidate.slug),
        short_hex()
    );
    let commit_message = format!(
        "docs(claude): promote learning '{}' to project rules",
        candidate.slug
    );
    let pr_title = format!("Promote learning: {title}");
    let pr_body = format!(
        "Promotes a Branchwork learning that has been surfaced into {hits} agent sessions \
         (threshold {threshold} within {days} days) to the project's `CLAUDE.md` under \
         `## Other rules`.\n\nLearning slug: `{slug}` (category: {category}).\n\n\
         Opened automatically by Branchwork's \"promote to CLAUDE.md\" flow.",
        hits = candidate.hit_count,
        threshold = candidate.threshold,
        days = candidate.window_days,
        slug = candidate.slug,
        category = candidate.category,
    );

    let outcome = github::open_claude_md_pr(github::OpenPrRequest {
        owner: &owner,
        repo: &repo,
        pat: &pat,
        file_path: "CLAUDE.md",
        section_heading: "## Other rules",
        rule_block: &rule_block,
        branch: &branch,
        commit_message: &commit_message,
        pr_title: &pr_title,
        pr_body: &pr_body,
    })
    .await;

    let (html_url, pr_number) = match outcome {
        github::OpenPrOutcome::Opened { html_url, number } => (html_url, number),
        github::OpenPrOutcome::Failed {
            step,
            http_status,
            message,
            body_excerpt,
        } => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "github_api_failed",
                    "step": step,
                    "httpStatus": http_status,
                    "message": message,
                    "hostResponseExcerpt": body_excerpt,
                })),
            )
                .into_response();
        }
        github::OpenPrOutcome::Transport { step, error } => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "github_unreachable",
                    "step": step,
                    "message": error,
                })),
            )
                .into_response();
        }
    };

    // Stamp the candidate row. A concurrent double-promote loses the race
    // here (returns false); we still opened a real PR, so surface our URL
    // either way — the duplicate is a vanishingly rare two-operator race.
    match crate::db::mark_learning_promoted(&state.db, &learning_id, &org_id, &html_url) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "[learnings] promote {learning_id}: row was already stamped by a concurrent caller; \
                 returning the freshly-opened PR {html_url} anyway"
            );
        }
        Err(e) => {
            // The PR is open but we failed to record it. Surface the URL so
            // the operator can wire it up manually; do not 500.
            eprintln!("[learnings] promote {learning_id}: mark_learning_promoted failed: {e}");
        }
    }

    // Audit + broadcast on the successful open.
    let diff = serde_json::json!({
        "learning_id": learning_id,
        "slug": candidate.slug,
        "project_id": project_id,
        "repo": format!("{owner}/{repo}"),
        "pr_url": html_url,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &org_id,
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            audit::actions::LEARNING_PROMOTE,
            audit::resources::LEARNING,
            Some(&learning_id),
            Some(&diff),
        );
    }
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "learning_promoted",
        serde_json::json!({
            "learning_id": learning_id,
            "org_id": org_id,
            "pr_url": html_url,
        }),
    );

    Json(serde_json::json!({
        "ok": true,
        "prUrl": html_url,
        "prNumber": pr_number,
    }))
    .into_response()
}

fn json_err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// Format the markdown block appended under `## Other rules`: an H3
/// heading derived from the learning slug, followed by the learning
/// body verbatim. This is both the diff preview and the content the PR
/// commits, so the two can never drift.
fn format_rule_block(c: &PromotionCandidate) -> String {
    format!("### {}\n\n{}", humanize_slug(&c.slug), c.body_md.trim())
}

/// Turn a kebab/snake slug into a Title-ish heading: split on `-`/`_`,
/// capitalise the first word. e.g. `deploy-by-sha-not-edge` →
/// `Deploy by sha not edge`.
fn humanize_slug(slug: &str) -> String {
    let words: String = slug
        .split(['-', '_'])
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => slug.to_string(),
    }
}

/// Sanitise a slug for use inside a git ref name: keep alphanumerics,
/// `-`, `_`; collapse everything else to `-`. The capture_learning
/// slugifier already produces a clean slug, but a hand-authored learning
/// could carry something odd.
fn slug_for_branch(slug: &str) -> String {
    let s: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "learning".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Short random hex suffix so a promote retry never collides with a
/// half-opened branch from a prior attempt.
fn short_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Resolve + load the master key, deriving `claude_dir` from
/// `settings_path`'s parent (mirrors `api/credentials.rs::load_master_key`).
fn load_master_key(state: &AppState) -> Result<MasterKey, crypto::CryptoError> {
    let claude_dir = state
        .settings_path
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = resolve_master_key_path(&claude_dir);
    crypto::load_or_generate_master_key(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saas::runner_ws::{RunnerRegistry, new_runner_registry};
    use rusqlite::Connection;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Build a minimal `AppState` whose DB carries just the tables this
    /// module touches. Mirrors the pattern other `api/*` tests use
    /// (`runners::tests::test_app_state`). The registry/runners fields
    /// are unused on the read paths exercised here.
    fn test_state() -> AppState {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ci_failure_events (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id     TEXT    NOT NULL,
                plan_name    TEXT    NOT NULL,
                task_number  TEXT,
                branch       TEXT    NOT NULL,
                run_id       TEXT    NOT NULL,
                run_url      TEXT,
                workflow     TEXT,
                conclusion   TEXT,
                failed_job   TEXT,
                summary      TEXT,
                org_id       TEXT    NOT NULL DEFAULT 'default-org',
                observed_at  TEXT    NOT NULL DEFAULT (datetime('now')),
                resolved_at  TEXT
             );
             CREATE TABLE agents (
                id              TEXT PRIMARY KEY,
                status          TEXT,
                started_at      TEXT DEFAULT (datetime('now'))
             );
             CREATE TABLE ci_runs (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                plan_name    TEXT NOT NULL,
                task_number  TEXT NOT NULL,
                run_id       TEXT,
                failure_log  TEXT
             );
             -- Org-shared learnings (Phase 2.2). Schema mirrors db.rs migrate().
             CREATE TABLE learnings (
                id                 TEXT PRIMARY KEY,
                org_id             TEXT NOT NULL DEFAULT 'default-org',
                kind               TEXT NOT NULL,
                category           TEXT NOT NULL,
                slug               TEXT NOT NULL,
                body_md            TEXT NOT NULL,
                source_agent_id    TEXT,
                source_ci_run_id   INTEGER,
                created_at         TEXT NOT NULL DEFAULT (datetime('now')),
                archived_at        TEXT,
                archived_reason    TEXT
             );
             CREATE UNIQUE INDEX idx_learnings_org_kind_slug
                ON learnings(org_id, kind, slug);
             -- Promotion candidates + projects (Phase 4.2). No FK in the
             -- minimal test schema, so org rows do not need to exist.
             CREATE TABLE promotion_candidates (
                learning_id      TEXT PRIMARY KEY,
                org_id           TEXT NOT NULL DEFAULT 'default-org',
                hit_count        INTEGER NOT NULL,
                threshold        INTEGER NOT NULL,
                window_days      INTEGER NOT NULL,
                created_at       TEXT NOT NULL DEFAULT (datetime('now')),
                promoted_pr_url  TEXT,
                promoted_at      TEXT
             );
             CREATE TABLE projects (
                id                     TEXT PRIMARY KEY,
                name                   TEXT NOT NULL,
                repo_url               TEXT NOT NULL,
                host                   TEXT NOT NULL DEFAULT 'other',
                owner                  TEXT,
                default_credential_id  TEXT,
                workspace_path         TEXT NOT NULL DEFAULT '',
                org_id                 TEXT NOT NULL DEFAULT 'default-org'
             );
             -- audit_logs: archive endpoint writes here.
             CREATE TABLE audit_logs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                org_id        TEXT NOT NULL DEFAULT 'default-org',
                user_id       TEXT,
                user_email    TEXT,
                action        TEXT NOT NULL,
                resource_type TEXT NOT NULL,
                resource_id   TEXT,
                diff          TEXT,
                created_at    TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .unwrap();
        let db: crate::db::Db = Arc::new(std::sync::Mutex::new(conn));
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let plans_dir = PathBuf::from("/tmp/branchwork-test-plans-learnings");
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            plans_dir.clone(),
            PathBuf::from("/tmp/branchwork-test-claude-learnings"),
            0,
            true,
        );
        let runners: RunnerRegistry = new_runner_registry();
        AppState {
            db,
            plans_dir,
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners,
            settings_path: PathBuf::from("/tmp/branchwork-test-settings-learnings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(HashSet::new())),
            started_at: std::time::Instant::now(),
        }
    }

    #[allow(clippy::too_many_arguments)] // every arg pins a wire-shape column the test asserts on.
    fn seed_event(
        state: &AppState,
        agent_id: &str,
        agent_status: Option<&str>,
        plan: &str,
        task: &str,
        run_id: &str,
        org_id: &str,
        resolved: Option<&str>,
    ) -> i64 {
        let conn = state.db.lock().unwrap();
        if let Some(status) = agent_status {
            conn.execute(
                "INSERT OR REPLACE INTO agents (id, status) VALUES (?1, ?2)",
                params![agent_id, status],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO ci_failure_events \
                (agent_id, plan_name, task_number, branch, run_id, run_url, \
                 workflow, conclusion, failed_job, summary, org_id, observed_at, resolved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, datetime('now'), ?11)",
            params![
                agent_id,
                plan,
                task,
                format!("branchwork/{plan}/{task}"),
                run_id,
                format!("https://example.test/runs/{run_id}"),
                "tests",
                "failure",
                format!("{} workflow failed", "tests"),
                org_id,
                resolved,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[tokio::test]
    async fn list_pending_returns_only_unresolved_for_org() {
        let state = test_state();
        // Two pending in default-org, one resolved (excluded), one in
        // a different org (excluded).
        let id_one = seed_event(
            &state,
            "agent-1",
            Some("running"),
            "plan-a",
            "1.4",
            "run-1",
            "default-org",
            None,
        );
        let id_two = seed_event(
            &state,
            "agent-2",
            Some("running"),
            "plan-b",
            "2.1",
            "run-2",
            "default-org",
            None,
        );
        let _resolved = seed_event(
            &state,
            "agent-3",
            Some("completed"),
            "plan-a",
            "1.5",
            "run-3",
            "default-org",
            Some("2026-01-01 00:00:00"),
        );
        let _other_org = seed_event(
            &state,
            "agent-4",
            Some("running"),
            "plan-c",
            "1.1",
            "run-4",
            "other-org",
            None,
        );

        let items = fetch_pending(&state, "default-org");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, id_one);
        assert_eq!(items[0].agent_status.as_deref(), Some("running"));
        assert_eq!(items[0].plan_name, "plan-a");
        assert_eq!(items[0].task_number.as_deref(), Some("1.4"));
        assert_eq!(items[1].id, id_two);
    }

    #[tokio::test]
    async fn list_pending_is_empty_when_all_resolved() {
        let state = test_state();
        seed_event(
            &state,
            "agent-1",
            Some("running"),
            "p",
            "1.1",
            "r",
            "default-org",
            Some("2026-01-01 00:00:00"),
        );
        assert!(fetch_pending(&state, "default-org").is_empty());
    }

    #[tokio::test]
    async fn list_pending_surfaces_null_agent_status_when_agent_row_missing() {
        // Defensive — record_ci_failure_observed only inserts when a
        // live agent owns the branch, so this is unreachable through
        // the production path, but the LEFT JOIN must not drop the row.
        let state = test_state();
        seed_event(
            &state,
            "ghost-agent",
            None,
            "p",
            "1.1",
            "r",
            "default-org",
            None,
        );
        let items = fetch_pending(&state, "default-org");
        assert_eq!(items.len(), 1);
        assert!(items[0].agent_status.is_none());
    }

    #[tokio::test]
    async fn list_pending_orders_oldest_first() {
        let state = test_state();
        // Insert in reverse order; observed_at defaults to NOW so
        // direct INSERTs in the same statement order match insertion.
        // The ORDER BY tie-breaker is `id ASC`, so we rely on that
        // here (millisecond precision in datetime('now') ties at this
        // resolution).
        let a = seed_event(
            &state,
            "a",
            Some("running"),
            "p",
            "1.1",
            "r1",
            "default-org",
            None,
        );
        let b = seed_event(
            &state,
            "b",
            Some("running"),
            "p",
            "1.2",
            "r2",
            "default-org",
            None,
        );
        let items = fetch_pending(&state, "default-org");
        assert_eq!(items[0].id, a);
        assert_eq!(items[1].id, b);
    }

    // ── Phase 2.2 — list / get / archive ─────────────────────────────────

    use crate::auth::AuthUser;
    use crate::db::{LearningInput, archive_learning as db_archive, create_learning};

    /// Build a `LearningInput` quickly. Caller picks id+kind+slug+category+org;
    /// body is fixed so tests don't have to invent prose.
    fn input<'a>(
        id: &'a str,
        org_id: &'a str,
        kind: LearningKind,
        category: &'a str,
        slug: &'a str,
    ) -> LearningInput<'a> {
        LearningInput {
            id,
            org_id,
            kind,
            category,
            slug,
            body_md: "**Why:** test\n\n**How to apply:** in tests.",
            source_agent_id: None,
            source_ci_run_id: None,
        }
    }

    /// Decode an `IntoResponse`-returning future down to (status, json).
    /// Pattern mirrors what other api/* tests do — works for both `impl
    /// IntoResponse` and `Response` returns.
    async fn drive<R: axum::response::IntoResponse>(r: R) -> (u16, serde_json::Value) {
        let resp = r.into_response();
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
        };
        (status, value)
    }

    fn anon() -> OptionalAuthUser {
        OptionalAuthUser(None)
    }

    fn authed(user_id: &str, email: &str, org_id: &str) -> OptionalAuthUser {
        OptionalAuthUser(Some(AuthUser {
            id: user_id.to_string(),
            email: email.to_string(),
            org_id: org_id.to_string(),
            org_role: "member".to_string(),
        }))
    }

    #[tokio::test]
    async fn list_returns_active_rows_only_by_default() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Project, "deploy", "b"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "outdated").unwrap();

        let (status, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery::default()),
                anon(),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "only active rows by default: {body}");
        assert_eq!(items[0]["id"], "L-2");
    }

    #[tokio::test]
    async fn list_with_include_archived_surfaces_tombstones() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Feedback, "testing", "b"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "outdated").unwrap();

        let (status, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    include: Some("archived".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "include=archived surfaces both: {body}");
        let archived: Vec<&str> = items
            .iter()
            .filter(|l| !l["archivedAt"].is_null())
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(archived, vec!["L-1"]);
    }

    #[tokio::test]
    async fn list_filters_by_kind_and_category() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "default-org", LearningKind::Feedback, "deploy", "b"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-3", "default-org", LearningKind::Project, "testing", "c"),
        )
        .unwrap();

        // kind=feedback only.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("feedback".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"L-1"));
        assert!(ids.contains(&"L-2"));

        // category=testing only.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    category: Some("testing".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"L-1"));
        assert!(ids.contains(&"L-3"));

        // kind=feedback + category=testing → just L-1.
        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("feedback".to_string()),
                    category: Some("testing".to_string()),
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        let ids: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["L-1"]);
    }

    #[tokio::test]
    async fn list_unknown_kind_falls_through_to_no_filter() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "testing", "a"),
        )
        .unwrap();

        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery {
                    kind: Some("user".to_string()), // not a valid wire form
                    ..Default::default()
                }),
                anon(),
            )
            .await,
        )
        .await;
        // Unknown kind → no filter → row still visible. The deliberate
        // permissive behaviour is documented inline.
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_is_org_scoped() {
        let state = test_state();
        // Seed a second org's organisations row so the FK is happy
        // (though we don't actually have an organizations table in the
        // minimal test schema — defer to default-org for the seed and
        // use the column directly).
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();
        create_learning(
            &state.db,
            &input("L-2", "org-2", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (_s, body) = drive(
            list_learnings(
                State(state.clone()),
                Query(ListLearningsQuery::default()),
                authed("u-2", "u2@example.com", "org-2"),
            )
            .await,
        )
        .await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "L-2");
        assert_eq!(items[0]["orgId"], "org-2");
    }

    #[tokio::test]
    async fn get_returns_full_body_for_active_row() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();

        let (status, body) =
            drive(get_learning(State(state.clone()), Path("L-1".to_string()), anon()).await).await;
        assert_eq!(status, 200);
        assert_eq!(body["id"], "L-1");
        assert!(body["bodyMd"].as_str().unwrap().starts_with("**Why:**"));
        assert!(body["archivedAt"].is_null());
    }

    #[tokio::test]
    async fn get_returns_archived_rows_too() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "superseded").unwrap();

        let (status, body) =
            drive(get_learning(State(state.clone()), Path("L-1".to_string()), anon()).await).await;
        assert_eq!(status, 200);
        assert_eq!(body["archivedReason"], "superseded");
        assert!(!body["archivedAt"].is_null());
    }

    #[tokio::test]
    async fn get_404s_on_unknown_id() {
        let state = test_state();
        let (status, body) =
            drive(get_learning(State(state.clone()), Path("nope".to_string()), anon()).await).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "learning_not_found");
    }

    #[tokio::test]
    async fn get_404s_on_cross_org() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "org-1", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            get_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-2", "u2@example.com", "org-2"),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404, "cross-org reads must 404, not 403: {body}");
    }

    #[tokio::test]
    async fn archive_happy_path_flips_row_and_writes_audit() {
        let state = test_state();
        create_learning(
            &state.db,
            &input(
                "L-1",
                "default-org",
                LearningKind::Feedback,
                "testing",
                "no-mocks",
            ),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-1", "alice@example.com", "default-org"),
                Json(ArchiveLearningBody {
                    reason: Some("superseded by ADR 0009".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["id"], "L-1");
        assert!(!body["archivedAt"].is_null());

        // Audit row landed with the right shape.
        let (action, resource, resource_id, diff_str): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT action, resource_type, resource_id, diff \
                   FROM audit_logs WHERE resource_id = ?1 \
                  ORDER BY id DESC LIMIT 1",
                params!["L-1"],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(action, "learning.archive");
        assert_eq!(resource, "learning");
        assert_eq!(resource_id.as_deref(), Some("L-1"));
        let diff: serde_json::Value = serde_json::from_str(diff_str.as_deref().unwrap()).unwrap();
        assert_eq!(diff["learning_id"], "L-1");
        assert_eq!(diff["kind"], "feedback");
        assert_eq!(diff["slug"], "no-mocks");
        assert_eq!(diff["category"], "testing");
        assert_eq!(diff["reason"], "superseded by ADR 0009");
    }

    #[tokio::test]
    async fn archive_422s_on_missing_reason() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody { reason: None }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "reason_required");

        // No audit row, no row state change.
        let audit_count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(audit_count, 0);
        let got = crate::db::get_learning(&state.db, "L-1").unwrap();
        assert!(got.archived_at.is_none());
    }

    #[tokio::test]
    async fn archive_422s_on_whitespace_only_reason() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("   \n\t  ".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "reason_required");
    }

    #[tokio::test]
    async fn archive_410s_on_already_archived() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();
        db_archive(&state.db, "L-1", "first-reason").unwrap();

        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("second-reason".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 410);
        assert_eq!(body["error"], "already_archived");
        // Response carries the ORIGINAL archive metadata, not the new
        // attempt's reason.
        assert_eq!(body["archivedReason"], "first-reason");

        // No new audit row from the refusal.
        let audit_count: i64 = {
            let conn = state.db.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(audit_count, 0);
    }

    #[tokio::test]
    async fn archive_404s_on_unknown_id() {
        let state = test_state();
        let (status, body) = drive(
            archive_learning(
                State(state.clone()),
                Path("nope".to_string()),
                anon(),
                Json(ArchiveLearningBody {
                    reason: Some("doesn't matter".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "learning_not_found");
    }

    #[tokio::test]
    async fn archive_404s_on_cross_org() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "org-1", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let (status, _body) = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-2", "u2@example.com", "org-2"),
                Json(ArchiveLearningBody {
                    reason: Some("wrong-org".to_string()),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, 404, "cross-org archive must 404");

        // Row in org-1 must not have been flipped.
        let got = crate::db::get_learning(&state.db, "L-1").unwrap();
        assert!(got.archived_at.is_none(), "cross-org caller cannot flip");
    }

    #[tokio::test]
    async fn archive_records_user_attribution_when_authed() {
        let state = test_state();
        create_learning(
            &state.db,
            &input("L-1", "default-org", LearningKind::Feedback, "x", "a"),
        )
        .unwrap();

        let _ = drive(
            archive_learning(
                State(state.clone()),
                Path("L-1".to_string()),
                authed("u-7", "bob@example.com", "default-org"),
                Json(ArchiveLearningBody {
                    reason: Some("ok".to_string()),
                }),
            )
            .await,
        )
        .await;

        let (user_id, user_email): (Option<String>, Option<String>) = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT user_id, user_email FROM audit_logs WHERE resource_id = ?1",
                params!["L-1"],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(user_id.as_deref(), Some("u-7"));
        assert_eq!(user_email.as_deref(), Some("bob@example.com"));
    }

    // ── Phase 4.2 — promotion candidates + promote validation ────────────

    /// Seed a learning + a promotion_candidates row (optionally already
    /// promoted). No FK in the minimal test schema, so org rows are not
    /// required.
    fn seed_candidate(state: &AppState, id: &str, org_id: &str, promoted: Option<&str>) {
        create_learning(
            &state.db,
            &input(
                id,
                org_id,
                LearningKind::Feedback,
                "ci",
                &format!("slug-{id}"),
            ),
        )
        .unwrap();
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO promotion_candidates \
                (learning_id, org_id, hit_count, threshold, window_days, promoted_pr_url, promoted_at) \
             VALUES (?1, ?2, 5, 5, 30, ?3, ?4)",
            params![id, org_id, promoted, promoted.map(|_| "2026-01-01 00:00:00")],
        )
        .unwrap();
    }

    fn seed_project(
        state: &AppState,
        id: &str,
        org_id: &str,
        host: &str,
        repo_url: &str,
        default_cred: Option<&str>,
    ) {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, repo_url, host, default_credential_id, org_id) \
             VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
            params![id, repo_url, host, default_cred, org_id],
        )
        .unwrap();
    }

    #[test]
    fn humanize_slug_titlecases_first_word() {
        assert_eq!(
            humanize_slug("deploy-by-sha-not-edge"),
            "Deploy by sha not edge"
        );
        assert_eq!(humanize_slug("no_mocks_in_tests"), "No mocks in tests");
        assert_eq!(humanize_slug(""), "");
    }

    #[test]
    fn format_rule_block_wraps_body_in_h3() {
        let c = PromotionCandidate {
            learning_id: "L-1".into(),
            org_id: "default-org".into(),
            kind: "feedback".into(),
            category: "ci".into(),
            slug: "check-runner-after-deploy".into(),
            body_md: "**Why:** prod recreates kill the WS.".into(),
            hit_count: 5,
            threshold: 5,
            window_days: 30,
            created_at: "2026-01-01 00:00:00".into(),
            promoted_pr_url: None,
            promoted_at: None,
        };
        let block = format_rule_block(&c);
        assert!(block.starts_with("### Check runner after deploy\n\n"));
        assert!(block.contains("**Why:** prod recreates kill the WS."));
    }

    #[tokio::test]
    async fn candidates_list_carries_preview_and_promoted_url() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        seed_candidate(
            &state,
            "L-2",
            "default-org",
            Some("https://github.com/o/r/pull/3"),
        );
        seed_candidate(&state, "L-other", "other-org", None);

        let (status, body) =
            drive(list_promotion_candidates(State(state.clone()), anon()).await).await;
        assert_eq!(status, 200);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "org scoping excludes other-org: {body}");
        // Each row carries an appendPreview (the H3-wrapped rule).
        for it in items {
            assert!(
                it["appendPreview"]
                    .as_str()
                    .unwrap()
                    .starts_with("### Slug"),
                "appendPreview present: {it}"
            );
        }
        let promoted: Vec<&str> = items
            .iter()
            .filter(|i| !i["promotedPrUrl"].is_null())
            .map(|i| i["learningId"].as_str().unwrap())
            .collect();
        assert_eq!(promoted, vec!["L-2"]);
    }

    async fn promote(
        state: &AppState,
        id: &str,
        auth: OptionalAuthUser,
        project_id: Option<&str>,
        credential_id: Option<&str>,
    ) -> (u16, serde_json::Value) {
        drive(
            promote_learning(
                State(state.clone()),
                Path(id.to_string()),
                auth,
                Json(PromoteBody {
                    project_id: project_id.map(str::to_string),
                    credential_id: credential_id.map(str::to_string),
                }),
            )
            .await,
        )
        .await
    }

    #[tokio::test]
    async fn promote_404s_when_not_a_candidate() {
        let state = test_state();
        let (status, body) = promote(&state, "nope", anon(), Some("p-1"), None).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "not_a_candidate");
    }

    #[tokio::test]
    async fn promote_409s_when_already_promoted() {
        let state = test_state();
        seed_candidate(
            &state,
            "L-1",
            "default-org",
            Some("https://github.com/o/r/pull/9"),
        );
        seed_project(
            &state,
            "p-1",
            "default-org",
            "github",
            "https://github.com/o/r.git",
            Some("c-1"),
        );
        let (status, body) = promote(&state, "L-1", anon(), Some("p-1"), None).await;
        assert_eq!(status, 409);
        assert_eq!(body["error"], "already_promoted");
        assert_eq!(body["prUrl"], "https://github.com/o/r/pull/9");
    }

    #[tokio::test]
    async fn promote_400s_without_project_id() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        let (status, body) = promote(&state, "L-1", anon(), None, None).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "project_required");
    }

    #[tokio::test]
    async fn promote_404s_when_project_missing() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        let (status, body) = promote(&state, "L-1", anon(), Some("ghost"), None).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "project_not_found");
    }

    #[tokio::test]
    async fn promote_422s_on_unsupported_host() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        seed_project(
            &state,
            "p-1",
            "default-org",
            "gitlab",
            "https://gitlab.com/o/r.git",
            Some("c-1"),
        );
        let (status, body) = promote(&state, "L-1", anon(), Some("p-1"), None).await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "unsupported_host");
    }

    #[tokio::test]
    async fn promote_422s_when_no_credential_resolvable() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        // GitHub host, valid repo, but no default credential and none in
        // the body → credential_required (returns before any master-key
        // / network work).
        seed_project(
            &state,
            "p-1",
            "default-org",
            "github",
            "https://github.com/o/r.git",
            None,
        );
        let (status, body) = promote(&state, "L-1", anon(), Some("p-1"), None).await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "credential_required");
    }

    #[tokio::test]
    async fn promote_422s_on_unparseable_repo_url() {
        let state = test_state();
        seed_candidate(&state, "L-1", "default-org", None);
        seed_project(
            &state,
            "p-1",
            "default-org",
            "github",
            "not-a-url",
            Some("c-1"),
        );
        let (status, body) = promote(&state, "L-1", anon(), Some("p-1"), None).await;
        assert_eq!(status, 422);
        assert_eq!(body["error"], "unparseable_repo_url");
    }

    #[tokio::test]
    async fn promote_is_org_scoped_404() {
        // A candidate seeded in other-org is invisible to a default-org
        // caller — surfaces as not_a_candidate, never reaches the project.
        let state = test_state();
        seed_candidate(&state, "L-1", "other-org", None);
        let (status, body) = promote(&state, "L-1", anon(), Some("p-1"), None).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "not_a_candidate");
    }
}
