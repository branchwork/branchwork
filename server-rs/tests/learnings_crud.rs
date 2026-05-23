//! End-to-end tests for the org-shared learnings API (Phase 2.2 of
//! `learning-hub-ci-failure-capture`).
//!
//! Acceptance criterion from the task brief: "curl-able CRUD-like
//! surface; archived entries do not show in the default list view but
//! appear with `?include=archived`." These tests spin up the real
//! `branchwork-server` binary against a scratch tempdir (no network,
//! no auth required — endpoints use `OptionalAuthUser` and fall back
//! to `default-org`), seed rows via direct SQLite writes (mirrors
//! `tests/pending_learnings.rs`), and exercise list / get / archive
//! via the production HTTP surface.

mod support;

use rusqlite::Connection;
use serde_json::json;
use support::TestDashboard;

/// Seed a single active learning row. Returns the id.
fn seed_learning(
    d: &TestDashboard,
    id: &str,
    kind: &str,
    category: &str,
    slug: &str,
    org_id: &str,
) {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO learnings \
            (id, org_id, kind, category, slug, body_md) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            org_id,
            kind,
            category,
            slug,
            format!("**Why:** seeded for tests\n\n**How to apply:** for id={id}."),
        ],
    )
    .unwrap();
}

/// Flip a row to archived in-place (bypassing the API) so we can verify
/// the list endpoint hides it by default.
fn force_archive(d: &TestDashboard, id: &str, reason: &str) {
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE learnings SET archived_at = datetime('now'), archived_reason = ?2 \
         WHERE id = ?1",
        rusqlite::params![id, reason],
    )
    .unwrap();
}

#[test]
fn list_get_archive_round_trip_via_http() {
    let d = TestDashboard::new();

    // Seed three rows: two active (different kinds), one already
    // archived (we'll verify it doesn't appear in the default list).
    seed_learning(&d, "L-1", "feedback", "testing", "no-mocks", "default-org");
    seed_learning(
        &d,
        "L-2",
        "project",
        "deploy",
        "blue-green-policy",
        "default-org",
    );
    seed_learning(&d, "L-3", "feedback", "testing", "old", "default-org");
    force_archive(&d, "L-3", "outdated-by-adr");

    // ── GET /api/learnings ───────────────────────────────────────────
    let (status, body) = d.get("/api/learnings");
    assert_eq!(status, 200, "list returns 200: {body}");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "default list hides archived: {body}");
    let ids: Vec<&str> = items.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"L-1"));
    assert!(ids.contains(&"L-2"));
    assert!(!ids.contains(&"L-3"), "L-3 is archived; must be hidden");

    // ── GET /api/learnings?include=archived ─────────────────────────
    let (status, body) = d.get("/api/learnings?include=archived");
    assert_eq!(status, 200);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "include=archived surfaces tombstones");
    let archived_row = items
        .iter()
        .find(|l| l["id"] == "L-3")
        .expect("L-3 visible with include=archived");
    assert!(!archived_row["archivedAt"].is_null());
    assert_eq!(archived_row["archivedReason"], "outdated-by-adr");

    // ── GET /api/learnings?kind=feedback ────────────────────────────
    let (_s, body) = d.get("/api/learnings?kind=feedback");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["L-1"], "kind=feedback (active only)");

    // ── GET /api/learnings?category=deploy ──────────────────────────
    let (_s, body) = d.get("/api/learnings?category=deploy");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["L-2"]);

    // ── GET /api/learnings/{id} ─────────────────────────────────────
    let (status, body) = d.get("/api/learnings/L-1");
    assert_eq!(status, 200);
    assert_eq!(body["id"], "L-1");
    assert_eq!(body["kind"], "feedback");
    assert_eq!(body["slug"], "no-mocks");
    assert!(body["bodyMd"].as_str().unwrap().contains("Why"));

    // ── GET /api/learnings/{id} on unknown id ───────────────────────
    let (status, body) = d.get("/api/learnings/does-not-exist");
    assert_eq!(status, 404);
    assert_eq!(body["error"], "learning_not_found");

    // ── POST /api/learnings/{id}/archive (happy path) ────────────────
    let (status, body) = d.post(
        "/api/learnings/L-1/archive",
        json!({ "reason": "superseded by ADR 0009" }),
    );
    assert_eq!(status, 200, "archive returns 200: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["id"], "L-1");
    assert!(!body["archivedAt"].is_null());

    // GET on the archived row still works (read returns tombstones).
    let (status, body) = d.get("/api/learnings/L-1");
    assert_eq!(status, 200);
    assert_eq!(body["archivedReason"], "superseded by ADR 0009");
    assert!(!body["archivedAt"].is_null());

    // It's now hidden from the default list.
    let (_s, body) = d.get("/api/learnings");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["L-2"], "L-1 archived → only L-2 active");

    // ── POST .../archive on already-archived → 410 Gone ────────────
    let (status, body) = d.post(
        "/api/learnings/L-1/archive",
        json!({ "reason": "second attempt" }),
    );
    assert_eq!(status, 410, "re-archive returns 410: {body}");
    assert_eq!(body["error"], "already_archived");
    // The original reason wins; second attempt does NOT overwrite.
    assert_eq!(body["archivedReason"], "superseded by ADR 0009");
}

