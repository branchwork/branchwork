//! One-shot YAML migrations applied at server startup.
//!
//! The Branchwork DB schema migrations live in [`crate::db::migrate`]; this
//! module is the parallel home for one-shot rewrites of the on-disk plan
//! YAML files. Each migration follows the same shape as the
//! `ci_backfill_v1_done` gate in [`crate::ci::backfill_aggregates`]: a
//! `pub fn spawn_*(state)` launcher that detaches the work via
//! `tokio::spawn`, an idempotency gate keyed in the `settings` table, and a
//! single audit-grade log line per rewritten file so an operator can read
//! the boot output and see what changed.
//!
//! Migrations here are intentionally *narrow*: they exist to repair a
//! specific incident in the wild (workflow rename, schema reshuffle, etc.)
//! and become permanent no-ops once every install has booted past them.
//! Bump the gate suffix (`_v1_done`, `_v2_done`, …) only when a downstream
//! change requires re-running on already-migrated databases.

use rusqlite::params;

use crate::api::plans::update_yaml_top_level_key;
use crate::db::Db;
use crate::plan_parser;
use crate::state::AppState;

// ── Phase 3.1: rename `Pipeline` → `task-tests` ────────────────────────────
//
// Phase 1 of the CI split (`tests.yml`/`pipeline.yml`) and Phase 2
// (`task-tests.yml`) moved the test jobs onto a dedicated workflow that
// fires on branchwork/** pushes. Plans configured with `Pipeline` as
// their blocking workflow now block on a workflow that no longer runs
// for task branches. This migration walks every plan YAML in
// `<plans_dir>` and flips any string match of `Pipeline` inside the
// top-level `ci_blocking_workflows` sequence to `task-tests`. Other
// entries (e.g. an operator-added `Docker`) survive unchanged.
//
// The acceptance criterion in the task brief is phrased in SQL terms
// (`UPDATE plan_blocking_workflows SET workflow_name = 'task-tests'
// WHERE workflow_name = 'Pipeline'`) but Branchwork stores
// `ci_blocking_workflows` in YAML, not in a SQL table; the equivalent
// rewrite is per-file. Idempotency is intrinsic — re-reading a plan
// that already says `task-tests` does not match the predicate — but
// the gate is set anyway so subsequent boots skip the directory walk.

const PIPELINE_RENAME_GATE_KEY: &str = "pipeline_to_task_tests_rename_v1_done";
const OLD_WORKFLOW_NAME: &str = "Pipeline";
const NEW_WORKFLOW_NAME: &str = "task-tests";

/// Spawn the Phase 3.1 plan-YAML rewrite. Detached via `tokio::spawn`
/// so the HTTP listener readiness probe is not blocked by the file
/// walk; gated by [`PIPELINE_RENAME_GATE_KEY`] in the `settings` table
/// so subsequent boots return immediately.
pub fn spawn_pipeline_to_task_tests_rename(state: AppState) {
    tokio::spawn(async move {
        rename_pipeline_to_task_tests(state).await;
    });
}

/// Walk `<plans_dir>` for YAML plans, flipping every
/// `ci_blocking_workflows: [..., Pipeline, ...]` entry to
/// `task-tests`. Logs one line per rewritten file and a summary line
/// at the end. Sets [`PIPELINE_RENAME_GATE_KEY`] unconditionally so
/// re-runs are skipped — re-running after a partial failure (e.g. a
/// read-only filesystem) requires flipping the flag manually.
pub async fn rename_pipeline_to_task_tests(state: AppState) {
    if gate_is_set(&state.db, PIPELINE_RENAME_GATE_KEY) {
        eprintln!("[migrations] pipeline→task-tests rename already applied, skipping");
        return;
    }

    let plans_dir = state.plans_dir.clone();
    let summary = match rewrite_plans_in_dir(&plans_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[migrations] pipeline→task-tests rename: failed to walk plans dir: {e}");
            return;
        }
    };

    eprintln!(
        "[migrations] pipeline→task-tests rename: scanned {} plans, rewrote {} files",
        summary.scanned, summary.rewrote,
    );

    set_gate(&state.db, PIPELINE_RENAME_GATE_KEY);
}

/// Result of a single rewrite pass — useful for tests that want to
/// assert on counts without re-scanning the directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RewriteSummary {
    pub scanned: usize,
    pub rewrote: usize,
}

