//! Mini-supervisor daemon: owns one PTY plus one local socket listener per
//! agent, replacing tmux as the agent persistence layer.
//!
//! Invoked two ways, both of which land in [`run_session`]:
//! - `orchestrai-server session --socket <s> --cwd <d> -- <cmd> <args...>`
//!   (the subcommand dispatched from `main.rs`).
//! - The standalone `session_daemon` binary in `src/bin/session_daemon.rs`.
//!
//! On Unix we fork + setsid so the daemon is re-parented to init and keeps
//! running after the server process (its parent) dies. Windows detachment
//! is handled separately in task 0.3.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Args;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

use super::session_protocol::{self, Message};

#[derive(Args, Debug, Clone)]
pub struct SessionArgs {
    /// Path to the Unix socket (or named pipe, on Windows) the daemon listens on.
    #[arg(long)]
    pub socket: PathBuf,

    /// Working directory the PTY command is spawned in.
    #[arg(long)]
    pub cwd: PathBuf,

    /// Initial PTY columns.
    #[arg(long, default_value_t = 120)]
    pub cols: u16,

    /// Initial PTY rows.
    #[arg(long, default_value_t = 40)]
    pub rows: u16,

    /// Command (and args) to run inside the PTY, after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 1..,
        required = true,
    )]
    pub cmd: Vec<String>,
}

/// Detach from the parent and run the daemon until the PTY exits or a
/// `Kill` arrives. Blocks the calling thread.
pub fn run_session(args: SessionArgs) -> io::Result<()> {
    detach_from_parent()?;
    run_session_in_place(args)
}

/// Same as [`run_session`] but without the fork/setsid dance — for tests
/// and for hosts that have already detached the process themselves.
pub fn run_session_in_place(args: SessionArgs) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_daemon(args))
}

