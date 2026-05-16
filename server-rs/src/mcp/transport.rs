//! Transport glue: wraps [`BranchworkMcp`] in either a streamable-HTTP
//! service (mounted on the axum router) or a stdio session (read
//! line-delimited JSON-RPC from stdin, write to stdout).
//!
//! Both transports take the same [`McpContext`] so tool behaviour is
//! identical whichever wire is used.

use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ServiceExt,
    transport::{
        io::stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
            session::local::{LocalSessionManager, SessionConfig},
        },
    },
};

use super::{BranchworkMcp, McpContext};

pub type McpService = StreamableHttpService<BranchworkMcp, LocalSessionManager>;

/// Default idle window before rmcp tears down an MCP session worker.
///
/// rmcp's `SessionConfig::DEFAULT_KEEP_ALIVE` is 5 minutes, which is
/// fatal for Branchwork: a single task agent often sits between
/// MCP calls for much longer (the model thinks, the user reads, the
/// CI run completes). Past 5 min the session worker exits with
/// `KeepAliveTimeout`, the spawned task in rmcp's tower calls
/// `close_session`, the next client request 404s with
/// "Session not found", and our spawned agents lose all their
/// in-flight MCP state until they reinit (which not every client
/// does cleanly).
///
/// Four hours is the sweet spot for current usage: long enough to
/// cover an unattended task chain or a user-driven debugging session
/// without losing context, short enough that a truly orphaned session
/// (browser tab closed, runner died) is still GC'd the same day. The
/// env override lets ops crank it further on long-running boxes
/// without a rebuild, or shorten it on memory-tight ones.
const DEFAULT_MCP_KEEP_ALIVE_SECS: u64 = 4 * 60 * 60;

pub fn build_http_service(ctx: McpContext) -> McpService {
    StreamableHttpService::new(
        move || Ok(BranchworkMcp::new(ctx.clone())),
        Arc::new(build_session_manager()),
        StreamableHttpServerConfig::default().with_allowed_hosts(build_allowed_hosts()),
    )
}

/// Build a `LocalSessionManager` whose per-session idle timeout reflects
/// Branchwork's reality (long-running task agents) instead of rmcp's
/// browser-tab-style default. Reads
/// `BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS` for an integer override;
/// invalid / unset → [`DEFAULT_MCP_KEEP_ALIVE_SECS`].
fn build_session_manager() -> LocalSessionManager {
    let mut session_config = SessionConfig::default();
    session_config.keep_alive = Some(Duration::from_secs(resolve_keep_alive_secs()));
    let mut mgr = LocalSessionManager::default();
    mgr.session_config = session_config;
    mgr
}

fn resolve_keep_alive_secs() -> u64 {
    std::env::var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MCP_KEEP_ALIVE_SECS)
}

/// rmcp 1.4 ships a DNS-rebinding defence: `StreamableHttpServerConfig`
/// defaults `allowed_hosts` to `["localhost", "127.0.0.1", "::1"]` and
/// returns 403 `Forbidden: Host header is not allowed` for any inbound
/// request whose `Host` header isn't in that list (rmcp tower.rs:72).
/// That's exactly right for a laptop deploy and exactly wrong for prod:
/// agents on the runner POST to `https://branchwork.dev/mcp`, the Host
/// header becomes `branchwork.dev`, and the MCP call 403s before any
/// tool handler runs — so `update_task_status` looked to the agent like
/// "the response never arrived" (T5.20 root cause).
///
/// Read `BRANCHWORK_PUBLIC_URL`, parse out the authority (host[:port]),
/// and add it to the allowlist alongside the loopback defaults. Empty
/// / unset → loopback-only, same as the rmcp default — local-dev and
/// standalone deploys behave identically to pre-T5.20. Invalid URLs
/// land in the allowlist verbatim only if they look authority-ish;
/// completely broken values are silently dropped so a typo can't open
/// the allowlist to arbitrary input.
fn build_allowed_hosts() -> Vec<String> {
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Ok(raw) = std::env::var("BRANCHWORK_PUBLIC_URL")
        && let Some(authority) = extract_authority(&raw)
    {
        hosts.push(authority);
    }
    hosts
}

