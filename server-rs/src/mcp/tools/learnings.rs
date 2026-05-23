//! Phase 1, Task 1.3: structured `capture_learning` MCP tool.
//!
//! The CI-failure capture surface from a fix-agent's point of view:
//! - Phase 1.1 (`ci_failure_events`) records that a CI run went red on a
//!   tracked branch.
//! - Phase 1.2 (pending-learning gate) refuses to mark the offending task
//!   `completed` until that row is resolved.
//! - This tool (Phase 1.3) is how the fix-agent resolves it: capture a
//!   structured lesson, write it to the org-shared `learnings` table, and
//!   clear the pending flag in one shot.
//!
//! Validation is intentionally strict — three semantic fields plus a
//! category, each refused if too short or vague. The `prevention_rule`
//! must carry a backtick-quoted command so the rule reads as actionable
//! instead of descriptive (the manual-rule guidance from the auto-memory
//! `feedback` type, applied to org-shared lessons).

use rmcp::{ErrorData as McpError, Json, handler::server::wrapper::Parameters, tool, tool_router};
use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{LearningInput, LearningKind};
use crate::mcp::BranchworkMcp;

/// Minimum length for `failure_summary` / `root_cause` / `prevention_rule`.
/// The task brief asks for ~40 chars; pick a round number so the
/// validation message can quote the exact limit.
const MIN_FIELD_LEN: usize = 40;

// ── Request / response schemas ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CaptureLearningRequest {
    /// Plan name (file stem, e.g. `my-plan` for `my-plan.yaml`).
    pub plan: String,
    /// Task number as it appears in the plan, e.g. `2.3`.
    pub task: String,
    /// One- or two-sentence summary of what went wrong. Concrete enough
    /// that a future reader can recognise the symptom without opening the
    /// failing run.
    pub failure_summary: String,
    /// Why the failure happened. Should name the specific mechanism or
    /// missing precondition, not just the surface error message.
    pub root_cause: String,
    /// A short rule for how a future agent avoids the same failure. MUST
    /// contain at least one backtick-quoted command (e.g. "Run
    /// `cargo fmt --check` before commit") so the rule is verifiable.
    pub prevention_rule: String,
    /// Short label grouping related lessons, e.g. `ci-rustfmt` or
    /// `merge-base-drift`. Slugified to form the learning's slug prefix.
    pub category: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CaptureLearningResponse {
    pub ok: bool,
    pub plan_name: String,
    pub task_number: String,
    pub learning_id: String,
    pub slug: String,
    pub kind: String,
    pub resolved_failures: usize,
}

/// Structured per-field validation error. Embedded in the `data` payload
/// of the `McpError` so an agent can react programmatically — match on
/// `errors[*].field` to know exactly which input to rewrite, instead of
/// regex-parsing the human-readable message.
#[derive(Debug, Clone, Serialize)]
struct FieldError {
    field: &'static str,
    message: String,
}

// ── Pure helpers (tested directly) ──────────────────────────────────────────

fn validate(req: &CaptureLearningRequest) -> Vec<FieldError> {
    let mut errs = Vec::new();
    check_len("failure_summary", &req.failure_summary, &mut errs);
    check_len("root_cause", &req.root_cause, &mut errs);
    check_len("prevention_rule", &req.prevention_rule, &mut errs);

    if req.category.trim().is_empty() {
        errs.push(FieldError {
            field: "category",
            message: "category is required (e.g. `ci-rustfmt`, `merge-base-drift`)".to_string(),
        });
    }

    if !has_backtick_command(&req.prevention_rule) {
        errs.push(FieldError {
            field: "prevention_rule",
            message: "prevention_rule must include a backtick-quoted command so the rule is \
                      actionable (e.g. \"Run `cargo fmt --check` before commit\")"
                .to_string(),
        });
    }
    errs
}

fn check_len(field: &'static str, value: &str, errs: &mut Vec<FieldError>) {
    let len = value.trim().chars().count();
    if len < MIN_FIELD_LEN {
        errs.push(FieldError {
            field,
            message: format!(
                "{field} must be at least {MIN_FIELD_LEN} characters (got {len}). Write a \
                 concrete sentence — a future agent reads this without context."
            ),
        });
    }
}

/// True iff `s` contains a `` `...` `` pair with non-whitespace content.
/// Used to enforce the actionable constraint on `prevention_rule` without
/// pulling in the `regex` crate just for one predicate.
fn has_backtick_command(s: &str) -> bool {
    if let Some(open) = s.find('`')
        && let Some(close_rel) = s[open + 1..].find('`')
    {
        let content = &s[open + 1..open + 1 + close_rel];
        return !content.trim().is_empty();
    }
    false
}

