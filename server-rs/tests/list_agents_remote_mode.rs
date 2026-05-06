//! Regression test for `GET /api/agents` dropping rows whose
//! `session_id` is NULL.
//!
//! Bug discovered live on 2026-05-06 against the SaaS deployment:
//! a runner-spawned agent existed in the DB with `mode='remote'` and
//! `session_id IS NULL` (the runner never echoed one back over the
//! wire), but the dashboard rendered an empty list. Root cause was
//! `api/agents.rs:107` decoding `session_id` as a non-optional
//! `String`; rusqlite's `query_map` returned `Err` for the row and
//! the surrounding `.flatten()` silently dropped it.
//!
//! This test inserts a sentinel agent row directly into the SQLite
//! db with `session_id = NULL`, calls `/api/agents`, and asserts the
//! row is present in the response (with `session_id: null`). Before
//! the fix this assertion failed because the row never made it into
//! the JSON list.

mod support;

use support::TestDashboard;

#[test]
fn list_agents_includes_rows_with_null_session_id() {
    let d = TestDashboard::new();

    // Two rows, both in the default org so list_agents returns them
    // without an auth header. Row A is a "local PTY" shape (session_id
    // populated) — the control. Row B is a "remote-mode" shape
    // (session_id NULL) — the regression target.
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents (id, session_id, cwd, status, mode) \
         VALUES (?1, ?2, ?3, 'running', 'pty')",
        rusqlite::params![
            "agent-with-session",
            "session-abc",
            d.project.to_string_lossy(),
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (id, session_id, cwd, status, mode) \
         VALUES (?1, NULL, ?2, 'running', 'remote')",
        rusqlite::params!["agent-without-session", d.project.to_string_lossy(),],
    )
    .unwrap();
    drop(conn);

    let (status, body) = d.get("/api/agents");
    assert_eq!(status, 200, "GET /api/agents body={body}");

    let rows = body.as_array().expect("response is an array");
    let with: Vec<_> = rows
        .iter()
        .filter(|r| r["id"] == "agent-with-session")
        .collect();
    let without: Vec<_> = rows
        .iter()
        .filter(|r| r["id"] == "agent-without-session")
        .collect();

    assert_eq!(
        with.len(),
        1,
        "control row (session_id populated) missing from response: {body}"
    );
    assert_eq!(
        without.len(),
        1,
        "regression: row with session_id=NULL was dropped by query_map; got {body}"
    );

    // session_id should round-trip as JSON null on the remote-mode row.
    assert!(
        without[0]["session_id"].is_null(),
        "remote-mode row should serialise session_id as null, got {:?}",
        without[0]["session_id"]
    );
    // And as a string on the local-PTY row.
    assert_eq!(
        with[0]["session_id"].as_str(),
        Some("session-abc"),
        "local-PTY row should serialise session_id as the inserted string"
    );
}
