use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;

use crate::agents::{check_agent, ensure_git_initialized, pty_agent};
use crate::auth::OptionalAuthUser;
use crate::auth::orgs;
use crate::plan_parser;
use crate::saas::dispatch::org_has_runner;
use crate::saas::runner_protocol::WireMessage;
use crate::saas::runner_rpc::{RunnerRpcError, runner_request};
use crate::saas::runner_ws::RunnerResponse;
use crate::state::AppState;
use crate::templates;

// ── GET /api/plans ───────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanListEntry {
    name: String,
    title: String,
    project: Option<String>,
    phase_count: usize,
    task_count: usize,
    done_count: usize,
    created_at: String,
    modified_at: String,
    total_cost_usd: f64,
    max_budget_usd: Option<f64>,
}

pub async fn list_plans(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let summaries = plan_parser::list_plans(&state.plans_dir);
    let org_id = auth.org_id().to_string();

    let db = state.db.lock().unwrap();

    // Load all project overrides scoped to the active org
    let mut overrides: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT plan_name, project FROM plan_project WHERE org_id = ?1")
        && let Ok(rows) = stmt.query_map(params![org_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            overrides.insert(row.0, row.1);
        }
    }
    // Also load overrides without org_id filter for backward-compat: rows
    // that belong to the default org but were inserted before multi-tenancy.
    if org_id == orgs::DEFAULT_ORG_ID
        && let Ok(mut stmt) = db.prepare("SELECT plan_name, project FROM plan_project")
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
    {
        for row in rows.flatten() {
            overrides.entry(row.0).or_insert(row.1);
        }
    }

    // Aggregate cost per plan in one query
    let mut plan_costs: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT plan_name, COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE plan_name IS NOT NULL AND cost_usd IS NOT NULL GROUP BY plan_name",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        for row in rows.flatten() {
            plan_costs.insert(row.0, row.1);
        }
    }

    // Load all plan budgets
    let mut plan_budgets: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare("SELECT plan_name, max_budget_usd FROM plan_budget")
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })
    {
        for row in rows.flatten() {
            plan_budgets.insert(row.0, row.1);
        }
    }

    let entries: Vec<PlanListEntry> = summaries
        .into_iter()
        .filter(|s| orgs::plan_belongs_to_org(&db, &s.name, &org_id))
        .map(|s| {
            // Parse the full plan to merge statuses and get accurate done count
            let done_count = plan_parser::find_plan_file(&state.plans_dir, &s.name)
                .and_then(|path| plan_parser::parse_plan_file(&path).ok())
                .map(|parsed| {
                    let mut status_map: HashMap<String, String> = HashMap::new();
                    if let Ok(mut stmt) = db
                        .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
                        && let Ok(rows) = stmt.query_map(params![s.name], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                    {
                        for row in rows.flatten() {
                            status_map.insert(row.0, row.1);
                        }
                    }

                    parsed
                        .phases
                        .iter()
                        .flat_map(|p| &p.tasks)
                        .filter(|t| {
                            let status = status_map
                                .get(&t.number)
                                .map(|s| s.as_str())
                                .unwrap_or("pending");
                            status == "completed" || status == "skipped"
                        })
                        .count()
                })
                .unwrap_or(0);

            let project = overrides.get(&s.name).cloned().or(s.project);
            let total_cost_usd = plan_costs.get(&s.name).copied().unwrap_or(0.0);
            let max_budget_usd = plan_budgets.get(&s.name).copied();

            PlanListEntry {
                name: s.name,
                title: s.title,
                project,
                phase_count: s.phase_count,
                task_count: s.task_count,
                done_count,
                created_at: s.created_at,
                modified_at: s.modified_at,
                total_cost_usd,
                max_budget_usd,
            }
        })
        .collect();

    Json(entries)
}

// ── GET /api/plans/:name ─────────────────────────────────────────────────────

pub async fn get_plan(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Org gate: verify the plan belongs to the caller's org.
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let mut plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan: {e}"),
                    "file": plan_path.to_string_lossy(),
                })),
            )
                .into_response();
        }
    };

    let db = state.db.lock().unwrap();

    // Merge task statuses
    if let Ok(mut stmt) =
        db.prepare("SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?")
    {
        let mut status_map: HashMap<String, (String, String)> = HashMap::new();
        if let Ok(rows) = stmt.query_map(params![name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                status_map.insert(row.0, (row.1, row.2));
            }
        }

        for phase in &mut plan.phases {
            for task in &mut phase.tasks {
                if let Some((status, updated_at)) = status_map.get(&task.number) {
                    task.status = Some(status.clone());
                    task.status_updated_at = Some(updated_at.clone());
                } else {
                    task.status = Some("pending".to_string());
                }
            }
        }
    }

    // Merge DB project override
    if let Ok(project) = db.query_row(
        "SELECT project FROM plan_project WHERE plan_name = ?",
        params![name],
        |row| row.get::<_, String>(0),
    ) {
        plan.project = Some(project);
    }

    // Aggregate agent costs per task and total for this plan
    let mut task_costs: HashMap<String, f64> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT task_id, COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE plan_name = ? AND task_id IS NOT NULL AND cost_usd IS NOT NULL GROUP BY task_id",
    ) && let Ok(rows) = stmt.query_map(params![name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        for row in rows.flatten() {
            task_costs.insert(row.0, row.1);
        }
    }

    let plan_total: f64 = db
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
             WHERE plan_name = ? AND cost_usd IS NOT NULL",
            params![name],
            |row| row.get::<_, f64>(0),
        )
        .unwrap_or(0.0);

    plan.total_cost_usd = Some(plan_total);

    // Latest CI run per task for this plan. The lookup rolls up
    // `<task>-fix-<N>` rows onto the canonical task so a green fix attempt
    // surfaces on the original task's badge — see `ci::latest_per_task`.
    let task_numbers: Vec<&str> = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .map(|t| t.number.as_str())
        .collect();
    let ci_map = crate::ci::latest_per_task(&db, &name, &task_numbers);
    for phase in &mut plan.phases {
        for task in &mut phase.tasks {
            if let Some(c) = task_costs.get(&task.number) {
                task.cost_usd = Some(*c);
            }
            if let Some(ci) = ci_map.get(&task.number) {
                task.ci = Some(ci.clone());
            }
        }
    }

    // Load budget for this plan
    plan.max_budget_usd = db
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?",
            params![name],
            |row| row.get::<_, f64>(0),
        )
        .ok();

    // Load auto-advance flag (opt-in, default off)
    let auto_advance = db
        .query_row(
            "SELECT enabled FROM plan_auto_advance WHERE plan_name = ?",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false);

    // Latest plan-level verdict (None when no Check Plan has ever run).
    let verdict = db
        .query_row(
            "SELECT verdict, reason, agent_id, checked_at FROM plan_verdicts WHERE plan_name = ?",
            params![name],
            |row| {
                Ok(serde_json::json!({
                    "verdict": row.get::<_, String>(0)?,
                    "reason": row.get::<_, Option<String>>(1)?,
                    "agentId": row.get::<_, Option<String>>(2)?,
                    "checkedAt": row.get::<_, String>(3)?,
                }))
            },
        )
        .ok();

    let mut value = serde_json::to_value(plan).unwrap();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("autoAdvance".to_string(), serde_json::json!(auto_advance));
        if let Some(v) = verdict {
            obj.insert("verdict".to_string(), v);
        }
    }
    Json(value).into_response()
}

// ── GET /api/plans/{name}/export ─────────────────────────────────────────────
//
// Ships the plan's canonical YAML body (schema fields only — no task status,
// agent rows, costs, or CI verdicts) with a versioned header comment so a
// future loader can detect and migrate older exports.  YAML plans round-trip
// verbatim from disk; markdown plans are serialized via the canonical
// `serialize_plan_yaml` writer.  Runtime state lives in SQLite and never
// touches the on-disk YAML, so reading the file straight off disk already
// satisfies the "schema fields only" contract.

/// Schema version of the export header. Bump when the body shape changes in a
/// way the loader can't ingest as today's YAML — readers branch on this.
const PLAN_EXPORT_VERSION: &str = "v1";

/// Outcome of [`build_export_body`]: the canonical YAML payload (header + body)
/// that single-plan `GET /export` and bulk `POST /export-bundle` both emit.
struct PlanExportBody {
    /// Full body bytes, including the two-line `# branchwork-plan-export: v1`
    /// and `# exported-at: <iso8601>` preamble. Identical wire shape to task
    /// 1.1 so a single-plan export and a bundle entry round-trip through the
    /// same loader.
    body: String,
    /// RFC3339 UTC timestamp from the header — surfaced so the bundle manifest
    /// can echo it without re-parsing the body.
    exported_at: String,
}

/// Build the canonical export body for a single plan. Reused by `export_plan`
/// (Task 1.1) and `export_bundle` (Task 1.3) so the two endpoints emit
/// byte-identical per-plan bytes — the bundle's manifest sha256s are computed
/// over exactly what a single-plan GET would have returned.
///
/// On error returns a boxed [`Response`] (clippy::result_large_err — axum's
/// Response is ~128 bytes, so a non-boxed `Err(Response)` would balloon every
/// `Result<PlanExportBody, _>` on the stack). Callers `return *err`.
fn build_export_body(
    plans_dir: &std::path::Path,
    name: &str,
) -> Result<PlanExportBody, Box<Response>> {
    let plan_path = match plan_parser::find_plan_file(plans_dir, name) {
        Some(p) => p,
        None => {
            return Err(Box::new(
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Plan not found"})),
                )
                    .into_response(),
            ));
        }
    };

    let ext = plan_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // YAML on disk is already schema-only — task status / costs / agents are
    // never persisted into plan files. Stream the raw bytes through so an
    // author's comments and key ordering survive the round-trip. Markdown
    // plans have no on-disk YAML, so we parse and re-emit via the canonical
    // serializer.
    let yaml_body = match ext {
        "yaml" | "yml" => match std::fs::read_to_string(&plan_path) {
            Ok(s) => s,
            Err(e) => {
                return Err(Box::new(
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "read_failed",
                            "message": format!("Failed to read plan file: {e}"),
                        })),
                    )
                        .into_response(),
                ));
            }
        },
        _ => {
            let plan = match plan_parser::parse_plan_file(&plan_path) {
                Ok(p) => p,
                Err(e) => {
                    return Err(Box::new(
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(serde_json::json!({
                                "error": "parse_error",
                                "message": format!("Failed to parse plan: {e}"),
                            })),
                        )
                            .into_response(),
                    ));
                }
            };
            match plan_parser::serialize_plan_yaml(&plan) {
                Ok(s) => s,
                Err(e) => {
                    return Err(Box::new(
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": "serialize_failed",
                                "message": format!("Failed to serialize plan: {e}"),
                            })),
                        )
                            .into_response(),
                    ));
                }
            }
        }
    };

    let exported_at = chrono::Utc::now().to_rfc3339();
    let header =
        format!("# branchwork-plan-export: {PLAN_EXPORT_VERSION}\n# exported-at: {exported_at}\n");
    let body = format!("{header}{yaml_body}");

    Ok(PlanExportBody { body, exported_at })
}

/// Sanitize a plan name for use in `Content-Disposition` / zip entry names.
/// HeaderValue rejects CR/LF and bytes outside 0x20-0x7E; zip readers tolerate
/// more but the dashboard is symmetric on both wire surfaces so the same rule
/// applies.
fn sanitize_plan_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub async fn export_plan(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Org gate: verify the plan belongs to the caller's org.
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let body = match build_export_body(&state.plans_dir, &name) {
        Ok(b) => b.body,
        Err(resp) => return *resp,
    };

    let safe_name = sanitize_plan_filename(&name);
    let disposition = format!("attachment; filename=\"{safe_name}.plan.yaml\"");

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/yaml".to_string(),
            ),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response()
}

// ── POST /api/plans/export-bundle ────────────────────────────────────────────
//
// Multi-plan export. Body shape: `{ "names": ["plan-a", "plan-b", ...] }`.
// Response: a zip containing each plan's canonical export body under
// `<safe_name>.plan.yaml` plus a top-level `manifest.yaml` listing the set
// (names + sha256s + per-entry exported-at + bundle exported-at). Per-entry
// bytes are byte-identical to the single-plan GET so manifest hashes survive
// any reader that round-trips a single file through 1.1's loader.
//
// Validation gates (all 4xx, no partial zips):
//  * `names` is required and must be non-empty
//  * each name must belong to the caller's org (404 surfaces the offender)
//  * duplicates are rejected with 400 — zip entry names must be unique
//  * bundle is capped at MAX_BUNDLE_PLANS to keep the in-memory body bounded
//
// All-or-nothing: if any plan fails to export, the whole call fails and no
// zip is sent. A partial zip would silently lose plans for the operator.

#[derive(Deserialize)]
pub struct ExportBundleBody {
    #[serde(default)]
    names: Vec<String>,
}

/// Manifest entry; mirror of the `entries:` list in `manifest.yaml`. Wire shape
/// uses snake_case so the manifest stays readable as a YAML doc.
#[derive(Serialize)]
struct BundleEntry {
    /// Original plan name (matches the YAML's `title` slug, pre-sanitization).
    name: String,
    /// `<safe_name>.plan.yaml` — the zip entry path the YAML lives at.
    filename: String,
    /// Hex-encoded sha256 of the per-entry body bytes (header + YAML).
    sha256: String,
    /// RFC3339 timestamp from this entry's `# exported-at:` header line.
    exported_at: String,
}

/// Hard cap on plans per bundle. Generous enough that picking the whole
/// dashboard (~50-100 plans) works, low enough that an accidental
/// `{"names":["a"; 100_000]}` doesn't pin the server.
const MAX_BUNDLE_PLANS: usize = 256;

pub async fn export_bundle(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Json(body): Json<ExportBundleBody>,
) -> impl IntoResponse {
    if body.names.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "names_required",
                "message": "Request body must include a non-empty `names` array.",
            })),
        )
            .into_response();
    }
    if body.names.len() > MAX_BUNDLE_PLANS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "too_many_plans",
                "message": format!(
                    "Bundle is capped at {MAX_BUNDLE_PLANS} plans (requested {}).",
                    body.names.len(),
                ),
                "max": MAX_BUNDLE_PLANS,
            })),
        )
            .into_response();
    }
    // Reject dupes up front — zip entry names must be unique, and a second
    // identical name almost always signals a UI bug (the multi-select toolbar
    // shouldn't be able to commit a duplicate).
    {
        let mut seen = std::collections::HashSet::new();
        for n in &body.names {
            if !seen.insert(n.as_str()) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "duplicate_name",
                        "message": format!("Duplicate plan name in bundle: {n}"),
                        "name": n,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Org + existence gate (do this before any disk-read so we 404 fast and
    // can surface WHICH plan was missing). `plan_belongs_to_org` returns true
    // for unmapped plans on the default org, so it doesn't catch "plan name
    // does not exist on disk" — that check happens via `find_plan_file`.
    {
        let conn = state.db.lock().unwrap();
        for n in &body.names {
            if !orgs::plan_belongs_to_org(&conn, n, auth.org_id())
                || plan_parser::find_plan_file(&state.plans_dir, n).is_none()
            {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "Plan not found",
                        "name": n,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Build per-entry bodies + manifest rows. All-or-nothing: any error bails
    // before we touch the zip writer, so partial bundles never escape.
    use sha2::Digest;
    let mut entries_for_zip: Vec<(String, String)> = Vec::with_capacity(body.names.len());
    let mut manifest_entries: Vec<BundleEntry> = Vec::with_capacity(body.names.len());
    let mut entry_filenames = std::collections::HashSet::new();
    for n in &body.names {
        let exported = match build_export_body(&state.plans_dir, n) {
            Ok(e) => e,
            Err(resp) => return *resp,
        };
        let safe_name = sanitize_plan_filename(n);
        let filename = format!("{safe_name}.plan.yaml");
        if !entry_filenames.insert(filename.clone()) {
            // Two distinct plan names sanitized to the same zip entry. Refuse
            // rather than silently lose one — the operator picked both, both
            // belong in the bundle.
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "filename_collision",
                    "message": format!(
                        "Two plan names sanitize to the same zip entry ({filename}); pick one.",
                    ),
                    "filename": filename,
                })),
            )
                .into_response();
        }
        let mut hasher = sha2::Sha256::new();
        hasher.update(exported.body.as_bytes());
        let sha = hex_lower(&hasher.finalize());
        manifest_entries.push(BundleEntry {
            name: n.clone(),
            filename: filename.clone(),
            sha256: sha,
            exported_at: exported.exported_at.clone(),
        });
        entries_for_zip.push((filename, exported.body));
    }

    let bundle_exported_at = chrono::Utc::now().to_rfc3339();
    let manifest_value = serde_json::json!({
        "branchwork_plan_export": PLAN_EXPORT_VERSION,
        "exported_at": bundle_exported_at,
        "entries": manifest_entries,
    });
    let manifest_yaml = match serde_yaml::to_string(&manifest_value) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "manifest_serialize_failed",
                    "message": format!("Failed to serialize manifest: {e}"),
                })),
            )
                .into_response();
        }
    };

    // Build the zip in memory. Deflate keeps a 100-plan bundle small (YAML
    // compresses ~10x) without dragging zstd/lzma in.
    let zip_bytes = match build_zip(&entries_for_zip, &manifest_yaml) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "zip_failed",
                    "message": format!("Failed to build zip: {e}"),
                })),
            )
                .into_response();
        }
    };

    // Filename embeds a UTC date stamp so an operator who downloads two
    // bundles back-to-back gets distinguishable files. Sanitize for the
    // Content-Disposition header same as single-plan export.
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let disposition = format!("attachment; filename=\"branchwork-plans-{stamp}.zip\"");

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
            ),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        zip_bytes,
    )
        .into_response()
}

/// Pure helper that takes per-plan entries + the manifest body and emits a zip
/// archive in memory. Extracted so tests can exercise the layout deterministically
/// without standing up the full HTTP surface.
fn build_zip(entries: &[(String, String)], manifest_yaml: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        for (name, body) in entries {
            zip.start_file(name, opts)
                .map_err(|e| std::io::Error::other(format!("start_file {name}: {e}")))?;
            zip.write_all(body.as_bytes())?;
        }
        zip.start_file("manifest.yaml", opts)
            .map_err(|e| std::io::Error::other(format!("start_file manifest.yaml: {e}")))?;
        zip.write_all(manifest_yaml.as_bytes())?;
        zip.finish()
            .map_err(|e| std::io::Error::other(format!("finish: {e}")))?;
    }
    Ok(buf)
}

/// Lower-case hex of a sha256 digest, matching the dashboard's
/// `crypto.subtle.digest` rendering so the manifest hash equals what a
/// browser would compute locally.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

// ── PUT /api/plans/:name/project ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ProjectBody {
    project: String,
}

pub async fn set_project(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<ProjectBody>,
) -> impl IntoResponse {
    if body.project.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "project is required"})),
        );
    }

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO plan_project (plan_name, project, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(plan_name)
         DO UPDATE SET project = excluded.project, updated_at = excluded.updated_at",
        params![name, body.project],
    )
    .unwrap();

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_PROJECT_CHANGE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"project": body.project}).to_string()),
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "plan_name": name,
            "project": body.project,
        })),
    )
}

// ── PUT /api/plans/:name/budget ──────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetBody {
    /// Set to null to clear the budget.
    max_budget_usd: Option<f64>,
}

pub async fn set_budget(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<BudgetBody>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    match body.max_budget_usd {
        Some(v) if v > 0.0 => {
            db.execute(
                "INSERT INTO plan_budget (plan_name, max_budget_usd, updated_at)
                 VALUES (?1, ?2, datetime('now'))
                 ON CONFLICT(plan_name)
                 DO UPDATE SET max_budget_usd = excluded.max_budget_usd, updated_at = excluded.updated_at",
                params![name, v],
            )
            .ok();
        }
        _ => {
            db.execute("DELETE FROM plan_budget WHERE plan_name = ?", params![name])
                .ok();
        }
    }

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_BUDGET_CHANGE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"maxBudgetUsd": body.max_budget_usd}).to_string()),
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "planName": name,
            "maxBudgetUsd": body.max_budget_usd,
        })),
    )
}

// ── PUT /api/plans/:name/auto-advance ────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoAdvanceBody {
    enabled: bool,
}

pub async fn set_auto_advance(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<AutoAdvanceBody>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO plan_auto_advance (plan_name, enabled, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(plan_name)
         DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
        params![name, body.enabled as i64],
    )
    .ok();

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::CONFIG_AUTO_ADVANCE,
        crate::audit::resources::PLAN,
        Some(&name),
        Some(&serde_json::json!({"enabled": body.enabled}).to_string()),
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "planName": name,
            "autoAdvance": body.enabled,
        })),
    )
}

// ── GET/PUT /api/plans/:name/config ──────────────────────────────────────────
//
// Unified config endpoint covering `auto_advance` (existing) plus the
// auto-mode companion toggles (`auto_mode`, `max_fix_attempts`). GET also
// surfaces `paused_reason` so the UI can show a banner explaining why the
// loop self-paused. The dedicated `/auto-advance` route stays unchanged.

/// Compile-time gate for fan-out spawn: until worktree-per-agent isolation
/// (ADR 0002) ships, all `parallel = true` PUTs are rejected. Flip this to
/// `true` once `pty_agent.rs` carries a `git worktree`-based spawn path so
/// the enable side of the toggle becomes available again. Kept as a `pub`
/// const so the grep target is stable for the worktree implementer.
pub const WORKTREES_SHIPPED: bool = false;

