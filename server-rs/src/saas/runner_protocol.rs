//! Wire protocol for runner <-> SaaS communication.
//!
//! Every message is JSON-serialized and wrapped in a [`WireMessage`] tagged
//! union. Reliable (outbox-backed) messages carry a monotonically increasing
//! `seq` per sender; best-effort messages (terminal I/O) carry `seq: null`.
//!
//! This module is **self-contained** — no `crate::` dependencies — so it can
//! be `#[path]`-included by the standalone `orchestrai_runner` binary.

use serde::{Deserialize, Serialize};

// ── Envelope ────────────────────────────────────────────────────────────────

/// Top-level frame on the WebSocket. Every message is an `Envelope`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Monotonic sequence number assigned by the sender's outbox.
    /// `None` for best-effort messages (terminal I/O, pong).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Identifies the runner. Set on every frame so the SaaS side can
    /// demux without relying on connection-level state.
    pub runner_id: String,
    /// The actual message payload.
    #[serde(flatten)]
    pub message: WireMessage,
}

// ── Wire message (tagged union) ─────────────────────────────────────────────

/// All message types that flow over the runner <-> SaaS WebSocket.
/// Discriminated by `"type"` in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    // ── Runner -> SaaS ──────────────────────────────────────────────────

    /// First message after connect. Carries runner metadata and driver
    /// capabilities so the dashboard can render the Drivers panel.
    RunnerHello {
        hostname: String,
        version: String,
        drivers: Vec<DriverAuthInfo>,
    },

    /// An agent was spawned on the runner.
    AgentStarted {
        agent_id: String,
        plan_name: String,
        task_id: String,
        driver: String,
        cwd: String,
    },

    /// Raw PTY output bytes (base64-encoded for JSON safety).
    /// Best-effort — no ACK, no outbox. High-frequency.
    AgentOutput {
        agent_id: String,
        /// Base64-encoded terminal bytes.
        data: String,
    },

    /// Agent process exited.
    AgentStopped {
        agent_id: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
    },

    /// Task status changed (reported by agent via MCP or detected by runner).
    TaskStatusChanged {
        plan_name: String,
        task_number: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Driver authentication status snapshot. Sent at startup and on change.
    DriverAuthReport {
        drivers: Vec<DriverAuthInfo>,
    },

    // ── SaaS -> Runner ──────────────────────────────────────────────────

    /// Dashboard user clicked "Start" — spawn an agent.
    StartAgent {
        agent_id: String,
        plan_name: String,
        task_id: String,
        prompt: String,
        cwd: String,
        driver: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_budget_usd: Option<f64>,
    },

    /// Kill a running agent.
    KillAgent { agent_id: String },

    /// Resize the agent's PTY.
    ResizeTerminal {
        agent_id: String,
        cols: u16,
        rows: u16,
    },

    /// Forward keyboard input to the agent's PTY.
    /// Best-effort — no ACK. `data` is base64-encoded.
    AgentInput {
        agent_id: String,
        data: String,
    },

    /// Request terminal replay from a byte offset (reconnecting browser).
    TerminalReplay {
        agent_id: String,
        from_offset: u64,
    },

    // ── Bidirectional ───────────────────────────────────────────────────

    /// Acknowledge receipt of a sequenced message. The receiver sends this
    /// after persisting the event so the sender can prune its outbox.
    Ack {
        /// The seq being acknowledged.
        ack_seq: u64,
    },

    /// Heartbeat probe. Sender expects a `Pong` within ~15s.
    Ping {},

    /// Heartbeat response.
    Pong {},

    /// Sent immediately after (re)connect. Tells the peer "replay everything
    /// after this seq" so the outbox can catch up.
    Resume {
        /// Last seq the sender successfully processed from this peer.
        last_seen_seq: u64,
    },
}

// ── Driver auth info ────────────────────────────────────────────────────────

/// Snapshot of a single driver's authentication status on the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverAuthInfo {
    pub name: String,
    pub status: DriverAuthStatus,
}

/// Mirrors `crate::agents::driver::AuthStatus` but lives here so the module
/// is self-contained. Conversion happens at the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriverAuthStatus {
    NotInstalled,
    Unauthenticated {
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
    Oauth {
        #[serde(skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    ApiKey,
    CloudProvider {
        provider: String,
    },
    Unknown,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

impl WireMessage {
    /// Returns `true` if this message type is best-effort (no outbox, no ACK).
    pub fn is_best_effort(&self) -> bool {
        matches!(
            self,
            WireMessage::AgentOutput { .. }
                | WireMessage::AgentInput { .. }
                | WireMessage::Ping {}
                | WireMessage::Pong {}
        )
    }

    /// Short label for logging / DB storage.
    pub fn event_type(&self) -> &'static str {
        match self {
            WireMessage::RunnerHello { .. } => "runner_hello",
            WireMessage::AgentStarted { .. } => "agent_started",
            WireMessage::AgentOutput { .. } => "agent_output",
            WireMessage::AgentStopped { .. } => "agent_stopped",
            WireMessage::TaskStatusChanged { .. } => "task_status_changed",
            WireMessage::DriverAuthReport { .. } => "driver_auth_report",
            WireMessage::StartAgent { .. } => "start_agent",
            WireMessage::KillAgent { .. } => "kill_agent",
            WireMessage::ResizeTerminal { .. } => "resize_terminal",
            WireMessage::AgentInput { .. } => "agent_input",
            WireMessage::TerminalReplay { .. } => "terminal_replay",
            WireMessage::Ack { .. } => "ack",
            WireMessage::Ping {} => "ping",
            WireMessage::Pong {} => "pong",
            WireMessage::Resume { .. } => "resume",
        }
    }
}

impl Envelope {
    /// Build an envelope for a reliable (outbox-backed) message.
    pub fn reliable(runner_id: String, seq: u64, message: WireMessage) -> Self {
        Self {
            seq: Some(seq),
            runner_id,
            message,
        }
    }

    /// Build an envelope for a best-effort message (no seq).
    pub fn best_effort(runner_id: String, message: WireMessage) -> Self {
        Self {
            seq: None,
            runner_id,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let env = Envelope::reliable(
            "runner-1".into(),
            42,
            WireMessage::RunnerHello {
                hostname: "laptop".into(),
                version: "0.3.0".into(),
                drivers: vec![DriverAuthInfo {
                    name: "claude".into(),
                    status: DriverAuthStatus::ApiKey,
                }],
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, Some(42));
        assert_eq!(back.runner_id, "runner-1");
        assert!(matches!(back.message, WireMessage::RunnerHello { .. }));
    }

    #[test]
    fn best_effort_has_no_seq() {
        let env = Envelope::best_effort(
            "r1".into(),
            WireMessage::AgentOutput {
                agent_id: "a1".into(),
                data: "aGVsbG8=".into(),
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"seq\""));
    }

    #[test]
    fn ack_round_trip() {
        let env = Envelope::reliable(
            "r1".into(),
            1,
            WireMessage::Ack { ack_seq: 42 },
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.message, WireMessage::Ack { ack_seq: 42 }));
    }

    #[test]
    fn is_best_effort_classification() {
        assert!(WireMessage::AgentOutput {
            agent_id: "a".into(),
            data: "x".into()
        }
        .is_best_effort());
        assert!(WireMessage::Ping {}.is_best_effort());
        assert!(!WireMessage::AgentStarted {
            agent_id: "a".into(),
            plan_name: "p".into(),
            task_id: "t".into(),
            driver: "d".into(),
            cwd: "/".into(),
        }
        .is_best_effort());
    }
}
