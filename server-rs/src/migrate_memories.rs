//! One-shot CLI: import per-user auto-memory `.md` files into the
//! org-shared `learnings` table.
//!
//! See `branchwork-server migrate-memories --help` for the user-facing
//! contract. The on-disk files stay where they are — they remain the
//! *user-private* slot. Only files the operator explicitly migrates land
//! in the org store.
//!
//! ## Format expected on disk
//!
//! Files live under `<claude-dir>/projects/<user>/memory/*.md`. Each is
//! a Markdown file with a YAML frontmatter block, e.g.
//!
//! ```text
//! ---
//! name: branchwork-as-learning-hub
//! description: ...
//! metadata:
//!   node_type: memory
//!   type: project
//! ---
//!
//! Body content here.
//! ```
//!
//! The `type` field may live at the top level (`type: feedback`) OR
//! nested under `metadata.type` — both shapes are seen in the demo
//! corpus and the parser tries both.
//!
//! ## Derivation rules
//!
//! - `kind`: parsed from the frontmatter `type` field. Only `feedback`,
//!   `project`, `reference` are accepted; `user` and unknown values are
//!   skipped (logged as `Skipped::UnsupportedKind`).
//! - `category`: the first underscore-delimited segment of the filename
//!   stem. `feedback_check_runner_after_deploy.md` → `feedback`. This
//!   intentionally mirrors `kind` in well-named files but allows divergence
//!   if a filename and its frontmatter disagree.
//! - `slug`: the full filename stem with underscores converted to dashes,
//!   lower-cased. `feedback_check_runner_after_deploy.md` →
//!   `feedback-check-runner-after-deploy`. Stable across re-runs, so the
//!   `(org_id, kind, slug)` unique constraint naturally provides
//!   idempotency.
//! - `body_md`: the entire file content (frontmatter + body) verbatim, so
//!   `name`, `description`, and `originSessionId` survive migration as
//!   audit metadata.
//!
//! Files named `MEMORY.md` are skipped — they are the index, not memories.
//!
//! ## Idempotency
//!
//! Re-running with the same args is safe. The unique index on
//! `learnings(org_id, kind, slug)` short-circuits repeat inserts; we
//! report those as `Skipped::AlreadyMigrated` rather than failing the
//! whole run.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use uuid::Uuid;

use crate::db::{Db, LearningError, LearningInput, LearningKind, create_learning};

/// Parsed contents of a single memory file, ready for insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMemory {
    pub filename: String,
    pub kind: LearningKind,
    pub category: String,
    pub slug: String,
    pub body_md: String,
}

/// Why a particular file did not produce a learning row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `MEMORY.md` (the index file) — never imported.
    IndexFile,
    /// Frontmatter present but no `type:` field that matches a
    /// `LearningKind` variant. `user` lands here too.
    UnsupportedKind(String),
    /// No frontmatter, or YAML parse failed.
    MissingFrontmatter,
    /// `(org_id, kind, slug)` already exists in the DB — re-run is safe.
    AlreadyMigrated,
}

/// One row in the run report. Either we wrote the learning or skipped
/// it with a reason; either way the operator gets a per-file line so
/// they can diff a dry-run vs an apply.
#[derive(Debug, Clone)]
pub enum Outcome {
    Inserted {
        filename: String,
        kind: LearningKind,
        category: String,
        slug: String,
    },
    WouldInsert {
        filename: String,
        kind: LearningKind,
        category: String,
        slug: String,
    },
    Skipped {
        filename: String,
        reason: SkipReason,
    },
    Failed {
        filename: String,
        error: String,
    },
}

/// Aggregate counts for the run, printed at the end so an operator can
/// confirm a re-run is fully idempotent (`inserted = 0`, `skipped =
/// AlreadyMigrated × N`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationSummary {
    pub inserted: usize,
    pub would_insert: usize,
    pub skipped: usize,
    pub failed: usize,
    pub total_files: usize,
}

/// Result of a full run. The CLI prints `outcomes` line-by-line then the
/// summary; tests assert on the structured values without re-parsing the
/// terminal output.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub outcomes: Vec<Outcome>,
    pub summary: MigrationSummary,
    pub memory_dir: PathBuf,
    pub apply: bool,
}