fn rewrite_plans_in_dir(plans_dir: &std::path::Path) -> std::io::Result<RewriteSummary> {
    let mut summary = RewriteSummary::default();
    let entries = std::fs::read_dir(plans_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        // Plan archives live under `<plans_dir>/archive/<name>.<utc>.yaml`;
        // never rewrite an archived snapshot, only live plans at the top
        // level of `<plans_dir>`.
        if !path.is_file() {
            continue;
        }
        if !plan_parser::is_plan_ext(&path) {
            continue;
        }
        // Markdown plans cannot express `ci_blocking_workflows` (the
        // parser silently drops it). Skip them entirely so the predicate
        // is unambiguous.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "yaml" | "yml") {
            continue;
        }
        summary.scanned += 1;

        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[migrations] pipeline→task-tests rename: failed to read {}: {e}",
                    path.display()
                );
                continue;
            }
        };

        let Some(new_list) = rewritten_blocking_workflows(&raw) else {
            continue;
        };

        let new_value = serde_yaml::Value::Sequence(
            new_list
                .iter()
                .map(|s| serde_yaml::Value::String(s.clone()))
                .collect(),
        );
        let updated =
            match update_yaml_top_level_key(&raw, "ci_blocking_workflows", Some(&new_value)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "[migrations] pipeline→task-tests rename: failed to edit {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };

        // Validate the post-edit YAML parses before writing so a bug in
        // the line-based editor surfaces as a skipped file with a loud
        // log line, not a corrupted plan on disk. Mirrors the
        // `put_plan_settings` write-side guard.
        if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(&updated) {
            eprintln!(
                "[migrations] pipeline→task-tests rename: post-edit YAML invalid for {}: {e}",
                path.display()
            );
            continue;
        }

        if let Err(e) = std::fs::write(&path, &updated) {
            eprintln!(
                "[migrations] pipeline→task-tests rename: failed to write {}: {e}",
                path.display()
            );
            continue;
        }

        summary.rewrote += 1;
        eprintln!(
            "[migrations] pipeline→task-tests rename: rewrote {}",
            path.display()
        );
    }
    Ok(summary)
}

/// Returns `Some(new_list)` when the YAML carries a
/// `ci_blocking_workflows` sequence that contains the literal string
/// `Pipeline`, with every match swapped to `task-tests` and every
/// other entry preserved verbatim. Returns `None` when the key is
/// absent, malformed, or already has no `Pipeline` entries — the
/// caller treats `None` as "skip, file already correct".
pub(crate) fn rewritten_blocking_workflows(yaml: &str) -> Option<Vec<String>> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let map = value.as_mapping()?;
    let list = map.get(serde_yaml::Value::String("ci_blocking_workflows".into()))?;
    let seq = list.as_sequence()?;

    let mut found = false;
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        let s = item.as_str()?.to_string();
        if s == OLD_WORKFLOW_NAME {
            out.push(NEW_WORKFLOW_NAME.to_string());
            found = true;
        } else {
            out.push(s);
        }
    }
    if !found { None } else { Some(out) }
}

fn gate_is_set(db: &Db, key: &str) -> bool {
    let conn = db.lock().unwrap();
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .ok();
    matches!(v.as_deref(), Some("1"))
}

