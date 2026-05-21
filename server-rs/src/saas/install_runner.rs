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
    fn install_script_banner_emits_start_session_line_after_runner_started() {
        // Ordering matters for the operator's reading flow: first they
        // see "* runner started (pid X)", then "* Start session will use:
        // …". Reversing the order would put the readiness verdict before
        // the runner actually has a pid, which would mislead an operator
        // skimming the tail of the install output.
        let started_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"ok "runner started (pid"#)
            .expect("runner-started line must remain in the script");
        let start_session_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"ok "Start session will use:"#)
            .expect("Start-session-will-use line must be emitted");
        assert!(
            started_at < start_session_at,
            "runner-started must precede Start-session-will-use in the banner"
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
    fn install_script_system_flag_requires_just_binary() {
        // --system is a modifier; using it alone is operator error and
        // must surface a clear refusal rather than silently no-op'ing.
        assert!(
            INSTALL_SCRIPT_TEMPLATE
                .contains(r#"err "--system is only meaningful with --just-binary""#),
            "must refuse --system without --just-binary"
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
    fn install_script_just_binary_skips_nohup_launch() {
        // The nohup launch path is for enroll/update/reset. just_binary
        // delegates to systemd and must NOT hit the nohup statement —
        // otherwise the script would start a second runner process
        // alongside the systemd unit and write a stale $PID_FILE.
        //
        // Pin this structurally: the `nohup "$RUNNER_BIN"` line must
        // live inside an `else` branch of a `[ "$MODE" = "just_binary" ]`
        // conditional. Grep for both halves.
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"if [ "$MODE" = "just_binary" ]; then"#),
            "lifecycle gate must branch on MODE=just_binary"
        );
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains(r#"PATH="$INSTALL_DIR:$PATH" nohup "$RUNNER_BIN""#),
            "the else branch must keep the original nohup launch"
        );
        let just_binary_branch = INSTALL_SCRIPT_TEMPLATE
            .find(r#"if [ "$MODE" = "just_binary" ]; then"#)
            .expect("just_binary lifecycle branch missing");
        let nohup_at = INSTALL_SCRIPT_TEMPLATE
            .find(r#"PATH="$INSTALL_DIR:$PATH" nohup"#)
            .expect("nohup launch line missing");
        assert!(
            just_binary_branch < nohup_at,
            "just_binary branch must come BEFORE the nohup fallback so the else clause guards it"
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
}
