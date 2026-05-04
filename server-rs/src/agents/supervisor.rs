//! Mini-supervisor daemon: owns one PTY plus one local socket listener per
//! agent, replacing tmux as the agent persistence layer.
//!
//! Invoked two ways, both of which land in [`run_session`]:
//! - `branchwork-server session --socket <s> --cwd <d> -- <cmd> <args...>`
//!   (the subcommand dispatched from `main.rs`).
//! - The standalone `session_daemon` binary in `src/bin/session_daemon.rs`.
//!
//! On Unix we fork + setsid so the daemon is re-parented to init and keeps
//! running after the server process (its parent) dies. On Windows we rely on
//! the parent spawning us with `CREATE_NO_WINDOW | DETACHED_PROCESS` (see
//! [`spawn_detached`]); no in-process detach step is required.
//!
//! The local IPC layer is abstracted through the `interprocess` crate's
//! `local_socket` module: Unix domain sockets on Unix, named pipes
//! (`\\.\pipe\oai-<stem>`) on Windows. The same framed protocol runs over
//! both.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Args;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{
    GenericFilePath, ListenerOptions, Name,
    tokio::{Listener as LocalListener, Stream as LocalStream},
};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tokio::sync::broadcast;

use super::session_protocol::{self, Message};

#[derive(Args, Debug, Clone)]
pub struct SessionArgs {
    /// Path to the local socket (or named pipe, on Windows) the daemon listens on.
    ///
    /// On Unix the path itself is the socket file. On Windows we derive a
    /// named-pipe name (`\\.\pipe\<file-stem>`) from the basename and use the
    /// original path for the on-disk transcript (`<path>.log`).
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
    // The parent is responsible for passing CREATE_NO_WINDOW | DETACHED_PROCESS
    // to CreateProcess (see [`spawn_detached`]). Once we're here the OS has
    // already severed the console association.
    Ok(())
}

/// Turn a socket `PathBuf` into an `interprocess` local-socket [`Name`] that
/// can be handed to `ListenerOptions` / `ConnectOptions`.
///
/// On Unix the path is used as-is (Unix domain socket). On Windows we derive
/// a named-pipe name of the form `\\.\pipe\<file-stem>`; the supplied path
/// itself is only used by callers that want a matching on-disk log file.
pub fn socket_name(socket: &Path) -> io::Result<Name<'static>> {
    #[cfg(windows)]
    {
        let stem = socket.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path has no basename")
        })?;
        let pipe = format!(r"\\.\pipe\{stem}");
        Ok(pipe.to_fs_name::<GenericFilePath>()?.into_owned())
    }
    #[cfg(unix)]
    {
        Ok(socket
            .to_path_buf()
            .to_fs_name::<GenericFilePath>()?
            .into_owned())
    }
}

/// Spawn the session daemon (or any helper binary) detached from the calling
/// process. On Windows this sets `CREATE_NO_WINDOW | DETACHED_PROCESS` so the
/// child has no console and survives when the parent exits. On Unix the
/// daemon itself calls [`detach_from_parent`] so nothing extra is needed
/// here.
///
/// Returns the PID of the directly spawned process. Note: on Unix the
/// daemon forks inside [`run_session`], so the returned PID is the
/// short-lived fork-parent's; the real daemon PID appears later in the
/// pidfile written by [`write_pidfile`]. Callers that need the durable
/// PID should prefer [`spawn_session_daemon`], which waits for the
/// pidfile.
// Dead in the standalone `session_daemon` binary (which `#[path]`-includes
// this file), used from the main `branchwork-server` binary. Silence the
// dual-binary dead-code warning.
#[allow(dead_code)]
pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<u32> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW = 0x0800_0000, DETACHED_PROCESS = 0x0000_0008
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Path of the pidfile sibling to `socket`. The daemon writes its own PID
/// here after detaching so the spawner can discover the real PID (on Unix
/// the `spawn()` PID belongs to the fork-parent that `_exit`s immediately).
pub fn pidfile_path(socket: &Path) -> PathBuf {
    socket.with_extension("pid")
}

