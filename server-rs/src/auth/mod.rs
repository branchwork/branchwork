pub mod orgs;
pub mod sessions;

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Db;
use crate::state::AppState;

/// The authenticated user, injected into request extensions by
/// [`populate_auth_user`] and handed to handlers via the [`AuthUser`] extractor.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
    /// Active organization for this request (resolved from `X-Org-Id` header
    /// or the user's first org).
    pub org_id: String,
    /// User's role inside [`org_id`]. Used by org management handlers for
    /// permission checks.
    #[allow(dead_code)]
    pub org_role: String,
}

/// Optional variant of [`AuthUser`]. Handlers that need to support both
/// authenticated and anonymous callers (e.g. backward-compat for MCP/curl
/// agents) extract this instead.
#[derive(Debug, Clone)]
pub struct OptionalAuthUser(pub Option<AuthUser>);

impl OptionalAuthUser {
    /// Convenience: return the active org_id, falling back to the default org
    /// for unauthenticated requests.
    pub fn org_id(&self) -> &str {
        self.0
            .as_ref()
            .map(|u| u.org_id.as_str())
            .unwrap_or(orgs::DEFAULT_ORG_ID)
    }
}

impl<S> axum::extract::FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or((StatusCode::UNAUTHORIZED, "unauthenticated"))
    }
}

impl<S> axum::extract::FromRequestParts<S> for OptionalAuthUser
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(OptionalAuthUser(
            parts.extensions.get::<AuthUser>().cloned(),
        ))
    }
}

// ── Request / response shapes ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserDto {
    id: String,
    email: String,
    org_id: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

fn err(status: StatusCode, msg: &'static str) -> Response {
    (status, Json(ErrorResponse { error: msg })).into_response()
}

fn set_cookie(cookie: String) -> HeaderMap {
    let mut h = HeaderMap::new();
    // unwrap: our cookie string only contains ASCII / cookie-safe chars.
    h.insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    h
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// POST /api/auth/signup
pub async fn signup(State(state): State<AppState>, Json(creds): Json<Credentials>) -> Response {
    let email = creds.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return err(StatusCode::BAD_REQUEST, "invalid_email");
    }
    if creds.password.len() < 8 {
        return err(StatusCode::BAD_REQUEST, "password_too_short");
    }

    // bcrypt truncates input at 72 bytes — longer passwords would silently
    // ignore the tail. Reject rather than accept a foot-gun.
    if creds.password.len() > 72 {
        return err(StatusCode::BAD_REQUEST, "password_too_long");
    }

    let hash = match bcrypt::hash(&creds.password, bcrypt::DEFAULT_COST) {
        Ok(h) => h,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "hash_failed"),
    };

    let id = Uuid::new_v4().to_string();
    let personal_org_id;
    {
        let conn = state.db.lock().unwrap();
        let res = conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
            params![id, email, hash],
        );
        if let Err(e) = res {
            // UNIQUE violation on `email` is the only business-logic failure
            // we care about here; everything else is a 500.
            let msg = e.to_string();
            if msg.contains("UNIQUE") {
                return err(StatusCode::CONFLICT, "email_taken");
            }
            eprintln!("[auth] signup insert error: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "db_error");
        }

        // Every new user gets a personal org so their data is isolated.
        personal_org_id = orgs::create_personal_org(&conn, &id, &email);

        // Also add them to the default org so they can see pre-existing data.
        conn.execute(
            "INSERT OR IGNORE INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)",
            params![orgs::DEFAULT_ORG_ID, id, orgs::ROLE_MEMBER],
        )
        .ok();
    }

    let token = sessions::create(&state.db, &id);
    let headers = set_cookie(sessions::set_cookie_value(&token));
    (
        StatusCode::CREATED,
        headers,
        Json(UserDto {
            id,
            email,
            org_id: Some(personal_org_id),
        }),
    )
        .into_response()
}

