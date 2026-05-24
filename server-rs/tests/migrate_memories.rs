//! Integration test for `branchwork-server migrate-memories ...`.
//!
//! Spawns the real binary (no axum server — `migrate-memories` is a
//! synchronous one-shot subcommand that exits when done) against a
//! tempdir-rooted `~/.claude/projects/<user>/memory/` populated with
//! the 9-file demo corpus from `/home/cpo/.claude/projects/-home-cpo-
//! branchwork/memory/`. Asserts (in order) that dry-run prints what it
//! would do AND leaves the DB empty, that `--apply` inserts rows
//! matching the on-disk files, and that re-running `--apply` is
//! idempotent (zero new rows because of the `(org_id, kind, slug)`
//! unique constraint).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_branchwork-server")
}

/// Lay down a representative subset of the demo corpus inside a fresh
/// `<claude_dir>/projects/<user>/memory/` directory. Uses concrete real
/// shapes from the operator's machine so a regression in the parser
/// (frontmatter detection, `metadata.type` nesting, the `user` kind
/// skip) surfaces here.
fn seed_corpus(claude_dir: &Path, user: &str) -> PathBuf {
    let memory_dir = claude_dir.join("projects").join(user).join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    // MEMORY.md — index file, must be skipped.
    std::fs::write(
        memory_dir.join("MEMORY.md"),
        "- [Check runner](feedback_check_runner_after_deploy.md) — index\n",
    )
    .unwrap();

    // feedback (top-level `type`).
    std::fs::write(
        memory_dir.join("feedback_check_runner_after_deploy.md"),
        "---\nname: Check runner after every Hetzner deploy\n\
         description: User feedback\n\
         type: feedback\n\
         originSessionId: abc\n---\n\
         **Why:** prod's container recreate closes every WebSocket.\n\n\
         **How to apply:** ...\n",
    )
    .unwrap();

    // project (nested `metadata.type`).
    std::fs::write(
        memory_dir.join("project_branchwork_as_learning_hub.md"),
        "---\nname: branchwork-as-learning-hub\n\
         description: North-star direction.\n\
         metadata:\n  node_type: memory\n  type: project\n---\n\
         Cyril's vision: ...\n",
    )
    .unwrap();

    // reference (top-level `type`).
    std::fs::write(
        memory_dir.join("reference_grafana_dashboards.md"),
        "---\nname: grafana-dashboards\ntype: reference\n---\n\
         grafana.internal/d/api-latency is the oncall latency dashboard.\n",
    )
    .unwrap();

    // user — unsupported kind, must be skipped (not promoted to org).
    std::fs::write(
        memory_dir.join("user_role.md"),
        "---\nname: user-role\ntype: user\n---\nuser context\n",
    )
    .unwrap();

    // broken — no frontmatter at all.
    std::fs::write(
        memory_dir.join("broken_no_frontmatter.md"),
        "this file has no frontmatter and should be skipped\n",
    )
    .unwrap();

    memory_dir
}

