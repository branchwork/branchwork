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

/// T4.3: Sentinel the install script template carries on a top-level
/// comment line. The server substitutes `env!("CARGO_PKG_VERSION")` for
/// it on every GET so the runner's periodic poll can grep for the
/// server's currently-on-offer version without having to download a
/// binary first.
///
/// Lives outside the install-script TEMPLATE replacement loop because
/// the runner's parser greps for the literal prefix
/// `# BRANCHWORK_SERVER_VERSION=` (defined in
/// `bin/branchwork_runner.rs::version_poll::SERVER_VERSION_MARKER`) —
/// the template carries the same prefix with `__SERVER_VERSION__`
/// after the `=` sign so the runner sees the marker even before the
/// substitution fires.
pub const SERVER_VERSION_PLACEHOLDER: &str = "__SERVER_VERSION__";

/// T1.2 (runner-daemon-workspace): the install script renders the
/// systemd user-mode unit into `~/.config/systemd/user/`. The unit body
/// itself is the canonical template at `deploy/branchwork-runner.service.in`
/// — kept as a separate file so it lints with
/// `systemd-analyze --user verify` independently of the shell script.
///
/// The script carries this sentinel inside a quoted heredoc
/// (`<<'UNIT'`) so `%h` and other systemd specifiers survive verbatim
/// until the unit is loaded by systemd at start time.
pub const UNIT_TEMPLATE_PLACEHOLDER: &str = "__UNIT_TEMPLATE__";

/// Embedded copy of `deploy/install-runner.sh`. Compiled in so the binary
/// is self-contained — no on-disk asset path to worry about at runtime.
const INSTALL_SCRIPT_TEMPLATE: &str = include_str!("../../../deploy/install-runner.sh");

/// Embedded copy of `deploy/branchwork-runner.service.in` (T1.1's
/// user-mode unit template). Substituted into the install script at
/// render time so the wire form ships a single self-contained
/// `curl | sh` body. Source of truth lives in `deploy/` so
/// `systemd-analyze --user verify` runs against the same bytes the
/// runtime emits.
const UNIT_TEMPLATE: &str = include_str!("../../../deploy/branchwork-runner.service.in");

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

/// Render the install script with placeholders replaced. Pure function
/// so unit tests can drive it without a live server.
///
/// Substitutions performed (in order):
///   • `__SAAS_URL__`       → `saas_url`
///   • `__SERVER_VERSION__` → compile-time `CARGO_PKG_VERSION` (T4.3)
///   • `__UNIT_TEMPLATE__`  → contents of `branchwork-runner.service.in`
///     (T1.2 — the unit body lives in a separate file so it lints
///     with `systemd-analyze --user verify`).
///
/// The unit-template substitution lands inside a quoted heredoc
/// (`<<'UNIT'` in install-runner.sh) so the file's `%h` specifiers
/// survive verbatim. The substituted block also includes the file's
/// trailing newline; cat treats that as one extra empty line in the
/// output, which is harmless for systemd unit parsing.
pub fn render_install_script(template: &str, saas_url: &str) -> String {
    template
        .replace(SAAS_URL_PLACEHOLDER, saas_url)
        .replace(SERVER_VERSION_PLACEHOLDER, env!("CARGO_PKG_VERSION"))
        .replace(UNIT_TEMPLATE_PLACEHOLDER, unit_template_body())
}