/// Resolve `<claude-dir>/projects/<user>/memory`.
pub fn memory_dir_for(claude_dir: &Path, user: &str) -> PathBuf {
    claude_dir.join("projects").join(user).join("memory")
}

/// Split a memory file into (frontmatter_yaml, body). Returns `None` if
/// the file does not open with a `---` line, which we treat as a
/// missing-frontmatter skip rather than a hard error (no LearningKind
/// to derive).
pub fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    // Frontmatter must start at byte 0 with `---` followed by a newline.
    // Anything looser (whitespace, BOM) is treated as missing.
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // Find the closing `---` on its own line. We look for `\n---\n` or
    // `\n---\r\n` or `\n---` at EOF.
    let end_marker_idx = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))
        .or_else(|| {
            if rest.ends_with("\n---") {
                Some(rest.len() - 4)
            } else {
                None
            }
        })?;
    let yaml = &rest[..end_marker_idx];
    // Body starts after the closing marker. Skip the `\n---` plus its
    // trailing newline (if any).
    let after = &rest[end_marker_idx + 4..]; // +4 = "\n---"
    let body = after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix('\n'))
        .unwrap_or(after);
    Some((yaml, body))
}

/// Look for a `type:` field in the frontmatter YAML, either top-level
/// or nested under `metadata.type`. Both shapes appear in the demo
/// corpus; we accept either.
pub fn extract_kind_from_yaml(yaml: &str) -> Option<String> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let map = value.as_mapping()?;
    if let Some(v) = map.get(serde_yaml::Value::String("type".to_string()))
        && let Some(s) = v.as_str()
    {
        return Some(s.to_string());
    }
    if let Some(meta) = map.get(serde_yaml::Value::String("metadata".to_string()))
        && let Some(meta_map) = meta.as_mapping()
        && let Some(v) = meta_map.get(serde_yaml::Value::String("type".to_string()))
        && let Some(s) = v.as_str()
    {
        return Some(s.to_string());
    }
    None
}

/// `feedback_check_runner_after_deploy.md` → `feedback`.
/// `project_branchwork_as_learning_hub.md` → `project`.
/// `no_underscore.md` → `no` (still the prefix before the first
/// underscore; if there is none, the whole stem).
/// A file like `MEMORY.md` is filtered before this, so we do not have
/// to special-case it here.
pub fn derive_category(filename: &str) -> String {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    match stem.find('_') {
        Some(idx) => stem[..idx].to_string(),
        None => stem.to_string(),
    }
}

/// `feedback_check_runner_after_deploy.md` →
/// `feedback-check-runner-after-deploy`. Lowercased and with `_` → `-`
/// so the resulting string is a clean kebab-case slug; the full stem is
/// preserved so re-runs key on the exact filename (idempotency).
pub fn derive_slug(filename: &str) -> String {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    let mut out = String::with_capacity(stem.len());
    let mut prev_dash = false;
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Parse one memory file's bytes into a `ParsedMemory`. Returns
/// `Err(SkipReason)` for files we want to skip cleanly (index file,
/// unknown kind, missing frontmatter); the caller logs the skip.
pub fn parse_memory_file(filename: &str, content: &str) -> Result<ParsedMemory, SkipReason> {
    if filename == "MEMORY.md" {
        return Err(SkipReason::IndexFile);
    }
    let (yaml, _body) = split_frontmatter(content).ok_or(SkipReason::MissingFrontmatter)?;
    let raw_kind = extract_kind_from_yaml(yaml).ok_or(SkipReason::MissingFrontmatter)?;
    let kind = LearningKind::parse(&raw_kind).ok_or(SkipReason::UnsupportedKind(raw_kind))?;
    let category = derive_category(filename);
    let slug = derive_slug(filename);
    Ok(ParsedMemory {
        filename: filename.to_string(),
        kind,
        category,
        slug,
        body_md: content.to_string(),
    })
}

/// List `*.md` files in `dir`, sorted alphabetically so the output is
/// deterministic across runs.
fn list_memory_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Try to look up the org_id for a provided `org` argument. Operators
/// can pass either the canonical org id (e.g. `default-org`) or the
/// human slug (e.g. `default`); we accept both so a typo on the CLI
/// surfaces as a hard error rather than a silent default-org write.
fn resolve_org_id(conn: &Connection, org: &str) -> Result<String, String> {
    // First try as the literal id.
    let by_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE id = ?1",
            rusqlite::params![org],
            |row| row.get(0),
        )
        .ok();
    if let Some(id) = by_id {
        return Ok(id);
    }
    // Then by slug.
    let by_slug: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            rusqlite::params![org],
            |row| row.get(0),
        )
        .ok();
    if let Some(id) = by_slug {
        return Ok(id);
    }
    Err(format!(
        "org not found: {org} (try `default-org` or the slug from /api/orgs)"
    ))
}