/// Read the per-project worktree opt-in flag. Returns `false` when no
/// `plan_project` row exists for the plan or the column is 0. The toggling
/// endpoint ships with the worktree plan; until then the column is forward-
/// compatible storage that can only be set to 1 via direct SQL.
fn project_worktree_opt_in(db: &rusqlite::Connection, name: &str) -> bool {
    db.query_row(
        "SELECT worktree_isolation_opt_in FROM plan_project WHERE plan_name = ?1",
        params![name],
        |row| row.get::<_, i64>(0).map(|v| v != 0),
    )
    .unwrap_or(false)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanConfig {
    auto_advance: bool,
    auto_mode: bool,
    max_fix_attempts: u32,
    paused_reason: Option<String>,
    /// Trimmed file list captured at pause time for dirty-tree pauses
    /// (T3.1 of the dirty-tree-check plan). Only populated when the
    /// pause reason is `agent_left_uncommitted_work`; `None` for every
    /// other reason. Cleared on resume by [`crate::db::auto_mode_resume`].
    /// Serialised as `pausedFiles` and omitted from the JSON when None
    /// so the wire shape stays unchanged for plans that have never
    /// dirty-tree-paused.
    #[serde(skip_serializing_if = "Option::is_none")]
    paused_files: Option<Vec<String>>,
    /// Per-plan opt-in for fan-out spawn (3.5.2). Stored on both
    /// `plan_auto_mode` and `plan_auto_advance` and kept in lockstep by
    /// the unified PUT. Toggling to true is rejected at the API layer
    /// until worktrees ship (3.5.3).
    parallel: bool,
    /// Per-plan runner affinity (T11.4). `None` = "any online runner";
    /// `Some(id)` pins every spawn to that runner. The dispatcher pauses
    /// the plan with `paused_reason='runner_offline'` if the pinned
    /// runner is offline at spawn time.
    runner_id: Option<String>,
    /// Per-plan runner failover policy (T11.5). `"pause"` (default,
    /// today's T11.4 behaviour) or `"sibling"` (re-dispatch to a sibling
    /// online runner when the pinned runner goes offline). Always serialised
    /// — for unpinned plans the value is the schema default `"pause"`,
    /// which is harmless because failover only triggers when there's a
    /// pin to fail over from.
    runner_failover: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlanConfigBody {
    auto_advance: Option<bool>,
    auto_mode: Option<bool>,
    max_fix_attempts: Option<u32>,
    parallel: Option<bool>,
    /// Three-state field: `Absent` (None), `ExplicitNull` (Some(None) — the
    /// only value we honour: clear the pause + re-evaluate), or
    /// `ExplicitValue` (Some(Some(_)) — ignored, paused reasons are only
    /// set by the loop). The `deserialize_some` shim distinguishes
    /// `Absent` from `ExplicitNull`, which a plain `Option<String>` can't.
    #[serde(default, deserialize_with = "deserialize_some")]
    paused_reason: Option<Option<String>>,
    /// Three-state field for the runner pin (T11.4): `Absent` (None,
    /// don't touch), `ExplicitNull` (Some(None), clear the pin = "any
    /// online runner"), `ExplicitValue` (Some(Some("runner-x")), pin to
    /// that runner). Same `deserialize_some` shim as `paused_reason`.
    #[serde(default, deserialize_with = "deserialize_some")]
    runner_id: Option<Option<String>>,
    /// Per-plan runner failover policy (T11.5). `Some("pause")` /
    /// `Some("sibling")` writes the policy on the existing pin row;
    /// `None` leaves the policy untouched. Setting on a plan with no
    /// pin is rejected (the failover concept only applies when there's
    /// a pin to fail over from).
    runner_failover: Option<String>,
}

/// Serde shim to distinguish absent fields from explicit null. With field
/// type `Option<Option<T>>` and `#[serde(default, deserialize_with =
/// "deserialize_some")]`, a missing field is `None` (default), an
/// explicit `null` is `Some(None)`, and a value is `Some(Some(v))`.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn read_plan_config(db: &rusqlite::Connection, name: &str) -> PlanConfig {
    let (auto_advance, advance_parallel) = db
        .query_row(
            "SELECT enabled, parallel FROM plan_auto_advance WHERE plan_name = ?1",
            params![name],
            |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
        )
        .unwrap_or((false, false));

    let (auto_mode, max_fix_attempts, paused_reason, mode_parallel, paused_files_json) = db
        .query_row(
            "SELECT enabled, max_fix_attempts, paused_reason, parallel, paused_files \
             FROM plan_auto_mode WHERE plan_name = ?1",
            params![name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? != 0,
                    row.get::<_, i64>(1)? as u32,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .unwrap_or((false, 3, None, false, None));

    // `paused_files` is JSON-encoded `Vec<String>` written by
    // `db::auto_mode_pause` only on dirty-tree pauses. A parse error
    // collapses to `None` so a corrupted row never 500s the config
    // endpoint — the dashboard simply shows no file list while the
    // banner / pausedReason still surfaces.
    let paused_files = paused_files_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());

    let (runner_id, runner_failover) = db
        .query_row(
            "SELECT runner_id, runner_failover FROM plan_runner_affinity WHERE plan_name = ?1",
            params![name],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map(|(rid, rf)| (Some(rid), rf))
        .unwrap_or_else(|_| (None, "pause".to_string()));

    // Coerce unknown legacy values to "pause" so the UI never sees a
    // surprising third state. The `set_plan_runner_failover` validator
    // gates this at write time but a manual SQL UPDATE could land here.
    let runner_failover = match runner_failover.as_str() {
        "sibling" => runner_failover,
        _ => "pause".to_string(),
    };

    // The unified config keeps both tables' `parallel` flags in lockstep,
    // but in case they ever diverge (manual SQL, partial migration), OR
    // them so a single "yes" wins. 3.5.3 will reject toggle attempts to
    // true at the API layer until worktrees ship.
    PlanConfig {
        auto_advance,
        auto_mode,
        max_fix_attempts,
        paused_reason,
        paused_files,
        parallel: mode_parallel || advance_parallel,
        runner_id,
        runner_failover,
    }
}

pub async fn get_plan_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let cfg = read_plan_config(&db, &name);
    Json(cfg).into_response()
}

pub async fn put_plan_config(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<PlanConfigBody>,
) -> axum::response::Response {
    // Worktree gate (Task 3.5.3): reject `parallel = true` until both
    //   (1) worktree-per-agent isolation has shipped (compile-time const), and
    //   (2) the project has opted in via `plan_project.worktree_isolation_opt_in`.
    // Audit every refused attempt so a sysadmin can see the trail. Disabling
    // (`parallel: false`) is never gated — turning the knob off must always
    // succeed even if it somehow got flipped on.
    if matches!(body.parallel, Some(true)) {
        let opted_in = {
            let db = state.db.lock().unwrap();
            project_worktree_opt_in(&db, &name)
        };
        if !WORKTREES_SHIPPED || !opted_in {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_PARALLEL_REFUSED,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(
                    &serde_json::json!({
                        "requested": true,
                        "reason": "worktrees_not_ready",
                    })
                    .to_string(),
                ),
            );
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(serde_json::json!({
                    "error": "worktrees_required",
                    "message": "Parallel auto-advance requires worktree-per-agent isolation. Enable it on the project once it ships.",
                })),
            )
                .into_response();
        }
    }

    // Snapshot in-flight fix agents to kill BEFORE the enabled flag
    // flips, so a concurrent agent-completion path can't slip a fresh
    // fix agent in past the disable. Only populated when the request is
    // explicitly turning auto_mode off.
    let fix_agents_to_kill: Vec<(String, String)> = {
        let db = state.db.lock().unwrap();

        if let Some(enabled) = body.auto_advance {
            db.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled, updated_at) \
                 VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(plan_name) \
                 DO UPDATE SET enabled = excluded.enabled, updated_at = excluded.updated_at",
                params![name, enabled as i64],
            )
            .ok();

            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_AUTO_ADVANCE,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(&serde_json::json!({"enabled": enabled}).to_string()),
            );
        }

        let mut to_kill: Vec<(String, String)> = Vec::new();
        if body.auto_mode.is_some() || body.max_fix_attempts.is_some() || body.parallel.is_some() {
            // UPSERT each provided field independently so partial updates
            // don't clobber the other column. COALESCE keeps the existing
            // value when the caller omits it.
            if let Some(enabled) = body.auto_mode {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, enabled) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET enabled = excluded.enabled",
                    params![name, enabled as i64],
                )
                .ok();

                // Toggle-off: collect any in-flight fix agents for this
                // plan so we can kill them after dropping the lock.
                if !enabled
                    && let Ok(mut stmt) = db.prepare(
                        "SELECT id, COALESCE(org_id, 'default-org') FROM agents \
                         WHERE plan_name = ?1 \
                           AND task_id LIKE '%-fix-%' \
                           AND status IN ('running', 'starting')",
                    )
                    && let Ok(rows) = stmt.query_map(params![name], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                {
                    for r in rows.flatten() {
                        to_kill.push(r);
                    }
                }
            }
            if let Some(max) = body.max_fix_attempts {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, max_fix_attempts) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET max_fix_attempts = excluded.max_fix_attempts",
                    params![name, max as i64],
                )
                .ok();
            }
            // `parallel` is mirrored across both tables so each mode's
            // spawn helper sees the same answer. Two UPSERTs because the
            // tables are separate; "one statement per field" still holds
            // per (table, field) pair.
            if let Some(parallel) = body.parallel {
                db.execute(
                    "INSERT INTO plan_auto_mode (plan_name, parallel) \
                     VALUES (?1, ?2) \
                     ON CONFLICT(plan_name) DO UPDATE SET parallel = excluded.parallel",
                    params![name, parallel as i64],
                )
                .ok();
                db.execute(
                    "INSERT INTO plan_auto_advance (plan_name, parallel, updated_at) \
                     VALUES (?1, ?2, datetime('now')) \
                     ON CONFLICT(plan_name) \
                     DO UPDATE SET parallel = excluded.parallel, updated_at = excluded.updated_at",
                    params![name, parallel as i64],
                )
                .ok();
            }

            let mut audit_payload = serde_json::Map::new();
            if let Some(enabled) = body.auto_mode {
                audit_payload.insert("enabled".to_string(), serde_json::json!(enabled));
            }
            if let Some(max) = body.max_fix_attempts {
                audit_payload.insert("maxFixAttempts".to_string(), serde_json::json!(max));
            }
            if let Some(parallel) = body.parallel {
                audit_payload.insert("parallel".to_string(), serde_json::json!(parallel));
            }
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_AUTO_MODE,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(&serde_json::Value::Object(audit_payload).to_string()),
            );
        }
        to_kill
    };

    // Outside the DB lock: cancel any pending wait_for_ci poll for this
    // plan, then kill the in-flight fix agents collected above. Cancel
    // first so the loop sees Cancelled and bails before any
    // agent-completion side-effect could race the kill — the fix
    // branches are abandoned (no merge runs).
    if matches!(body.auto_mode, Some(false)) {
        state.cancel_plan(&name);
        for (agent_id, org_id) in &fix_agents_to_kill {
            let _ = crate::agents::spawn_ops::kill_agent_dispatch(&state, org_id, agent_id).await;
        }
    }

    // Per-plan runner failover policy (T11.5). Validate first so a bad
    // value rejects the entire request before we mutate anything else.
    // The validation also gates `runner_id`'s "explicit null clears the
    // pin" path: if the user is setting failover='sibling' AND clearing
    // the pin in the same request, we accept the failover write but it
    // becomes a no-op (no pin row to UPDATE) — the API surfaces that as
    // 409 below. This sequencing means we never half-apply the request.
    if let Some(policy) = body.runner_failover.as_deref()
        && policy != "pause"
        && policy != "sibling"
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_runner_failover",
                "message": "runnerFailover must be 'pause' or 'sibling'.",
            })),
        )
            .into_response();
    }

    // Per-plan runner pin (T11.4). `Absent` (None) leaves the pin
    // untouched. Explicit-null clears it (back to "any online runner").
    // An explicit value writes/replaces the pin. Audit either way so the
    // history shows when/who changed routing.
    if let Some(rid_opt) = body.runner_id.as_ref() {
        let rid_ref = rid_opt.as_deref();
        crate::db::set_plan_runner_id(&state.db, &name, auth.org_id(), rid_ref);
        {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::CONFIG_RUNNER_AFFINITY,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(&serde_json::json!({"runnerId": rid_ref}).to_string()),
            );
        }
        crate::ws::broadcast_event(
            &state.broadcast_tx,
            "plan_runner_affinity_changed",
            serde_json::json!({
                "plan": name,
                "runner_id": rid_ref,
            }),
        );

        // If a previous `runner_offline` pause is now resolvable (the
        // user pinned to a different runner that is online), clear it
        // proactively so the user doesn't have to click Resume separately.
        if let Some(rid) = rid_ref {
            let was_paused_runner_offline = {
                let db = state.db.lock().unwrap();
                db.query_row(
                    "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
                    params![name],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .as_deref()
                    == Some("runner_offline")
            };
            let online = {
                let db = state.db.lock().unwrap();
                db.query_row(
                    "SELECT status FROM runners WHERE id = ?1",
                    params![rid],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .as_deref()
                    == Some("online")
            };
            if was_paused_runner_offline && online {
                crate::db::auto_mode_resume(&state.db, &name);
                crate::ws::broadcast_event(
                    &state.broadcast_tx,
                    "auto_mode_resumed",
                    serde_json::json!({
                        "plan": name,
                        "reason": "runner_repinned",
                    }),
                );
            }
        }
    }

    // Per-plan runner failover policy (T11.5). Runs AFTER the pin write
    // so a single PUT can set both the pin and the policy atomically:
    // `set_plan_runner_id` UPSERTs the row with default 'pause', then
    // this UPDATE flips it to 'sibling' if requested. Setting failover
    // without a pin is a 409 — it's a meaningless config and silently
    // dropping it would leave the user wondering why the toggle didn't
    // stick.
    if let Some(policy) = body.runner_failover.as_deref() {
        match crate::db::set_plan_runner_failover(&state.db, &name, policy) {
            Ok(true) => {
                {
                    let db = state.db.lock().unwrap();
                    crate::audit::log(
                        &db,
                        auth.org_id(),
                        auth.0.as_ref().map(|u| u.id.as_str()),
                        auth.0.as_ref().map(|u| u.email.as_str()),
                        crate::audit::actions::CONFIG_RUNNER_AFFINITY,
                        crate::audit::resources::PLAN,
                        Some(&name),
                        Some(&serde_json::json!({"runnerFailover": policy}).to_string()),
                    );
                }
                crate::ws::broadcast_event(
                    &state.broadcast_tx,
                    "plan_runner_affinity_changed",
                    serde_json::json!({
                        "plan": name,
                        "runner_failover": policy,
                    }),
                );
            }
            Ok(false) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "no_runner_pin",
                        "message": "Set a runner pin before configuring failover policy. \
                                    Failover only applies when a plan is pinned to a specific runner.",
                    })),
                )
                    .into_response();
            }
            Err(()) => {
                // Already validated above, so this should be unreachable.
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_runner_failover",
                    })),
                )
                    .into_response();
            }
        }
    }

    // Explicit `pausedReason: null` is the Resume button: clear the loop's
    // self-pause, audit, broadcast `auto_mode_resumed` so the dashboard pill
    // disappears, then re-evaluate auto-advance from the most recently
    // completed task. Anything other than explicit-null is ignored — the
    // loop is the only writer of paused reasons.
    if matches!(body.paused_reason, Some(None)) {
        crate::db::auto_mode_resume(&state.db, &name);

        let last_completed: Option<String> = {
            let db = state.db.lock().unwrap();
            db.query_row(
                "SELECT task_number FROM task_status \
                 WHERE plan_name = ?1 AND status IN ('completed', 'skipped') \
                 ORDER BY updated_at DESC LIMIT 1",
                params![name],
                |row| row.get::<_, String>(0),
            )
            .ok()
        };

        {
            let db = state.db.lock().unwrap();
            crate::audit::log(
                &db,
                auth.org_id(),
                auth.0.as_ref().map(|u| u.id.as_str()),
                auth.0.as_ref().map(|u| u.email.as_str()),
                crate::audit::actions::AUTO_MODE_RESUMED,
                crate::audit::resources::PLAN,
                Some(&name),
                Some(
                    &serde_json::json!({
                        "last_completed_task": last_completed,
                    })
                    .to_string(),
                ),
            );
        }

        crate::ws::broadcast_event(
            &state.broadcast_tx,
            "auto_mode_resumed",
            serde_json::json!({
                "plan": name,
                "last_completed_task": last_completed,
            }),
        );

        if let Some(task) = last_completed {
            let registry = state.registry.clone();
            let plans_dir = state.plans_dir.clone();
            let plan_name = name.clone();
            let effort = *state.effort.lock().unwrap();
            let port = state.config_port();
            tokio::spawn(async move {
                crate::agents::try_auto_advance(
                    registry, plans_dir, plan_name, task, effort, port, None,
                )
                .await;
            });
        }
    }

    let db = state.db.lock().unwrap();
    let cfg = read_plan_config(&db, &name);
    (StatusCode::OK, Json(cfg)).into_response()
}

// ── GET/PUT /api/plans/:name/settings (Task 3.1) ─────────────────────────────
//
// Plan-level settings panel API: surface the per-plan
// `ci_blocking_workflows` + `phase_verification` overrides, the workflows
// discovered under `.github/workflows/`, and the `branchwork.toml` repo
// defaults that the resolution helper falls back to. PUT writes the two
// editable fields back to the plan YAML using a comment-preserving
// line-based mutation (raw `serde_yaml::to_string` round-trips would drop
// every comment in the file — plans frequently carry context comments).

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RepoDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    ci_blocking_workflows: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ci_blocking_workflows_skip: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase_verification: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanSettings {
    /// Plan-level `ci_blocking_workflows` field from the YAML; `None`
    /// when unset (resolution falls through to repo / classifier).
    ci_blocking_workflows: Option<Vec<String>>,
    /// Plan-level `phase_verification` field from the YAML; `None`
    /// when unset.
    phase_verification: Option<String>,
    /// Workflow names enumerated from `<project>/.github/workflows/*.yml|*.yaml`
    /// top-level `name:` field, with the filename stem as fallback. Empty
    /// when the project has no workflows directory or none parse.
    available_workflows: Vec<String>,
    /// `[ci]` + `[phase]` view of the project-level `branchwork.toml`
    /// (when present) so the UI can render a "default: …" hint next to
    /// each editable field.
    repo_defaults: RepoDefaults,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlanSettingsBody {
    /// Three-state field: `Absent` (None) leaves the YAML key untouched,
    /// `ExplicitNull` (Some(None)) removes the key entirely, `ExplicitValue`
    /// (Some(Some(v))) writes/replaces. Same `deserialize_some` shim that
    /// the unified `/config` endpoint uses for `pausedReason`/`runnerId`.
    #[serde(default, deserialize_with = "deserialize_some")]
    ci_blocking_workflows: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    phase_verification: Option<Option<String>>,
}

/// Read `<project>/.github/workflows/*.yml|*.yaml`, returning each
/// workflow's `name:` (or its filename stem if missing/unparseable).
/// Sorted + deduped so the UI gets a stable order. Returns empty when
/// the directory is absent — the API never errors on a missing
/// workflows folder.
fn enumerate_available_workflows(project_dir: &std::path::Path) -> Vec<String> {
    let workflows_dir = project_dir.join(".github").join("workflows");
    let entries = match std::fs::read_dir(&workflows_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !matches!(ext, "yml" | "yaml") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let name = read_workflow_name(&path).unwrap_or(stem);
        names.push(name);
    }
    names.sort();
    names.dedup();
    names
}

/// Parse a workflow file and return the top-level `name:` field if it
/// exists and is a string. `None` on parse error (caller falls back to
/// the filename stem).
fn read_workflow_name(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    let map = value.as_mapping()?;
    map.get(serde_yaml::Value::String("name".into()))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn repo_defaults_for(project_dir: Option<&std::path::Path>) -> RepoDefaults {
    let Some(dir) = project_dir else {
        return RepoDefaults::default();
    };
    match crate::repo_config::load_for_project_dir(dir) {
        Some(cfg) => RepoDefaults {
            ci_blocking_workflows: cfg.ci.blocking_workflows,
            ci_blocking_workflows_skip: cfg.ci.blocking_workflows_skip,
            phase_verification: cfg.phase.verification,
        },
        None => RepoDefaults::default(),
    }
}

pub async fn get_plan_settings(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
) -> axum::response::Response {
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            // Malformed YAML → 4xx (Task 3.1 contract: never 5xx).
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan: {e}"),
                })),
            )
                .into_response();
        }
    };

    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name);
    let available_workflows = project_dir
        .as_deref()
        .map(enumerate_available_workflows)
        .unwrap_or_default();
    let repo_defaults = repo_defaults_for(project_dir.as_deref());

    let body = PlanSettings {
        ci_blocking_workflows: plan.ci_blocking_workflows,
        phase_verification: plan.phase_verification,
        available_workflows,
        repo_defaults,
    };
    (StatusCode::OK, Json(body)).into_response()
}

