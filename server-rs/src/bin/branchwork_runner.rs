//! Standalone agent runner binary.
//!
//! Runs on the customer's machine or CI, connects to the Branchwork SaaS
//! dashboard via authenticated WebSocket, and executes agents locally.
//! Events are reliably delivered via a local SQLite outbox; dropped
//! connections trigger replay on reconnect.
//!
//! ## Usage
//!
//! ```bash
//! branchwork-runner \
//!   --saas-url wss://app.branchwork.dev \
//!   --token <api-token> \
//!   --cwd /path/to/project
//! ```
//!
//! The runner reuses `branchwork-server session` as the per-agent supervisor
//! daemon — the server binary must be on `$PATH` or specified via `--server-bin`.

// Pull in self-contained modules via #[path] so this binary compiles
// independently of the main branchwork-server crate.
#[path = "../git_helpers.rs"]
mod git_helpers;
#[path = "../saas/outbox.rs"]
mod outbox;
#[path = "../saas/runner_protocol.rs"]
pub mod runner_protocol;
#[path = "../agents/session_protocol.rs"]
mod session_protocol;

// `git_helpers.rs` references types via `crate::saas::runner_protocol` so
// the same `use` statement compiles in both the server crate (where the
// hierarchy actually exists) and this runner binary. Re-export the leaf
// module under that path here — runner-internal callers keep using the
// flat `runner_protocol` module name.
mod saas {
    pub use super::runner_protocol;
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rusqlite::Connection;
use tokio::sync::{Mutex, mpsc};

use runner_protocol::{
    DriverAuthInfo, DriverAuthStatus, Envelope, FolderEntry, GhRun, MergeOutcome, WireMessage,
};

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "branchwork-runner",
    about = "Branchwork remote agent runner — connects to the SaaS dashboard and executes agents locally"
)]
struct Cli {
    /// SaaS dashboard URL (e.g. wss://app.branchwork.dev or ws://localhost:3100).
    #[arg(long, env = "BRANCHWORK_SAAS_URL")]
    saas_url: String,

    /// API token for authentication (from the dashboard's runner management).
    #[arg(long, env = "BRANCHWORK_RUNNER_TOKEN")]
    token: String,

    /// Working directory for agents. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,

    /// Stable runner ID. Auto-generated and persisted if not specified.
    #[arg(long, env = "BRANCHWORK_RUNNER_ID")]
    runner_id: Option<String>,

    /// Path to the local SQLite database for the outbox.
    /// Defaults to `~/.branchwork-runner/runner.db`.
    #[arg(long)]
    db_path: Option<PathBuf>,

    /// Path to the `branchwork-server` binary (needed for spawning session
    /// daemons). Defaults to finding it on `$PATH`.
    #[arg(long)]
    server_bin: Option<PathBuf>,
}

// ── Runner state ────────────────────────────────────────────────────────────

struct RunnerState {
    runner_id: String,
    db: Arc<Mutex<Connection>>,
    /// Currently running agents: agent_id -> AgentHandle
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    /// Channel to send messages to the WebSocket writer.
    ws_tx: mpsc::UnboundedSender<String>,
    cwd: PathBuf,
    server_bin: PathBuf,
}

struct AgentHandle {
    /// PID of the session daemon.
    pid: u32,
    /// Socket path for the session daemon.
    socket_path: PathBuf,
    /// Abort handle for the I/O forwarding task.
    io_task: tokio::task::JoinHandle<()>,
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = rt.block_on(run(cli)) {
        eprintln!("[runner] fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve paths.
    let cwd = std::fs::canonicalize(&cli.cwd)?;
    let db_path = cli.db_path.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".branchwork-runner")
            .join("runner.db")
    });
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let server_bin = cli.server_bin.unwrap_or_else(|| {
        which("branchwork-server").unwrap_or_else(|| PathBuf::from("branchwork-server"))
    });

    // Init local DB.
    let conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    outbox::init_runner_outbox(&conn);
    outbox::init_seq_tracker(&conn);

    // Load or generate runner_id.
    let runner_id = cli
        .runner_id
        .unwrap_or_else(|| load_or_generate_runner_id(&conn));

    println!(
        "[runner] id={runner_id} cwd={} db={}",
        cwd.display(),
        db_path.display()
    );

    let db = Arc::new(Mutex::new(conn));

    // Build the WebSocket URL.
    let ws_url = build_ws_url(&cli.saas_url, &cli.token);

    // Reconnect loop with exponential backoff.
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        println!("[runner] connecting to {}", cli.saas_url);

        match connect_and_run(&ws_url, &runner_id, &cwd, &server_bin, db.clone()).await {
            Ok(()) => {
                println!("[runner] connection closed cleanly");
            }
            Err(e) => {
                eprintln!("[runner] connection error: {e}");
            }
        }

        // Jitter: ±25% of backoff.
        let jitter_ms = {
            use rand::RngCore;
            let mut rng = rand::rng();
            let base = backoff.as_millis() as u64;
            let jitter_range = base / 4;
            if jitter_range > 0 {
                let jitter = (rng.next_u64() % (jitter_range * 2)) as i64 - jitter_range as i64;
                Duration::from_millis((base as i64 + jitter).max(100) as u64)
            } else {
                backoff
            }
        };

        println!("[runner] reconnecting in {}ms", jitter_ms.as_millis());
        tokio::time::sleep(jitter_ms).await;

        // Exponential backoff capped at 30s.
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Single connection lifecycle.
async fn connect_and_run(
    ws_url: &str,
    runner_id: &str,
    cwd: &Path,
    server_bin: &Path,
    db: Arc<Mutex<Connection>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Connect via tokio-tungstenite.
    let (ws_stream, _response) = tokio_tungstenite::connect_async(ws_url).await?;
    let (ws_write, ws_read) = futures_util::StreamExt::split(ws_stream);

    println!("[runner] connected");

    // Channel for outbound WebSocket messages.
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<String>();

    let state = Arc::new(RunnerState {
        runner_id: runner_id.to_string(),
        db: db.clone(),
        agents: Arc::new(Mutex::new(HashMap::new())),
        ws_tx: ws_tx.clone(),
        cwd: cwd.to_path_buf(),
        server_bin: server_bin.to_path_buf(),
    });

    // ── Writer task: flush channel messages to WebSocket ─────────────────
    let writer = tokio::spawn(ws_writer(ws_write, ws_rx));

    // ── Send runner_hello ────────────────────────────────────────────────
    let drivers = collect_driver_auth();
    let hello = WireMessage::RunnerHello {
        hostname: hostname(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        drivers: drivers.clone(),
    };
    send_reliable(&state, hello).await;

    // ── Send driver_auth_report ─────────────────────────────────────────
    let auth_report = WireMessage::DriverAuthReport { drivers };
    send_reliable(&state, auth_report).await;

    // ── Send Resume with our last_seen_seq ──────────────────────────────
    {
        let last_seq = {
            let conn = db.lock().await;
            outbox::last_seen_seq(&conn, "server")
        };
        let resume = Envelope::best_effort(
            runner_id.to_string(),
            WireMessage::Resume {
                last_seen_seq: last_seq,
            },
        );
        let _ = ws_tx.send(serde_json::to_string(&resume)?);
    }

    // ── Heartbeat task ──────────────────────────────────────────────────
    let heartbeat_tx = ws_tx.clone();
    let heartbeat_rid = runner_id.to_string();
    let heartbeat = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let ping = Envelope::best_effort(heartbeat_rid.clone(), WireMessage::Ping {});
            if heartbeat_tx
                .send(serde_json::to_string(&ping).unwrap_or_default())
                .is_err()
            {
                break;
            }
        }
    });

    // ── Reader task: process incoming messages from SaaS ────────────────
    let read_result = ws_reader(ws_read, state.clone()).await;

    // ── Cleanup ─────────────────────────────────────────────────────────
    heartbeat.abort();
    writer.abort();

    read_result
}

