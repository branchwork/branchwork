use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

/// Thread-safe handle to the SQLite database.
pub type Db = Arc<Mutex<Connection>>;

/// Return the set of task numbers whose `task_status` is `completed` or
/// `skipped` for the given plan. Used to evaluate task dependency gates.
pub fn completed_task_numbers(conn: &Connection, plan_name: &str) -> HashSet<String> {
    conn.prepare(
        "SELECT task_number FROM task_status \
         WHERE plan_name = ?1 AND status IN ('completed', 'skipped')",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()
    })
    .unwrap_or_default()
}

/// Load the recorded learnings for a single task. Most-recent first.
pub fn task_learnings(conn: &Connection, plan_name: &str, task_number: &str) -> Vec<String> {
    conn.prepare(
        "SELECT learning FROM task_learnings \
         WHERE plan_name = ?1 AND task_number = ?2 \
         ORDER BY id DESC",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![plan_name, task_number], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

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
            id                TEXT PRIMARY KEY,
            session_id        TEXT,
            pid               INTEGER,
            parent_agent_id   TEXT,
            plan_name         TEXT,
            task_id           TEXT,
            cwd               TEXT NOT NULL,
            status            TEXT NOT NULL DEFAULT 'starting',
            mode              TEXT NOT NULL DEFAULT 'pty',
            prompt            TEXT,
            started_at        TEXT NOT NULL DEFAULT (datetime('now')),
            finished_at       TEXT,
            last_tool         TEXT,
            last_activity_at  TEXT,
            base_commit       TEXT,
            branch            TEXT,
            source_branch     TEXT,
            supervisor_socket TEXT,
            driver            TEXT DEFAULT 'claude',
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

        CREATE TABLE IF NOT EXISTS plan_budget (
            plan_name      TEXT PRIMARY KEY,
            max_budget_usd REAL NOT NULL,
            updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS plan_auto_advance (
            plan_name  TEXT PRIMARY KEY,
            enabled    INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS task_learnings (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_name    TEXT    NOT NULL,
            task_number  TEXT    NOT NULL,
            learning     TEXT    NOT NULL,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_learnings_plan_task ON task_learnings(plan_name, task_number);

        CREATE TABLE IF NOT EXISTS users (
            id             TEXT PRIMARY KEY,
            email          TEXT NOT NULL UNIQUE,
            password_hash  TEXT NOT NULL,
            created_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

        CREATE TABLE IF NOT EXISTS sessions (
            token       TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            expires_at  TEXT NOT NULL,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);

        CREATE TABLE IF NOT EXISTS ci_runs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_name    TEXT    NOT NULL,
            task_number  TEXT    NOT NULL,
            agent_id     TEXT,
            provider     TEXT    NOT NULL DEFAULT 'github',
            commit_sha   TEXT,
            branch       TEXT,
            run_id       TEXT,
            run_url      TEXT,
            status       TEXT    NOT NULL DEFAULT 'pending',
            conclusion   TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_ci_runs_plan_task ON ci_runs(plan_name, task_number);
        CREATE INDEX IF NOT EXISTS idx_ci_runs_status ON ci_runs(status);
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
    // Path to the session-daemon's local socket / named pipe. NULL for legacy
    // rows written before the tmux → supervisor switch; those are treated as
    // `detached` on first boot post-upgrade.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN supervisor_socket TEXT;")
        .ok();
    // Name of the AgentDriver that spawned this agent (e.g. "claude").
    // NULL on rows written before driver selection existed; readers treat
    // NULL as the default driver.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN driver TEXT DEFAULT 'claude';")
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(tables.contains(&"task_learnings".to_string()));
        assert!(tables.contains(&"users".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
    }

    #[test]
    fn task_learnings_round_trip() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.1", "first learning"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.1", "second learning"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.2", "other task learning"],
        )
        .unwrap();

        let ls = task_learnings(&conn, "plan-a", "1.1");
        // Most-recent first.
        assert_eq!(ls, vec!["second learning", "first learning"]);

        assert_eq!(
            task_learnings(&conn, "plan-a", "1.2"),
            vec!["other task learning"]
        );
        assert!(task_learnings(&conn, "plan-a", "9.9").is_empty());
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
    fn completed_task_numbers_gate() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('p1', '1.1', 'completed'),
               ('p1', '1.2', 'skipped'),
               ('p1', '1.3', 'in_progress'),
               ('p1', '1.4', 'pending'),
               ('p2', '1.1', 'completed');",
        )
        .unwrap();

        let done = completed_task_numbers(&conn, "p1");
        assert!(done.contains("1.1"));
        assert!(done.contains("1.2"));
        assert!(!done.contains("1.3"));
        assert!(!done.contains("1.4"));
        assert_eq!(done.len(), 2);

        let empty = completed_task_numbers(&conn, "nonexistent");
        assert!(empty.is_empty());
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
    fn agents_table_has_driver_column() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, driver) VALUES (?1, ?2, ?3, ?4)",
            params!["a1", "/tmp", "running", "claude"],
        )
        .unwrap();
        let drv: Option<String> = conn
            .query_row(
                "SELECT driver FROM agents WHERE id = ?1",
                params!["a1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(drv.as_deref(), Some("claude"));

        // Default when not specified
        conn.execute(
            "INSERT INTO agents (id, cwd, status) VALUES (?1, ?2, ?3)",
            params!["a2", "/tmp", "running"],
        )
        .unwrap();
        let drv2: Option<String> = conn
            .query_row(
                "SELECT driver FROM agents WHERE id = ?1",
                params!["a2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(drv2.as_deref(), Some("claude"));
    }

    #[test]
    fn agents_table_has_supervisor_socket_column() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, supervisor_socket) VALUES (?1, ?2, ?3, ?4)",
            params!["a1", "/tmp", "running", "/tmp/a1.sock"],
        )
        .unwrap();
        let sock: Option<String> = conn
            .query_row(
                "SELECT supervisor_socket FROM agents WHERE id = ?1",
                params!["a1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sock.as_deref(), Some("/tmp/a1.sock"));
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
