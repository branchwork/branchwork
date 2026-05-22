//! E2E tests for `POST /api/projects/{id}/clone` (Phase 2.2 of
//! runner-daemon-workspace).
//!
//! Acceptance criterion from the task brief:
//!
//! > With a stubbed no-op credential the runner clones a public repo into
//! > `$HOME/<name>` and the project row's `workspace_path` is updated.
//!
//! Standalone-mode tests use a local `file://` bare repo as the "remote"
//! so they run offline and deterministically — `git clone <url> <dst>`
//! treats `file://` URLs the same as `https://` ones, so the contract
//! the dispatcher rides is the same. The SaaS-mode branch (round-trip
//! via runner) is covered by the in-process runner tests in
//! `src/bin/branchwork_runner.rs::tests` since wiring up a real WS
//! handshake here would duplicate `saas/runner_rpc.rs` coverage.

mod support;

use serde_json::json;
use std::path::Path;
use std::process::Command;
use support::TestDashboard;

/// Run a `git` command in `dir`. Mirrors the helper in
/// `tests/support/mod.rs` (private to that file); copied here so this
/// test file is self-contained.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git invocation");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Seed a local bare repo at `bare_path` with one commit pushed to it.
/// Returns the `file://` URL pointing at the bare repo — usable directly
/// as `git clone <url>` input.
fn seed_bare_origin(workdir: &Path, bare_path: &Path) -> String {
    let seed = workdir.join("clone-test-seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "master"]);
    git(&seed, &["config", "user.email", "test@branchwork.local"]);
    git(&seed, &["config", "user.name", "Test"]);
    std::fs::write(seed.join("README.md"), "cloned content\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    std::fs::create_dir_all(bare_path).unwrap();
    git(bare_path, &["init", "--bare", "-q", "-b", "master"]);
    git(
        &seed,
        &["remote", "add", "origin", &bare_path.display().to_string()],
    );
    git(&seed, &["push", "-q", "origin", "master"]);
    format!("file://{}", bare_path.display())
}

/// Headline acceptance test for Phase 2.2: clone a "public" repo (local
/// `file://` bare) into `$HOME/<name>` and verify the project row's
/// `workspace_path` was updated to the runner-resolved absolute path.
#[test]
fn clone_project_into_home_updates_workspace_path() {
    let d = TestDashboard::new();
    let home = d.dir.path();
    let bare = d.dir.path().join("clone-acceptance-origin.git");
    let url = seed_bare_origin(d.dir.path(), &bare);

    // 1. Create the project row. Default `workspace_path` resolves to
    //    `$HOME/<name>` — i.e. our tempdir.
    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "clone-acceptance",
            "repo_url": url,
            "credentialId": "stub-no-op-cred",
        }),
    );
    assert_eq!(status, 201, "create: {body}");
    let id = body["id"].as_str().expect("id").to_string();
    let workspace_path = body["workspacePath"]
        .as_str()
        .expect("workspacePath")
        .to_string();
    let expected_under_home = home.join("clone-acceptance");
    assert_eq!(
        workspace_path,
        expected_under_home.display().to_string(),
        "default workspace_path should land under $HOME/<name>"
    );

    // The clone is supposed to *create* the directory — it must not exist
    // yet, otherwise the runner's `destination already exists` guard would
    // refuse.
    assert!(!expected_under_home.exists());

    // 2. Trigger the clone.
    let (status, body) = d.post(&format!("/api/projects/{id}/clone"), json!({}));
    assert_eq!(status, 200, "clone: {body}");
    assert_eq!(body["ok"].as_bool(), Some(true));
    let resolved_path = body["resolvedPath"].as_str().expect("resolvedPath");
    assert_eq!(resolved_path, expected_under_home.display().to_string());

    // 3. Verify the working tree landed on disk.
    assert!(
        expected_under_home.join(".git").is_dir(),
        ".git dir must exist after clone"
    );
    assert!(
        expected_under_home.join("README.md").is_file(),
        "cloned README must exist"
    );

    // 4. Verify the project row's workspace_path was updated.
    let (status, body) = d.get("/api/projects");
    assert_eq!(status, 200);
    let projects = body["projects"].as_array().expect("projects array");
    let row = projects
        .iter()
        .find(|p| p["id"].as_str() == Some(&id))
        .expect("project row still present");
    assert_eq!(
        row["workspacePath"].as_str(),
        Some(resolved_path),
        "workspace_path on the project row must match the resolved path"
    );
}

#[test]
fn clone_project_returns_404_for_unknown_id() {
    let d = TestDashboard::new();
    let (status, body) = d.post("/api/projects/no-such-id/clone", json!({}));
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("project_not_found"));
}

#[test]
fn clone_project_returns_500_on_git_failure() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "bad-clone",
            "repo_url": "/tmp/branchwork-no-such-repo-ever",
        }),
    );
    assert_eq!(status, 201, "create: {body}");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = d.post(&format!("/api/projects/{id}/clone"), json!({}));
    assert_eq!(status, 500, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("clone_failed"));
    assert!(
        body["message"]
            .as_str()
            .map(|m| !m.is_empty())
            .unwrap_or(false),
        "clone_failed must carry a non-empty error message, got: {body}"
    );
}

#[test]
fn clone_project_refuses_when_destination_already_exists() {
    let d = TestDashboard::new();
    let home = d.dir.path();
    let dest = home.join("already-cloned");
    std::fs::create_dir_all(&dest).unwrap();

    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "already-cloned",
            "repo_url": "file:///does/not/matter.git",
        }),
    );
    assert_eq!(status, 201, "create: {body}");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = d.post(&format!("/api/projects/{id}/clone"), json!({}));
    assert_eq!(status, 500, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("clone_failed"));
    assert!(
        body["message"]
            .as_str()
            .map(|m| m.contains("destination already exists"))
            .unwrap_or(false),
        "expected 'destination already exists' in error, got: {body}"
    );
}

#[test]
fn clone_project_honors_explicit_workspace_path_override() {
    let d = TestDashboard::new();
    let bare = d.dir.path().join("clone-override-origin.git");
    let url = seed_bare_origin(d.dir.path(), &bare);
    let explicit = d.dir.path().join("custom-dir/sub/proj");

    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "override-target",
            "repo_url": url,
            "workspace_path": explicit.to_string_lossy(),
        }),
    );
    assert_eq!(status, 201, "create: {body}");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = d.post(&format!("/api/projects/{id}/clone"), json!({}));
    assert_eq!(status, 200, "clone: {body}");
    assert_eq!(
        body["resolvedPath"].as_str(),
        Some(explicit.display().to_string()).as_deref()
    );
    assert!(explicit.join(".git").is_dir());
}
