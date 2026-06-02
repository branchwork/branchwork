//! Integration tests for `GET /api/orgs/:slug/orphan-worktrees` (Phase 5,
//! Task 5.1 of worktree-per-agent-isolation).
//!
//! These spin up the real `branchwork-server` binary against a scratch
//! tempdir, seed a `worktree_orphans` row directly via SQLite (standing in
//! for what the boot sweep records), authenticate via `/api/auth/signup`,
//! and exercise the admin-gated read endpoint end-to-end:
//!
//! - owner of an org → 200 with the orphan in the snake_case JSON shape
//! - unauthenticated → 401
//! - member (not admin) of the org → 403 admin_required
//! - unknown org slug → 404 org_not_found

mod support;

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

use support::TestDashboard;

fn signup(d: &TestDashboard, jar: &Path, email: &str, password: &str) -> Value {
    let body = json!({ "email": email, "password": password });
    let (status, value) = http_with_jar(
        "POST",
        &format!("{}/api/auth/signup", d.base_url),
        Some(body),
        Some(jar),
    );
    assert_eq!(status, 201, "signup failed: status={status} body={value}");
    value
}

fn get_authed(d: &TestDashboard, jar: &Path, path: &str) -> (u16, Value) {
    http_with_jar("GET", &format!("{}{path}", d.base_url), None, Some(jar))
}

/// Cookie-aware curl helper (mirrors `tests/credentials.rs`).
fn http_with_jar(method: &str, url: &str, body: Option<Value>, jar: Option<&Path>) -> (u16, Value) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-o",
        "-",
        "-w",
        "\n\n__STATUS__:%{http_code}",
        "-X",
        method,
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(j) = jar {
        cmd.args(["-c", &j.to_string_lossy(), "-b", &j.to_string_lossy()]);
    }
    cmd.arg(url);
    let body_str;
    if let Some(b) = body {
        body_str = serde_json::to_string(&b).unwrap();
        cmd.args(["-d", &body_str]);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("curl: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body_str, status_str) = stdout
        .rsplit_once("\n\n__STATUS__:")
        .unwrap_or_else(|| panic!("bad curl output: {stdout}"));
    let status: u16 = status_str.trim().parse().unwrap_or(0);
    let value: Value = if body_str.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body_str).unwrap_or(Value::String(body_str.to_string()))
    };
    (status, value)
}

/// Insert a `worktree_orphans` row straight into the server's SQLite DB —
/// standing in for the boot sweep so the test is deterministic.
fn seed_orphan(d: &TestDashboard, slug: &str, path: &str, branch: &str) {
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "INSERT INTO worktree_orphans \
            (project_slug, path, branch, last_modified, first_detected) \
         VALUES (?1, ?2, ?3, datetime('now'), datetime('now'))",
        rusqlite::params![slug, path, branch],
    )
    .expect("seed orphan row");
}

/// Find the slug of the org the signed-up user owns (their personal org).
fn owner_org_slug(d: &TestDashboard, jar: &Path) -> String {
    let (status, orgs) = get_authed(d, jar, "/api/orgs");
    assert_eq!(status, 200, "GET /api/orgs failed: {orgs}");
    orgs.as_array()
        .expect("orgs array")
        .iter()
        .find(|o| o["role"] == "owner")
        .and_then(|o| o["slug"].as_str())
        .map(|s| s.to_string())
        .expect("user should own at least one org")
}

#[test]
fn admin_sees_recorded_orphan_with_snake_case_shape() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "owner@example.com", "hunter2hunter2");

    seed_orphan(
        &d,
        "branchwork",
        "/home/x/.branchwork/worktrees/branchwork/my-plan/0.1-abcd1234",
        "branchwork/my-plan/0.1",
    );

    let slug = owner_org_slug(&d, &jar);
    let (status, body) = get_authed(&d, &jar, &format!("/api/orgs/{slug}/orphan-worktrees"));
    assert_eq!(status, 200, "expected 200, got {status}: {body}");

    let orphans = body["orphans"].as_array().expect("orphans array");
    assert_eq!(orphans.len(), 1, "exactly the seeded orphan: {body}");
    let o = &orphans[0];
    assert_eq!(
        o["path"],
        "/home/x/.branchwork/worktrees/branchwork/my-plan/0.1-abcd1234"
    );
    assert_eq!(o["project_slug"], "branchwork");
    assert_eq!(o["branch"], "branchwork/my-plan/0.1");
    // snake_case keys are present (last_modified / first_detected, not camelCase).
    assert!(
        o.get("last_modified").is_some(),
        "last_modified key present"
    );
    assert!(
        o.get("first_detected").and_then(|v| v.as_str()).is_some(),
        "first_detected populated"
    );
}

#[test]
fn unauthenticated_request_is_rejected() {
    let d = TestDashboard::new();
    // No cookie jar → no session.
    let (status, _body) = http_with_jar(
        "GET",
        &format!("{}/api/orgs/default/orphan-worktrees", d.base_url),
        None,
        None,
    );
    assert_eq!(status, 401, "anonymous caller must be 401");
}

#[test]
fn member_without_admin_role_is_forbidden() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "member@example.com", "hunter2hunter2");
    // Signup adds the user to the default org as a plain `member`, not an
    // admin, so the admin-gated endpoint refuses for the `default` slug.
    let (status, body) = get_authed(&d, &jar, "/api/orgs/default/orphan-worktrees");
    assert_eq!(status, 403, "member must be 403: {body}");
    assert_eq!(body["error"], "admin_required");
}

#[test]
fn unknown_org_slug_is_not_found() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "ghost@example.com", "hunter2hunter2");
    let (status, body) = get_authed(&d, &jar, "/api/orgs/no-such-org/orphan-worktrees");
    assert_eq!(status, 404, "unknown slug must be 404: {body}");
    assert_eq!(body["error"], "org_not_found");
}