/// WebSocket writer: drains the channel and sends each string as a Text frame.
async fn ws_writer(
    mut ws_write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Message,
    >,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    use futures_util::SinkExt;
    while let Some(msg) = rx.recv().await {
        if ws_write
            .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

/// WebSocket reader: processes frames from the SaaS.
async fn ws_reader(
    mut ws_read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    state: Arc<RunnerState>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    while let Some(msg_result) = ws_read.next().await {
        let msg = msg_result?;

        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(_) => {
                let pong = Envelope::best_effort(state.runner_id.clone(), WireMessage::Pong {});
                let _ = state
                    .ws_tx
                    .send(serde_json::to_string(&pong).unwrap_or_default());
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(envelope) = serde_json::from_str::<Envelope>(&text) else {
            eprintln!(
                "[runner] failed to parse envelope: {}",
                &text[..80.min(text.len())]
            );
            continue;
        };

        // ACK reliable messages.
        if let Some(seq) = envelope.seq {
            let is_new = {
                let conn = state.db.lock().await;
                outbox::advance_peer_seq(&conn, "server", seq)
            };

            // Always ACK (so server prunes its outbox).
            let ack =
                Envelope::best_effort(state.runner_id.clone(), WireMessage::Ack { ack_seq: seq });
            let _ = state
                .ws_tx
                .send(serde_json::to_string(&ack).unwrap_or_default());

            if !is_new {
                continue; // Duplicate — skip.
            }
        }

        handle_server_message(&state, &envelope).await;
    }

    Ok(())
}

/// Handle a single message from the SaaS server.
async fn handle_server_message(state: &RunnerState, envelope: &Envelope) {
    match &envelope.message {
        WireMessage::StartAgent {
            agent_id,
            plan_name,
            task_id,
            prompt,
            cwd,
            driver,
            effort,
            max_budget_usd,
        } => {
            println!(
                "[runner] start_agent: {} plan={} task={} driver={}",
                agent_id, plan_name, task_id, driver
            );

            let agent_cwd = if cwd.is_empty() {
                state.cwd.clone()
            } else {
                PathBuf::from(cwd)
            };

            // Spawn the session daemon.
            match spawn_agent(
                state,
                agent_id,
                &agent_cwd,
                driver,
                prompt,
                effort.as_deref(),
                *max_budget_usd,
            )
            .await
            {
                Ok(()) => {
                    // Report agent_started.
                    let started = WireMessage::AgentStarted {
                        agent_id: agent_id.clone(),
                        plan_name: plan_name.clone(),
                        task_id: task_id.clone(),
                        driver: driver.clone(),
                        cwd: agent_cwd.display().to_string(),
                    };
                    send_reliable(state, started).await;
                }
                Err(e) => {
                    eprintln!("[runner] failed to spawn agent {agent_id}: {e}");
                    // Report immediate failure.
                    let stopped = WireMessage::AgentStopped {
                        agent_id: agent_id.clone(),
                        status: "failed".into(),
                        cost_usd: None,
                        stop_reason: Some(format!("spawn failed: {e}")),
                    };
                    send_reliable(state, stopped).await;
                }
            }
        }

        WireMessage::KillAgent { agent_id } => {
            println!("[runner] kill_agent: {agent_id}");
            let mut agents = state.agents.lock().await;
            if let Some(handle) = agents.remove(agent_id) {
                handle.io_task.abort();
                // Send SIGTERM to the session daemon.
                #[cfg(unix)]
                unsafe {
                    libc::kill(handle.pid as i32, libc::SIGTERM);
                }
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &handle.pid.to_string(), "/T"])
                        .status();
                }
            }
        }

        WireMessage::AgentInput { agent_id, data } => {
            // Forward to the local session daemon.
            if let Ok(bytes) = base64_decode(data) {
                let agents = state.agents.lock().await;
                if let Some(handle) = agents.get(agent_id.as_str())
                    && let Ok(mut stream) = connect_to_socket(&handle.socket_path).await
                {
                    let msg = session_protocol::Message::Input(bytes);
                    let _ = session_protocol::write_frame(&mut stream, &msg).await;
                }
            }
        }

        WireMessage::ResizeTerminal {
            agent_id,
            cols,
            rows,
        } => {
            let agents = state.agents.lock().await;
            if let Some(handle) = agents.get(agent_id.as_str())
                && let Ok(mut stream) = connect_to_socket(&handle.socket_path).await
            {
                let msg = session_protocol::Message::Resize {
                    cols: *cols,
                    rows: *rows,
                };
                let _ = session_protocol::write_frame(&mut stream, &msg).await;
            }
        }

        WireMessage::Resume { last_seen_seq } => {
            // SaaS wants us to replay from this seq.
            let events = {
                let conn = state.db.lock().await;
                outbox::replay_runner_events(&conn, *last_seen_seq)
            };
            for (seq, _event_type, payload) in events {
                if let Ok(msg) = serde_json::from_str::<WireMessage>(&payload) {
                    let env = Envelope::reliable(state.runner_id.clone(), seq, msg);
                    let _ = state
                        .ws_tx
                        .send(serde_json::to_string(&env).unwrap_or_default());
                }
            }
        }

        WireMessage::Ack { ack_seq } => {
            let conn = state.db.lock().await;
            outbox::mark_runner_acked(&conn, *ack_seq);
        }

        WireMessage::Pong {} => {
            // Heartbeat response — connection is alive.
        }

        WireMessage::ListFolders { req_id } => {
            let entries = list_home_folders();
            let reply = Envelope::best_effort(
                state.runner_id.clone(),
                WireMessage::FoldersListed {
                    req_id: req_id.clone(),
                    entries,
                },
            );
            let _ = state
                .ws_tx
                .send(serde_json::to_string(&reply).unwrap_or_default());
        }

        WireMessage::CreateFolder {
            req_id,
            path,
            create_if_missing,
        } => {
            let (ok, resolved_path, error) =
                check_or_create_folder(path, *create_if_missing);
            let reply = WireMessage::FolderCreated {
                req_id: req_id.clone(),
                ok,
                resolved_path,
                error,
            };
            let env = Envelope::best_effort(state.runner_id.clone(), reply);
            let _ = state
                .ws_tx
                .send(serde_json::to_string(&env).unwrap_or_default());
        }

        WireMessage::GetDefaultBranch { req_id, cwd } => {
            let req_id = req_id.clone();
            let reply = match validated_cwd(state, cwd) {
                Ok(path) => {
                    let branch =
                        run_blocking_with_timeout(READ_TIMEOUT, move || {
                            git_helpers::git_default_branch(&path)
                        })
                        .await
                        .unwrap_or(None);
                    WireMessage::DefaultBranchResolved {
                        req_id,
                        branch,
                    }
                }
                Err(e) => {
                    eprintln!("[runner] get_default_branch rejected cwd: {e}");
                    WireMessage::DefaultBranchResolved {
                        req_id,
                        branch: None,
                    }
                }
            };
            send_best_effort(state, reply);
        }

        WireMessage::ListBranches { req_id, cwd } => {
            let req_id = req_id.clone();
            let reply = match validated_cwd(state, cwd) {
                Ok(path) => {
                    let branches = run_blocking_with_timeout(READ_TIMEOUT, move || {
                        git_helpers::git_list_branches(&path)
                    })
                    .await
                    .unwrap_or_default();
                    WireMessage::BranchesListed { req_id, branches }
                }
                Err(e) => {
                    eprintln!("[runner] list_branches rejected cwd: {e}");
                    WireMessage::BranchesListed {
                        req_id,
                        branches: Vec::new(),
                    }
                }
            };
            send_best_effort(state, reply);
        }

        WireMessage::MergeBranch {
            req_id,
            cwd,
            target,
            task_branch,
        } => {
            let req_id = req_id.clone();
            let target = target.clone();
            let task_branch = task_branch.clone();
            let outcome = match validated_cwd(state, cwd) {
                Ok(path) => run_blocking_with_timeout(MERGE_TIMEOUT, move || {
                    git_helpers::merge_branch_local(&path, &target, &task_branch)
                })
                .await
                .unwrap_or_else(|| MergeOutcome::Other {
                    stderr: format!(
                        "merge timed out after {}s",
                        MERGE_TIMEOUT.as_secs()
                    ),
                }),
                Err(e) => MergeOutcome::Other { stderr: e },
            };
            send_best_effort(state, WireMessage::MergeResult { req_id, outcome });
        }

        WireMessage::PushBranch {
            req_id,
            cwd,
            branch,
        } => {
            let req_id = req_id.clone();
            let branch = branch.clone();
            let (ok, stderr) = match validated_cwd(state, cwd) {
                Ok(path) => {
                    let result = run_blocking_with_timeout(PUSH_TIMEOUT, move || {
                        git_helpers::push_branch_local(&path, &branch)
                    })
                    .await;
                    match result {
                        Some(Ok(())) => (true, None),
                        Some(Err(e)) => (false, Some(e)),
                        None => (
                            false,
                            Some(format!(
                                "push timed out after {}s",
                                PUSH_TIMEOUT.as_secs()
                            )),
                        ),
                    }
                }
                Err(e) => (false, Some(e)),
            };
            send_best_effort(
                state,
                WireMessage::PushResult {
                    req_id,
                    ok,
                    stderr,
                },
            );
        }

        WireMessage::GhRunList { req_id, cwd, sha } => {
            let req_id = req_id.clone();
            let sha = sha.clone();
            let run: Option<GhRun> = match validated_cwd(state, cwd) {
                Ok(path) => run_blocking_with_timeout(GH_TIMEOUT, move || {
                    git_helpers::gh_run_list_local(&path, &sha)
                })
                .await
                .unwrap_or(None),
                Err(e) => {
                    eprintln!("[runner] gh_run_list rejected cwd: {e}");
                    None
                }
            };
            send_best_effort(state, WireMessage::GhRunListed { req_id, run });
        }

        WireMessage::GhFailureLog {
            req_id,
            cwd,
            run_id,
        } => {
            let req_id = req_id.clone();
            let run_id = run_id.clone();
            let log: Option<String> = match validated_cwd(state, cwd) {
                Ok(path) => run_blocking_with_timeout(GH_TIMEOUT, move || {
                    git_helpers::gh_failure_log_local(&path, &run_id)
                })
                .await
                .unwrap_or(None),
                Err(e) => {
                    eprintln!("[runner] gh_failure_log rejected cwd: {e}");
                    None
                }
            };
            send_best_effort(state, WireMessage::GhFailureLogFetched { req_id, log });
        }

        // Runner doesn't receive these from server (runner→saas direction
        // only; the server sending them would be a protocol violation).
        WireMessage::RunnerHello { .. }
        | WireMessage::AgentStarted { .. }
        | WireMessage::AgentOutput { .. }
        | WireMessage::AgentStopped { .. }
        | WireMessage::TaskStatusChanged { .. }
        | WireMessage::DriverAuthReport { .. }
        | WireMessage::FoldersListed { .. }
        | WireMessage::FolderCreated { .. }
        | WireMessage::DefaultBranchResolved { .. }
        | WireMessage::BranchesListed { .. }
        | WireMessage::MergeResult { .. }
        | WireMessage::PushResult { .. }
        | WireMessage::GhRunListed { .. }
        | WireMessage::GhFailureLogFetched { .. }
        // saas→runner variants the runner doesn't act on yet (handlers
        // land in later phases).
        | WireMessage::TerminalReplay { .. }
        | WireMessage::Ping {} => {}
    }
}

