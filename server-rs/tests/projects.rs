//! E2E tests for the projects API (Phase 2.1 of runner-daemon-workspace).
//!
//! Covered:
//! - `POST /api/projects` happy path: default `workspace_path` lives under
//!   `$HOME/<name>` (the explicit acceptance criterion for the task).
//! - `POST /api/projects` honors an explicit `workspace_path` override.
//! - Validation: empty `name`, bad slug, missing `repo_url`, unknown host.
//! - Collision: a second insert with the same `(org_id, name)` returns 409.
//! - `GET /api/projects` returns the row(s).
//! - `DELETE /api/projects/{id}` removes the row; `?wipe_on_disk=true`
//!   also clears the workspace directory.

mod support;

use serde_json::json;
use support::TestDashboard;

/// Acceptance criterion from the task brief:
///
/// > `POST /api/projects` with `repo_url` + `credential_id` returns a row
/// > whose `workspace_path` lives under `$HOME` by default.
#[test]
fn create_project_defaults_workspace_path_under_home() {
    let d = TestDashboard::new();
    // TestDashboard sets HOME=tempdir, so anything under d.dir.path() is
    // "under $HOME" for the spawned server.
    let home = d.dir.path();

    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "my-new-project",
            "repo_url": "https://github.com/branchwork/example.git",
            "credentialId": "cred-123",
        }),
    );
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["name"].as_str(), Some("my-new-project"));
    assert_eq!(
        body["repoUrl"].as_str(),
        Some("https://github.com/branchwork/example.git")
    );
    assert_eq!(body["defaultCredentialId"].as_str(), Some("cred-123"));
    assert_eq!(body["host"].as_str(), Some("other")); // default
    let workspace_path = body["workspacePath"].as_str().unwrap_or("");
    assert!(
        workspace_path.starts_with(&home.to_string_lossy().to_string()),
        "expected workspace_path '{workspace_path}' to start with $HOME '{}'",
        home.display()
    );
    assert!(
        workspace_path.ends_with("my-new-project"),
        "expected workspace_path '{workspace_path}' to end with project name"
    );
    assert!(!body["id"].as_str().unwrap_or("").is_empty());
    assert!(!body["createdAt"].as_str().unwrap_or("").is_empty());
}

#[test]
fn create_project_honors_explicit_workspace_path() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "elsewhere",
            "repo_url": "git@github.com:foo/bar.git",
            "workspace_path": "/srv/projects/custom",
        }),
    );
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["workspacePath"].as_str(), Some("/srv/projects/custom"));
}

#[test]
fn create_project_accepts_host_enum() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "gh-project",
            "repo_url": "https://github.com/foo/bar.git",
            "host": "github",
            "owner": "foo",
        }),
    );
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["host"].as_str(), Some("github"));
    assert_eq!(body["owner"].as_str(), Some("foo"));
}

#[test]
fn create_project_rejects_empty_name() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/projects",
        json!({ "name": "", "repo_url": "https://example.com/x.git" }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("name_required"));
}

#[test]
fn create_project_rejects_invalid_name() {
    let d = TestDashboard::new();
    // Path traversal attempt in the name.
    let (status, body) = d.post(
        "/api/projects",
        json!({ "name": "../escape", "repo_url": "https://example.com/x.git" }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("invalid_name"));
}

#[test]
fn create_project_rejects_missing_repo_url() {
    let d = TestDashboard::new();
    let (status, body) = d.post("/api/projects", json!({ "name": "x", "repo_url": "" }));
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("repo_url_required"));
}

#[test]
fn create_project_rejects_unknown_host() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/projects",
        json!({
            "name": "x",
            "repo_url": "https://example.com/x.git",
            "host": "azure",
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("invalid_host"));
}

#[test]
fn create_project_returns_409_on_name_collision() {
    let d = TestDashboard::new();
    let (s1, b1) = d.post(
        "/api/projects",
        json!({
            "name": "dup",
            "repo_url": "https://example.com/a.git",
        }),
    );
    assert_eq!(s1, 201, "body: {b1}");

    let (s2, b2) = d.post(
        "/api/projects",
        json!({
            "name": "dup",
            "repo_url": "https://example.com/b.git",
        }),
    );
    assert_eq!(s2, 409, "body: {b2}");
    assert_eq!(b2["error"].as_str(), Some("name_taken"));
}

