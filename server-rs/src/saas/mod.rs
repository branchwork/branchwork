//! SaaS foundation: remote agent execution via runners.
//!
//! The SaaS dashboard can't run user agents directly — that needs the user's
//! code and their Claude/Cursor credentials. Runners are lightweight binaries
//! that run on the customer's machine (or CI) and connect to the SaaS
//! dashboard via authenticated WebSocket.
//!
//! ## Modules
//!
//! - [`runner_protocol`] — wire-protocol types (events, commands, ACK)
//! - [`outbox`] — SQLite-backed outbox for at-least-once delivery
//! - [`runner_ws`] — server-side WebSocket handler + token management API

pub mod outbox;
pub mod runner_protocol;
pub mod runner_ws;