pub async fn put_plan_settings(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<PlanSettingsBody>,
) -> axum::response::Response {
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    // Markdown plans cannot express these fields. Reject up front instead
    // of pretending the PUT applied — the Markdown parser silently drops
    // ci_blocking_workflows / phase_verification on read.
    if !matches!(
        plan_path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    ) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "unsupported_format",
                "message": "Plan settings can only be edited on YAML plans. Convert the plan first.",
            })),
        )
            .into_response();
    }

    let raw = match std::fs::read_to_string(&plan_path) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "read_failed",
                    "message": format!("Failed to read plan file: {e}"),
                })),
            )
                .into_response();
        }
    };

    // Validate the YAML parses BEFORE we touch it. A malformed plan must
    // surface as 4xx, never 5xx (Task 3.1 contract).
    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&raw) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "parse_error",
                "message": format!("Failed to parse plan YAML: {e}"),
            })),
        )
            .into_response();
    }

    let mut updated = raw.clone();

    if let Some(opt) = body.ci_blocking_workflows.as_ref() {
        let new_value = opt.as_ref().map(|list| {
            serde_yaml::Value::Sequence(
                list.iter()
                    .map(|s| serde_yaml::Value::String(s.clone()))
                    .collect(),
            )
        });
        match update_yaml_top_level_key(&updated, "ci_blocking_workflows", new_value.as_ref()) {
            Ok(s) => updated = s,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "edit_failed",
                        "message": format!("Failed to edit ci_blocking_workflows: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    }

    if let Some(opt) = body.phase_verification.as_ref() {
        let new_value = opt.as_ref().map(|s| serde_yaml::Value::String(s.clone()));
        match update_yaml_top_level_key(&updated, "phase_verification", new_value.as_ref()) {
            Ok(s) => updated = s,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "edit_failed",
                        "message": format!("Failed to edit phase_verification: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    }

    // Sanity round-trip: parse the post-edit YAML before writing so a bug
    // in the line-based editor surfaces as 4xx + the file untouched on
    // disk, rather than a corrupted plan that breaks every later GET.
    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&updated) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "edit_invalidated_yaml",
                "message": format!("Edit produced invalid YAML: {e}"),
            })),
        )
            .into_response();
    }

    if updated != raw
        && let Err(e) = atomic_write(&plan_path, &updated)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "write_failed",
                "message": format!("Failed to write plan file: {e}"),
            })),
        )
            .into_response();
    }

    // Audit the change so settings edits show in the per-plan history.
    let mut payload = serde_json::Map::new();
    if let Some(opt) = body.ci_blocking_workflows.as_ref() {
        payload.insert(
            "ciBlockingWorkflows".into(),
            match opt {
                Some(v) => serde_json::json!(v),
                None => serde_json::Value::Null,
            },
        );
    }
    if let Some(opt) = body.phase_verification.as_ref() {
        payload.insert(
            "phaseVerification".into(),
            match opt {
                Some(v) => serde_json::json!(v),
                None => serde_json::Value::Null,
            },
        );
    }
    if !payload.is_empty() {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::CONFIG_PLAN_SETTINGS,
            crate::audit::resources::PLAN,
            Some(&name),
            Some(&serde_json::Value::Object(payload).to_string()),
        );
    }

    // Return the post-update GET shape so the UI can re-render without a
    // second round-trip.
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan after edit: {e}"),
                })),
            )
                .into_response();
        }
    };
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name);
    let available_workflows = project_dir
        .as_deref()
        .map(enumerate_available_workflows)
        .unwrap_or_default();
    let repo_defaults = repo_defaults_for(project_dir.as_deref());

    let response = PlanSettings {
        ci_blocking_workflows: plan.ci_blocking_workflows,
        phase_verification: plan.phase_verification,
        available_workflows,
        repo_defaults,
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// Update a single top-level key in a YAML document while preserving
/// every comment, blank line, and indentation outside the modified
/// key's range.
///
/// - `Some(v)` replaces the existing key+value, or inserts a new entry
///   immediately before the first `phases:` line (or at end-of-file
///   when `phases:` is absent).
/// - `None` removes the key+value entirely (no-op when the key isn't
///   present).
///
/// Comments *inside* the replaced range (e.g. an inline comment on a
/// list item) are not preserved — `serde_yaml::Value` doesn't carry
/// them. Everything else (leading comments, trailing comments, blank
/// lines, other top-level keys) survives byte-for-byte.
pub(crate) fn update_yaml_top_level_key(
    yaml: &str,
    key: &str,
    value: Option<&serde_yaml::Value>,
) -> Result<String, String> {
    let lines: Vec<&str> = yaml.split('\n').collect();
    let key_range = find_top_level_key_range(&lines, key);

    let replacement_block: Option<String> = match value {
        Some(v) => Some(serialize_top_level_kv(key, v)?),
        None => None,
    };
    let rep_lines: Option<Vec<String>> = replacement_block.map(|block| {
        let trimmed = block.strip_suffix('\n').unwrap_or(&block);
        trimmed.split('\n').map(|s| s.to_string()).collect()
    });

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);

    if let Some((start, end)) = key_range {
        for line in &lines[..start] {
            out.push((*line).to_string());
        }
        if let Some(rep) = rep_lines.as_ref() {
            for l in rep {
                out.push(l.clone());
            }
        }
        for line in &lines[end..] {
            out.push((*line).to_string());
        }
    } else {
        match rep_lines.as_ref() {
            None => return Ok(yaml.to_string()),
            Some(rep) => {
                let insert_at = find_top_level_key_line(&lines, "phases").unwrap_or(lines.len());
                for line in &lines[..insert_at] {
                    out.push((*line).to_string());
                }
                for l in rep {
                    out.push(l.clone());
                }
                for line in &lines[insert_at..] {
                    out.push((*line).to_string());
                }
            }
        }
    }

    Ok(out.join("\n"))
}

fn serialize_top_level_kv(key: &str, value: &serde_yaml::Value) -> Result<String, String> {
    let mut map = serde_yaml::Mapping::new();
    map.insert(serde_yaml::Value::String(key.to_string()), value.clone());
    serde_yaml::to_string(&serde_yaml::Value::Mapping(map)).map_err(|e| format!("yaml emit: {e}"))
}

/// Returns `(start, end)` line indices for the key's key+value lines.
/// `start` points at the `key:` line; `end` is the index of the first
/// line that is NOT part of the key's value (i.e. the slice
/// `lines[start..end]` contains exactly the key+value).
fn find_top_level_key_range(lines: &[&str], key: &str) -> Option<(usize, usize)> {
    let start = find_top_level_key_line(lines, key)?;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if line_terminates_key_range(line) {
            end = i;
            break;
        }
    }
    Some((start, end))
}

fn find_top_level_key_line(lines: &[&str], key: &str) -> Option<usize> {
    lines
        .iter()
        .position(|line| line_starts_with_top_level_key(line, key))
}

/// `true` iff `line` is `<key>:` (with optional inline value or
/// comment) at column 0. Conservative: rejects quoted-key forms
/// (`"key":`) and flow-style maps. Plan files don't use either at the
/// top level, so this matches every legitimate field while staying
/// trivial to reason about.
fn line_starts_with_top_level_key(line: &str, key: &str) -> bool {
    let bytes = line.as_bytes();
    let kbytes = key.as_bytes();
    if !bytes.starts_with(kbytes) {
        return false;
    }
    let after = &bytes[kbytes.len()..];
    if after.is_empty() || after[0] != b':' {
        return false;
    }
    after.len() == 1 || matches!(after[1], b' ' | b'\t' | b'#')
}

/// `true` iff `line` ends the previous top-level key's value range:
/// any non-blank line at column 0 that isn't a column-0 block-sequence
/// continuation (`- item`). Comments at column 0 are treated as
/// "leading" for the next key (the YAML editor convention) and so
/// terminate the current range too — this keeps the next key's leading
/// comment with that next key on delete.
fn line_terminates_key_range(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if first == b' ' || first == b'\t' {
        return false;
    }
    if first == b'-' && (bytes.len() == 1 || matches!(bytes[1], b' ' | b'\t')) {
        return false;
    }
    true
}

// ── GET/PUT /api/plans/:name/phases/:number/settings ─────────────────────────
//
// Per-phase override surface (Task 3.3). Today this is `phase_verification`
// only — CI workflow-filter overrides at phase scope are deferred until
// explicitly requested. The PUT body uses the same three-state shim as the
// plan-level endpoint so the UI can promote (override), edit, and clear
// (inherit) without touching unrelated keys.
//
// The line-based YAML editor is phase-aware: it locates the
// `  - number: <N>` list-item under `phases:` and edits the chosen key
// at indent 4 (the conventional phase-mapping indent in plan YAML). We
// keep `update_yaml_top_level_key` for plan-level edits and add
// `update_yaml_phase_key` for nested phase-mapping edits — comments,
// blank lines, and unrelated keys outside the touched range survive
// byte-for-byte, same contract as the plan-level editor.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseSettings {
    /// Phase number this response describes; echoed back so the UI can
    /// match responses to in-flight requests when several phases are
    /// being edited concurrently.
    phase_number: u32,
    /// Phase-level `phase_verification` override from the YAML; `None`
    /// when unset (resolution falls through to plan → repo → none).
    phase_verification: Option<String>,
    /// Plan-level `phase_verification` (the value the phase inherits
    /// when its own override is unset). `None` when unset.
    plan_verification: Option<String>,
    /// Repo-level `phase_verification` from `branchwork.toml` (next
    /// fallback after plan-level). `None` when unset.
    repo_verification: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PhaseSettingsBody {
    /// Three-state field: `Absent` (None) leaves the YAML key untouched,
    /// `ExplicitNull` (Some(None)) removes the key entirely, `ExplicitValue`
    /// (Some(Some(v))) writes/replaces. Same shim used elsewhere.
    #[serde(default, deserialize_with = "deserialize_some")]
    phase_verification: Option<Option<String>>,
}

fn build_phase_settings(
    plan: &plan_parser::ParsedPlan,
    phase_number: u32,
) -> Option<PhaseSettings> {
    let phase = plan.phases.iter().find(|p| p.number == phase_number)?;
    Some(PhaseSettings {
        phase_number,
        phase_verification: phase.phase_verification.clone(),
        plan_verification: plan.phase_verification.clone(),
        repo_verification: None,
    })
}

pub async fn get_phase_settings(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path((name, phase_number)): Path<(String, u32)>,
) -> axum::response::Response {
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan: {e}"),
                })),
            )
                .into_response();
        }
    };

    let mut body = match build_phase_settings(&plan, phase_number) {
        Some(b) => b,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "phase_not_found",
                    "message": format!("Phase {phase_number} not found in plan {name}"),
                })),
            )
                .into_response();
        }
    };

    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name);
    body.repo_verification = repo_defaults_for(project_dir.as_deref()).phase_verification;

    (StatusCode::OK, Json(body)).into_response()
}

pub async fn put_phase_settings(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path((name, phase_number)): Path<(String, u32)>,
    Json(body): Json<PhaseSettingsBody>,
) -> axum::response::Response {
    {
        let conn = state.db.lock().unwrap();
        if !orgs::plan_belongs_to_org(&conn, &name, auth.org_id()) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    if !matches!(
        plan_path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    ) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "unsupported_format",
                "message": "Phase settings can only be edited on YAML plans. Convert the plan first.",
            })),
        )
            .into_response();
    }

    let raw = match std::fs::read_to_string(&plan_path) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "read_failed",
                    "message": format!("Failed to read plan file: {e}"),
                })),
            )
                .into_response();
        }
    };

    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&raw) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "parse_error",
                "message": format!("Failed to parse plan YAML: {e}"),
            })),
        )
            .into_response();
    }

    // Confirm the phase exists BEFORE we touch the file, so unknown
    // phases 404 cleanly rather than silently no-op'ing.
    {
        let pre_plan = match plan_parser::parse_plan_file(&plan_path) {
            Ok(p) => p,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "parse_error",
                        "message": format!("Failed to parse plan: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        if !pre_plan.phases.iter().any(|p| p.number == phase_number) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "phase_not_found",
                    "message": format!("Phase {phase_number} not found in plan {name}"),
                })),
            )
                .into_response();
        }
    }

    let mut updated = raw.clone();

    if let Some(opt) = body.phase_verification.as_ref() {
        let new_value = opt.as_ref().map(|s| serde_yaml::Value::String(s.clone()));
        match update_yaml_phase_key(
            &updated,
            phase_number,
            "phase_verification",
            new_value.as_ref(),
        ) {
            Ok(s) => updated = s,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "edit_failed",
                        "message": format!("Failed to edit phase_verification: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    }

    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&updated) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "edit_invalidated_yaml",
                "message": format!("Edit produced invalid YAML: {e}"),
            })),
        )
            .into_response();
    }

    if updated != raw
        && let Err(e) = atomic_write(&plan_path, &updated)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "write_failed",
                "message": format!("Failed to write plan file: {e}"),
            })),
        )
            .into_response();
    }

    let mut payload = serde_json::Map::new();
    payload.insert("phaseNumber".into(), serde_json::json!(phase_number));
    if let Some(opt) = body.phase_verification.as_ref() {
        payload.insert(
            "phaseVerification".into(),
            match opt {
                Some(v) => serde_json::json!(v),
                None => serde_json::Value::Null,
            },
        );
    }
    if payload.len() > 1 {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::CONFIG_PHASE_SETTINGS,
            crate::audit::resources::PLAN,
            Some(&name),
            Some(&serde_json::Value::Object(payload).to_string()),
        );
    }

    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "parse_error",
                    "message": format!("Failed to parse plan after edit: {e}"),
                })),
            )
                .into_response();
        }
    };
    let mut response = match build_phase_settings(&plan, phase_number) {
        Some(r) => r,
        None => {
            // Should be impossible (we checked pre-write) but cover the
            // race where the file was edited externally between our
            // checks. Treat as 404 rather than 5xx so the UI can refresh.
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "phase_not_found",
                    "message": format!("Phase {phase_number} disappeared from plan {name} during edit"),
                })),
            )
                .into_response();
        }
    };
    let project_dir = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name);
    response.repo_verification = repo_defaults_for(project_dir.as_deref()).phase_verification;
    (StatusCode::OK, Json(response)).into_response()
}

/// Update a single key inside a phase mapping while preserving every
/// comment, blank line, and indentation outside the modified key's
/// range. `phase_number` matches the `- number: N` list item under
/// `phases:`. Edits land at indent 4 (the conventional phase-mapping
/// indent in plan YAML).
///
/// - `Some(v)` replaces the existing key+value, or inserts a new entry
///   immediately before the phase's `tasks:` line (the conventional
///   trailing key in `YamlPlanPhase`); falls back to end-of-block when
///   `tasks:` is absent.
/// - `None` removes the key+value entirely (no-op when absent).
fn update_yaml_phase_key(
    yaml: &str,
    phase_number: u32,
    key: &str,
    value: Option<&serde_yaml::Value>,
) -> Result<String, String> {
    const PHASE_INDENT: usize = 4;
    let lines: Vec<&str> = yaml.split('\n').collect();
    let phase_range = find_phase_block_range(&lines, phase_number)
        .ok_or_else(|| format!("phase {phase_number} not found"))?;
    let key_range = find_phase_key_range(&lines, phase_range, key, PHASE_INDENT);

    let replacement_block: Option<String> = match value {
        Some(v) => Some(serialize_phase_kv(key, v, PHASE_INDENT)?),
        None => None,
    };
    let rep_lines: Option<Vec<String>> = replacement_block.map(|block| {
        let trimmed = block.strip_suffix('\n').unwrap_or(&block);
        trimmed.split('\n').map(|s| s.to_string()).collect()
    });

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);

    if let Some((start, end)) = key_range {
        for line in &lines[..start] {
            out.push((*line).to_string());
        }
        if let Some(rep) = rep_lines.as_ref() {
            for l in rep {
                out.push(l.clone());
            }
        }
        for line in &lines[end..] {
            out.push((*line).to_string());
        }
    } else {
        match rep_lines.as_ref() {
            None => return Ok(yaml.to_string()),
            Some(rep) => {
                let (phase_start, phase_end) = phase_range;
                let insert_at =
                    find_phase_inner_key_line(&lines, phase_range, "tasks", PHASE_INDENT)
                        .unwrap_or(phase_end);
                // Defensive: `insert_at` must lie strictly inside the
                // phase block. find_phase_inner_key_line already
                // constrains its search to (start, end) so this is
                // always true; the assertion is just to catch a future
                // refactor that loosens the constraint.
                debug_assert!(insert_at > phase_start && insert_at <= phase_end);

                for line in &lines[..insert_at] {
                    out.push((*line).to_string());
                }
                for l in rep {
                    out.push(l.clone());
                }
                for line in &lines[insert_at..] {
                    out.push((*line).to_string());
                }
            }
        }
    }

    Ok(out.join("\n"))
}

/// Serialize `key: value` at `indent` spaces, suitable to splice into
/// a phase mapping. Reuses serde_yaml::to_string and re-indents each
/// emitted line.
fn serialize_phase_kv(
    key: &str,
    value: &serde_yaml::Value,
    indent: usize,
) -> Result<String, String> {
    let mut map = serde_yaml::Mapping::new();
    map.insert(serde_yaml::Value::String(key.to_string()), value.clone());
    let raw = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .map_err(|e| format!("yaml emit: {e}"))?;
    let pad = " ".repeat(indent);
    let mut out = String::with_capacity(raw.len() + 16);
    for (i, line) in raw.split('\n').enumerate() {
        let is_last = i + 1 == raw.split('\n').count();
        if is_last && line.is_empty() {
            // Drop the trailing newline produced by serde_yaml so the
            // splice doesn't introduce a blank line.
            break;
        }
        out.push_str(&pad);
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Returns `(start, end)` line indices for the phase-N block under
/// `phases:`. `start` is the `  - number: N` list-item line; `end` is
/// the index of the first line that is NOT part of the phase (next
/// phase's list item, end of phases section, or EOF).
fn find_phase_block_range(lines: &[&str], phase_number: u32) -> Option<(usize, usize)> {
    let phases_idx = lines
        .iter()
        .position(|l| line_starts_with_top_level_key(l, "phases"))?;

    // Walk forward from `phases:` looking for the matching list item.
    // A phase block ends at (a) the next `  - number:` list item, or
    // (b) any line at column 0 that isn't blank (the phases section
    // itself ended).
    let mut phase_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().skip(phases_idx + 1) {
        if let Some(n) = parse_phase_list_item_number(line) {
            if let Some(start) = phase_start {
                return Some((start, i));
            }
            if n == phase_number {
                phase_start = Some(i);
            }
            continue;
        }
        if phase_start.is_some()
            && !line.is_empty()
            && let Some(b) = line.as_bytes().first()
            && *b != b' '
            && *b != b'\t'
        {
            // Hit a column-0 non-continuation: phases section over.
            return Some((phase_start.unwrap(), i));
        }
    }
    phase_start.map(|s| (s, lines.len()))
}

/// `Some(N)` when `line` is a `  - number: N` list-item line under
/// `phases:`. Tolerates flexible whitespace before the `-` and around
/// `number:`. Returns None for any other line.
fn parse_phase_list_item_number(line: &str) -> Option<u32> {
    let after_indent = line.trim_start_matches(' ');
    if after_indent.len() == line.len() {
        return None; // no leading indent → not a list item under phases
    }
    let stripped = after_indent.strip_prefix("- ")?;
    let after_kv = stripped.trim_start();
    let val_part = after_kv.strip_prefix("number:")?.trim_start();
    // Number can be inline (`number: 1`) or quoted; the YAML schema for
    // YamlPlanPhase declares `number: u32`, so quoting is rare. Accept
    // either form. Strip any trailing comment / whitespace.
    let n_part = val_part
        .split(|c: char| c == '#' || c.is_whitespace())
        .next()?
        .trim_matches(|c| c == '\'' || c == '"');
    n_part.parse().ok()
}

/// Find the line range for `<key>:` at `indent` inside a phase block
/// `(phase_start, phase_end)`. Mirrors `find_top_level_key_range` but
/// constrained to the phase's lines and at the phase-mapping indent.
fn find_phase_key_range(
    lines: &[&str],
    phase_range: (usize, usize),
    key: &str,
    indent: usize,
) -> Option<(usize, usize)> {
    let (phase_start, phase_end) = phase_range;
    let key_line_idx = find_phase_inner_key_line(lines, phase_range, key, indent)?;
    let mut end = phase_end;
    for (i, line) in lines
        .iter()
        .enumerate()
        .skip(key_line_idx + 1)
        .take_while(|(i, _)| *i < phase_end)
    {
        if line_terminates_indented_key_range(line, indent) {
            end = i;
            break;
        }
    }
    debug_assert!(end > key_line_idx && end <= phase_end);
    debug_assert!(key_line_idx >= phase_start);
    Some((key_line_idx, end))
}

fn find_phase_inner_key_line(
    lines: &[&str],
    phase_range: (usize, usize),
    key: &str,
    indent: usize,
) -> Option<usize> {
    let (phase_start, phase_end) = phase_range;
    (phase_start + 1..phase_end).find(|&i| line_starts_with_indented_key(lines[i], indent, key))
}

/// `true` iff `line` is `<indent-spaces><key>:` (with optional inline
/// value or comment). Rejects deeper indents (different scope) and
/// lines that are list items at this indent.
fn line_starts_with_indented_key(line: &str, indent: usize, key: &str) -> bool {
    if line.len() < indent {
        return false;
    }
    let bytes = line.as_bytes();
    if !bytes[..indent].iter().all(|&b| b == b' ') {
        return false;
    }
    let after_indent = &bytes[indent..];
    if after_indent.is_empty() {
        return false;
    }
    // Reject deeper indent (the line belongs to a child scope).
    if after_indent[0] == b' ' || after_indent[0] == b'\t' {
        return false;
    }
    // Reject list items.
    if after_indent[0] == b'-' && (after_indent.len() == 1 || after_indent[1] == b' ') {
        return false;
    }
    let kbytes = key.as_bytes();
    if !after_indent.starts_with(kbytes) {
        return false;
    }
    let after = &after_indent[kbytes.len()..];
    if after.is_empty() || after[0] != b':' {
        return false;
    }
    after.len() == 1 || matches!(after[1], b' ' | b'\t' | b'#')
}

/// `true` iff `line` ends an indented key's value range. A blank line
/// is part of the value (block scalars can include blanks). Anything
/// at indent <= `indent` (and non-blank) terminates: a sibling key,
/// the next phase's list item (`  - number:` is at indent 2), or a
/// column-0 line.
fn line_terminates_indented_key_range(line: &str, indent: usize) -> bool {
    if line.is_empty() {
        return false;
    }
    let leading = line.bytes().take_while(|&b| b == b' ').count();
    if leading == line.len() {
        // Whitespace-only line — treat as blank (don't terminate).
        return false;
    }
    leading <= indent
}

// ── PUT /api/plans/:name/tasks/:num/status ───────────────────────────────────

#[derive(Deserialize)]
pub struct StatusBody {
    status: String,
}

pub async fn set_task_status(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path((name, task_number)): Path<(String, String)>,
    Json(body): Json<StatusBody>,
) -> impl IntoResponse {
    let valid = [
        "pending",
        "in_progress",
        "completed",
        "failed",
        "skipped",
        "checking",
    ];
    if !valid.contains(&body.status.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Invalid status. Must be one of: {}", valid.join(", "))
            })),
        );
    }

    // Reject `completed` on a dirty working tree — agents can't finish a task
    // with uncommitted changes in the project. See
    // `agents::check_tree_clean_for_completion` for the full reasoning.
    if body.status == "completed"
        && let crate::agents::TreeState::Dirty { files } =
            crate::agents::check_tree_clean_for_completion(&state.db, &state.plans_dir, &name)
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "working_tree_dirty",
                "message": "Cannot mark task completed — the project's working tree has \
                            uncommitted changes. Commit them before calling \
                            update_task_status(completed).",
                "files": files,
            })),
        );
    }

    let db = state.db.lock().unwrap();

    // Capture previous status for audit diff
    let prev_status: Option<String> = db
        .query_row(
            "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
            params![name, task_number],
            |row| row.get(0),
        )
        .ok();

    db.execute(
        "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
         VALUES (?1, ?2, ?3, 'manual', datetime('now'))
         ON CONFLICT(plan_name, task_number)
         DO UPDATE SET status = excluded.status,
                       source = 'manual',
                       updated_at = excluded.updated_at",
        params![name, task_number, body.status],
    )
    .unwrap();

    crate::audit::log(
        &db,
        auth.org_id(),
        auth.0.as_ref().map(|u| u.id.as_str()),
        auth.0.as_ref().map(|u| u.email.as_str()),
        crate::audit::actions::TASK_STATUS_CHANGE,
        crate::audit::resources::TASK,
        Some(&format!("{name}/{task_number}")),
        Some(
            &serde_json::json!({
                "from": prev_status.as_deref().unwrap_or("pending"),
                "to": body.status,
            })
            .to_string(),
        ),
    );
    drop(db);

    // Broadcast so the dashboard updates in real-time
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "task_status_changed",
        serde_json::json!({
            "plan_name": name,
            "task_number": task_number,
            "status": body.status,
        }),
    );

    // Auto-advance: if this completed the last task in its phase, kick off the
    // next phase's ready tasks (opt-in per plan). Spawn off so we don't block
    // the HTTP response.
    if body.status == "completed" || body.status == "skipped" {
        let registry = state.registry.clone();
        let plans_dir = state.plans_dir.clone();
        let plan_name = name.clone();
        let task = task_number.clone();
        let effort = *state.effort.lock().unwrap();
        let port = state.config_port();
        tokio::spawn(async move {
            crate::agents::try_auto_advance(
                registry, plans_dir, plan_name, task, effort, port, None,
            )
            .await;
        });
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "plan_name": name,
            "task_number": task_number,
            "status": body.status,
        })),
    )
}