#[test]
fn list_projects_returns_inserted_rows() {
    let d = TestDashboard::new();
    let (s1, _) = d.post(
        "/api/projects",
        json!({
            "name": "alpha",
            "repo_url": "https://example.com/a.git",
        }),
    );
    assert_eq!(s1, 201);
    let (s2, _) = d.post(
        "/api/projects",
        json!({
            "name": "bravo",
            "repo_url": "https://example.com/b.git",
        }),
    );
    assert_eq!(s2, 201);

    let (status, body) = d.get("/api/projects");
    assert_eq!(status, 200, "body: {body}");
    let arr = body["projects"]
        .as_array()
        .expect("projects must be an array");
    assert_eq!(arr.len(), 2, "body: {body}");
    let names: Vec<&str> = arr.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"bravo"));
}

#[test]
fn list_projects_returns_empty_array_when_no_rows() {
    let d = TestDashboard::new();
    let (status, body) = d.get("/api/projects");
    assert_eq!(status, 200, "body: {body}");
    let arr = body["projects"].as_array().expect("array");
    assert!(arr.is_empty(), "body: {body}");
}

#[test]
fn delete_project_removes_row_and_returns_200() {
    let d = TestDashboard::new();
    let (s1, body1) = d.post(
        "/api/projects",
        json!({
            "name": "to-delete",
            "repo_url": "https://example.com/x.git",
        }),
    );
    assert_eq!(s1, 201, "body: {body1}");
    let id = body1["id"].as_str().expect("id field").to_string();

    let (s2, body2) = d.delete(&format!("/api/projects/{id}"));
    assert_eq!(s2, 200, "body: {body2}");
    assert_eq!(body2["deleted"].as_bool(), Some(true));
    assert_eq!(body2["wipeAttempted"].as_bool(), Some(false));

    // Second delete is 404.
    let (s3, body3) = d.delete(&format!("/api/projects/{id}"));
    assert_eq!(s3, 404, "body: {body3}");
}

#[test]
fn delete_project_returns_404_for_unknown_id() {
    let d = TestDashboard::new();
    let (status, body) = d.delete("/api/projects/does-not-exist");
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("project_not_found"));
}

#[test]
fn delete_project_with_wipe_on_disk_removes_workspace() {
    let d = TestDashboard::new();
    // Create a project that points at a directory we'll actually populate.
    let workspace = d.dir.path().join("workspace-to-wipe");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("dummy.txt"), "hello").unwrap();
    assert!(workspace.exists());

    let (s1, body1) = d.post(
        "/api/projects",
        json!({
            "name": "wipe-me",
            "repo_url": "https://example.com/x.git",
            "workspace_path": workspace.to_string_lossy(),
        }),
    );
    assert_eq!(s1, 201, "body: {body1}");
    let id = body1["id"].as_str().unwrap().to_string();

    let (s2, body2) = d.delete(&format!("/api/projects/{id}?wipe_on_disk=true"));
    assert_eq!(s2, 200, "body: {body2}");
    assert_eq!(body2["wipeAttempted"].as_bool(), Some(true));
    assert!(body2["wipeFailed"].is_null(), "body: {body2}");
    assert!(!workspace.exists(), "workspace should have been removed");
}

#[test]
fn delete_project_without_wipe_does_not_touch_workspace() {
    let d = TestDashboard::new();
    let workspace = d.dir.path().join("workspace-to-keep");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("dummy.txt"), "hello").unwrap();

    let (s1, body1) = d.post(
        "/api/projects",
        json!({
            "name": "keep-disk",
            "repo_url": "https://example.com/x.git",
            "workspace_path": workspace.to_string_lossy(),
        }),
    );
    assert_eq!(s1, 201, "body: {body1}");
    let id = body1["id"].as_str().unwrap().to_string();

    let (s2, body2) = d.delete(&format!("/api/projects/{id}"));
    assert_eq!(s2, 200, "body: {body2}");
    assert_eq!(body2["wipeAttempted"].as_bool(), Some(false));
    assert!(
        workspace.exists(),
        "workspace must survive a non-wipe delete"
    );
}

#[test]
fn delete_project_wipe_on_missing_dir_succeeds() {
    let d = TestDashboard::new();
    // workspace_path points at a non-existent dir; wipe is a no-op success.
    let workspace = d.dir.path().join("never-created");
    let (s1, body1) = d.post(
        "/api/projects",
        json!({
            "name": "missing-dir",
            "repo_url": "https://example.com/x.git",
            "workspace_path": workspace.to_string_lossy(),
        }),
    );
    assert_eq!(s1, 201, "body: {body1}");
    let id = body1["id"].as_str().unwrap().to_string();

    let (s2, body2) = d.delete(&format!("/api/projects/{id}?wipe_on_disk=true"));
    assert_eq!(s2, 200, "body: {body2}");
    assert_eq!(body2["wipeAttempted"].as_bool(), Some(true));
    assert!(body2["wipeFailed"].is_null(), "body: {body2}");
}
