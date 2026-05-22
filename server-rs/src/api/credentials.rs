//! Branchwork-managed credentials API (Phase 3.2 of
//! `runner-daemon-workspace`).
//!
//! These endpoints sit on top of the schema + helpers established in Phase
//! 3.1 (`server-rs/src/crypto.rs` + the `credentials` table in `db.rs`).
//! Plaintext NEVER reaches the wire — `list` and `create` responses carry
//! only operator-safe fields (`name`, `kind`, `public_part`, `host_hint`,
//! `scopes`). The encrypted blob lives on disk, sealed by the master key
//! at `$BRANCHWORK_DATA_DIR/.master.key`; later phases consume
//! `db::get_credential` at the moment of dispatch.
//!
//! ## Endpoints
//!
//! - `GET /api/credentials` — list the caller's credentials. No secrets in
//!   the payload, ever. Archived rows are filtered out.
//! - `POST /api/credentials` — create (or generate) a credential. Three
//!   shapes:
//!     - `kind: "ssh_key" | "gh_pat" | "gitlab_pat"` + `secret`: store the
//!       caller-supplied secret as-is. `public_part` (the SSH public key
//!       string, or a fingerprint / label for PATs) is optional and stored
//!       verbatim.
//!     - `kind: "generate"`: server generates a fresh ed25519 keypair,
//!       stores the OpenSSH-format private key encrypted, and returns the
//!       OpenSSH one-line public key (`ssh-ed25519 AAAA... [comment]`) in
//!       the response so the operator can paste it into GitHub /
//!       elsewhere. Stored `kind` becomes `"ssh_key"`; the response sets
//!       `generated: true` so the UI can hint the user to copy the
//!       public part on this one occasion.
//! - `DELETE /api/credentials/{id}` — archive (soft-delete) a credential.
//!   Refuses with 409 + `credential_in_use` if any
//!   `projects.default_credential_id` still references this row; the
//!   operator must clear the default on those projects first.
//!
//! ## Auth model
//!
//! Credentials are user-scoped (the `credentials.owner_user_id` FK to
//! `users(id)` cascades on user deletion). Endpoints require an
//! authenticated user — anonymous callers get 401. The acceptance criteria
//! pin a curl-driven smoke test; tests in `server-rs/tests/credentials.rs`
//! sign up + login + carry the session cookie.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use ssh_key::{Algorithm, LineEnding, PrivateKey};
use uuid::Uuid;

use crate::audit;
use crate::auth::AuthUser;
use crate::crypto::{self, MasterKey, resolve_master_key_path};
use crate::db::{CredentialInput, CredentialKind};
use crate::state::AppState;

/// Wire shape of a credential row. Secrets NEVER appear here; only the
/// operator-safe metadata (name, kind, public_part, host_hint, scopes,
/// created_at). The Rust struct is also reused for the `POST` response, so
/// the freshly-created row reads back identical to a `GET` list row.
///
/// `used_by_count` is the number of `projects.default_credential_id`
/// references this row has, populated by the `GET` list endpoint via a
/// subquery so the UI can render `used by N projects` without a
/// per-row probe. Always `0` on the `POST` response (a freshly-created
/// row by definition has zero references yet). Marked `default` on
/// serde so older test fixtures that build the struct without setting
/// the field keep compiling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub public_part: Option<String>,
    pub host_hint: Option<String>,
    pub scopes: Option<String>,
    pub owner_user_id: String,
    pub created_at: String,
    #[serde(default)]
    pub used_by_count: i64,
}