/// Main entry point. Walks the memory dir, parses each file, and either
/// reports what *would* be written (dry-run) or actually inserts.
///
/// Returns `Err` only for catastrophic failure (memory dir missing, org
/// not found). Per-file failures land in the `outcomes` list so the
/// caller can render a full report.
pub fn run(
    claude_dir: &Path,
    user: &str,
    org: &str,
    apply: bool,
    db: &Db,
) -> Result<MigrationReport, String> {
    let memory_dir = memory_dir_for(claude_dir, user);
    if !memory_dir.is_dir() {
        return Err(format!(
            "memory directory not found: {} (expected `<claude-dir>/projects/<user>/memory/`)",
            memory_dir.display()
        ));
    }

    let org_id = {
        let conn = db.lock().unwrap();
        resolve_org_id(&conn, org)?
    };

    let files = list_memory_files(&memory_dir)
        .map_err(|e| format!("failed to read {}: {e}", memory_dir.display()))?;

    let mut outcomes = Vec::with_capacity(files.len());
    let mut summary = MigrationSummary {
        total_files: files.len(),
        ..Default::default()
    };

    for path in files {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let content = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                outcomes.push(Outcome::Failed {
                    filename: filename.clone(),
                    error: format!("read failed: {e}"),
                });
                summary.failed += 1;
                continue;
            }
        };
        let parsed = match parse_memory_file(&filename, &content) {
            Ok(p) => p,
            Err(reason) => {
                summary.skipped += 1;
                outcomes.push(Outcome::Skipped { filename, reason });
                continue;
            }
        };

        if !apply {
            summary.would_insert += 1;
            outcomes.push(Outcome::WouldInsert {
                filename,
                kind: parsed.kind,
                category: parsed.category,
                slug: parsed.slug,
            });
            continue;
        }

        let id = Uuid::new_v4().to_string();
        let input = LearningInput {
            id: &id,
            org_id: &org_id,
            kind: parsed.kind,
            category: &parsed.category,
            slug: &parsed.slug,
            body_md: &parsed.body_md,
            source_agent_id: None,
            source_ci_run_id: None,
        };
        match create_learning(db, &input) {
            Ok(()) => {
                summary.inserted += 1;
                outcomes.push(Outcome::Inserted {
                    filename,
                    kind: parsed.kind,
                    category: parsed.category,
                    slug: parsed.slug,
                });
            }
            Err(LearningError::SlugCollision) => {
                summary.skipped += 1;
                outcomes.push(Outcome::Skipped {
                    filename,
                    reason: SkipReason::AlreadyMigrated,
                });
            }
            Err(e) => {
                summary.failed += 1;
                outcomes.push(Outcome::Failed {
                    filename,
                    error: format!("{e}"),
                });
            }
        }
    }

    Ok(MigrationReport {
        outcomes,
        summary,
        memory_dir,
        apply,
    })
}