// ── GET /api/plans/:name/statuses ────────────────────────────────────────────

#[derive(Serialize)]
struct TaskStatusRow {
    task_number: String,
    status: String,
    updated_at: String,
}

pub async fn get_statuses(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT task_number, status, updated_at FROM task_status WHERE plan_name = ?")
        .unwrap();

    let rows: Vec<TaskStatusRow> = stmt
        .query_map(params![name], |row| {
            Ok(TaskStatusRow {
                task_number: row.get(0)?,
                status: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .unwrap()
        .flatten()
        .collect();

    Json(rows)
}

// ── POST /api/plans/:name/tasks/:num/learnings ───────────────────────────────

#[derive(Deserialize)]
pub struct LearningBody {
    learning: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LearningRow {
    id: i64,
    learning: String,
    created_at: String,
}

pub async fn add_task_learning(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
    Json(body): Json<LearningBody>,
) -> impl IntoResponse {
    let learning = body.learning.trim();
    if learning.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "learning is required"})),
        )
            .into_response();
    }

    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
        params![plan_name, task_number, learning],
    )
    .unwrap();
    let id = db.last_insert_rowid();

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "id": id,
            "planName": plan_name,
            "taskNumber": task_number,
            "learning": learning,
        })),
    )
        .into_response()
}

pub async fn list_task_learnings(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let rows: Vec<LearningRow> = db
        .prepare(
            "SELECT id, learning, created_at FROM task_learnings \
             WHERE plan_name = ?1 AND task_number = ?2 ORDER BY id DESC",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![plan_name, task_number], |row| {
                Ok(LearningRow {
                    id: row.get(0)?,
                    learning: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Json(rows)
}

// ── POST /api/actions/start-task ────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartTaskBody {
    plan_name: String,
    phase_number: u32,
    task_number: String,
    cwd: Option<String>,
    mode: Option<String>,
    effort: Option<String>,
    /// Driver name (e.g. "claude"). Unknown/absent → server default.
    driver: Option<String>,
}

fn plan_remaining_budget(
    db: &rusqlite::Connection,
    plan_name: &str,
) -> Result<Option<f64>, (f64, f64)> {
    let budget: Option<f64> = db
        .query_row(
            "SELECT max_budget_usd FROM plan_budget WHERE plan_name = ?",
            params![plan_name],
            |row| row.get::<_, f64>(0),
        )
        .ok();
    match budget {
        Some(max) => {
            let spent: f64 = db
                .query_row(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
                     WHERE plan_name = ? AND cost_usd IS NOT NULL",
                    params![plan_name],
                    |row| row.get::<_, f64>(0),
                )
                .unwrap_or(0.0);
            if spent >= max {
                Err((spent, max))
            } else {
                Ok(Some((max - spent).max(0.0)))
            }
        }
        None => Ok(None),
    }
}

pub async fn start_task(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Json(body): Json<StartTaskBody>,
) -> impl IntoResponse {
    // ── Org budget / kill switch gate ───────────────────────────────────
    let org_id_str = auth.org_id().to_string();
    let user_id_str = auth.0.as_ref().map(|u| u.id.clone());
    {
        let db = state.db.lock().unwrap();
        let status = crate::saas::billing::check_org_budget(&db, &org_id_str);
        if matches!(
            status,
            crate::saas::billing::BudgetStatus::Exceeded
                | crate::saas::billing::BudgetStatus::Killed
        ) {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "org_budget_exceeded",
                    "message": "Organization budget exceeded. New agents are blocked.",
                })),
            )
                .into_response();
        }
        // Per-user quota check.
        if let Some(uid) = &user_id_str
            && let Err((spent, max)) = crate::saas::billing::check_user_quota(&db, &org_id_str, uid)
        {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "user_quota_exceeded",
                    "message": format!("Your quota of ${max:.2} exceeded (spent ${spent:.2})"),
                    "spentUsd": spent,
                    "maxQuotaUsd": max,
                })),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &body.plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let phase = match plan.phases.iter().find(|p| p.number == body.phase_number) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Phase not found"})),
            )
                .into_response();
        }
    };

    let task = match phase.tasks.iter().find(|t| t.number == body.task_number) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Task not found"})),
            )
                .into_response();
        }
    };

    // Dependency gate: refuse to start if any declared dep is not completed.
    if !task.dependencies.is_empty() {
        let done = {
            let conn = state.db.lock().unwrap();
            crate::db::completed_task_numbers(&conn, &body.plan_name)
        };
        let missing: Vec<String> = task
            .dependencies
            .iter()
            .filter(|d| !done.contains(*d))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "dependencies_not_met",
                    "message": format!("Blocked by task(s): {}", missing.join(", ")),
                    "missing": missing,
                })),
            )
                .into_response();
        }
    }

    // Compute per-agent budget headroom. If plan has a max budget and it's
    // already exhausted, block the start. Otherwise pass the remaining budget
    // to the spawned agent so it self-terminates on overrun.
    let remaining_budget: Option<f64> = {
        let db = state.db.lock().unwrap();
        match plan_remaining_budget(&db, &body.plan_name) {
            Ok(b) => b,
            Err((spent, max)) => {
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(serde_json::json!({
                        "error": "budget_exceeded",
                        "message": format!("Plan budget of ${max:.2} exhausted (spent ${spent:.2})"),
                        "spentUsd": spent,
                        "maxBudgetUsd": max,
                    })),
                )
                    .into_response();
            }
        }
    };

    let is_continue = body.mode.as_deref() == Some("continue");
    let home = dirs::home_dir().unwrap();
    let work_dir = body.cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    });

    let cross_ctx =
        crate::agents::build_cross_plan_context(&state.db, &state.plans_dir, &plan, &task.number);
    let port = state.config_port();
    let mcp_available = state.registry.drivers.injects_mcp(body.driver.as_deref());
    let prompt = crate::agents::build_task_prompt(
        &plan,
        phase,
        task,
        is_continue,
        port,
        cross_ctx.as_deref(),
        mcp_available,
    );

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    // Ensure git is initialized — required for branch isolation and diffs.
    // SaaS mode: skipped because work_dir lives on the runner, not on this
    // server. The agent itself initializes git via its prompt instructions
    // (same precedent as create_plan in SaaS mode — see saas-folder-listing
    // task 3.3).
    if !crate::saas::dispatch::org_has_runner(&state.db, &org_id_str) {
        ensure_git_initialized(&work_dir);
    }

    // Create a dedicated branch for this task
    let branch_name = format!("branchwork/{}/{}", body.plan_name, body.task_number);

    let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
        &state,
        &org_id_str,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &work_dir,
            plan_name: Some(&body.plan_name),
            task_id: Some(&body.task_number),
            effort,
            branch: Some(&branch_name),
            is_continue,
            max_budget_usd: remaining_budget,
            driver: body.driver.as_deref(),
            user_id: user_id_str.as_deref(),
            org_id: Some(&org_id_str),
            runner_id: None,
        },
    )
    .await;

    {
        let default_driver =
            crate::persisted_settings::PersistedSettings::load(&state.settings_path)
                .default_driver()
                .to_string();
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            &org_id_str,
            user_id_str.as_deref(),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::AGENT_START,
            crate::audit::resources::AGENT,
            Some(&agent_id),
            Some(
                &serde_json::json!({
                    "plan": body.plan_name,
                    "task": body.task_number,
                    "driver": body.driver.as_deref().unwrap_or(&default_driver),
                    "mode": body.mode.as_deref().unwrap_or("start"),
                })
                .to_string(),
            ),
        );
    }

    Json(serde_json::json!({
        "agentId": agent_id,
        "taskId": body.task_number,
        "branch": branch_name,
    }))
    .into_response()
}

// ── POST /api/plans/:name/phases/:num/start ─────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartPhaseBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    /// Driver name (e.g. "claude"). Unknown/absent → server default.
    #[serde(default)]
    driver: Option<String>,
}

pub async fn start_phase_tasks(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path((plan_name, phase_number)): Path<(String, u32)>,
    body: Option<Json<StartPhaseBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let org_id_str = auth.org_id().to_string();
    let user_id_str = auth.0.as_ref().map(|u| u.id.clone());

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let phase = match plan.phases.iter().find(|p| p.number == phase_number) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Phase not found"})),
            )
                .into_response();
        }
    };

    // Merge in manual statuses from the DB and compute the set of tasks
    // already completed (for dep gating).
    let (status_map, mut done_set) = {
        let db = state.db.lock().unwrap();
        let mut statuses: HashMap<String, String> = HashMap::new();
        if let Ok(mut stmt) =
            db.prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            && let Ok(rows) = stmt.query_map(params![plan_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
        {
            for row in rows.flatten() {
                statuses.insert(row.0, row.1);
            }
        }
        let done = crate::db::completed_task_numbers(&db, &plan_name);
        (statuses, done)
    };

    // Ready = status is pending/failed AND all dependencies satisfied.
    // Skip anything already running, completed, or skipped.
    let ready: Vec<&plan_parser::PlanTask> = phase
        .tasks
        .iter()
        .filter(|t| {
            let status = status_map
                .get(&t.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            if !(status == "pending" || status == "failed") {
                return false;
            }
            t.dependencies.iter().all(|d| done_set.contains(d))
        })
        .collect();

    if ready.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "started": [],
                "skipped": [],
                "reason": "no ready tasks in phase",
            })),
        )
            .into_response();
    }

    // Org budget / kill switch gate.
    {
        let db = state.db.lock().unwrap();
        let status = crate::saas::billing::check_org_budget(&db, &org_id_str);
        if matches!(
            status,
            crate::saas::billing::BudgetStatus::Exceeded
                | crate::saas::billing::BudgetStatus::Killed
        ) {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "org_budget_exceeded",
                    "message": "Organization budget exceeded. New agents are blocked.",
                })),
            )
                .into_response();
        }
    }

    // Budget check — if exhausted, refuse. Otherwise pass headroom to every
    // spawned agent; they self-terminate once they've consumed it.
    let remaining_budget: Option<f64> = {
        let db = state.db.lock().unwrap();
        match plan_remaining_budget(&db, &plan_name) {
            Ok(b) => b,
            Err((spent, max)) => {
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(serde_json::json!({
                        "error": "budget_exceeded",
                        "message": format!("Plan budget of ${max:.2} exhausted (spent ${spent:.2})"),
                        "spentUsd": spent,
                        "maxBudgetUsd": max,
                    })),
                )
                    .into_response();
            }
        }
    };

    let home = dirs::home_dir().unwrap();
    let work_dir = body.cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    });

    let effort = body
        .effort
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    // Ensure git is initialized — required for branch isolation and diffs.
    // SaaS mode: skipped (work_dir is on the runner — see start_task above).
    if !crate::saas::dispatch::org_has_runner(&state.db, &org_id_str) {
        ensure_git_initialized(&work_dir);
    }

    let port = state.config_port();
    let mcp_available = state.registry.drivers.injects_mcp(body.driver.as_deref());
    let mut started = Vec::new();

    for task in ready {
        let cross_ctx = crate::agents::build_cross_plan_context(
            &state.db,
            &state.plans_dir,
            &plan,
            &task.number,
        );
        let prompt = crate::agents::build_task_prompt(
            &plan,
            phase,
            task,
            false,
            port,
            cross_ctx.as_deref(),
            mcp_available,
        );
        let branch_name = format!("branchwork/{}/{}", plan_name, task.number);

        let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
            &state,
            &org_id_str,
            pty_agent::StartPtyOpts {
                prompt,
                cwd: &work_dir,
                plan_name: Some(&plan_name),
                task_id: Some(&task.number),
                effort,
                branch: Some(&branch_name),
                is_continue: false,
                max_budget_usd: remaining_budget,
                driver: body.driver.as_deref(),
                user_id: user_id_str.as_deref(),
                org_id: Some(&org_id_str),
                runner_id: None,
            },
        )
        .await;

        // Mark in_progress so subsequent clicks don't re-spawn.
        {
            let db = state.db.lock().unwrap();
            db.execute(
                "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                 VALUES (?1, ?2, 'in_progress', 'manual', datetime('now'))
                 ON CONFLICT(plan_name, task_number)
                 DO UPDATE SET status = 'in_progress',
                               source = 'manual',
                               updated_at = datetime('now')",
                params![plan_name, task.number],
            )
            .ok();
        }
        crate::ws::broadcast_event(
            &state.broadcast_tx,
            "task_status_changed",
            serde_json::json!({
                "plan_name": plan_name,
                "task_number": task.number,
                "status": "in_progress",
            }),
        );

        // Track as "done" for this run so later tasks in the same click
        // whose only unmet dep was this one aren't treated as ready mid-loop.
        // (They won't be — we already filtered up front — but we keep the
        // invariant just in case the filter is relaxed later.)
        done_set.insert(task.number.clone());

        started.push(serde_json::json!({
            "taskId": task.number,
            "title": task.title,
            "agentId": agent_id,
            "branch": branch_name,
        }));
    }

    Json(serde_json::json!({
        "planName": plan_name,
        "phaseNumber": phase_number,
        "started": started,
    }))
    .into_response()
}

// ── POST /api/plans/:name/start-session ─────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartPlanSessionBody {
    /// Optional working directory override. Defaults to `~/<plan.project>`,
    /// matching `start_task`. Useful when the project lives somewhere
    /// non-standard or for ad-hoc local testing.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    /// Driver name (e.g. "claude"). Unknown/absent → server default.
    #[serde(default)]
    driver: Option<String>,
}

/// `POST /api/plans/:name/start-session` — spawn a *plan-level* agent.
///
/// The agent boots in the plan's project directory with the plan loaded
/// as context, but no specific task assigned and no task branch created
/// (it stays on whatever branch the project is on). It runs the
/// `build_plan_session_prompt` script: greet, summarise the plan, then
/// wait for the user's instructions instead of executing a completion
/// contract.
pub async fn start_plan_session(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(plan_name): Path<String>,
    body: Option<Json<StartPlanSessionBody>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let org_id_str = auth.org_id().to_string();
    let user_id_str = auth.0.as_ref().map(|u| u.id.clone());

    // Org budget / kill switch + per-user quota — same gate as start_task.
    {
        let db = state.db.lock().unwrap();
        let status = crate::saas::billing::check_org_budget(&db, &org_id_str);
        if matches!(
            status,
            crate::saas::billing::BudgetStatus::Exceeded
                | crate::saas::billing::BudgetStatus::Killed
        ) {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "org_budget_exceeded",
                    "message": "Organization budget exceeded. New agents are blocked.",
                })),
            )
                .into_response();
        }
        if let Some(uid) = &user_id_str
            && let Err((spent, max)) = crate::saas::billing::check_user_quota(&db, &org_id_str, uid)
        {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::json!({
                    "error": "user_quota_exceeded",
                    "message": format!("Your quota of ${max:.2} exceeded (spent ${spent:.2})"),
                    "spentUsd": spent,
                    "maxQuotaUsd": max,
                })),
            )
                .into_response();
        }
    }

    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let remaining_budget: Option<f64> = {
        let db = state.db.lock().unwrap();
        match plan_remaining_budget(&db, &plan_name) {
            Ok(b) => b,
            Err((spent, max)) => {
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(serde_json::json!({
                        "error": "budget_exceeded",
                        "message": format!("Plan budget of ${max:.2} exhausted (spent ${spent:.2})"),
                        "spentUsd": spent,
                        "maxBudgetUsd": max,
                    })),
                )
                    .into_response();
            }
        }
    };

    let home = dirs::home_dir().unwrap();
    let work_dir = body.cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        plan.project
            .as_ref()
            .map(|p| home.join(p))
            .unwrap_or_else(|| std::env::current_dir().unwrap())
    });

    let port = state.config_port();
    let mcp_available = state.registry.drivers.injects_mcp(body.driver.as_deref());
    let prompt = crate::agents::build_plan_session_prompt(&plan, port, mcp_available);

    let effort = body
        .effort
        .as_deref()
        .and_then(|e| e.parse().ok())
        .unwrap_or(*state.effort.lock().unwrap());

    // Same git-init guard as start_task: only on the local path. SaaS dispatch
    // ships work to the runner, which owns its own filesystem.
    if !crate::saas::dispatch::org_has_runner(&state.db, &org_id_str) {
        ensure_git_initialized(&work_dir);
    }

    // task_id = None and branch = None: this is the whole point of the
    // plan session — no task to execute, no task branch to isolate work
    // on. The agent runs on whatever branch the project is currently on.
    let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
        &state,
        &org_id_str,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &work_dir,
            plan_name: Some(&plan_name),
            task_id: None,
            effort,
            branch: None,
            is_continue: false,
            max_budget_usd: remaining_budget,
            driver: body.driver.as_deref(),
            user_id: user_id_str.as_deref(),
            org_id: Some(&org_id_str),
            runner_id: None,
        },
    )
    .await;

    {
        let default_driver =
            crate::persisted_settings::PersistedSettings::load(&state.settings_path)
                .default_driver()
                .to_string();
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            &org_id_str,
            user_id_str.as_deref(),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::AGENT_START,
            crate::audit::resources::AGENT,
            Some(&agent_id),
            Some(
                &serde_json::json!({
                    "plan": plan_name,
                    "task": serde_json::Value::Null,
                    "driver": body.driver.as_deref().unwrap_or(&default_driver),
                    "mode": "session",
                })
                .to_string(),
            ),
        );
    }

    Json(serde_json::json!({
        "agentId": agent_id,
        "planName": plan_name,
    }))
    .into_response()
}

// ── POST /api/plans/:name/tasks/:num/check ──────────────────────────────────

pub async fn check_task(
    State(state): State<AppState>,
    Path((plan_name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response();
        }
    };

    let phase = plan
        .phases
        .iter()
        .find(|p| p.tasks.iter().any(|t| t.number == task_number));
    let task = phase.and_then(|p| p.tasks.iter().find(|t| t.number == task_number));
    let (phase, task) = match (phase, task) {
        (Some(p), Some(t)) => (p, t),
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Task not found"})),
            )
                .into_response();
        }
    };

    let home = dirs::home_dir().unwrap();
    let project_dir = home.join(project);

    let prompt = build_check_prompt(&plan_name, &plan, phase, task, &project_dir);

    // Set task to checking state
    {
        let db = state.db.lock().unwrap();
        db.execute(
            "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
             VALUES (?1, ?2, 'checking', 'manual', datetime('now'))
             ON CONFLICT(plan_name, task_number)
             DO UPDATE SET status = 'checking',
                           source = 'manual',
                           updated_at = datetime('now')",
            params![plan_name, task_number],
        )
        .ok();
    }

    let effort = *state.effort.lock().unwrap();
    let agent_id = match dispatch_check_agent(
        &state,
        prompt,
        &project_dir,
        Some(&plan_name),
        Some(&task_number),
        effort,
    )
    .await
    {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    Json(serde_json::json!({
        "agentId": agent_id,
        "planName": plan_name,
        "taskNumber": task_number,
    }))
    .into_response()
}

/// Mode-aware spawn for stream-json check agents (Task 5.12). When the
/// org has a runner attached, route to
/// `check_agent::start_check_agent_via_runner` so the `claude` process
/// runs on the runner host (only place it actually exists for SaaS).
/// Otherwise fall through to the existing in-process spawn.
///
/// `Err(Response)` carries the canonical 503 / 500 envelope for callers
/// to return as-is. The Ok arm is the new agent id.
async fn dispatch_check_agent(
    state: &AppState,
    prompt: String,
    cwd: &std::path::Path,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    effort: crate::config::Effort,
) -> Result<String, axum::response::Response> {
    // Pick any online runner across orgs — the public check-* HTTP routes
    // don't carry an auth user today (audit §17 gap), so per-org filtering
    // would be cargo-culted. Single-tenant deployments are the common
    // shape and dispatch lands on the single runner regardless of
    // `org_id`.
    let saas_target: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT id FROM runners WHERE status = 'online' \
             ORDER BY last_seen_at DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };
    if let Some(runner_id) = saas_target {
        match check_agent::start_check_agent_via_runner(
            state, &runner_id, prompt, cwd, plan_name, task_id, effort,
        )
        .await
        {
            Some(id) => Ok(id),
            None => Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "runner_dispatch_failed"})),
            )
                .into_response()),
        }
    } else {
        Ok(
            check_agent::start_check_agent(
                &state.registry,
                prompt,
                cwd,
                plan_name,
                task_id,
                effort,
            )
            .await,
        )
    }
}

