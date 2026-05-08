//! Integration test for the initial-resize gate (plan
//! `dashboard-stability` task 4.1).
//!
//! The supervisor daemon spawns the agent's PTY at the default 120×40 so
//! that auto-mode / MCP / detached spawns still work without a dashboard
//! attached. Before this fix landed, the daemon's PTY reader started
//! draining the kernel buffer immediately — so the bytes Claude Code
//! emitted at startup (boot banner, first paint) were forwarded to
//! whatever client eventually attached, even when that client was a
//! sidebar-collapsed dashboard that wanted a much narrower viewport. The
//! result was double-rendered scrollback and a flash of 120-wide content
//! that wrapped onto multiple lines in the panel.
//!
//! The fix in `supervisor.rs` holds the PTY reader on a sync gate until
//! either (a) the first `Resize` message arrives from a dashboard client
//! and is applied via `master.resize()`, or (b) a 500 ms grace timeout
//! expires.
//!
//! Two scenarios pin the behaviour:
//!
//! 1. `resize_landing_during_grace_unblocks_pty_reader` — a dashboard
//!    client connects before the grace expires, sends `Resize{80, 24}`,
//!    and then sends a single byte of stdin to drive the substitute's
//!    first read. The substitute observes the post-resize geometry and
//!    the client receives the printed result. Demonstrates the resize
//!    + input plumbing still works through the gate.
//!
//! 2. `pty_reader_held_for_grace_when_no_resize_arrives` — the canonical
//!    regression test. The substitute prints "ready" at startup, before
//!    any client attaches. The test connects ~150 ms later (well after
//!    the substitute has emitted its bytes, but well before the 500 ms
//!    grace expires) and asserts the bytes still arrive. Without the
//!    gate the daemon would have already broadcast to zero subscribers
//!    — `tokio::sync::broadcast::Sender::send` drops the value when no
//!    receivers are active — and the client would see nothing.

#![cfg(unix)]

#[path = "../src/agents/session_protocol.rs"]
mod session_protocol;

// `#[allow(dead_code)]` here covers the supervisor entry points the test
// doesn't reach (`run_session`, `detach_from_parent`, `spawn_detached`,
// …). They're load-bearing in the main binary but unreachable from this
// integration test, which uses `run_session_in_place` directly.
#[allow(dead_code)]
#[path = "../src/agents/supervisor.rs"]
mod supervisor;

use std::path::Path;
use std::time::{Duration, Instant};

use interprocess::local_socket::ConnectOptions;
use interprocess::local_socket::tokio::Stream as LocalStream;
use interprocess::local_socket::tokio::prelude::*;

use session_protocol::Message;
use supervisor::{SessionArgs, run_session_in_place, socket_name};

/// Substitute that blocks on a single line of stdin, then prints the
/// current TIOCGWINSZ via `stty size` (output: `<rows> <cols>`), then
/// sleeps. The substitute must wait for input rather than printing
/// immediately so the test can drive the daemon's frame ordering
/// deterministically (resize → input → response). `read line` covers
/// the canonical-mode line discipline (`ICANON`) /bin/sh inherits from
/// the freshly-opened PTY — `dd bs=1 count=1` would also work but only
/// if we sent a newline, which `read line` makes explicit.
fn substitute_echoes_winsize_on_stdin() -> Vec<String> {
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "read line; stty size; sleep 30".into(),
    ]
}

/// Substitute that prints a marker immediately at startup, then sleeps.
/// Used by the gate-timeout regression test below — we want bytes in the
/// kernel PTY buffer before any client connects.
fn substitute_immediate_print() -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), "echo ready; sleep 30".into()]
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..100 {
        if socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("socket never appeared at {}", socket.display());
}

async fn connect(socket: &Path) -> LocalStream {
    let name = socket_name(socket).expect("socket name");
    ConnectOptions::new()
        .name(name)
        .connect_tokio()
        .await
        .expect("connect local socket")
}

