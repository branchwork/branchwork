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

    /// Pre-T5.22 regression: the unsubstituted-script check on line 60
    /// of install-runner.sh literally compared `$SAAS_URL` to
    /// `__SAAS_URL__`, so after `render_install_script` rewrote *every*
    /// occurrence of the placeholder, the comparison became
    /// `"$saas_url" = "$saas_url"` — always true, and the rendered
    /// script always errored out with "this script was not fetched from
    /// a Branchwork dashboard". The fix splits the sentinel literal into
    /// three concatenated shell strings (`"__""SAAS_URL""__"`) so the
    /// contiguous placeholder appears only in the assignment on line 42,
    /// not in the check. This test pins both halves of the invariant:
    ///   (a) the rendered script must substitute the line-42 default to
    ///       the configured public URL, and
    ///   (b) the rendered script must NOT contain a literal comparison
    ///       to that same URL value (the post-T5.22 sentinel is built at
    ///       shell runtime from the concat'd pieces, not baked in).
    #[test]
    fn render_install_script_leaves_sentinel_check_intact() {
        let url = "https://branchwork.dev";
        let rendered = render_install_script(INSTALL_SCRIPT_TEMPLATE, url);
        assert!(
            rendered.contains(&format!("SAAS_URL=\"${{BRANCHWORK_SAAS_URL:-{url}}}\"")),
            "line-42 default must be substituted to the configured URL"
        );
        // The post-T5.22 sentinel is built from three string literals at
        // shell-runtime so the contiguous placeholder is NOT in the
        // rendered output anywhere except the line-42 default value.
        let occurrences = rendered.matches(SAAS_URL_PLACEHOLDER).count();
        assert_eq!(
            occurrences, 0,
            "no contiguous {SAAS_URL_PLACEHOLDER} should remain after render — got {occurrences}"
        );
        // The runtime concat MUST still be present, otherwise the
        // sentinel check is gone entirely and someone running the raw
        // file from git would silently proceed with a literal
        // `__SAAS_URL__` as SAAS_URL.
        assert!(
            rendered.contains(r#"_UNSUBSTITUTED="__""SAAS_URL""__""#),
            "runtime-concat sentinel must survive in the rendered script"
        );
        // And the tautology must be gone: `"$SAAS_URL" = "<url>"` would
        // mean we've reintroduced the original bug.
        assert!(
            !rendered.contains(&format!("[ \"$SAAS_URL\" = \"{url}\" ]")),
            "rendered script must not compare $SAAS_URL to the substituted URL — that's the T5.22 bug"
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

    // ── T2.2 foreign-runner detection contract ──────────────────────────
    //
    // These tests pin the user-visible surface of the foreign-runner
    // refusal so a future edit to install-runner.sh cannot silently break
    // the acceptance criterion ("another runner is already running as pid
    // 12345 from /opt/other-runner; pass --force-replace to take over").

    #[test]
    fn install_script_carries_force_replace_flag() {
        // The flag must be parseable + documented in usage.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("--force-replace"),
            "install-runner.sh must expose --force-replace"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("FORCE_REPLACE=0"),
            "install-runner.sh must initialise FORCE_REPLACE"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("--force-replace [TOKEN]"),
            "usage banner must list the --force-replace form"
        );
    }

    #[test]
    fn install_script_emits_canonical_foreign_runner_error() {
        // Exact substring is what the dashboard runbook + future scrapers
        // will grep for. Any rewording must update both sides in lockstep.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("another runner is already running as pid"),
            "must emit the canonical 'another runner is already running as pid…' error"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("pass --force-replace to take over"),
            "error must suggest --force-replace as the remediation"
        );
    }

    #[test]
    fn install_script_uses_kernel_truth_for_detection() {
        // Defense against a future regression that re-introduces
        // pgrep -f against the host process list (forbidden by ADR 0005:
        // matches the production supervisor's command line).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("/proc/[0-9]*"),
            "Linux detection must walk /proc/[0-9]*/exe — kernel-truth, no pattern matching"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("readlink \"$proc_pid_dir/exe\""),
            "must readlink /proc/<pid>/exe to confirm binary identity"
        );
        // macOS fallback path — pgrep -x is safe on Darwin (no comm
        // truncation) and lsof gives us the binary path.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("pgrep -x branchwork-runner"),
            "macOS fallback must pgrep -x (exact name, no -f substring match)"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("lsof -p"),
            "macOS fallback must lsof the candidate PID for txt-type FD"
        );
        // Negative contract: no `pgrep -f branchwork-runner` in an
        // executable line (comments referring to the forbidden pattern
        // by name are fine — they document what we deliberately avoid).
        let has_unscoped_pgrep_f = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("pgrep -f branchwork-runner")
        });
        assert!(
            !has_unscoped_pgrep_f,
            "install-runner.sh must NOT pgrep -f as executable code (CLAUDE.md / ADR 0005)"
        );
        let has_killall = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("killall branchwork-runner")
        });
        assert!(
            !has_killall,
            "install-runner.sh must NOT killall branchwork-runner as executable code (ADR 0005)"
        );
    }

    #[test]
    fn install_script_skips_own_managed_pid() {
        // The check must subtract our own $PID_FILE's PID from the
        // foreign-candidate set — otherwise a re-run while our own
        // runner is up would always 1.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("our_managed_pid"),
            "must read our managed PID from $PID_FILE before scoring"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ -n "$our_pid" ] && [ "$pid" = "$our_pid" ]"#),
            "must skip our own managed PID inside the candidate loop"
        );
    }

    #[test]
    fn install_script_invokes_foreign_check_before_binary_download() {
        // Failing fast keeps a re-paste from wasting bandwidth on a host
        // that already has a foreign runner. The `check_foreign_runners`
        // call line must come before the first `download_binary_to` call.
        // (Renamed from `download_binary` in T3.1 to make the per-binary
        // target output path explicit.)
        let template = INSTALL_SCRIPT_TEMPLATE;
        let check_at = template
            .find("\ncheck_foreign_runners\n")
            .expect("check_foreign_runners must be invoked at top level");
        let download_at = template
            .find("download_binary_to ")
            .expect("download_binary_to must appear in the script");
        assert!(
            check_at < download_at,
            "check_foreign_runners must run before download_binary_to (fail fast on contention)"
        );
    }

    #[test]
    fn install_script_force_replace_uses_sigterm_then_sigkill() {
        // Mirrors the SIGTERM-then-SIGKILL ladder we use for our own
        // managed runner — the foreign install gets the same grace
        // period before we force-quit it.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("kill -TERM \"$pid\""),
            "force-replace must SIGTERM the foreign PID first"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("kill -KILL \"$pid\""),
            "force-replace must SIGKILL after the grace window"
        );
    }

    // ── T3.1 paired-binary install contract ─────────────────────────────
    //
    // install-runner.sh must drop BOTH `branchwork-runner` and
    // `branchwork-server` at $HOME/.local/bin/ so Start session works
    // without an operator-supplied --server-bin hint. These tests pin
    // the surface so a future re-org of the source-resolution loop
    // can't silently regress to runner-only install.

    #[test]
    fn install_script_installs_both_binaries() {
        // The two target paths must be declared next to each other and
        // both flow into the final `mv $TMP_* $INSTALL_DIR/*` step.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"RUNNER_BIN="$INSTALL_DIR/branchwork-runner""#),
            "must declare RUNNER_BIN under $INSTALL_DIR"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"SERVER_BIN="$INSTALL_DIR/branchwork-server""#),
            "must declare SERVER_BIN under $INSTALL_DIR (T3.1 paired install)"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"mv "$TMP_RUNNER" "$RUNNER_BIN""#),
            "must move the runner tmpfile into $RUNNER_BIN"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"mv "$TMP_SERVER" "$SERVER_BIN""#),
            "must move the server tmpfile into $SERVER_BIN"
        );
    }

    #[test]
    fn install_script_atomic_pair_no_half_install() {
        // Both binaries must be downloaded to tmpfiles BEFORE either is
        // moved into $INSTALL_DIR, so a partial network failure cannot
        // leave the host with a runner pointing at a missing server.
        // We assert this structurally: every reference to mv into the
        // final path must come after every download_binary_to call.
        let template = INSTALL_SCRIPT_TEMPLATE;
        let last_download = template
            .rfind("download_binary_to")
            .expect("script must call download_binary_to at least once");
        let first_mv = template
            .find(r#"mv "$TMP_RUNNER" "$RUNNER_BIN""#)
            .expect("script must mv the runner into $RUNNER_BIN");
        assert!(
            last_download < first_mv,
            "every download must precede every final-path mv (atomic-pair contract)"
        );
        let extract_call_idx = template
            .find("if extract_via_docker ")
            .expect("docker fallback must run between downloads and mv");
        assert!(
            extract_call_idx < first_mv,
            "docker fallback must precede the final-path mv"
        );
    }

    #[test]
    fn install_script_has_per_binary_overrides() {
        // BRANCHWORK_BINARY_URL stays runner-only for backward compat;
        // BRANCHWORK_SERVER_BINARY_URL is the new sibling override so
        // dogfooders can pin a custom server build alongside a custom
        // runner build.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("BRANCHWORK_BINARY_URL"),
            "runner override env var must remain wired"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("BRANCHWORK_SERVER_BINARY_URL"),
            "T3.1 must add BRANCHWORK_SERVER_BINARY_URL as the server-side override"
        );
        // The two overrides must be processed at the top of the source
        // resolution loop (env > release > docker), in that order, so
        // operators can pin a custom build without docker on the host.
        let template = INSTALL_SCRIPT_TEMPLATE;
        let runner_env_idx = template
            .find(r#"if [ -n "${BRANCHWORK_BINARY_URL:-}" ]"#)
            .expect("runner env override branch missing");
        let server_env_idx = template
            .find(r#"if [ -n "${BRANCHWORK_SERVER_BINARY_URL:-}" ]"#)
            .expect("server env override branch missing");
        let release_idx = template
            .find("releases/latest/download/branchwork-runner-")
            .expect("release asset path missing");
        assert!(
            runner_env_idx < release_idx && server_env_idx < release_idx,
            "env overrides must run before release-asset fallback"
        );
    }

    #[test]
    fn install_script_release_assets_cover_both_binaries() {
        // Mirror release naming: one URL per binary per platform.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains("releases/latest/download/branchwork-runner-${os}-${arch}"),
            "must point at the runner release asset URL pattern"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains("releases/latest/download/branchwork-server-${os}-${arch}"),
            "must point at the server release asset URL pattern (T3.1)"
        );
    }

    #[test]
    fn install_script_docker_extracts_both_binaries() {
        // The :edge image carries both binaries under /usr/local/bin
        // (deploy/Dockerfile stage 3). A single container creation
        // yields both — reuse the same `docker create` instead of
        // paying the cost twice.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("docker cp \"$cid:/usr/local/bin/branchwork-runner\""),
            "docker extract must copy the runner from /usr/local/bin"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("docker cp \"$cid:/usr/local/bin/branchwork-server\""),
            "docker extract must copy the server from /usr/local/bin (T3.1)"
        );
        // Exactly one executable `docker create` to avoid double image
        // pull. Comments may mention the phrase too, so count only lines
        // that are not pure comments.
        let create_count = INSTALL_SCRIPT_TEMPLATE
            .lines()
            .filter(|l| !l.trim_start().starts_with('#') && l.contains("docker create"))
            .count();
        assert_eq!(
            create_count, 1,
            "expected exactly one executable `docker create` (got {create_count}); both binaries must come from the same container"
        );
    }

    #[test]
    fn install_script_launches_runner_with_install_dir_on_path() {
        // The runner's `which("branchwork-server")` resolver only finds
        // our paired binary if $INSTALL_DIR is on PATH at launch. nohup
        // inherits the operator's shell PATH, which on minimal hosts
        // may NOT include $HOME/.local/bin — prepend $INSTALL_DIR so
        // the lookup is deterministic regardless of dotfile state.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"PATH="$INSTALL_DIR:$PATH" nohup "$RUNNER_BIN""#),
            "must prepend $INSTALL_DIR to PATH on the nohup launch line"
        );
    }
}