fn build_body(req: &CaptureLearningRequest) -> String {
    format!(
        "## Failure summary\n\n{summary}\n\n\
         ## Root cause\n\n{cause}\n\n\
         ## Prevention rule\n\n{rule}\n",
        summary = req.failure_summary.trim(),
        cause = req.root_cause.trim(),
        rule = req.prevention_rule.trim(),
    )
}

/// Build a slug of the form `{kebab(category)}-{uuid8}`. The 8-char UUID
/// suffix guarantees the `(org_id, kind, slug)` unique index never trips
/// on repeat captures with the same category, while keeping the slug
/// recognisable in dashboard listings (filter by prefix → lessons in
/// that category). A category that slugifies to empty falls back to
/// `learning` so we always have a non-empty prefix.
fn build_slug(category: &str) -> String {
    let mut collapsed = String::with_capacity(category.len());
    let mut prev_dash = false;
    for c in category.trim().chars() {
        if c.is_ascii_alphanumeric() {
            collapsed.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            collapsed.push('-');
            prev_dash = true;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let prefix = if trimmed.is_empty() {
        "learning"
    } else {
        trimmed
    };
    let suffix = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &suffix[..8])
}

// ── Tool ────────────────────────────────────────────────────────────────────

#[tool_router(router = learnings_router, vis = "pub")]
impl BranchworkMcp {
    #[tool(
        description = "Capture an org-shared learning explaining a CI failure on the given \
                       task and how to avoid it. Writes a row to the org `learnings` table \
                       and clears the pending-learning gate so the task can be marked \
                       completed. Each of failure_summary / root_cause / prevention_rule \
                       must be at least 40 characters, and prevention_rule must contain at \
                       least one backtick-quoted command so the rule is actionable (e.g. \
                       \"Run `cargo fmt --check` before commit\"). category is a short \
                       label grouping related lessons (e.g. `ci-rustfmt`)."
    )]
    pub async fn capture_learning(
        &self,
        Parameters(req): Parameters<CaptureLearningRequest>,
    ) -> Result<Json<CaptureLearningResponse>, McpError> {
        let errs = validate(&req);
        if !errs.is_empty() {
            let summary = errs
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(McpError::invalid_params(
                format!("capture_learning validation failed: {summary}"),
                Some(serde_json::json!({
                    "code": "validation_failed",
                    "errors": errs,
                })),
            ));
        }

        // Look up the most-recent pending CI failure for (plan, task) so we
        // can backlink source_agent_id / source_ci_run_id on the learning
        // row. If none are pending — defensive against retries or proactive
        // capture — we still write the learning with NULL backlinks and a
        // default org_id; the call still succeeds because the only goal of
        // the gate-clear path is to make sure the next completed PUT passes.
        let authoring_ctx = {
            let conn = self.ctx.db.lock().unwrap();
            conn.query_row(
                "SELECT id, agent_id, org_id \
                   FROM ci_failure_events \
                  WHERE plan_name = ?1 \
                    AND task_number = ?2 \
                    AND resolved_at IS NULL \
                  ORDER BY observed_at DESC, id DESC \
                  LIMIT 1",
                params![req.plan, req.task],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok()
        };
        let (source_agent_id, source_ci_run_id, org_id) = match authoring_ctx {
            Some((id, agent_id, org_id)) => (Some(agent_id), Some(id), org_id),
            None => (None, None, "default-org".to_string()),
        };

        let learning_id = Uuid::new_v4().to_string();
        let slug = build_slug(&req.category);
        let body = build_body(&req);
        let kind = LearningKind::Feedback;
        let category = req.category.trim();

        crate::db::create_learning(
            &self.ctx.db,
            &LearningInput {
                id: &learning_id,
                org_id: &org_id,
                kind,
                category,
                slug: &slug,
                body_md: &body,
                source_agent_id: source_agent_id.as_deref(),
                source_ci_run_id,
            },
        )
        .map_err(|e| McpError::internal_error(format!("failed to write learning: {e}"), None))?;

        // Clear the pending-learning gate. This is the unblock side of the
        // tool — once these rows flip to resolved, the next
        // `update_task_status(completed)` for (plan, task) passes the gate.
        let resolved_failures =
            crate::db::mark_ci_failures_resolved(&self.ctx.db, &req.plan, &req.task);

        Ok(Json(CaptureLearningResponse {
            ok: true,
            plan_name: req.plan,
            task_number: req.task,
            learning_id,
            slug,
            kind: kind.as_str().to_string(),
            resolved_failures,
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the Phase 1, Task 1.3 `capture_learning` tool.
    //! Mirrors the McpContext construction pattern used by
    //! `mcp/tools/status.rs::tests`.
    use super::*;
    use crate::agents::AgentRegistry;
    use crate::config::Effort;
    use crate::db;
    use crate::mcp::{BranchworkMcp, McpContext};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    fn test_mcp() -> (BranchworkMcp, TempDir) {
        let dir = TempDir::new().unwrap();
        let plans_dir = dir.path().join("plans");
        std::fs::create_dir_all(&plans_dir).unwrap();
        let db = db::init(&dir.path().join("test.db"));
        let (tx, _rx) = broadcast::channel::<String>(32);
        let registry = AgentRegistry::new(
            db.clone(),
            tx.clone(),
            None,
            PathBuf::from("/tmp/branchwork-test-sockets"),
            PathBuf::from("/nonexistent/branchwork-server"),
            3100,
            true,
        );
        let ctx = McpContext {
            plans_dir,
            db,
            broadcast_tx: tx,
            registry,
            effort: Arc::new(Mutex::new(Effort::High)),
            port: 3100,
        };
        (BranchworkMcp::new(ctx), dir)
    }

    /// Seed a pending `ci_failure_events` row matching the shape T1.1 +
    /// T1.2 produce. Returns the rowid so callers can assert backlinks.
    fn seed_pending_failure(
        mcp: &BranchworkMcp,
        plan: &str,
        task: &str,
        run_id: &str,
        agent_id: &str,
    ) -> i64 {
        let conn = mcp.ctx.db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id, \
                  run_url, workflow, conclusion, summary, org_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'default-org')",
            params![
                agent_id,
                plan,
                task,
                format!("branchwork/{}/{}", plan, task),
                run_id,
                format!("https://github.com/owner/repo/actions/runs/{}", run_id),
                "tests.yml",
                "failure",
                format!("workflow tests.yml failed (run {})", run_id),
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn well_formed_request(plan: &str, task: &str) -> CaptureLearningRequest {
        CaptureLearningRequest {
            plan: plan.to_string(),
            task: task.to_string(),
            failure_summary: "cargo fmt --check failed on push because a multi-line \
                              format!() call had not been formatted locally before commit"
                .to_string(),
            root_cause: "rustfmt collapses multi-line format!() invocations onto one line \
                         when the result fits under 100 chars; this commit added one such \
                         call that exceeded the line budget mid-edit"
                .to_string(),
            prevention_rule: "Always run `cargo fmt --check` before `git commit` so \
                              formatter divergence surfaces locally instead of in CI"
                .to_string(),
            category: "ci-rustfmt".to_string(),
        }
    }

    // ── Pure-helper tests ──

    #[test]
    fn has_backtick_command_recognises_a_simple_pair() {
        assert!(has_backtick_command("Run `cargo fmt --check` first"));
    }

    #[test]
    fn has_backtick_command_rejects_no_backticks() {
        assert!(!has_backtick_command("Run cargo fmt --check first"));
    }

    #[test]
    fn has_backtick_command_rejects_empty_pair() {
        assert!(!has_backtick_command("Run `` first"));
        assert!(!has_backtick_command("Run `   ` first"));
    }

    #[test]
    fn has_backtick_command_rejects_unclosed_backtick() {
        assert!(!has_backtick_command("Run `cargo fmt --check first"));
    }

    #[test]
    fn build_slug_kebabs_category_and_appends_uuid_suffix() {
        let slug = build_slug("CI Rustfmt");
        assert!(
            slug.starts_with("ci-rustfmt-"),
            "expected ci-rustfmt prefix, got {slug}"
        );
        // 11 prefix chars + 8 UUID chars = 19 total
        assert_eq!(slug.len(), "ci-rustfmt-".len() + 8, "{slug}");
    }

    #[test]
    fn build_slug_collapses_separators_and_strips_edges() {
        let slug = build_slug("  ___ci///rustfmt___  ");
        assert!(slug.starts_with("ci-rustfmt-"), "{slug}");
    }

    #[test]
    fn build_slug_falls_back_to_learning_when_category_slugifies_to_empty() {
        let slug = build_slug("###");
        assert!(slug.starts_with("learning-"), "{slug}");
    }

    #[test]
    fn build_slug_repeated_calls_produce_distinct_slugs() {
        let a = build_slug("ci");
        let b = build_slug("ci");
        assert_ne!(a, b, "uuid suffix should make repeat calls unique");
    }

    #[test]
    fn validate_passes_a_well_formed_request() {
        let req = well_formed_request("p", "1.1");
        assert!(validate(&req).is_empty());
    }

    #[test]
    fn validate_flags_each_short_field_independently() {
        let mut req = well_formed_request("p", "1.1");
        req.failure_summary = "too short".to_string();
        req.root_cause = "also short".to_string();
        req.prevention_rule = "very short `cmd`".to_string();
        let errs = validate(&req);
        let fields: Vec<&str> = errs.iter().map(|e| e.field).collect();
        assert!(fields.contains(&"failure_summary"), "{fields:?}");
        assert!(fields.contains(&"root_cause"), "{fields:?}");
        assert!(fields.contains(&"prevention_rule"), "{fields:?}");
    }

    #[test]
    fn validate_flags_missing_backtick_in_prevention_rule() {
        let mut req = well_formed_request("p", "1.1");
        req.prevention_rule =
            "Always run cargo fmt --check before commit so divergence surfaces locally".to_string();
        let errs = validate(&req);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].field, "prevention_rule");
        assert!(errs[0].message.contains("backtick"));
    }

    #[test]
    fn validate_flags_empty_category() {
        let mut req = well_formed_request("p", "1.1");
        req.category = "   ".to_string();
        let errs = validate(&req);
        let categories: Vec<&str> = errs.iter().map(|e| e.field).collect();
        assert!(categories.contains(&"category"), "{categories:?}");
    }

    // ── End-to-end tool tests ──

    /// Headline acceptance test: a red CI -> capture_learning call with a
    /// well-formed payload unblocks the agent. Asserts the learning row
    /// exists, source_ci_run_id backlinks to the seeded pending failure,
    /// and the pending-learning gate is cleared.
    #[tokio::test]
    async fn capture_learning_well_formed_unblocks_the_agent() {
        let (mcp, _dir) = test_mcp();
        let pending_id = seed_pending_failure(&mcp, "p", "1.1", "12345", "agent-A");

        let res = mcp
            .capture_learning(Parameters(well_formed_request("p", "1.1")))
            .await
            .expect("well-formed payload should succeed");
        let body = res.0;
        assert!(body.ok);
        assert_eq!(body.plan_name, "p");
        assert_eq!(body.task_number, "1.1");
        assert_eq!(body.resolved_failures, 1);
        assert_eq!(body.kind, "feedback");
        assert!(body.slug.starts_with("ci-rustfmt-"), "{}", body.slug);

        // The learning row was written with the expected backlinks.
        let stored = crate::db::get_learning(&mcp.ctx.db, &body.learning_id)
            .expect("learning row must exist");
        assert_eq!(stored.kind, "feedback");
        assert_eq!(stored.category, "ci-rustfmt");
        assert_eq!(stored.source_agent_id.as_deref(), Some("agent-A"));
        assert_eq!(stored.source_ci_run_id, Some(pending_id));
        assert!(
            stored.body_md.contains("## Failure summary"),
            "{}",
            stored.body_md
        );
        assert!(
            stored.body_md.contains("## Root cause"),
            "{}",
            stored.body_md
        );
        assert!(
            stored.body_md.contains("## Prevention rule"),
            "{}",
            stored.body_md
        );

        // Gate is cleared: subsequent pending_ci_failures returns empty.
        let pending = crate::db::pending_ci_failures(&mcp.ctx.db, "p", "1.1");
        assert!(
            pending.is_empty(),
            "capture_learning must mark the pending failure resolved"
        );
    }

    /// The end-to-end unblock acceptance: after capture_learning,
    /// `update_task_status(completed)` for the same (plan, task) passes the
    /// pending-learning gate. Pinning both ends here keeps the round-trip
    /// contract self-checking inside one file.
    #[tokio::test]
    async fn capture_learning_then_update_completed_passes_the_gate() {
        use crate::mcp::tools::status::UpdateTaskStatusRequest;
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345", "agent-A");

        mcp.capture_learning(Parameters(well_formed_request("p", "1.1")))
            .await
            .expect("capture should succeed");

        mcp.update_task_status(Parameters(UpdateTaskStatusRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            status: "completed".to_string(),
            reason: None,
        }))
        .await
        .expect("post-capture completed must pass the pending-learning gate");
    }

    /// Validation errors come back as a structured `McpError` so the agent
    /// can react programmatically. `code` is `validation_failed`, `errors`
    /// lists per-field messages.
    #[tokio::test]
    async fn capture_learning_short_field_returns_structured_validation_error() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345", "agent-A");

        let mut req = well_formed_request("p", "1.1");
        req.failure_summary = "too short".to_string();

        let res = mcp.capture_learning(Parameters(req)).await;
        let err = match res {
            Ok(_) => panic!("expected validation_failed McpError, got Ok"),
            Err(e) => e,
        };
        assert!(
            err.message.contains("validation"),
            "message should mention validation: {}",
            err.message
        );
        let data = err.data.expect("data payload is required");
        assert_eq!(data["code"], "validation_failed", "{data}");
        let errors = data["errors"].as_array().expect("errors must be an array");
        let fields: Vec<&str> = errors
            .iter()
            .map(|e| e["field"].as_str().unwrap())
            .collect();
        assert!(fields.contains(&"failure_summary"), "{fields:?}");

        // Validation must NOT write a row or clear the gate.
        let pending = crate::db::pending_ci_failures(&mcp.ctx.db, "p", "1.1");
        assert_eq!(
            pending.len(),
            1,
            "rejected payload must leave the gate intact"
        );
    }

    #[tokio::test]
    async fn capture_learning_missing_backtick_command_returns_validation_error() {
        let (mcp, _dir) = test_mcp();
        seed_pending_failure(&mcp, "p", "1.1", "12345", "agent-A");

        let mut req = well_formed_request("p", "1.1");
        req.prevention_rule = "Always run cargo fmt --check before commit so a formatter \
                               divergence surfaces locally instead of red CI"
            .to_string();

        let res = mcp.capture_learning(Parameters(req)).await;
        let err = match res {
            Ok(_) => panic!("expected validation_failed McpError, got Ok"),
            Err(e) => e,
        };
        let data = err.data.expect("data payload is required");
        assert_eq!(data["code"], "validation_failed");
        let errors = data["errors"].as_array().expect("errors must be an array");
        assert!(
            errors.iter().any(|e| e["field"] == "prevention_rule"
                && e["message"].as_str().unwrap().contains("backtick")),
            "must call out the missing-backtick failure: {errors:?}"
        );

        // Pending failure is still pending — gate must not clear on reject.
        let pending = crate::db::pending_ci_failures(&mcp.ctx.db, "p", "1.1");
        assert_eq!(pending.len(), 1);
    }

    /// Combined-error path: many things wrong at once → agent gets a
    /// single response listing every field that needs a rewrite, so it can
    /// fix them in one retry instead of guessing one-at-a-time.
    #[tokio::test]
    async fn capture_learning_combines_multiple_field_errors_in_one_response() {
        let (mcp, _dir) = test_mcp();
        let req = CaptureLearningRequest {
            plan: "p".to_string(),
            task: "1.1".to_string(),
            failure_summary: "too short".to_string(),
            root_cause: "also short".to_string(),
            prevention_rule: "no backticks here either".to_string(),
            category: "".to_string(),
        };
        let err = mcp
            .capture_learning(Parameters(req))
            .await
            .err()
            .expect("expected validation error");
        let data = err.data.expect("data payload");
        let errors = data["errors"].as_array().unwrap();
        let fields: Vec<&str> = errors
            .iter()
            .map(|e| e["field"].as_str().unwrap())
            .collect();
        // Every short field plus category plus the backtick rule should
        // each surface, so an agent rewriting once can fix everything.
        for expected in [
            "failure_summary",
            "root_cause",
            "prevention_rule",
            "category",
        ] {
            assert!(fields.contains(&expected), "missing {expected}: {fields:?}");
        }
    }

    /// Proactive / retry path: no pending failure exists for (plan, task).
    /// The tool still writes the learning (with NULL backlinks) and
    /// returns ok with resolved_failures = 0. Defensive against double-
    /// captures.
    #[tokio::test]
    async fn capture_learning_without_pending_failure_still_writes_learning() {
        let (mcp, _dir) = test_mcp();
        // No seed → no pending row.
        let res = mcp
            .capture_learning(Parameters(well_formed_request("p", "1.1")))
            .await
            .expect("no-pending payload should succeed");
        let body = res.0;
        assert_eq!(body.resolved_failures, 0);
        let stored = crate::db::get_learning(&mcp.ctx.db, &body.learning_id)
            .expect("learning row must exist");
        assert_eq!(stored.source_agent_id, None);
        assert_eq!(stored.source_ci_run_id, None);
        assert_eq!(stored.org_id, "default-org");
    }
}
