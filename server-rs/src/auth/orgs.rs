//! Organization management: CRUD, membership, and plan-ownership mapping.
//!
//! An **organization** is the multi-tenancy boundary. Plans, agents, costs, and
//! budgets all belong to exactly one org. Users belong to one or more orgs with
//! a role (owner › admin › member › viewer).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::AuthUser;
use crate::state::AppState;

// ── Constants ───────────────────────────────────────────────────────────────

/// Deterministic ID for the org that inherits all pre-multi-tenancy data.
pub const DEFAULT_ORG_ID: &str = "default-org";

pub const ROLE_OWNER: &str = "owner";
pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_MEMBER: &str = "member";
pub const ROLE_VIEWER: &str = "viewer";

const VALID_ROLES: &[&str] = &[ROLE_OWNER, ROLE_ADMIN, ROLE_MEMBER, ROLE_VIEWER];

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Organization {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMembership {
    pub org_id: String,
    pub org_name: String,
    pub org_slug: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgMember {
    pub user_id: String,
    pub email: String,
    pub role: String,
    pub joined_at: String,
}

// ── DB helpers (used by auth middleware and other modules) ───────────────────

/// Load all org memberships for a user. Empty vec if none.
pub fn user_memberships(conn: &Connection, user_id: &str) -> Vec<OrgMembership> {
    conn.prepare(
        "SELECT om.org_id, o.name, o.slug, om.role \
         FROM org_members om \
         JOIN organizations o ON o.id = om.org_id \
         WHERE om.user_id = ?1 \
         ORDER BY o.name",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![user_id], |row| {
            Ok(OrgMembership {
                org_id: row.get(0)?,
                org_name: row.get(1)?,
                org_slug: row.get(2)?,
                role: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Check whether `plan_name` belongs to `org_id`.
///
/// A plan belongs to an org if it has an explicit `plan_org` row for that org,
/// **or** if no `plan_org` row exists at all and the requested org is the
/// default org (backward-compat for pre-multi-tenancy data).
pub fn plan_belongs_to_org(conn: &Connection, plan_name: &str, org_id: &str) -> bool {
    match conn
        .query_row(
            "SELECT org_id FROM plan_org WHERE plan_name = ?1",
            params![plan_name],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    {
        Some(owner) => owner == org_id,
        // No explicit mapping → belongs to the default org.
        None => org_id == DEFAULT_ORG_ID,
    }
}

/// Return the org_id that owns `plan_name`, or [`DEFAULT_ORG_ID`] if unmapped.
#[allow(dead_code)] // public API — used by future plan-assignment code
pub fn org_for_plan(conn: &Connection, plan_name: &str) -> String {
    conn.query_row(
        "SELECT org_id FROM plan_org WHERE plan_name = ?1",
        params![plan_name],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| DEFAULT_ORG_ID.to_string())
}

/// Assign a plan to an org. Idempotent (upsert).
#[allow(dead_code)] // public API — called by plan creation code path
pub fn assign_plan_to_org(conn: &Connection, plan_name: &str, org_id: &str) {
    conn.execute(
        "INSERT INTO plan_org (plan_name, org_id) VALUES (?1, ?2) \
         ON CONFLICT(plan_name) DO UPDATE SET org_id = excluded.org_id",
        params![plan_name, org_id],
    )
    .ok();
}

/// Create a personal org for a newly registered user and make them the owner.
/// Returns the org_id.
pub fn create_personal_org(conn: &Connection, user_id: &str, email: &str) -> String {
    let org_id = Uuid::new_v4().to_string();
    let slug = format!("personal-{}", &user_id[..8.min(user_id.len())]);
    let name = format!("{}'s org", email.split('@').next().unwrap_or(email));
    conn.execute(
        "INSERT INTO organizations (id, name, slug) VALUES (?1, ?2, ?3)",
        params![org_id, name, slug],
    )
    .expect("failed to create personal org");
    conn.execute(
        "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
        params![org_id, user_id, ROLE_OWNER],
    )
    .expect("failed to add owner to personal org");
    org_id
}

/// Migration helper: create the default org and assign every existing user to
/// it as an owner. Called from [`crate::db::migrate`]. Idempotent.
pub fn ensure_default_org(conn: &Connection) {
    conn.execute(
        "INSERT OR IGNORE INTO organizations (id, name, slug) VALUES (?1, ?2, ?3)",
        params![DEFAULT_ORG_ID, "Default Organization", "default"],
    )
    .ok();

    // Every user that is not yet in *any* org gets added to the default org.
    conn.execute_batch(
        "INSERT OR IGNORE INTO org_members (org_id, user_id, role) \
         SELECT 'default-org', id, 'owner' FROM users \
         WHERE id NOT IN (SELECT user_id FROM org_members)",
    )
    .ok();

    // Claim any plans that have DB presence but no plan_org row.
    conn.execute_batch(
        "INSERT OR IGNORE INTO plan_org (plan_name, org_id) \
         SELECT plan_name, 'default-org' FROM plan_project \
         WHERE plan_name NOT IN (SELECT plan_name FROM plan_org)",
    )
    .ok();
    conn.execute_batch(
        "INSERT OR IGNORE INTO plan_org (plan_name, org_id) \
         SELECT DISTINCT plan_name, 'default-org' FROM task_status \
         WHERE plan_name NOT IN (SELECT plan_name FROM plan_org)",
    )
    .ok();
}

// ── Request / response DTOs ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateOrgBody {
    pub name: String,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct AddMemberBody {
    pub email: String,
    pub role: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateRoleBody {
    pub role: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(ErrorResponse { error: msg })).into_response()
}

// ── Handlers ────────────────────────────────────────────────────────────────

/// `POST /api/orgs` — create a new organization.
pub async fn create_org(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateOrgBody>,
) -> Response {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name_required");
    }

    let slug = body
        .slug
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| slugify(&name));
    if slug.is_empty() {
        return err(StatusCode::BAD_REQUEST, "invalid_slug");
    }

    let id = Uuid::new_v4().to_string();
    {
        let conn = state.db.lock().unwrap();
        let res = conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES (?1, ?2, ?3)",
            params![id, name, slug],
        );
        if let Err(e) = res {
            if e.to_string().contains("UNIQUE") {
                return err(StatusCode::CONFLICT, "slug_taken");
            }
            return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error");
        }
        // Creator becomes owner
        conn.execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
            params![id, user.id, ROLE_OWNER],
        )
        .ok();
    }

    (
        StatusCode::CREATED,
        Json(Organization {
            id,
            name,
            slug,
            created_at: chrono::Utc::now().to_rfc3339(),
        }),
    )
        .into_response()
}

/// `GET /api/orgs` — list orgs the current user belongs to.
pub async fn list_orgs(State(state): State<AppState>, user: AuthUser) -> Response {
    let conn = state.db.lock().unwrap();
    let memberships = user_memberships(&conn, &user.id);
    let orgs: Vec<serde_json::Value> = memberships
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "id": m.org_id,
                "name": m.org_name,
                "slug": m.org_slug,
                "role": m.role,
            })
        })
        .collect();
    Json(orgs).into_response()
}