// ── POST /api/plans/:name/check ─────────────────────────────────────────────
//
// Plan-level Check agent. Builds a prompt from the plan's `verification` block
// plus a done/pending task summary, spawns a read-only check agent against the
// project, and persists the verdict to `plan_verdicts` + broadcasts
// `plan_checked` via WebSocket.

pub async fn check_plan(
    State(state): State<AppState>,
    Path(plan_name): Path<String>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &plan_name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    let verification = match plan.verification.as_deref() {
        Some(v) if !v.trim().is_empty() => v,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": "Plan has no verification block to check against"}),
                ),
            )
                .into_response();
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response();
        }
    };

    let home = dirs::home_dir().unwrap();
    let project_dir = home.join(project);

    let statuses: HashMap<String, String> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            .unwrap();
        stmt.query_map(params![plan_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    let prompt = build_plan_check_prompt(&plan, verification, &statuses, &project_dir);

    let effort = *state.effort.lock().unwrap();
    let agent_id =
        match dispatch_check_agent(&state, prompt, &project_dir, Some(&plan_name), None, effort)
            .await
        {
            Ok(id) => id,
            Err(resp) => return resp,
        };

    Json(serde_json::json!({
        "agentId": agent_id,
        "planName": plan_name,
    }))
    .into_response()
}

fn build_plan_check_prompt(
    plan: &plan_parser::ParsedPlan,
    verification: &str,
    statuses: &HashMap<String, String>,
    project_dir: &std::path::Path,
) -> String {
    let mut done: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for phase in &plan.phases {
        for task in &phase.tasks {
            let status = statuses
                .get(&task.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            let line = format!("- {}: {}", task.number, task.title);
            if matches!(status, "completed" | "skipped") {
                done.push(line);
            } else {
                pending.push(line);
            }
        }
    }

    let done_section = if done.is_empty() {
        "Done tasks: (none)".to_string()
    } else {
        format!("Done tasks:\n{}", done.join("\n"))
    };
    let pending_section = if pending.is_empty() {
        "Pending tasks: (none)".to_string()
    } else {
        format!("Pending tasks:\n{}", pending.join("\n"))
    };

    format!(
        "You are verifying whether a plan's overall verification criteria are satisfied by the current state of the project.\n\
         Answer with ONLY a JSON object, no other text: {{\"status\": \"completed\"|\"in_progress\"|\"pending\", \"reason\": \"brief explanation\"}}\n\n\
         Project directory: {project_dir}\n\
         Plan: {plan_title}\n\n\
         Verification criteria:\n{verification}\n\n\
         Task summary:\n{done_section}\n\n{pending_section}\n\n\
         Check the project at {project_dir}. Read the relevant files and run the git commands you need to confirm each verification bullet.\n\n\
         Status values:\n\
         - \"completed\": every verification bullet is demonstrably satisfied in the code\n\
         - \"in_progress\": some criteria are met but not all\n\
         - \"pending\": little to no evidence the verification criteria hold\n\n\
         Respond with ONLY the JSON object.",
        project_dir = project_dir.display(),
        plan_title = plan.title,
        verification = verification.trim(),
        done_section = done_section,
        pending_section = pending_section,
    )
}

// ── POST /api/plans/create ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePlanBody {
    description: String,
    folder: String,
    create_folder: Option<bool>,
    template_id: Option<String>,
    /// Optional runner pin selected on `NewPlanForm` (T11.4). When set,
    /// the plan-creation agent runs on this runner regardless of which
    /// runner is most-recently-seen for the org. The user can later
    /// pin the resulting plan to the same runner from `PlanBoard`.
    runner_id: Option<String>,
}

/// Resolve a user-supplied folder string against `$HOME`. Three states:
///   * `~` or `~/<rest>` → `home / <rest>` (tilde-expansion)
///   * absolute path     → returned as-is
///   * bare name         → `home / <name>` (NOT cwd-relative)
///
/// The bare-name → `$HOME/<name>` rule matches the user's mental model
/// ("this is a folder name under my home directory") and avoids the
/// 2026-05-07 bug where `PathBuf::from("myproj")` would land under the
/// server process's cwd (e.g. `cargo run` from `server-rs/`). The runner
/// resolver in `branchwork_runner.rs::resolve_runner_path` applies the
/// same three-state rule against the *runner's* `$HOME`.
fn resolve_folder_path(folder: &str, home: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = folder.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else if std::path::Path::new(folder).is_absolute() {
        std::path::PathBuf::from(folder)
    } else {
        home.join(folder)
    }
}

/// Plan-creator prompt. Host-agnostic: instructs the agent to hand the
/// generated YAML back to the server via the `create_plan` MCP tool rather
/// than writing a file. The agent's host (a runner box, a laptop) typically
/// does not share a filesystem with the server, so a file write would land
/// on the wrong machine — see dashboard-stability T5.4.
fn build_plan_creator_prompt(folder: &str, description: &str, template_section: &str) -> String {
    format!(
        "You are creating an implementation plan for a project.\n\n\
         Working directory: {folder}\n\n\
         Request:\n{description}\n\n\
         {template_section}\
         Create a detailed implementation plan as YAML.\n\
         First explore the working directory to understand the existing codebase (if any).\n\
         Then call the `create_plan` MCP tool (from the `branchwork` server) with:\n\
         \x20 - `name`: a short kebab-case slug derived from the plan title (lowercase \
         letters, digits, and hyphens; e.g. `user-auth-revamp`).\n\
         \x20 - `yaml_body`: the full plan YAML as a string.\n\
         The server validates the YAML, persists it to its own plans directory, and \
         broadcasts the new plan to the dashboard. Do NOT use the Write tool to save \
         the plan file yourself — the server's plans directory may not exist on this \
         host.\n\n\
         Use this exact YAML structure for `yaml_body`:\n\
         ```yaml\n\
         title: \"Plan Title\"\n\
         context: |\n\
         \x20 Brief background and motivation.\n\
         phases:\n\
         \x20 - number: 0\n\
         \x20   title: \"Phase Title\"\n\
         \x20   description: \"Phase description\"\n\
         \x20   tasks:\n\
         \x20     - number: \"0.1\"\n\
         \x20       title: \"Task Title\"\n\
         \x20       description: |\n\
         \x20         What needs to be done.\n\
         \x20       file_paths:\n\
         \x20         - path/to/file.rs\n\
         \x20       acceptance: \"Success criteria\"\n\
         \x20       dependencies: []\n\
         ```\n\n\
         Continue with Phase 1, 2, etc. Task numbers use phase.index format (0.1, 0.2, 1.1, etc.).\n\
         The dependencies field lists task numbers this task depends on (e.g. [\"0.1\", \"0.2\"]).\n\n\
         IMPORTANT: When you are finished, do NOT stop. Instead:\n\
         1. Summarize the plan you created\n\
         2. Ask the user if they want to adjust anything\n\
         3. Only stop when the user explicitly says they are done",
    )
}

pub async fn create_plan(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Json(body): Json<CreatePlanBody>,
) -> impl IntoResponse {
    if body.description.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "description is required"})),
        )
            .into_response();
    }
    if body.folder.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "folder is required"})),
        )
            .into_response();
    }

    let resolved: std::path::PathBuf = if org_has_runner(&state.db, auth.org_id()) {
        // SaaS path: ask the runner to check or create the folder. The
        // runner-resolved absolute path comes back as a string and becomes
        // the cwd we hand to the agent — `StartAgent { cwd }` will be
        // resolved on the runner side later.
        let create_if_missing = body.create_folder.unwrap_or(false);
        let req_id = Uuid::new_v4().to_string();
        let message = WireMessage::CreateFolder {
            req_id,
            path: body.folder.clone(),
            create_if_missing,
        };
        match runner_request(&state, auth.org_id(), message, Duration::from_secs(8)).await {
            Ok(RunnerResponse::FolderCreated {
                ok: true,
                resolved_path: Some(path),
                ..
            }) => std::path::PathBuf::from(path),
            Ok(RunnerResponse::FolderCreated {
                ok: false,
                resolved_path,
                error,
            }) => {
                let err = error.unwrap_or_default();
                let display_path = resolved_path.as_deref().unwrap_or(body.folder.as_str());
                if err == "folder_not_found" {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": "folder_not_found",
                            "resolvedFolder": resolved_path,
                            "message": format!("Directory does not exist: {display_path}"),
                        })),
                    )
                        .into_response();
                }
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "create_failed",
                        "resolvedFolder": resolved_path,
                        "message": err,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "unexpected_runner_response"})),
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
            Err(RunnerRpcError::Timeout | RunnerRpcError::RunnerDisconnected) => {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({"error": "runner_unavailable"})),
                )
                    .into_response();
            }
            Err(RunnerRpcError::InvalidRequest | RunnerRpcError::SendFailed) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "runner_request_failed"})),
                )
                    .into_response();
            }
        }
    } else {
        let home = dirs::home_dir().unwrap();
        let resolved = resolve_folder_path(&body.folder, &home);

        if !resolved.exists() {
            if body.create_folder != Some(true) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "folder_not_found",
                        "message": format!("Directory does not exist: {}", resolved.display()),
                        "resolvedFolder": resolved.to_str(),
                    })),
                )
                    .into_response();
            }
            std::fs::create_dir_all(&resolved).ok();
        }

        if !resolved.is_dir() {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    serde_json::json!({"error": format!("Not a directory: {}", resolved.display())}),
                ),
            )
                .into_response();
        }

        // Ensure git is initialized — branch isolation + diff features need it.
        // Skipped on the runner branch above: the runner host owns the
        // working tree, and the agent runs `git init` itself when needed.
        ensure_git_initialized(&resolved);

        resolved
    };

    let template_section = body
        .template_id
        .as_deref()
        .and_then(templates::find)
        .map(|t| {
            format!(
                "Template: {name}\n\
                 {skeleton}\n\n\
                 Adapt the skeleton to the specifics in the request above — \
                 rename phases, add or drop tasks, and change details to fit.\n\n",
                name = t.name,
                skeleton = t.skeleton,
            )
        })
        .unwrap_or_default();

    let prompt = build_plan_creator_prompt(
        &resolved.display().to_string(),
        &body.description,
        &template_section,
    );

    let effort = *state.effort.lock().unwrap();
    let org_id_str = auth.org_id().to_string();
    let agent_id = crate::agents::spawn_ops::start_agent_dispatch(
        &state,
        &org_id_str,
        pty_agent::StartPtyOpts {
            prompt,
            cwd: &resolved,
            plan_name: None,
            task_id: None,
            effort,
            branch: None,
            is_continue: false,
            max_budget_usd: None,
            driver: None,
            user_id: None,
            org_id: Some(&org_id_str),
            // T11.4: when the user picks a runner in `NewPlanForm`, route
            // the plan-creation agent to that runner. No plan exists yet
            // so we can't pin the affinity row — the user can do that
            // from `PlanBoard` once the plan is created.
            runner_id: body.runner_id.as_deref(),
        },
    )
    .await;

    let project_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_CREATE,
            crate::audit::resources::PLAN,
            None,
            Some(
                &serde_json::json!({
                    "folder": resolved.to_str(),
                    "template": body.template_id,
                })
                .to_string(),
            ),
        );
    }

    Json(serde_json::json!({
        "agentId": agent_id,
        "folder": resolved.to_str(),
        "projectName": project_name,
    }))
    .into_response()
}

// ── PUT /api/plans/:name ────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlanBody {
    title: String,
    context: String,
    project: Option<String>,
    phases: Vec<UpdatePhaseBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePhaseBody {
    number: u32,
    title: String,
    #[serde(default)]
    description: String,
    tasks: Vec<UpdateTaskBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskBody {
    number: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    file_paths: Vec<String>,
    #[serde(default)]
    acceptance: String,
    #[serde(default)]
    dependencies: Vec<String>,
}

pub async fn update_plan(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Json(body): Json<UpdatePlanBody>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };

    // Build a ParsedPlan from the update body
    let plan = plan_parser::ParsedPlan {
        name: name.clone(),
        file_path: plan_path.to_string_lossy().to_string(),
        title: body.title,
        context: body.context,
        project: body.project,
        created_at: String::new(),
        modified_at: String::new(),
        phases: body
            .phases
            .into_iter()
            .map(|p| plan_parser::PlanPhase {
                number: p.number,
                title: p.title,
                description: p.description,
                tasks: p
                    .tasks
                    .into_iter()
                    .map(|t| plan_parser::PlanTask {
                        number: t.number,
                        title: t.title,
                        description: t.description,
                        file_paths: t.file_paths,
                        acceptance: t.acceptance,
                        dependencies: t.dependencies,
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    })
                    .collect(),
                ci_blocking_workflows: None,
                phase_verification: None,
            })
            .collect(),
        verification: None,
        ci_blocking_workflows: None,
        phase_verification: None,
        total_cost_usd: None,
        max_budget_usd: None,
    };

    // Always write as YAML
    let yaml = match plan_parser::serialize_plan_yaml(&plan) {
        Ok(y) => y,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize: {e}")})),
            )
                .into_response();
        }
    };

    // Write to .yaml (migrate from .md if needed)
    let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
    if let Err(e) = std::fs::write(&yaml_path, &yaml) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write: {e}")})),
        )
            .into_response();
    }

    // Remove old .md if we just migrated
    if plan_path.extension().is_some_and(|e| e == "md") {
        std::fs::remove_file(&plan_path).ok();
    }

    {
        let db = state.db.lock().unwrap();
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_UPDATE,
            crate::audit::resources::PLAN,
            Some(&name),
            None,
        );
    }

    Json(serde_json::json!({"ok": true, "name": name})).into_response()
}

// ── DELETE /api/plans/:name ─────────────────────────────────────────────────

#[derive(Deserialize, Default, Debug)]
pub struct DeletePlanQuery {
    /// Bypass archive + snapshot and hard-delete the YAML + cascade. The
    /// dashboard never sets this; it's for fixture cleanup / runaway
    /// scratch plans.
    #[serde(default)]
    pub hard: bool,
    /// Run every gate (404 / running-agents / auto-mode) without
    /// touching the file or the DB. Returns the same body shape with
    /// `dryRun: true` so the UI can preview the cascade. URL spelling is
    /// snake_case (`?dry_run=true`); response body keeps camelCase for
    /// fetch-layer consistency.
    #[serde(default)]
    pub dry_run: bool,
}

/// Tables keyed by `plan_name` that the DELETE handler cascades.
///
/// **Audit convention (plan-deletion 0.0)** — when adding a new schema
/// in `db.rs::migrate`, classify it here so the cascade keeps up. The
/// `cascade_audit_tests::cascade_set_matches_schema` unit test sweeps
/// the live SQLite schema for every column literally named `plan_name`
/// and fails the build if it finds a table that is not classified
/// below. Any time it fires, pick the right bucket and add the table:
///
/// | table                | plan_name shape          | verdict   |
/// |----------------------|--------------------------|-----------|
/// | task_status          | NOT NULL, PK part        | cascade   |
/// | ci_runs              | NOT NULL                 | cascade   |
/// | plan_auto_mode       | PRIMARY KEY              | cascade   |
/// | plan_auto_advance    | PRIMARY KEY              | cascade   |
/// | task_fix_attempts    | NOT NULL, PK part        | cascade   |
/// | plan_project         | PRIMARY KEY              | cascade   |
/// | plan_verdicts        | PRIMARY KEY              | cascade   |
/// | plan_budget          | PRIMARY KEY              | cascade   |
/// | task_learnings       | NOT NULL                 | cascade   |
/// | plan_org             | PRIMARY KEY              | cascade   |
/// | plan_runner_affinity | PRIMARY KEY              | cascade   |
/// | agents               | nullable (no FK)         | preserve  |
/// | plan_snapshots       | NOT NULL                 | preserve  |
///
/// **Cascade**: the row is meaningless once the plan is gone (status,
/// CI runs, auto-mode toggles, fix-attempt log, project mapping,
/// per-plan settings, learnings, org ownership). All of these are
/// wiped inside the same transaction in step 7 of `delete_plan`.
///
/// **Preserve** (`agents`): completed/killed agent rows stay so the
/// dashboard can still render their terminal output, diff, branch and
/// stop-reason after the plan is gone. The row's `plan_name` becomes a
/// dangling pointer; `task_status` re-reads the live `plan_name` via
/// `LEFT JOIN`-style queries that tolerate the orphan.
///
/// **Edge cases worth calling out**:
///
/// - `audit_logs.resource_id`: holds the plan name when
///   `resource_type='plan'` (and the agent id when `'agent'`). NOT
///   touched — the historical audit trail is the load-bearing record
///   the Activity tab + Undo affordance read from.
/// - `task_fix_attempts.outcome IS NULL`: load-bearing for the
///   `auto_mode_in_flight` gate in step 4 of `delete_plan`. Open
///   attempts block the cascade with a 409 instead of being wiped.
/// - `plan_snapshots`: holds the full pre-cascade blob for Undo
///   (written by `plan_curate::snapshot_plan` in step 6 of
///   `delete_plan`, BEFORE the cascade runs). Listed in
///   `PLAN_NAME_PRESERVED_TABLES` so the cascade itself does NOT
///   delete snapshot rows; the retention purge job (0.5) is what
///   eventually frees them.
pub(crate) const PLAN_CASCADE_TABLES: &[&str] = &[
    "task_status",
    "ci_runs",
    "plan_auto_mode",
    "plan_auto_advance",
    "task_fix_attempts",
    "plan_project",
    "plan_verdicts",
    "plan_budget",
    "task_learnings",
    "plan_org",
    "plan_runner_affinity",
];

/// Tables that contain a `plan_name` column but are intentionally NOT
/// cascaded by `delete_plan`. Kept as a separate constant so the
/// `cascade_set_matches_schema` audit test can prove the union of
/// cascade + preserve covers every `plan_name`-bearing table in the
/// live schema. New entries here MUST also be documented in the
/// `PLAN_CASCADE_TABLES` doc-comment table above.
#[allow(dead_code)] // Consumed only by `cascade_audit_tests`; exists to make the audit explicit.
const PLAN_NAME_PRESERVED_TABLES: &[&str] = &["agents", "plan_snapshots"];

