//! Dispatch helpers for routing operations via runner vs local filesystem.
//!
//! In SaaS deployments the dashboard cannot touch the customer's filesystem
//! directly — it must round-trip through a registered runner. In standalone
//! deployments there are no runners and the dashboard owns the local fs.
//! [`org_has_runner`] is the boolean that selects between these modes.

use rusqlite::params;

use crate::db::Db;

/// Returns `true` iff at least one row exists in `runners` for `org_id`,
/// regardless of `status`. The presence of *any* runner row — online or
/// offline, freshly registered or long-departed — is the SaaS-mode signal:
/// once an org has registered a runner, every folder op routes through one.
/// Orgs with zero runner rows are treated as standalone deployments where
/// the local filesystem is authoritative.
#[allow(dead_code)] // consumers land in Phase 3.2+ of saas-folder-listing-via-runner
pub fn org_has_runner(db: &Db, org_id: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM runners WHERE org_id = ?1)",
        params![org_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n != 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use rusqlite::Connection;

    /// In-memory DB with the `runners` table only — minimal subset needed
    /// by `org_has_runner`. Schema mirrors db.rs:217.
    fn empty_runners_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, \
               name TEXT, \
               org_id TEXT, \
               status TEXT, \
               hostname TEXT, \
               version TEXT, \
               last_seen_at TEXT, \
               created_at TEXT \
             );",
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    fn seed_runner(db: &Db, runner_id: &str, org_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status) \
             VALUES (?1, 'test', ?2, ?3)",
            params![runner_id, org_id, status],
        )
        .unwrap();
    }

    #[test]
    fn two_orgs_one_with_runner_one_without() {
        let db = empty_runners_db();
        seed_runner(&db, "r1", "org-with", "online");
        // org-without has zero rows in runners. Seeding org-with first also
        // catches a missing WHERE clause: a query that returns true on any
        // non-empty table would flunk the second assertion.
        assert!(org_has_runner(&db, "org-with"));
        assert!(!org_has_runner(&db, "org-without"));
    }

    #[test]
    fn offline_runner_still_counts() {
        // The doc-comment promises "regardless of status" — ever-registered
        // is the SaaS-mode signal, not currently-online.
        let db = empty_runners_db();
        seed_runner(&db, "r1", "org-1", "offline");
        assert!(org_has_runner(&db, "org-1"));
    }

    #[test]
    fn empty_runners_table_returns_false() {
        let db = empty_runners_db();
        assert!(!org_has_runner(&db, "any-org"));
    }
}