#[test]
fn archive_requires_reason() {
    let d = TestDashboard::new();
    seed_learning(&d, "L-1", "feedback", "x", "a", "default-org");

    // No body at all → axum's JSON extractor rejects with 415/422 long
    // before our handler runs. Posting an empty JSON object exercises
    // OUR reason_required gate.
    let (status, body) = d.post("/api/learnings/L-1/archive", json!({}));
    assert_eq!(status, 422, "missing reason → 422: {body}");
    assert_eq!(body["error"], "reason_required");

    let (status, body) = d.post("/api/learnings/L-1/archive", json!({ "reason": "" }));
    assert_eq!(status, 422, "empty reason → 422: {body}");
    assert_eq!(body["error"], "reason_required");

    let (status, body) = d.post(
        "/api/learnings/L-1/archive",
        json!({ "reason": "   \t\n  " }),
    );
    assert_eq!(status, 422, "whitespace-only reason → 422: {body}");
    assert_eq!(body["error"], "reason_required");

    // Row stays active.
    let (status, body) = d.get("/api/learnings/L-1");
    assert_eq!(status, 200);
    assert!(body["archivedAt"].is_null(), "no reason → no flip");
}

#[test]
fn archive_404s_on_unknown_id() {
    let d = TestDashboard::new();
    let (status, body) = d.post(
        "/api/learnings/no-such-id/archive",
        json!({ "reason": "ok" }),
    );
    assert_eq!(status, 404, "unknown id → 404: {body}");
    assert_eq!(body["error"], "learning_not_found");
}

#[test]
fn list_get_archive_are_org_scoped() {
    // Seed rows in two orgs. The anonymous caller resolves to
    // default-org and must not see other-org rows.
    let d = TestDashboard::new();
    seed_learning(&d, "L-default", "feedback", "x", "a", "default-org");

    // Other-org seed must go via direct INSERT — the API never lets
    // an unauthenticated caller write to a non-default org.
    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = Connection::open(&db_path).unwrap();
    // Make sure the FK target exists. organizations.id has a `default-org`
    // row from `ensure_default_org` already; add a second.
    conn.execute(
        "INSERT INTO organizations (id, name, slug) VALUES ('other-org', 'Other', 'other')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO learnings (id, org_id, kind, category, slug, body_md) \
         VALUES ('L-other', 'other-org', 'feedback', 'x', 'a', 'body')",
        [],
    )
    .unwrap();

    // List: only default-org row.
    let (_s, body) = d.get("/api/learnings");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["L-default"],
        "anonymous caller → default-org only"
    );

    // Get cross-org → 404 (no leak about whether the id exists).
    let (status, body) = d.get("/api/learnings/L-other");
    assert_eq!(status, 404);
    assert_eq!(body["error"], "learning_not_found");

    // Archive cross-org → 404 + the row stays active.
    let (status, _b) = d.post(
        "/api/learnings/L-other/archive",
        json!({ "reason": "wrong-org" }),
    );
    assert_eq!(status, 404);
    let (archived_at, archived_reason): (Option<String>, Option<String>) = {
        let conn = Connection::open(&db_path).unwrap();
        conn.query_row(
            "SELECT archived_at, archived_reason FROM learnings WHERE id = 'L-other'",
            [],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap()
    };
    assert!(archived_at.is_none(), "cross-org archive must not flip");
    assert!(archived_reason.is_none());
}

#[test]
fn archive_writes_audit_row() {
    let d = TestDashboard::new();
    seed_learning(&d, "L-1", "feedback", "testing", "no-mocks", "default-org");

    let (status, _b) = d.post(
        "/api/learnings/L-1/archive",
        json!({ "reason": "audit-trail-test" }),
    );
    assert_eq!(status, 200);

    let db_path = d.dir.path().join(".claude/branchwork.db");
    let conn = Connection::open(&db_path).unwrap();
    let (action, resource_type, resource_id, diff): (
        String,
        String,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT action, resource_type, resource_id, diff \
               FROM audit_logs WHERE resource_id = ?1 \
              ORDER BY id DESC LIMIT 1",
            rusqlite::params!["L-1"],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(action, "learning.archive");
    assert_eq!(resource_type, "learning");
    assert_eq!(resource_id.as_deref(), Some("L-1"));
    let diff: serde_json::Value = serde_json::from_str(diff.as_deref().unwrap()).unwrap();
    assert_eq!(diff["reason"], "audit-trail-test");
    assert_eq!(diff["slug"], "no-mocks");
    assert_eq!(diff["kind"], "feedback");
}
