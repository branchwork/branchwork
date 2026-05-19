//! Regression tests for Task 1.1 of the
//! `runner-install-and-spawn-reliability` plan.
//!
//! When `Command::spawn` returns Err in the runner (typically because the
//! `branchwork-server` binary is missing from the runner's PATH), the
//! runner posts a structured `AgentSpawnFailed` envelope to the server.
//! The server stashes the rendered message on `agents.spawn_error` and the
//! dashboard reads it back via `GET /api/agents` to render an inline error
//! banner. Pre-fix the runner silently logged and the dashboard showed
//! `failed` with no actionable reason — the "Start session" button turned
//! grey and did nothing.
//!
//! These tests exercise the two halves the wire variant connects:
//!   1. The `agents.spawn_error` column survives a round-trip through the
//!      SQLite schema migration.
//!   2. `GET /api/agents` echoes the column back as `spawn_error` in the
//!      JSON response (camelCase parity: it's snake_case in the DB and on
//!      the wire — matches every other field on this endpoint).

mod support;

use support::TestDashboard;

#[test]
fn list_agents_echoes_spawn_error_column() {
    let d = TestDashboard::new();

    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    // Three rows in the default org: a successful local PTY (control,
    // spawn_error NULL), a runner-side spawn failure (the regression
    // target), and a different kind of failure that flows through
    // `stop_reason` instead of `spawn_error`. Pinning all three at once
    // makes sure a future code change that conflates the columns trips.
    conn.execute(
        "INSERT INTO agents (id, session_id, cwd, status, mode) \
         VALUES (?1, ?2, ?3, 'running', 'pty')",
        rusqlite::params!["agent-ok", "session-ok", d.project.to_string_lossy(),],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (id, cwd, status, mode, spawn_error) \
         VALUES (?1, ?2, 'failed', 'remote', ?3)",
        rusqlite::params![
            "agent-spawn-failed",
            d.project.to_string_lossy(),
            "runner could not spawn: /usr/local/bin/branchwork-server (ENOENT)",
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (id, cwd, status, mode, stop_reason) \
         VALUES (?1, ?2, 'failed', 'pty', ?3)",
        rusqlite::params![
            "agent-other-failed",
            d.project.to_string_lossy(),
            "supervisor spawn error: read timeout",
        ],
    )
    .unwrap();
    drop(conn);

    let (status, body) = d.get("/api/agents");
    assert_eq!(status, 200, "GET /api/agents body={body}");
    let rows = body.as_array().expect("response is an array");

    let by_id = |id: &str| {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("row {id} missing from /api/agents body: {body}"))
    };

    // Control: a healthy row has spawn_error=null.
    assert!(
        by_id("agent-ok")["spawn_error"].is_null(),
        "healthy agent should not carry spawn_error: {body}"
    );

    // Regression target: the structured spawn_error message round-trips
    // verbatim. The dashboard expects the same string the SaaS handler
    // rendered, so a future format change here is wire-visible.
    let row = by_id("agent-spawn-failed");
    assert_eq!(
        row["spawn_error"].as_str(),
        Some("runner could not spawn: /usr/local/bin/branchwork-server (ENOENT)"),
        "spawn_error must echo what the SaaS handler wrote: {body}"
    );
    assert_eq!(
        row["status"].as_str(),
        Some("failed"),
        "spawn-failed row should still flip to status=failed alongside spawn_error"
    );

    // A non-spawn failure (e.g. socket-never-appeared timeout) flows
    // through stop_reason only — spawn_error stays NULL so the UI
    // doesn't render a misleading "runner could not spawn" banner for
    // an actually-spawned-but-unreachable daemon.
    assert!(
        by_id("agent-other-failed")["spawn_error"].is_null(),
        "non-spawn failures should leave spawn_error NULL: {body}"
    );
}
