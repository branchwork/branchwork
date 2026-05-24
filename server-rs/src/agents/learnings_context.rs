//! Surface relevant prior learnings to a fresh agent at spawn time.
//!
//! Branchwork captures `learnings` rows (kind=feedback/project/reference)
//! as agents discover gotchas, validated approaches, and CI-failure
//! post-mortems. Without this module those rows are passive memory —
//! readable on the dashboard, but never delivered to the next agent
//! that would benefit from them.
//!
//! Task 3.1 is the surfacing path: when a new agent is dispatched for a
//! task, fetch the top-K most-relevant active learnings for the plan's
//! org, render them into a `<learnings>...</learnings>` block, and
//! prepend the block to the agent's initial prompt.
//!
//! ## Filters (per the plan brief)
//!
//! 1. **Plan's project (org).** Learnings are org-scoped, so the
//!    plan-to-org mapping is the project filter — we look up the owning
//!    org via [`crate::auth::orgs::org_for_plan`] and pull only that
//!    org's non-archived learnings.
//! 2. **File-path textual overlap.** Each `task.file_paths` entry is
//!    tokenised into path segments and basename stems. A learning that
//!    mentions any of those tokens in its `category`, `slug`, or
//!    `body_md` scores points (with the highest weight on `category`,
//!    matching the headline acceptance criterion).
//! 3. **Same-workflow CI failure backlinks.** A learning written by
//!    `capture_learning` (Phase 1.3) backlinks to a `ci_failure_events`
//!    row via `source_ci_run_id`. If that row's `workflow` matches a
//!    workflow that has previously failed on the same plan, the
//!    learning gets a large boost — that's exactly the prior-incident-
//!    memory the brief asks us to surface.
//!
//! Scoring is deliberately additive: a learning that hits both (2) and
//! (3) outranks one that hits only one. Ties break by `created_at`
//! descending so the most recent of equally-relevant rows wins.
//!
//! ## Why the renderer is a separate function
//!
//! The integration test inspects "the prompt assembly output". Splitting
//! selection from rendering lets the selector be unit-tested with
//! synthetic [`Learning`] fixtures (no plan / no DB plumbing) while the
//! renderer is fully pure and produces a deterministic string.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::db::{Db, Learning};

/// Default cap on the number of learnings injected into the prompt.
/// Five is small enough that an agent reads them; the budget keeps the
/// per-spawn token cost predictable even on plans with hundreds of
/// historical learnings.
pub const DEFAULT_LEARNINGS_TOP_K: usize = 5;

/// Minimum length for a file-path token to participate in scoring.
/// Filters out ambiguous fragments like `rs`, `ts`, `db` that would
/// otherwise match every codebase learning.
const MIN_TOKEN_LEN: usize = 3;

/// Score weights, in points. Kept const so the test fixtures can pin
/// the exact relative ordering without hard-coding magic numbers.
const SCORE_CATEGORY_MATCH: i64 = 10;
const SCORE_SLUG_MATCH: i64 = 5;
const SCORE_BODY_MATCH: i64 = 2;
const SCORE_WORKFLOW_BACKLINK: i64 = 15;
const SCORE_FEEDBACK_KIND: i64 = 1;

/// Pull active learnings for the plan's org, score by relevance, and
/// return the top `top_k` by score then recency.
///
/// Returns an empty `Vec` when:
/// - The plan has no org (default-org with no learnings),
/// - No learnings overlap with any of the file-path tokens AND no
///   learning is backlinked to a workflow that has failed on this plan.
///
/// Callers should treat an empty Vec as "no block to render" — the
/// rendered string in that case would be a misleading "(none)" header.
pub fn select_relevant_learnings(
    db: &Db,
    plan_name: &str,
    task_file_paths: &[String],
    top_k: usize,
) -> Vec<Learning> {
    if top_k == 0 {
        return Vec::new();
    }

    let org_id = {
        let conn = db.lock().unwrap();
        crate::auth::orgs::org_for_plan(&conn, plan_name)
    };

    let plan_workflows = workflows_with_failures_for_plan(db, plan_name);
    let ci_backlinks = ci_backlinks_for_org(db, &org_id);
    let path_tokens = tokens_from_file_paths(task_file_paths);

    let candidates = crate::db::list_learnings_for_org(db, &org_id, None, false);

    let mut scored: Vec<(i64, Learning)> = candidates
        .into_iter()
        .filter_map(|l| {
            let score = score_learning_relevance(&l, &path_tokens, &plan_workflows, &ci_backlinks);
            if score > 0 { Some((score, l)) } else { None }
        })
        .collect();

    // Score desc, then created_at desc, then id desc for stable ordering.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.created_at.cmp(&a.1.created_at))
            .then_with(|| b.1.id.cmp(&a.1.id))
    });

    scored.into_iter().take(top_k).map(|(_, l)| l).collect()
}

