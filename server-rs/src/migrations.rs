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

// ── Task 1.2 (cadence plan): grandfather merge_cadence='task' ───────────────
//
// `plan_auto_mode.merge_cadence` lands NULLable (NULL = inherit project
// default, which itself defaults to `phase`). The new default for fresh
// plans is therefore `phase`, but every plan that already existed when
// this column shipped used the legacy per-task-merge behaviour and would
// silently flip cadence on the first auto-mode tick after upgrade. To
// preserve behaviour, this one-shot migration writes `task` on every
// plan known at upgrade time:
//
//   - For every YAML/MD plan file at `<plans_dir>/*` we UPSERT a
//     `plan_auto_mode` row with `merge_cadence='task'` (without touching
//     `enabled` / `max_fix_attempts` / sibling columns).
//   - Existing `plan_auto_mode` rows whose `merge_cadence IS NULL` are
//     bulk-UPDATEd to `task` even if the plan file no longer exists,
//     so a plan that was deleted from disk but still carries audit
//     state stays consistent.
//
// Once [`MERGE_CADENCE_GRANDFATHER_GATE_KEY`] is set, subsequent boots
// skip the directory walk. New plans created post-migration go through
// the normal API flow (no implicit write of `task`) and inherit the
// project default. Bump the suffix only if a downstream change forces a
// re-run on already-migrated databases.

const MERGE_CADENCE_GRANDFATHER_GATE_KEY: &str = "plan_merge_cadence_grandfather_v1_done";
const LEGACY_MERGE_CADENCE: &str = "task";

/// Spawn the one-shot grandfather pass. Detached via `tokio::spawn` so
/// the HTTP listener readiness probe is not blocked by the directory
/// walk; gated by [`MERGE_CADENCE_GRANDFATHER_GATE_KEY`] in the
/// `settings` table so subsequent boots return immediately.
pub fn spawn_grandfather_merge_cadence(state: AppState) {
    tokio::spawn(async move {
        grandfather_merge_cadence(state).await;
    });
}

/// Result of a single grandfather pass — useful for tests that want to
/// assert on counts without re-scanning the directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GrandfatherSummary {
    /// Plan-file basenames discovered in `<plans_dir>/*`.
    pub scanned: usize,
    /// New `plan_auto_mode` rows created (or rows whose
    /// `merge_cadence` was just freshly set on UPSERT).
    pub written: usize,
}

pub async fn grandfather_merge_cadence(state: AppState) {
    if gate_is_set(&state.db, MERGE_CADENCE_GRANDFATHER_GATE_KEY) {
        eprintln!("[migrations] merge_cadence grandfather already applied, skipping");
        return;
    }

    let plans_dir = state.plans_dir.clone();
    let summary = match grandfather_merge_cadence_inner(&state.db, &plans_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[migrations] merge_cadence grandfather: failed to walk plans dir: {e}");
            return;
        }
    };

    eprintln!(
        "[migrations] merge_cadence grandfather: scanned {} plans, wrote 'task' to {} \
         row(s) (existing plans pinned to legacy behaviour; new plans inherit project default)",
        summary.scanned, summary.written,
    );

    set_gate(&state.db, MERGE_CADENCE_GRANDFATHER_GATE_KEY);
}

