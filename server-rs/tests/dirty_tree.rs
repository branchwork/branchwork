//! E2E tests for the per-project dirty-tree allowlist when the canonical
//! `branchwork.toml` shipped at the repo root is in effect.
//!
//! Acceptance criterion for Task 2.2 of
//! `dirty-tree-check-stop-pausing-on-write-noise`: on master with Phase 1
//! NOT yet applied (i.e. tracked `*.log` files still present in the
//! working tree), the dirty-tree check returns `Clean` because the
//! shipped `branchwork.toml` filters `*.log` and `.bob/**`. The test
//! drives the HTTP surface (PUT /api/plans/:n/tasks/:num/status with
//! status=completed) because `branchwork-server` is binary-only and the
//! `agents::check_tree_clean_for_completion` helper can't be imported
//! from an integration test crate.
//!
//! See also: `server-rs/src/agents/mod.rs` unit tests (Task 2.1) which
//! cover the in-process matcher against hand-written `branchwork.toml`
//! fixtures. This file pins the actual file shipped at the repo root.

mod support;

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::TestDashboard;

/// Absolute path to the canonical `branchwork.toml` shipped at the
/// branchwork repo root — i.e. the file produced by this task.
/// `CARGO_MANIFEST_DIR` points at `server-rs/`; the repo root is one
/// level up. Computed at runtime (not `include_str!`) so the test
/// exercises the file as it lives on disk.
fn canonical_branchwork_toml() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("CARGO_MANIFEST_DIR should have a parent (the repo root)")
        .join("branchwork.toml")
}

fn run_git(args: &[&str], cwd: &Path) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Commit a `server.log` on the project's master branch to mimic the
/// pre-Phase-1 state (the file was tracked until Task 1.1 untracked it
/// via `git rm --cached`). Then dirty the file by rewriting it.
fn track_then_dirty_server_log(project: &Path) {
    std::fs::write(project.join("server.log"), "tracked baseline\n").unwrap();
    run_git(&["add", "server.log"], project);
    run_git(
        &["commit", "-q", "-m", "track server.log (pre-phase-1)"],
        project,
    );
    // Now dirty it — mimics a live `cargo run` writing to the file.
    std::fs::write(
        project.join("server.log"),
        "[server] starting up...\n[server] listening on :3100\n",
    )
    .unwrap();
}