/// Render a `<learnings>...</learnings>` block from a list of selected
/// learnings. Returns `None` for an empty list — the caller must NOT
/// prepend a stub block, because an empty `<learnings/>` would just be
/// noise the agent has to skim past.
pub fn render_learnings_block(learnings: &[Learning]) -> Option<String> {
    if learnings.is_empty() {
        return None;
    }

    let mut s = String::from("<learnings>\n");
    s.push_str(
        "Relevant prior learnings from previous Branchwork sessions on this project. \
         These are durable lessons the user (or earlier agents) captured; honour them \
         unless the current task explicitly overrides.\n\n",
    );
    for (i, l) in learnings.iter().enumerate() {
        s.push_str(&format!(
            "{n}. [{kind}] {category} ({created})\n",
            n = i + 1,
            kind = l.kind,
            category = l.category,
            created = l.created_at,
        ));
        s.push_str(l.body_md.trim());
        s.push_str("\n\n");
    }
    s.push_str("</learnings>");
    Some(s)
}

/// Pure scoring helper. Exposed for unit-tests; the production path
/// drives it from `select_relevant_learnings`.
///
/// `path_tokens` is the result of [`tokens_from_file_paths`].
/// `plan_failure_workflows` is the set of workflow names that have
/// previously failed on this plan (filter (c) input).
/// `ci_backlinks` maps `ci_failure_events.id -> workflow` for the org
/// so we can resolve a learning's `source_ci_run_id` into a workflow
/// name without re-querying per row.
///
/// Returns 0 when the learning has NO relevance signal (no path-token
/// match, no workflow backlink). The kind-tie-breaker is only added on
/// top of a real signal — otherwise every feedback memory in the org
/// would surface for every spawn.
pub fn score_learning_relevance(
    learning: &Learning,
    path_tokens: &[String],
    plan_failure_workflows: &HashSet<String>,
    ci_backlinks: &HashMap<i64, String>,
) -> i64 {
    let mut signal: i64 = 0;
    let cat_lower = learning.category.to_lowercase();
    let slug_lower = learning.slug.to_lowercase();
    let body_lower = learning.body_md.to_lowercase();

    for token in path_tokens {
        if token.len() < MIN_TOKEN_LEN {
            continue;
        }
        if cat_lower.contains(token) {
            signal += SCORE_CATEGORY_MATCH;
        }
        if slug_lower.contains(token) {
            signal += SCORE_SLUG_MATCH;
        }
        if body_lower.contains(token) {
            signal += SCORE_BODY_MATCH;
        }
    }

    // Workflow-backlink boost: this learning was written when CI failed
    // on a workflow that has ALSO failed on this plan. That's the exact
    // "prior incident memory" the brief surfaces with filter (c).
    if let Some(id) = learning.source_ci_run_id
        && let Some(workflow) = ci_backlinks.get(&id)
        && plan_failure_workflows.contains(workflow.as_str())
    {
        signal += SCORE_WORKFLOW_BACKLINK;
    }

    // No signal = no relevance, regardless of kind. Returning 0 keeps the
    // caller's `score > 0` filter the single gate on inclusion.
    if signal == 0 {
        return 0;
    }

    // Feedback is the kind callers most commonly want surfaced (per the
    // auto-memory contract: rules learned the hard way). Tiny tie-break
    // weight applied AFTER signal-gating so an unrelated feedback memory
    // doesn't leak in just for being a feedback row.
    let bonus = if learning.kind == "feedback" {
        SCORE_FEEDBACK_KIND
    } else {
        0
    };

    signal + bonus
}