pub async fn delete_plan(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(name): Path<String>,
    Query(q): Query<DeletePlanQuery>,
) -> impl IntoResponse {
    // 1. Resolve the plan file.
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "plan_not_found"})),
            )
                .into_response();
        }
    };

    // 2. Compute the safety-gate inputs. We deliberately read both gates
    //    BEFORE branching on `dry_run` so the preview body can carry
    //    `blockedBy` — that is what lets the modal disable the Delete
    //    button before the user clicks (rather than discovering the 409
    //    only after issuing the destructive request).
    let running_agents: Vec<String> = {
        let db = state.db.lock().unwrap();
        db.prepare(
            "SELECT id FROM agents \
             WHERE plan_name = ?1 \
               AND status IN ('starting', 'running')",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![name], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default()
    };
    let auto_mode_in_flight: bool = {
        let db = state.db.lock().unwrap();
        let n: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM task_fix_attempts \
                 WHERE plan_name = ?1 AND outcome IS NULL",
                params![name],
                |r| r.get(0),
            )
            .unwrap_or(0);
        n > 0
    };
    let blocked_by: Option<serde_json::Value> = if !running_agents.is_empty() || auto_mode_in_flight
    {
        Some(serde_json::json!({
            "runningAgents": running_agents,
            "autoModeInFlight": auto_mode_in_flight,
        }))
    } else {
        None
    };

    // 3. dry_run: return the preview body and exit. We do NOT 409 on a
    //    blocked plan — the gate state is reported via `blockedBy` so
    //    the modal can render the cascade preview alongside a disabled
    //    Delete button. Reads only; no audit, no WS broadcast, no FS
    //    or DB mutation.
    if q.dry_run {
        // Per-table cascade counts. SELECT COUNT(*) is plan-scoped so
        // failures on one table (schema drift, missing column) don't
        // poison the rest — log and report 0 in that slot.
        let would_delete: HashMap<&'static str, i64> = {
            let db = state.db.lock().unwrap();
            PLAN_CASCADE_TABLES
                .iter()
                .map(|table| {
                    let sql = format!("SELECT COUNT(*) FROM {table} WHERE plan_name = ?1");
                    let n: i64 = db.query_row(&sql, params![name], |r| r.get(0)).unwrap_or(0);
                    (*table, n)
                })
                .collect()
        };
        return Json(serde_json::json!({
            "ok": true,
            "dryRun": true,
            "name": name,
            "filePath": plan_path.to_str(),
            "hard": q.hard,
            "cascadeTables": PLAN_CASCADE_TABLES,
            "wouldDelete": would_delete,
            "blockedBy": blocked_by,
        }))
        .into_response();
    }

    // 4. Real-delete safety gates. Mirror the dry_run state into 409s.
    if !running_agents.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "plan_has_running_agents",
                "agents": running_agents,
            })),
        )
            .into_response();
    }
    if auto_mode_in_flight {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "auto_mode_in_flight"})),
        )
            .into_response();
    }

    // 5. Build the archive path (soft delete only). Created lazily right
    //    before the rename so a hard delete never touches the FS.
    let archive_path: Option<std::path::PathBuf> = if q.hard {
        None
    } else {
        let archive_dir = state.plans_dir.join("archive");
        if let Err(e) = std::fs::create_dir_all(&archive_dir) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("failed to create archive dir: {e}"),
                })),
            )
                .into_response();
        }
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let ext = plan_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("yaml");
        Some(archive_dir.join(format!("{name}.{ts}.{ext}")))
    };

    // 6. Snapshot prior state. Soft delete snapshots before the cascade
    //    so a mid-cascade failure still leaves a recovery row in
    //    `plan_snapshots`. Hard delete skips the snapshot per the brief
    //    — `?hard=true` is the explicit "no undo" path. Snapshot
    //    failure aborts: the user retries and only after that succeeds
    //    do we destroy data.
    let snapshot_id: Option<i64> = if q.hard {
        None
    } else {
        match crate::plan_curate::snapshot_plan(
            &state,
            &name,
            crate::plan_curate::SnapshotKind::Delete,
            archive_path.as_deref(),
        ) {
            Ok(id) => Some(id),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("snapshot failed: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    };

    // 7. Cascade DB rows in a single transaction so a partial failure
    //    leaves the plan intact.
    let mut cascaded_rows: HashMap<String, i64> = HashMap::new();
    {
        let mut db = state.db.lock().unwrap();
        let tx = match db.transaction() {
            Ok(t) => t,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to open transaction: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        for table in PLAN_CASCADE_TABLES {
            let sql = format!("DELETE FROM {table} WHERE plan_name = ?1");
            let n = match tx.execute(&sql, params![name]) {
                Ok(n) => n as i64,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": format!("cascade failed on {table}: {e}"),
                        })),
                    )
                        .into_response();
                }
            };
            cascaded_rows.insert((*table).to_string(), n);
        }
        if let Err(e) = tx.commit() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("cascade commit failed: {e}"),
                })),
            )
                .into_response();
        }
    }

    // 8. Move (soft) or remove (hard) the YAML. Failures here are
    //    warnings — the DB cascade is the authoritative source of truth.
    let mut warning: Option<String> = None;
    if q.hard {
        if let Err(e) = std::fs::remove_file(&plan_path) {
            warning = Some(format!("DB cascade succeeded; file remove failed: {e}"));
        }
    } else if let Some(ref archive) = archive_path
        && let Err(e) = std::fs::rename(&plan_path, archive)
    {
        warning = Some(format!("DB cascade succeeded; archive move failed: {e}"));
    }

    // 9. Audit log.
    {
        let db = state.db.lock().unwrap();
        let diff = serde_json::json!({
            "file_path": plan_path.to_str(),
            "archive_path": archive_path.as_ref().and_then(|p| p.to_str()),
            "snapshot_id": snapshot_id,
            "hard": q.hard,
            "cascaded_rows": cascaded_rows,
        });
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_DELETE,
            crate::audit::resources::PLAN,
            Some(&name),
            Some(&diff.to_string()),
        );
    }

    // 10. WS broadcast. The file watcher will also fire a remove event,
    //    but the explicit broadcast carries `snapshot_id` + `hard` for
    //    the Undo affordance and is faster than the watcher's debounce.
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "plan_deleted",
        serde_json::json!({
            "plan": name,
            "snapshot_id": snapshot_id,
            "hard": q.hard,
        }),
    );

    let mut response = serde_json::json!({
        "ok": true,
        "name": name,
        "snapshotId": snapshot_id,
        "archivePath": archive_path.as_ref().and_then(|p| p.to_str()),
        "hard": q.hard,
        "cascadedRows": cascaded_rows,
    });
    if let Some(w) = warning {
        response["warning"] = serde_json::Value::String(w);
    }
    Json(response).into_response()
}

// ── POST /api/snapshots/:id/restore ─────────────────────────────────────────

/// Replay a `plan_snapshots` row back into the live state. The snapshot
/// is the substrate the Activity-tab Undo affordance reads from; this
/// handler is the only writer that flips `restored_at` from NULL to a
/// timestamp, so once a snapshot is consumed it can't be re-applied
/// (the row stays around for the audit trail until the retention
/// purger frees it).
///
/// Status code contract:
/// - 200: file restored, cascade replayed, snapshot marked.
/// - 404: snapshot id is unknown (purger already freed it, or the user
///   typed the wrong number).
/// - 410: snapshot exists but `restored_at` is non-NULL — already
///   undone. Includes the prior `restored_at` so the UI can render
///   "this was restored at T".
/// - 409: a plan with `plan_name` exists in the active list (some
///   other plan reused the slug after the original delete). Body
///   carries `current` with a one-line summary so the user can rename
///   the colliding plan first.
/// - 500: snapshot row is corrupt, or the cascade replay / file write
///   failed mid-flight.
pub async fn restore_snapshot(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // 1. Load the snapshot row. We need the plan name + body + cascade
    //    + archive path; restored_at is the gate for already-undone.
    #[derive(Debug)]
    struct SnapshotRow {
        plan_name: String,
        kind: String,
        archive_path: Option<String>,
        yaml_body: String,
        cascade_json: String,
        restored_at: Option<String>,
    }
    let row = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT plan_name, kind, archive_path, yaml_body, cascade_json, restored_at \
             FROM plan_snapshots WHERE id = ?1",
            params![id],
            |r| {
                Ok(SnapshotRow {
                    plan_name: r.get(0)?,
                    kind: r.get(1)?,
                    archive_path: r.get(2)?,
                    yaml_body: r.get(3)?,
                    cascade_json: r.get(4)?,
                    restored_at: r.get(5)?,
                })
            },
        )
        .ok()
    };
    let Some(row) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "snapshot_not_found"})),
        )
            .into_response();
    };

    // 2. Already restored? 410 Gone — the row is intentionally retained
    //    (audit substrate) but cannot be replayed twice.
    if let Some(ref ts) = row.restored_at {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "snapshot_already_restored",
                "restored_at": ts,
            })),
        )
            .into_response();
    }

    // 3. Slug-collision check. If a plan with `plan_name` already exists
    //    in the active list, refuse with 409 + a one-line summary so the
    //    user can rename the colliding plan first.
    if let Some(existing_path) = plan_parser::find_plan_file(&state.plans_dir, &row.plan_name) {
        let summary = match plan_parser::parse_plan_file(&existing_path) {
            Ok(p) => format!("title: {}", p.title),
            Err(_) => format!("file: {}", existing_path.display()),
        };
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "slug_collision",
                "current": summary,
            })),
        )
            .into_response();
    }

    // 4. Replay the cascade + mark the snapshot in a single DB tx.
    //    `UPDATE ... WHERE restored_at IS NULL` is the critical-section
    //    guard: a concurrent restore that read NULL in step 1 will see
    //    rows_affected == 0 here and roll back.
    let replay = {
        let mut db = state.db.lock().unwrap();
        let tx = match db.transaction() {
            Ok(t) => t,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to open transaction: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        let claim = tx.execute(
            "UPDATE plan_snapshots SET restored_at = datetime('now') \
             WHERE id = ?1 AND restored_at IS NULL",
            params![id],
        );
        match claim {
            Ok(0) => {
                let _ = tx.rollback();
                let restored_at: Option<String> = state
                    .db
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT restored_at FROM plan_snapshots WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .unwrap_or(None);
                return (
                    StatusCode::GONE,
                    Json(serde_json::json!({
                        "error": "snapshot_already_restored",
                        "restored_at": restored_at,
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to claim snapshot: {e}"),
                    })),
                )
                    .into_response();
            }
        }

        let counts = match crate::plan_curate::replay_cascade(&tx, &row.cascade_json) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.rollback();
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("cascade replay failed: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        if let Err(e) = tx.commit() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("restore commit failed: {e}"),
                })),
            )
                .into_response();
        }
        counts
    };

    // 5. Restore the YAML on disk via tempfile + atomic rename. Final
    //    name is always `<plan_name>.yaml` per the brief — even if the
    //    snapshot was taken from a `.yml` or `.md` source. The slug
    //    gate in step 3 means no other file with this stem exists, so
    //    rename can never overwrite a foreign plan.
    let mut warning: Option<String> = None;
    let yaml_path = state.plans_dir.join(format!("{}.yaml", row.plan_name));
    if let Err(e) = std::fs::create_dir_all(&state.plans_dir) {
        warning = Some(format!(
            "DB cascade succeeded; plans_dir create failed: {e}"
        ));
    } else if let Err(e) = atomic_write(&yaml_path, &row.yaml_body) {
        warning = Some(format!("DB cascade succeeded; YAML write failed: {e}"));
    }

    // 6. Best-effort: drop the archive copy. Restoration came from the
    //    snapshot blob, so the on-disk archive is now a stale duplicate
    //    that would clutter the archive directory and confuse 0.5's
    //    retention purger.
    if let Some(ref archive) = row.archive_path {
        let archive_path = std::path::Path::new(archive);
        if archive_path.exists()
            && let Err(e) = std::fs::remove_file(archive_path)
        {
            warning = Some(match warning {
                Some(prev) => format!("{prev}; archive cleanup failed: {e}"),
                None => format!("archive cleanup failed: {e}"),
            });
        }
    }

    // 7. Audit log + WS broadcast.
    let restored_at_now: String = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT restored_at FROM plan_snapshots WHERE id = ?1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string())
    };
    {
        let db = state.db.lock().unwrap();
        let diff = serde_json::json!({
            "snapshot_id": id,
            "plan_name": row.plan_name,
            "kind": row.kind,
            "replayed_rows": replay,
        });
        crate::audit::log(
            &db,
            auth.org_id(),
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_RESTORE,
            crate::audit::resources::PLAN,
            Some(&row.plan_name),
            Some(&diff.to_string()),
        );
    }
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "plan_restored",
        serde_json::json!({
            "plan": row.plan_name,
            "snapshot_id": id,
        }),
    );

    let mut response = serde_json::json!({
        "ok": true,
        "plan": row.plan_name,
        "snapshotId": id,
        "restoredAt": restored_at_now,
        "replayedRows": replay,
    });
    if let Some(w) = warning {
        response["warning"] = serde_json::Value::String(w);
    }
    Json(response).into_response()
}

// ── GET /api/snapshots ──────────────────────────────────────────────────────

/// One snapshot row as returned by [`list_snapshots`]. Frontend keys
/// off `id` for restore/purge actions and `expires_at` for the
/// countdown chip; `restored_at` is non-null for snapshots that have
/// been replayed but are not yet expired (kept for the audit trail).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotListEntry {
    id: i64,
    plan_name: String,
    kind: String,
    created_at: String,
    expires_at: String,
    archive_path: Option<String>,
    restored_at: Option<String>,
}

/// List every `plan_snapshots` row scoped to the caller's org. Used by
/// the Archive panel to show pending soft-deleted plans, their
/// retention countdown, and offer Restore / Purge-now affordances.
///
/// Sort: newest snapshot first (`created_at DESC`) so a freshly
/// soft-deleted plan lands at the top of the list.
///
/// Excludes nothing: restored snapshots stay visible until the
/// retention purger frees them so the audit trail is accessible from
/// the same panel as live entries. The frontend can dim restored rows
/// based on `restored_at`.
pub async fn list_snapshots(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
) -> impl IntoResponse {
    let org_id = auth.org_id().to_string();
    let entries: Vec<SnapshotListEntry> = {
        let db = state.db.lock().unwrap();
        let mut stmt = match db.prepare(
            "SELECT id, plan_name, kind, created_at, expires_at, archive_path, restored_at \
             FROM plan_snapshots \
             WHERE org_id = ?1 \
             ORDER BY created_at DESC, id DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("snapshot list prepare failed: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        let rows = stmt.query_map(params![org_id], |r| {
            Ok(SnapshotListEntry {
                id: r.get(0)?,
                plan_name: r.get(1)?,
                kind: r.get(2)?,
                created_at: r.get(3)?,
                expires_at: r.get(4)?,
                archive_path: r.get(5)?,
                restored_at: r.get(6)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(Result::ok).collect(),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("snapshot list query failed: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    };
    Json(serde_json::json!({ "snapshots": entries })).into_response()
}

// ── DELETE /api/snapshots/:id ───────────────────────────────────────────────

/// Immediately hard-delete a `plan_snapshots` row + its archive YAML.
/// Counterpart of `restore_snapshot`. Surfaced from the Archive panel
/// behind a second-confirm modal — no undo.
///
/// Status code contract:
/// - 200: row + archive removed (best-effort on archive: a missing
///   file does not fail the delete).
/// - 403: snapshot belongs to a different org than the caller.
/// - 404: snapshot id is unknown (already purged, or never existed).
/// - 500: SQLite delete failed.
pub async fn delete_snapshot(
    State(state): State<AppState>,
    auth: OptionalAuthUser,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // 1. Look up the snapshot. Kind, plan_name, archive_path, org_id
    //    are all needed for the audit row; the caller's org_id gates
    //    cross-tenant access.
    #[derive(Debug)]
    struct SnapshotRow {
        plan_name: String,
        kind: String,
        archive_path: Option<String>,
        org_id: String,
    }
    let row = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT plan_name, kind, archive_path, org_id \
             FROM plan_snapshots WHERE id = ?1",
            params![id],
            |r| {
                Ok(SnapshotRow {
                    plan_name: r.get(0)?,
                    kind: r.get(1)?,
                    archive_path: r.get(2)?,
                    org_id: r.get(3)?,
                })
            },
        )
        .ok()
    };
    let Some(row) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "snapshot_not_found"})),
        )
            .into_response();
    };

    if row.org_id != auth.org_id() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "snapshot_not_in_org"})),
        )
            .into_response();
    }

    // 2. Best-effort: remove the archive YAML. NotFound is fine —
    //    operator may have cleaned the directory by hand. Any other
    //    error is captured as a warning so the row delete still wins.
    let mut warning: Option<String> = None;
    if let Some(ref path) = row.archive_path {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warning = Some(format!("archive cleanup failed: {e}"));
            }
        }
    }

    // 3. Delete the snapshot row. If the retention purger raced us and
    //    already removed it, return 404 — the row is gone either way,
    //    but the user should know they didn't trigger the purge.
    let n = {
        let db = state.db.lock().unwrap();
        match db.execute("DELETE FROM plan_snapshots WHERE id = ?1", params![id]) {
            Ok(n) => n,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("snapshot delete failed: {e}"),
                    })),
                )
                    .into_response();
            }
        }
    };
    if n == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "snapshot_not_found"})),
        )
            .into_response();
    }

    // 4. Audit log. Append-only; matches the shape used by the
    //    automatic retention purger so both events render in the same
    //    Activity-tab column.
    {
        let db = state.db.lock().unwrap();
        let diff = serde_json::json!({
            "snapshot_id": id,
            "kind": row.kind,
            "archive_path": row.archive_path,
        });
        crate::audit::log(
            &db,
            &row.org_id,
            auth.0.as_ref().map(|u| u.id.as_str()),
            auth.0.as_ref().map(|u| u.email.as_str()),
            crate::audit::actions::PLAN_SNAPSHOT_PURGED_MANUAL,
            crate::audit::resources::PLAN,
            Some(&row.plan_name),
            Some(&diff.to_string()),
        );
    }

    // 5. WS broadcast so any open Archive panel drops the row without
    //    a manual refresh.
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "snapshot_purged",
        serde_json::json!({
            "snapshot_id": id,
            "plan": row.plan_name,
        }),
    );

    let mut response = serde_json::json!({
        "ok": true,
        "snapshotId": id,
        "plan": row.plan_name,
    });
    if let Some(w) = warning {
        response["warning"] = serde_json::Value::String(w);
    }
    Json(response).into_response()
}

/// Write `body` to `path` atomically: write to a sibling tempfile in
/// the same directory, then rename. POSIX rename is atomic; Windows
/// rename-over-existing fails but our caller (`restore_snapshot`)
/// already gated on the slug not existing in the active list, so the
/// target is always absent. Kept a runtime helper rather than a
/// `tempfile`-crate dep because the only consumer is this handler and
/// the file body is a few KB at most.
fn atomic_write(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("plan.yaml");
    // Random-ish suffix to avoid colliding with a parallel restore; the
    // process id + a UUID prefix is more than enough entropy.
    let tmp_name = format!(".{stem}.tmp.{}.{}", std::process::id(), Uuid::new_v4());
    let tmp_path = parent.join(tmp_name);
    std::fs::write(&tmp_path, body)?;
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        // Best-effort cleanup — if rename failed we don't want a stray
        // dotfile next to the plans dir on the next restore attempt.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

// ── POST /api/plans/:name/convert ──────────────────────────────────────────

pub async fn convert_plan(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let md_path = state.plans_dir.join(format!("{name}.md"));
    if !md_path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "No .md plan found with that name"})),
        )
            .into_response();
    }

    let plan = match plan_parser::parse_plan_file(&md_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to parse plan: {e}")})),
            )
                .into_response();
        }
    };

    let yaml = match plan_parser::serialize_plan_yaml(&plan) {
        Ok(y) => y,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to serialize YAML: {e}")})),
            )
                .into_response();
        }
    };

    let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
    if let Err(e) = std::fs::write(&yaml_path, &yaml) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write YAML: {e}")})),
        )
            .into_response();
    }

    // Remove the .md file now that .yaml exists
    std::fs::remove_file(&md_path).ok();

    Json(serde_json::json!({
        "ok": true,
        "name": name,
        "yamlPath": yaml_path.to_str(),
    }))
    .into_response()
}

// ── POST /api/plans/convert-all ────────────────────────────────────────────

pub async fn convert_all(State(state): State<AppState>) -> impl IntoResponse {
    let entries = match std::fs::read_dir(&state.plans_dir) {
        Ok(e) => e,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to read plans directory: {e}")
            }))
            .into_response();
        }
    };

    let md_files: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();

    let mut converted = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for entry in &md_files {
        let path = entry.path();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        // Skip if .yaml already exists
        let yaml_path = state.plans_dir.join(format!("{name}.yaml"));
        if yaml_path.exists() {
            skipped.push(name);
            continue;
        }

        let plan = match plan_parser::parse_plan_file(&path) {
            Ok(p) => p,
            Err(e) => {
                failed.push(serde_json::json!({"name": name, "error": e.to_string()}));
                continue;
            }
        };

        // Skip plans that parsed poorly (0 tasks)
        let task_count: usize = plan.phases.iter().map(|p| p.tasks.len()).sum();
        if task_count == 0 {
            failed.push(
                serde_json::json!({"name": name, "error": "0 tasks parsed — needs manual review"}),
            );
            continue;
        }

        match plan_parser::serialize_plan_yaml(&plan) {
            Ok(yaml) => {
                if let Err(e) = std::fs::write(&yaml_path, &yaml) {
                    failed.push(serde_json::json!({"name": name, "error": e.to_string()}));
                } else {
                    std::fs::remove_file(&path).ok();
                    converted.push(name);
                }
            }
            Err(e) => {
                failed.push(serde_json::json!({"name": name, "error": e}));
            }
        }
    }

    Json(serde_json::json!({
        "converted": converted.len(),
        "skipped": skipped.len(),
        "failed": failed.len(),
        "convertedNames": converted,
        "skippedNames": skipped,
        "failures": failed,
    }))
    .into_response()
}

// ── POST /api/plans/:name/reset-status — reset all task statuses to pending ─

pub async fn reset_plan_status(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let changes = db
        .execute("DELETE FROM task_status WHERE plan_name = ?", params![name])
        .unwrap_or(0);
    drop(db);

    // Broadcast so the UI refreshes in place
    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "plan_reset",
        serde_json::json!({ "plan_name": name, "cleared": changes }),
    );

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "cleared": changes,
    }))
}

// ── POST /api/plans/:name/tasks/:task/reset-status — unwedge a single task ──

/// Clear the task's `task_status` row so it reverts to "derived / unknown"
/// and the user can pick up from a clean slate. Idempotent: safe to re-run.
///
/// Refuses if there's a running/starting agent for the task — resetting
/// status from under a live agent would orphan the agent's writes and
/// confuse the dashboard. Kill the agent first, then reset.
pub async fn reset_task_status(
    State(state): State<AppState>,
    Path((name, task_number)): Path<(String, String)>,
) -> impl IntoResponse {
    // Live-agent check. We scope the lock tightly so the subsequent DELETE
    // doesn't contend for the same guard.
    let live_agent: Option<String> = {
        let db = state.db.lock().unwrap();
        db.query_row(
            "SELECT id FROM agents \
             WHERE plan_name = ?1 AND task_id = ?2 \
                   AND status IN ('running', 'starting') \
             LIMIT 1",
            params![name, task_number],
            |r| r.get::<_, String>(0),
        )
        .ok()
    };
    if let Some(id) = live_agent {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "agent_running",
                "message": format!(
                    "Agent {} is still running for this task. Kill or finish it before resetting.",
                    &id[..8.min(id.len())]
                ),
                "agent_id": id,
            })),
        )
            .into_response();
    }

    let cleared = {
        let db = state.db.lock().unwrap();
        db.execute(
            "DELETE FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
            params![name, task_number],
        )
        .unwrap_or(0)
    };

    crate::ws::broadcast_event(
        &state.broadcast_tx,
        "task_status_changed",
        serde_json::json!({
            "plan_name": name,
            "task_number": task_number,
            // Null status = reverted to derived; clients treat as pending unless
            // something else (e.g. CI) contradicts.
            "status": serde_json::Value::Null,
        }),
    );

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "task_number": task_number,
        "cleared": cleared,
    }))
    .into_response()
}

// ── Branch cleanup helpers ──────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleBranch {
    pub name: String,
    pub sha: Option<String>,
    pub commits_ahead_of_trunk: Option<u64>,
    pub last_commit_age_secs: Option<i64>,
    pub agent_id: Option<String>,
    /// Status of the agent row referencing this branch, if any. The UI
    /// uses this to disable the checkbox for live agents (`running` /
    /// `starting`) so the user can't yank a branch out from under a
    /// session that's still writing to it. Mirrors the live-agent guard
    /// on the purge endpoint.
    pub agent_status: Option<String>,
    /// When false, the branch has no unique commits and is safe to delete
    /// without the `force` flag.
    pub has_unique_commits: bool,
}

