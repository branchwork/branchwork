//! End-to-end test for the supervisor-crash / orphan handling path.
//!
//! Acceptance criterion 3.4: spawn a real PTY agent, SIGKILL the
//! supervisor daemon, and observe two things:
//!
//! 1. The agent row flips to `failed` / `stop_reason='supervisor_unreachable'`
//!    and the dashboard broadcasts an `agent_stopped` event so connected
//!    task cards unlock without waiting for a manual refresh.
//! 2. After the test restarts the server, the row stays terminal — no
//!    zombie `running` row creeps back in via cleanup_and_reattach.
//!
//! Why we drive the *EOF* path (not the 45 s heartbeat poll): SIGKILL
//! on the daemon PID closes its supervisor socket immediately, which
//! makes the server's reader task observe EOF and run `on_agent_exit`.
//! That handler reads the daemon's pidfile presence as a crash signal
//! (the daemon would have removed the pidfile during its orderly
//! shutdown) and writes `stop_reason='supervisor_unreachable'` — same
//! terminal state the heartbeat-timeout path produces, but in <1 s
//! instead of ~45 s. The acceptance criterion is "Times out in <60 s",
//! so the EOF path is the only realistic choice; the heartbeat path
//! itself is unit-tested in `agents/pty_agent.rs::tests`.
//!
//! Unix-only: the bash stub claude needs Unix process semantics, and
//! `kill -9` is used to terminate the daemon. The same posture as
//! `tests/unattended_auto_mode_e2e.rs` and
//! `tests/check_prompt_pruned_branch.rs`.

#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PLAN_NAME: &str = "orphan-fixture";
const PROJECT_NAME: &str = "project";
const TASK_NUMBER: &str = "1.1";

/// Plan YAML — single phase, single task. Auto-mode is left off so
/// the supervisor crash test runs in isolation, without the auto-mode
/// chain trying to merge a task that never finished.
fn plan_yaml() -> String {
    format!(
        "title: orphan fixture\n\
         context: ''\n\
         project: {PROJECT_NAME}\n\
         phases:\n  \
         - number: 1\n    \
           title: Phase 1\n    \
           description: ''\n    \
           tasks:\n      \
           - number: '{TASK_NUMBER}'\n        \
             title: Task 1.1\n        \
             description: ''\n        \
             acceptance: ''\n",
    )
}

/// Bash stub for `claude`. It must:
///   1. Tolerate the flags Claude's spawn_args emits (we ignore values
///      we don't need so the daemon's argv doesn't trip `set -u`).
///   2. Write a marker line so the supervisor's reader observes data
///      flowing — proves the PTY pipe is wired before we kill anything.
///   3. Block on the PTY stdin until EOF. When the test SIGKILLs the
///      daemon, the PTY master closes; `cat` sees EOF on stdin and
///      exits, the bash script exits, the PTY child is gone — but the
///      damage is already done: the daemon was SIGKILL'd, so its
///      pidfile is still on disk and on_agent_exit reads that as a
///      crash. The whole point of holding the PTY is to keep the
///      daemon alive *until* the kill.
const STUB_SCRIPT: &str = r#"#!/usr/bin/env bash
set -e
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id|--add-dir|--effort|--mcp-config|--settings|--max-budget-usd) shift 2 ;;
        *) shift ;;
    esac
done

echo "stub claude alive"
# Block until the PTY master closes (i.e. supervisor death). `cat` reads
# stdin until EOF; on PTY teardown the slave returns EOF and we exit
# cleanly. Without this the script would race ahead and the supervisor
# would shut itself down before the test gets a chance to SIGKILL it.
cat > /dev/null
"#;

