use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use rusqlite::{OptionalExtension, params};

use crate::db::Db;

/// Sliding expiry window. Each request with a valid token pushes `expires_at`
/// forward by this much. Long enough that a desktop user logged in on Monday
/// morning is still logged in Tuesday morning without a round-trip login.
pub const SESSION_TTL: Duration = Duration::days(7);

/// Cookie name used on the HTTP boundary.
pub const COOKIE_NAME: &str = "branchwork_session";

/// Server-side session record. The raw `token` is the opaque cookie value; we
/// store it hashed-equivalent only because SQLite lookups on the primary key
/// are already constant-time at the DB layer and sessions are short-lived.
#[derive(Debug, Clone)]
#[allow(dead_code)] // token/expires_at are load-bearing in tests and future
// refresh-response code, but aren't read by the current request hot-path.
pub struct Session {
    pub token: String,
    pub user_id: String,
    pub expires_at: DateTime<Utc>,
}

/// Generate a fresh 256-bit URL-safe token.
fn new_token() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    // URL-safe hex — no padding, no special chars. 64 chars is fine as a
    // cookie value and in logs/DB.
    let mut s = String::with_capacity(64);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Create a new session for `user_id`. Returns the raw token to set in the
/// cookie.
pub fn create(db: &Db, user_id: &str) -> String {
    let token = new_token();
    let expires = Utc::now() + SESSION_TTL;
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
        params![token, user_id, expires.to_rfc3339()],
    )
    .expect("failed to insert session");
    token
}

/// Look up a session by its raw token. Returns None if the token is unknown
/// or already past its `expires_at`. On a hit, slides the expiry forward —
/// this is what keeps logged-in users logged in.
pub fn lookup_and_slide(db: &Db, token: &str) -> Option<Session> {
    let now = Utc::now();
    let conn = db.lock().unwrap();
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT token, user_id, expires_at FROM sessions WHERE token = ?1",
            params![token],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (token, user_id, expires_at) = row?;
    let expires: DateTime<Utc> = DateTime::parse_from_rfc3339(&expires_at)
        .ok()?
        .with_timezone(&Utc);
    if expires <= now {
        // Stale — clean up while we're here.
        conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])
            .ok();
        return None;
    }
    let new_expires = now + SESSION_TTL;
    conn.execute(
        "UPDATE sessions SET expires_at = ?1 WHERE token = ?2",
        params![new_expires.to_rfc3339(), token],
    )
    .ok();
    Some(Session {
        token,
        user_id,
        expires_at: new_expires,
    })
}

/// Invalidate a single session. Idempotent — missing tokens are silently OK.
pub fn delete(db: &Db, token: &str) {
    let conn = db.lock().unwrap();
    conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])
        .ok();
}

/// Extract the session token from a request's `Cookie` header, if any.
pub fn token_from_cookie_header(header: &str) -> Option<String> {
    for part in header.split(';') {
        let kv = part.trim();
        if let Some(rest) = kv.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Render the `Set-Cookie` value for a newly issued token. HttpOnly + SameSite=Lax
/// so the cookie survives a browser refresh but is not exposed to JS and isn't
/// sent on cross-site POSTs. Not marked Secure — Branchwork is typically run on
/// `http://localhost` and a Secure cookie would never stick.
pub fn set_cookie_value(token: &str) -> String {
    let max_age = SESSION_TTL.num_seconds();
    format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}")
}

/// Render a `Set-Cookie` value that clears the cookie — for logout.
pub fn clear_cookie_value() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::TempDir;

    fn fresh() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = db::init(&path);
        // Create a user we can reference.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'a@b.com', 'x')",
                [],
            )
            .unwrap();
        }
        (db, dir)
    }

    #[test]
    fn create_and_lookup_roundtrip() {
        let (db, _dir) = fresh();
        let token = create(&db, "u1");
        let s = lookup_and_slide(&db, &token).expect("session should exist");
        assert_eq!(s.user_id, "u1");
    }

    #[test]
    fn expired_session_is_rejected_and_cleaned_up() {
        let (db, _dir) = fresh();
        // Manually insert a session with an expired timestamp.
        let token = "deadbeef".to_string();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, 'u1', ?2)",
                params![token, (Utc::now() - Duration::seconds(1)).to_rfc3339()],
            )
            .unwrap();
        }
        assert!(lookup_and_slide(&db, &token).is_none());
        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token = ?1",
                params![token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "expired session should be deleted on miss");
    }

    #[test]
    fn delete_is_idempotent() {
        let (db, _dir) = fresh();
        delete(&db, "never-existed");
        let token = create(&db, "u1");
        delete(&db, &token);
        assert!(lookup_and_slide(&db, &token).is_none());
    }

    #[test]
    fn parses_cookie_from_header() {
        let t = token_from_cookie_header(&format!("other=x; {COOKIE_NAME}=abc123; extra=y"));
        assert_eq!(t.as_deref(), Some("abc123"));
        assert!(token_from_cookie_header("other=x").is_none());
    }
}