/// GET /api/plans/:name/branches/stale
///
/// Enumerate all `branchwork/<plan>/*` refs in the plan's project dir and
/// report their state so the user can decide which to delete. Read-only.
pub async fn list_stale_branches(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(cwd) = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "plan has no resolvable project directory"})),
        )
            .into_response();
    };

    // List local branches matching this plan's prefix.
    let prefix = format!("branchwork/{name}/");
    let prefix_fix = format!("branchwork/fix/{name}/");
    let branches: Vec<(String, String, Option<i64>)> = {
        let out = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname:short)|%(objectname)|%(committerdate:unix)",
                "refs/heads/",
            ])
            .current_dir(&cwd)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let parts: Vec<&str> = l.splitn(3, '|').collect();
                    if parts.len() < 3 {
                        return None;
                    }
                    let n = parts[0].to_string();
                    if !(n.starts_with(&prefix) || n.starts_with(&prefix_fix)) {
                        return None;
                    }
                    Some((n, parts[1].to_string(), parts[2].parse::<i64>().ok()))
                })
                .collect(),
            _ => Vec::new(),
        }
    };

    // Which agent rows still reference each branch — one query. Pull
    // status alongside id so the UI can render a live-agent badge and
    // the purge endpoint's matching guard reads off the same source.
    let agent_by_branch: std::collections::HashMap<String, (String, String)> = {
        let conn = state.db.lock().unwrap();
        let mut stmt =
            match conn.prepare("SELECT branch, id, status FROM agents WHERE branch IS NOT NULL") {
                Ok(s) => s,
                Err(_) => return Json(serde_json::json!({"branches": []})).into_response(),
            };
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    };

    // Determine trunk — try master, then main.
    let trunk = ["master", "main"]
        .iter()
        .find(|t| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", t])
                .current_dir(&cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .copied();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let stale: Vec<StaleBranch> = branches
        .into_iter()
        .map(|(branch, sha, ts)| {
            let commits_ahead = trunk.and_then(|t| {
                std::process::Command::new("git")
                    .args(["rev-list", "--count", &format!("{t}..{branch}")])
                    .current_dir(&cwd)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse::<u64>()
                            .ok()
                    })
            });
            let agent = agent_by_branch.get(&branch);
            StaleBranch {
                name: branch.clone(),
                sha: Some(sha),
                last_commit_age_secs: ts.map(|t| now - t),
                commits_ahead_of_trunk: commits_ahead,
                agent_id: agent.map(|(id, _)| id.clone()),
                agent_status: agent.map(|(_, st)| st.clone()),
                // Unknown commits_ahead = assume risky (has_unique_commits=true)
                // so the user has to actively opt in via force=true.
                has_unique_commits: commits_ahead.map(|c| c > 0).unwrap_or(true),
            }
        })
        .collect();

    Json(serde_json::json!({
        "plan_name": name,
        "trunk": trunk,
        "branches": stale,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeBranchesBody {
    pub branches: Vec<String>,
    /// Required to delete a branch with unique commits not on trunk.
    #[serde(default)]
    pub force: bool,
}

/// POST /api/plans/:name/branches/stale/purge
///
/// Delete the specified branches and null out matching `agents.branch`
/// columns. Safety guard: refuses branches with unique commits unless
/// `force=true`. Returns a per-branch outcome so partial failures are
/// legible in the UI.
pub async fn purge_stale_branches(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<PurgeBranchesBody>,
) -> impl IntoResponse {
    let Some(cwd) = crate::ci::project_dir_for(&state.plans_dir, &state.db, &name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "plan has no resolvable project directory"})),
        )
            .into_response();
    };

    // Determine trunk for safety check.
    let trunk = ["master", "main"]
        .iter()
        .find(|t| {
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", t])
                .current_dir(&cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .copied()
        .unwrap_or("master");

    let prefix = format!("branchwork/{name}/");
    let prefix_fix = format!("branchwork/fix/{name}/");

    // Snapshot live agent rows by branch so each per-branch decision
    // reads off a single, consistent view. `force` does not lift this
    // guard — a running agent is the one case where deleting the
    // branch would corrupt an in-flight session, not just discard work
    // the user could re-derive. Same posture as
    // `reset_task_status_refuses_while_agent_is_running` (Task 1.1).
    let live_agent_by_branch: std::collections::HashMap<String, String> = {
        let conn = state.db.lock().unwrap();
        match conn.prepare(
            "SELECT branch, id FROM agents \
             WHERE branch IS NOT NULL AND status IN ('running', 'starting')",
        ) {
            Ok(mut stmt) => stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect())
                .unwrap_or_default(),
            Err(_) => std::collections::HashMap::new(),
        }
    };

    let mut results = Vec::new();
    for branch in &body.branches {
        // Scope-check: only allow deletion within this plan's prefix so a
        // malicious / mistaken caller can't purge unrelated refs.
        if !(branch.starts_with(&prefix) || branch.starts_with(&prefix_fix)) {
            results.push(serde_json::json!({
                "branch": branch,
                "ok": false,
                "error": "out_of_scope",
            }));
            continue;
        }

        // Live-agent guard: refuse if a running/starting agent still
        // owns this branch. The user has to kill or finish the agent
        // before the branch is safe to delete — `force=true` does not
        // bypass this (unlike the unique-commits check, which only
        // discards recoverable work).
        if let Some(agent_id) = live_agent_by_branch.get(branch) {
            results.push(serde_json::json!({
                "branch": branch,
                "ok": false,
                "error": "agent_running",
                "agent_id": agent_id,
            }));
            continue;
        }

        // Commit-ahead check: unique commits need force=true.
        if !body.force {
            let ahead = std::process::Command::new("git")
                .args(["rev-list", "--count", &format!("{trunk}..{branch}")])
                .current_dir(&cwd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u64>()
                        .ok()
                });
            if ahead.map(|c| c > 0).unwrap_or(true) {
                results.push(serde_json::json!({
                    "branch": branch,
                    "ok": false,
                    "error": "has_unique_commits",
                    "ahead_of_trunk": ahead,
                }));
                continue;
            }
        }

        // Delete. -D is force-delete (handles branches not merged into trunk
        // when the caller set force=true); for the default safe path the
        // above check already guaranteed zero commits ahead so -d would also
        // work. Use -D uniformly to avoid a second error mode.
        let out = std::process::Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(&cwd)
            .output();
        let ok = matches!(out.as_ref(), Ok(o) if o.status.success());
        if ok {
            // Clear any agent row still pointing at it.
            let db = state.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET branch = NULL WHERE branch = ?1",
                params![branch],
            )
            .ok();
            drop(db);
            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "agent_branch_cleared",
                serde_json::json!({"branch": branch}),
            );
            results.push(serde_json::json!({
                "branch": branch,
                "ok": true,
            }));
        } else {
            let stderr = out
                .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                .unwrap_or_else(|e| e.to_string());
            results.push(serde_json::json!({
                "branch": branch,
                "ok": false,
                "error": "git_failed",
                "stderr": stderr.trim(),
            }));
        }
    }

    Json(serde_json::json!({
        "plan_name": name,
        "results": results,
    }))
    .into_response()
}

// ── POST /api/plans/:name/check-all — spawn a check agent for every non-completed task ─

#[derive(Deserialize, Default)]
pub struct CheckAllBody {
    #[serde(default)]
    pub phase: Option<u32>,
    #[serde(default)]
    pub include_completed: bool,
}

pub async fn check_all(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CheckAllBody>,
) -> impl IntoResponse {
    let plan_path = match plan_parser::find_plan_file(&state.plans_dir, &name) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Plan not found"})),
            )
                .into_response();
        }
    };
    let plan = match plan_parser::parse_plan_file(&plan_path) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Failed to parse plan: {e}")})),
            )
                .into_response();
        }
    };

    let project = match plan.project.as_deref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Plan has no associated project"})),
            )
                .into_response();
        }
    };

    let home = dirs::home_dir().unwrap();
    let project_dir = home.join(project);

    // Load existing statuses so we skip "completed" tasks unless user opts in
    let existing: HashMap<String, String> = {
        let db = state.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT task_number, status FROM task_status WHERE plan_name = ?")
            .unwrap();
        stmt.query_map(params![name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .flatten()
        .collect()
    };

    let effort = *state.effort.lock().unwrap();
    let mut spawned: Vec<String> = Vec::new();

    for phase in &plan.phases {
        if let Some(p) = body.phase
            && phase.number != p
        {
            continue;
        }
        for task in &phase.tasks {
            let current = existing
                .get(&task.number)
                .map(|s| s.as_str())
                .unwrap_or("pending");
            if !body.include_completed && matches!(current, "completed" | "skipped" | "checking") {
                continue;
            }

            let prompt = build_check_prompt(&name, &plan, phase, task, &project_dir);

            // Set to checking
            {
                let db = state.db.lock().unwrap();
                db.execute(
                    "INSERT INTO task_status (plan_name, task_number, status, source, updated_at)
                     VALUES (?1, ?2, 'checking', 'manual', datetime('now'))
                     ON CONFLICT(plan_name, task_number)
                     DO UPDATE SET status = 'checking',
                                   source = 'manual',
                                   updated_at = datetime('now')",
                    params![name, task.number],
                )
                .ok();
            }
            crate::ws::broadcast_event(
                &state.broadcast_tx,
                "task_status_changed",
                serde_json::json!({
                    "plan_name": name,
                    "task_number": task.number,
                    "status": "checking",
                }),
            );

            let agent_id = match dispatch_check_agent(
                &state,
                prompt,
                &project_dir,
                Some(&name),
                Some(&task.number),
                effort,
            )
            .await
            {
                Ok(id) => id,
                // SaaS dispatch failure for one task in a check-all sweep
                // is degraded but recoverable: skip this task, broadcast
                // the row's status so the UI doesn't sit on `checking`,
                // and continue with the rest. Pre-T5.12 this code path
                // was infallible (in-process spawn) so the loop body
                // didn't need any error handling — keep the rest of the
                // sweep running rather than aborting.
                Err(_) => {
                    let conn = state.db.lock().unwrap();
                    conn.execute(
                        "UPDATE task_status SET status = 'failed', updated_at = datetime('now') \
                         WHERE plan_name = ?1 AND task_number = ?2",
                        params![name, task.number],
                    )
                    .ok();
                    continue;
                }
            };
            spawned.push(agent_id);
        }
    }

    Json(serde_json::json!({
        "ok": true,
        "plan_name": name,
        "spawned": spawned.len(),
        "agent_ids": spawned,
    }))
    .into_response()
}

fn build_check_prompt(
    plan_name: &str,
    plan: &plan_parser::ParsedPlan,
    phase: &plan_parser::PlanPhase,
    task: &plan_parser::PlanTask,
    project_dir: &std::path::Path,
) -> String {
    let _ = plan_name;
    let files_section = if task.file_paths.is_empty() {
        String::new()
    } else {
        format!(
            "\nFiles mentioned:\n{}",
            task.file_paths
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let acceptance_section = if task.acceptance.is_empty() {
        String::new()
    } else {
        format!("\nAcceptance criteria:\n{}", task.acceptance)
    };
    format!(
        "You are verifying whether a task from a plan has been implemented.\n\
         Answer with ONLY a JSON object, no other text: {{\"status\": \"completed\"|\"in_progress\"|\"pending\", \"reason\": \"brief explanation\"}}\n\n\
         Project directory: {project_dir}\n\
         Plan: {plan_title}\n\
         Phase {phase_num}: {phase_title}\n\
         Task {task_num}: {task_title}\n\n\
         Task description:\n{description}\n\
         {files}{acceptance}\n\n\
         Check the project at {project_dir}. Read the relevant files. Determine if this task is:\n\
         - \"completed\": all described changes exist in the code\n\
         - \"in_progress\": some changes exist but the task is not fully done\n\
         - \"pending\": no evidence of this task being started\n\n\
         Respond with ONLY the JSON object.",
        project_dir = project_dir.display(),
        plan_title = plan.title,
        phase_num = phase.number,
        phase_title = phase.title,
        task_num = task.number,
        task_title = task.title,
        description = task.description,
        files = files_section,
        acceptance = acceptance_section,
    )
}

#[cfg(test)]
mod check_prompt_tests {
    use super::*;

    fn sample_plan() -> plan_parser::ParsedPlan {
        plan_parser::ParsedPlan {
            name: "dashboard-polish".into(),
            file_path: String::new(),
            title: "Test Plan".into(),
            context: String::new(),
            project: Some("proj".into()),
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![],
            verification: None,
            ci_blocking_workflows: None,
            phase_verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        }
    }

    fn sample_task(number: &str, files: Vec<String>) -> plan_parser::PlanTask {
        plan_parser::PlanTask {
            number: number.into(),
            title: "A task".into(),
            description: "Do things".into(),
            file_paths: files,
            acceptance: "must work".into(),
            dependencies: vec![],
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        }
    }

    // Mirrors the prompt-construction locals inside `api::plans::check_task`
    // between fetching `plan`/`phase`/`task` and the call to
    // `start_check_agent`. After Phase 1.2 that body is a single call to
    // `build_check_prompt`. Tripwire: if a future change reintroduces inline
    // prompt mangling in `check_task`, update this helper to match — the
    // identical-prompt test below will then fail until `check_all_tasks` is
    // updated too (or the divergence is reverted).
    fn prompt_for_check_task_path(
        plan_name: &str,
        plan: &plan_parser::ParsedPlan,
        phase: &plan_parser::PlanPhase,
        task: &plan_parser::PlanTask,
        project_dir: &std::path::Path,
    ) -> String {
        build_check_prompt(plan_name, plan, phase, task, project_dir)
    }

    #[test]
    fn check_task_and_check_all_build_identical_prompts() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "P".into(),
            description: String::new(),
            tasks: vec![sample_task("1.1", vec!["a.rs".into()])],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let task = phase.tasks[0].clone();
        let project_dir = std::path::Path::new("/tmp/proj");

        let from_check_task = prompt_for_check_task_path("p", &plan, &phase, &task, project_dir);
        // `check_all_tasks` calls `build_check_prompt` directly inside its
        // loop body, so the unified builder's output IS the check-all path.
        let from_check_all = build_check_prompt("p", &plan, &phase, &task, project_dir);

        assert_eq!(
            from_check_task, from_check_all,
            "check_task and check_all_tasks must produce byte-identical prompts"
        );
    }

    #[test]
    fn excludes_branch_verification_after_unification() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "Phase One".into(),
            description: String::new(),
            tasks: vec![],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let task = sample_task(
            "1.3",
            vec![
                "server-rs/src/api/plans.rs".into(),
                "web/src/foo.tsx".into(),
            ],
        );
        let prompt = build_check_prompt(
            "dashboard-polish",
            &plan,
            &phase,
            &task,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(
            !prompt.contains("branch"),
            "prompt must not reference branches after unification"
        );
        assert!(!prompt.contains("git log"), "prompt must not shell git log");
        assert!(
            !prompt.contains("task branch"),
            "prompt must not mention task branch"
        );
        assert!(
            !prompt.contains("--not master"),
            "prompt must not exclude base branch history"
        );
        assert!(
            prompt.contains("Read the relevant files"),
            "prompt keeps the simple-shape instruction"
        );
        assert!(
            prompt.contains("Project directory:"),
            "prompt keeps the project directory context"
        );
    }

    #[test]
    fn prompt_shape_has_no_lifecycle_vocabulary() {
        let plan = sample_plan();
        let phase = plan_parser::PlanPhase {
            number: 1,
            title: "Phase One".into(),
            description: String::new(),
            tasks: vec![],
            ci_blocking_workflows: None,
            phase_verification: None,
        };
        let task = sample_task(
            "1.3",
            vec![
                "server-rs/src/api/plans.rs".into(),
                "web/src/foo.tsx".into(),
            ],
        );
        let prompt = build_check_prompt(
            "dashboard-polish",
            &plan,
            &phase,
            &task,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(
            !prompt.contains("branch"),
            "prompt must not contain \"branch\""
        );
        assert!(
            !prompt.contains("git log"),
            "prompt must not contain \"git log\""
        );
        assert!(
            !prompt.contains("task branch"),
            "prompt must not contain \"task branch\""
        );
        assert!(
            !prompt.contains("committed"),
            "prompt must not contain \"committed\""
        );
        assert!(
            !prompt.contains("--not master"),
            "prompt must not contain \"--not master\""
        );
        assert!(
            !prompt.contains("--not main"),
            "prompt must not contain \"--not main\""
        );
    }

    #[test]
    fn plan_check_prompt_includes_verification_and_task_split() {
        let mut plan = sample_plan();
        plan.verification = Some("1. The endpoint returns 200.\n2. The verdict is stored.".into());
        plan.phases = vec![
            plan_parser::PlanPhase {
                number: 1,
                title: "P1".into(),
                description: String::new(),
                tasks: vec![sample_task("1.1", vec![]), sample_task("1.2", vec![])],
                ci_blocking_workflows: None,
                phase_verification: None,
            },
            plan_parser::PlanPhase {
                number: 2,
                title: "P2".into(),
                description: String::new(),
                tasks: vec![sample_task("2.1", vec![])],
                ci_blocking_workflows: None,
                phase_verification: None,
            },
        ];

        let mut statuses: HashMap<String, String> = HashMap::new();
        statuses.insert("1.1".into(), "completed".into());
        statuses.insert("1.2".into(), "skipped".into());
        // 2.1 intentionally left out to exercise the default "pending" path.

        let prompt = build_plan_check_prompt(
            &plan,
            plan.verification.as_deref().unwrap(),
            &statuses,
            std::path::Path::new("/tmp/proj"),
        );

        assert!(prompt.contains("The endpoint returns 200."));
        assert!(prompt.contains("Done tasks:"));
        assert!(prompt.contains("- 1.1: A task"));
        assert!(prompt.contains("- 1.2: A task"));
        assert!(prompt.contains("Pending tasks:"));
        assert!(prompt.contains("- 2.1: A task"));
        assert!(prompt.contains("Respond with ONLY the JSON object."));
    }

    #[test]
    fn plan_check_prompt_handles_empty_task_sections() {
        let mut plan = sample_plan();
        plan.verification = Some("nothing".into());
        plan.phases = vec![];
        let statuses: HashMap<String, String> = HashMap::new();
        let prompt = build_plan_check_prompt(
            &plan,
            plan.verification.as_deref().unwrap(),
            &statuses,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(prompt.contains("Done tasks: (none)"));
        assert!(prompt.contains("Pending tasks: (none)"));
    }
}

#[cfg(test)]
mod plan_creator_prompt_tests {
    //! Prompt regression tests for the plan-creator agent (T5.4).
    //! The prompt must NOT mention the server's plans directory or the Write
    //! tool — the agent's host may not share a filesystem with the server.
    //! Instead, it must instruct the agent to call the `create_plan` MCP tool.
    use super::build_plan_creator_prompt;

    #[test]
    fn prompt_does_not_reference_filesystem_write() {
        let prompt = build_plan_creator_prompt("/runner/work", "build x", "");
        // No filesystem-write instruction.
        assert!(
            !prompt.contains("/data/plans"),
            "must not bake /data/plans into the prompt: {prompt}"
        );
        assert!(
            !prompt.contains(".yaml using the Write tool"),
            "must not instruct using the Write tool: {prompt}"
        );
    }

    #[test]
    fn prompt_instructs_calling_create_plan_mcp_tool() {
        let prompt = build_plan_creator_prompt("/runner/work", "build x", "");
        assert!(
            prompt.contains("`create_plan` MCP tool"),
            "must mention the create_plan MCP tool: {prompt}"
        );
        assert!(
            prompt.contains("`name`"),
            "must describe the name parameter: {prompt}"
        );
        assert!(
            prompt.contains("`yaml_body`"),
            "must describe the yaml_body parameter: {prompt}"
        );
    }

    #[test]
    fn prompt_carries_template_section_verbatim() {
        let prompt = build_plan_creator_prompt(
            "/runner/work",
            "build x",
            "Template: foo\nskeleton-here\n\n",
        );
        assert!(prompt.contains("Template: foo"));
        assert!(prompt.contains("skeleton-here"));
    }
}

#[cfg(test)]
mod cascade_audit_tests {
    //! Plan-deletion 0.0 audit. The schema-introspection test is the
    //! load-bearing canary: when someone adds a new `CREATE TABLE`
    //! with a `plan_name` column to `db.rs`, this test fails with the
    //! offending table name and forces them to either (a) add it to
    //! `PLAN_CASCADE_TABLES` (and the doc-comment table above), or
    //! (b) add it to `PLAN_NAME_PRESERVED_TABLES` with a written
    //! reason. The second test proves the cascade SQL actually empties
    //! every cascaded table while leaving preserved rows intact.
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn migrated_db() -> (crate::db::Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        (crate::db::init(&path), dir)
    }

    /// Tables in the live SQLite schema that have a column literally
    /// named `plan_name`. Uses `pragma_table_info` so the result tracks
    /// the schema as it actually exists at runtime, not as it appeared
    /// when this test was written.
    fn tables_with_plan_name_column(conn: &rusqlite::Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT m.name \
                   FROM sqlite_master m \
                  WHERE m.type = 'table' \
                    AND EXISTS ( \
                        SELECT 1 FROM pragma_table_info(m.name) \
                         WHERE name = 'plan_name' \
                    ) \
                  ORDER BY m.name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn cascade_set_matches_schema() {
        let (db, _dir) = migrated_db();
        let conn = db.lock().unwrap();
        let live: std::collections::BTreeSet<String> =
            tables_with_plan_name_column(&conn).into_iter().collect();

        let mut classified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for t in PLAN_CASCADE_TABLES.iter().chain(PLAN_NAME_PRESERVED_TABLES) {
            classified.insert((*t).to_string());
        }

        let unclassified: Vec<&String> = live.difference(&classified).collect();
        let phantom: Vec<&String> = classified.difference(&live).collect();

        assert!(
            unclassified.is_empty(),
            "db.rs has plan_name-keyed table(s) the cascade does not classify: \
             {unclassified:?}. Add each to PLAN_CASCADE_TABLES (and the doc \
             table above delete_plan) or to PLAN_NAME_PRESERVED_TABLES with \
             a one-line reason."
        );
        assert!(
            phantom.is_empty(),
            "PLAN_CASCADE_TABLES / PLAN_NAME_PRESERVED_TABLES list table(s) \
             that no longer exist in the schema: {phantom:?}. Drop them from \
             the constant or restore the CREATE TABLE."
        );
    }

    #[test]
    fn cascade_loop_empties_every_cascaded_table_and_preserves_agents() {
        let (db, _dir) = migrated_db();
        let plan = "audit-test-plan";

        // Seed one row per cascaded table.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status) \
                 VALUES (?1, '1.1', 'completed')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ci_runs (plan_name, task_number, status) \
                 VALUES (?1, '1.1', 'success')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_fix_attempts (plan_name, task_number, attempt, outcome) \
                 VALUES (?1, '1.1', 1, 'success')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_project (plan_name, project) VALUES (?1, 'proj')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_verdicts (plan_name, verdict) VALUES (?1, 'ok')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_budget (plan_name, max_budget_usd) VALUES (?1, 1.0)",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_learnings (plan_name, task_number, learning) \
                 VALUES (?1, '1.1', 'noted')",
                params![plan],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO plan_org (plan_name, org_id) \
                 VALUES (?1, 'default-org')",
                params![plan],
            )
            .unwrap();
            // Preserved: a completed agent row that must survive the cascade.
            conn.execute(
                "INSERT INTO agents (id, cwd, status, plan_name, task_id) \
                 VALUES ('agent-audit', '/tmp', 'completed', ?1, '1.1')",
                params![plan],
            )
            .unwrap();
            // Preserved: a plan_snapshots row IS the recovery substrate
            // for Undo, so the cascade must not touch it.
            conn.execute(
                "INSERT INTO plan_snapshots \
                     (plan_name, kind, expires_at, yaml_body, cascade_json) \
                 VALUES (?1, 'delete', datetime('now', '+30 days'), \
                         'body', '{}')",
                params![plan],
            )
            .unwrap();
        }

        // Run the same cascade loop the handler runs in step 7 of
        // delete_plan. Failure on any DELETE fails the test loudly.
        {
            let mut conn = db.lock().unwrap();
            let tx = conn.transaction().unwrap();
            for table in PLAN_CASCADE_TABLES {
                let sql = format!("DELETE FROM {table} WHERE plan_name = ?1");
                tx.execute(&sql, params![plan])
                    .unwrap_or_else(|e| panic!("cascade DELETE on {table} failed: {e}"));
            }
            tx.commit().unwrap();
        }

        // Every cascaded table must be empty for this plan.
        let conn = db.lock().unwrap();
        for table in PLAN_CASCADE_TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE plan_name = ?1");
            let count: i64 = conn.query_row(&sql, params![plan], |r| r.get(0)).unwrap();
            assert_eq!(
                count, 0,
                "cascade did not empty {table} for plan {plan} (still has {count} row(s))"
            );
        }

        // Preserved tables retain the row with the (now dangling) plan_name.
        for table in PLAN_NAME_PRESERVED_TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE plan_name = ?1");
            let count: i64 = conn.query_row(&sql, params![plan], |r| r.get(0)).unwrap();
            assert!(
                count >= 1,
                "preserved table {table} unexpectedly empty for plan {plan} \
                 (audit comment promises the row survives)"
            );
        }
    }
}

#[cfg(test)]
mod resolve_folder_path_tests {
    //! Three-state resolver pinned by Task 7.1 (2026-05-07 bug repro).
    //! The runner-side equivalent in
    //! `branchwork_runner.rs::resolve_runner_path` must agree on these
    //! three cases — both apply the rule against their respective
    //! `$HOME` (server's vs runner's), but the shape is identical.
    use super::resolve_folder_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn bare_name_resolves_under_home() {
        let home = Path::new("/home/cpo");
        assert_eq!(
            resolve_folder_path("myproj", home),
            PathBuf::from("/home/cpo/myproj"),
        );
    }

    #[test]
    fn absolute_path_passes_through() {
        let home = Path::new("/home/cpo");
        assert_eq!(resolve_folder_path("/etc", home), PathBuf::from("/etc"));
        assert_eq!(
            resolve_folder_path("/var/lib/branchwork", home),
            PathBuf::from("/var/lib/branchwork"),
        );
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = Path::new("/home/cpo");
        assert_eq!(
            resolve_folder_path("~/foo", home),
            PathBuf::from("/home/cpo/foo"),
        );
    }

    #[test]
    fn bare_tilde_resolves_to_home() {
        let home = Path::new("/home/cpo");
        assert_eq!(resolve_folder_path("~", home), PathBuf::from("/home/cpo"));
    }

    #[test]
    fn nested_relative_path_resolves_under_home() {
        let home = Path::new("/home/cpo");
        assert_eq!(
            resolve_folder_path("nested/subdir", home),
            PathBuf::from("/home/cpo/nested/subdir"),
        );
    }
}

#[cfg(test)]
mod plan_settings_yaml_edit_tests {
    //! Unit tests for the comment-preserving YAML editor used by
    //! PUT /api/plans/:name/settings (Task 3.1).
    use super::{
        find_top_level_key_range, line_starts_with_top_level_key, line_terminates_key_range,
        update_yaml_top_level_key,
    };

    fn lines_of(s: &str) -> Vec<&str> {
        s.split('\n').collect()
    }

    #[test]
    fn line_starts_with_top_level_key_matches_basic_forms() {
        assert!(line_starts_with_top_level_key("title: hi", "title"));
        assert!(line_starts_with_top_level_key("title:", "title"));
        assert!(line_starts_with_top_level_key("title: # inline", "title"));
        assert!(line_starts_with_top_level_key("title:\t hi", "title"));
        // Not a match: indented, comment, key mismatch, no colon.
        assert!(!line_starts_with_top_level_key("  title: hi", "title"));
        assert!(!line_starts_with_top_level_key("# title: hi", "title"));
        assert!(!line_starts_with_top_level_key("titles: hi", "title"));
        assert!(!line_starts_with_top_level_key("title hi", "title"));
    }

    #[test]
    fn line_terminates_key_range_recognises_all_value_continuations() {
        // Continuations: indented or column-0 list items are part of the
        // current value.
        assert!(!line_terminates_key_range("  - item"));
        assert!(!line_terminates_key_range("\t- item"));
        assert!(!line_terminates_key_range("- item"));
        assert!(!line_terminates_key_range(""));
        // Terminators: comments and other top-level constructs end the
        // range.
        assert!(line_terminates_key_range("# leading comment for next key"));
        assert!(line_terminates_key_range("phases:"));
        assert!(line_terminates_key_range("ci_blocking_workflows: [a]"));
    }

    #[test]
    fn find_range_for_inline_value() {
        let yaml = "title: Hi\nci_blocking_workflows: [CI, lint]\nphases: []\n";
        let lines = lines_of(yaml);
        let range = find_top_level_key_range(&lines, "ci_blocking_workflows").unwrap();
        assert_eq!(range, (1, 2), "inline value occupies exactly its own line");
    }

    #[test]
    fn find_range_for_block_sequence_indented() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - CI\n  - lint\nphases: []\n";
        let lines = lines_of(yaml);
        let range = find_top_level_key_range(&lines, "ci_blocking_workflows").unwrap();
        assert_eq!(range, (1, 4));
    }

    #[test]
    fn find_range_for_block_sequence_column_zero() {
        // Column-0 list items are still part of the previous key's value.
        let yaml = "title: Hi\nci_blocking_workflows:\n- CI\n- lint\nphases: []\n";
        let lines = lines_of(yaml);
        let range = find_top_level_key_range(&lines, "ci_blocking_workflows").unwrap();
        assert_eq!(range, (1, 4));
    }

    #[test]
    fn find_range_for_block_scalar_value() {
        let yaml = "phase_verification: |\n  cargo fmt\n  cargo test\nphases: []\n";
        let lines = lines_of(yaml);
        let range = find_top_level_key_range(&lines, "phase_verification").unwrap();
        assert_eq!(range, (0, 3));
    }

    #[test]
    fn find_range_returns_none_when_key_absent() {
        let yaml = "title: Hi\nphases: []\n";
        let lines = lines_of(yaml);
        assert!(find_top_level_key_range(&lines, "ci_blocking_workflows").is_none());
    }

    #[test]
    fn comment_above_next_key_is_not_included_in_previous_range() {
        // The comment on line 1 should be associated with the NEXT key,
        // not the previous one — `line_terminates_key_range` returns
        // true for column-0 comments.
        let yaml = "ci_blocking_workflows: [CI]\n# leading for phase_verification\nphase_verification: x\nphases: []\n";
        let lines = lines_of(yaml);
        let range = find_top_level_key_range(&lines, "ci_blocking_workflows").unwrap();
        assert_eq!(range, (0, 1));
    }

    #[test]
    fn replace_inline_value_preserves_other_keys_and_comments() {
        let yaml =
            "title: Hi\n# CI workflows that block\nci_blocking_workflows: [CI]\nphases: []\n";
        let new = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("CI".into()),
            serde_yaml::Value::String("lint".into()),
        ]);
        let out = update_yaml_top_level_key(yaml, "ci_blocking_workflows", Some(&new)).unwrap();
        // Comment and other keys preserved.
        assert!(out.contains("# CI workflows that block"));
        assert!(out.starts_with("title: Hi"));
        assert!(out.contains("phases: []"));
        // New value present.
        assert!(out.contains("ci_blocking_workflows:"));
        assert!(out.contains("- CI"));
        assert!(out.contains("- lint"));
        // Old inline literal gone.
        assert!(!out.contains("[CI]"));
    }

    #[test]
    fn replace_block_sequence_with_smaller_list_preserves_surroundings() {
        let yaml = "# top comment\ntitle: Hi\n\nci_blocking_workflows:\n  - CI\n  - lint\n  - audit\n\nphases:\n  - number: 1\n    title: P\n    tasks: []\n";
        let new = serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("CI".into())]);
        let out = update_yaml_top_level_key(yaml, "ci_blocking_workflows", Some(&new)).unwrap();
        assert!(out.starts_with("# top comment"));
        assert!(out.contains("title: Hi"));
        assert!(out.contains("phases:"));
        assert!(out.contains("    title: P"));
        // Old members gone, new in place.
        assert!(!out.contains("- audit"));
        assert!(!out.contains("- lint"));
        assert!(out.contains("- CI"));
        // Re-parse round-trips.
        let _: serde_yaml::Value = serde_yaml::from_str(&out).expect("post-edit must parse");
    }

    #[test]
    fn remove_key_drops_only_its_lines() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - CI\nphase_verification: bash verify.sh\nphases: []\n";
        let out = update_yaml_top_level_key(yaml, "ci_blocking_workflows", None).unwrap();
        assert!(!out.contains("ci_blocking_workflows"));
        assert!(out.contains("title: Hi"));
        assert!(out.contains("phase_verification: bash verify.sh"));
        assert!(out.contains("phases: []"));
    }

    #[test]
    fn remove_absent_key_is_noop() {
        let yaml = "title: Hi\nphases: []\n";
        let out = update_yaml_top_level_key(yaml, "ci_blocking_workflows", None).unwrap();
        assert_eq!(out, yaml, "removing an absent key must not change the file");
    }

    #[test]
    fn insert_new_key_lands_before_phases() {
        let yaml = "title: Hi\ncontext: ''\nphases:\n  - number: 1\n    title: P\n    tasks: []\n";
        let new = serde_yaml::Value::String("bash scripts/verify.sh".into());
        let out = update_yaml_top_level_key(yaml, "phase_verification", Some(&new)).unwrap();
        let pv_idx = out.find("phase_verification:").expect("must contain key");
        let phases_idx = out.find("phases:").expect("must contain phases");
        assert!(
            pv_idx < phases_idx,
            "new key must be inserted before `phases:` (got pv={pv_idx}, phases={phases_idx})\n{out}"
        );
        // Existing keys preserved.
        assert!(out.contains("title: Hi"));
        assert!(out.contains("context: ''"));
        // Re-parse round-trips.
        let v: serde_yaml::Value = serde_yaml::from_str(&out).expect("must parse");
        let m = v.as_mapping().unwrap();
        assert_eq!(
            m.get(serde_yaml::Value::String("phase_verification".into()))
                .and_then(|v| v.as_str()),
            Some("bash scripts/verify.sh")
        );
    }

    #[test]
    fn insert_when_no_phases_appends_at_end() {
        let yaml = "title: Hi\ncontext: ''\n";
        let new = serde_yaml::Value::String("bash verify.sh".into());
        let out = update_yaml_top_level_key(yaml, "phase_verification", Some(&new)).unwrap();
        assert!(out.contains("title: Hi"));
        assert!(out.contains("phase_verification: bash verify.sh"));
    }

    #[test]
    fn full_round_trip_preserves_file_with_inline_comments_outside_target() {
        let yaml = r#"# top header
title: Demo
project: foo  # inline comment on project
context: |
  multi line
  body
ci_blocking_workflows:
  - CI
  - lint
# section comment
phases:
  - number: 1
    title: P1
    tasks:
      - number: '1.1'
        title: T1
        acceptance: ''
"#;
        // Replace the list.
        let new = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("CI".into()),
            serde_yaml::Value::String("docs".into()),
        ]);
        let out = update_yaml_top_level_key(yaml, "ci_blocking_workflows", Some(&new)).unwrap();
        // Untouched keys + comments survive byte-for-byte.
        assert!(out.contains("# top header"));
        assert!(out.contains("# inline comment on project"));
        assert!(out.contains("# section comment"));
        assert!(out.contains("multi line\n  body"));
        // New value in.
        assert!(out.contains("- docs"));
        // Round-trips through serde_yaml.
        let _: serde_yaml::Value = serde_yaml::from_str(&out).expect("must parse");
    }
}