/// Tokenise a list of file paths into lowercased path-segment + basename-
/// stem strings. Used as the `path_tokens` input to
/// [`score_learning_relevance`].
///
/// E.g. `["server-rs/src/agents/spawn_ops.rs"]` produces
/// `["spawn_ops", "spawn_ops.rs", "agents", "src", "server-rs", "server-rs/src/agents/spawn_ops.rs"]`
/// (order unspecified — caller treats it as a set).
pub fn tokens_from_file_paths(paths: &[String]) -> Vec<String> {
    let mut out: HashSet<String> = HashSet::new();
    for p in paths {
        let p_trim = p.trim();
        if p_trim.is_empty() {
            continue;
        }
        out.insert(p_trim.to_lowercase());
        for seg in p_trim.split('/') {
            if seg.is_empty() {
                continue;
            }
            out.insert(seg.to_lowercase());
            if let Some(idx) = seg.rfind('.') {
                let stem = &seg[..idx];
                if !stem.is_empty() {
                    out.insert(stem.to_lowercase());
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Distinct workflow names from `ci_failure_events` rows for this
/// plan. Each name is the GitHub workflow display name as captured by
/// `record_ci_failure_observed`.
fn workflows_with_failures_for_plan(db: &Db, plan_name: &str) -> HashSet<String> {
    let conn = db.lock().unwrap();
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT workflow FROM ci_failure_events \
          WHERE plan_name = ?1 AND workflow IS NOT NULL",
    ) else {
        return HashSet::new();
    };
    stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Map `ci_failure_events.id -> workflow` for the whole org. A learning's
/// `source_ci_run_id` is a `ci_failure_events.id` (per the Phase 1.3
/// `capture_learning` tool); the table holds the workflow name we need.
fn ci_backlinks_for_org(db: &Db, org_id: &str) -> HashMap<i64, String> {
    let conn = db.lock().unwrap();
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, workflow FROM ci_failure_events \
          WHERE org_id = ?1 AND workflow IS NOT NULL",
    ) else {
        return HashMap::new();
    };
    stmt.query_map(params![org_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })
    .map(|rows| rows.flatten().collect::<HashMap<_, _>>())
    .unwrap_or_default()
}

/// Resolve a task's `file_paths` by re-reading the plan file from disk.
///
/// Returns an empty Vec when the plan isn't on disk, the task number
/// doesn't match any task in the plan, or the YAML is malformed. The
/// caller treats "empty file_paths" as "no path-driven match" and
/// falls back to filter (c) alone.
pub fn task_file_paths_for_plan(
    plans_dir: &std::path::Path,
    plan_name: &str,
    task_id: &str,
) -> Vec<String> {
    let Some(plan_path) = crate::plan_parser::find_plan_file(plans_dir, plan_name) else {
        return Vec::new();
    };
    let Ok(plan) = crate::plan_parser::parse_plan_file(&plan_path) else {
        return Vec::new();
    };
    plan.phases
        .into_iter()
        .flat_map(|p| p.tasks.into_iter())
        .find(|t| t.number == task_id)
        .map(|t| t.file_paths)
        .unwrap_or_default()
}

/// Compose: pull file_paths off the plan + select relevant learnings +
/// render the block. Returns `None` when there's nothing to inject
/// (empty result or top_k=0). The caller (typically
/// `start_agent_dispatch`) prepends the result to the agent's prompt.
pub fn build_learnings_block(
    db: &Db,
    plans_dir: &std::path::Path,
    plan_name: Option<&str>,
    task_id: Option<&str>,
) -> Option<String> {
    let plan_name = plan_name?;
    let task_id = task_id?;
    let file_paths = task_file_paths_for_plan(plans_dir, plan_name, task_id);
    let learnings = select_relevant_learnings(db, plan_name, &file_paths, DEFAULT_LEARNINGS_TOP_K);
    render_learnings_block(&learnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn mk_learning(
        id: &str,
        kind: &str,
        category: &str,
        slug: &str,
        body: &str,
        created_at: &str,
        source_ci_run_id: Option<i64>,
    ) -> Learning {
        Learning {
            id: id.to_string(),
            org_id: "default-org".to_string(),
            kind: kind.to_string(),
            category: category.to_string(),
            slug: slug.to_string(),
            body_md: body.to_string(),
            source_agent_id: None,
            source_ci_run_id,
            created_at: created_at.to_string(),
            archived_at: None,
            archived_reason: None,
        }
    }

    // ── tokens_from_file_paths ──────────────────────────────────────────

    #[test]
    fn tokens_include_segments_and_basename_stem() {
        let paths = vec!["server-rs/src/agents/spawn_ops.rs".to_string()];
        let tokens: HashSet<String> = tokens_from_file_paths(&paths).into_iter().collect();
        assert!(tokens.contains("spawn_ops"));
        assert!(tokens.contains("spawn_ops.rs"));
        assert!(tokens.contains("agents"));
        assert!(tokens.contains("src"));
        assert!(tokens.contains("server-rs"));
        // Full path also present so a learning that names the literal
        // path string scores.
        assert!(tokens.contains("server-rs/src/agents/spawn_ops.rs"));
    }

    #[test]
    fn tokens_dedupe_across_paths() {
        let paths = vec![
            "server-rs/src/agents/spawn_ops.rs".to_string(),
            "server-rs/src/agents/learnings_context.rs".to_string(),
        ];
        let tokens = tokens_from_file_paths(&paths);
        let agents_count = tokens.iter().filter(|t| *t == "agents").count();
        assert_eq!(agents_count, 1, "duplicates must collapse");
    }

    #[test]
    fn tokens_skip_empty_segments() {
        let paths = vec!["//foo//".to_string()];
        let tokens: HashSet<String> = tokens_from_file_paths(&paths).into_iter().collect();
        assert!(tokens.contains("foo"));
        assert!(!tokens.contains(""));
    }

    #[test]
    fn tokens_ignore_pure_whitespace_paths() {
        let paths = vec!["   ".to_string(), "".to_string()];
        let tokens = tokens_from_file_paths(&paths);
        assert!(tokens.is_empty());
    }

    // ── score_learning_relevance ────────────────────────────────────────

    #[test]
    fn category_match_scores_higher_than_body_match() {
        let by_cat = mk_learning(
            "a",
            "feedback",
            "spawn_ops",
            "x",
            "irrelevant body",
            "2026-05-24T10:00:00",
            None,
        );
        let by_body = mk_learning(
            "b",
            "feedback",
            "unrelated",
            "y",
            "spawn_ops appears here in the body",
            "2026-05-24T10:00:00",
            None,
        );
        let tokens = vec!["spawn_ops".to_string()];
        let workflows = HashSet::new();
        let backlinks = HashMap::new();
        let s_cat = score_learning_relevance(&by_cat, &tokens, &workflows, &backlinks);
        let s_body = score_learning_relevance(&by_body, &tokens, &workflows, &backlinks);
        assert!(
            s_cat > s_body,
            "category match should outrank body match: {s_cat} vs {s_body}"
        );
    }

    #[test]
    fn tiny_tokens_are_ignored() {
        // "rs" is below MIN_TOKEN_LEN and must NOT trigger a match even
        // though it appears in the category of many Rust-related rows.
        let l = mk_learning(
            "a",
            "feedback",
            "rs",
            "rs-stuff",
            "rs everywhere",
            "2026-05-24T10:00:00",
            None,
        );
        let tokens = vec!["rs".to_string()];
        let workflows = HashSet::new();
        let backlinks = HashMap::new();
        // No path match (token too short) AND no workflow backlink ⇒
        // no signal. The feedback-kind bonus does NOT apply without a
        // real signal; the row must NOT surface.
        let score = score_learning_relevance(&l, &tokens, &workflows, &backlinks);
        assert_eq!(score, 0);
    }

    #[test]
    fn workflow_backlink_boosts_score() {
        let l_no_ci = mk_learning(
            "a",
            "feedback",
            "unrelated",
            "x",
            "no relevance",
            "2026-05-24T10:00:00",
            None,
        );
        let l_ci = mk_learning(
            "b",
            "feedback",
            "unrelated",
            "y",
            "no path match either",
            "2026-05-24T10:00:00",
            Some(42),
        );
        let tokens: Vec<String> = vec![];
        let mut workflows = HashSet::new();
        workflows.insert("CI".to_string());
        let mut backlinks = HashMap::new();
        backlinks.insert(42i64, "CI".to_string());

        let s_no = score_learning_relevance(&l_no_ci, &tokens, &workflows, &backlinks);
        let s_yes = score_learning_relevance(&l_ci, &tokens, &workflows, &backlinks);
        // No signal at all ⇒ 0 (feedback kind alone is not a signal).
        assert_eq!(s_no, 0);
        // Workflow backlink ⇒ signal SCORE_WORKFLOW_BACKLINK, plus the
        // feedback tie-break bonus.
        assert_eq!(s_yes, SCORE_WORKFLOW_BACKLINK + SCORE_FEEDBACK_KIND);
    }

    #[test]
    fn workflow_backlink_does_not_fire_when_workflow_mismatches() {
        let l = mk_learning(
            "b",
            "feedback",
            "unrelated",
            "y",
            "no path match",
            "2026-05-24T10:00:00",
            Some(42),
        );
        let tokens: Vec<String> = vec![];
        let mut workflows = HashSet::new();
        workflows.insert("Docker".to_string()); // plan has Docker failures
        let mut backlinks = HashMap::new();
        backlinks.insert(42i64, "CI".to_string()); // learning came from CI failure
        let score = score_learning_relevance(&l, &tokens, &workflows, &backlinks);
        assert_eq!(score, 0, "different workflow = no boost = no signal at all");
    }

    #[test]
    fn feedback_kind_outranks_project_when_otherwise_tied() {
        let f = mk_learning(
            "a",
            "feedback",
            "spawn_ops",
            "x",
            "body",
            "2026-05-24T10:00:00",
            None,
        );
        let p = mk_learning(
            "b",
            "project",
            "spawn_ops",
            "y",
            "body",
            "2026-05-24T10:00:00",
            None,
        );
        let tokens = vec!["spawn_ops".to_string()];
        let workflows = HashSet::new();
        let backlinks = HashMap::new();
        let s_f = score_learning_relevance(&f, &tokens, &workflows, &backlinks);
        let s_p = score_learning_relevance(&p, &tokens, &workflows, &backlinks);
        assert!(s_f > s_p, "feedback bonus should break the tie");
    }

    // ── render_learnings_block ──────────────────────────────────────────

    #[test]
    fn render_returns_none_for_empty_list() {
        let out = render_learnings_block(&[]);
        assert!(out.is_none(), "empty input must produce no block");
    }

    #[test]
    fn render_wraps_in_xml_tags_with_header() {
        let l = mk_learning(
            "a",
            "feedback",
            "spawn_ops",
            "spawn-ops-gotcha",
            "Don't forget to refresh runners after dispatch.",
            "2026-05-24T10:00:00",
            None,
        );
        let out = render_learnings_block(&[l]).unwrap();
        assert!(out.starts_with("<learnings>\n"));
        assert!(out.ends_with("</learnings>"));
        assert!(out.contains("[feedback]"));
        assert!(out.contains("spawn_ops"));
        assert!(out.contains("Don't forget to refresh runners"));
    }

    #[test]
    fn render_preserves_order_in_output() {
        let l1 = mk_learning(
            "a",
            "feedback",
            "first",
            "s1",
            "body-first",
            "2026-05-24T10:00:00",
            None,
        );
        let l2 = mk_learning(
            "b",
            "feedback",
            "second",
            "s2",
            "body-second",
            "2026-05-24T09:00:00",
            None,
        );
        let out = render_learnings_block(&[l1, l2]).unwrap();
        let i_first = out.find("body-first").unwrap();
        let i_second = out.find("body-second").unwrap();
        assert!(
            i_first < i_second,
            "input order must be preserved in render output"
        );
    }

    // ── end-to-end selector (with real DB) ──────────────────────────────

    fn full_db() -> (Db, tempfile::TempDir) {
        let td = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&td.path().join("test.db"));
        (db, td)
    }

    fn seed_learning(
        db: &Db,
        id: &str,
        kind: &str,
        category: &str,
        slug: &str,
        body: &str,
        source_ci_run_id: Option<i64>,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO learnings (id, org_id, kind, category, slug, body_md, source_ci_run_id) \
             VALUES (?1, 'default-org', ?2, ?3, ?4, ?5, ?6)",
            params![id, kind, category, slug, body, source_ci_run_id],
        )
        .unwrap();
    }

    fn seed_ci_failure(db: &Db, plan: &str, workflow: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                (agent_id, plan_name, branch, run_id, workflow, conclusion, org_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'failure', 'default-org')",
            params!["agent-1", plan, "branchwork/p/0.1", "run-1", workflow,],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn selector_returns_empty_when_no_learnings() {
        let (db, _td) = full_db();
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            5,
        );
        assert!(out.is_empty());
    }

    /// Headline acceptance: the most-recent feedback learning whose
    /// category overlaps with the task's file_paths gets selected.
    #[test]
    fn selector_picks_feedback_whose_category_matches_file_paths() {
        let (db, _td) = full_db();
        // Two learnings: one category-matches, one doesn't.
        seed_learning(
            &db,
            "match",
            "feedback",
            "spawn_ops",
            "spawn-ops-gotcha",
            "Refresh runners after dispatch.",
            None,
        );
        seed_learning(
            &db,
            "miss",
            "feedback",
            "completely-unrelated",
            "u-slug",
            "body",
            None,
        );
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            5,
        );
        assert!(
            out.iter().any(|l| l.id == "match"),
            "category-matching learning must be selected"
        );
        assert!(
            !out.iter().any(|l| l.id == "miss"),
            "unrelated learning must not be selected"
        );
    }

    #[test]
    fn selector_orders_by_score_then_recency() {
        let (db, _td) = full_db();
        // Highest score: category match.
        seed_learning(&db, "high", "feedback", "spawn_ops", "high", "body", None);
        // Lower score: body-only match.
        seed_learning(
            &db,
            "low",
            "feedback",
            "unrelated",
            "low",
            "spawn_ops appears in body only",
            None,
        );
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            5,
        );
        assert_eq!(out[0].id, "high", "category match must outrank body match");
    }

    #[test]
    fn selector_top_k_caps_result_count() {
        let (db, _td) = full_db();
        for i in 0..8 {
            seed_learning(
                &db,
                &format!("l{i}"),
                "feedback",
                "spawn_ops",
                &format!("s{i}"),
                "body",
                None,
            );
        }
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            3,
        );
        assert_eq!(out.len(), 3, "top_k must cap result length");
    }

    #[test]
    fn selector_top_k_zero_returns_empty() {
        let (db, _td) = full_db();
        seed_learning(&db, "l", "feedback", "spawn_ops", "s", "body", None);
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            0,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn selector_uses_workflow_backlink_when_no_file_path_match() {
        let (db, _td) = full_db();
        let failure_id = seed_ci_failure(&db, "p", "Tests");
        // Learning has no category/slug/body overlap with file_paths,
        // but links to a CI failure for workflow=Tests on plan p.
        seed_learning(
            &db,
            "via-workflow",
            "feedback",
            "totally-unrelated-category",
            "s",
            "body has no overlap",
            Some(failure_id),
        );
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            5,
        );
        assert!(
            out.iter().any(|l| l.id == "via-workflow"),
            "workflow-backlink alone must surface the learning: {out:?}"
        );
    }

    #[test]
    fn selector_skips_archived_learnings() {
        let (db, _td) = full_db();
        seed_learning(
            &db,
            "active",
            "feedback",
            "spawn_ops",
            "active",
            "body",
            None,
        );
        // Archive one row.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO learnings (id, org_id, kind, category, slug, body_md, archived_at, archived_reason) \
                 VALUES ('archived', 'default-org', 'feedback', 'spawn_ops', 'archived', 'body', datetime('now'), 'outdated')",
                [],
            )
            .unwrap();
        }
        let out = select_relevant_learnings(
            &db,
            "p",
            &["server-rs/src/agents/spawn_ops.rs".to_string()],
            5,
        );
        assert!(out.iter().any(|l| l.id == "active"));
        assert!(
            !out.iter().any(|l| l.id == "archived"),
            "archived learnings must not surface"
        );
    }

    // ── task_file_paths_for_plan ────────────────────────────────────────

    #[test]
    fn task_file_paths_for_plan_returns_empty_when_plan_missing() {
        let td = tempfile::TempDir::new().unwrap();
        let out = task_file_paths_for_plan(td.path(), "no-such-plan", "1.1");
        assert!(out.is_empty());
    }

    #[test]
    fn task_file_paths_for_plan_reads_yaml() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("my-plan.yaml");
        let yaml = r#"title: My Plan