/// Path of the transcript-log sibling to `socket`.
pub fn log_path(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// Spawn `branchwork-server session …` detached, then block (async) until the
/// daemon has written its pidfile and the listener is accepting connections.
/// Returns the durable daemon PID.
///
/// `cmd` is the command (argv[0] + args) to run inside the PTY, e.g.
/// `["claude", "--session-id", …]`.
// See `spawn_detached` — used from the main binary, unused in the
// standalone session_daemon binary.
#[allow(dead_code)]
pub async fn spawn_session_daemon(
    exe: &Path,
    socket: &Path,
    cwd: &Path,
    cols: u16,
    rows: u16,
    cmd: &[String],
) -> io::Result<u32> {
    // Clear any stale pidfile from a previous crashed daemon.
    let pid_path = pidfile_path(socket);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&pid_path);

    let socket_str = socket.to_string_lossy().to_string();
    let cwd_str = cwd.to_string_lossy().to_string();
    let cols_str = cols.to_string();
    let rows_str = rows.to_string();

    let mut args: Vec<&str> = vec![
        "session",
        "--socket",
        &socket_str,
        "--cwd",
        &cwd_str,
        "--cols",
        &cols_str,
        "--rows",
        &rows_str,
        "--",
    ];
    for a in cmd {
        args.push(a.as_str());
    }
    let _ = spawn_detached(exe, &args)?;

    // Wait (up to ~5s) for the daemon to write its pidfile.
    for _ in 0..50 {
        if let Ok(contents) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = contents.trim().parse::<u32>()
        {
            return Ok(pid);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "session daemon did not write pidfile at {}",
            pid_path.display()
        ),
    ))
}