/// Request body for `POST /api/credentials`.
///
/// `kind` selects the create-shape:
/// - `ssh_key` / `gh_pat` / `gitlab_pat`: `secret` REQUIRED (the raw
///   private key body or PAT string). `publicPart` optional.
/// - `generate`: server-side ed25519 generation. `secret` MUST be absent.
///
/// Wire-form fields use camelCase (`publicPart`, `hostHint`) to match
/// the rest of the dashboard API; snake_case aliases are accepted for
/// curl ergonomics. Every field is `Option<...>` (with `#[serde(default)]`)
/// so a missing `name` surfaces as our own `name_required` 400 rather
/// than the 422 axum-serde emits for missing required fields.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCredentialBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default, alias = "public_part")]
    pub public_part: Option<String>,
    #[serde(default, alias = "host_hint")]
    pub host_hint: Option<String>,
    #[serde(default)]
    pub scopes: Option<String>,
}

/// `POST` response. Mirrors [`CredentialRow`] plus a `generated` boolean
/// that the dashboard uses to render "copy this public key into GitHub
/// now — we will not show it again" once.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCredentialResponse {
    #[serde(flatten)]
    pub row: CredentialRow,
    /// True iff the server minted a fresh ed25519 pair on this request.
    /// (`public_part` is the OpenSSH one-line public key on this branch.)
    pub generated: bool,
}

/// `GET /api/credentials` — list the caller's active credentials.
///
/// Secrets are NEVER returned; the encrypted column is excluded from the
/// SELECT. Archived rows are filtered out. Newest first.
pub async fn list_credentials(State(state): State<AppState>, user: AuthUser) -> impl IntoResponse {
    let rows: Vec<CredentialRow> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT c.id, c.name, c.kind, c.public_part, c.host_hint, c.scopes, \
                    c.owner_user_id, c.created_at, \
                    (SELECT COUNT(*) FROM projects p \
                       WHERE p.default_credential_id = c.id) AS used_by_count \
               FROM credentials c \
              WHERE c.owner_user_id = ?1 AND c.archived_at IS NULL \
              ORDER BY c.created_at DESC",
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[credentials] list prepare failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "list_failed" })),
                )
                    .into_response();
            }
        };
        match stmt.query_map(params![user.id], |r| {
            Ok(CredentialRow {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                public_part: r.get(3)?,
                host_hint: r.get(4)?,
                scopes: r.get(5)?,
                owner_user_id: r.get(6)?,
                created_at: r.get(7)?,
                used_by_count: r.get(8)?,
            })
        }) {
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                eprintln!("[credentials] list query failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "list_failed" })),
                )
                    .into_response();
            }
        }
    };

    Json(serde_json::json!({ "credentials": rows })).into_response()
}