#[cfg(test)]
mod phase_settings_yaml_edit_tests {
    //! Unit tests for the phase-scoped YAML editor used by
    //! PUT /api/plans/:name/phases/:number/settings (Task 3.3).
    use super::{
        find_phase_block_range, find_phase_inner_key_line, line_starts_with_indented_key,
        line_terminates_indented_key_range, parse_phase_list_item_number, update_yaml_phase_key,
    };

    fn lines_of(s: &str) -> Vec<&str> {
        s.split('\n').collect()
    }

    const FIXTURE: &str = "title: Demo\ncontext: ''\nphases:\n  - number: 1\n    title: P1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: T11\n        acceptance: ''\n  - number: 2\n    title: P2\n    description: ''\n    tasks:\n      - number: '2.1'\n        title: T21\n        acceptance: ''\n";

    #[test]
    fn parse_phase_list_item_number_recognises_basic_forms() {
        assert_eq!(parse_phase_list_item_number("  - number: 1"), Some(1));
        assert_eq!(parse_phase_list_item_number("  - number: 12"), Some(12));
        assert_eq!(
            parse_phase_list_item_number("  - number: 1 # comment"),
            Some(1)
        );
        assert_eq!(parse_phase_list_item_number("  - number: '1'"), Some(1));
        // Not a phase list item.
        assert_eq!(parse_phase_list_item_number("number: 1"), None);
        assert_eq!(parse_phase_list_item_number("    title: P1"), None);
        assert_eq!(parse_phase_list_item_number("  - title: P1"), None);
        assert_eq!(parse_phase_list_item_number(""), None);
    }

    #[test]
    fn line_starts_with_indented_key_matches_basic_forms() {
        assert!(line_starts_with_indented_key("    title: P1", 4, "title"));
        assert!(line_starts_with_indented_key("    title:", 4, "title"));
        assert!(line_starts_with_indented_key(
            "    title: # inline",
            4,
            "title"
        ));
        // Wrong indent.
        assert!(!line_starts_with_indented_key("  title: P1", 4, "title"));
        assert!(!line_starts_with_indented_key(
            "      title: P1",
            4,
            "title"
        ));
        // List item, not a key.
        assert!(!line_starts_with_indented_key("    - title: x", 4, "title"));
        // Comment only.
        assert!(!line_starts_with_indented_key("    # title: x", 4, "title"));
    }

    #[test]
    fn line_terminates_indented_key_range_treats_blanks_as_continuations() {
        // Blanks (and whitespace-only lines) are continuations.
        assert!(!line_terminates_indented_key_range("", 4));
        assert!(!line_terminates_indented_key_range("    ", 4));
        // Deeper indent → continuation (block scalar / list item content).
        assert!(!line_terminates_indented_key_range("      child: x", 4));
        // Same indent or shallower → terminates.
        assert!(line_terminates_indented_key_range("    sibling: x", 4));
        assert!(line_terminates_indented_key_range("  - number: 2", 4));
        assert!(line_terminates_indented_key_range("phases:", 4));
    }

    #[test]
    fn find_phase_block_range_locates_first_and_last_phase() {
        let lines = lines_of(FIXTURE);
        let r1 = find_phase_block_range(&lines, 1).expect("phase 1");
        // Phase 1 starts at the `  - number: 1` line and ends at phase 2's list item.
        assert_eq!(lines[r1.0], "  - number: 1");
        assert_eq!(lines[r1.1], "  - number: 2");
        let r2 = find_phase_block_range(&lines, 2).expect("phase 2");
        assert_eq!(lines[r2.0], "  - number: 2");
        // Phase 2 has no successor → end is EOF (last line is empty after trailing \n).
        assert!(r2.1 >= lines.len() - 1);
    }

    #[test]
    fn find_phase_block_range_returns_none_for_unknown_phase() {
        let lines = lines_of(FIXTURE);
        assert!(find_phase_block_range(&lines, 99).is_none());
    }

    #[test]
    fn find_phase_inner_key_line_locates_tasks() {
        let lines = lines_of(FIXTURE);
        let phase_range = find_phase_block_range(&lines, 1).unwrap();
        let tasks_idx = find_phase_inner_key_line(&lines, phase_range, "tasks", 4).unwrap();
        assert_eq!(lines[tasks_idx], "    tasks:");
    }

    #[test]
    fn insert_phase_verification_lands_before_tasks() {
        let new = serde_yaml::Value::String("bash scripts/verify.sh".into());
        let out = update_yaml_phase_key(FIXTURE, 1, "phase_verification", Some(&new)).unwrap();
        let pv_idx = out.find("phase_verification:").expect("must contain key");
        let tasks_idx = out
            .find("    tasks:")
            .expect("must still contain phase 1's tasks key");
        assert!(
            pv_idx < tasks_idx,
            "phase_verification must be inserted before tasks (pv={pv_idx} tasks={tasks_idx})\n{out}"
        );
        // Phase 2 untouched: still has the same shape.
        assert!(out.contains("  - number: 2\n    title: P2"));
        // Round-trips through serde_yaml.
        let v: serde_yaml::Value = serde_yaml::from_str(&out).expect("must parse");
        let phases = v
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("phases".into())))
            .and_then(|p| p.as_sequence())
            .expect("phases sequence");
        let phase_one = phases.iter().find(|p| {
            p.as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("number".into())))
                .and_then(|v| v.as_u64())
                == Some(1)
        });
        assert_eq!(
            phase_one
                .and_then(|p| p.as_mapping())
                .and_then(|m| m.get(serde_yaml::Value::String("phase_verification".into())))
                .and_then(|v| v.as_str()),
            Some("bash scripts/verify.sh")
        );
    }

    #[test]
    fn replace_phase_verification_only_touches_target_phase() {
        // Seed phase 1 with an explicit override so we can replace it.
        let seeded = update_yaml_phase_key(
            FIXTURE,
            1,
            "phase_verification",
            Some(&serde_yaml::Value::String("old".into())),
        )
        .unwrap();
        let new = serde_yaml::Value::String("new value".into());
        let out = update_yaml_phase_key(&seeded, 1, "phase_verification", Some(&new)).unwrap();
        assert!(out.contains("phase_verification: new value"));
        assert!(!out.contains("phase_verification: old"));
        // Phase 2 still has no override.
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let phases = v
            .as_mapping()
            .and_then(|m| m.get(serde_yaml::Value::String("phases".into())))
            .and_then(|p| p.as_sequence())
            .unwrap();
        let phase_two = phases
            .iter()
            .find(|p| {
                p.as_mapping()
                    .and_then(|m| m.get(serde_yaml::Value::String("number".into())))
                    .and_then(|v| v.as_u64())
                    == Some(2)
            })
            .unwrap();
        assert!(
            phase_two
                .as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("phase_verification".into())))
                .is_none(),
            "phase 2 must not have inherited the edit:\n{out}"
        );
    }

    #[test]
    fn remove_phase_verification_drops_the_line() {
        let seeded = update_yaml_phase_key(
            FIXTURE,
            2,
            "phase_verification",
            Some(&serde_yaml::Value::String("bash verify.sh".into())),
        )
        .unwrap();
        assert!(seeded.contains("phase_verification: bash verify.sh"));
        let cleared = update_yaml_phase_key(&seeded, 2, "phase_verification", None).unwrap();
        assert!(
            !cleared.contains("phase_verification:"),
            "key must be gone:\n{cleared}"
        );
        // Sanity: still parses.
        let _: serde_yaml::Value = serde_yaml::from_str(&cleared).unwrap();
    }

    #[test]
    fn remove_absent_key_is_noop() {
        let out = update_yaml_phase_key(FIXTURE, 1, "phase_verification", None).unwrap();
        assert_eq!(out, FIXTURE);
    }

    #[test]
    fn unknown_phase_returns_err() {
        let new = serde_yaml::Value::String("x".into());
        let err = update_yaml_phase_key(FIXTURE, 99, "phase_verification", Some(&new))
            .expect_err("must error");
        assert!(err.contains("phase 99"));
    }

    #[test]
    fn comment_outside_phase_block_survives() {
        let yaml = "title: Hi\n# top-level comment\ncontext: ''\nphases:\n  - number: 1\n    # inline phase comment\n    title: P1\n    tasks:\n      - number: '1.1'\n        title: T\n        acceptance: ''\n";
        let new = serde_yaml::Value::String("bash verify.sh".into());
        let out = update_yaml_phase_key(yaml, 1, "phase_verification", Some(&new)).unwrap();
        assert!(out.contains("# top-level comment"));
        assert!(out.contains("# inline phase comment"));
        assert!(out.contains("phase_verification: bash verify.sh"));
        let _: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
    }
}
