//! Regression tests for Task 5.5 of the
//! `runner-install-and-spawn-reliability` plan.
//!
//! Pre-fix `POST /api/agents/{id}/finish` looked up the "most recently
//! seen online runner" via `pick_runner_for_org` and shipped a
//! `WireMessage::AgentInput` envelope to that runner via
//! `command_tx.send`, then returned 200. Three failure modes silently
//! dropped graceful exit:
//!   - **Wrong runner targeted** if the org had multiple runners — the
//!     in-flight session daemon lives only on the runner that spawned
//!     it, so an `AgentInput` to a different runner is a no-op.
//!   - **Spawning runner offline** — the envelope landed on the wire
//!     but the daemon was unreachable (`AgentInput` is best-effort,
//!     doesn't replay through outbox).
//!   - **Version too old** — a runner whose `runner_protocol.rs`
//!     predates `AgentInput` (v0.3.0) has no handler and silently
//!     drops the envelope.
//!
//! Post-fix Finish targets the runner pinned on `agents.runner_id` (the
//! runner that received the `StartAgent` envelope at spawn time), and
//! refuses with 409 when that runner is unavailable or below the
//! minimum version that handles `AgentInput`. These tests pin all
//! three branches via HTTP + DB seeding (the agent never actually
//! spawns — `start_pty_agent`'s registry isn't involved on
//! mode='remote' rows).

mod support;

use support::TestDashboard;

fn seed_runner(d: &TestDashboard, id: &str, status: &str, version: Option<&str>) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO runners (id, name, org_id, status, last_seen_at, version) \
         VALUES (?1, ?2, 'default-org', ?3, datetime('now'), ?4)",
        rusqlite::params![id, format!("runner-{id}"), status, version],
    )
    .unwrap();
}

fn seed_remote_agent(d: &TestDashboard, agent_id: &str, runner_id: Option<&str>) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents (id, session_id, cwd, status, mode, driver, org_id, runner_id) \
         VALUES (?1, ?2, ?3, 'running', 'remote', 'claude', 'default-org', ?4)",
        rusqlite::params![
            agent_id,
            format!("session-{agent_id}"),
            d.project.to_string_lossy(),
            runner_id,
        ],
    )
    .unwrap();
}

/// Acceptance path (b): a runner stuck on v0.3.0 returns a structured
/// 409 with `code=spawn_runner_too_old`, the actual version, and the
/// minimum required version — no silent 200, the operator can see
/// exactly why graceful exit refused.
#[test]
fn finish_refuses_with_409_when_spawning_runner_is_too_old() {
    let d = TestDashboard::new();
    seed_runner(&d, "runner-v030", "online", Some("0.3.0"));
    seed_remote_agent(&d, "agent-001", Some("runner-v030"));

    let (status, body) = d.post("/api/agents/agent-001/finish", serde_json::Value::Null);
    assert_eq!(status, 409, "expected 409, got {status} body={body}");
    assert_eq!(body["code"].as_str(), Some("spawn_runner_too_old"));
    assert_eq!(body["runnerId"].as_str(), Some("runner-v030"));
    assert_eq!(body["runnerVersion"].as_str(), Some("0.3.0"));
    assert_eq!(
        body["minimumVersion"].as_str(),
        Some("0.4.0"),
        "minimum version is the brief's `>= T5.9 release`",
    );
    // The message must surface the actual + minimum so the dashboard
    // can render it without doing semver math itself.
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(msg.contains("0.3.0"), "message must name actual: {msg}");
    assert!(msg.contains("0.4.0"), "message must name minimum: {msg}");
}

/// Spawning runner row exists but its status is `offline` ⇒ 409
/// `spawn_runner_unavailable`. Pre-fix this would silently 200 after
/// dropping the envelope into the outbox (which `AgentInput` doesn't
/// replay from).
#[test]
fn finish_refuses_with_409_when_spawning_runner_is_offline() {
    let d = TestDashboard::new();
    seed_runner(&d, "runner-offline", "offline", Some("0.5.70"));
    seed_remote_agent(&d, "agent-002", Some("runner-offline"));

    let (status, body) = d.post("/api/agents/agent-002/finish", serde_json::Value::Null);
    assert_eq!(status, 409, "expected 409, got {status} body={body}");
    assert_eq!(body["code"].as_str(), Some("spawn_runner_unavailable"));
    assert_eq!(body["runnerId"].as_str(), Some("runner-offline"));
    assert_eq!(body["runnerStatus"].as_str(), Some("offline"));
}

/// Spawning runner row was deleted (orphan agent) ⇒ 409
/// `spawn_runner_unavailable`. Without this, Finish would inherit
/// the pre-fix pick_runner_for_org behaviour and dispatch to a
/// completely unrelated runner.
#[test]
fn finish_refuses_with_409_when_spawning_runner_was_removed() {
    let d = TestDashboard::new();
    // No `runners` row inserted — orphan agent.
    seed_remote_agent(&d, "agent-003", Some("runner-deleted"));

    let (status, body) = d.post("/api/agents/agent-003/finish", serde_json::Value::Null);
    assert_eq!(status, 409, "expected 409, got {status} body={body}");
    assert_eq!(body["code"].as_str(), Some("spawn_runner_unavailable"));
    assert_eq!(body["runnerId"].as_str(), Some("runner-deleted"));
}