/// `POST /api/credentials` — create or generate a credential.
///
/// See module-level docs for the three accepted body shapes. Validation
/// errors return 400 with a structured `{error, message}` body before any
/// crypto / DB work; collisions surface as 500 with `insert_failed`
/// (credential ids are server-minted UUIDs so a real collision is
/// vanishingly unlikely — the 500 is for unexpected DB errors).
pub async fn create_credential(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateCredentialBody>,
) -> impl IntoResponse {
    // ── name validation ─────────────────────────────────────────────────
    let name = body.name.as_deref().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name_required" })),
        )
            .into_response();
    }
    if !is_valid_credential_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_name",
                "message": "name must be 1-128 chars, printable, no leading whitespace",
            })),
        )
            .into_response();
    }

    // ── kind dispatch ───────────────────────────────────────────────────
    let kind_owned = body.kind.as_deref().unwrap_or("").trim().to_string();
    if kind_owned.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "kind_required",
                "message": "kind must be one of: ssh_key, gh_pat, gitlab_pat, generate",
            })),
        )
            .into_response();
    }
    let kind_str = kind_owned.as_str();
    let generate = kind_str == "generate";

    // ── resolve the secret + public_part + stored kind ──────────────────
    let (stored_kind, secret_bytes, public_part) = if generate {
        // `kind: "generate"` triggers server-side ed25519 minting. The
        // caller MUST NOT supply `secret` — refusing here makes the
        // semantics unambiguous (we never silently overwrite a user-
        // supplied secret).
        if body.secret.is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "secret_not_allowed_for_generate",
                    "message": "kind=generate mints a fresh ed25519 pair; do not pass `secret`",
                })),
            )
                .into_response();
        }
        match generate_ed25519_keypair(&name) {
            Ok((private_pem, public_openssh)) => (
                CredentialKind::SshKey,
                private_pem.into_bytes(),
                Some(public_openssh),
            ),
            Err(e) => {
                eprintln!("[credentials] ed25519 generation failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "generation_failed",
                        "message": e,
                    })),
                )
                    .into_response();
            }
        }
    } else {
        let Some(kind) = CredentialKind::parse(kind_str) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_kind",
                    "message": "kind must be one of: ssh_key, gh_pat, gitlab_pat, generate",
                })),
            )
                .into_response();
        };
        let Some(secret) = body.secret.as_ref() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "secret_required",
                    "message": "secret is required for non-generate kinds",
                })),
            )
                .into_response();
        };
        if secret.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "secret_required",
                    "message": "secret must be non-empty",
                })),
            )
                .into_response();
        }
        (kind, secret.as_bytes().to_vec(), body.public_part.clone())
    };

    // ── load the master key ─────────────────────────────────────────────
    let key = match load_master_key(&state) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[credentials] master key load failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "master_key_unavailable",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    // ── insert ──────────────────────────────────────────────────────────
    let id = Uuid::new_v4().to_string();
    let input = CredentialInput {
        id: &id,
        owner_user_id: &user.id,
        name: &name,
        kind: stored_kind,
        public_part: public_part.as_deref(),
        secret_plaintext: &secret_bytes,
        host_hint: body.host_hint.as_deref(),
        scopes: body.scopes.as_deref(),
    };
    if let Err(e) = crate::db::set_credential(&state.db, &key, &input) {
        eprintln!("[credentials] insert failed: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "insert_failed",
                "message": e.to_string(),
            })),
        )
            .into_response();
    }

    // Read back created_at so the response carries the authoritative
    // timestamp the DB assigned.
    let created_at: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT created_at FROM credentials WHERE id = ?1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };

    let row = CredentialRow {
        id: id.clone(),
        name: name.clone(),
        kind: stored_kind.as_str().to_string(),
        public_part: public_part.clone(),
        host_hint: body.host_hint.clone(),
        scopes: body.scopes.clone(),
        owner_user_id: user.id.clone(),
        created_at,
        // Fresh row: no project can reference it yet (the create
        // endpoint never accepts a project_id binding inline). Set
        // explicitly rather than relying on `Default::default()` so a
        // future refactor that flips Serialize-only -> Deserialize+
        // round-trip doesn't silently regress to a stale count.
        used_by_count: 0,
    };

    // ── audit ───────────────────────────────────────────────────────────
    let diff = serde_json::json!({
        "credential_id": row.id,
        "name": row.name,
        "kind": row.kind,
        "host_hint": row.host_hint,
        "generated": generate,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(user.id.as_str()),
            Some(user.email.as_str()),
            audit::actions::CREDENTIAL_CREATE,
            audit::resources::CREDENTIAL,
            Some(&row.id),
            Some(&diff),
        );
    }

    let resp = CreateCredentialResponse {
        row,
        generated: generate,
    };
    (StatusCode::CREATED, Json(resp)).into_response()
}