// ── Wall-clock caps for runner-side git/gh shell-outs ───────────────────────
//
// Each handler runs the helper on a blocking thread and races it against
// these timeouts. On timeout the spawned task is detached (it keeps running
// until the child process finishes), but the handler's req_id slot is freed
// immediately so a hung `git` or `gh` invocation can't permanently park the
// reply channel. The dispatcher on the SaaS side (saas/runner_rpc.rs) has
// its own, longer timeout that wraps the round-trip.
//
// Numbers track the brief: 30 s for read/merge/gh, 60 s for push.

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MERGE_TIMEOUT: Duration = Duration::from_secs(30);
const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Validate a request-supplied `cwd` against the runner's canonical
/// `--cwd`. Refuses anything outside the canonical root so a buggy or
/// malicious server can't pivot the runner into an arbitrary directory.
///
/// The runner already canonicalises `state.cwd` once at startup. We
/// canonicalise the request path here too — which doubles as an existence
/// check, since `canonicalize` errors on missing components.
fn validated_cwd(state: &RunnerState, requested: &str) -> Result<PathBuf, String> {
    let req = PathBuf::from(requested);
    let canonical = std::fs::canonicalize(&req)
        .map_err(|e| format!("cwd not canonicalisable ({}): {e}", req.display()))?;
    if !canonical.starts_with(&state.cwd) {
        return Err(format!(
            "cwd {} outside runner root {}",
            canonical.display(),
            state.cwd.display()
        ));
    }
    Ok(canonical)
}