/// A separate online runner exists in the same org. Pre-fix Finish
/// would dispatch to it and silently 200. Post-fix it must STILL 409
/// — the spawning runner is the only valid target.
#[test]
fn finish_does_not_fall_through_to_a_different_runner() {
    let d = TestDashboard::new();
    // The runner that spawned the agent is offline.
    seed_runner(&d, "runner-spawn", "offline", Some("0.5.70"));
    // An UNRELATED runner is online (the pre-fix bug magnet).
    seed_runner(&d, "runner-other", "online", Some("0.5.70"));
    seed_remote_agent(&d, "agent-004", Some("runner-spawn"));

    let (status, body) = d.post("/api/agents/agent-004/finish", serde_json::Value::Null);
    assert_eq!(
        status, 409,
        "Finish must target the spawning runner, not any-online: {body}",
    );
    assert_eq!(body["code"].as_str(), Some("spawn_runner_unavailable"));
    assert_eq!(
        body["runnerId"].as_str(),
        Some("runner-spawn"),
        "the spawning runner is the body's runnerId, not the random online one",
    );
}

/// Unparseable runner version is treated as too old — better to 409
/// loudly than silently dispatch to a runner that may or may not
/// support `AgentInput`.
#[test]
fn finish_refuses_with_409_when_runner_version_is_unparseable() {
    let d = TestDashboard::new();
    seed_runner(&d, "runner-custom", "online", Some("custom-build"));
    seed_remote_agent(&d, "agent-005", Some("runner-custom"));

    let (status, body) = d.post("/api/agents/agent-005/finish", serde_json::Value::Null);
    assert_eq!(status, 409, "got {status} body={body}");
    assert_eq!(body["code"].as_str(), Some("spawn_runner_too_old"));
}

/// Agent that doesn't exist ⇒ 404 (no change vs pre-fix — keeps the
/// existing surface for the dashboard's stale-row-click handler).
#[test]
fn finish_returns_404_for_unknown_agent() {
    let d = TestDashboard::new();
    let (status, body) = d.post("/api/agents/no-such-agent/finish", serde_json::Value::Null);
    assert_eq!(status, 404, "expected 404, got {status} body={body}");
    assert_eq!(body["error"].as_str(), Some("Agent not found"));
}

/// Acceptance path (a): a fresh runner whose version is >= minimum
/// AND is online passes the gate. The HTTP layer can't actually
/// dispatch the envelope (no live WS in these integration tests
/// without a real runner), so we settle for the next-best signal:
/// the 409 paths above all fire BEFORE the runners-registry lookup,
/// so a green response from those gates means the response status
/// is NOT 409 spawn_runner_* — it's either 200 (would-be happy path
/// when a WS exists) or 409 spawn_runner_unavailable from the
/// registry lookup (no WS for the runner in these tests because
/// it's seeded into the DB but not connected).
///
/// The DB-vs-registry split is real: `runner.status='online'` in the
/// DB can lag behind a disconnected WS, so the final registry lookup
/// in `finish_agent` is the authoritative live check. This test
/// asserts that gate fires AFTER the DB-side checks (version, status),
/// so the operator sees the more specific error rather than a generic
/// "no_runner_connected".
#[test]
fn finish_passes_db_gates_when_runner_meets_minimum_version_and_is_online() {
    let d = TestDashboard::new();
    seed_runner(&d, "runner-fresh", "online", Some("0.5.70"));
    seed_remote_agent(&d, "agent-006", Some("runner-fresh"));

    let (status, body) = d.post("/api/agents/agent-006/finish", serde_json::Value::Null);

    // The DB gates pass (status=online, version=0.5.70 >= 0.4.0); the
    // final registry-lookup fails (no live WS for runner-fresh in the
    // integration harness) and returns 409 spawn_runner_unavailable
    // — but crucially NOT spawn_runner_too_old, which would mean the
    // version gate misfired.
    assert_eq!(status, 409, "got {status} body={body}");
    assert_eq!(
        body["code"].as_str(),
        Some("spawn_runner_unavailable"),
        "version gate should NOT fire — runner reports 0.5.70 which is >= 0.4.0 minimum",
    );
}

/// Legacy agent row (predates the `runner_id` column) ⇒ Finish must
/// not 5xx. It falls back to `pick_runner_for_org` with the same
/// pre-fix semantics — chosen as a minimal-disruption migration so
/// the v0.3.0 production agents from the 2026-05-19 incident don't
/// become un-finish-able after this fix.
#[test]
fn finish_falls_back_to_pick_runner_for_legacy_rows_with_null_runner_id() {
    let d = TestDashboard::new();
    seed_runner(&d, "runner-legacy", "online", Some("0.5.70"));
    seed_remote_agent(&d, "agent-007", None); // NULL runner_id ⇒ legacy

    let (status, body) = d.post("/api/agents/agent-007/finish", serde_json::Value::Null);
    // Same final outcome as the "fresh" case — DB gates pass, the
    // registry lookup fails for lack of a live WS, so the body is
    // 409 spawn_runner_unavailable rather than too_old.
    assert_eq!(status, 409, "got {status} body={body}");
    assert_eq!(body["code"].as_str(), Some("spawn_runner_unavailable"));
    assert_eq!(
        body["runnerId"].as_str(),
        Some("runner-legacy"),
        "legacy fallback resolves to pick_runner_for_org",
    );
}