/// Render a `MigrationReport` as human-readable lines on stdout. Kept
/// separate from `run` so tests can introspect the report structurally.
pub fn print_report(report: &MigrationReport) {
    let mode = if report.apply { "APPLY" } else { "DRY-RUN" };
    println!(
        "[migrate-memories] {} scanning {}",
        mode,
        report.memory_dir.display()
    );
    // Group outcomes by category for readability.
    let mut by_category: BTreeMap<String, Vec<&Outcome>> = BTreeMap::new();
    for o in &report.outcomes {
        let cat = match o {
            Outcome::Inserted { category, .. } | Outcome::WouldInsert { category, .. } => {
                category.clone()
            }
            Outcome::Skipped { .. } | Outcome::Failed { .. } => "skipped".to_string(),
        };
        by_category.entry(cat).or_default().push(o);
    }
    for (cat, group) in &by_category {
        println!("  [{cat}]");
        for o in group {
            match o {
                Outcome::Inserted {
                    filename,
                    kind,
                    slug,
                    ..
                } => println!("    + {filename} → kind={} slug={slug}", kind.as_str()),
                Outcome::WouldInsert {
                    filename,
                    kind,
                    slug,
                    ..
                } => println!("    ~ {filename} → kind={} slug={slug}", kind.as_str()),
                Outcome::Skipped { filename, reason } => {
                    let why = match reason {
                        SkipReason::IndexFile => "index file".to_string(),
                        SkipReason::UnsupportedKind(k) => format!("unsupported kind `{k}`"),
                        SkipReason::MissingFrontmatter => {
                            "missing or malformed frontmatter".to_string()
                        }
                        SkipReason::AlreadyMigrated => "already migrated".to_string(),
                    };
                    println!("    - {filename}: {why}");
                }
                Outcome::Failed { filename, error } => {
                    println!("    ! {filename}: {error}");
                }
            }
        }
    }
    println!();
    println!(
        "[migrate-memories] {} files: inserted={} would_insert={} skipped={} failed={}",
        report.summary.total_files,
        report.summary.inserted,
        report.summary.would_insert,
        report.summary.skipped,
        report.summary.failed,
    );
    if !report.apply && report.summary.would_insert > 0 {
        println!("[migrate-memories] dry-run only — pass --apply to insert rows.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pure helpers ────────────────────────────────────────────────────

    #[test]
    fn split_frontmatter_handles_lf() {
        let content = "---\nname: x\ntype: feedback\n---\nbody here\n";
        let (yaml, body) = split_frontmatter(content).unwrap();
        assert_eq!(yaml, "name: x\ntype: feedback");
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn split_frontmatter_handles_crlf() {
        // CRLF case: the slice keeps the line-ending CR before the
        // closing `\n---\r\n` marker. That's fine — `\r` is whitespace
        // in YAML so serde_yaml still parses it. The contract we pin is
        // (a) the body strips both CRLF and LF prefixes, and (b) the
        // YAML still parses to the expected `type:` field.
        let content = "---\r\nname: x\r\ntype: feedback\r\n---\r\nbody\r\n";
        let (yaml, body) = split_frontmatter(content).unwrap();
        assert_eq!(body, "body\r\n");
        assert_eq!(extract_kind_from_yaml(yaml).as_deref(), Some("feedback"));
    }

    #[test]
    fn split_frontmatter_returns_none_when_no_opening_marker() {
        assert!(split_frontmatter("name: x\ntype: feedback\n").is_none());
    }

    #[test]
    fn split_frontmatter_returns_none_when_no_closing_marker() {
        assert!(split_frontmatter("---\nname: x\nbody\n").is_none());
    }

    #[test]
    fn extract_kind_reads_top_level_type() {
        let yaml = "name: x\ntype: feedback\norigin: y\n";
        assert_eq!(extract_kind_from_yaml(yaml).as_deref(), Some("feedback"));
    }

    #[test]
    fn extract_kind_reads_nested_metadata_type() {
        let yaml = "name: x\nmetadata:\n  node_type: memory\n  type: project\n";
        assert_eq!(extract_kind_from_yaml(yaml).as_deref(), Some("project"));
    }

    #[test]
    fn extract_kind_returns_none_when_absent() {
        let yaml = "name: x\ndescription: nope\n";
        assert_eq!(extract_kind_from_yaml(yaml), None);
    }

    #[test]
    fn extract_kind_prefers_top_level_over_nested() {
        // Documented precedence; pin so a future refactor cannot silently
        // flip the order.
        let yaml = "type: feedback\nmetadata:\n  type: project\n";
        assert_eq!(extract_kind_from_yaml(yaml).as_deref(), Some("feedback"));
    }

    #[test]
    fn derive_category_takes_first_underscore_segment() {
        assert_eq!(
            derive_category("feedback_check_runner_after_deploy.md"),
            "feedback"
        );
        assert_eq!(
            derive_category("project_branchwork_as_learning_hub.md"),
            "project"
        );
    }

    #[test]
    fn derive_category_falls_back_to_full_stem_when_no_underscore() {
        assert_eq!(derive_category("standalone.md"), "standalone");
    }

    #[test]
    fn derive_slug_lowercases_and_kebabs_full_stem() {
        assert_eq!(
            derive_slug("feedback_check_runner_after_deploy.md"),
            "feedback-check-runner-after-deploy"
        );
        assert_eq!(
            derive_slug("Project_Branchwork_As_Learning_Hub.md"),
            "project-branchwork-as-learning-hub"
        );
    }

    #[test]
    fn derive_slug_collapses_runs_of_non_alphanumerics() {
        assert_eq!(derive_slug("weird--name__here.md"), "weird-name-here");
    }

    // ── parse_memory_file ───────────────────────────────────────────────

    #[test]
    fn parse_skips_memory_index() {
        let err = parse_memory_file("MEMORY.md", "any content").unwrap_err();
        assert_eq!(err, SkipReason::IndexFile);
    }

    #[test]
    fn parse_skips_when_no_frontmatter() {
        let err = parse_memory_file("feedback_x.md", "no frontmatter here\n").unwrap_err();
        assert_eq!(err, SkipReason::MissingFrontmatter);
    }

    #[test]
    fn parse_skips_user_kind_as_unsupported() {
        let content = "---\nname: x\ntype: user\n---\nbody\n";
        let err = parse_memory_file("user_role.md", content).unwrap_err();
        assert_eq!(err, SkipReason::UnsupportedKind("user".to_string()));
    }

    #[test]
    fn parse_accepts_feedback_with_top_level_type() {
        let content = "---\nname: x\ntype: feedback\n---\nthe body\n";
        let parsed = parse_memory_file("feedback_check_runner_after_deploy.md", content).unwrap();
        assert_eq!(parsed.kind, LearningKind::Feedback);
        assert_eq!(parsed.category, "feedback");
        assert_eq!(parsed.slug, "feedback-check-runner-after-deploy");
        // body_md must preserve frontmatter for audit-trail purposes.
        assert_eq!(parsed.body_md, content);
    }

    #[test]
    fn parse_accepts_project_with_nested_metadata_type() {
        let content = "---\n\
name: x\n\
metadata:\n  node_type: memory\n  type: project\n\
---\nthe body\n";
        let parsed = parse_memory_file("project_branchwork_as_learning_hub.md", content).unwrap();
        assert_eq!(parsed.kind, LearningKind::Project);
        assert_eq!(parsed.category, "project");
        assert_eq!(parsed.slug, "project-branchwork-as-learning-hub");
    }

    #[test]
    fn parse_accepts_reference_kind() {
        let content = "---\nname: x\ntype: reference\n---\nbody\n";
        let parsed = parse_memory_file("reference_grafana_dashboards.md", content).unwrap();
        assert_eq!(parsed.kind, LearningKind::Reference);
        assert_eq!(parsed.category, "reference");
    }

    // ── End-to-end via run() ────────────────────────────────────────────

    fn setup_demo_corpus(claude_dir: &Path, user: &str) -> PathBuf {
        let dir = memory_dir_for(claude_dir, user);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("MEMORY.md"),
            "- [example](feedback_example.md) — index entry\n",
        )
        .unwrap();
        fs::write(
            dir.join("feedback_check_runner.md"),
            "---\nname: Check runner\ntype: feedback\n---\n**Why:** ...\n",
        )
        .unwrap();
        fs::write(
            dir.join("project_north_star.md"),
            "---\nname: north-star\nmetadata:\n  type: project\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            dir.join("reference_dashboards.md"),
            "---\nname: dash\ntype: reference\n---\ngrafana.internal/...\n",
        )
        .unwrap();
        fs::write(
            dir.join("user_role.md"),
            "---\nname: role\ntype: user\n---\nuser content\n",
        )
        .unwrap();
        fs::write(
            dir.join("broken_no_frontmatter.md"),
            "no frontmatter at all\n",
        )
        .unwrap();
        dir
    }

    fn fresh_db(tmp: &Path) -> Db {
        let path = tmp.join("branchwork.db");
        crate::db::init(&path)
    }

    #[test]
    fn run_dry_run_reports_every_file_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        setup_demo_corpus(&claude_dir, "-home-user-project");
        let db = fresh_db(tmp.path());

        let report = run(&claude_dir, "-home-user-project", "default-org", false, &db).unwrap();
        // 6 files total: 3 importable + 3 skipped (MEMORY.md, user, broken).
        assert_eq!(report.summary.total_files, 6);
        assert_eq!(report.summary.would_insert, 3);
        assert_eq!(report.summary.skipped, 3);
        assert_eq!(report.summary.inserted, 0);
        assert_eq!(report.summary.failed, 0);

        // Confirm no rows landed.
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM learnings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "dry-run must not insert rows");
    }

    #[test]
    fn run_apply_inserts_rows_with_correct_kind_and_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        setup_demo_corpus(&claude_dir, "-home-user-project");
        let db = fresh_db(tmp.path());

        let report = run(&claude_dir, "-home-user-project", "default-org", true, &db).unwrap();
        assert_eq!(report.summary.inserted, 3);
        assert_eq!(report.summary.would_insert, 0);
        assert_eq!(report.summary.skipped, 3);
        assert_eq!(report.summary.failed, 0);

        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT kind, category, slug FROM learnings WHERE org_id = ?1 ORDER BY slug")
            .unwrap();
        let rows: Vec<(String, String, String)> = stmt
            .query_map(rusqlite::params!["default-org"], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "feedback".to_string(),
                    "feedback".to_string(),
                    "feedback-check-runner".to_string(),
                ),
                (
                    "project".to_string(),
                    "project".to_string(),
                    "project-north-star".to_string(),
                ),
                (
                    "reference".to_string(),
                    "reference".to_string(),
                    "reference-dashboards".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn run_apply_is_idempotent_on_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        setup_demo_corpus(&claude_dir, "-home-user-project");
        let db = fresh_db(tmp.path());

        // First apply: 3 inserts, 3 skipped.
        let first = run(&claude_dir, "-home-user-project", "default-org", true, &db).unwrap();
        assert_eq!(first.summary.inserted, 3);

        // Second apply: 0 inserts, 6 skipped (3 were already-migrated, 3
        // are the same kind-unsupported / index / broken set).
        let second = run(&claude_dir, "-home-user-project", "default-org", true, &db).unwrap();
        assert_eq!(second.summary.inserted, 0);
        assert_eq!(second.summary.skipped, 6);
        assert_eq!(second.summary.failed, 0);

        let already_migrated_count = second
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    Outcome::Skipped {
                        reason: SkipReason::AlreadyMigrated,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            already_migrated_count, 3,
            "rerun must surface the 3 prior rows as AlreadyMigrated"
        );

        // Row count unchanged.
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM learnings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn run_errors_when_memory_dir_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        let db = fresh_db(tmp.path());
        let err = run(&claude_dir, "ghost-user", "default-org", true, &db).unwrap_err();
        assert!(err.contains("memory directory not found"), "got: {err}");
    }

    #[test]
    fn run_errors_when_org_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        setup_demo_corpus(&claude_dir, "-home-user-project");
        let db = fresh_db(tmp.path());
        let err = run(&claude_dir, "-home-user-project", "bogus-org", true, &db).unwrap_err();
        assert!(err.contains("org not found"), "got: {err}");
    }

    #[test]
    fn run_accepts_org_slug_alias_for_default_org() {
        // `default-org` is the id; `default` is the slug from
        // `ensure_default_org`. Both should resolve to the same row.
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        setup_demo_corpus(&claude_dir, "-home-user-project");
        let db = fresh_db(tmp.path());
        let report = run(&claude_dir, "-home-user-project", "default", true, &db).unwrap();
        assert_eq!(report.summary.inserted, 3);
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM learnings WHERE org_id = 'default-org'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }
}