/// Trim the trailing newline from the embedded unit-template file before
/// substituting it into the install script's heredoc. The .in file ends
/// with `WantedBy=default.target\n`; without trimming, the substituted
/// heredoc body would gain an extra blank line before the closing
/// `UNIT` terminator. Systemd is tolerant of trailing blank lines, but
/// trimming keeps the rendered script tidy and the byte-for-byte sync
/// tests easier to reason about.
fn unit_template_body() -> &'static str {
    UNIT_TEMPLATE.trim_end_matches('\n')
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
    fn rendered_script_carries_server_version_sentinel() {
        // T4.3 pin: the rendered script must contain the literal
        // `# BRANCHWORK_SERVER_VERSION=<semver>` line so the runner's
        // periodic poll can parse it. The semver must match the
        // server-binary's compile-time CARGO_PKG_VERSION (we
        // substitute `__SERVER_VERSION__` for `env!("CARGO_PKG_VERSION")`).
        let rendered = render_install_script(INSTALL_SCRIPT_TEMPLATE, "https://example.com");
        let expected_marker = format!("# BRANCHWORK_SERVER_VERSION={}", env!("CARGO_PKG_VERSION"));
        assert!(
            rendered.contains(&expected_marker),
            "rendered install-runner.sh must contain `{expected_marker}` so the runner's \
             version-poll can grep it; got first 200 chars: {first}",
            first = &rendered.chars().take(200).collect::<String>()
        );
        // The placeholder must be fully substituted — no `__SERVER_VERSION__`
        // remains anywhere in the rendered output.
        assert!(
            !rendered.contains(SERVER_VERSION_PLACEHOLDER),
            "render_install_script must replace every {SERVER_VERSION_PLACEHOLDER}"
        );
    }

    #[test]
    fn embedded_template_carries_server_version_placeholder() {
        // Guards against the script being rewritten without the
        // placeholder; the runner's periodic poll would then see a stale
        // server-version string baked in at build time.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(SERVER_VERSION_PLACEHOLDER),
            "install-runner.sh must embed {SERVER_VERSION_PLACEHOLDER} for the server to substitute"
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
        //
        // T3.3 wrapped the call in `if [ "$MODE" != "just_binary" ]` so
        // systemd-managed installs (where the runner PID lives outside
        // $PID_FILE) are not flagged. The indentation moved from
        // column-0 to four spaces; the contract is unchanged.
        let template = INSTALL_SCRIPT_TEMPLATE;
        let check_at = template
            .find("\n    check_foreign_runners\n")
            .or_else(|| template.find("\ncheck_foreign_runners\n"))
            .expect("check_foreign_runners must be invoked");
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
    fn install_script_user_unit_execstart_passes_server_bin_explicitly() {
        // T1.2 replaced the nohup launch with a systemd unit; the runner
        // now resolves `branchwork-server` via the explicit
        // `--server-bin %h/.local/bin/branchwork-server` flag rather than
        // the `which("branchwork-server")` fallback (systemd's user PATH
        // is not guaranteed to carry $HOME/.local/bin). The unit
        // template at deploy/branchwork-runner.service.in is the source
        // of truth; substituted into the script via
        // `__UNIT_TEMPLATE__`. Render the script and confirm the literal
        // flag survives into the on-disk unit body.
        let rendered = render_install_script(INSTALL_SCRIPT_TEMPLATE, "https://example.com");
        assert!(
            rendered.contains(
                "ExecStart=%h/.local/bin/branchwork-runner --server-bin %h/.local/bin/branchwork-server"
            ),
            "user-mode unit must pass --server-bin in ExecStart so the runner's branchwork-server lookup is deterministic under systemd"
        );
    }

    #[test]
    fn install_script_system_unit_execstart_passes_server_bin_explicitly() {
        // Same as the user-mode test above, but the system-mode unit
        // uses absolute paths (since %h would expand to /root without
        // an explicit User=) — pin the same --server-bin contract.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"ExecStart=$runner_home/.local/bin/branchwork-runner --server-bin $runner_home/.local/bin/branchwork-server"#
            ),
            "system-mode unit must pass --server-bin in ExecStart with absolute paths"
        );
    }

    // ── T3.2 dual-binary banner contract ────────────────────────────────
    //
    // The success banner must name both binaries with their versions and
    // surface "Start session will use: <path>" so the operator can
    // confirm dual-binary readiness before opening the dashboard. When
    // `branchwork-server --version` fails (older binary that predates
    // `#[command(version)]`, foreign-arch download, corrupted bytes),
    // the banner falls back to the verbatim T3.2 acceptance copy
    // "(could not verify — Start session will fail)".

    #[test]
    fn install_script_probes_both_binary_versions() {
        // Both probes must be wired (runner_version + server_version),
        // each `2>/dev/null | head -n1` so a stub that prints multiple
        // lines on stderr cannot derail the banner. `|| true` is the
        // belt-and-braces guard against POSIX `set -e` even though
        // command-substitution assignments do not trip the option.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"runner_version="$("$RUNNER_BIN" --version 2>/dev/null | head -n1 || true)""#
            ),
            "must probe `branchwork-runner --version` into $runner_version"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"server_version="$("$SERVER_BIN" --version 2>/dev/null | head -n1 || true)""#
            ),
            "must probe `branchwork-server --version` into $server_version"
        );
    }

    #[test]
    fn install_script_annotates_install_lines_with_versions() {
        // The two `installed …` lines render the version inline when the
        // probe succeeded — `${var:+ (...)}` is the POSIX-portable way to
        // omit the suffix when the probe failed (empty $var), which keeps
        // a successful runner install readable even if the server probe
        // failed (or vice versa).
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"ok "installed $RUNNER_BIN${runner_version:+ ($runner_version)}""#),
            "runner install line must conditionally render $runner_version"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"ok "installed $SERVER_BIN${server_version:+ ($server_version)}""#),
            "server install line must conditionally render $server_version"
        );
    }

    #[test]
    fn install_script_banner_announces_start_session_target() {
        // Brief: "Start session will use: /home/cpo/.local/bin/branchwork-server".
        // The path is interpolated from $SERVER_BIN and the version
        // appended in parentheses on the happy path. The literal phrase
        // "Start session will use:" must appear in both branches so the
        // dashboard runbook can grep for it.
        let starts = INSTALL_SCRIPT_TEMPLATE
            .matches(r#"ok "Start session will use: $SERVER_BIN"#)
            .count();
        assert_eq!(
            starts, 2,
            "must emit `Start session will use: $SERVER_BIN …` in BOTH the success and fallback branches (got {starts} matches)"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"ok "Start session will use: $SERVER_BIN ($server_version)""#),
            "happy-path banner must append ($server_version) to the resolved server path"
        );
    }

    #[test]
    fn install_script_banner_carries_could_not_verify_fallback() {
        // T3.2 acceptance criterion: when `branchwork-server --version`
        // fails, the banner literally says
        // "(could not verify — Start session will fail)". The em-dash
        // (U+2014) is the canonical separator — must match byte-for-byte
        // so the runbook's grep keeps working.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("(could not verify \u{2014} Start session will fail)"),
            "fallback message must read '(could not verify — Start session will fail)' verbatim"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ -n "$server_version" ]; then"#),
            "fallback must be gated on $server_version being non-empty"
        );
    }

    #[test]
    fn install_script_banner_emits_start_session_line_after_unit_enabled() {
        // T1.2 replaced "runner started (pid X)" with
        // "branchwork-runner.service enabled and running" — the systemd
        // unit owns the lifecycle now. Operator reading flow still
        // requires the readiness banner to precede the Start-session
        // verdict (otherwise the verdict would print before the runner
        // is actually up).
        let enabled_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"ok "branchwork-runner.service enabled and running""#)
            .expect("unit-enabled banner line must remain in the script");
        let start_session_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"ok "Start session will use:"#)
            .expect("Start-session-will-use line must be emitted");
        assert!(
            enabled_at < start_session_at,
            "unit-enabled banner must precede Start-session-will-use line"
        );
    }

    // ── T3.3 --just-binary in-place upgrade contract ────────────────────
    //
    // install-runner.sh must expose a binary-swap-only mode for hosts
    // where systemd owns the runner. The mode:
    //   • parses `--just-binary` (and `--upgrade` alias) and an optional
    //     `--system` modifier;
    //   • refuses on missing config.toml (no enroll, only upgrade);
    //   • is mutually exclusive with --reset, --rotate-token, --force-replace;
    //   • compares old and new binary versions BEFORE the mv and exits 0
    //     with the canonical "already at vX.Y.Z — nothing to do" line
    //     when both binaries already match;
    //   • restarts the systemd unit (`systemctl --user restart
    //     branchwork-runner` by default, `sudo systemctl restart
    //     branchwork-runner` with --system) instead of launching the
    //     runner via nohup;
    //   • never writes $CONFIG_FILE so the existing token + saas_url
    //     stay byte-for-byte identical.

    #[test]
    fn install_script_carries_just_binary_flag() {
        // Both the canonical and alias forms must be parseable.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("--just-binary|--upgrade)"),
            "install-runner.sh must accept --just-binary and --upgrade as aliases"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("JUST_BINARY=0"),
            "install-runner.sh must initialise JUST_BINARY"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("--just-binary [--system]"),
            "usage banner must list the --just-binary form with the --system modifier"
        );
    }

    #[test]
    fn install_script_just_binary_validates_args() {
        // No token, no --reset, no --rotate-token, no --force-replace.
        // Each refusal must be wired so a future edit can't silently
        // re-introduce one of the mutually-exclusive combinations.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"err "--just-binary takes no token (it does not enroll, only swaps binaries)""#
            ),
            "must refuse a positional token under --just-binary"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--just-binary and --reset are mutually exclusive""#),
            "must refuse --just-binary + --reset"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--just-binary and --rotate-token are mutually exclusive""#),
            "must refuse --just-binary + --rotate-token"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--just-binary and --force-replace are mutually exclusive""#),
            "must refuse --just-binary + --force-replace"
        );
    }

    #[test]
    fn install_script_just_binary_requires_existing_install() {
        // Missing $CONFIG_FILE is a hard error — the brief is explicit
        // that just-binary mode "does not enroll, it only swaps binaries
        // on an existing install".
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--just-binary requires an existing install at $CONFIG_FILE""#),
            "must refuse --just-binary on a host with no existing install"
        );
    }

    #[test]
    fn install_script_just_binary_requires_systemctl() {
        // The mode is for systemd-managed installs (see
        // [[runner-daemon-workspace]]); a host without systemctl
        // cannot satisfy the restart step, so fail fast with a
        // pointer to the right plan.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--just-binary requires systemctl (host has no systemd)""#),
            "must refuse --just-binary on a host with no systemctl"
        );
    }

    #[test]
    fn install_script_system_flag_requires_root_for_enroll() {
        // T1.2 lifted the `--system is only meaningful with --just-binary`
        // gate so --system now works alongside enroll/update/reset (it
        // writes the unit to /etc/systemd/system/ instead of
        // ~/.config/systemd/user/). That path needs root; the script
        // must refuse unprivileged invocations with a clear message.
        // --just-binary --system stays unprivileged because it delegates
        // the restart to `sudo systemctl restart`.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--system requires root (use sudo or run as root)""#),
            "must refuse non-root --system for enroll/update/reset modes"
        );
        // The gate must NOT fire for --just-binary --system (that's the
        // operator's pre-T1.2 entry point and stays unprivileged).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"if [ "$SYSTEM_INSTALL" = "1" ] && [ "$MODE" != "just_binary" ]; then"#
            ),
            "root-check must skip MODE=just_binary (delegates to sudo systemctl)"
        );
        // Negative contract: the prior `--system is only meaningful` gate
        // must be gone.
        assert!(
            !INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--system is only meaningful with --just-binary""#),
            "the prior just-binary-only gate on --system must be lifted by T1.2"
        );
    }

    #[test]
    fn install_script_just_binary_does_not_write_config_toml() {
        // The config-write gate must NOT include MODE=just_binary, so a
        // dashboard one-click Upgrade cannot accidentally clobber the
        // stored token or saas_url. The acceptance criterion is verbatim:
        // "leaves ~/.branchwork-runner/config.toml byte-for-byte
        // identical to before".
        let gate = INSTALL_SCRIPT_TEMPLATE
            .find(r#"if [ "$MODE" = "enroll" ] || [ "$MODE" = "reset" ] || [ "$ROTATE_TOKEN" = "1" ]"#)
            .expect("config-write gate must remain on enroll/reset/rotate only");
        let cat_at = INSTALL_SCRIPT_TEMPLATE[gate..]
            .find(r#"cat > "$CONFIG_FILE""#)
            .expect("config-write must be the next statement after the gate");
        // The phrase "just_binary" must not appear inside the gate or
        // the cat heredoc block — guard against a future edit that
        // accidentally re-includes the mode in the write path.
        let cat_abs = gate + cat_at;
        let block = &INSTALL_SCRIPT_TEMPLATE[gate..cat_abs];
        assert!(
            !block.contains("just_binary"),
            "config-write gate must not mention just_binary (the mode never writes $CONFIG_FILE)"
        );
    }

    #[test]
    fn install_script_just_binary_idempotency_message() {
        // Acceptance criterion: "already at v0.5.X — nothing to do".
        // The em-dash (U+2014) is the canonical separator — matched
        // byte-for-byte by the runbook grep + this test.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                "ok \"already at v$(short_version \"$current_runner_version\") \u{2014} nothing to do\""
            ),
            "idempotency banner must read 'already at vX.Y.Z — nothing to do' verbatim"
        );
        // The check must compare BOTH binary versions (not just the
        // runner) — a server-only drift would otherwise silently skip
        // the upgrade. The two-binary-AND gate is the pin.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"current_runner_version="$("$RUNNER_BIN" --version"#),
            "must probe the on-disk runner version before deciding to swap"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"current_server_version="$("$SERVER_BIN" --version"#),
            "must probe the on-disk server version before deciding to swap"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"new_runner_version="$("$TMP_RUNNER" --version"#),
            "must probe the new runner version (in $TMP_RUNNER) before deciding to swap"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"new_server_version="$("$TMP_SERVER" --version"#),
            "must probe the new server version (in $TMP_SERVER) before deciding to swap"
        );
    }

    #[test]
    fn install_script_just_binary_idempotency_runs_before_mv() {
        // The idempotency `exit 0` must fire BEFORE the binaries are
        // moved into place — otherwise a no-op upgrade would still
        // pointlessly overwrite the live binaries (harmless today but
        // would trip systemctl's binary-changed detection on some
        // distros, and adds disk churn).
        let exit_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"ok "already at v$(short_version "#)
            .expect("idempotency exit branch must exist");
        let mv_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"mv "$TMP_RUNNER" "$RUNNER_BIN""#)
            .expect("final-path mv must remain");
        assert!(
            exit_at < mv_at,
            "idempotency check must run before the binary swap (exit_at={exit_at} mv_at={mv_at})"
        );
    }

    #[test]
    fn install_script_just_binary_restarts_systemd_user_mode() {
        // Default --just-binary lifecycle is user-mode systemd.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("systemctl --user restart branchwork-runner"),
            "just_binary default must `systemctl --user restart branchwork-runner`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"ok "restarted branchwork-runner via systemd""#),
            "just_binary must announce the successful systemd restart"
        );
    }

    #[test]
    fn install_script_just_binary_system_mode_uses_sudo() {
        // --system flips to root systemd. The literal sudo prefix
        // matters because the runbook + dashboard upgrade button rely
        // on it: the operator opts into sudo by setting --system,
        // never implicitly.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("sudo systemctl restart branchwork-runner"),
            "just_binary + --system must `sudo systemctl restart branchwork-runner`"
        );
        // The branch gate must be on SYSTEM_INSTALL=1 so a future
        // edit can't accidentally fall through to user-mode under
        // --system.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ "$SYSTEM_INSTALL" = "1" ]; then"#),
            "must branch on SYSTEM_INSTALL=1 for the sudo path"
        );
    }

    #[test]
    fn install_script_has_no_nohup_launch_after_t1_2() {
        // T1.2 replaced the nohup launch with a systemd unit for ALL
        // non-just_binary modes. The `PATH="$INSTALL_DIR:$PATH" nohup
        // "$RUNNER_BIN"` line must be gone — leaving it in place would
        // race the systemd unit (two runner processes, one stale
        // PID_FILE). The lifecycle gate on `MODE=just_binary` is still
        // there but its `else` branch now runs systemctl, not nohup.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ "$MODE" = "just_binary" ]; then"#),
            "lifecycle gate must branch on MODE=just_binary"
        );
        // Negative contract: no nohup launch anywhere in executable code.
        let has_nohup = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("nohup \"$RUNNER_BIN\"")
        });
        assert!(
            !has_nohup,
            "post-T1.2 install-runner.sh must NOT contain `nohup \"$RUNNER_BIN\"` as executable code"
        );
        // Companion negative: no `echo $! > "$PID_FILE"` either — that
        // line stamped the nohup PID for the legacy lifecycle path.
        let has_pidfile_stamp = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains(r#"echo $! > "$PID_FILE""#)
        });
        assert!(
            !has_pidfile_stamp,
            "post-T1.2 install-runner.sh must NOT stamp a PID into $PID_FILE — systemd owns the lifecycle"
        );
    }

    #[test]
    fn install_script_just_binary_skips_stop_existing_runner() {
        // stop_existing_runner targets $PID_FILE, which is only written
        // by the nohup-launch path. systemd-managed installs never
        // populate it, so calling stop_existing_runner is at best a
        // no-op and at worst confusing (it might find a stale PID from
        // a prior nohup install and try to TERM it).
        // The gate must restrict the call to update + reset only.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains("case \"$MODE\" in\n    update|reset) stop_existing_runner ;;"),
            "stop_existing_runner must only run for MODE in {{update, reset}} — just_binary is systemd's job"
        );
        // Negative contract: the prior `if [ "$MODE" != "enroll" ]`
        // gate (which would have included just_binary) must be gone.
        assert!(
            !INSTALL_SCRIPT_TEMPLATE.contains(
                r#"if [ "$MODE" != "enroll" ]; then
    stop_existing_runner
fi"#
            ),
            "the legacy `MODE != enroll` gate must be replaced with the explicit case"
        );
    }

    #[test]
    fn install_script_just_binary_skips_foreign_runner_check() {
        // Foreign-runner detection uses $PID_FILE to subtract our own
        // managed PID from the candidate list. systemd-managed installs
        // don't write that file, so the check would always flag the
        // systemd runner as foreign — false positive. Skip it under
        // --just-binary.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                "if [ \"$MODE\" != \"just_binary\" ]; then\n    check_foreign_runners\nfi"
            ),
            "check_foreign_runners must be gated to skip MODE=just_binary"
        );
    }

    #[test]
    fn install_script_just_binary_completion_line() {
        // The completion case must carry a just_binary arm so the
        // operator-visible verdict reads as a successful upgrade rather
        // than a generic "OK". Wording mirrors update/reset for
        // consistency with the dashboard runbook grep.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"just_binary) ok "upgraded binaries in place (config untouched)" ;;"#),
            "case `$MODE` completion line must include the just_binary arm"
        );
    }

    #[test]
    fn install_script_just_binary_next_steps_uses_systemctl() {
        // The trailing Next-steps block must point at systemctl/journalctl
        // for the systemd path, not the PID_FILE/LOG_FILE references that
        // only apply to the nohup-launch modes. Otherwise the operator
        // sees "kill $(cat $PID_FILE)" which has no relation to the
        // systemd unit they actually need to manage.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"unit_status_cmd="systemctl --user status branchwork-runner""#),
            "user-mode next-steps must reference `systemctl --user status branchwork-runner`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"unit_status_cmd="sudo systemctl status branchwork-runner""#),
            "system-mode next-steps must reference `sudo systemctl status branchwork-runner`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains("Binary upgrade only \u{2014} the existing systemd unit and"),
            "just_binary next-steps banner must announce config-preservation"
        );
    }

    // ── T1.2 systemd-lifecycle contract ─────────────────────────────────
    //
    // install-runner.sh enroll/update/reset modes own the systemd unit
    // lifecycle (T1.2 replaced the nohup launch). The script must:
    //   • render the unit from deploy/branchwork-runner.service.in via
    //     the __UNIT_TEMPLATE__ placeholder, into
    //     ~/.config/systemd/user/branchwork-runner.service (user mode)
    //     or /etc/systemd/system/branchwork-runner.service (--system).
    //   • write a sidecar env file at ~/.branchwork-runner/env carrying
    //     BRANCHWORK_SAAS_URL + BRANCHWORK_RUNNER_TOKEN in KEY=VALUE
    //     syntax (so the unit's EnvironmentFile= can source it).
    //   • daemon-reload + enable --now the unit.
    //   • user mode: also `loginctl enable-linger "$USER"` so the
    //     runner survives logouts and reboots.
    //   • migrate from pre-T1.2 nohup installs by SIGTERMing any
    //     legacy $PID_FILE-tracked process before the unit comes up.

    #[test]
    fn embedded_template_carries_unit_template_sentinel() {
        // Guard: install-runner.sh must embed __UNIT_TEMPLATE__ so the
        // server has something to substitute. Without this, the
        // user-mode heredoc would have an empty body and `systemctl
        // --user daemon-reload` would later fail with "service file
        // empty".
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(UNIT_TEMPLATE_PLACEHOLDER),
            "install-runner.sh must embed {UNIT_TEMPLATE_PLACEHOLDER} for the server to substitute"
        );
    }

    #[test]
    fn embedded_unit_template_file_is_nonempty_and_has_install_section() {
        // Sanity check on deploy/branchwork-runner.service.in itself:
        // present, non-trivial, and carries the minimal directives the
        // T1.1 acceptance pinned (Type=simple, Restart=on-failure,
        // [Install] section). If a future edit accidentally truncates
        // the file, this test fires before the substitution path goes
        // out to operators.
        assert!(
            UNIT_TEMPLATE.len() > 200,
            "unit template suspiciously short"
        );
        assert!(
            UNIT_TEMPLATE.contains("[Unit]")
                && UNIT_TEMPLATE.contains("[Service]")
                && UNIT_TEMPLATE.contains("[Install]"),
            "unit template must carry the three core sections"
        );
        assert!(
            UNIT_TEMPLATE.contains("Type=simple"),
            "unit template must declare Type=simple"
        );
        assert!(
            UNIT_TEMPLATE.contains("Restart=on-failure"),
            "unit template must declare Restart=on-failure"
        );
        assert!(
            UNIT_TEMPLATE.contains("WantedBy=default.target"),
            "unit template must declare WantedBy=default.target (user mode)"
        );
    }

    #[test]
    fn rendered_script_substitutes_unit_template() {
        // After render, the placeholder must be gone everywhere — a
        // stray `__UNIT_TEMPLATE__` would mean systemd sees the literal
        // string as a unit body and refuses to load.
        let rendered = render_install_script(INSTALL_SCRIPT_TEMPLATE, "https://example.com");
        assert!(
            !rendered.contains(UNIT_TEMPLATE_PLACEHOLDER),
            "render_install_script must replace every {UNIT_TEMPLATE_PLACEHOLDER}"
        );
    }

    #[test]
    fn rendered_script_carries_unit_template_body() {
        // The substituted body must include the canonical directives
        // from the .in file. Asserting on a handful of lines (rather
        // than the whole file) makes this test resilient to whitespace
        // tweaks in the .in template while still catching catastrophic
        // failures (empty substitution, wrong content).
        let rendered = render_install_script(INSTALL_SCRIPT_TEMPLATE, "https://example.com");
        assert!(
            rendered.contains("[Unit]"),
            "rendered script must contain the [Unit] section header"
        );
        assert!(
            rendered.contains("Description=Branchwork SaaS runner (user mode)"),
            "rendered script must carry the canonical Description= line"
        );
        assert!(
            rendered.contains(
                "ExecStart=%h/.local/bin/branchwork-runner --server-bin %h/.local/bin/branchwork-server"
            ),
            "rendered script must carry the unit's ExecStart line verbatim"
        );
        assert!(
            rendered.contains("EnvironmentFile=%h/.branchwork-runner/env"),
            "rendered script must carry the EnvironmentFile= directive"
        );
        assert!(
            rendered.contains("WantedBy=default.target"),
            "rendered script must carry the WantedBy= directive"
        );
    }

    #[test]
    fn install_script_writes_user_unit_via_quoted_heredoc() {
        // The user-mode unit is written via `cat > ... <<'UNIT'` so the
        // body's `%h` specifiers survive verbatim until systemd
        // expands them at unit-load time. An unquoted heredoc would
        // let the shell try to expand `%h` (which produces empty
        // string in POSIX sh) and corrupt the unit content.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"cat > "$user_unit_file" <<'UNIT'"#),
            "user unit must be written via a SINGLE-QUOTED heredoc (<<'UNIT')"
        );
        // And the target path must be inside the systemd user-config
        // dir — not /etc/systemd/system/ which would be system mode.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"user_unit_dir="$HOME/.config/systemd/user""#),
            "user-mode unit dir must be ~/.config/systemd/user"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"user_unit_file="$user_unit_dir/branchwork-runner.service""#),
            "user-mode unit file must be branchwork-runner.service under the user dir"
        );
    }

    #[test]
    fn install_script_writes_system_unit_with_user_directive() {
        // System mode writes the unit inline (not via the .in
        // substitution) because the User= / Group= / WorkingDirectory=
        // / absolute-path differences are mode-specific. Pin the
        // canonical directives so a future edit can't silently drop
        // any of them.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"system_unit_file="/etc/systemd/system/branchwork-runner.service""#),
            "system-mode unit path must be /etc/systemd/system/branchwork-runner.service"
        );
        // User= and Group= must use the install user (SUDO_USER or USER).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("User=$runner_user"),
            "system unit must declare User=<install user>"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("Group=$runner_user"),
            "system unit must declare Group=<install user>"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("WorkingDirectory=$runner_home"),
            "system unit must set WorkingDirectory= to the user's home"
        );
        // System mode pulls in multi-user.target so it starts at boot.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("WantedBy=multi-user.target"),
            "system unit must declare WantedBy=multi-user.target"
        );
        // EnvironmentFile must use an absolute path under $runner_home
        // (system units cannot rely on %h expanding correctly without
        // an explicit User=, even with User= it expands at load time).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("EnvironmentFile=$runner_home/.branchwork-runner/env"),
            "system unit must EnvironmentFile= from the user's home"
        );
    }

    #[test]
    fn install_script_writes_sidecar_env_file() {
        // The runner reads BRANCHWORK_SAAS_URL + BRANCHWORK_RUNNER_TOKEN
        // via clap's env=, so the systemd unit needs a KEY=VALUE file
        // to source via EnvironmentFile=. The env file lives alongside
        // config.toml (same dir, same 0700 chmod) and is rewritten on
        // every install run except just_binary (which preserves the
        // existing config + env).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"cat > "$CONFIG_DIR/env" <<ENV"#),
            "must write $CONFIG_DIR/env via a heredoc"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("BRANCHWORK_SAAS_URL=$SAAS_URL"),
            "env file must export BRANCHWORK_SAAS_URL"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("BRANCHWORK_RUNNER_TOKEN=$TOKEN"),
            "env file must export BRANCHWORK_RUNNER_TOKEN"
        );
        // The unquoted heredoc (<<ENV) is INTENTIONAL — we want
        // $SAAS_URL and $TOKEN expanded inside.
        assert!(
            !INSTALL_SCRIPT_TEMPLATE.contains(r#"cat > "$CONFIG_DIR/env" <<'ENV'"#),
            "env-file heredoc must be unquoted so $SAAS_URL / $TOKEN expand"
        );
        // just_binary mode does NOT write the env file — the existing
        // env file alongside the existing config.toml is preserved.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ "$MODE" != "just_binary" ]; then"#),
            "env-file write must be gated to skip MODE=just_binary"
        );
    }

    #[test]
    fn install_script_runs_systemctl_daemon_reload_and_enable_now() {
        // T1.2 acceptance: a fresh `curl … | sh -s -- <token>` install
        // must leave a running unit. The two commands that do that are
        // `systemctl --user daemon-reload` (pick up the new unit file)
        // and `systemctl --user enable --now branchwork-runner` (start
        // + enable for autostart). Both must be present for both user
        // and system modes.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("systemctl --user daemon-reload"),
            "user mode must run `systemctl --user daemon-reload`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("systemctl --user enable --now branchwork-runner"),
            "user mode must run `systemctl --user enable --now branchwork-runner`"
        );
        // System mode: same two but without --user (root is the
        // systemd manager).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("systemctl daemon-reload"),
            "system mode must run `systemctl daemon-reload`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("systemctl enable --now branchwork-runner"),
            "system mode must run `systemctl enable --now branchwork-runner`"
        );
    }

    #[test]
    fn install_script_enables_user_linger() {
        // Acceptance: the runner must survive logouts/reboots. For
        // user-mode units that means `loginctl enable-linger "$USER"`
        // — without it, the user systemd instance stops when the last
        // login session ends, taking the runner with it.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"loginctl enable-linger "$USER""#),
            "user mode must call `loginctl enable-linger \"$USER\"`"
        );
        // Linger must be best-effort: locked-down corporate hosts
        // refuse linger and we still want the install to succeed
        // (runner just stops on logout, which is fine for interactive
        // sessions).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("loginctl enable-linger failed"),
            "linger failure must downgrade to a note (not exit 1)"
        );
        // System mode does NOT need linger — system units run
        // regardless of login state. Pin this structurally: the linger
        // CALL must live AFTER write_user_unit's invocation (the user
        // arm of the lifecycle if/else), so the system arm cannot fall
        // through into it. Use the executable-call pattern
        // `if ! loginctl enable-linger "$USER"` so we skip the
        // comment-block mentions earlier in the script.
        let linger_call_idx = INSTALL_SCRIPT_TEMPLATE
            .find(r#"if ! loginctl enable-linger "$USER""#)
            .expect("linger call must exist as executable code");
        let write_user_call_idx = INSTALL_SCRIPT_TEMPLATE
            .find("\n        write_user_unit\n")
            .expect("write_user_unit call must exist (8-space indent in else branch)");
        assert!(
            write_user_call_idx < linger_call_idx,
            "linger call must come AFTER write_user_unit (it lives in the same user-mode branch)"
        );
        // Defensive: the linger CALL must NOT live anywhere near
        // write_system_unit (the system-mode arm).
        let write_system_call_idx = INSTALL_SCRIPT_TEMPLATE
            .find("\n        write_system_unit\n")
            .expect("write_system_unit call must exist (8-space indent in if branch)");
        assert!(
            write_system_call_idx < linger_call_idx,
            "write_system_unit must come before the linger call (system arm runs first)"
        );
        // And there must be only ONE linger CALL (no duplicate in the
        // system arm).
        let linger_call_count = INSTALL_SCRIPT_TEMPLATE
            .matches(r#"if ! loginctl enable-linger "$USER""#)
            .count();
        assert_eq!(
            linger_call_count, 1,
            "exactly one executable `loginctl enable-linger \"$USER\"` call; got {linger_call_count}"
        );
    }

    #[test]
    fn install_script_requires_systemctl_for_enroll() {
        // T1.2 dropped the non-systemd fallback (the previous nohup
        // launch). enroll/update/reset on a host without systemctl
        // must fail fast with a clear pointer.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "branchwork-runner needs systemd (no systemctl on PATH)""#),
            "ensure_systemctl must emit the canonical no-systemd error"
        );
        // The check must run BEFORE write_user_unit / write_system_unit
        // — calling them on a non-systemd host would write dead files.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("ensure_systemctl"),
            "ensure_systemctl must be invoked"
        );
    }

    #[test]
    fn install_script_migrates_legacy_nohup_via_pidfile() {
        // Pre-T1.2 installs launched the runner via nohup and stamped
        // its PID into $PID_FILE. The T1.2 install path must detect
        // and stop that legacy process before bringing the unit up so
        // we don't end up with two runners competing for the same
        // SaaS slot. The function is called only for non-just_binary
        // modes (just_binary already ran under systemd).
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("stop_legacy_nohup_runner"),
            "must invoke stop_legacy_nohup_runner during the systemd-lifecycle path"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(
                r#"note "migrating from legacy nohup runner (pid $pid \u{2192} systemd)""#
            ) || INSTALL_SCRIPT_TEMPLATE
                .contains(r#"note "migrating from legacy nohup runner (pid $pid → systemd)""#),
            "must announce the legacy-PID migration so operators see the handover"
        );
    }

    #[test]
    fn install_script_unit_enabled_banner_announces_success() {
        // The success banner for enroll/update/reset must include the
        // canonical "branchwork-runner.service enabled and running"
        // line — that string is what the dashboard runbook (and the
        // T6.4 deploy workflow) greps for.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"ok "branchwork-runner.service enabled and running""#),
            "must emit the canonical 'enabled and running' success line"
        );
    }

    #[test]
    fn install_script_next_steps_uses_systemctl_for_enroll() {
        // The trailing Next-steps block for enroll/update/reset modes
        // must reference systemctl + journalctl (not tail -f $LOG_FILE
        // and kill $(cat $PID_FILE) which were the pre-T1.2 nohup
        // affordances). System and user modes get different commands.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"unit_stop_cmd="systemctl --user stop branchwork-runner""#),
            "user-mode next-steps must reference `systemctl --user stop branchwork-runner`"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"unit_stop_cmd="systemctl stop branchwork-runner""#),
            "system-mode next-steps must reference `systemctl stop branchwork-runner`"
        );
        // Negative contract: the legacy "tail -f $LOG_FILE" /
        // "kill $(cat $PID_FILE)" lines must be gone outside comments.
        let has_tail_log = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("tail -f $LOG_FILE")
        });
        assert!(
            !has_tail_log,
            "post-T1.2 next-steps must not reference `tail -f $LOG_FILE`"
        );
        let has_kill_pidfile = INSTALL_SCRIPT_TEMPLATE.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#') && t.contains("kill \\$(cat $PID_FILE)")
        });
        assert!(
            !has_kill_pidfile,
            "post-T1.2 next-steps must not reference `kill $(cat $PID_FILE)`"
        );
    }
}