#[cfg(unix)]
fn detach_from_parent() -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // Fork once, setsid in the child, then close std fds so the daemon
    // doesn't hold the invoker's controlling terminal open.
    unsafe {
        match libc::fork() {
            n if n < 0 => return Err(io::Error::last_os_error()),
            n if n > 0 => libc::_exit(0),
            _ => {}
        }
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Ok(devnull) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            let fd = devnull.as_raw_fd();
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn detach_from_parent() -> io::Result<()> {
    // Task 0.3 handles DETACHED_PROCESS at CreateProcess time; the daemon
    // itself has nothing to do once it's already been spawned detached.
    Ok(())
}

async fn run_daemon(args: SessionArgs) -> io::Result<()> {
    let SessionArgs {
        socket,
        cwd,
        cols,
        rows,
        cmd,
    } = args;
    if cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing command to run",
        ));
    }

    // A stale socket from a previous crash would make bind() fail with EADDRINUSE.
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;

    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::other(format!("openpty: {e}")))?;

    let mut builder = CommandBuilder::new(&cmd[0]);
    for a in &cmd[1..] {
        builder.arg(a);
    }
    builder.cwd(&cwd);
    builder.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| io::Error::other(format!("spawn: {e}")))?;
    drop(pair.slave);

    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::other(format!("clone reader: {e}")))?;
    let mut pty_writer = pair
        .master
        .take_writer()
        .map_err(|e| io::Error::other(format!("take writer: {e}")))?;
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));

    // Every byte the PTY emits gets appended to this log, so a client that
    // reconnects after a crash still has the full transcript on disk.
    let log_path = socket.with_extension("log");
    let log_file = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?,
    ));

    let (out_tx, _) = broadcast::channel::<Vec<u8>>(1024);
    let (shutdown_tx, _) = broadcast::channel::<()>(4);
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // PTY → log + all connected clients. The read itself is blocking, so it
    // runs on a dedicated OS thread.
    {
        let out_tx = out_tx.clone();
        let log_file = log_file.clone();
        let shutdown_tx = shutdown_tx.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            loop {
                match pty_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        if let Ok(mut f) = log_file.lock() {
                            let _ = f.write_all(&chunk);
                            let _ = f.flush();
                        }
                        // send() fails when there are no subscribers; that's
                        // the normal state when no client is attached.
                        let _ = out_tx.send(chunk);
                    }
                    Err(_) => break,
                }
            }
            let _ = shutdown_tx.send(());
        });
    }

    // Client input → PTY. Serializing through a sync mpsc keeps writes
    // atomic-per-frame even when multiple clients are connected.
    std::thread::spawn(move || {
        use std::io::Write;
        while let Ok(bytes) = in_rx.recv() {
            if pty_writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = pty_writer.flush();
        }
    });

    let mut shutdown_rx = shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
                    let out_rx = out_tx.subscribe();
                    let in_tx = in_tx.clone();
                    let master = master.clone();
                    let child = child.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tokio::spawn(async move {
                        handle_client(stream, out_rx, in_tx, master, child, shutdown_tx).await;
                    });
                }
                Err(_) => break,
            },
        }
    }

    // Best-effort cleanup: kill the child, remove the socket. The log file
    // is already flushed on every chunk.
    {
        let mut c = child.lock().unwrap();
        let _ = c.kill();
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    mut out_rx: broadcast::Receiver<Vec<u8>>,
    in_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    shutdown_tx: broadcast::Sender<()>,
) {
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

    let mut reader_task = {
        let write_half = write_half.clone();
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            loop {
                let msg = match session_protocol::read_frame(&mut reader).await {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    Message::Input(bytes) => {
                        if in_tx.send(bytes).is_err() {
                            break;
                        }
                    }
                    Message::Resize { cols, rows } => {
                        if let Ok(m) = master.lock() {
                            let _ = m.resize(PtySize {
                                cols,
                                rows,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                    }
                    Message::Kill => {
                        if let Ok(mut c) = child.lock() {
                            let _ = c.kill();
                        }
                        let _ = shutdown_tx.send(());
                        break;
                    }
                    Message::Ping => {
                        let mut w = write_half.lock().await;
                        if session_protocol::write_frame(&mut *w, &Message::Pong)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Message::Output(_) | Message::Pong => {}
                }
            }
        })
    };

    let mut writer_task = {
        let write_half = write_half.clone();
        tokio::spawn(async move {
            loop {
                match out_rx.recv().await {
                    Ok(bytes) => {
                        let mut w = write_half.lock().await;
                        if session_protocol::write_frame(&mut *w, &Message::Output(bytes))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    tokio::select! {
        _ = &mut reader_task => writer_task.abort(),
        _ = &mut writer_task => reader_task.abort(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn wait_for_socket(path: &std::path::Path) {
        for _ in 0..50 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    #[tokio::test]
    async fn daemon_proxies_pty_output_to_client() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("test.sock");
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            // Delay the print so the client has time to connect and subscribe
            // to the broadcast. Output emitted before anyone's listening is
            // intentionally dropped (the on-disk log is the historical record).
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "sleep 0.4; printf 'hello-from-pty\\n'; sleep 0.2".into(),
            ],
        };

        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;
        let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf: Vec<u8> = Vec::new();
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(
                Duration::from_millis(300),
                session_protocol::read_frame(&mut client),
            )
            .await
            {
                Ok(Ok(Message::Output(b))) => buf.extend(b),
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
            if buf
                .windows(b"hello-from-pty".len())
                .any(|w| w == b"hello-from-pty")
            {
                break;
            }
        }
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;

        assert!(
            buf.windows(b"hello-from-pty".len())
                .any(|w| w == b"hello-from-pty"),
            "expected PTY output to include 'hello-from-pty', got: {:?}",
            String::from_utf8_lossy(&buf),
        );
    }

    #[tokio::test]
    async fn daemon_writes_to_log_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("log.sock");
        let log_path = socket.with_extension("log");
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'logged-line\\n'".into(),
            ],
        };
        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;

        // Let the PTY run to completion (child exits → reader sees EOF →
        // daemon shuts down).
        let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;

        let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            contents.contains("logged-line"),
            "expected log to contain 'logged-line', got: {contents:?}",
        );
    }

    #[tokio::test]
    async fn daemon_accepts_reconnect_after_client_drops() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("reconnect.sock");
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            // Long-running: two prints spaced out, so the second one arrives
            // only after the first client has dropped.
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'first\\n'; sleep 0.4; printf 'second\\n'; sleep 0.2".into(),
            ],
        };
        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;
        {
            let mut c1 = tokio::net::UnixStream::connect(&socket).await.unwrap();
            // Drain a couple of frames so the broadcast receiver is exercised,
            // then drop.
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                session_protocol::read_frame(&mut c1),
            )
            .await;
        }

        // Second connection must still work.
        let mut c2 = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut buf: Vec<u8> = Vec::new();
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(
                Duration::from_millis(300),
                session_protocol::read_frame(&mut c2),
            )
            .await
            {
                Ok(Ok(Message::Output(b))) => buf.extend(b),
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
            if buf.windows(6).any(|w| w == b"second") {
                break;
            }
        }
        drop(c2);
        let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;

        assert!(
            buf.windows(6).any(|w| w == b"second"),
            "expected reconnect to see live output; got: {:?}",
            String::from_utf8_lossy(&buf),
        );
    }
}
