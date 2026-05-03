//! E2E tests for `GET /api/folders` runner-vs-local dispatch (Task 3.2).
//!
//! Covered:
//! - Standalone (no runner row): handler returns the server's `$HOME` children.
//! - SaaS with offline/missing runner: handler returns 503 `no_runner_connected`.
//!
//! The "SaaS with a connected runner" path is exercised by the in-process
//! integration test in `saas::runner_rpc::tests::real_ws_disconnect_drains_pending_senders_and_wakes_receivers`
//! and the unit tests in `saas::runner_rpc::tests`. Driving a real WS client
//! through the e2e binary harness adds protocol surface without strengthening
//! the dispatch claim, so we skip it here.

mod support;

use rusqlite::params;
use support::TestDashboard;

// Windows `dirs::home_dir()` reads `SHGetKnownFolderPath(FOLDERID_Profile)`
// and ignores the `HOME`/`USERPROFILE` env vars the harness sets, so the scan
// hits the real runner profile rather than our tempdir — same constraint that
// drives `tests/recovery.rs` and `tests/merge_guard.rs` to use absolute paths.
// The `saas_*` companion below still runs on Windows and covers the dispatch
// branch we actually care about.
#[cfg(not(windows))]
#[test]
fn standalone_returns_local_home_folders() {
    let d = TestDashboard::new();
    // The harness sets HOME=tempdir; create a couple of visible dirs and a
    // hidden one so the response shape is meaningful.
    std::fs::create_dir_all(d.dir.path().join("projects")).unwrap();
    std::fs::create_dir_all(d.dir.path().join("notes")).unwrap();
    std::fs::create_dir_all(d.dir.path().join(".hidden")).unwrap();

    let (status, body) = d.get("/api/folders");
    assert_eq!(status, 200, "body: {body}");

    let entries = body
        .as_array()
        .unwrap_or_else(|| panic!("expected array, got {body}"));
    let names: Vec<String> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "projects"),
        "missing 'projects' in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "notes"),
        "missing 'notes' in {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with('.')),
        "hidden dir leaked: {names:?}"
    );
}

#[test]
fn saas_with_no_connected_runner_returns_503() {
    let d = TestDashboard::new();
    // Seed a runner row for the default org without any active WS connection.
    // org_has_runner returns true (any row counts), so the handler routes to
    // the runner; runner_request finds nothing in the in-memory registry and
    // returns NoConnectedRunner -> 503.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
         VALUES (?1, 'phantom', 'default-org', 'offline', datetime('now'))",
        params!["runner-offline-test"],
    )
    .unwrap();
    drop(conn);

    let (status, body) = d.get("/api/folders");
    assert_eq!(status, 503, "body: {body}");
    assert_eq!(
        body["error"].as_str(),
        Some("no_runner_connected"),
        "body: {body}"
    );
}