/// `GET /api/orgs/:slug` — get org details + member list.
pub async fn get_org(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
) -> Response {
    let conn = state.db.lock().unwrap();
    let org: Option<Organization> = conn
        .query_row(
            "SELECT id, name, slug, created_at FROM organizations WHERE slug = ?1",
            params![slug],
            |row| {
                Ok(Organization {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    slug: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten();

    let org = match org {
        Some(o) => o,
        None => return err(StatusCode::NOT_FOUND, "org_not_found"),
    };

    // Verify caller is a member
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org.id, user.id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if role.is_none() {
        return err(StatusCode::FORBIDDEN, "not_a_member");
    }

    // Load members
    let members: Vec<OrgMember> = conn
        .prepare(
            "SELECT om.user_id, u.email, om.role, om.joined_at \
             FROM org_members om JOIN users u ON u.id = om.user_id \
             WHERE om.org_id = ?1 ORDER BY om.joined_at",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![org.id], |row| {
                Ok(OrgMember {
                    user_id: row.get(0)?,
                    email: row.get(1)?,
                    role: row.get(2)?,
                    joined_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Json(serde_json::json!({
        "id": org.id,
        "name": org.name,
        "slug": org.slug,
        "createdAt": org.created_at,
        "members": members,
    }))
    .into_response()
}

/// `POST /api/orgs/:slug/members` — add a user to the org by email.
pub async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(slug): Path<String>,
    Json(body): Json<AddMemberBody>,
) -> Response {
    let role = body.role.as_deref().unwrap_or(ROLE_MEMBER);
    if !VALID_ROLES.contains(&role) {
        return err(StatusCode::BAD_REQUEST, "invalid_role");
    }

    let conn = state.db.lock().unwrap();

    // Resolve org
    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let org_id = match org_id {
        Some(id) => id,
        None => return err(StatusCode::NOT_FOUND, "org_not_found"),
    };

    // Caller must be owner or admin
    let caller_role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    match caller_role.as_deref() {
        Some(ROLE_OWNER) | Some(ROLE_ADMIN) => {}
        _ => return err(StatusCode::FORBIDDEN, "insufficient_permissions"),
    }

    // Resolve target user by email
    let target_id: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE email = ?1",
            params![body.email.trim().to_lowercase()],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let target_id = match target_id {
        Some(id) => id,
        None => return err(StatusCode::NOT_FOUND, "user_not_found"),
    };

    let res = conn.execute(
        "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3) \
         ON CONFLICT(org_id, user_id) DO UPDATE SET role = excluded.role",
        params![org_id, target_id, role],
    );
    if res.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error");
    }

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::ORG_MEMBER_ADD,
        crate::audit::resources::ORG,
        Some(&slug),
        Some(
            &serde_json::json!({
                "targetUser": target_id,
                "email": body.email.trim().to_lowercase(),
                "role": role,
            })
            .to_string(),
        ),
    );

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

/// `DELETE /api/orgs/:slug/members/:user_id` — remove a member.
pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((slug, target_user_id)): Path<(String, String)>,
) -> Response {
    let conn = state.db.lock().unwrap();

    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let org_id = match org_id {
        Some(id) => id,
        None => return err(StatusCode::NOT_FOUND, "org_not_found"),
    };

    // Caller must be owner or admin (or removing themselves)
    if target_user_id != user.id {
        let caller_role: Option<String> = conn
            .query_row(
                "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
                params![org_id, user.id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        match caller_role.as_deref() {
            Some(ROLE_OWNER) | Some(ROLE_ADMIN) => {}
            _ => return err(StatusCode::FORBIDDEN, "insufficient_permissions"),
        }
    }

    conn.execute(
        "DELETE FROM org_members WHERE org_id = ?1 AND user_id = ?2",
        params![org_id, target_user_id],
    )
    .ok();

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::ORG_MEMBER_REMOVE,
        crate::audit::resources::ORG,
        Some(&slug),
        Some(&serde_json::json!({"targetUser": target_user_id}).to_string()),
    );

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

/// `PUT /api/orgs/:slug/members/:user_id/role` — change a member's role.
pub async fn update_member_role(
    State(state): State<AppState>,
    user: AuthUser,
    Path((slug, target_user_id)): Path<(String, String)>,
    Json(body): Json<UpdateRoleBody>,
) -> Response {
    if !VALID_ROLES.contains(&body.role.as_str()) {
        return err(StatusCode::BAD_REQUEST, "invalid_role");
    }

    let conn = state.db.lock().unwrap();

    let org_id: Option<String> = conn
        .query_row(
            "SELECT id FROM organizations WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let org_id = match org_id {
        Some(id) => id,
        None => return err(StatusCode::NOT_FOUND, "org_not_found"),
    };

    // Only owners can change roles
    let caller_role: Option<String> = conn
        .query_row(
            "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user.id],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if caller_role.as_deref() != Some(ROLE_OWNER) {
        return err(StatusCode::FORBIDDEN, "owner_only");
    }

    conn.execute(
        "UPDATE org_members SET role = ?1 WHERE org_id = ?2 AND user_id = ?3",
        params![body.role, org_id, target_user_id],
    )
    .ok();

    crate::audit::log(
        &conn,
        &org_id,
        Some(&user.id),
        Some(&user.email),
        crate::audit::actions::ORG_MEMBER_ROLE_CHANGE,
        crate::audit::resources::ORG,
        Some(&slug),
        Some(
            &serde_json::json!({
                "targetUser": target_user_id,
                "newRole": body.role,
            })
            .to_string(),
        ),
    );

    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
}

// ── Utilities ───────────────────────────────────────────────────────────────

/// Derive a URL-safe slug from a human name.
fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Db};
    use tempfile::TempDir;

    fn fresh() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = db::init(&path);
        (db, dir)
    }

    #[test]
    fn default_org_exists_after_migration() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();
        let name: String = conn
            .query_row(
                "SELECT name FROM organizations WHERE id = ?1",
                params![DEFAULT_ORG_ID],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(name, "Default Organization");
    }

    #[test]
    fn existing_users_assigned_to_default_org() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'a@b.com', 'x')",
            [],
        )
        .unwrap();
        drop(conn);

        // Re-run migration (simulates server restart after user was created
        // but before multi-tenancy existed)
        {
            let conn = db.lock().unwrap();
            super::ensure_default_org(&conn);
        }

        let conn = db.lock().unwrap();
        let role: String = conn
            .query_row(
                "SELECT role FROM org_members WHERE org_id = ?1 AND user_id = 'u1'",
                params![DEFAULT_ORG_ID],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(role, ROLE_OWNER);
    }

    #[test]
    fn plan_belongs_to_org_default_fallback() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();
        // Plan with no plan_org entry → belongs to default org
        assert!(plan_belongs_to_org(&conn, "orphan-plan", DEFAULT_ORG_ID));
        assert!(!plan_belongs_to_org(&conn, "orphan-plan", "other-org"));
    }

    #[test]
    fn plan_belongs_to_org_explicit() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();
        // Create the org first (FK constraint)
        conn.execute(
            "INSERT INTO organizations (id, name, slug) VALUES ('org-x', 'Org X', 'org-x')",
            [],
        )
        .unwrap();
        assign_plan_to_org(&conn, "my-plan", "org-x");
        assert!(plan_belongs_to_org(&conn, "my-plan", "org-x"));
        assert!(!plan_belongs_to_org(&conn, "my-plan", "org-y"));
        assert!(!plan_belongs_to_org(&conn, "my-plan", DEFAULT_ORG_ID));
    }

    #[test]
    fn personal_org_created_on_signup() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'alice@test.com', 'x')",
            [],
        )
        .unwrap();
        let org_id = create_personal_org(&conn, "u1", "alice@test.com");
        let memberships = user_memberships(&conn, "u1");
        let personal = memberships.iter().find(|m| m.org_id == org_id);
        assert!(personal.is_some());
        assert_eq!(personal.unwrap().role, ROLE_OWNER);
    }

    #[test]
    fn org_isolation_between_users() {
        let (db, _dir) = fresh();
        let conn = db.lock().unwrap();

        // Create two users with personal orgs
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u1', 'a@test.com', 'x')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES ('u2', 'b@test.com', 'x')",
            [],
        )
        .unwrap();
        let org_a = create_personal_org(&conn, "u1", "a@test.com");
        let org_b = create_personal_org(&conn, "u2", "b@test.com");

        // Assign plans
        assign_plan_to_org(&conn, "plan-a", &org_a);
        assign_plan_to_org(&conn, "plan-b", &org_b);

        // User A in org X can't see user B's plans in org Y
        assert!(plan_belongs_to_org(&conn, "plan-a", &org_a));
        assert!(!plan_belongs_to_org(&conn, "plan-a", &org_b));
        assert!(plan_belongs_to_org(&conn, "plan-b", &org_b));
        assert!(!plan_belongs_to_org(&conn, "plan-b", &org_a));
    }

    #[test]
    fn slugify_works() {
        assert_eq!(slugify("My Cool Org"), "my-cool-org");
        assert_eq!(slugify("  Test  "), "test");
        assert_eq!(slugify("hello_world!"), "hello-world");
    }
}