context: ctx
phases:
  - number: 1
    title: Phase one
    tasks:
      - number: "1.1"
        title: Do thing
        description: ""
        file_paths:
          - server-rs/src/agents/spawn_ops.rs
          - web/src/components/Foo.tsx
        acceptance: ""
"#;
        std::fs::write(&path, yaml).unwrap();
        let out = task_file_paths_for_plan(td.path(), "my-plan", "1.1");
        assert_eq!(
            out,
            vec![
                "server-rs/src/agents/spawn_ops.rs".to_string(),
                "web/src/components/Foo.tsx".to_string(),
            ]
        );
    }

    // ── build_learnings_block ──────────────────────────────────────────

    #[test]
    fn build_learnings_block_returns_none_when_no_plan_or_task() {
        let (db, _td) = full_db();
        let plans_dir: PathBuf = "/no/such/path".into();
        assert!(build_learnings_block(&db, &plans_dir, None, Some("1.1")).is_none());
        assert!(build_learnings_block(&db, &plans_dir, Some("p"), None).is_none());
    }

    #[test]
    fn build_learnings_block_returns_none_when_no_matches() {
        let (db, _td) = full_db();
        let td = tempfile::TempDir::new().unwrap();
        let yaml = r#"title: P
context: c
phases:
  - number: 1
    title: P1
    tasks:
      - number: "1.1"
        title: T
        description: ""
        file_paths:
          - server-rs/src/agents/spawn_ops.rs
        acceptance: ""
"#;
        std::fs::write(td.path().join("p.yaml"), yaml).unwrap();
        let out = build_learnings_block(&db, td.path(), Some("p"), Some("1.1"));
        assert!(out.is_none(), "no learnings = no block");
    }

    #[test]
    fn build_learnings_block_produces_xml_when_matches_exist() {
        let (db, _td) = full_db();
        seed_learning(
            &db,
            "match",
            "feedback",
            "spawn_ops",
            "spawn-ops",
            "Refresh runners after dispatch.",
            None,
        );
        let td = tempfile::TempDir::new().unwrap();
        let yaml = r#"title: P
context: c
phases:
  - number: 1
    title: P1
    tasks:
      - number: "1.1"
        title: T
        description: ""
        file_paths:
          - server-rs/src/agents/spawn_ops.rs
        acceptance: ""
"#;
        std::fs::write(td.path().join("p.yaml"), yaml).unwrap();
        let out = build_learnings_block(&db, td.path(), Some("p"), Some("1.1")).unwrap();
        assert!(out.starts_with("<learnings>"));
        assert!(out.contains("Refresh runners after dispatch."));
    }
}
