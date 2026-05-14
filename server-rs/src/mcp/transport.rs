//! Transport glue: wraps [`BranchworkMcp`] in either a streamable-HTTP
//! service (mounted on the axum router) or a stdio session (read
//! line-delimited JSON-RPC from stdin, write to stdout).
//!
//! Both transports take the same [`McpContext`] so tool behaviour is
//! identical whichever wire is used.

use std::sync::Arc;

use rmcp::{
    ServiceExt,
    transport::{
        io::stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};

use super::{BranchworkMcp, McpContext};

pub type McpService = StreamableHttpService<BranchworkMcp, LocalSessionManager>;

pub fn build_http_service(ctx: McpContext) -> McpService {
    StreamableHttpService::new(
        move || Ok(BranchworkMcp::new(ctx.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(build_allowed_hosts()),
    )
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
