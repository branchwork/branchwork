//! Leaf module that scaffolds the per-project `branchwork.toml` shipped
//! with freshly-created projects.
//!
//! Plan `ci-cadence-build-vs-test-configurable` task 4.2 flips the
//! default merge cadence for **newly-created projects** from the
//! legacy `task` value to `phase` (the spec'd default from the original
//! proposition). Task 1.2 of the same plan already grandfathered every
//! existing plan_auto_mode row to `task` so live work continues unchanged.
//! This module is the on-disk side of the flip: when Branchwork creates
//! a new project folder (`POST /api/plans/create` with
//! `createFolder: true` on standalone, or `CreateFolder { create_if_missing: true }`
//! on the runner), it drops a small `branchwork.toml` next to the
//! freshly-`mkdir`d directory that pins `merge_cadence = "phase"`
//! explicitly.
//!
//! The scaffold is intentionally **opt-out-by-skip**: pre-existing
//! projects (the user picks a folder that already exists) are left
//! untouched. They inherit `MergeCadence::default()` which is also
//! `phase`, so the behaviour matches the brief acceptance
//! ("Pre-existing projects keep their explicit (or grandfathered)
//! setting.") — explicit-via-file for new projects, implicit-via-default
//! for pre-existing ones with no `branchwork.toml`. Pre-existing
//! projects that already carry a `branchwork.toml` keep whatever
//! `merge_cadence` they declared (the helper is idempotent: file-exists
//! check returns `false` without writing).
//!
//! Self-contained: no `crate::` deps. Both the server binary and the
//! runner binary include this file (the server via `mod project_scaffold;`
//! in `main.rs`, the runner via `#[path = "../project_scaffold.rs"]`)
//! because the file lands on whichever host actually creates the
//! project folder — standalone server in self-hosted mode, runner in
//! SaaS mode.

#![allow(dead_code)] // Both binaries include this module; each uses the same single fn.

use std::path::Path;

/// Body written to a freshly-scaffolded `branchwork.toml`. Pinned at
/// `phase` per the cadence plan's acceptance criterion. Comments mirror
/// the inline schema reference in [`crate::repo_config`] doc-comment so
/// a user reading the scaffolded file can pick a different cadence
/// without cross-referencing.
const SCAFFOLD_BODY: &str = "# Branchwork project config. Full schema reference:\n\
                             #   docs/reference/branchwork-toml.md\n\
                             \n\
                             [auto_mode]\n\
                             # When auto-mode should merge a task's branch into the default branch:\n\
                             #   \"task\"  — every completed task (legacy, fastest feedback)\n\
                             #   \"phase\" — at phase boundary (default for new projects)\n\
                             #   \"plan\"  — only at end of plan (one shipped version per plan)\n\
                             merge_cadence = \"phase\"\n";

/// Drop a minimal `branchwork.toml` into `cwd` if one doesn't already
/// exist. Returns `true` if a file was newly written, `false` if one
/// already existed OR the write failed (best-effort — never panics,
/// never blocks project creation).
///
/// Idempotent: safe to call repeatedly. Callers can rely on the post-
/// condition `cwd/branchwork.toml` existing on `Ok(true)` and on
/// "existed beforehand OR write failed" semantics on `false`.
pub fn scaffold_branchwork_toml_if_missing(cwd: &Path) -> bool {
    let path = cwd.join("branchwork.toml");
    if path.exists() {
        return false;
    }
    match std::fs::write(&path, SCAFFOLD_BODY) {
        Ok(()) => {
            println!(
                "[branchwork] scaffolded branchwork.toml in {}",
                cwd.display()
            );
            true
        }
        Err(e) => {
            eprintln!(
                "[branchwork] failed to scaffold branchwork.toml in {}: {e}",
                cwd.display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scaffolds_when_missing_and_returns_true() {
        let dir = TempDir::new().expect("tempdir");
        let did_write = scaffold_branchwork_toml_if_missing(dir.path());
        assert!(did_write, "first call should write the file");
        let path = dir.path().join("branchwork.toml");
        assert!(path.exists(), "branchwork.toml should exist on disk");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(
            body.contains("merge_cadence = \"phase\""),
            "scaffolded body must pin merge_cadence to phase: {body}"
        );
    }

    #[test]
    fn no_op_when_file_already_exists() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("branchwork.toml");
        // Pre-existing project: user already has a branchwork.toml with
        // their explicit cadence pick (or any unrelated content). The
        // scaffold MUST NOT overwrite it.
        std::fs::write(&path, "merge_cadence = \"task\"\n").expect("seed");
        let did_write = scaffold_branchwork_toml_if_missing(dir.path());
        assert!(!did_write, "second call on existing file should no-op");
        let body = std::fs::read_to_string(&path).expect("read");
        assert_eq!(body, "merge_cadence = \"task\"\n", "body must be preserved");
    }

    #[test]
    fn scaffolded_body_parses_as_phase_via_repo_config() {
        // The on-disk body must round-trip through the production
        // RepoConfig deserializer — otherwise the scaffold would write
        // a file the rest of the server can't load. We don't import
        // `crate::repo_config` here (this is a leaf module the runner
        // also includes) so we parse the body via raw toml + assert
        // the cadence string survives.
        let dir = TempDir::new().expect("tempdir");
        scaffold_branchwork_toml_if_missing(dir.path());
        let body = std::fs::read_to_string(dir.path().join("branchwork.toml")).expect("read");
        let parsed: toml::Value =
            toml::from_str(&body).expect("scaffolded body must be valid TOML");
        let cadence = parsed
            .get("auto_mode")
            .and_then(|v| v.get("merge_cadence"))
            .and_then(|v| v.as_str());
        assert_eq!(cadence, Some("phase"));
    }

    #[test]
    fn idempotent_across_repeated_calls() {
        let dir = TempDir::new().expect("tempdir");
        let first = scaffold_branchwork_toml_if_missing(dir.path());
        let second = scaffold_branchwork_toml_if_missing(dir.path());
        let third = scaffold_branchwork_toml_if_missing(dir.path());
        assert_eq!(
            (first, second, third),
            (true, false, false),
            "first call writes, every subsequent call is a no-op"
        );
    }
}