/// Extract the `host[:port]` chunk from a URL string. Strips the scheme
/// (`https://`), the path (`/mcp`), and any trailing slash so the host
/// matches what rmcp's `NormalizedAuthority` parser compares against.
/// Returns `None` when the input doesn't look like an absolute URL.
fn extract_authority(url: &str) -> Option<String> {
    let after_scheme = url
        .find("://")
        .map(|i| &url[i + 3..])
        .unwrap_or(url.trim_start_matches('/'));
    let authority = after_scheme.split('/').next()?.trim();
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// Serve one MCP session over stdin/stdout until the client disconnects.
///
/// In stdio mode only protocol bytes may appear on stdout; logs must go
/// to stderr. Callers are responsible for not writing to stdout
/// themselves while this future is running.
pub async fn run_stdio(ctx: McpContext) -> Result<(), Box<dyn std::error::Error>> {
    let service = BranchworkMcp::new(ctx).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_authority_strips_scheme_and_path() {
        assert_eq!(
            extract_authority("https://branchwork.dev/mcp"),
            Some("branchwork.dev".into())
        );
        assert_eq!(
            extract_authority("https://app.branchwork.dev:8443/"),
            Some("app.branchwork.dev:8443".into())
        );
        assert_eq!(
            extract_authority("http://localhost:3100"),
            Some("localhost:3100".into())
        );
    }

    #[test]
    fn extract_authority_rejects_empty_input() {
        assert_eq!(extract_authority(""), None);
        assert_eq!(extract_authority("/"), None);
        assert_eq!(extract_authority("https:///"), None);
    }

    #[test]
    fn keep_alive_defaults_to_four_hours_when_env_unset() {
        // SAFETY: env vars are process-wide and unsafe to mutate from
        // multiple threads. Cargo-test default is single-thread per
        // binary, so the test serialises against the override test
        // below by virtue of running in the same thread.
        unsafe { std::env::remove_var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS") };
        assert_eq!(resolve_keep_alive_secs(), 4 * 60 * 60);
    }

    #[test]
    fn keep_alive_env_override_wins() {
        unsafe { std::env::set_var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS", "60") };
        assert_eq!(resolve_keep_alive_secs(), 60);
        unsafe { std::env::remove_var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS") };
    }

    #[test]
    fn keep_alive_ignores_invalid_or_zero_override() {
        // Zero and garbage both fall back to the default — zero would
        // disable the GC entirely, garbage means the operator typoed.
        for bad in ["0", "not-a-number", "-1"] {
            unsafe { std::env::set_var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS", bad) };
            assert_eq!(
                resolve_keep_alive_secs(),
                4 * 60 * 60,
                "expected default for {bad:?}",
            );
        }
        unsafe { std::env::remove_var("BRANCHWORK_MCP_SESSION_KEEP_ALIVE_SECS") };
    }

    /// Pins the rmcp 1.4 default extension: loopback always present so
    /// local-dev / standalone Self-test endpoints don't break, plus the
    /// public-URL host when set. Reverting T5.20 to
    /// `StreamableHttpServerConfig::default()` fails this — the prod
    /// host disappears from the allowlist and `update_task_status`
    /// from the runner-side claude 403s again.
    #[test]
    fn build_allowed_hosts_adds_public_url_authority() {
        // SAFETY: env vars are process-wide and unsafe to mutate from
        // multiple threads. Accept the cargo-test-default
        // single-thread-per-binary assumption here.
        unsafe { std::env::set_var("BRANCHWORK_PUBLIC_URL", "https://branchwork.dev/") };
        let hosts = build_allowed_hosts();
        assert!(hosts.contains(&"localhost".to_string()));
        assert!(hosts.contains(&"127.0.0.1".to_string()));
        assert!(hosts.contains(&"branchwork.dev".to_string()));
        unsafe { std::env::remove_var("BRANCHWORK_PUBLIC_URL") };
    }
}