/// Pure helper used by both production [`grandfather_merge_cadence`] and
/// the unit tests. Touches only `plan_auto_mode.merge_cadence` — never
/// `enabled` / `max_fix_attempts` / sibling columns, so a plan that was
/// already opted into auto-mode survives untouched modulo the cadence
/// write.
pub(crate) fn grandfather_merge_cadence_inner(
    db: &Db,
    plans_dir: &std::path::Path,
) -> std::io::Result<GrandfatherSummary> {
    let mut summary = GrandfatherSummary::default();
    let entries = std::fs::read_dir(plans_dir)?;
    let mut plan_names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Archives live under `<plans_dir>/archive/<name>.<utc>.yaml`;
        // never touch a snapshot, only live top-level plan files.
        if !path.is_file() {
            continue;
        }
        if !plan_parser::is_plan_ext(&path) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        plan_names.push(stem.to_string());
    }
    summary.scanned = plan_names.len();

    let conn = db.lock().unwrap();

    // Pass 1: UPSERT one row per plan file with merge_cadence='task'.
    // We use ON CONFLICT DO UPDATE so any existing row gets its cadence
    // pinned to 'task' WITHOUT touching `enabled`, `max_fix_attempts`,
    // etc. — the partial-update pattern matches `set_plan_merge_cadence`.
    // Tracking the inserts/updates as one bucket here is intentional: a
    // plan that was already opted into auto-mode (row exists, cadence
    // NULL) needs the same treatment as a plan with no row at all.
    for name in &plan_names {
        let n = conn
            .execute(
                "INSERT INTO plan_auto_mode (plan_name, merge_cadence) \
                 VALUES (?1, ?2) \
                 ON CONFLICT(plan_name) DO UPDATE SET \
                    merge_cadence = excluded.merge_cadence \
                  WHERE plan_auto_mode.merge_cadence IS NULL",
                params![name, LEGACY_MERGE_CADENCE],
            )
            .unwrap_or(0);
        if n > 0 {
            summary.written += 1;
        }
    }

    // Pass 2: catch orphan rows — plans whose YAML file was deleted but
    // whose `plan_auto_mode` row still carries audit state (paused_reason,
    // fix attempts, etc.). Pin those too so the legacy contract holds
    // uniformly: "every plan that existed when this column shipped is
    // pinned to 'task'".
    let n = conn
        .execute(
            "UPDATE plan_auto_mode \
                SET merge_cadence = ?1 \
              WHERE merge_cadence IS NULL",
            params![LEGACY_MERGE_CADENCE],
        )
        .unwrap_or(0);
    summary.written += n;

    Ok(summary)
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

    // ── grandfather_merge_cadence (Task 1.2 of cadence plan) ───────

    fn count_cadence_rows(db: &crate::db::Db, cadence: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM plan_auto_mode WHERE merge_cadence = ?1",
            params![cadence],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_null_cadence_rows(db: &crate::db::Db) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM plan_auto_mode WHERE merge_cadence IS NULL",
            params![],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn grandfather_writes_task_on_every_existing_plan_file() {
        // Headline acceptance criterion: a 100-plan DB after migration
        // has every existing row at `merge_cadence='task'`.
        let dir = TempDir::new().unwrap();
        let plans_dir = dir.path();
        for i in 0..100 {
            write_plan(
                plans_dir,
                &format!("plan-{i:03}.yaml"),
                "title: T\nphases: []\n",
            );
        }
        let db = crate::db::init(std::path::Path::new(":memory:"));
        let summary = grandfather_merge_cadence_inner(&db, plans_dir).unwrap();
        assert_eq!(summary.scanned, 100);
        assert_eq!(summary.written, 100);
        assert_eq!(count_cadence_rows(&db, "task"), 100);
        assert_eq!(count_null_cadence_rows(&db), 0);
    }

    #[test]
    fn grandfather_preserves_existing_auto_mode_state() {
        // The cadence write must NOT clobber enabled / max_fix_attempts /
        // parallel / paused_reason — those belong to user-driven toggles.
        let dir = TempDir::new().unwrap();
        write_plan(dir.path(), "plan-a.yaml", "title: T\nphases: []\n");
        let db = crate::db::init(std::path::Path::new(":memory:"));
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode \
                   (plan_name, enabled, max_fix_attempts, parallel, paused_reason) \
                 VALUES (?1, 1, 7, 1, 'merge_conflict')",
                params!["plan-a"],
            )
            .unwrap();
        }

        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.scanned, 1);
        // One write: the UPDATE that set cadence on the existing row.
        assert_eq!(summary.written, 1);

        let cfg = crate::db::auto_mode_config(&db, "plan-a");
        assert!(cfg.enabled, "enabled must survive grandfather");
        assert_eq!(cfg.max_fix_attempts, 7);
        assert!(cfg.parallel);
        assert_eq!(cfg.paused_reason.as_deref(), Some("merge_conflict"));
        assert_eq!(
            cfg.merge_cadence,
            Some(crate::repo_config::MergeCadence::Task)
        );
    }

    #[test]
    fn grandfather_skips_rows_with_explicit_cadence_already_set() {
        // A user who set 'plan' explicitly between upgrades must not be
        // bulk-rewritten to 'task'. The WHERE filter on the DO UPDATE
        // gates that case.
        let dir = TempDir::new().unwrap();
        write_plan(dir.path(), "plan-pinned.yaml", "title: T\nphases: []\n");
        let db = crate::db::init(std::path::Path::new(":memory:"));
        crate::db::set_plan_merge_cadence(
            &db,
            "plan-pinned",
            Some(crate::repo_config::MergeCadence::Plan),
        );

        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.scanned, 1);
        assert_eq!(
            summary.written, 0,
            "explicit cadence must NOT be overwritten by grandfather"
        );
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-pinned"),
            Some(crate::repo_config::MergeCadence::Plan),
            "user-set cadence must be preserved verbatim"
        );
    }

    #[test]
    fn grandfather_catches_orphan_rows_without_a_yaml_on_disk() {
        // A plan that was deleted from disk but still carries
        // `plan_auto_mode` state (paused_reason etc.) must also be
        // grandfathered to 'task' so the legacy contract holds uniformly.
        let dir = TempDir::new().unwrap();
        // No YAML on disk for plan-orphan.
        let db = crate::db::init(std::path::Path::new(":memory:"));
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["plan-orphan"],
            )
            .unwrap();
        }
        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.scanned, 0);
        // Pass 2 catches the orphan.
        assert_eq!(summary.written, 1);
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-orphan"),
            Some(crate::repo_config::MergeCadence::Task)
        );
    }

    #[test]
    fn grandfather_is_idempotent_on_repeated_runs() {
        // Re-running the inner helper after the first pass writes nothing
        // (every row already has merge_cadence set). The settings-table
        // gate is checked by the public wrapper; the inner pass is
        // intrinsically idempotent.
        let dir = TempDir::new().unwrap();
        for i in 0..3 {
            write_plan(
                dir.path(),
                &format!("plan-{i}.yaml"),
                "title: T\nphases: []\n",
            );
        }
        let db = crate::db::init(std::path::Path::new(":memory:"));

        let first = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(first.written, 3);

        let second = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(
            second.written, 0,
            "second pass must write nothing — every row already has cadence set"
        );
        assert_eq!(second.scanned, 3, "directory walk still happens");
    }

    #[test]
    fn fresh_plans_after_migration_still_inherit_default() {
        // Acceptance criterion: a freshly-created plan inherits `phase`
        // from the project. The grandfather migration writes 'task' to
        // every plan known AT THE TIME OF UPGRADE. Plans created after
        // the gate is set never see the migration and must default to
        // NULL (= inherit project default).
        let dir = TempDir::new().unwrap();
        write_plan(dir.path(), "plan-original.yaml", "title: T\nphases: []\n");
        let db = crate::db::init(std::path::Path::new(":memory:"));

        // First boot: grandfather every plan on disk.
        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.written, 1);

        // Simulate a freshly-created plan via the normal API flow: the
        // user toggles auto-mode on, which UPSERTs a plan_auto_mode row
        // with `enabled=1` and merge_cadence column omitted from the
        // INSERT — column NULL by default.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["plan-fresh"],
            )
            .unwrap();
        }
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-fresh"),
            None,
            "fresh plan must NOT inherit the legacy 'task' grandfather — it inherits the project default"
        );

        // The original grandfathered plan stays at 'task'.
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-original"),
            Some(crate::repo_config::MergeCadence::Task)
        );
    }

    #[test]
    fn grandfather_gate_blocks_repeated_runs() {
        // Public wrapper: settings-table gate makes the second run a no-op
        // even if the underlying state could still be mutated.
        // Driven via the inner helper + explicit gate manipulation since
        // the public wrapper just wraps the gate read/inner/gate set
        // sequence — the gate semantics are the only new contract here.
        let dir = TempDir::new().unwrap();
        write_plan(dir.path(), "plan-a.yaml", "title: T\nphases: []\n");

        let db = crate::db::init(std::path::Path::new(":memory:"));
        assert!(!gate_is_set(&db, MERGE_CADENCE_GRANDFATHER_GATE_KEY));

        // First boot equivalent: inner pass + gate set (matches the
        // `grandfather_merge_cadence` wrapper body).
        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.written, 1);
        set_gate(&db, MERGE_CADENCE_GRANDFATHER_GATE_KEY);
        assert!(gate_is_set(&db, MERGE_CADENCE_GRANDFATHER_GATE_KEY));
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-a"),
            Some(crate::repo_config::MergeCadence::Task)
        );

        // Roll the value on disk to 'plan' → re-invoking the wrapper
        // would early-return on the gate and the value must stay 'plan'.
        crate::db::set_plan_merge_cadence(
            &db,
            "plan-a",
            Some(crate::repo_config::MergeCadence::Plan),
        );
        assert!(
            gate_is_set(&db, MERGE_CADENCE_GRANDFATHER_GATE_KEY),
            "gate is sticky across writes — wrapper short-circuits next call"
        );
        // Sanity: the inner helper is intrinsically idempotent — even
        // bypassing the gate, the WHERE filter on the DO UPDATE preserves
        // the explicit choice.
        let summary = grandfather_merge_cadence_inner(&db, dir.path()).unwrap();
        assert_eq!(summary.written, 0, "explicit cadence is preserved");
        assert_eq!(
            crate::db::plan_merge_cadence(&db, "plan-a"),
            Some(crate::repo_config::MergeCadence::Plan)
        );
    }
}
