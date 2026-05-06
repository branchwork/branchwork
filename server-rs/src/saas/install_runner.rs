//! Install-runner endpoints — one-line runner onboarding (Task 4.8).
//!
//! Two endpoints:
//!
//! - `GET /install-runner.sh` (public) — POSIX shell script with the
//!   dashboard's effective public URL templated in. Served as
//!   `text/x-shellscript` so `curl | sh` keeps its UX.
//! - `POST /api/runners/install-command` (auth) — mints a fresh runner
//!   token (same path as `POST /api/runners/tokens`) and returns the
//!   ready-to-paste `curl … | sh -s -- <TOKEN>` one-liner.
//!
//! The script template lives at `deploy/install-runner.sh` and is
//! compiled into the binary via `include_str!` so a single binary ships
//! everything the runner host needs to bootstrap.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::auth::AuthUser;
use crate::state::AppState;

use super::runner_ws::sha256_hex;

/// Sentinel string the script carries until the server substitutes its
/// public URL on every request. Kept as a `pub const` so unit tests can
/// assert the substitution actually fires.
pub const SAAS_URL_PLACEHOLDER: &str = "__SAAS_URL__";

/// Embedded copy of `deploy/install-runner.sh`. Compiled in so the binary
/// is self-contained — no on-disk asset path to worry about at runtime.
const INSTALL_SCRIPT_TEMPLATE: &str = include_str!("../../../deploy/install-runner.sh");

/// Resolve the URL the dashboard advertises for itself, in priority order:
/// 1. `BRANCHWORK_PUBLIC_URL` (set by the prod overlay) — the only signal
///    the operator can fully control.
/// 2. `X-Forwarded-Proto` + `X-Forwarded-Host` (Cloudflare / reverse-proxy
///    deployments).
/// 3. Bare `Host` header — `http://` for `localhost*`, `https://` otherwise.
///
/// Returns a URL with no trailing slash so callers can append their own.
pub fn effective_public_url(headers: &HeaderMap) -> String {
    let env_url = std::env::var("BRANCHWORK_PUBLIC_URL").ok();
    let xfh = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let xfp = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    resolve_public_url(
        env_url.as_deref(),
        xfh.as_deref(),
        host.as_deref(),
        xfp.as_deref(),
    )
}

/// Pure split of [`effective_public_url`] so unit tests don't have to
/// mutate the process env (parallel `cargo test` makes env mutation
/// unsafe). Mirrors the `IdleFinishConfig::from_values` convention in
/// `auto_mode.rs`.
pub fn resolve_public_url(
    env_url: Option<&str>,
    xfh: Option<&str>,
    host: Option<&str>,
    xfp: Option<&str>,
) -> String {
    if let Some(env_url) = env_url {
        let trimmed = env_url.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let host = xfh.or(host).unwrap_or("localhost:3100");
    let proto = xfp.unwrap_or_else(|| {
        if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
            "http"
        } else {
            "https"
        }
    });
    format!("{proto}://{host}")
}

/// Render the install script with the placeholder replaced by `saas_url`.
/// Pure function so unit tests can drive it without a live server.
pub fn render_install_script(template: &str, saas_url: &str) -> String {
    template.replace(SAAS_URL_PLACEHOLDER, saas_url)
}

/// Build the curl-pipe-sh command surfaced in the modal. Token is
/// single-quoted so a future token format with shell metacharacters
/// would not break the line.
pub fn build_install_command(saas_url: &str, token: &str) -> String {
    format!("curl -fsSL {saas_url}/install-runner.sh | sh -s -- '{token}'")
}

// ── GET /install-runner.sh ──────────────────────────────────────────────────

/// Serve the install script. Public — the whole point is `curl | sh`
/// from a clean host with no credentials available.
pub async fn serve_install_script(headers: HeaderMap) -> impl IntoResponse {
    let saas_url = effective_public_url(&headers);
    let script = render_install_script(INSTALL_SCRIPT_TEMPLATE, &saas_url);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        script,
    )
}

// ── POST /api/runners/install-command ───────────────────────────────────────

#[derive(Deserialize)]
pub struct InstallCommandRequest {
    pub runner_name: String,
}

#[derive(Serialize)]
pub struct InstallCommandResponse {
    pub token: String,
    pub command: String,
    pub runner_name: String,
    pub saas_url: String,
}

