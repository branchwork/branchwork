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

use super::{McpContext, BranchworkMcp};

pub type McpService = StreamableHttpService<BranchworkMcp, LocalSessionManager>;

pub fn build_http_service(ctx: McpContext) -> McpService {
    StreamableHttpService::new(
        move || Ok(BranchworkMcp::new(ctx.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
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