/// Run the daemon on a dedicated OS thread. `run_session_in_place`
/// builds its own current-thread tokio runtime; we deliberately skip
/// `run_session` (which forks + setsid) so the daemon stays a child of
/// the test process and a Kill frame at teardown actually shuts it down.
fn spawn_daemon(args: SessionArgs) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = run_session_in_place(args);
    })
}

/// Read frames from `read_half` until `needle` appears in the
/// concatenated `Output` stream or `deadline` elapses. Returns whatever
/// has been collected so the caller can panic with the full buffer.
async fn drain_until<R>(read_half: &mut R, needle: &str, deadline: Instant) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match tokio::time::timeout(
            Duration::from_millis(300),
            session_protocol::read_frame(read_half),
        )
        .await
        {
            Ok(Ok(Message::Output(b))) => buf.extend(b),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => {}
        }
        if String::from_utf8_lossy(&buf).contains(needle) {
            break;
        }
    }
    buf
}

#[tokio::test(flavor = "multi_thread")]
async fn resize_landing_during_grace_unblocks_pty_reader() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("pty-resize.sock");

    let _daemon = spawn_daemon(SessionArgs {
        socket: socket.clone(),
        cwd: std::env::temp_dir(),
        cols: 120,
        rows: 40,
        env: Vec::new(),
        cmd: substitute_echoes_winsize_on_stdin(),
    });

    wait_for_socket(&socket).await;
    let stream = connect(&socket).await;
    let (mut read_half, mut write_half) = stream.split();

    // Resize first — `handle_client` processes the local-socket frames
    // serially, so the kernel-side `master.resize()` lands before we
    // unblock the substitute's stdin read with the byte below.
    session_protocol::write_frame(&mut write_half, &Message::Resize { cols: 80, rows: 24 })
        .await
        .expect("write resize");
    // Small settle so the kernel has fully applied the resize before
    // the substitute's `stty size` reads it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    // `\n` (carriage return is what a real Enter keystroke delivers
    // through a PTY, but a literal LF also satisfies `read line` in
    // /bin/sh's canonical mode).
    session_protocol::write_frame(&mut write_half, &Message::Input(b"x\n".to_vec()))
        .await
        .expect("write input");

    let buf = drain_until(
        &mut read_half,
        "24 80",
        Instant::now() + Duration::from_secs(5),
    )
    .await;

    // Tear down the daemon so its supervisor thread exits and the
    // tempdir can be reaped.
    let _ = session_protocol::write_frame(&mut write_half, &Message::Kill).await;

    let text = String::from_utf8_lossy(&buf).to_string();
    assert!(
        text.contains("24 80"),
        "expected substitute to observe resized PTY (24 80), got: {text:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pty_reader_held_for_grace_when_no_resize_arrives() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("pty-grace.sock");

    let _daemon = spawn_daemon(SessionArgs {
        socket: socket.clone(),
        cwd: std::env::temp_dir(),
        cols: 120,
        rows: 40,
        env: Vec::new(),
        cmd: substitute_immediate_print(),
    });

    wait_for_socket(&socket).await;

    // Sleep generously past substitute startup so its "ready" bytes are
    // definitely sitting in the kernel PTY buffer. Without the gate the
    // daemon's reader has long since drained them and broadcast to a
    // zero-subscriber channel — those bytes are unrecoverable. With the
    // gate the daemon hasn't read yet, so we get a chance to subscribe
    // before anything is broadcast.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let stream = connect(&socket).await;
    let (mut read_half, mut write_half) = stream.split();

    // Cap the wait at 2 s — well above the 500 ms grace timeout, well
    // under the substitute's `sleep 30`, so a regression that loses the
    // bytes shows up as a deterministic timeout rather than a flake.
    let buf = drain_until(
        &mut read_half,
        "ready",
        Instant::now() + Duration::from_secs(2),
    )
    .await;

    let _ = session_protocol::write_frame(&mut write_half, &Message::Kill).await;

    let text = String::from_utf8_lossy(&buf).to_string();
    assert!(
        text.contains("ready"),
        "expected ready marker forwarded after gate timeout, got: {text:?}"
    );
}