/// Mint a runner token and return the ready-to-paste install command.
/// Same DB path as `POST /api/runners/tokens` (token is hashed only by
/// `sha256_hex` today, which is identity-on-32-byte-hex by design — see
/// the comment in `runner_ws::sha256_hex`).
pub async fn issue_install_command(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Json(body): Json<InstallCommandRequest>,
) -> Response {
    let runner_name = body.runner_name.trim();
    if runner_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "runner_name_required"})),
        )
            .into_response();
    }

    let token = generate_token();
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "INSERT INTO runner_tokens (token_hash, runner_name, org_id, created_by) \
             VALUES (?1, ?2, ?3, ?4)",
            params![sha256_hex(&token), runner_name, user.org_id, user.id],
        )
        .expect("failed to insert runner token");
    }

    let saas_url = effective_public_url(&headers);
    let command = build_install_command(&saas_url, &token);

    (
        StatusCode::CREATED,
        Json(InstallCommandResponse {
            token,
            command,
            runner_name: runner_name.to_string(),
            saas_url,
        }),
    )
        .into_response()
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(64);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_install_script_substitutes_placeholder() {
        let tmpl = "SAAS=__SAAS_URL__\necho hi";
        assert_eq!(
            render_install_script(tmpl, "https://example.com"),
            "SAAS=https://example.com\necho hi"
        );
    }

    #[test]
    fn render_install_script_replaces_every_occurrence() {
        // The script references __SAAS_URL__ in several places (config
        // file, runner CLI args, next-step hints). All must end up
        // pointing at the same dashboard.
        let tmpl = "a=__SAAS_URL__ b=__SAAS_URL__";
        assert_eq!(
            render_install_script(tmpl, "https://x"),
            "a=https://x b=https://x"
        );
    }

    #[test]
    fn embedded_template_carries_the_sentinel() {
        // Guards against the script being rewritten without leaving the
        // placeholder behind — render_install_script becomes a no-op
        // and the runner would `connect` to literal `__SAAS_URL__`.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(SAAS_URL_PLACEHOLDER),
            "install-runner.sh must embed {SAAS_URL_PLACEHOLDER} for the server to substitute"
        );
    }

    #[test]
    fn build_install_command_quotes_token() {
        // A future token format with shell metacharacters (& | ;) would
        // break the line if the token landed unquoted into `sh -s --`.
        // Single quotes are fine because today's tokens are 32-byte hex.
        assert_eq!(
            build_install_command("https://branchwork.dev", "abc123"),
            "curl -fsSL https://branchwork.dev/install-runner.sh | sh -s -- 'abc123'",
        );
    }

    #[test]
    fn resolve_public_url_honors_env_var_first() {
        assert_eq!(
            resolve_public_url(
                Some("https://override.example"),
                None,
                Some("shouldnotmatter"),
                None,
            ),
            "https://override.example",
        );
    }

    #[test]
    fn resolve_public_url_strips_trailing_slash() {
        assert_eq!(
            resolve_public_url(Some("https://x/"), None, None, None),
            "https://x",
        );
    }

    #[test]
    fn resolve_public_url_treats_blank_env_as_unset() {
        // Setting BRANCHWORK_PUBLIC_URL="" by mistake (a not-uncommon
        // shell-paste accident) must not collapse the URL to "https://"
        // and break every install command. The empty/whitespace branch
        // falls through to the header-based path.
        assert_eq!(
            resolve_public_url(Some("   "), None, Some("branchwork.dev"), None),
            "https://branchwork.dev",
        );
    }

    #[test]
    fn resolve_public_url_uses_xfp_when_present() {
        assert_eq!(
            resolve_public_url(None, Some("dash.example"), None, Some("https")),
            "https://dash.example",
        );
    }

    #[test]
    fn resolve_public_url_xfh_beats_host() {
        // Cloudflare et al. set X-Forwarded-Host to the original Host the
        // browser sent; the proxied request's Host header is the inner
        // service. We must trust the X-Forwarded-Host so the install
        // command points at the public URL, not the internal one.
        assert_eq!(
            resolve_public_url(
                None,
                Some("public.example"),
                Some("internal:3100"),
                Some("https"),
            ),
            "https://public.example",
        );
    }

    #[test]
    fn resolve_public_url_localhost_is_http() {
        assert_eq!(
            resolve_public_url(None, None, Some("localhost:3100"), None),
            "http://localhost:3100",
        );
    }

    #[test]
    fn resolve_public_url_remote_host_is_https() {
        assert_eq!(
            resolve_public_url(None, None, Some("branchwork.dev"), None),
            "https://branchwork.dev",
        );
    }

    #[test]
    fn resolve_public_url_falls_back_when_nothing_known() {
        // Nothing set at all (e.g. unit-test invocation with no headers)
        // should still produce a usable URL; localhost is the only sane
        // default.
        assert_eq!(
            resolve_public_url(None, None, None, None),
            "http://localhost:3100",
        );
    }
}