/// Per-test fixture. Owns the tempdir, the server child, and enough
/// state to spawn a *second* server child against the same scratch
/// directory so the post-restart assertion can run under a fresh
/// process without re-creating the plan / DB / project.
struct Fixture {
    /// Tempdir kept alive for the fixture's lifetime. Survives the
    /// server restart so the second boot sees the same DB + project.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    /// Scratch project dir — only used inside `new()` to set up the
    /// initial git repo, but kept on the struct for `TEST_SERVER_LOG`
    /// debugging.
    #[allow(dead_code)]
    project: PathBuf,
    /// Plans directory — only used inside `new()` to drop the YAML
    /// plan file. Kept on the struct for the same debugging reason.
    #[allow(dead_code)]
    plans_dir: PathBuf,
    db_path: PathBuf,
    port: u16,
    base_url: String,
    claude_dir: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    path_var: String,
    /// Wrapped in Option so `restart_server` can take + drop the old
    /// child before spawning a new one without `Drop` racing.
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let claude_dir = dir.path().join(".claude");
        let plans_dir = claude_dir.join("plans");
        let project = dir.path().join(PROJECT_NAME);
        let stub_bin = dir.path().join("stubbin");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&stub_bin).unwrap();
        std::fs::create_dir_all(&claude_dir).unwrap();

        // Initialise the scratch project so `git_checkout_branch`
        // inside `start_pty_agent` has somewhere to land.
        run_git(&project, &["init", "-q", "-b", "master"]);
        run_git(&project, &["config", "user.email", "test@orphan.local"]);
        run_git(&project, &["config", "user.name", "Orphan Test"]);
        std::fs::write(project.join("README.md"), "test project\n").unwrap();
        run_git(&project, &["add", "README.md"]);
        run_git(&project, &["commit", "-q", "-m", "initial"]);

        // Drop the stub claude in stubbin/ and chmod +x.
        let stub_path = stub_bin.join("claude");
        std::fs::write(&stub_path, STUB_SCRIPT).unwrap();
        let mut perms = std::fs::metadata(&stub_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_path, perms).unwrap();

        std::fs::write(plans_dir.join(format!("{PLAN_NAME}.yaml")), plan_yaml()).unwrap();

        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");
        let bin: PathBuf = env!("CARGO_BIN_EXE_branchwork-server").into();

        // Prepend stubbin to PATH so the daemon resolves `claude` to our shim.
        let mut path_var = stub_bin.to_string_lossy().to_string();
        if let Ok(existing) = std::env::var("PATH") {
            path_var.push(':');
            path_var.push_str(&existing);
        }

        let home = dir.path().to_path_buf();
        let child = spawn_server(&bin, port, &claude_dir, &home, &path_var);
        wait_healthy(&base_url);

        let db_path = claude_dir.join("branchwork.db");
        Self {
            dir,
            project,
            plans_dir,
            db_path,
            port,
            base_url,
            claude_dir,
            home,
            bin,
            path_var,
            child: Some(child),
        }
    }

    /// Kill the current server child and spawn a new one against the
    /// same `claude_dir` / port. The new server's `cleanup_and_reattach`
    /// runs on boot and is what drives the post-restart assertion.
    fn restart_server(&mut self) {
        if let Some(mut c) = self.child.take() {
            // SIGKILL: the production code path being tested here cares
            // about clean state-machine recovery, not about the server
            // itself doing graceful shutdown. SIGKILL also matches the
            // worst-case scenario the test is meant to model.
            let _ = c.kill();
            let _ = c.wait();
        }
        // Linux's TCP stack puts the just-released listening port into
        // a brief TIME_WAIT-ish state; without SO_REUSEADDR the new
        // bind() can race. The server uses SO_REUSEADDR by default in
        // axum's listener, but a small sleep eliminates the residual
        // race in CI and is dwarfed by the test budget anyway.
        std::thread::sleep(Duration::from_millis(200));
        let new_child = spawn_server(
            &self.bin,
            self.port,
            &self.claude_dir,
            &self.home,
            &self.path_var,
        );
        self.child = Some(new_child);
        wait_healthy(&self.base_url);
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        http("POST", &format!("{}{path}", self.base_url), Some(body))
    }

    fn put(&self, path: &str, body: Value) -> (u16, Value) {
        http("PUT", &format!("{}{path}", self.base_url), Some(body))
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db_path).expect("open db")
    }

    fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/ws", self.port)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_server(bin: &Path, port: u16, claude_dir: &Path, home: &Path, path_var: &str) -> Child {
    Command::new(bin)
        .args([
            "--port",
            &port.to_string(),
            "--claude-dir",
            &claude_dir.to_string_lossy(),
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", path_var)
        .stdout(if std::env::var("TEST_SERVER_LOG").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stderr(if std::env::var("TEST_SERVER_LOG").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .expect("spawn branchwork-server")
}

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    if !out.status.success() {
        panic!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

fn wait_healthy(base_url: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let (s, _) = http("GET", &format!("{base_url}/api/health"), None);
        if s == 200 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server at {base_url} never became healthy");
}

fn http(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-o",
        "-",
        "-w",
        "\n\n__STATUS__:%{http_code}",
        "-X",
        method,
        "-H",
        "Content-Type: application/json",
        url,
    ]);
    let body_str;
    if let Some(b) = body {
        body_str = serde_json::to_string(&b).unwrap();
        cmd.args(["-d", &body_str]);
    }
    let out = cmd.output().unwrap_or_else(|e| panic!("curl: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body_str, status_str) = stdout
        .rsplit_once("\n\n__STATUS__:")
        .unwrap_or_else(|| panic!("bad curl output: {stdout}"));
    let status: u16 = status_str.trim().parse().unwrap_or(0);
    let value: Value = if body_str.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body_str).unwrap_or(Value::String(body_str.to_string()))
    };
    (status, value)
}

/// Spawn a thread that subscribes to the dashboard WS and accumulates
/// every text frame into the shared `events` Vec. Stops when `stop` is
/// flipped or when the WS connection ends. Returns the join handle so
/// the test can wait for clean shutdown.
fn spawn_ws_collector(
    ws_url: String,
    events: Arc<Mutex<Vec<String>>>,
    connected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for WS collector");
        rt.block_on(async move {
            use futures_util::StreamExt;
            use tokio_tungstenite::tungstenite::Message;
            let (ws, _resp) = match tokio_tungstenite::connect_async(&ws_url).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[ws collector] connect failed: {e}");
                    return;
                }
            };
            let (_write, mut read) = ws.split();
            connected.store(true, Ordering::SeqCst);
            // Poll with a short timeout so the stop flag can be honoured
            // even when the broadcast channel is quiet.
            while !stop.load(Ordering::SeqCst) {
                match tokio::time::timeout(Duration::from_millis(100), read.next()).await {
                    Ok(Some(Ok(Message::Text(t)))) => {
                        events.lock().unwrap().push(t.to_string());
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(_))) => break,
                    Ok(None) => break,
                    Err(_) => continue, // recv timeout, loop and re-check stop
                }
            }
        });
    })
}

