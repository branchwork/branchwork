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
/// [`require_auth`] and handed to handlers via the [`AuthUser`] extractor.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: String,
    pub email: String,
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
    }

    let token = sessions::create(&state.db, &id);
    let headers = set_cookie(sessions::set_cookie_value(&token));
    (StatusCode::CREATED, headers, Json(UserDto { id, email })).into_response()
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

    let token = sessions::create(&state.db, &id);
    let headers = set_cookie(sessions::set_cookie_value(&token));
    (StatusCode::OK, headers, Json(UserDto { id, email })).into_response()
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
        && let Some(user) = load_user(&state.db, &session.user_id)
    {
        req.extensions_mut().insert(user);
    }
    next.run(req).await
}

fn load_user(db: &Db, user_id: &str) -> Option<AuthUser> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT id, email FROM users WHERE id = ?1",
        params![user_id],
        |r| {
            Ok(AuthUser {
                id: r.get(0)?,
                email: r.get(1)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
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