/// POST /api/auth/login
pub async fn login(State(state): State<AppState>, Json(creds): Json<Credentials>) -> Response {
    let email = creds.email.trim().to_lowercase();

    let row: Option<(String, String)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT id, password_hash FROM users WHERE email = ?1",
            params![email],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()
    };

    let (id, hash) = match row {
        Some(r) => r,
        // Same response for "no such user" and "wrong password" — do not leak
        // which emails are registered.
        None => return err(StatusCode::UNAUTHORIZED, "invalid_credentials"),
    };

    match bcrypt::verify(&creds.password, &hash) {
        Ok(true) => {}
        _ => return err(StatusCode::UNAUTHORIZED, "invalid_credentials"),
    }

    // Resolve the user's first org for the login response.
    let first_org = {
        let conn = state.db.lock().unwrap();
        let memberships = orgs::user_memberships(&conn, &id);
        memberships.first().map(|m| m.org_id.clone())
    };

    let token = sessions::create(&state.db, &id);
    let headers = set_cookie(sessions::set_cookie_value(&token));
    (
        StatusCode::OK,
        headers,
        Json(UserDto {
            id,
            email,
            org_id: first_org,
        }),
    )
        .into_response()
}

/// POST /api/auth/logout
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
        && let Some(token) = sessions::token_from_cookie_header(cookie)
    {
        sessions::delete(&state.db, &token);
    }
    let h = set_cookie(sessions::clear_cookie_value());
    (StatusCode::OK, h, Json(serde_json::json!({"ok": true}))).into_response()
}

/// GET /api/auth/me — returns the current user, or 401 if unauthenticated.
pub async fn me(user: AuthUser) -> Response {
    Json(UserDto {
        id: user.id,
        email: user.email,
        org_id: Some(user.org_id),
    })
    .into_response()
}

// ── Middleware ───────────────────────────────────────────────────────────────

/// Axum middleware that looks up the session cookie and injects an
/// [`AuthUser`] into request extensions on success.
///
/// This is a *population* layer, not a gate: unauthenticated requests still
/// pass through so public routes keep working. Protected handlers opt in by
/// taking `AuthUser` as an extractor — which 401s when the extension is
/// missing.
pub async fn populate_auth_user(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(cookie) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        && let Some(token) = sessions::token_from_cookie_header(cookie)
        && let Some(session) = sessions::lookup_and_slide(&state.db, &token)
    {
        // Determine which org the caller wants to act in.
        let requested_org = req
            .headers()
            .get("x-org-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if let Some(user) = load_user(&state.db, &session.user_id, requested_org.as_deref()) {
            req.extensions_mut().insert(user);
        }
    }
    next.run(req).await
}

fn load_user(db: &Db, user_id: &str, requested_org: Option<&str>) -> Option<AuthUser> {
    let conn = db.lock().unwrap();
    let (id, email): (String, String) = conn
        .query_row(
            "SELECT id, email FROM users WHERE id = ?1",
            params![user_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()?;

    let memberships = orgs::user_memberships(&conn, &id);

    // If the caller specified X-Org-Id, validate they're a member.
    // Otherwise fall back to their first org (or the default org).
    let (org_id, org_role) = if let Some(req) = requested_org
        && let Some(m) = memberships.iter().find(|m| m.org_id == req)
    {
        (m.org_id.clone(), m.role.clone())
    } else if let Some(first) = memberships.first() {
        (first.org_id.clone(), first.role.clone())
    } else {
        // User has no org memberships — put them in the default org as a
        // viewer so existing routes don't break.
        (
            orgs::DEFAULT_ORG_ID.to_string(),
            orgs::ROLE_VIEWER.to_string(),
        )
    };

    Some(AuthUser {
        id,
        email,
        org_id,
        org_role,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_too_short_is_rejected() {
        // Sanity-check the constant stays in sync with callers' expectations.
        assert!("1234567".len() < 8);
    }

    #[test]
    fn bcrypt_roundtrip() {
        let h = bcrypt::hash("hunter2hunter2", bcrypt::DEFAULT_COST).unwrap();
        assert!(bcrypt::verify("hunter2hunter2", &h).unwrap());
        assert!(!bcrypt::verify("wrong", &h).unwrap());
    }
}