/// `DELETE /api/credentials/{id}` — archive (soft-delete) a credential.
///
/// Status codes:
/// - 200: archived. Body: `{ ok: true, id }`.
/// - 404: no row matched (`id` does not exist OR belongs to another user
///   OR is already archived).
/// - 409: at least one `projects.default_credential_id` still references
///   this row. Body includes the offending project ids so the operator
///   can clear them. NO audit row is written on this branch — the
///   operator's intent was refused.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthUser,
) -> impl IntoResponse {
    // Look up the row scoped to the caller. We need the name + kind for
    // the audit diff, and we use this query as the user-ownership +
    // not-archived guard.
    let existing: Option<(String, String)> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT name, kind FROM credentials \
              WHERE id = ?1 AND owner_user_id = ?2 AND archived_at IS NULL",
            params![id, user.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok()
    };

    let Some((name, kind)) = existing else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "credential_not_found" })),
        )
            .into_response();
    };

    // Referential-integrity check: any project still using this as its
    // default credential blocks the delete. We collect the offending
    // project ids so the UI can render an actionable list.
    let referencing_projects: Vec<String> = {
        let conn = state.db.lock().unwrap();
        let mut stmt =
            match conn.prepare("SELECT id FROM projects WHERE default_credential_id = ?1") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[credentials] project ref-check prepare failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({ "error": "delete_failed" })),
                    )
                        .into_response();
                }
            };
        match stmt.query_map(params![id], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows.flatten().collect(),
            Err(e) => {
                eprintln!("[credentials] project ref-check query failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "delete_failed" })),
                )
                    .into_response();
            }
        }
    };

    if !referencing_projects.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "credential_in_use",
                "message": format!(
                    "{} project(s) reference this credential as their default; clear the default on those projects first",
                    referencing_projects.len()
                ),
                "projectIds": referencing_projects,
            })),
        )
            .into_response();
    }

    // Archive (the helper is idempotent; archiving an already-archived
    // row returns Ok(false), but we already filtered those out above).
    let archived = match crate::db::archive_credential(&state.db, &id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[credentials] archive failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "delete_failed",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    if !archived {
        // A concurrent caller archived this row between the SELECT and
        // the UPDATE. Treat as 404 — both readers see the same state.
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "credential_not_found" })),
        )
            .into_response();
    }

    // ── audit ───────────────────────────────────────────────────────────
    let diff = serde_json::json!({
        "credential_id": id,
        "name": name,
        "kind": kind,
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        audit::log(
            &conn,
            &user.org_id,
            Some(user.id.as_str()),
            Some(user.email.as_str()),
            audit::actions::CREDENTIAL_DELETE,
            audit::resources::CREDENTIAL,
            Some(&id),
            Some(&diff),
        );
    }

    Json(serde_json::json!({
        "ok": true,
        "id": id,
    }))
    .into_response()
}

/// Resolve and load the master key for the current process.
///
/// Mirrors the resolution rules documented in `crypto::resolve_master_key_path`:
/// honors `$BRANCHWORK_DATA_DIR` when set, otherwise falls back to the
/// server's `claude_dir`. We derive `claude_dir` from `settings_path`'s
/// parent (the settings file lives at `<claude_dir>/branchwork-settings.json`)
/// to avoid threading a new field through `AppState` — credentials
/// endpoints are infrequent, so the single `read()` per call is fine.
fn load_master_key(state: &AppState) -> Result<MasterKey, crypto::CryptoError> {
    let claude_dir = state
        .settings_path
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = resolve_master_key_path(&claude_dir);
    crypto::load_or_generate_master_key(&path)
}

/// Generate a fresh ed25519 keypair, returning
/// `(openssh_pem_private_key, openssh_one_line_public_key)`.
///
/// - Private key is in OpenSSH PEM format (`-----BEGIN OPENSSH PRIVATE KEY-----
///   …`) so a future dispatch path can drop it into `~/.ssh/` and
///   `ssh-add` it without re-encoding.
/// - Public key is the OpenSSH one-line form (`ssh-ed25519 AAAAC3...
///   <comment>`) — the exact bytes the operator pastes into GitHub.
///
/// The `comment` field embeds `branchwork:<name>` so a key on a remote
/// host is visibly attributable to this Branchwork instance.
fn generate_ed25519_keypair(name: &str) -> Result<(String, String), String> {
    let mut rng = ssh_key::rand_core::OsRng;
    let mut private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| format!("ed25519 keygen failed: {e}"))?;
    let comment = format!("branchwork:{name}");
    private_key.set_comment(&comment);
    let pem = private_key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("openssh private-key encoding failed: {e}"))?;
    let public_openssh = private_key
        .public_key()
        .to_openssh()
        .map_err(|e| format!("openssh public-key encoding failed: {e}"))?;
    // `pem` is a `Zeroizing<String>` (or similar) wrapper in some ssh-key
    // versions; call `.to_string()` to produce a plain `String` so the
    // caller can move it into an owned byte buffer without import dance.
    Ok((pem.to_string(), public_openssh))
}