/// Run `f` on a blocking thread, racing it against `timeout`. Returns
/// `Some(result)` if `f` completed in time, `None` on timeout (the spawned
/// task is detached and the underlying child process is left to exit on its
/// own). The handler always replies to the SaaS side regardless of which
/// branch fires.
async fn run_blocking_with_timeout<T, F>(timeout: Duration, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let handle = tokio::task::spawn_blocking(f);
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            eprintln!("[runner] blocking task panicked: {e}");
            None
        }
        Err(_) => None,
    }
}

/// Send a best-effort reply over the WebSocket. Drops silently if the
/// writer task is gone — the SaaS side will time out and the next reconnect
/// is the right recovery.
fn send_best_effort(state: &RunnerState, message: WireMessage) {
    let env = Envelope::best_effort(state.runner_id.clone(), message);
    let _ = state
        .ws_tx
        .send(serde_json::to_string(&env).unwrap_or_default());
}

/// Resolve a runner-side folder path. `~` and `~/...` expand against
/// `dirs::home_dir()`; everything else is treated as already-absolute and
/// passed through unchanged.
fn resolve_runner_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else if path == "~" {
        dirs::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(path)
    }
}

/// Existence-check or `mkdir -p` on a runner-side path. Always returns the
/// resolved absolute path string in `resolved_path` so the caller can echo it
/// back to the dashboard regardless of outcome (which is what the
/// `folder_not_found` UX flow needs to render the prompt).
fn check_or_create_folder(
    path: &str,
    create_if_missing: bool,
) -> (bool, Option<String>, Option<String>) {
    let resolved = resolve_runner_path(path);
    let resolved_str = Some(resolved.display().to_string());
    if create_if_missing {
        match std::fs::create_dir_all(&resolved) {
            Ok(()) if resolved.is_dir() => (true, resolved_str, None),
            Ok(()) => (
                false,
                resolved_str,
                Some(format!("not a directory: {}", resolved.display())),
            ),
            Err(e) => (false, resolved_str, Some(e.to_string())),
        }
    } else if resolved.is_dir() {
        (true, resolved_str, None)
    } else {
        (false, resolved_str, Some("folder_not_found".to_string()))
    }
}