/// Write the current process's PID to `socket`'s sibling `.pid` file.
/// Called by the daemon itself once it's finished detaching so the spawner
/// can discover it.
fn write_pidfile(socket: &Path) -> io::Result<()> {
    let pid_path = pidfile_path(socket);
    std::fs::write(&pid_path, std::process::id().to_string())
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

    // Stale socket files from a previous crash would make bind() fail with
    // EADDRINUSE on Unix. Windows named pipes aren't file-backed so there's
    // nothing to clean up there; `ListenerOptions::try_overwrite(true)` is
    // documented as a no-op on Windows.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket);

    let name = socket_name(&socket)?;
    let listener: LocalListener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_tokio()?;

    // Record our own PID so the spawning server can discover the real daemon
    // (on Unix, `spawn()` returned the fork-parent, which has already exited).
    if let Err(e) = write_pidfile(&socket) {
        eprintln!(
            "[session daemon] failed to write pidfile {}: {e}",
            pidfile_path(&socket).display()
        );
    }

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
    // reconnects after a crash still has the full transcript on disk. The
    // `.log` sibling path is valid on both Unix and Windows.
    let log = log_path(&socket);
    let log_file = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?,
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
                Ok(stream) => {
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

    // Best-effort cleanup: kill the child, drop the pidfile, and on Unix
    // unlink the socket file. The log file is already flushed on every chunk.
    {
        let mut c = child.lock().unwrap();
        let _ = c.kill();
    }
    let _ = std::fs::remove_file(pidfile_path(&socket));
    #[cfg(unix)]
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle_client(
    stream: LocalStream,
    mut out_rx: broadcast::Receiver<Vec<u8>>,
    in_tx: std::sync::mpsc::Sender<Vec<u8>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    shutdown_tx: broadcast::Sender<()>,
) {
    let (read_half, write_half) = stream.split();
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
    use interprocess::local_socket::ConnectOptions;
    use std::time::Duration;

    /// Shell command that works on the current platform. Tests run through
    /// the PTY so we need a real shell; `/bin/sh` on Unix and `cmd.exe` on
    /// Windows are both guaranteed present on their respective CI runners.
    #[cfg(not(windows))]
    fn shell(body: &str) -> Vec<String> {
        #[cfg(unix)]
        {
            vec!["/bin/sh".into(), "-c".into(), body.into()]
        }
        #[cfg(windows)]
        {
            vec!["cmd.exe".into(), "/c".into(), body.into()]
        }
    }

    /// Short delay, via shell built-ins. On Unix this is `sleep 0.4`; on
    /// Windows we pipe to `ping` since `timeout` misbehaves under ConPTY.
    #[cfg(not(windows))]
    fn sleep_fragment_short() -> &'static str {
        #[cfg(unix)]
        {
            "sleep 0.4"
        }
        #[cfg(windows)]
        {
            // `ping -n 2` issues 2 pings spaced ~1s apart (≈1s total delay).
            "ping -n 2 127.0.0.1 > NUL"
        }
    }

    #[cfg(not(windows))]
    fn echo_line(text: &str) -> String {
        #[cfg(unix)]
        {
            format!("printf '{text}\\n'")
        }
        #[cfg(windows)]
        {
            format!("echo {text}")
        }
    }

    #[cfg(not(windows))]
    async fn wait_for_socket(socket: &Path) {
        // On Unix the socket is a real file path, so `exists()` works. On
        // Windows named pipes aren't file-system visible, so we instead
        // probe by attempting a connect; the first success unblocks us.
        #[cfg(unix)]
        {
            for _ in 0..50 {
                if socket.exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        #[cfg(windows)]
        {
            for _ in 0..50 {
                if let Ok(name) = socket_name(socket)
                    && ConnectOptions::new()
                        .name(name)
                        .connect_tokio()
                        .await
                        .is_ok()
                {
                    // Give the listener a beat to settle; probe stream is dropped.
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
    }

    #[cfg(not(windows))]
    async fn connect_client(socket: &Path) -> LocalStream {
        let name = socket_name(socket).expect("socket name");
        ConnectOptions::new()
            .name(name)
            .connect_tokio()
            .await
            .expect("connect local socket")
    }

    fn temp_socket(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    /// Cross-platform IPC end-to-end roundtrip: bind a listener via
    /// [`socket_name`], connect from a client task, exchange a `Ping`/`Pong`
    /// frame, and shut down. This is the load-bearing test that proves the
    /// `interprocess` abstraction (Unix domain socket on Linux/macOS,
    /// `\\.\pipe\<stem>` named pipe on Windows) actually works on every
    /// supported platform — the PTY-driven tests below are gated off on
    /// Windows because of an unrelated ConPTY/`cmd.exe` interaction, so this
    /// is the only e2e coverage we have for the supervisor's IPC layer
    /// there. Keep it always-on; do not add a `cfg` gate.
    #[tokio::test]
    async fn local_socket_listener_round_trips_ping_pong_cross_platform() {
        let (_dir, socket) = temp_socket("oai-rt.sock");

        // Server side: bind a listener and accept exactly one client.
        let server_socket = socket.clone();
        let server_task = tokio::spawn(async move {
            let name = socket_name(&server_socket).expect("server: socket name");
            let listener: LocalListener = ListenerOptions::new()
                .name(name)
                .try_overwrite(true)
                .create_tokio()
                .expect("server: listener");

            let stream = listener.accept().await.expect("server: accept");
            let (mut read_half, write_half) = stream.split();
            let msg = session_protocol::read_frame(&mut read_half)
                .await
                .expect("server: read ping");
            assert!(
                matches!(msg, Message::Ping),
                "server: expected Ping, got {msg:?}"
            );
            let mut write_half = write_half;
            session_protocol::write_frame(&mut write_half, &Message::Pong)
                .await
                .expect("server: write pong");
            // Hold the write half open until the client has had a chance to
            // pull the framed bytes off the wire — dropping immediately can
            // race the named-pipe peer on Windows.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(write_half);
        });

        // Wait for the listener to actually be bound. Probing via connect is
        // the only file-system-agnostic signal: Unix would let us `exists()`
        // the socket file, but Windows named pipes aren't filesystem-visible.
        let mut client = None;
        for _ in 0..100 {
            let name = match socket_name(&socket) {
                Ok(n) => n,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
            };
            match ConnectOptions::new().name(name).connect_tokio().await {
                Ok(s) => {
                    client = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let client = client.expect("client: connect within deadline");

        let (mut read_half, mut write_half) = client.split();
        session_protocol::write_frame(&mut write_half, &Message::Ping)
            .await
            .expect("client: write ping");
        let pong = tokio::time::timeout(
            Duration::from_secs(5),
            session_protocol::read_frame(&mut read_half),
        )
        .await
        .expect("client: read pong did not arrive within 5s")
        .expect("client: read frame ok");
        assert!(
            matches!(pong, Message::Pong),
            "client: expected Pong, got {pong:?}"
        );

        // Server task should observe the Ping and write Pong cleanly.
        tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server task did not finish within 5s")
            .expect("server task panicked");
    }

    // TODO(windows): these three PTY end-to-end tests drive `cmd.exe echo`
    // through ConPTY and, in GitHub Actions `windows-latest`, the child's
    // output never reaches our log reader — only the ConPTY init query
    // (`\x1b[6n`) is observed. cmd.exe also uses `&` (not `;`) as a command
    // separator, so the chained-shell tests below would need restructuring
    // before they could run there anyway. We rely on
    // [`local_socket_listener_round_trips_ping_pong_cross_platform`] above
    // to exercise the IPC layer (named-pipe binding, accept, framed
    // protocol round-trip) on Windows; the Unix gate below is removed once
    // we have a real Windows dev loop to debug ConPTY there.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn daemon_proxies_pty_output_to_client() {
        let (_dir, socket) = temp_socket("oai-proxy.sock");
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            // Delay the print so the client has time to connect and subscribe
            // to the broadcast. Output emitted before anyone's listening is
            // intentionally dropped (the on-disk log is the historical record).
            cmd: shell(&format!(
                "{sleep}; {echo}; {sleep2}",
                sleep = sleep_fragment_short(),
                echo = echo_line("hello-from-pty"),
                sleep2 = sleep_fragment_short(),
            )),
        };

        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;
        let mut client = connect_client(&socket).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
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
        let _ = tokio::time::timeout(Duration::from_secs(10), daemon).await;

        assert!(
            buf.windows(b"hello-from-pty".len())
                .any(|w| w == b"hello-from-pty"),
            "expected PTY output to include 'hello-from-pty', got: {:?}",
            String::from_utf8_lossy(&buf),
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn daemon_writes_to_log_file() {
        let (_dir, socket) = temp_socket("oai-log.sock");
        let log = log_path(&socket);
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            cmd: shell(&echo_line("logged-line")),
        };
        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;

        // Let the PTY run to completion (child exits → reader sees EOF →
        // daemon shuts down).
        let _ = tokio::time::timeout(Duration::from_secs(10), daemon).await;

        let contents = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            contents.contains("logged-line"),
            "expected log to contain 'logged-line', got: {contents:?}",
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn daemon_accepts_reconnect_after_client_drops() {
        let (_dir, socket) = temp_socket("oai-reconnect.sock");
        let args = SessionArgs {
            socket: socket.clone(),
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            // Long-running: two prints spaced out, so the second one arrives
            // only after the first client has dropped.
            cmd: shell(&format!(
                "{echo1}; {sleep}; {echo2}; {sleep2}",
                echo1 = echo_line("first"),
                echo2 = echo_line("second"),
                sleep = sleep_fragment_short(),
                sleep2 = sleep_fragment_short(),
            )),
        };
        let daemon = tokio::spawn(async move { run_daemon(args).await });

        wait_for_socket(&socket).await;
        {
            let mut c1 = connect_client(&socket).await;
            // Drain a couple of frames so the broadcast receiver is exercised,
            // then drop.
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                session_protocol::read_frame(&mut c1),
            )
            .await;
        }

        // Second connection must still work.
        let mut c2 = connect_client(&socket).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
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
        let _ = tokio::time::timeout(Duration::from_secs(10), daemon).await;

        assert!(
            buf.windows(6).any(|w| w == b"second"),
            "expected reconnect to see live output; got: {:?}",
            String::from_utf8_lossy(&buf),
        );
    }

    #[test]
    fn socket_name_roundtrip() {
        // Stems with hyphens and digits should parse on both platforms.
        let p = PathBuf::from(if cfg!(windows) {
            r"C:\Users\oai\oai-abc123.sock"
        } else {
            "/tmp/oai/oai-abc123.sock"
        });
        let name = socket_name(&p).expect("socket name");
        assert!(name.is_path());
    }
}