/// Open the in-tree SQLite DB and count rows in `learnings` filtered
/// by org. We intentionally do not use the `/api/learnings` HTTP route
/// here because that would require spinning up the server; the CLI
/// uses the same DB file, so a direct read is the lowest-friction
/// assertion.
fn count_learnings(claude_dir: &Path, org_id: &str) -> i64 {
    let db = claude_dir.join("branchwork.db");
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM learnings WHERE org_id = ?1",
        rusqlite::params![org_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
}

/// `(kind, category, slug)` per row, ordered by slug — gives a stable
/// snapshot the test can diff.
fn learnings_snapshot(claude_dir: &Path, org_id: &str) -> Vec<(String, String, String)> {
    let db = claude_dir.join("branchwork.db");
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT kind, category, slug FROM learnings \
             WHERE org_id = ?1 \
             ORDER BY slug",
        )
        .unwrap();
    stmt.query_map(rusqlite::params![org_id], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn run_migrate(claude_dir: &Path, user: &str, org: &str, apply: bool) -> (i32, String, String) {
    let mut cmd = Command::new(binary());
    cmd.arg("--claude-dir").arg(claude_dir);
    cmd.arg("migrate-memories");
    // Use the `--user=<value>` form (not `--user <value>`) because the
    // real Claude Code convention produces project subdir names that
    // begin with a dash (`-home-cpo-branchwork`) — without `=`, clap
    // sees `-h` and trips on it as a short flag.
    cmd.arg(format!("--user={user}"));
    cmd.arg("--org").arg(org);
    if apply {
        cmd.arg("--apply");
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().expect("spawn branchwork-server");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn dry_run_then_apply_then_rerun_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let user = "-tmp-user-project";
    let memory_dir = seed_corpus(&claude_dir, user);

    // ── 1. Dry-run: no rows written, output mentions 3 would-inserts.
    let (status, stdout, stderr) = run_migrate(&claude_dir, user, "default-org", false);
    assert_eq!(status, 0, "dry-run must exit 0; stderr={stderr}");
    assert!(
        stdout.contains("DRY-RUN"),
        "stdout missing DRY-RUN: {stdout}"
    );
    assert!(
        stdout.contains("would_insert=3"),
        "expected 3 would-inserts: {stdout}"
    );
    assert!(
        stdout.contains("inserted=0"),
        "dry-run must not insert: {stdout}"
    );
    assert_eq!(
        count_learnings(&claude_dir, "default-org"),
        0,
        "dry-run must leave the DB untouched"
    );

    // ── 2. Apply: rows match the on-disk files.
    let (status, stdout, stderr) = run_migrate(&claude_dir, user, "default-org", true);
    assert_eq!(status, 0, "apply must exit 0; stderr={stderr}");
    assert!(
        stdout.contains("APPLY"),
        "stdout missing APPLY marker: {stdout}"
    );
    assert!(
        stdout.contains("inserted=3"),
        "expected 3 inserts: {stdout}"
    );

    let snapshot = learnings_snapshot(&claude_dir, "default-org");
    assert_eq!(
        snapshot,
        vec![
            (
                "feedback".to_string(),
                "feedback".to_string(),
                "feedback-check-runner-after-deploy".to_string(),
            ),
            (
                "project".to_string(),
                "project".to_string(),
                "project-branchwork-as-learning-hub".to_string(),
            ),
            (
                "reference".to_string(),
                "reference".to_string(),
                "reference-grafana-dashboards".to_string(),
            ),
        ],
        "rows match the on-disk files (acceptance criterion)"
    );

    // body_md preserves the full file content (frontmatter included)
    // for audit traceability.
    let db = claude_dir.join("branchwork.db");
    let conn = Connection::open(&db).unwrap();
    let body_md: String = conn
        .query_row(
            "SELECT body_md FROM learnings WHERE slug = 'feedback-check-runner-after-deploy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        body_md.starts_with("---\n"),
        "body_md should preserve frontmatter; got: {}",
        body_md.chars().take(40).collect::<String>()
    );
    assert!(
        body_md.contains("**Why:**"),
        "body_md should preserve body content"
    );

    // On-disk files MUST be untouched — they remain the user-private slot.
    assert!(
        memory_dir
            .join("feedback_check_runner_after_deploy.md")
            .exists(),
        "memory file must survive migration"
    );

    // ── 3. Re-run apply: 0 new inserts, 6 skipped, DB row count unchanged.
    let (status, stdout, stderr) = run_migrate(&claude_dir, user, "default-org", true);
    assert_eq!(status, 0, "rerun must exit 0; stderr={stderr}");
    assert!(
        stdout.contains("inserted=0"),
        "rerun must not insert again (idempotency): {stdout}"
    );
    // 6 = 3 (already-migrated) + 1 (user kind) + 1 (broken frontmatter) + 1 (index).
    assert!(
        stdout.contains("skipped=6"),
        "rerun skip count must be 6: {stdout}"
    );
    assert_eq!(
        count_learnings(&claude_dir, "default-org"),
        3,
        "row count unchanged after rerun"
    );
}

#[test]
fn errors_when_user_dir_is_missing() {
    let tmp = TempDir::new().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // No `seed_corpus` — bare claude_dir.

    let (status, _stdout, stderr) = run_migrate(&claude_dir, "ghost-user", "default-org", true);
    assert_ne!(status, 0, "missing memory dir must be a hard error");
    assert!(
        stderr.contains("memory directory not found"),
        "expected a clear missing-dir message; got: {stderr}"
    );
}

#[test]
fn errors_when_org_is_unknown() {
    let tmp = TempDir::new().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let user = "-tmp-user-project";
    seed_corpus(&claude_dir, user);

    let (status, _stdout, stderr) = run_migrate(&claude_dir, user, "totally-bogus-org", true);
    assert_ne!(status, 0, "unknown org must be a hard error");
    assert!(
        stderr.contains("org not found"),
        "expected `org not found`; got: {stderr}"
    );
}

#[test]
fn accepts_default_org_slug_alias() {
    let tmp = TempDir::new().unwrap();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let user = "-tmp-user-project";
    seed_corpus(&claude_dir, user);

    // "default" is the SLUG of "default-org" — the CLI accepts both so
    // an operator passing the dashboard-visible slug Just Works.
    let (status, stdout, _stderr) = run_migrate(&claude_dir, user, "default", true);
    assert_eq!(status, 0, "slug alias must resolve");
    assert!(stdout.contains("inserted=3"));
    assert_eq!(count_learnings(&claude_dir, "default-org"), 3);
}