fn single_task_plan(name: &str, project_dir: &Path) -> String {
    // NB: literal `\n` escapes inside a single-line string preserve
    // indentation in the rendered YAML. The backslash-newline form
    // (e.g. `"foo\n\<newline>  bar\n"`) silently strips the leading
    // whitespace of every continuation line and produces unindented
    // YAML — bug flagged in the auto-mode phase-verify plan T1.2
    // learning. Match `tests/plan_config.rs::minimal_plan` exactly.
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

/// Acceptance for Task 2.2: the canonical `branchwork.toml` shipped in
/// this repo lets the dirty-tree check return `Clean` even when a
/// tracked `*.log` is dirty, so plans don't pause on operational
/// write-noise.
#[test]
fn dirty_tree_check_returns_clean_with_canonical_branchwork_toml() {
    let d = TestDashboard::new();
    track_then_dirty_server_log(&d.project);

    // Copy the SHIPPED branchwork.toml into the scratch project — this
    // is the file we want to pin the behaviour of. Reading from disk
    // (not include_str!) means deleting or breaking the file at the
    // repo root will surface here.
    let canonical = canonical_branchwork_toml();
    std::fs::copy(&canonical, d.project.join("branchwork.toml"))
        .unwrap_or_else(|e| panic!("copy {} → project: {e}", canonical.display()));

    let plan_name = "dirty-tree-canonical-toml";
    d.create_plan(plan_name, &single_task_plan(plan_name, &d.project));

    let (status, body) = d.put(
        &format!("/api/plans/{plan_name}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );

    // 200 = Clean → status flipped to completed. 409 with
    // error="working_tree_dirty" is the failure signal: either the
    // canonical file is missing, the `*.log` pattern was dropped, or
    // the matcher regressed.
    assert_eq!(
        status,
        200,
        "expected 200 (Clean), got {status}; body={body}; \
         canonical branchwork.toml={}",
        canonical.display()
    );
}

/// Regression-side: without the `branchwork.toml`, the same dirty
/// `server.log` MUST still trip the dirty-tree check. Pins the
/// allowlist as the actual cause of the Clean verdict above — not
/// some other accidental short-circuit.
#[test]
fn dirty_tree_check_returns_dirty_without_branchwork_toml() {
    let d = TestDashboard::new();
    track_then_dirty_server_log(&d.project);
    // Deliberately do NOT copy branchwork.toml into the project.

    let plan_name = "dirty-tree-no-toml";
    d.create_plan(plan_name, &single_task_plan(plan_name, &d.project));

    let (status, body) = d.put(
        &format!("/api/plans/{plan_name}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );

    assert_eq!(
        status, 409,
        "expected 409 (Dirty without allowlist), got {status}; body={body}"
    );
    assert_eq!(body["error"], "working_tree_dirty", "body={body}");
}

/// Acceptance for the second pattern in the canonical file: a tracked
/// `.bob/`-rooted path must also be filtered. The `.bob/` family is
/// covered by Phase 1 Task 1.2 but, like `*.log`, older checkouts may
/// still have files tracked under it.
#[test]
fn dirty_tree_check_returns_clean_for_dirty_bob_dir_with_canonical_branchwork_toml() {
    let d = TestDashboard::new();
    // Track a sentinel file under .bob/ so `git status` will surface
    // it when modified — mirrors the pre-Phase-1 .bob/notes/pending-notes.txt
    // state.
    std::fs::create_dir_all(d.project.join(".bob")).unwrap();
    std::fs::write(d.project.join(".bob/notes.txt"), "baseline\n").unwrap();
    run_git(&["add", ".bob/notes.txt"], &d.project);
    run_git(
        &["commit", "-q", "-m", "track .bob/notes.txt (pre-phase-1)"],
        &d.project,
    );
    std::fs::write(d.project.join(".bob/notes.txt"), "scratched\n").unwrap();

    std::fs::copy(
        canonical_branchwork_toml(),
        d.project.join("branchwork.toml"),
    )
    .unwrap();

    let plan_name = "dirty-tree-bob-glob";
    d.create_plan(plan_name, &single_task_plan(plan_name, &d.project));

    let (status, body) = d.put(
        &format!("/api/plans/{plan_name}/tasks/1.1/status"),
        json!({"status": "completed"}),
    );
    assert_eq!(
        status, 200,
        "expected 200 (Clean — .bob/** ignored), got {status}; body={body}"
    );
}

/// Sanity test for the canonical TOML itself: it parses, and the two
/// patterns we depend on (`*.log`, `.bob/**`) are both present. Catches
/// a regression where the file is reformatted/replaced in a way that
/// keeps it valid TOML but loses one of the keys.
#[test]
fn canonical_branchwork_toml_carries_minimum_viable_ignore_list() {
    let path = canonical_branchwork_toml();
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let parsed: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let ignore = parsed
        .get("auto_mode")
        .and_then(|v| v.get("dirty_tree"))
        .and_then(|v| v.get("ignore"))
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| {
            panic!(
                "expected [auto_mode.dirty_tree] ignore = [...] in {}",
                path.display()
            )
        });
    let entries: Vec<&str> = ignore.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        entries.contains(&"*.log"),
        "canonical branchwork.toml must keep `*.log` in ignore list, got {entries:?}"
    );
    assert!(
        entries.contains(&".bob/**"),
        "canonical branchwork.toml must keep `.bob/**` in ignore list, got {entries:?}"
    );
}
