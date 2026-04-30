//! Tool implementations for the Branchwork MCP server.
//!
//! Each submodule defines a `#[tool_router]` impl block on
//! [`super::BranchworkMcp`] with a named router; `BranchworkMcp::new` combines
//! them.

pub mod plans;
pub mod status;
