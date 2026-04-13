use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// Thread-safe handle to the SQLite database.
pub type Db = Arc<Mutex<Connection>>;

/// Open (or create) the database at `db_path` and run migrations.
pub fn init(db_path: &Path) -> Db {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create db directory");
    }

    let conn = Connection::open(db_path)
        .unwrap_or_else(|e| panic!("failed to open database at {}: {e}", db_path.display()));

    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .expect("failed to set journal_mode");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("failed to enable foreign keys");

    migrate(&conn);

    Arc::new(Mutex::new(conn))
}

fn migrate(conn: &Connection) {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS hook_events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT    NOT NULL,
            hook_type   TEXT    NOT NULL,
            tool_name   TEXT,
            tool_input  TEXT,
            timestamp   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_hook_session ON hook_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_type    ON hook_events(hook_type);

        CREATE TABLE IF NOT EXISTS agents (
            id               TEXT PRIMARY KEY,
            session_id       TEXT,
            pid              INTEGER,
            parent_agent_id  TEXT,
            plan_name        TEXT,
            task_id          TEXT,
            cwd              TEXT NOT NULL,
            status           TEXT NOT NULL DEFAULT 'starting',
            mode             TEXT NOT NULL DEFAULT 'pty',
            prompt           TEXT,
            started_at       TEXT NOT NULL DEFAULT (datetime('now')),
            finished_at      TEXT,
            last_tool        TEXT,
            last_activity_at TEXT,
            base_commit      TEXT,
            branch           TEXT,
            source_branch    TEXT,
            FOREIGN KEY (parent_agent_id) REFERENCES agents(id)
        );
        CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);

        CREATE TABLE IF NOT EXISTS agent_output (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT NOT NULL,
            message_type TEXT NOT NULL,
            content      TEXT NOT NULL,
            timestamp    TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (agent_id) REFERENCES agents(id)
        );
        CREATE INDEX IF NOT EXISTS idx_output_agent ON agent_output(agent_id);

        CREATE TABLE IF NOT EXISTS plan_project (
            plan_name  TEXT PRIMARY KEY,
            project    TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS task_status (
            plan_name   TEXT NOT NULL,
            task_number TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'pending',
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (plan_name, task_number)
        );
        ",
    )
    .expect("failed to run schema migration");

    // Add columns for existing databases
    conn.execute_batch("ALTER TABLE agents ADD COLUMN base_commit TEXT;")
        .ok(); // ignore error if column already exists
    conn.execute_batch("ALTER TABLE agents ADD COLUMN branch TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE agents ADD COLUMN source_branch TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE agents ADD COLUMN cost_usd REAL;")
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    fn test_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = init(&path);
        (db, dir)
    }

    #[test]
    fn creates_all_tables() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(tables.contains(&"hook_events".to_string()));
        assert!(tables.contains(&"agents".to_string()));
        assert!(tables.contains(&"agent_output".to_string()));
        assert!(tables.contains(&"plan_project".to_string()));
        assert!(tables.contains(&"task_status".to_string()));
    }

    #[test]
    fn idempotent_migration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        // Run init twice — should not panic
        let _db1 = init(&path);
        let _db2 = init(&path);
    }

    #[test]
    fn insert_and_query_hook_event() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO hook_events (session_id, hook_type, tool_name) VALUES (?1, ?2, ?3)",
            params!["sess-1", "PostToolUse", "Bash"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM hook_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_and_query_task_status() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, ?3)",
            params!["my-plan", "1.1", "completed"],
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
                params!["my-plan", "1.1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn insert_agent_with_parent() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO agents (id, cwd, status) VALUES (?1, ?2, ?3)",
            params!["agent-1", "/tmp", "running"],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO agents (id, cwd, status, parent_agent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["agent-2", "/tmp", "running", "agent-1"],
        )
        .unwrap();

        let parent: String = conn
            .query_row(
                "SELECT parent_agent_id FROM agents WHERE id = ?1",
                params!["agent-2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, "agent-1");
    }

    #[test]
    fn wal_mode_enabled() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn db_path_created_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c").join("test.db");
        let _db = init(&nested);
        assert!(nested.exists());
    }
}
