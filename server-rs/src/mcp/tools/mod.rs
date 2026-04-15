//! Tool implementations for the orchestrAI MCP server.
//!
//! Each submodule defines a `#[tool_router]` impl block on
//! [`super::OrchestrAiMcp`] with a named router; `OrchestrAiMcp::new` combines
//! them.

pub mod plans;
pub mod status;
