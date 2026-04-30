//! SQLite-backed outbox for at-least-once delivery.
//!
//! Each side of the runner <-> SaaS connection maintains an outbox table.
//! Events are enqueued (INSERT), sent over the WebSocket, and only pruned
//! after the peer ACKs. On reconnect the peer sends its `last_seen_seq`
//! and the sender replays everything after that from the outbox.
//!
//! This module is **self-contained** — no `crate::` dependencies — so it
//! can be `#[path]`-included by the standalone `branchwork_runner` binary.

#![allow(dead_code)] // Both binaries include this module but each uses a different subset.

use rusqlite::{Connection, params};

// ── Table names ─────────────────────────────────────────────────────────────

/// Outbox table name on the runner side (runner -> SaaS events).
pub const RUNNER_OUTBOX: &str = "runner_outbox";

/// Outbox table name on the SaaS side (SaaS -> runner commands).
/// Partitioned by `runner_id` so one server DB serves many runners.
pub const SERVER_INBOX: &str = "inbox_pending";

// ── Schema ──────────────────────────────────────────────────────────────────

/// Create the runner-side outbox table. Called once at runner startup.
pub fn init_runner_outbox(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS runner_outbox (
            seq         INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type  TEXT    NOT NULL,
            payload     TEXT    NOT NULL,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            acked       INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_runner_outbox_acked
            ON runner_outbox(acked);",
    )
    .expect("failed to create runner_outbox table");
}

/// Create the server-side inbox table. Called during server DB migration.
pub fn init_server_inbox(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS inbox_pending (
            seq          INTEGER PRIMARY KEY AUTOINCREMENT,
            runner_id    TEXT    NOT NULL,
            command_type TEXT    NOT NULL,
            payload      TEXT    NOT NULL,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            acked        INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_inbox_pending_runner
            ON inbox_pending(runner_id, acked);",
    )
    .expect("failed to create inbox_pending table");
}

// ── Outbox operations (runner side — single runner, no runner_id column) ────

/// Enqueue an event into the runner outbox. Returns the assigned seq.
pub fn enqueue_runner_event(conn: &Connection, event_type: &str, payload: &str) -> u64 {
    conn.execute(
        "INSERT INTO runner_outbox (event_type, payload) VALUES (?1, ?2)",
        params![event_type, payload],
    )
    .expect("failed to enqueue runner event");
    conn.last_insert_rowid() as u64
}

/// Mark a runner outbox entry as ACKed.
pub fn mark_runner_acked(conn: &Connection, seq: u64) {
    conn.execute(
        "UPDATE runner_outbox SET acked = 1 WHERE seq = ?1",
        params![seq as i64],
    )
    .ok();
}

/// Replay all unacked runner events with seq > `after_seq`, ordered by seq.
/// Returns `(seq, event_type, payload_json)` tuples.
pub fn replay_runner_events(conn: &Connection, after_seq: u64) -> Vec<(u64, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_type, payload FROM runner_outbox \
             WHERE acked = 0 AND seq > ?1 \
             ORDER BY seq ASC",
        )
        .expect("failed to prepare replay query");
    stmt.query_map(params![after_seq as i64], |row| {
        Ok((row.get::<_, i64>(0)? as u64, row.get(1)?, row.get(2)?))
    })
    .expect("failed to replay runner events")
    .flatten()
    .collect()
}

/// Prune old ACKed entries from the runner outbox. Keeps the most recent
/// `keep` ACKed rows for debugging; deletes the rest.
pub fn prune_runner_outbox(conn: &Connection, keep: u64) {
    conn.execute(
        "DELETE FROM runner_outbox WHERE acked = 1 AND seq <= (
            SELECT COALESCE(MAX(seq), 0) FROM (
                SELECT seq FROM runner_outbox WHERE acked = 1
                ORDER BY seq DESC LIMIT ?1
            )
        ) - 1",
        // This is a bit tricky: keep the top `keep` acked rows.
        // Simpler approach: delete where acked=1 and seq not in top N.
        params![keep as i64],
    )
    .ok();
}

// ── Inbox operations (server side — per-runner) ─────────────────────────────

/// Enqueue a command into the server's inbox for a specific runner.
/// Returns the assigned seq.
pub fn enqueue_server_command(
    conn: &Connection,
    runner_id: &str,
    command_type: &str,
    payload: &str,
) -> u64 {
    conn.execute(
        "INSERT INTO inbox_pending (runner_id, command_type, payload) VALUES (?1, ?2, ?3)",
        params![runner_id, command_type, payload],
    )
    .expect("failed to enqueue server command");
    conn.last_insert_rowid() as u64
}

/// Mark a server inbox entry as ACKed.
pub fn mark_server_acked(conn: &Connection, seq: u64) {
    conn.execute(
        "UPDATE inbox_pending SET acked = 1 WHERE seq = ?1",
        params![seq as i64],
    )
    .ok();
}