/// List non-dot directories at the top level of the runner's home dir.
/// Mirrors `api/settings.rs::list_folders` for SaaS folder picking.
fn list_home_folders() -> Vec<FolderEntry> {
    let home = dirs::home_dir().unwrap_or_default();
    match std::fs::read_dir(&home) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .map(|e| FolderEntry {
                name: e.file_name().to_string_lossy().to_string(),
                path: e.path().to_string_lossy().to_string(),
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

// ── Agent spawning ──────────────────────────────────────────────────────────

/// Spawn a session daemon for an agent and wire up I/O forwarding.
async fn spawn_agent(
    state: &RunnerState,
    agent_id: &str,
    cwd: &Path,
    driver: &str,
    prompt: &str,
    effort: Option<&str>,
    _max_budget_usd: Option<f64>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Build socket path.
    let sockets_dir = state.cwd.join(".branchwork-runner-sessions");
    std::fs::create_dir_all(&sockets_dir)?;
    let socket_path = sockets_dir.join(format!("{agent_id}.sock"));

    // Build the command to spawn. The session daemon expects:
    // branchwork-server session --socket <path> --cwd <dir> [--cols C --rows R] -- <cmd...>
    let binary = match driver {
        "claude" => "claude",
        "aider" => "aider",
        "codex" => "codex",
        "gemini" => "gemini",
        _ => "claude",
    };

    let mut args = vec![
        "session".to_string(),
        "--socket".to_string(),
        socket_path.display().to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--".to_string(),
        binary.to_string(),
    ];

    // Add effort for Claude.
    if binary == "claude"
        && let Some(eff) = effort
    {
        args.push("--effort".to_string());
        args.push(eff.to_string());
    }

    // Spawn the session daemon.
    let mut cmd = std::process::Command::new(&state.server_bin);
    cmd.args(&args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }

    let child = cmd.spawn()?;
    let pid = child.id();

    println!(
        "[runner] spawned session daemon pid={pid} socket={}",
        socket_path.display()
    );

    // Wait for the socket to appear.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket_path.exists() {
        if tokio::time::Instant::now() > deadline {
            return Err("session daemon socket did not appear within 10s".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Small extra delay for the daemon to start listening.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect and start I/O forwarding.
    let ws_tx = state.ws_tx.clone();
    let runner_id = state.runner_id.clone();
    let aid = agent_id.to_string();
    let sock = socket_path.clone();
    let prompt_bytes = prompt.as_bytes().to_vec();

    let io_task = tokio::spawn(async move {
        if let Err(e) = forward_agent_io(&sock, &ws_tx, &runner_id, &aid, &prompt_bytes).await {
            eprintln!("[runner] agent {aid} I/O error: {e}");
        }

        // Agent exited — report.
        let stopped = WireMessage::AgentStopped {
            agent_id: aid.clone(),
            status: "completed".into(),
            cost_usd: None,
            stop_reason: None,
        };
        let env = Envelope::best_effort(runner_id.clone(), stopped);
        let _ = ws_tx.send(serde_json::to_string(&env).unwrap_or_default());
    });

    state.agents.lock().await.insert(
        agent_id.to_string(),
        AgentHandle {
            pid,
            socket_path,
            io_task,
        },
    );

    Ok(())
}

/// Connect to a session daemon and forward I/O between it and the WebSocket.
async fn forward_agent_io(
    socket_path: &Path,
    ws_tx: &mpsc::UnboundedSender<String>,
    runner_id: &str,
    agent_id: &str,
    prompt: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_to_socket(socket_path).await?;

    // Read output from the daemon and forward to SaaS.
    let mut readiness_buf = Vec::with_capacity(16 * 1024);
    let mut prompt_injected = false;

    loop {
        match session_protocol::read_frame(&mut stream).await {
            Ok(session_protocol::Message::Output(data)) => {
                // Check for readiness (Claude prompt glyph ❯).
                if !prompt_injected {
                    readiness_buf.extend_from_slice(&data);
                    if readiness_buf.len() > 16 * 1024 {
                        readiness_buf.drain(..readiness_buf.len() - 8 * 1024);
                    }
                    if is_ready(&readiness_buf) {
                        // Inject prompt.
                        let input_msg = session_protocol::Message::Input(prompt.to_vec());
                        session_protocol::write_frame(&mut stream, &input_msg).await?;
                        prompt_injected = true;
                    }
                }

                // Forward output to SaaS as best-effort.
                let encoded = base64_encode(&data);
                let output = Envelope::best_effort(
                    runner_id.to_string(),
                    WireMessage::AgentOutput {
                        agent_id: agent_id.to_string(),
                        data: encoded,
                    },
                );
                let _ = ws_tx.send(serde_json::to_string(&output).unwrap_or_default());
            }
            Ok(session_protocol::Message::Pong) => {
                // Daemon heartbeat response — connection alive.
            }
            Ok(_) => {
                // Ignore other message types from daemon.
            }
            Err(_) => {
                // EOF or error — daemon exited.
                break;
            }
        }
    }

    Ok(())
}

// ── Outbox integration ──────────────────────────────────────────────────────

/// Enqueue a reliable message in the outbox and send it over the WebSocket.
async fn send_reliable(state: &RunnerState, message: WireMessage) {
    let payload = serde_json::to_string(&message).unwrap_or_default();
    let seq = {
        let conn = state.db.lock().await;
        outbox::enqueue_runner_event(&conn, message.event_type(), &payload)
    };
    let env = Envelope::reliable(state.runner_id.clone(), seq, message);
    let _ = state
        .ws_tx
        .send(serde_json::to_string(&env).unwrap_or_default());
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| {
            #[cfg(unix)]
            {
                let mut buf = [0u8; 256];
                unsafe {
                    if libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) == 0 {
                        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                        return String::from_utf8_lossy(&buf[..len]).to_string();
                    }
                }
            }
            "unknown".to_string()
        })
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var("PATH").ok().and_then(|path| {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let exe = dir.join(format!("{binary}.exe"));
                if exe.is_file() {
                    return Some(exe);
                }
            }
        }
        None
    })
}

fn build_ws_url(saas_url: &str, token: &str) -> String {
    let base = saas_url.trim_end_matches('/');
    // Convert http(s) to ws(s) if needed.
    let ws_base = if base.starts_with("https://") {
        base.replacen("https://", "wss://", 1)
    } else if base.starts_with("http://") {
        base.replacen("http://", "ws://", 1)
    } else {
        base.to_string()
    };
    format!("{ws_base}/ws/runner?token={token}")
}

fn load_or_generate_runner_id(conn: &Connection) -> String {
    // Try to load from the seq_tracker table (we reuse it for config).
    let existing: Option<String> = conn
        .query_row(
            "SELECT peer_id FROM seq_tracker WHERE peer_id LIKE 'runner-%' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        return id;
    }

    let id = format!("runner-{}", uuid::Uuid::new_v4());
    conn.execute(
        "INSERT OR IGNORE INTO seq_tracker (peer_id, last_seq) VALUES (?1, 0)",
        rusqlite::params![id],
    )
    .ok();
    id
}

fn collect_driver_auth() -> Vec<DriverAuthInfo> {
    // Check common drivers.
    let mut drivers = Vec::new();

    for (name, binary, env_vars) in [
        ("claude", "claude", vec!["ANTHROPIC_API_KEY"]),
        (
            "aider",
            "aider",
            vec!["OPENAI_API_KEY", "ANTHROPIC_API_KEY"],
        ),
        ("codex", "codex", vec!["OPENAI_API_KEY"]),
        ("gemini", "gemini", vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
    ] {
        let status = if which(binary).is_none() {
            DriverAuthStatus::NotInstalled
        } else {
            let has_key = env_vars
                .iter()
                .any(|v| std::env::var(v).ok().is_some_and(|s| !s.trim().is_empty()));
            if has_key {
                DriverAuthStatus::ApiKey
            } else {
                DriverAuthStatus::Unknown
            }
        };
        drivers.push(DriverAuthInfo {
            name: name.to_string(),
            status,
        });
    }

    drivers
}

/// Detect when the CLI is ready for input. Checks for the Claude prompt glyph.
fn is_ready(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    // Claude prompt glyph (❯) or generic REPL prompt (\n> )
    s.contains('❯') || s.contains("\n> ")
}

fn base64_encode(data: &[u8]) -> String {
    // Simple base64 without pulling in a crate.
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

fn base64_decode(input: &str) -> Result<Vec<u8>, &'static str> {
    fn val(c: u8) -> Result<u32, &'static str> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err("invalid base64 character"),
        }
    }
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = val(chunk[2])?;
        let d = val(chunk[3])?;
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        result.push(((triple >> 16) & 0xFF) as u8);
        if chunk[2] != b'=' {
            result.push(((triple >> 8) & 0xFF) as u8);
        }
        if chunk[3] != b'=' {
            result.push((triple & 0xFF) as u8);
        }
    }
    Ok(result)
}

