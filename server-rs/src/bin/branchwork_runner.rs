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
#[path = "../saas/outbox.rs"]
mod outbox;
#[path = "../saas/runner_protocol.rs"]
mod runner_protocol;
#[path = "../agents/session_protocol.rs"]
mod session_protocol;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rusqlite::Connection;
use tokio::sync::{Mutex, mpsc};

use runner_protocol::{DriverAuthInfo, DriverAuthStatus, Envelope, FolderEntry, WireMessage};

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
        // saas→runner variants the runner doesn't act on yet (handlers
        // land in later phases of the folder-listing plan).
        | WireMessage::TerminalReplay { .. }
        | WireMessage::CreateFolder { .. }
        | WireMessage::Ping {} => {}
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
}