/// Validation for credential names.
///
/// Looser than project-name slug rules (credentials carry human-friendly
/// descriptions like `Cyril's GitHub PAT — work laptop`), but still tight
/// enough to keep them safe to render in audit log diffs without escape
/// gymnastics. Rejects empty, leading-whitespace, and >128-char names;
/// allows any other printable Unicode.
fn is_valid_credential_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    if name.chars().next().is_some_and(|c| c.is_whitespace()) {
        return false;
    }
    // Reject control characters (newlines, NULs, etc.). Everything else
    // is fine — credential names are free-form labels.
    !name.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_credential_name_accepts_typical_labels() {
        assert!(is_valid_credential_name("My GitHub PAT"));
        assert!(is_valid_credential_name("work-laptop key"));
        assert!(is_valid_credential_name("Cyril's GitHub PAT — work laptop"));
        assert!(is_valid_credential_name("a"));
        assert!(is_valid_credential_name(&"a".repeat(128)));
    }

    #[test]
    fn is_valid_credential_name_rejects_empty_and_oversize() {
        assert!(!is_valid_credential_name(""));
        assert!(!is_valid_credential_name(&"a".repeat(129)));
    }

    #[test]
    fn is_valid_credential_name_rejects_leading_whitespace_and_control_chars() {
        assert!(!is_valid_credential_name(" leading space"));
        assert!(!is_valid_credential_name("with\nnewline"));
        assert!(!is_valid_credential_name("with\0null"));
        assert!(!is_valid_credential_name("with\ttab"));
    }

    #[test]
    fn generate_ed25519_keypair_returns_well_formed_openssh_blobs() {
        let (pem, pub_openssh) = generate_ed25519_keypair("test-key").unwrap();
        // Private key has the OpenSSH PEM envelope.
        assert!(
            pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"),
            "private key missing PEM header: {pem:?}"
        );
        assert!(
            pem.trim_end()
                .ends_with("-----END OPENSSH PRIVATE KEY-----"),
            "private key missing PEM footer: {pem:?}"
        );
        // Public key is `ssh-ed25519 <base64> <comment>` one-liner.
        assert!(
            pub_openssh.starts_with("ssh-ed25519 "),
            "public key missing ssh-ed25519 prefix: {pub_openssh:?}"
        );
        assert!(
            pub_openssh.contains("branchwork:test-key"),
            "public key missing branchwork:<name> comment: {pub_openssh:?}"
        );
        // No newlines in the one-line public form.
        assert!(
            !pub_openssh.contains('\n'),
            "public key should be a single line: {pub_openssh:?}"
        );
    }

    #[test]
    fn generate_ed25519_keypair_round_trips_through_ssh_key_parse() {
        // Sanity: the PEM we emit must round-trip through the same crate
        // (so a future dispatch path that re-parses it gets the same key).
        let (pem, pub_openssh) = generate_ed25519_keypair("rtrip").unwrap();
        let reparsed = PrivateKey::from_openssh(&pem).expect("parse PEM");
        let reparsed_pub = reparsed.public_key().to_openssh().expect("encode pub");
        assert_eq!(reparsed_pub, pub_openssh);
    }

    #[test]
    fn generate_ed25519_keypair_produces_distinct_keys_per_call() {
        // Two consecutive generations under the same name must NOT
        // produce the same key material — sanity check on the RNG path.
        let (pem_a, _) = generate_ed25519_keypair("same").unwrap();
        let (pem_b, _) = generate_ed25519_keypair("same").unwrap();
        assert_ne!(pem_a, pem_b, "two ed25519 generations collided");
    }
}