async fn connect_to_socket(
    socket_path: &Path,
) -> Result<interprocess::local_socket::tokio::Stream, Box<dyn std::error::Error>> {
    use interprocess::local_socket::ConnectOptions;
    use interprocess::local_socket::GenericFilePath;
    use interprocess::local_socket::tokio::prelude::*;

    let name = socket_path
        .to_path_buf()
        .to_fs_name::<GenericFilePath>()?
        .into_owned();
    let stream = ConnectOptions::new().name(name).connect_tokio().await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let input = b"Hello, World!";
        let encoded = base64_encode(input);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn build_ws_url_converts_http() {
        assert_eq!(
            build_ws_url("http://localhost:3100", "tok123"),
            "ws://localhost:3100/ws/runner?token=tok123"
        );
        assert_eq!(
            build_ws_url("https://app.example.com", "tok"),
            "wss://app.example.com/ws/runner?token=tok"
        );
        assert_eq!(
            build_ws_url("wss://already.ws", "t"),
            "wss://already.ws/ws/runner?token=t"
        );
    }

    #[test]
    fn readiness_detection() {
        assert!(is_ready("some output ❯ ".as_bytes()));
        assert!(is_ready("line1\n> ".as_bytes()));
        assert!(!is_ready(b"not ready yet"));
    }

    #[test]
    fn resolve_runner_path_expands_tilde_prefix() {
        let home = dirs::home_dir().expect("test host should have a home dir");
        assert_eq!(resolve_runner_path("~"), home);
        assert_eq!(
            resolve_runner_path("~/new-project"),
            home.join("new-project")
        );
        assert_eq!(
            resolve_runner_path("~/nested/deep/dir"),
            home.join("nested/deep/dir")
        );
    }

    #[test]
    fn resolve_runner_path_passes_absolute_through() {
        assert_eq!(
            resolve_runner_path("/tmp/branchwork-test"),
            PathBuf::from("/tmp/branchwork-test")
        );
        // Bare names without ~ are not expanded.
        assert_eq!(resolve_runner_path("relative"), PathBuf::from("relative"));
        // ~user (not ~/) is not expanded — left as-is.
        assert_eq!(resolve_runner_path("~root"), PathBuf::from("~root"));
    }

    #[test]
    fn resolve_runner_path_create_dir_all_is_idempotent() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-runner-test-{}", uuid::Uuid::new_v4()));
        let target = tmp.join("a/b/c");
        let resolved = resolve_runner_path(&target.display().to_string());
        std::fs::create_dir_all(&resolved).expect("first create");
        // mkdir -p semantics: a second call must succeed too.
        std::fs::create_dir_all(&resolved).expect("second create");
        assert!(resolved.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn check_or_create_folder_existing_dir_without_creation_returns_ok() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-existing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let (ok, resolved, error) = check_or_create_folder(&tmp.display().to_string(), false);
        assert!(ok);
        assert_eq!(resolved, Some(tmp.display().to_string()));
        assert!(error.is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn check_or_create_folder_missing_without_creation_returns_folder_not_found() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-missing-{}", uuid::Uuid::new_v4()));
        // Do NOT create the dir — caller must see folder_not_found.
        let (ok, resolved, error) = check_or_create_folder(&tmp.display().to_string(), false);
        assert!(!ok);
        assert_eq!(resolved, Some(tmp.display().to_string()));
        assert_eq!(error.as_deref(), Some("folder_not_found"));
    }

    #[test]
    fn check_or_create_folder_missing_with_creation_makes_dir() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-create-{}", uuid::Uuid::new_v4()));
        let nested = tmp.join("a/b/c");
        let (ok, resolved, error) = check_or_create_folder(&nested.display().to_string(), true);
        assert!(ok);
        assert_eq!(resolved, Some(nested.display().to_string()));
        assert!(error.is_none());
        assert!(nested.is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn check_or_create_folder_with_creation_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!(
            "branchwork-cof-idempotent-{}",
            uuid::Uuid::new_v4()
        ));
        for _ in 0..2 {
            let (ok, _, error) = check_or_create_folder(&tmp.display().to_string(), true);
            assert!(ok);
            assert!(error.is_none());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Phase 5.7 handler tests ─────────────────────────────────────────
    //
    // Drive each new RPC handler arm directly through `handle_server_message`
    // and observe the reply on the writer channel. This exercises:
    //   - cwd validation (path inside vs outside the canonical runner cwd),
    //   - the wire shape of every reply variant,
    //   - the underlying git/gh shell-out (via git_helpers).
    //
    // We intentionally do not stand up a real WS round-trip — the runner's
    // protocol layer is already covered by saas/runner_rpc.rs's
    // real_ws_disconnect_drains_pending_senders_and_wakes_receivers. What
    // these tests guard is the correctness of the runner side of each pair.

    use tempfile::TempDir;
    use tokio::sync::{Mutex, mpsc};

    /// Run a `git` command in `dir` and panic if it fails. Test-only helper.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {args:?} failed in {}: {}\n{}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// Initialise a repo at `dir` on `branch` with one empty commit. Mirrors
    /// the helper that lives next to the git_helpers tests; copied here so
    /// the runner-bin test file is self-contained.
    fn git_init_with_commit(dir: &Path, branch: &str) {
        git(dir, &["init", "-b", branch]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["commit", "--allow-empty", "-m", "init"]);
    }

    /// Build a minimal `RunnerState` rooted at `cwd`. Returns the state
    /// alongside the receive side of the writer channel so tests can read
    /// envelopes the handler emits.
    fn make_test_state(cwd: PathBuf) -> (Arc<RunnerState>, mpsc::UnboundedReceiver<String>) {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        outbox::init_runner_outbox(&conn);
        outbox::init_seq_tracker(&conn);
        let (ws_tx, ws_rx) = mpsc::unbounded_channel::<String>();
        let canonical = std::fs::canonicalize(&cwd).expect("canonicalize tempdir");
        let state = Arc::new(RunnerState {
            runner_id: "runner-test".to_string(),
            db: Arc::new(Mutex::new(conn)),
            agents: Arc::new(Mutex::new(HashMap::new())),
            ws_tx,
            cwd: canonical,
            server_bin: PathBuf::from("/usr/bin/true"),
        });
        (state, ws_rx)
    }

    /// Wrap `message` in a best-effort envelope and feed it to
    /// `handle_server_message`. Returns the first reply parsed off the
    /// writer channel.
    async fn dispatch(
        state: &Arc<RunnerState>,
        rx: &mut mpsc::UnboundedReceiver<String>,
        message: WireMessage,
    ) -> WireMessage {
        let env = Envelope::best_effort(state.runner_id.clone(), message);
        handle_server_message(state, &env).await;
        let raw = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("handler did not reply within 5s")
            .expect("ws_tx channel closed");
        let env: Envelope = serde_json::from_str(&raw).expect("reply parses");
        env.message
    }

    #[tokio::test]
    async fn get_default_branch_resolves_master() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GetDefaultBranch {
                req_id: "req-1".into(),
                cwd: state.cwd.display().to_string(),
            },
        )
        .await;
        match reply {
            WireMessage::DefaultBranchResolved { req_id, branch } => {
                assert_eq!(req_id, "req-1");
                assert_eq!(branch.as_deref(), Some("master"));
            }
            other => panic!("expected DefaultBranchResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_branches_returns_alphabetical_no_remotes() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        git(dir.path(), &["branch", "feature/x"]);
        git(dir.path(), &["branch", "bw/1.1"]);
        // A fake remote-tracking ref must NOT show up — `git branch` without
        // --all hides remotes by default; this just pins the contract.
        std::fs::create_dir_all(dir.path().join(".git/refs/remotes/origin")).unwrap();
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(
            dir.path().join(".git/refs/remotes/origin/main"),
            head.stdout,
        )
        .unwrap();

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::ListBranches {
                req_id: "req-2".into(),
                cwd: state.cwd.display().to_string(),
            },
        )
        .await;
        match reply {
            WireMessage::BranchesListed { req_id, branches } => {
                assert_eq!(req_id, "req-2");
                assert_eq!(
                    branches,
                    vec![
                        "bw/1.1".to_string(),
                        "feature/x".to_string(),
                        "master".to_string()
                    ]
                );
            }
            other => panic!("expected BranchesListed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_branch_happy_path_replies_ok_with_sha() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        git(dir.path(), &["checkout", "-b", "feature/x"]);
        std::fs::write(dir.path().join("foo.txt"), "hi").unwrap();
        git(dir.path(), &["add", "foo.txt"]);
        git(dir.path(), &["commit", "-m", "add foo"]);
        git(dir.path(), &["checkout", "master"]);

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeBranch {
                req_id: "req-3".into(),
                cwd: state.cwd.display().to_string(),
                target: "master".into(),
                task_branch: "feature/x".into(),
            },
        )
        .await;
        match reply {
            WireMessage::MergeResult {
                req_id,
                outcome: MergeOutcome::Ok { merged_sha },
            } => {
                assert_eq!(req_id, "req-3");
                assert_eq!(merged_sha.len(), 40);
            }
            other => panic!("expected MergeResult::Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_branch_empty_replies_empty_branch() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        git(dir.path(), &["branch", "feature/empty"]);

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeBranch {
                req_id: "req-4".into(),
                cwd: state.cwd.display().to_string(),
                target: "master".into(),
                task_branch: "feature/empty".into(),
            },
        )
        .await;
        assert!(matches!(
            reply,
            WireMessage::MergeResult {
                outcome: MergeOutcome::EmptyBranch,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn merge_branch_conflict_replies_conflict_and_aborts() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        std::fs::write(dir.path().join("c.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "base"]);
        git(dir.path(), &["checkout", "-b", "feature/conflict"]);
        std::fs::write(dir.path().join("c.txt"), "branch\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "branch"]);
        git(dir.path(), &["checkout", "master"]);
        std::fs::write(dir.path().join("c.txt"), "master\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "master"]);

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeBranch {
                req_id: "req-5".into(),
                cwd: state.cwd.display().to_string(),
                target: "master".into(),
                task_branch: "feature/conflict".into(),
            },
        )
        .await;
        match reply {
            WireMessage::MergeResult {
                req_id,
                outcome: MergeOutcome::Conflict { stderr: _ },
            } => assert_eq!(req_id, "req-5"),
            other => panic!("expected MergeResult::Conflict, got {other:?}"),
        }
        // Acceptance: runner must have aborted cleanly — no leftover MERGE_HEAD.
        // (git's CONFLICT message goes to stdout, so stderr may be empty here
        // even though the conflict was correctly detected.)
        assert!(
            !dir.path().join(".git/MERGE_HEAD").exists(),
            "MERGE_HEAD lingering after conflict reply"
        );
    }

    #[tokio::test]
    async fn push_branch_to_local_bare_origin_replies_ok() {
        let dir = TempDir::new().unwrap();
        let origin = dir.path().join("origin.git");
        std::process::Command::new("git")
            .args(["init", "--bare", "-b", "master"])
            .arg(&origin)
            .status()
            .unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git_init_with_commit(&work, "master");
        git(
            &work,
            &["remote", "add", "origin", &origin.display().to_string()],
        );

        let (state, mut rx) = make_test_state(work.clone());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::PushBranch {
                req_id: "req-6".into(),
                cwd: state.cwd.display().to_string(),
                branch: "master".into(),
            },
        )
        .await;
        match reply {
            WireMessage::PushResult { req_id, ok, stderr } => {
                assert_eq!(req_id, "req-6");
                assert!(ok, "push should succeed; stderr={stderr:?}");
            }
            other => panic!("expected PushResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gh_run_list_against_unknown_sha_replies_run_none() {
        // No git repo at the cwd → gh exits non-zero → helper returns None.
        // Acceptance criterion: `run: None`, NOT an error variant.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GhRunList {
                req_id: "req-7".into(),
                cwd: state.cwd.display().to_string(),
                sha: "deadbeef".into(),
            },
        )
        .await;
        match reply {
            WireMessage::GhRunListed { req_id, run } => {
                assert_eq!(req_id, "req-7");
                assert!(run.is_none(), "expected run: None, got {run:?}");
            }
            other => panic!("expected GhRunListed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gh_failure_log_for_unknown_run_replies_log_none() {
        // Without a real failed gh run we can't get a tail back, so this
        // covers the protocol shape (req_id round-trip + `log: None` when
        // gh fails). The trim-tail logic is exercised by git_helpers in the
        // server-bin test suite.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GhFailureLog {
                req_id: "req-8".into(),
                cwd: state.cwd.display().to_string(),
                run_id: "0".into(),
            },
        )
        .await;
        match reply {
            WireMessage::GhFailureLogFetched { req_id, log } => {
                assert_eq!(req_id, "req-8");
                assert!(log.is_none(), "expected log: None, got {log:?}");
            }
            other => panic!("expected GhFailureLogFetched, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cwd_outside_canonical_root_replies_with_safe_default() {
        // Acceptance: cwd outside the runner's canonical --cwd must NOT
        // execute. Reply uses the variant's "no result" shape (None/empty)
        // rather than a free-form error so existing protocol parsers stay
        // strict.
        let root = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(root.path().to_path_buf());

        // Build a sibling tempdir that is OUTSIDE the runner's canonical root.
        let outside = TempDir::new().unwrap();
        git_init_with_commit(outside.path(), "master");

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GetDefaultBranch {
                req_id: "req-9".into(),
                cwd: outside.path().display().to_string(),
            },
        )
        .await;
        match reply {
            WireMessage::DefaultBranchResolved { req_id, branch } => {
                assert_eq!(req_id, "req-9");
                assert!(
                    branch.is_none(),
                    "outside-root cwd must NOT resolve, got {branch:?}"
                );
            }
            other => panic!("expected DefaultBranchResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cwd_outside_canonical_root_for_merge_replies_other_error() {
        let root = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(root.path().to_path_buf());
        let outside = TempDir::new().unwrap();
        git_init_with_commit(outside.path(), "master");

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeBranch {
                req_id: "req-10".into(),
                cwd: outside.path().display().to_string(),
                target: "master".into(),
                task_branch: "feature/x".into(),
            },
        )
        .await;
        // Merge has no "no result" — it must surface the rejection as
        // MergeOutcome::Other so the SaaS side can map it to 5xx.
        match reply {
            WireMessage::MergeResult {
                req_id,
                outcome: MergeOutcome::Other { stderr },
            } => {
                assert_eq!(req_id, "req-10");
                assert!(stderr.contains("outside runner root"), "stderr={stderr}");
            }
            other => panic!("expected MergeResult::Other, got {other:?}"),
        }
    }
}
