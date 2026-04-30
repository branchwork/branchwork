//! Standalone `session_daemon` binary. Equivalent to
//! `branchwork-server session ...` — both dispatch into
//! [`supervisor::run_session`]. Exists so that tests and alternate callers
//! can invoke the daemon without knowing the main binary's subcommand layout.
//!
//! Uses `#[path]` to inline the supervisor + session_protocol modules
//! instead of requiring a separate library crate. `supervisor.rs` talks to
//! `session_protocol` via `super::`, which resolves whether the pair lives
//! under `crate::agents::…` (main binary) or at crate root (this binary).

#[path = "../agents/session_protocol.rs"]
mod session_protocol;

#[path = "../agents/supervisor.rs"]
mod supervisor;

use clap::Parser;
use supervisor::SessionArgs;

#[derive(Parser, Debug)]
#[command(name = "session_daemon", about = "Branchwork mini-supervisor daemon")]
struct Cli {
    #[command(flatten)]
    args: SessionArgs,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = supervisor::run_session(cli.args) {
        eprintln!("session daemon error: {e}");
        std::process::exit(1);
    }
}