/// Read the supervisor's daemon PID from the agents row. Returns `None`
/// while the row is still in `starting` (pid not yet written) or absent.
fn agent_status_and_pid(
    db: &rusqlite::Connection,
    agent_id: &str,
) -> (Option<String>, Option<i64>) {
    db.query_row(
        "SELECT status, pid FROM agents WHERE id = ?1",
        rusqlite::params![agent_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
            ))
        },
    )
    .unwrap_or((None, None))
}

fn agent_stop_reason(db: &rusqlite::Connection, agent_id: &str) -> Option<String> {
    db.query_row(
        "SELECT stop_reason FROM agents WHERE id = ?1",
        rusqlite::params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// Send SIGKILL to a Unix PID. We shell out to `kill -9` to keep the
/// test free of libc usage, mirroring the existing test files which
/// avoid platform-specific direct syscalls.
fn sigkill(pid: i64) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

/// Headline test: kill the supervisor; assert
/// `failed`/`supervisor_unreachable`; assert `agent_stopped` broadcast;
/// restart server; assert no zombie `running` row.
#[test]
fn supervisor_kill_marks_unreachable_and_survives_restart() {
    let mut fx = Fixture::new();

    // 1. Subscribe to the dashboard WS *before* we kick anything off so
    //    we don't miss the agent_stopped broadcast that fires when
    //    on_agent_exit flips the row.
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let connected = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let collector =
        spawn_ws_collector(fx.ws_url(), events.clone(), connected.clone(), stop.clone());
    // Wait for the collector to actually be subscribed; otherwise the
    // first events out of the broadcast channel can race ahead of the
    // WS handshake.
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    while !connected.load(Ordering::SeqCst) && Instant::now() < connect_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        connected.load(Ordering::SeqCst),
        "WS collector never connected"
    );

    // 2. Map the plan to our scratch project so spawn lands in the right
    //    cwd (`<HOME>/project`). The plan YAML's bare `project: project`
    //    field works alone, but PUT'ing the project endpoint also seeds
    //    the `plan_project` table which downstream paths read.
    let (s, _) = fx.put(
        &format!("/api/plans/{PLAN_NAME}/project"),
        json!({ "project": PROJECT_NAME }),
    );
    assert_eq!(s, 200);

    // 3. Spawn the agent.
    let (s, body) = fx.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": TASK_NUMBER,
        }),
    );
    assert_eq!(s, 200, "start-task failed: {body}");
    let agent_id = body
        .get("agentId")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("agent_id").and_then(|v| v.as_str()))
        .or_else(|| body.get("id").and_then(|v| v.as_str()))
        .expect("response carries an agent id")
        .to_string();
    assert!(!agent_id.is_empty(), "empty agent id: {body}");

    // 4. Wait for the row to flip from `starting` to `running` AND for
    //    the supervisor pid to be written. start_pty_agent does both
    //    in a single UPDATE after spawn_session_daemon returns the pid.
    //    Generous deadline (10 s) to absorb cold-cache spawn latency.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pid: Option<i64> = None;
    while Instant::now() < deadline {
        let conn = fx.db();
        let (status, p) = agent_status_and_pid(&conn, &agent_id);
        drop(conn);
        if status.as_deref() == Some("running") && p.is_some() {
            pid = p;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pid = pid.unwrap_or_else(|| {
        let conn = fx.db();
        let (status, p) = agent_status_and_pid(&conn, &agent_id);
        panic!("agent {agent_id} never reached running with pid; status={status:?} pid={p:?}");
    });

    // 5. SIGKILL the supervisor daemon. Kernel closes its PTY +
    //    listening socket; the server-side reader hits EOF and runs
    //    on_agent_exit. The pidfile sibling is still on disk because
    //    the daemon never got a chance to clean it up — that's the
    //    crash signal on_agent_exit reads as `supervisor_unreachable`.
    sigkill(pid);

    // 6. Poll for the row to flip to `failed` / supervisor_unreachable.
    //    EOF detection is sub-second on Linux; 30 s is huge headroom
    //    even on slow CI.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut final_status: Option<String> = None;
    let mut final_reason: Option<String> = None;
    while Instant::now() < deadline {
        let conn = fx.db();
        let (s, _) = agent_status_and_pid(&conn, &agent_id);
        let r = agent_stop_reason(&conn, &agent_id);
        drop(conn);
        if matches!(s.as_deref(), Some("failed" | "killed")) && r.is_some() {
            final_status = s;
            final_reason = r;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        final_status.as_deref(),
        Some("failed"),
        "agent did not flip to failed; reason={final_reason:?}"
    );
    assert_eq!(
        final_reason.as_deref(),
        Some("supervisor_unreachable"),
        "expected supervisor_unreachable, got {final_reason:?}"
    );

    // 7. Verify the agent_stopped broadcast went out. We poll the
    //    accumulated event log for an envelope of the right shape —
    //    `{"type":"agent_stopped","data":{"id":<agent>,"status":"failed",
    //    "stop_reason":"supervisor_unreachable", ...}, "timestamp":...}`.
    //    Up to 2 s of additional slack lets the broadcast traverse the
    //    runtime → tungstenite → mutex pipeline before we look.
    let deadline = Instant::now() + Duration::from_secs(2);
    let agent_stopped_seen = loop {
        let snap = events.lock().unwrap().clone();
        if snap.iter().any(|raw| {
            let v: Value = match serde_json::from_str(raw) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("type").and_then(|t| t.as_str()) == Some("agent_stopped")
                && v.get("data")
                    .and_then(|d| d.get("id"))
                    .and_then(|i| i.as_str())
                    == Some(agent_id.as_str())
                && v.get("data")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                    == Some("supervisor_unreachable")
        }) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let snap = events.lock().unwrap().clone();
    assert!(
        agent_stopped_seen,
        "no agent_stopped/supervisor_unreachable broadcast seen; events={snap:?}"
    );

    // Stop the WS collector before restart so it doesn't trip on the
    // server going away. Best-effort join — the collector loop checks
    // `stop` every 100 ms.
    stop.store(true, Ordering::SeqCst);
    let _ = collector.join();

    // 8. Restart the server. cleanup_and_reattach runs on boot. Because
    //    the agents row is already `failed` (terminal), the cleanup
    //    pass has nothing to do for this row — the `WHERE status IN
    //    ('running', 'starting')` filter excludes it. This assertion
    //    catches a regression where a future change to cleanup somehow
    //    flips terminal rows back to running.
    fx.restart_server();

    let conn = fx.db();
    let (status, _pid) = agent_status_and_pid(&conn, &agent_id);
    let reason = agent_stop_reason(&conn, &agent_id);
    drop(conn);
    assert_eq!(
        status.as_deref(),
        Some("failed"),
        "row regressed to {status:?} after restart"
    );
    assert_eq!(
        reason.as_deref(),
        Some("supervisor_unreachable"),
        "stop_reason changed to {reason:?} after restart"
    );

    // 9. Sanity: no other row for this agent slipped into running/
    //    starting via a stray cleanup path. The DB count should be
    //    exactly one row, status=failed.
    let conn = fx.db();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE id = ?1 AND status IN ('running', 'starting')",
            rusqlite::params![agent_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(-1);
    drop(conn);
    assert_eq!(
        count, 0,
        "post-restart sweep left a zombie running/starting row for {agent_id}"
    );
}
