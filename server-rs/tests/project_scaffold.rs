//! E2E tests for the per-project `branchwork.toml` scaffold (cadence
//! plan T4.2). Cover:
//!
//! - `POST /api/plans/create` with `createFolder: true` against a name
//!   that doesn't exist yet → folder + `branchwork.toml` are both on
//!   disk, with `merge_cadence = "phase"`.
//! - `POST /api/plans/create` with `createFolder: true` against a name
//!   that already exists on disk → branchwork.toml is NOT scaffolded
//!   (the user's project is left alone).
//!
//! The runner-side path is covered by inline unit tests in
//! `bin/branchwork_runner.rs::tests::check_or_create_folder_*` — driving
//! a real WS round-trip through the binary harness adds protocol
//! surface without strengthening the scaffold claim (same logic the
//! `tests/folders.rs` header explains for the runner-vs-local dispatch).
//!
//! Windows skip: `dirs::home_dir()` on Windows reads from the Win32 API
//! and ignores `HOME`/`USERPROFILE`, so the create_plan resolver lands
//! on the real runner profile rather than our tempdir. Same caveat the
//! sibling test files document.

#![cfg(not(windows))]

mod support;

use serde_json::json;
use support::TestDashboard;

/// Acceptance criterion for the cadence plan T4.2:
/// "A freshly-created project has `merge_cadence = "phase"` in its
/// `branchwork.toml`."
#[test]
fn create_plan_scaffolds_branchwork_toml_with_phase_cadence_on_fresh_folder() {
    let d = TestDashboard::new();
    let folder_name = format!("scaffold-fresh-{}", uuid::Uuid::new_v4());
    let (status, body) = d.post(
        "/api/plans/create",
        json!({
            "description": "freshly-created project for the scaffold test",
            "folder": folder_name,
            "createFolder": true,
        }),
    );
    assert_eq!(status, 200, "body: {body}");

    let resolved = d.dir.path().join(&folder_name);
    assert!(
        resolved.is_dir(),
        "create_plan should mkdir the requested folder: {}",
        resolved.display()
    );
    let toml_path = resolved.join("branchwork.toml");
    assert!(
        toml_path.exists(),
        "scaffold MUST drop branchwork.toml into a freshly-created folder: {}",
        toml_path.display()
    );
    let toml_body = std::fs::read_to_string(&toml_path).expect("read scaffolded toml");
    assert!(
        toml_body.contains("merge_cadence = \"phase\""),
        "scaffolded body must pin merge_cadence to phase:\n{toml_body}"
    );
}

/// Counterpart: pre-existing folders keep their on-disk state. The
/// brief acceptance — "Pre-existing projects keep their explicit (or
/// grandfathered) setting." — means the scaffold MUST NOT fire on a
/// folder that existed before the create_plan call, even if no
/// `branchwork.toml` is present (the absence is its own opinion —
/// `MergeCadence::default()` resolves to `phase` anyway, but the
/// scaffold not running is the operator-visible promise).
#[test]
fn create_plan_does_not_scaffold_branchwork_toml_on_existing_folder() {
    let d = TestDashboard::new();
    let folder_name = format!("scaffold-existing-{}", uuid::Uuid::new_v4());
    let resolved = d.dir.path().join(&folder_name);
    std::fs::create_dir_all(&resolved).expect("pre-create folder");

    let (status, body) = d.post(
        "/api/plans/create",
        json!({
            "description": "user picks an existing folder for the plan",
            "folder": folder_name,
            "createFolder": true,
        }),
    );
    assert_eq!(status, 200, "body: {body}");

    let toml_path = resolved.join("branchwork.toml");
    assert!(
        !toml_path.exists(),
        "scaffold MUST NOT fire on a pre-existing project folder: {}",
        toml_path.display()
    );
}

/// Pre-existing `branchwork.toml` (user already pinned a different
/// cadence) is left verbatim. The scaffold helper's idempotency is
/// covered by the unit tests in `project_scaffold.rs::tests`; this
/// e2e test makes sure the end-to-end HTTP path doesn't accidentally
/// overwrite by going through a different code path.
#[test]
fn create_plan_preserves_existing_branchwork_toml_verbatim() {
    let d = TestDashboard::new();
    let folder_name = format!("scaffold-preserve-{}", uuid::Uuid::new_v4());
    let resolved = d.dir.path().join(&folder_name);
    std::fs::create_dir_all(&resolved).expect("pre-create folder");
    let toml_path = resolved.join("branchwork.toml");
    let user_body = "[auto_mode]\nmerge_cadence = \"task\"\n";
    std::fs::write(&toml_path, user_body).expect("seed user branchwork.toml");

    let (status, body) = d.post(
        "/api/plans/create",
        json!({
            "description": "user already has an explicit branchwork.toml",
            "folder": folder_name,
            "createFolder": true,
        }),
    );
    assert_eq!(status, 200, "body: {body}");

    let on_disk = std::fs::read_to_string(&toml_path).expect("read");
    assert_eq!(
        on_disk, user_body,
        "pre-existing branchwork.toml must be preserved verbatim"
    );
}