fn set_gate(db: &Db, key: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, '1') \
         ON CONFLICT(key) DO UPDATE SET value = '1'",
        params![key],
    )
    .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_plan(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
    }

    fn read_plan(dir: &std::path::Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::read_to_string(&path).unwrap()
    }

    // ── rewritten_blocking_workflows ─────────────────────────────────

    #[test]
    fn pipeline_alone_is_rewritten_to_task_tests() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - Pipeline\nphases: []\n";
        let out = rewritten_blocking_workflows(yaml).unwrap();
        assert_eq!(out, vec!["task-tests".to_string()]);
    }

    #[test]
    fn pipeline_in_mixed_sequence_only_swaps_pipeline_entry() {
        let yaml = "title: Hi\n\
                    ci_blocking_workflows:\n  \
                    - Pipeline\n  \
                    - Docker\n  \
                    - Pipeline\nphases: []\n";
        let out = rewritten_blocking_workflows(yaml).unwrap();
        assert_eq!(
            out,
            vec![
                "task-tests".to_string(),
                "Docker".to_string(),
                "task-tests".to_string(),
            ]
        );
    }

    #[test]
    fn flow_style_list_with_pipeline_is_rewritten() {
        let yaml = "title: Hi\nci_blocking_workflows: [Pipeline, Docker]\nphases: []\n";
        let out = rewritten_blocking_workflows(yaml).unwrap();
        assert_eq!(out, vec!["task-tests".to_string(), "Docker".to_string()]);
    }

    #[test]
    fn no_pipeline_entry_returns_none() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - Docker\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn already_migrated_list_returns_none() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - task-tests\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn case_sensitive_match_does_not_touch_pipeline_lowercase() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - pipeline\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn substring_match_does_not_touch_pipelined_or_pipeline_v2() {
        let yaml = "title: Hi\n\
                    ci_blocking_workflows:\n  \
                    - Pipelined\n  \
                    - Pipeline v2\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn missing_ci_blocking_workflows_returns_none() {
        let yaml = "title: Hi\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn non_sequence_ci_blocking_workflows_returns_none() {
        let yaml = "title: Hi\nci_blocking_workflows: Pipeline\nphases: []\n";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    #[test]
    fn malformed_yaml_returns_none() {
        let yaml = "title: Hi\nci_blocking_workflows:\n  - [unterminated";
        assert!(rewritten_blocking_workflows(yaml).is_none());
    }

    // ── rewrite_plans_in_dir ─────────────────────────────────────────

    #[test]
    fn rewrites_n_pipeline_plans_in_directory_and_leaves_zero_pipeline_rows() {
        // Acceptance test verbatim from the brief: "On a DB with N rows of
        // `plan_blocking_workflows.workflow_name = 'Pipeline'`, the migration
        // leaves zero such rows and N rows of `task-tests`." The N here is 3.
        let dir = TempDir::new().unwrap();
        for i in 0..3 {
            write_plan(
                dir.path(),
                &format!("plan-{i}.yaml"),
                "title: T\nci_blocking_workflows:\n  - Pipeline\nphases: []\n",
            );
        }
        // Plus one bystander plan that should NOT change.
        write_plan(
            dir.path(),
            "bystander.yaml",
            "title: B\nci_blocking_workflows:\n  - Docker\nphases: []\n",
        );

        let summary = rewrite_plans_in_dir(dir.path()).unwrap();
        assert_eq!(summary.scanned, 4);
        assert_eq!(summary.rewrote, 3);

        for i in 0..3 {
            let raw = read_plan(dir.path(), &format!("plan-{i}.yaml"));
            assert!(raw.contains("task-tests"), "plan-{i} not rewritten: {raw}");
            assert!(
                !raw.contains("Pipeline"),
                "plan-{i} still has Pipeline: {raw}"
            );
        }
        let bystander = read_plan(dir.path(), "bystander.yaml");
        assert!(bystander.contains("Docker"));
        assert!(!bystander.contains("task-tests"));
    }

    #[test]
    fn rerun_after_rewrite_makes_zero_changes() {
        let dir = TempDir::new().unwrap();
        write_plan(
            dir.path(),
            "plan-a.yaml",
            "title: A\nci_blocking_workflows:\n  - Pipeline\nphases: []\n",
        );

        let first = rewrite_plans_in_dir(dir.path()).unwrap();
        assert_eq!(first.rewrote, 1);

        let after_first = read_plan(dir.path(), "plan-a.yaml");
        let second = rewrite_plans_in_dir(dir.path()).unwrap();
        assert_eq!(
            second,
            RewriteSummary {
                scanned: 1,
                rewrote: 0
            },
            "second pass must be a no-op (idempotency)"
        );
        let after_second = read_plan(dir.path(), "plan-a.yaml");
        assert_eq!(
            after_first, after_second,
            "second pass must not touch the file on disk"
        );
    }

    #[test]
    fn preserves_top_level_comments_outside_the_rewritten_key() {
        let yaml = "# This is a plan that does important things\n\
                    title: Important\n\
                    # context comment\n\
                    context: |\n  multi\n  line\n\
                    ci_blocking_workflows:\n  - Pipeline\n\
                    # phases comment\n\
                    phases: []\n";
        let dir = TempDir::new().unwrap();
        write_plan(dir.path(), "plan.yaml", yaml);

        let summary = rewrite_plans_in_dir(dir.path()).unwrap();
        assert_eq!(summary.rewrote, 1);

        let after = read_plan(dir.path(), "plan.yaml");
        assert!(after.contains("# This is a plan that does important things"));
        assert!(after.contains("# context comment"));
        assert!(after.contains("# phases comment"));
        assert!(after.contains("task-tests"));
        assert!(!after.contains("Pipeline"));
    }

    #[test]
    fn skips_markdown_plans_even_when_they_mention_pipeline() {
        let dir = TempDir::new().unwrap();
        // .md plan with literal "Pipeline" in the body — must NOT be rewritten,
        // ci_blocking_workflows is a YAML-only field and the migration must
        // not silently mutate markdown bodies.
        write_plan(
            dir.path(),
            "story.md",
            "# Story\n\nThe Pipeline workflow used to fire here.\n",
        );
        write_plan(
            dir.path(),
            "real.yaml",
            "title: R\nci_blocking_workflows:\n  - Pipeline\nphases: []\n",
        );

        let summary = rewrite_plans_in_dir(dir.path()).unwrap();
        assert_eq!(summary.scanned, 1, "markdown plans are skipped pre-scan");
        assert_eq!(summary.rewrote, 1);

        let md = read_plan(dir.path(), "story.md");
        assert!(md.contains("Pipeline"), "markdown body must be untouched");
    }

    #[test]
    fn missing_plans_dir_is_surface_error_not_panic() {
        let dir = TempDir::new().unwrap();
        let bogus = dir.path().join("does-not-exist");
        let err = rewrite_plans_in_dir(&bogus).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ── gate semantics ───────────────────────────────────────────────

    #[test]
    fn gate_round_trip_via_settings_table() {
        let db = crate::db::init(std::path::Path::new(":memory:"));
        assert!(!gate_is_set(&db, PIPELINE_RENAME_GATE_KEY));
        set_gate(&db, PIPELINE_RENAME_GATE_KEY);
        assert!(gate_is_set(&db, PIPELINE_RENAME_GATE_KEY));
        // Re-set is idempotent — UPSERT path keeps value at '1'.
        set_gate(&db, PIPELINE_RENAME_GATE_KEY);
        assert!(gate_is_set(&db, PIPELINE_RENAME_GATE_KEY));
    }
}