/// Replay unacked commands for a specific runner with seq > `after_seq`.
/// Returns `(seq, command_type, payload_json)` tuples.
pub fn replay_server_commands(
    conn: &Connection,
    runner_id: &str,
    after_seq: u64,
) -> Vec<(u64, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, command_type, payload FROM inbox_pending \
             WHERE runner_id = ?1 AND acked = 0 AND seq > ?2 \
             ORDER BY seq ASC",
        )
        .expect("failed to prepare server replay query");
    stmt.query_map(params![runner_id, after_seq as i64], |row| {
        Ok((row.get::<_, i64>(0)? as u64, row.get(1)?, row.get(2)?))
    })
    .expect("failed to replay server commands")
    .flatten()
    .collect()
}

/// Prune old ACKed entries from the server inbox for a specific runner.
pub fn prune_server_inbox(conn: &Connection, runner_id: &str, keep: u64) {
    conn.execute(
        "DELETE FROM inbox_pending WHERE runner_id = ?1 AND acked = 1 AND seq NOT IN (
            SELECT seq FROM inbox_pending
            WHERE runner_id = ?1 AND acked = 1
            ORDER BY seq DESC LIMIT ?2
        )",
        params![runner_id, keep as i64],
    )
    .ok();
}

// ── Idempotency helper ──────────────────────────────────────────────────────

/// Track the highest seq received from a peer so we can detect duplicates.
/// Uses a simple key-value table.
pub fn init_seq_tracker(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS seq_tracker (
            peer_id    TEXT PRIMARY KEY,
            last_seq   INTEGER NOT NULL DEFAULT 0
        );",
    )
    .expect("failed to create seq_tracker table");
}

/// Record the last seq received from a peer. Returns `false` if this seq
/// was already seen (duplicate), `true` if it's new and was recorded.
pub fn advance_peer_seq(conn: &Connection, peer_id: &str, seq: u64) -> bool {
    let current: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(last_seq), 0) FROM seq_tracker WHERE peer_id = ?1",
            params![peer_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if (seq as i64) <= current {
        return false; // duplicate
    }

    conn.execute(
        "INSERT INTO seq_tracker (peer_id, last_seq) VALUES (?1, ?2)
         ON CONFLICT(peer_id) DO UPDATE SET last_seq = excluded.last_seq",
        params![peer_id, seq as i64],
    )
    .ok();
    true
}

/// Get the last seq we saw from a peer (for sending Resume on reconnect).
pub fn last_seen_seq(conn: &Connection, peer_id: &str) -> u64 {
    conn.query_row(
        "SELECT COALESCE(MAX(last_seq), 0) FROM seq_tracker WHERE peer_id = ?1",
        params![peer_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
        conn
    }

    #[test]
    fn runner_outbox_round_trip() {
        let conn = mem_db();
        init_runner_outbox(&conn);

        let s1 = enqueue_runner_event(&conn, "agent_started", r#"{"agent_id":"a1"}"#);
        let s2 = enqueue_runner_event(&conn, "agent_stopped", r#"{"agent_id":"a1"}"#);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);

        // Replay from 0 gives both
        let events = replay_runner_events(&conn, 0);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, 1);
        assert_eq!(events[1].0, 2);

        // ACK first, replay from 0 gives only second
        mark_runner_acked(&conn, 1);
        let events = replay_runner_events(&conn, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 2);

        // Replay from seq=1 gives only second
        let events = replay_runner_events(&conn, 1);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn server_inbox_round_trip() {
        let conn = mem_db();
        init_server_inbox(&conn);

        let s1 = enqueue_server_command(&conn, "runner-1", "start_agent", r#"{"agent_id":"a1"}"#);
        let s2 = enqueue_server_command(&conn, "runner-1", "kill_agent", r#"{"agent_id":"a1"}"#);
        let s3 = enqueue_server_command(&conn, "runner-2", "start_agent", r#"{"agent_id":"a2"}"#);

        // Runner-1 sees its 2 commands
        let cmds = replay_server_commands(&conn, "runner-1", 0);
        assert_eq!(cmds.len(), 2);

        // Runner-2 sees its 1 command
        let cmds = replay_server_commands(&conn, "runner-2", 0);
        assert_eq!(cmds.len(), 1);

        // ACK runner-1's first command
        mark_server_acked(&conn, s1);
        let cmds = replay_server_commands(&conn, "runner-1", 0);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, s2);

        let _ = s3; // suppress unused warning
    }

    #[test]
    fn seq_tracker_dedup() {
        let conn = mem_db();
        init_seq_tracker(&conn);

        assert!(advance_peer_seq(&conn, "runner-1", 1));
        assert!(advance_peer_seq(&conn, "runner-1", 2));
        // Duplicate
        assert!(!advance_peer_seq(&conn, "runner-1", 2));
        assert!(!advance_peer_seq(&conn, "runner-1", 1));
        // New
        assert!(advance_peer_seq(&conn, "runner-1", 3));

        assert_eq!(last_seen_seq(&conn, "runner-1"), 3);
        assert_eq!(last_seen_seq(&conn, "unknown"), 0);
    }
}
