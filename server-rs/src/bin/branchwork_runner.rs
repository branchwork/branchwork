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
#[path = "../project_scaffold.rs"]
mod project_scaffold;
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

// Same trick for `crate::ci::aggregate`: the runner pulls in the
// aggregation rule via `#[path]` so the Reglyze regression test (in
// ci/aggregate.rs) is the single place that exercises it for both
// modes. Server crate has the real `mod ci { pub mod aggregate; }`
// hierarchy in ci.rs. Use a flat `#[path]` mod plus a re-export so
// `crate::ci::aggregate::*` resolves identically in both crates;
// nested `#[path]` inside an inline module resolves the path
// relative to the synthetic `<parent>/<mod_name>/` directory and
// breaks for files outside that subtree.
//
// `ci::aggregate::is_workflow_blocking_by_default` carries the level-4
// smart classifier the runner falls back on when the server didn't ship
// an explicit allowlist on the wire. The classifier lives next to
// `compute_with_filter` rather than in `ci::resolution` so the runner
// can depend on it without pulling the plan-parser / repo-config
// machinery (those are server-only crates).
#[path = "../ci/aggregate.rs"]
pub mod ci_aggregate;
mod ci {
    pub use super::ci_aggregate as aggregate;
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use clap::Parser;
use rusqlite::Connection;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};

use runner_protocol::{
    CiAggregate, CiRunSummary, DriverAuthInfo, DriverAuthStatus, Envelope, FolderEntry, GhRun,
    MergeOutcome, WireMessage,
};

// ── Runner log shipper ──────────────────────────────────────────────────────
//
// One tokio task drains a bounded channel of `LogLine`s, throttles emission
// at ~200 lines/sec, and ships each line over the WS as
// `WireMessage::RunnerLogLine`. The macros `log_info!`/`log_warn!` push to
// the channel and ALSO print to the original stdout/stderr so `journalctl`
// and tty users still see everything.
mod runner_log {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use chrono::{SecondsFormat, Utc};
    use tokio::sync::mpsc;

    use crate::runner_protocol::{Envelope, WireMessage};

    /// Per-runner emission cap. The dashboard documents this; bursts beyond
    /// it surface as a `[truncated N lines]` synthetic line.
    pub const MAX_LINES_PER_SECOND: usize = 200;
    /// Bounded channel capacity. Producer `try_send` returns `Full` when this
    /// is hit; we count the drop and the next surviving emit prepends a
    /// `[truncated N lines]` marker.
    pub const MAX_PENDING_LINES: usize = 256;

    #[derive(Debug, Clone)]
    pub struct LogLine {
        pub ts: String,
        pub level: &'static str,
        pub line: String,
    }

    /// Routing slot for the active log shipper. Set by `install` at the top
    /// of `connect_and_run`; cleared by `uninstall` on disconnect so emits
    /// in the reconnect gap are dropped silently rather than counting
    /// against the truncated tally.
    static SENDER: Mutex<Option<mpsc::Sender<LogLine>>> = Mutex::new(None);

    /// Producer-side drop count. The shipper reads-and-resets this before
    /// each emit; if non-zero it prepends a `[truncated N lines]` line.
    static DROPPED: AtomicU64 = AtomicU64::new(0);

    /// Push a line. Non-blocking; called from `log_info!`/`log_warn!`.
    pub fn try_send(line: LogLine) {
        // Lock is held for a single clone (Sender is cheap to clone). If the
        // slot is being swapped right now, just drop the line — the macros
        // never block on this path.
        let tx = match SENDER.lock() {
            Ok(g) => g.as_ref().cloned(),
            Err(_) => None,
        };
        if let Some(tx) = tx
            && tx.try_send(line).is_err()
        {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Spawn the consumer task. Returns the `Sender` end so the caller can
    /// install it via `install` for the duration of a connection.
    pub fn spawn_shipper(
        runner_id: String,
        ws_tx: mpsc::UnboundedSender<String>,
    ) -> (mpsc::Sender<LogLine>, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<LogLine>(MAX_PENDING_LINES);
        let handle = tokio::spawn(async move {
            // Token bucket: one slot every 1000/200 = 5ms.
            let interval_ms = (1000_u64 / MAX_LINES_PER_SECOND as u64).max(1);
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately — skip it so the first line has
            // to wait for the second tick (matches the documented cadence).
            interval.tick().await;
            while let Some(line) = rx.recv().await {
                interval.tick().await;
                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    let marker = WireMessage::RunnerLogLine {
                        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                        level: "warn".into(),
                        line: format!("[truncated {dropped} lines]"),
                    };
                    let env = Envelope::best_effort(runner_id.clone(), marker);
                    let _ = ws_tx.send(serde_json::to_string(&env).unwrap_or_default());
                    // The marker pays a token of its own so a burst of
                    // drops can't double the visible emission rate.
                    interval.tick().await;
                }
                let env = Envelope::best_effort(
                    runner_id.clone(),
                    WireMessage::RunnerLogLine {
                        ts: line.ts,
                        level: line.level.into(),
                        line: line.line,
                    },
                );
                let _ = ws_tx.send(serde_json::to_string(&env).unwrap_or_default());
            }
        });
        (tx, handle)
    }

    /// Install the active shipper sender. Subsequent `try_send` calls route
    /// through it.
    pub fn install(tx: mpsc::Sender<LogLine>) {
        if let Ok(mut g) = SENDER.lock() {
            *g = Some(tx);
        }
    }

    /// Drop the active sender. `try_send` becomes a silent no-op until the
    /// next `install`. Pending-drop counter is also reset so the truncated
    /// marker on the next connection reflects only that connection's drops.
    pub fn uninstall() {
        if let Ok(mut g) = SENDER.lock() {
            *g = None;
        }
        DROPPED.store(0, Ordering::Relaxed);
    }

    /// Build a timestamped log line. Used by the macros so the timestamp
    /// is captured at the call site (not at consume time).
    pub fn now() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

/// Per-runner runtime health metrics. Lives next to `runner_log` because
/// the two share the same emission style (best-effort, no outbox, latest-
/// snapshot-wins) and the same lifecycle (per-connection install /
/// uninstall is unnecessary — these counters survive reconnects so the
/// 24 h reconnect window stays accurate across flaps).
mod runner_health {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// 24 h window for the reconnect counter. The dashboard chip turns red
    /// past 5 events in this window.
    pub const RECONNECT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

    /// Cap on the in-memory CI-poll latency ring. Just enough to compute
    /// stable percentiles over a few hours of polling without unbounded
    /// growth. Each entry is a u32, so 256 entries is ~1 KB — trivially
    /// affordable on the runner host.
    pub const CI_LATENCY_RING_CAP: usize = 256;

    /// Reconnect timestamps inside the trailing 24 h window. A push always
    /// trims first so the reported count is always fresh; readers don't
    /// need to filter.
    static RECONNECTS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

    /// Trailing CI-poll wall-clock samples (milliseconds). FIFO ring; new
    /// samples push back, oldest evict from the front when the cap is hit.
    static CI_LATENCIES_MS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    /// Reap timestamps for orphaned session sockets / zombie children
    /// inside the trailing 24 h window. Bumped from two paths:
    /// (a) `cleanup_and_reattach_runner` when a stale socket has no
    /// listener, and (b) the `waitpid` reaper when a detached child
    /// exits between heartbeats. Pushes trim outside the window so the
    /// counter is always fresh on read.
    static ORPHANS_REAPED: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

    /// Record one reconnect. Call this once per `connect_and_run` invocation
    /// after the WS handshake succeeds — the count is "successful reconnects
    /// in the last 24 h," not "connection attempts." A flapping runner that
    /// never establishes a WS still shows zero (the dashboard surfaces this
    /// as `status=offline` instead).
    pub fn record_reconnect() {
        let now = Instant::now();
        if let Ok(mut g) = RECONNECTS.lock() {
            g.push(now);
            // Trim outside the window.
            g.retain(|t| now.duration_since(*t) < RECONNECT_WINDOW);
        }
    }

    /// Read the current reconnect count in the trailing 24 h window. Trims
    /// stale entries so even a tick that happens hours after the last
    /// reconnect reports a fresh number.
    pub fn ws_reconnects_24h() -> u32 {
        let now = Instant::now();
        if let Ok(mut g) = RECONNECTS.lock() {
            g.retain(|t| now.duration_since(*t) < RECONNECT_WINDOW);
            g.len() as u32
        } else {
            0
        }
    }

    /// Bump the orphan-reap counter. Call once per stale socket
    /// (`cleanup_and_reattach_runner`) and once per reaped zombie
    /// (waitpid reaper) so `RunnerHealth.orphans_reaped_24h` reflects
    /// the named-wart "the supervisor died but the runner kept running"
    /// failure mode (Task 11.6).
    pub fn record_orphan_reaped() {
        let now = Instant::now();
        if let Ok(mut g) = ORPHANS_REAPED.lock() {
            g.push(now);
            g.retain(|t| now.duration_since(*t) < RECONNECT_WINDOW);
        }
    }

    /// Read the current orphan-reaped count in the trailing 24 h window.
    /// Same trim-then-count idiom as `ws_reconnects_24h`.
    pub fn orphans_reaped_24h() -> u32 {
        let now = Instant::now();
        if let Ok(mut g) = ORPHANS_REAPED.lock() {
            g.retain(|t| now.duration_since(*t) < RECONNECT_WINDOW);
            g.len() as u32
        } else {
            0
        }
    }

    /// Record one CI poll wall-clock sample. Cap-bounded ring; pushes evict
    /// the oldest entry when the cap is hit.
    pub fn record_ci_poll_ms(ms: u32) {
        if let Ok(mut g) = CI_LATENCIES_MS.lock() {
            g.push(ms);
            if g.len() > CI_LATENCY_RING_CAP {
                let drop = g.len() - CI_LATENCY_RING_CAP;
                g.drain(..drop);
            }
        }
    }

    /// Compute (p50, p99) over the current ring. Returns `(None, None)`
    /// when no samples have been collected yet — the wire variant skips
    /// these fields so the dashboard renders "—" rather than a bogus 0.
    pub fn ci_latency_percentiles() -> (Option<u32>, Option<u32>) {
        let snapshot = match CI_LATENCIES_MS.lock() {
            Ok(g) => g.clone(),
            Err(_) => return (None, None),
        };
        percentiles_from(&snapshot)
    }

    /// Pure split for testability. Sorts `samples` ascending and reads
    /// `(p50, p99)` from the resulting indices. Returns `(None, None)` for
    /// empty input.
    pub fn percentiles_from(samples: &[u32]) -> (Option<u32>, Option<u32>) {
        if samples.is_empty() {
            return (None, None);
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let p50 = sorted[sorted.len() / 2];
        // Nearest-rank p99: `ceil(0.99 * n) - 1`, clamped to last index.
        let n = sorted.len();
        let p99_idx = ((n as f64 * 0.99).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        let p99 = sorted[p99_idx];
        (Some(p50), Some(p99))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn percentiles_empty() {
            assert_eq!(percentiles_from(&[]), (None, None));
        }

        #[test]
        fn percentiles_single() {
            assert_eq!(percentiles_from(&[42]), (Some(42), Some(42)));
        }

        #[test]
        fn percentiles_unsorted_input() {
            // Inputs out of order — function must sort before reading.
            let samples: Vec<u32> = vec![10, 5, 100, 20, 50];
            let (p50, p99) = percentiles_from(&samples);
            assert_eq!(p50, Some(20));
            // p99 of 5 elements is the largest one (nearest-rank).
            assert_eq!(p99, Some(100));
        }

        #[test]
        fn percentiles_one_hundred_samples() {
            // 100 samples 1..=100: p50 → 51 (mid index), p99 → 99 (index 98).
            let samples: Vec<u32> = (1..=100).collect();
            let (p50, p99) = percentiles_from(&samples);
            assert_eq!(p50, Some(51));
            assert_eq!(p99, Some(99));
        }

        #[test]
        fn record_ci_poll_evicts_oldest_when_capped() {
            // Push past the cap; verify the latest sample is in the
            // distribution and the oldest got dropped.
            for i in 0..(CI_LATENCY_RING_CAP as u32 + 10) {
                record_ci_poll_ms(i);
            }
            let snapshot = CI_LATENCIES_MS.lock().unwrap().clone();
            assert_eq!(snapshot.len(), CI_LATENCY_RING_CAP);
            // First entries 0..10 dropped. The first surviving entry is 10.
            assert_eq!(snapshot.first().copied(), Some(10));
        }
    }
}

/// Print to stdout AND ship as `RunnerLogLine` (level=info). Mirrors the
/// existing `[runner] ...` prefix convention; callers keep their format
/// strings unchanged.
macro_rules! log_info {
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        let __ts = $crate::runner_log::now();
        println!("{}", __line);
        $crate::runner_log::try_send($crate::runner_log::LogLine {
            ts: __ts,
            level: "info",
            line: __line,
        });
    }};
}

/// Print to stderr AND ship as `RunnerLogLine` (level=warn). Used for
/// non-fatal warnings and recoverable errors.
macro_rules! log_warn {
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        let __ts = $crate::runner_log::now();
        eprintln!("{}", __line);
        $crate::runner_log::try_send($crate::runner_log::LogLine {
            ts: __ts,
            level: "warn",
            line: __line,
        });
    }};
}

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
    /// `plan_name → cwd` populated from each `StartAgent`. The CI handlers
    /// (`GetCiRunStatus`, `CiFailureLog`) only carry `plan_name` on the wire,
    /// so the runner needs a sticky map to recover the directory it should
    /// run `gh` in. Survives agent exit so the auto-mode loop can keep
    /// polling after the task agent has stopped.
    plan_cwd: Arc<Mutex<HashMap<String, PathBuf>>>,
    /// Per-SHA aggregate cache for `GetCiRunStatus` (~10 s TTL). The
    /// `latest_sha_by_plan` side-table lets `CiFailureLog { run_id: None }`
    /// re-resolve the failing run for the most recent SHA the runner saw
    /// for a plan — that's the auto-mode 3.1 use-case where the loop has
    /// dropped the run id by the time it decides to fetch a log.
    ci_cache: Arc<Mutex<CiAggregateCache>>,
    /// Set to `true` when the runner has accepted a `WireMessage::ShutdownRequest`
    /// from the SaaS server. Causes the reconnect loop to break out and the
    /// process to exit cleanly after the in-flight session daemons have been
    /// asked to drain. Shared via `Arc` so the main loop sees the same flag
    /// the message handler flips.
    shutdown_requested: Arc<AtomicBool>,
}

struct AgentHandle {
    /// PID of the session daemon.
    pid: u32,
    /// Socket path for the session daemon.
    socket_path: PathBuf,
    /// Abort handle for the I/O forwarding task.
    io_task: tokio::task::JoinHandle<()>,
    /// Where the agent was spawned. Stored at `StartAgent` time so the
    /// `MergeAgentBranch` and `HasGithubActions` handlers can reach the
    /// same directory the agent committed to (the runner's canonical
    /// `--cwd` may be a parent if multiple projects share one runner).
    cwd: PathBuf,
    /// `--mcp-config` temp file written at spawn (T5.2). `None` when the
    /// driver opted out (`mcp_config` empty on the wire). Removed on
    /// graceful exit (inside the io_task) or explicit kill / shutdown
    /// drain (here, since `io_task.abort()` skips the closure's cleanup).
    mcp_config_path: Option<PathBuf>,
    /// `--settings` temp file written at spawn (T5.2). Same lifecycle as
    /// `mcp_config_path`.
    settings_path: Option<PathBuf>,
}

/// Best-effort removal of the per-agent temp files written at spawn.
/// Used by the kill / drain paths where `io_task.abort()` skips the
/// closure-level cleanup. Idempotent: a missing file is not an error.
fn cleanup_agent_temp_files(handle: &AgentHandle) {
    for p in [
        handle.mcp_config_path.as_deref(),
        handle.settings_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Err(e) = std::fs::remove_file(p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log_warn!("[runner] failed to remove {}: {e}", p.display());
        }
    }
}

/// Aggregate-cache TTL. ~10 s per the auto-mode brief — short enough that
/// a stale entry doesn't paper over a CI state change, long enough that a
/// tight server-side poll loop on a 1–2 s cadence collapses into one `gh`
/// call per cycle.
const CI_CACHE_TTL: Duration = Duration::from_secs(10);

/// Soft cap on cache entries. Eviction is opportunistic on insert: when
/// the map exceeds this size we drop entries whose age exceeds 5×TTL.
const CI_CACHE_MAX_ENTRIES: usize = 64;

#[derive(Default)]
struct CiAggregateCache {
    /// SHA → most recently computed aggregate (and the time it was
    /// computed). `aggregate: None` is itself a cacheable result — it
    /// means "`gh run list` returned no rows for this SHA yet" and the
    /// loop should keep polling.
    by_sha: HashMap<String, CachedAggregate>,
    /// `plan_name → most-recent SHA the runner has been asked about`.
    /// Populated as a side effect of every `GetCiRunStatus` so the
    /// `run_id: None` branch of `CiFailureLog` can re-resolve.
    latest_sha_by_plan: HashMap<String, String>,
}

#[derive(Clone)]
struct CachedAggregate {
    computed_at: Instant,
    aggregate: Option<CiAggregate>,
}

impl CiAggregateCache {
    fn fresh(&self, sha: &str) -> Option<Option<CiAggregate>> {
        let entry = self.by_sha.get(sha)?;
        if entry.computed_at.elapsed() < CI_CACHE_TTL {
            Some(entry.aggregate.clone())
        } else {
            None
        }
    }

    fn put(&mut self, sha: String, aggregate: Option<CiAggregate>) {
        self.by_sha.insert(
            sha,
            CachedAggregate {
                computed_at: Instant::now(),
                aggregate,
            },
        );
        if self.by_sha.len() > CI_CACHE_MAX_ENTRIES {
            self.by_sha
                .retain(|_, e| e.computed_at.elapsed() < CI_CACHE_TTL * 5);
        }
    }

    fn record_plan_sha(&mut self, plan: String, sha: String) {
        self.latest_sha_by_plan.insert(plan, sha);
    }

    fn latest_sha_for_plan(&self, plan: &str) -> Option<String> {
        self.latest_sha_by_plan.get(plan).cloned()
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = rt.block_on(run(cli)) {
        // Shipper isn't installed at this point in the lifecycle so this is
        // a plain stderr; the caller will only see it if they're tailing
        // the runner host directly.
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

    // T1.2 self-diagnostic. `resolve_server_bin` returns both the path the
    // runner will hand to `Command::spawn` AND a structured diagnostic shipped
    // on `RunnerHello`. The dashboard surfaces the diagnostic next to the
    // version chip so a missing `branchwork-server` shows up as a red cross
    // BEFORE the first Start session click — no more silent gap.
    let server_bin_resolution = resolve_server_bin(cli.server_bin.as_deref());
    let server_bin = server_bin_resolution.spawn_target.clone();
    let server_bin_diagnostic = server_bin_resolution.diagnostic.clone();
    match &server_bin_diagnostic {
        runner_protocol::ServerBinDiagnostic::Found { path } => {
            println!("[runner] server-bin resolved: {path}");
        }
        runner_protocol::ServerBinDiagnostic::NotFound { searched, reason } => {
            eprintln!(
                "[runner] WARNING server-bin unresolved: searched={searched} reason={reason}"
            );
        }
    }

    // Init local DB.
    let conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    outbox::init_runner_outbox(&conn);
    outbox::init_seq_tracker(&conn);

    // Load or generate runner_id.
    let runner_id = cli
        .runner_id
        .unwrap_or_else(|| load_or_generate_runner_id(&conn));

    // Shipper is per-connection (installed below in `connect_and_run`); this
    // pre-connect line is plain stdout — there's no WS yet to ship to.
    println!(
        "[runner] id={runner_id} cwd={} db={}",
        cwd.display(),
        db_path.display()
    );

    let db = Arc::new(Mutex::new(conn));

    // Build the WebSocket URL.
    let ws_url = build_ws_url(&cli.saas_url, &cli.token);

    // Process-wide shutdown flag. Flipped by `WireMessage::ShutdownRequest`
    // (operator clicked the dashboard button) to break out of the reconnect
    // loop after the current connection ends. Cloned per connection so each
    // `RunnerState` sees the same atomic.
    let shutdown = Arc::new(AtomicBool::new(false));

    // Reconnect loop with exponential backoff.
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        // Pre-connect: no shipper installed yet, so these are stdout-only.
        // Once `connect_and_run` has the WS up it installs the shipper and
        // every subsequent `log_info!`/`log_warn!` reaches the dashboard.
        println!("[runner] connecting to {}", cli.saas_url);

        match connect_and_run(
            &ws_url,
            &runner_id,
            &cwd,
            &server_bin,
            server_bin_diagnostic.clone(),
            db.clone(),
            shutdown.clone(),
        )
        .await
        {
            Ok(()) => {
                println!("[runner] connection closed cleanly");
            }
            Err(e) => {
                eprintln!("[runner] connection error: {e}");
            }
        }

        // Operator-requested shutdown takes precedence over reconnect. By
        // the time we get here, `handle_server_message` has already asked
        // every session daemon to exit; we just close out cleanly so the
        // host's process supervisor doesn't restart us. Token revocation
        // (Revoke button) is handled by the SaaS side: a now-revoked token
        // would fail the WS upgrade with 401 on the next connect attempt
        // anyway, but acknowledging the shutdown intent here gives a
        // cleaner exit message in journalctl.
        if shutdown.load(AtomicOrdering::Relaxed) {
            println!("[runner] shutdown requested by SaaS — exiting");
            return Ok(());
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

        // Reconnect log is also pre-shipper.
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
    server_bin_diagnostic: runner_protocol::ServerBinDiagnostic,
    db: Arc<Mutex<Connection>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Connect via tokio-tungstenite.
    let (ws_stream, _response) = tokio_tungstenite::connect_async(ws_url).await?;
    let (ws_write, ws_read) = futures_util::StreamExt::split(ws_stream);

    // Channel for outbound WebSocket messages.
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<String>();

    // Install the runner-log shipper now that we have a live ws_tx. Every
    // `log_info!`/`log_warn!` from this point on routes through the
    // shipper to the dashboard `/runners/{id}` tail panel; we still print
    // to local stdout/stderr so journalctl + tty users see the same line.
    let (log_tx, log_shipper_handle) =
        runner_log::spawn_shipper(runner_id.to_string(), ws_tx.clone());
    runner_log::install(log_tx);

    log_info!("[runner] connected");

    // Record this successful reconnect for the 24 h reconnect counter that
    // ships with `RunnerHealth`. Done once per `connect_and_run` so a
    // flapping link surfaces a rising count and triggers the dashboard's
    // red chip past the threshold (5 in 24 h).
    runner_health::record_reconnect();

    let state = Arc::new(RunnerState {
        runner_id: runner_id.to_string(),
        db: db.clone(),
        agents: Arc::new(Mutex::new(HashMap::new())),
        ws_tx: ws_tx.clone(),
        cwd: cwd.to_path_buf(),
        server_bin: server_bin.to_path_buf(),
        plan_cwd: Arc::new(Mutex::new(HashMap::new())),
        ci_cache: Arc::new(Mutex::new(CiAggregateCache::default())),
        shutdown_requested: shutdown.clone(),
    });

    // ── Reattach to surviving session daemons (Task 11.6) ────────────────
    // Re-populate `state.agents` from `<cwd>/.branchwork-runner-sessions/`.
    // Idempotent: agents already in memory are skipped, so calling this
    // on every reconnect catches new survivors without double-spawning.
    // Sockets whose owning daemon is dead get reaped (and counted into
    // `orphans_reaped_24h`).
    cleanup_and_reattach_runner(&state).await;

    // ── Writer task: flush channel messages to WebSocket ─────────────────
    let writer = tokio::spawn(ws_writer(ws_write, ws_rx));

    // ── Send runner_hello ────────────────────────────────────────────────
    // `active_agents` is the snapshot of supervisors the runner is still
    // tracking in memory — `cleanup_and_reattach_runner` already ran
    // before this point and populated `state.agents` from any on-disk
    // session sockets that survived the runner-process restart. The
    // server uses this list to undo `cleanup_and_reattach`'s blanket
    // `killed/supervisor_died` marking of remote agents on its own boot
    // (T5.14). Snapshotting under the lock is fine — Hello fires once
    // per WS connect, the lock is uncontended.
    let drivers = collect_driver_auth();
    let active_agents: Vec<String> = state.agents.lock().await.keys().cloned().collect();
    log_info!(
        "[runner] hello: {} drivers, {} active agents reattached",
        drivers.len(),
        active_agents.len()
    );
    let hello = WireMessage::RunnerHello {
        hostname: hostname(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        drivers: drivers.clone(),
        active_agents,
        // T1.2: hand the dashboard the runner's startup self-diagnostic for
        // the spawn target. Same value across reconnects (resolved once at
        // runner boot), so the dashboard chip reflects the runner's current
        // capability to spawn session daemons regardless of WS flap.
        server_bin: Some(server_bin_diagnostic.clone()),
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

    // ── Zombie reaper (Task 11.6) ───────────────────────────────────────
    // Detached session daemons (CommandExt::pre_exec calls setsid + we
    // never `wait()` them) become zombies when they exit between
    // heartbeats. The reaper polls every 5 s and `waitpid(-1, WNOHANG)`s
    // any pending children, emitting one log line per reap so a pile-up
    // (e.g. an OOM-killer storm taking out half a dozen daemons in a
    // burst) surfaces in the dashboard tail as well as the host's
    // `journalctl`. Cancellation: aborted with the rest of the
    // per-connection tasks at the end of `connect_and_run` — process-
    // global lifetime would also be fine but the per-connection scope
    // keeps the join-handle wiring uniform.
    let reaper = spawn_zombie_reaper();

    // ── Health ticker ───────────────────────────────────────────────────
    // Periodic best-effort RunnerHealth snapshot. Fires every ~30 s; the
    // dashboard's `Health` panel reads the latest snapshot, so missed
    // ticks during a flap are absorbed by the next surviving tick.
    let health_tx = ws_tx.clone();
    let health_rid = runner_id.to_string();
    let health_db = db.clone();
    let health_ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // Fire the first sample 30 s after connect rather than at t=0 so
        // the dashboard sees a single fresh snapshot per reconnect rather
        // than a burst of half-empty ones.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate-fire first tick
        loop {
            interval.tick().await;
            let outbox_depth = {
                let conn = health_db.lock().await;
                outbox::runner_outbox_depth(&conn)
            };
            let (p50, p99) = runner_health::ci_latency_percentiles();
            let snapshot = WireMessage::RunnerHealth {
                outbox_depth: outbox_depth.min(u32::MAX as u64) as u32,
                ws_reconnects_24h: runner_health::ws_reconnects_24h(),
                ci_poll_ms_p50: p50,
                ci_poll_ms_p99: p99,
                orphans_reaped_24h: runner_health::orphans_reaped_24h(),
            };
            let env = Envelope::best_effort(health_rid.clone(), snapshot);
            if health_tx
                .send(serde_json::to_string(&env).unwrap_or_default())
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
    health_ticker.abort();
    reaper.abort();
    writer.abort();
    // Drop the shipper sender slot so emits in the reconnect gap don't
    // count against the truncated tally; the consumer task ends naturally
    // when its receiver closes (the `log_tx` clone we held above is now
    // the only outstanding sender after `uninstall`).
    runner_log::uninstall();
    log_shipper_handle.abort();

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
            log_warn!(
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

        // After processing each frame, check if a ShutdownRequest just
        // flipped the shutdown flag — if so, drop the WS by returning. The
        // outer reconnect loop checks the same flag and exits the process
        // cleanly instead of sleeping + retrying.
        if state.shutdown_requested.load(AtomicOrdering::Relaxed) {
            log_info!("[runner] closing WS after honored shutdown");
            break;
        }
    }

    Ok(())
}

/// Handle a single message from the SaaS server.
async fn handle_server_message(state: &Arc<RunnerState>, envelope: &Envelope) {
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
            skip_permissions,
            session_id,
            mcp_config,
            settings_json,
        } => {
            log_info!(
                "[runner] start_agent: {} plan={} task={} driver={}",
                agent_id, plan_name, task_id, driver
            );

            let agent_cwd = if cwd.is_empty() {
                state.cwd.clone()
            } else {
                PathBuf::from(cwd)
            };

            // T5.16: pre-spawn task-branch checkout. Standalone's
            // `pty_agent::start_pty_agent` runs `git checkout -b
            // branchwork/<plan>/<task>` before exec'ing claude — that's
            // why standalone agents always commit on the right branch.
            // SaaS skipped this (per ADR 0007's spawn-divergence audit)
            // and trusted the prompt to nudge claude into doing the
            // checkout itself; in practice claude does it intermittently
            // — sometimes commits land on `master` and the merge button
            // hits "branch X — not something we can merge". User saw
            // this twice on `aso2-browser-homage/2.4` and `2.7`.
            //
            // Mirror standalone here: try `checkout` first (for an
            // existing branch — preserves prior commits if the agent
            // was respawned), fall back to `checkout -b` (fresh
            // branch). Failure is logged + tolerated; the agent
            // proceeds on whatever HEAD is, same as pre-T5.16
            // behaviour, so cwd-not-a-git-repo cases don't fail the
            // spawn outright.
            if !plan_name.is_empty() && !task_id.is_empty() {
                let branch = format!("branchwork/{plan_name}/{task_id}");
                checkout_task_branch(&agent_cwd, &branch);
            }

            // Older servers (pre-T5.2) do not populate session_id on the
            // wire — fall back to a runner-generated UUID so Claude still
            // gets `--session-id <something>` and the agents row carries
            // a non-NULL session_id (degraded vs the new path: the
            // server's row id won't match Claude's, but the spawn works).
            let session_id = if session_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                session_id.clone()
            };

            // Spawn the session daemon.
            match spawn_agent(
                state,
                agent_id,
                &session_id,
                &agent_cwd,
                driver,
                prompt,
                effort.as_deref(),
                *max_budget_usd,
                skip_permissions.unwrap_or(false),
                mcp_config,
                settings_json,
            )
            .await
            {
                Ok(()) => {
                    // Remember plan→cwd so CI handlers (which only carry
                    // `plan_name` on the wire) can later run `gh` in the
                    // right directory. Survives agent exit.
                    state
                        .plan_cwd
                        .lock()
                        .await
                        .insert(plan_name.clone(), agent_cwd.clone());

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
                    log_warn!("[runner] failed to spawn agent {agent_id}: {e}");
                    // Task 1.1: surface a structured `AgentSpawnFailed`
                    // envelope BEFORE the generic `AgentStopped` so the
                    // server can store the errno + command path on the
                    // agent row and the dashboard renders an actionable
                    // message ("runner could not spawn: /usr/local/bin/
                    // branchwork-server (ENOENT)") instead of going silent.
                    //
                    // Both events are reliable + outbox-backed, so the
                    // server sees `agent_spawn_failed` followed by
                    // `agent_stopped` in stable seq order. The
                    // `AgentStopped` is kept for back-compat with older
                    // servers (and for the same row-status flip every
                    // other terminal path produces).
                    if let SpawnAgentError::Spawn { io, command } = &e {
                        let (errno, errno_str) = classify_spawn_io_error(io);
                        let failed = WireMessage::AgentSpawnFailed {
                            agent_id: agent_id.clone(),
                            command: command.clone(),
                            errno,
                            errno_str,
                        };
                        send_reliable(state, failed).await;
                    }
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
            log_info!("[runner] kill_agent: {agent_id}");
            let mut agents = state.agents.lock().await;
            if let Some(handle) = agents.remove(agent_id) {
                handle.io_task.abort();
                // io_task.abort() skips the closure-level cleanup, so
                // remove the per-agent temp files here instead.
                cleanup_agent_temp_files(&handle);
                // SIGTERM the daemon's WHOLE process group (-pid) so the
                // daemon AND its claude child both go. The supervisor
                // calls `setsid()` after fork (`agents/supervisor.rs`)
                // making it a session/group leader; the claude PTY child
                // it spawns via `pty.spawn_command` joins that group.
                // `libc::kill(-pid, ...)` is the canonical Unix call to
                // signal the entire group.
                //
                // Pre-T5.13 we passed `+pid` here, which only signalled
                // the daemon. The daemon's exit didn't propagate SIGHUP
                // to its child fast enough (or at all on some kernels)
                // and claude kept running orphaned off init — every
                // Finish/Kill click leaked a claude session.
                #[cfg(unix)]
                unsafe {
                    if handle.pid == 0 {
                        // Reattach didn't recover a real pid (no pidfile
                        // and no fork-parent fallback). `libc::kill(0,
                        // ...)` and `libc::kill(-0, ...)` both signal
                        // the RUNNER's own process group — would kill
                        // the runner. The agent is already orphaned
                        // off init; log + skip. The session daemon
                        // will exit on its own when the PTY child
                        // does, or on the next host reboot.
                        log_warn!(
                            "[runner] kill_agent {agent_id}: no daemon pid (reattach without pidfile); skipping signal"
                        );
                    } else {
                        let pgid = -(handle.pid as i32);
                        libc::kill(pgid, libc::SIGTERM);
                    }
                }
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &handle.pid.to_string(), "/T"])
                        .status();
                }
            }
        }

        WireMessage::ShutdownRequest { reason } => {
            // Operator clicked "Request shutdown" on the dashboard. The
            // runner is free to ignore this — we don't (today). Drain
            // every in-flight session daemon via SIGTERM (Unix) /
            // taskkill (Windows), then flip the process-wide shutdown
            // flag so the reconnect loop exits cleanly after this WS
            // closes. The reconnect-loop check is what actually exits
            // the process; flipping the flag here is what makes that
            // check short-circuit instead of looping.
            //
            // Note we do NOT process::exit() inline — that would race
            // with in-flight ACK frames the runner still owes the SaaS
            // server. Letting the WS close naturally lets the SaaS see
            // a clean disconnect, broadcast `runner_disconnected`, and
            // flip the dashboard status pill to offline.
            let reason_str = reason.as_deref().unwrap_or("(no reason given)");
            log_info!("[runner] shutdown requested by SaaS: {reason_str}");

            // Drain in-flight agents. Mirrors the KillAgent handler but
            // applied across the whole map. The session daemons get
            // SIGTERM, which lets them flush their PTY and write a
            // graceful-exit footer in <socket>.log — the dashboard's
            // /terminal replay surface stays consistent.
            let mut agents = state.agents.lock().await;
            let drained: Vec<AgentHandle> = agents.drain().map(|(_, h)| h).collect();
            drop(agents);
            for handle in drained {
                handle.io_task.abort();
                // io_task.abort() skips the closure-level cleanup, so
                // remove the per-agent temp files here instead.
                cleanup_agent_temp_files(&handle);
                // Skip kill on pid=0 — that's a sentinel value used by
                // unit tests (seed_test_agent), and on Unix `kill(0,
                // SIGTERM)` sends to the calling process group which
                // would terminate the runner itself.
                if handle.pid == 0 {
                    continue;
                }
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

            // Flip the flag — `ws_reader` checks it at top of every loop
            // iteration and breaks, and the outer reconnect loop checks
            // it before sleeping.
            state.shutdown_requested.store(true, AtomicOrdering::Relaxed);
        }

        WireMessage::StartCheckAgent {
            agent_id,
            session_id,
            prompt,
            cwd,
            effort,
        } => {
            // Mirror `agents/check_agent.rs::start_check_agent` but on the
            // runner host: spawn `claude -p --output-format stream-json`,
            // pipe the prompt in, stream stdout back to the SaaS server one
            // wire frame per JSON line. Same allowed-tools surface (read +
            // git read), same `--permission-mode plan`, same `--effort`
            // forwarding. Server-side bookkeeping (agents row, agent_output
            // table, broadcasts) lives entirely in `runner_ws.rs`'s
            // `CheckAgentOutput` / `AgentStopped` handlers.
            let agent_id = agent_id.clone();
            let session_id = session_id.clone();
            let prompt = prompt.clone();
            let cwd = cwd.clone();
            let effort = effort.clone();
            let cwd_path = match validated_cwd(state, &cwd) {
                Ok(p) => p,
                Err(e) => {
                    log_warn!(
                        "[runner] start_check_agent {agent_id}: rejected cwd: {e}"
                    );
                    send_best_effort(
                        state,
                        WireMessage::AgentStopped {
                            agent_id: agent_id.clone(),
                            status: "failed".into(),
                            cost_usd: None,
                            stop_reason: Some(format!("cwd rejected: {e}")),
                        },
                    );
                    return;
                }
            };
            log_info!("[runner] start_check_agent: {agent_id} cwd={}", cwd_path.display());

            let mut child = match std::process::Command::new("claude")
                .args([
                    "-p",
                    "--verbose",
                    "--output-format",
                    "stream-json",
                    "--input-format",
                    "stream-json",
                    "--session-id",
                    &session_id,
                    "--add-dir",
                    &cwd_path.to_string_lossy(),
                    "--permission-mode",
                    "plan",
                    "--allowedTools",
                    "Read,Glob,Grep,Bash(git:*)",
                    "--effort",
                    &effort,
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .current_dir(&cwd_path)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    log_warn!(
                        "[runner] start_check_agent {agent_id}: failed to spawn claude: {e}"
                    );
                    send_best_effort(
                        state,
                        WireMessage::AgentStopped {
                            agent_id: agent_id.clone(),
                            status: "failed".into(),
                            cost_usd: None,
                            stop_reason: Some(format!("spawn claude: {e}")),
                        },
                    );
                    return;
                }
            };

            // Pipe the prompt into stdin as a single stream-json `user`
            // message and close stdin. Same shape the standalone path
            // writes in `check_agent.rs:88-99`.
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let msg = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": prompt}]
                    }
                });
                let _ = writeln!(stdin, "{msg}");
                drop(stdin);
            }

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    send_best_effort(
                        state,
                        WireMessage::AgentStopped {
                            agent_id: agent_id.clone(),
                            status: "failed".into(),
                            cost_usd: None,
                            stop_reason: Some("claude stdout was None".into()),
                        },
                    );
                    return;
                }
            };

            // Spawn a tokio blocking task to drain stdout line-by-line.
            // Each line is a complete stream-json record; forward as
            // `CheckAgentOutput`. When stdout closes, `child.wait()` for
            // the exit code and emit `AgentStopped`.
            let state_clone = state.clone();
            let aid = agent_id.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, BufReader};
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    if line.is_empty() {
                        continue;
                    }
                    send_best_effort(
                        &state_clone,
                        WireMessage::CheckAgentOutput {
                            agent_id: aid.clone(),
                            line,
                        },
                    );
                }
                let exit = child.wait().ok();
                let code = exit.and_then(|s| s.code()).unwrap_or(-1);
                let status = if code == 0 { "completed" } else { "failed" };
                send_best_effort(
                    &state_clone,
                    WireMessage::AgentStopped {
                        agent_id: aid,
                        status: status.into(),
                        cost_usd: None,
                        stop_reason: if code == 0 {
                            None
                        } else {
                            Some(format!("claude exit code {code}"))
                        },
                    },
                );
            });
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
                    log_warn!("[runner] get_default_branch rejected cwd: {e}");
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
                    log_warn!("[runner] list_branches rejected cwd: {e}");
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

        WireMessage::DiscardBranch {
            req_id,
            cwd,
            target,
            task_branch,
        } => {
            let req_id = req_id.clone();
            let target = target.clone();
            let task_branch = task_branch.clone();
            let (ok, error) = match validated_cwd(state, cwd) {
                Ok(path) => match run_blocking_with_timeout(MERGE_TIMEOUT, move || {
                    git_helpers::discard_branch_local(&path, &target, &task_branch)
                })
                .await
                {
                    Some(Ok(())) => (true, None),
                    Some(Err(e)) => (false, Some(e)),
                    None => (
                        false,
                        Some(format!(
                            "discard timed out after {}s",
                            MERGE_TIMEOUT.as_secs()
                        )),
                    ),
                },
                Err(e) => (false, Some(e)),
            };
            send_best_effort(state, WireMessage::BranchDiscarded { req_id, ok, error });
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
                        // SaaS-mode visibility gaps tracked as follow-ups:
                        // (a) rebase-retry history is discarded — the
                        //     wire today carries only `{ok, stderr}`.
                        // (b) `PushError::RebaseConflict { files }`
                        //     collapses into the same flat stderr string,
                        //     so the SaaS server treats a runner-side
                        //     rebase conflict as a generic push failure
                        //     and does NOT fire the
                        //     `auto_push_rebase_conflict` pause path.
                        // Both are intentional for Phase 1; extending
                        // `PushResult` would un-block the SaaS side.
                        Some(Ok(_report)) => (true, None),
                        Some(Err(e)) => (false, Some(e.to_string())),
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
                    log_warn!("[runner] gh_run_list rejected cwd: {e}");
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
                    log_warn!("[runner] gh_failure_log rejected cwd: {e}");
                    None
                }
            };
            send_best_effort(state, WireMessage::GhFailureLogFetched { req_id, log });
        }

        WireMessage::MergeAgentBranch {
            req_id,
            agent_id,
            into,
        } => {
            let req_id = req_id.clone();
            let agent_id = agent_id.clone();
            let into = into.clone();

            // Look up the agent's cwd from the runner's in-memory registry.
            // AgentHandle is inserted at `StartAgent` and never removed on a
            // clean exit, so the auto-mode loop's "agent finished, now merge"
            // round-trip finds the right path.
            let agent_cwd = state.agents.lock().await.get(&agent_id).map(|h| h.cwd.clone());

            let Some(cwd) = agent_cwd else {
                send_best_effort(
                    state,
                    WireMessage::AgentBranchMerged {
                        req_id,
                        ok: false,
                        merged_sha: None,
                        target_branch: String::new(),
                        had_conflict: false,
                        error: Some("agent_not_found_on_runner".to_string()),
                    },
                );
                return;
            };

            let cwd_for_blocking = cwd.clone();
            let into_for_blocking = into.clone();
            let result = run_blocking_with_timeout(MERGE_TIMEOUT, move || {
                merge_agent_branch_on_runner(&cwd_for_blocking, into_for_blocking.as_deref())
            })
            .await;

            let reply = match result {
                Some(merge) => WireMessage::AgentBranchMerged {
                    req_id,
                    ok: merge.merged_sha.is_some(),
                    merged_sha: merge.merged_sha,
                    target_branch: merge.target_branch,
                    had_conflict: merge.had_conflict,
                    error: merge.error,
                },
                None => WireMessage::AgentBranchMerged {
                    req_id,
                    ok: false,
                    merged_sha: None,
                    target_branch: String::new(),
                    had_conflict: false,
                    error: Some(format!(
                        "merge timed out after {}s",
                        MERGE_TIMEOUT.as_secs()
                    )),
                },
            };
            send_best_effort(state, reply);
        }

        WireMessage::HasGithubActions { req_id, agent_id } => {
            let req_id = req_id.clone();
            let agent_id = agent_id.clone();

            let agent_cwd = state.agents.lock().await.get(&agent_id).map(|h| h.cwd.clone());

            let present = match agent_cwd {
                Some(cwd) => run_blocking_with_timeout(READ_TIMEOUT, move || {
                    has_github_actions(&cwd)
                })
                .await
                .unwrap_or(false),
                None => false,
            };

            send_best_effort(
                state,
                WireMessage::GithubActionsDetected { req_id, present },
            );
        }

        WireMessage::GetCiRunStatus {
            req_id,
            plan_name,
            task_number: _,
            merged_sha,
            blocking_workflows,
        } => {
            let req_id = req_id.clone();
            let plan_name = plan_name.clone();
            let merged_sha = merged_sha.clone();
            let blocking_workflows = blocking_workflows.clone();

            // Always record plan→sha so a follow-up `CiFailureLog { run_id:
            // None }` can re-resolve.
            {
                let mut cache = state.ci_cache.lock().await;
                cache.record_plan_sha(plan_name.clone(), merged_sha.clone());
            }

            // Fast path: serve a fresh cached aggregate. The cache is keyed
            // by SHA only — same SHA + same allowlist hits the cached
            // entry. A different allowlist coming in for the same SHA
            // (rare in practice — the user would have to edit
            // `ci_blocking_workflows` mid-poll) recomputes from the cached
            // raw runs via `re_filter_cached_aggregate` so we don't hit
            // `gh` again.
            let cached = state.ci_cache.lock().await.fresh(&merged_sha);

            let aggregate = if let Some(cached) = cached {
                re_filter_cached_aggregate(cached, blocking_workflows.as_deref())
            } else {
                let cwd = state
                    .plan_cwd
                    .lock()
                    .await
                    .get(&plan_name)
                    .cloned()
                    .unwrap_or_else(|| state.cwd.clone());

                let allowlist = blocking_workflows.clone();
                let computed = run_blocking_with_timeout(GH_TIMEOUT, {
                    let sha = merged_sha.clone();
                    move || compute_ci_aggregate(&cwd, &sha, allowlist.as_deref())
                })
                .await
                .unwrap_or(None);

                state
                    .ci_cache
                    .lock()
                    .await
                    .put(merged_sha.clone(), computed.clone());
                computed
            };

            send_best_effort(
                state,
                WireMessage::CiRunStatusResolved { req_id, aggregate },
            );
        }

        WireMessage::CiFailureLog {
            req_id,
            plan_name,
            run_id,
        } => {
            let req_id = req_id.clone();
            let plan_name = plan_name.clone();
            let explicit_run_id = run_id.clone();

            // Resolve the run id to fetch — explicit on the wire, otherwise
            // re-resolved from the most recent cached aggregate for this
            // plan. This is the auto-mode 3.1 path: by the time the loop
            // sees a Red outcome it has dropped the run id.
            let resolved_run_id: Option<String> = match explicit_run_id {
                Some(id) if !id.is_empty() => Some(id),
                _ => {
                    let cache = state.ci_cache.lock().await;
                    let latest_sha = cache.latest_sha_for_plan(&plan_name);
                    latest_sha
                        .and_then(|sha| cache.by_sha.get(&sha).cloned())
                        .and_then(|entry| entry.aggregate)
                        .and_then(|agg| agg.failing_run_id)
                }
            };

            let Some(run_id_used) = resolved_run_id else {
                send_best_effort(
                    state,
                    WireMessage::CiFailureLogResolved {
                        req_id,
                        log: None,
                        run_id_used: None,
                    },
                );
                return;
            };

            // Resolve cwd the same way `GetCiRunStatus` does — fall back to
            // the runner's canonical root if the plan hasn't started any
            // agents yet (defensive; should not happen on the auto-mode
            // path because the loop only fetches logs after a merge).
            let cwd = state
                .plan_cwd
                .lock()
                .await
                .get(&plan_name)
                .cloned()
                .unwrap_or_else(|| state.cwd.clone());

            let run_id_for_blocking = run_id_used.clone();
            let log = run_blocking_with_timeout(GH_TIMEOUT, move || {
                git_helpers::gh_failure_log_local(&cwd, &run_id_for_blocking)
            })
            .await
            .unwrap_or(None);

            send_best_effort(
                state,
                WireMessage::CiFailureLogResolved {
                    req_id,
                    log,
                    run_id_used: Some(run_id_used),
                },
            );
        }

        // Runner doesn't receive these from server (runner→saas direction
        // only; the server sending them would be a protocol violation).
        WireMessage::RunnerHello { .. }
        | WireMessage::AgentStarted { .. }
        | WireMessage::AgentOutput { .. }
        | WireMessage::AgentStopped { .. }
        | WireMessage::AgentSpawnFailed { .. }
        | WireMessage::TaskStatusChanged { .. }
        | WireMessage::DriverAuthReport { .. }
        | WireMessage::FoldersListed { .. }
        | WireMessage::FolderCreated { .. }
        | WireMessage::DefaultBranchResolved { .. }
        | WireMessage::BranchesListed { .. }
        | WireMessage::MergeResult { .. }
        | WireMessage::BranchDiscarded { .. }
        | WireMessage::CheckAgentOutput { .. }
        | WireMessage::PushResult { .. }
        | WireMessage::GhRunListed { .. }
        | WireMessage::GhFailureLogFetched { .. }
        | WireMessage::AgentBranchMerged { .. }
        | WireMessage::GithubActionsDetected { .. }
        | WireMessage::CiRunStatusResolved { .. }
        | WireMessage::CiFailureLogResolved { .. }
        // The runner is the producer of `RunnerLogLine` and
        // `RunnerHealth`; receiving either back from the server is a
        // protocol violation we silently drop.
        | WireMessage::RunnerLogLine { .. }
        | WireMessage::RunnerHealth { .. }
        // saas→runner variants the runner doesn't act on (no current handler).
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
            log_warn!("[runner] blocking task panicked: {e}");
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

/// Resolve a runner-side folder path. Three states:
///   * `~` or `~/<rest>` → `$HOME / <rest>` (tilde-expansion)
///   * absolute path     → returned as-is
///   * bare name         → `$HOME / <name>` (NOT cwd-relative)
///
/// The bare-name → `$HOME/<name>` rule matches the user's mental model
/// ("this is a folder name under my home directory") and the standalone
/// resolver in `api::plans::resolve_folder_path`. Pre-2026-05-07 a bare
/// name like `myproj` would fall through to `PathBuf::from("myproj")`,
/// then `mkdir -p` would treat it as absolute (`/myproj`) — wrong, just
/// differently from the server-side bug.
fn resolve_runner_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        dirs::home_dir()
            .unwrap_or_default()
            .join(rest.trim_start_matches('/'))
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        dirs::home_dir().unwrap_or_default().join(path)
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
        // T4.2 cadence plan: scaffold `branchwork.toml` with
        // `merge_cadence = "phase"` ONLY when we just made the folder.
        // `existed_before` is captured pre-`create_dir_all` so the
        // `mkdir -p`'s built-in idempotence doesn't mask the "already
        // there" case. Pre-existing folders keep their existing
        // `branchwork.toml` (or inherit `MergeCadence::default() =
        // phase` when absent) per the acceptance criterion.
        let existed_before = resolved.exists();
        match std::fs::create_dir_all(&resolved) {
            Ok(()) if resolved.is_dir() => {
                if !existed_before {
                    project_scaffold::scaffold_branchwork_toml_if_missing(&resolved);
                }
                (true, resolved_str, None)
            }
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

// ── Auto-mode handler helpers ───────────────────────────────────────────────
//
// The four high-level handlers (MergeAgentBranch, HasGithubActions,
// GetCiRunStatus, CiFailureLog) all delegate to small synchronous helpers
// that the handler arm wraps in `run_blocking_with_timeout`. Keeping the
// helpers pure-sync (and small) means they can be unit-tested directly
// against tempdir git repos and stub `gh` outputs.

/// Outcome of [`merge_agent_branch_on_runner`]. Wire conversion happens
/// in the `MergeAgentBranch` arm of [`handle_server_message`]: this struct
/// just carries the four pieces of information the wire reply needs.
struct AgentMergeResult {
    merged_sha: Option<String>,
    target_branch: String,
    had_conflict: bool,
    error: Option<String>,
}

/// Run the same five-step merge sequence as `api::agents::merge_agent_branch_inner`,
/// resolving inputs locally:
///
/// 1. Resolve `task_branch` from the cwd's current `HEAD`. Auto-mode policy
///    leaves the agent on its task branch on exit, so this is reliable.
/// 2. Resolve the canonical default branch via `git_default_branch`.
/// 3. Pick a target: the dropdown override (if it appears in
///    `git_list_branches`), otherwise the canonical default, otherwise
///    `"main"` — same precedence as `api::agents::resolve_merge_target`.
/// 4. Run `merge_branch_local` (no-commit guard, checkout, merge,
///    branch cleanup).
/// 5. On success, push origin if the target equals the canonical default.
///    Push errors are swallowed — the merge is the load-bearing step; CI
///    gating is owned by `HasGithubActions` upstream.
fn merge_agent_branch_on_runner(cwd: &Path, into: Option<&str>) -> AgentMergeResult {
    let Some(task_branch) = git_helpers::git_current_branch(cwd) else {
        return AgentMergeResult {
            merged_sha: None,
            target_branch: String::new(),
            had_conflict: false,
            error: Some("could not resolve task branch (HEAD detached?)".to_string()),
        };
    };

    let default = git_helpers::git_default_branch(cwd);
    let explicit = into.filter(|s| !s.is_empty());
    let target = if let Some(into) = explicit {
        let branches = git_helpers::git_list_branches(cwd);
        if branches.iter().any(|b| b == into) {
            into.to_string()
        } else {
            default.clone().unwrap_or_else(|| "main".to_string())
        }
    } else {
        default.clone().unwrap_or_else(|| "main".to_string())
    };

    if task_branch == target {
        return AgentMergeResult {
            merged_sha: None,
            target_branch: target,
            had_conflict: false,
            error: Some("empty_branch".to_string()),
        };
    }

    match git_helpers::merge_branch_local(cwd, &target, &task_branch) {
        MergeOutcome::Ok { merged_sha } => {
            // Push only when target is the canonical default — same gate as
            // `ci::should_record_ci_run` on the server side.
            if default.as_deref() == Some(target.as_str()) {
                let _ = git_helpers::push_branch_local(cwd, &target);
            }
            AgentMergeResult {
                merged_sha: Some(merged_sha),
                target_branch: target,
                had_conflict: false,
                error: None,
            }
        }
        MergeOutcome::EmptyBranch => AgentMergeResult {
            merged_sha: None,
            target_branch: target,
            had_conflict: false,
            error: Some("empty_branch".to_string()),
        },
        MergeOutcome::Conflict { stderr } => AgentMergeResult {
            merged_sha: None,
            target_branch: target,
            had_conflict: true,
            error: Some(stderr),
        },
        MergeOutcome::CheckoutFailed { stderr } => AgentMergeResult {
            merged_sha: None,
            target_branch: target,
            had_conflict: false,
            error: Some(format!("checkout failed: {stderr}")),
        },
        MergeOutcome::Other { stderr } => AgentMergeResult {
            merged_sha: None,
            target_branch: target,
            had_conflict: false,
            error: Some(stderr),
        },
    }
}

/// Glob `cwd/.github/workflows/*.{yml,yaml}` and report whether at least
/// one match exists. Filename-only check — no YAML parsing, since the
/// auto-mode loop just needs to know whether to enter the CI-gate state.
fn has_github_actions(cwd: &Path) -> bool {
    let workflows = cwd.join(".github").join("workflows");
    let Ok(rd) = std::fs::read_dir(&workflows) else {
        return false;
    };
    rd.flatten().any(|e| {
        let path = e.path();
        path.is_file()
            && path
                .extension()
                .and_then(|s| s.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
                .unwrap_or(false)
    })
}

/// One workflow run as parsed from `gh run list --json
/// databaseId,workflowName,status,conclusion,createdAt`. Fields beyond
/// `databaseId` are best-effort: defaulted on absent JSON keys so a stub
/// `gh` (or a future schema change) doesn't turn the whole aggregate into
/// `None`.
#[derive(Debug, Deserialize, Clone)]
struct GhRunDetail {
    #[serde(rename = "databaseId")]
    database_id: i64,
    #[serde(rename = "workflowName", default)]
    workflow_name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    /// ISO-8601 timestamp. Used as the workflow-graph-order fallback for
    /// `failing_run_id` (the brief asks for `dependsOn`/`workflow_run`
    /// first, but `gh` doesn't expose those).
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

/// Shell `gh run list --commit <sha> --json
/// databaseId,workflowName,status,conclusion,createdAt --limit 50` in
/// `cwd`. Returns `None` only when the call itself failed (gh not
/// installed, no auth, etc) — an empty result set comes back as
/// `Some(vec![])` so the aggregator can distinguish "still polling" from
/// "tooling broken."
fn gh_run_list_full(cwd: &Path, sha: &str) -> Option<Vec<GhRunDetail>> {
    let out = std::process::Command::new("gh")
        .args([
            "run",
            "list",
            "--commit",
            sha,
            "--json",
            "databaseId,workflowName,status,conclusion,createdAt",
            "--limit",
            "50",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Compute the per-SHA aggregate from runner-side `gh` shell-outs. Returns
/// `None` when `gh run list` failed, when no runs exist for the SHA yet,
/// or when the runs vec is empty (still polling). The `Reglyze`-shape
/// aggregation — failure-poisons-skip — is in [`aggregate_runs`].
///
/// `blocking_workflows` is the level-1-3 explicit allowlist resolved
/// server-side and shipped to the runner via [`WireMessage::GetCiRunStatus`].
/// `Some(list)` ⇒ apply the list verbatim (`BlockingSource::Config`).
/// `None` ⇒ runner runs the level-4 smart classifier itself
/// (`BlockingSource::Classifier`). See [`aggregate_runs`] for the
/// workflow-graph rule and the per-run `informational` partition.
///
/// Wall-clock for the full `gh run list` + per-run `gh run view` chain is
/// recorded into the trailing CI-poll latency ring so the next
/// `RunnerHealth` tick reports fresh p50/p99 numbers. Latency is captured
/// even when the call returns `None` (gh missing / no runs yet) — the
/// dashboard cares about how long the call took, not whether it found
/// anything.
fn compute_ci_aggregate(
    cwd: &Path,
    sha: &str,
    blocking_workflows: Option<&[String]>,
) -> Option<CiAggregate> {
    let started = Instant::now();
    let result = compute_ci_aggregate_inner(cwd, sha, blocking_workflows);
    let elapsed_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    runner_health::record_ci_poll_ms(elapsed_ms);
    result
}

fn compute_ci_aggregate_inner(
    cwd: &Path,
    sha: &str,
    blocking_workflows: Option<&[String]>,
) -> Option<CiAggregate> {
    let runs = gh_run_list_full(cwd, sha)?;
    if runs.is_empty() {
        return None;
    }
    Some(aggregate_runs(runs, blocking_workflows))
}

/// Build the per-SHA aggregate from `gh run list` rows. Sorts inputs by
/// `createdAt` ascending so the shared rule in
/// [`ci::aggregate::compute_with_filter`] picks the root-cause failure as
/// `failing_run_id` rather than a downstream one. Mode-shared aggregation
/// rule (Reglyze regression test lives in `ci::aggregate::tests`).
///
/// `blocking_workflows` carries the resolved level-1-3 explicit allowlist
/// from the server. When `Some(list)`, runs whose `workflow_name` is not
/// in the list are flagged informational and excluded from the aggregate
/// `conclusion` / `failing_run_id` (audit log: `via-config`). When
/// `None`, the level-4 smart classifier
/// (`(?i)docker|deploy|publish|release|bench|fuzz`) is applied to the
/// runner's own discovered workflow names (audit log: `via-classifier`).
fn aggregate_runs(
    mut runs: Vec<GhRunDetail>,
    blocking_workflows: Option<&[String]>,
) -> CiAggregate {
    use ci::aggregate::is_workflow_blocking_by_default;
    use ci::aggregate::{BlockingFilter, compute_with_filter};

    // Sort by createdAt ascending. `gh` emits ISO-8601 strings which sort
    // lexicographically into chronological order; missing timestamps stay
    // in input order via `Option`'s default `Ord` (None < Some(_)).
    runs.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut summaries: Vec<CiRunSummary> = runs
        .iter()
        .map(|r| CiRunSummary {
            run_id: r.database_id.to_string(),
            workflow_name: r.workflow_name.clone(),
            status: if r.status.is_empty() {
                "completed".to_string()
            } else {
                r.status.clone()
            },
            conclusion: r.conclusion.clone(),
            // mark_upstream_skips below sets this — initialize to false so
            // the rule has a single source of truth.
            skipped_due_to_upstream: false,
            // compute_with_filter sets this from the partition decision.
            informational: false,
        })
        .collect();

    ci::aggregate::mark_upstream_skips(&mut summaries);

    match blocking_workflows {
        Some(list) => {
            let filter = BlockingFilter::config(list);
            compute_with_filter(&summaries, Some(&filter))
        }
        None => {
            // Level-4 fallback: classifier over discovered workflow names.
            // Build the allowlist from the runs themselves, then apply
            // compute_with_filter so the per-run `informational` flag is
            // set the same way as in the explicit-allowlist path.
            let allowlist: Vec<String> = summaries
                .iter()
                .map(|s| s.workflow_name.clone())
                .filter(|name| is_workflow_blocking_by_default(name))
                .collect();
            let filter = BlockingFilter::classifier(&allowlist);
            compute_with_filter(&summaries, Some(&filter))
        }
    }
}

/// Re-apply the resolved allowlist to a previously-cached aggregate. The
/// runner's `CiAggregateCache` is keyed by SHA only — same SHA + same
/// allowlist returns the cached aggregate cheaply, but a different
/// allowlist coming in for the same SHA (e.g. the user edited
/// `ci_blocking_workflows` between two polls) needs the cached
/// `CiRunSummary` rows re-partitioned without paying for another
/// `gh run list` shell-out.
///
/// The cached aggregate already has `skipped_due_to_upstream` set
/// correctly — that flag depends only on the runs themselves — so we
/// pass the cached `runs` straight back through `compute_with_filter`.
fn re_filter_cached_aggregate(
    cached: Option<CiAggregate>,
    blocking_workflows: Option<&[String]>,
) -> Option<CiAggregate> {
    use ci::aggregate::is_workflow_blocking_by_default;
    use ci::aggregate::{BlockingFilter, compute_with_filter};

    let cached = cached?;
    let summaries = cached.runs;

    let aggregate = match blocking_workflows {
        Some(list) => {
            let filter = BlockingFilter::config(list);
            compute_with_filter(&summaries, Some(&filter))
        }
        None => {
            let allowlist: Vec<String> = summaries
                .iter()
                .map(|s| s.workflow_name.clone())
                .filter(|name| is_workflow_blocking_by_default(name))
                .collect();
            let filter = BlockingFilter::classifier(&allowlist);
            compute_with_filter(&summaries, Some(&filter))
        }
    };
    Some(aggregate)
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
/// Driver-specific env vars to set on the spawned CLI process. Mirrors
/// `AgentDriver::extra_env` in the server crate — kept as a tiny inline lookup
/// because the runner binary does not pull in the driver module. Only Claude
/// declares env vars today; other drivers return an empty slice.
///
/// `CLAUDE_CODE_SANDBOXED=1` is the official Anthropic-supported flag for
/// containerised launches: it short-circuits Claude Code's trust-workspace
/// gate so the binary never blocks at the "Trust this folder?" dialog when
/// the runner spawns it into a freshly-created project directory.
fn extra_env_for_driver(driver: &str) -> Vec<(String, String)> {
    match driver {
        "claude" => vec![("CLAUDE_CODE_SANDBOXED".to_string(), "1".to_string())],
        "bob" => {
            // Bob Shell needs ANTHROPIC_API_KEY to authenticate with the API.
            // Pass it through from the runner's environment if available.
            if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                vec![("ANTHROPIC_API_KEY".to_string(), key)]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Pre-spawn task-branch checkout. Mirrors what
/// `pty_agent::start_pty_agent` does in standalone — switch to (or
/// create) `branchwork/<plan>/<task>` so anything the agent commits
/// lands on the right branch and the merge button finds a ref to
/// `git merge` against.
///
/// Two-step to preserve prior commits on a respawn:
///   1. `git checkout <branch>` — succeeds if the branch already
///      exists from a previous run (rare but happens when an agent
///      is killed mid-work and re-Started later).
///   2. on failure, `git checkout -b <branch>` — creates from
///      current HEAD.
///
/// Failure is logged but doesn't abort the spawn: cwd-not-a-git-repo
/// cases (folder created via `CreateFolder` and not yet `git
/// init`'d) used to work pre-T5.16 because no checkout was
/// attempted. Don't regress that — let the supervisor + claude
/// proceed and the agent prompt will guide a `git init` if
/// applicable.
fn checkout_task_branch(cwd: &Path, branch: &str) {
    // Check if cwd is a git repo at all. If not, log and skip — same
    // posture standalone takes when `ensure_git_initialized` returns
    // false: don't fail the spawn, just degrade.
    let in_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    let in_repo_ok = matches!(
        &in_repo,
        Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true"
    );
    if !in_repo_ok {
        log_warn!(
            "[runner] checkout_task_branch: {} is not a git repo, skipping {branch} checkout",
            cwd.display()
        );
        return;
    }

    // Try plain checkout first — preserves an existing branch's
    // commits if the agent is being respawned.
    let plain = std::process::Command::new("git")
        .args(["checkout", branch])
        .current_dir(cwd)
        .output();
    if matches!(&plain, Ok(o) if o.status.success()) {
        log_info!("[runner] checkout_task_branch: switched to existing {branch}");
        return;
    }

    // Branch doesn't exist — create it from current HEAD.
    let create = std::process::Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(cwd)
        .output();
    match create {
        Ok(o) if o.status.success() => {
            log_info!("[runner] checkout_task_branch: created {branch}");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            log_warn!(
                "[runner] checkout_task_branch: `git checkout -b {branch}` failed in {}: {}",
                cwd.display(),
                stderr.trim()
            );
        }
        Err(e) => {
            log_warn!(
                "[runner] checkout_task_branch: failed to run git in {}: {e}",
                cwd.display()
            );
        }
    }
}

#[cfg(test)]
mod checkout_task_branch_tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) {
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    fn current_branch(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// New branch case — `git checkout -b` fallback. Pre-T5.16 the
    /// runner skipped this entirely and the agent inherited
    /// whatever HEAD was, which intermittently meant master and
    /// merge later 503'd with "not something we can merge".
    #[test]
    fn checkout_creates_branch_when_missing() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        assert_eq!(current_branch(dir.path()), "main");

        checkout_task_branch(dir.path(), "branchwork/test-plan/0.1");
        assert_eq!(current_branch(dir.path()), "branchwork/test-plan/0.1");
    }

    /// Respawn case — branch already exists, plain `git checkout`
    /// preserves its commits. Use a sentinel commit on the branch
    /// to prove we didn't reset it via `-B`.
    #[test]
    fn checkout_preserves_existing_branch_commits() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        Command::new("git")
            .args(["checkout", "-b", "branchwork/test-plan/0.1"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("sentinel"), "x").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "sentinel"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        checkout_task_branch(dir.path(), "branchwork/test-plan/0.1");
        assert_eq!(current_branch(dir.path()), "branchwork/test-plan/0.1");
        assert!(
            dir.path().join("sentinel").exists(),
            "sentinel must survive"
        );
    }

    /// Non-git cwd — degrade gracefully. Pre-T5.16 the runner just
    /// didn't attempt the checkout; T5.16 must not regress that
    /// path or `CreateFolder`-without-init flows fail spawn.
    #[test]
    fn checkout_skips_when_not_a_git_repo() {
        let dir = TempDir::new().unwrap();
        // No `git init` — function should log and return without
        // touching anything.
        checkout_task_branch(dir.path(), "branchwork/test-plan/0.1");
        // Nothing to assert beyond "no panic"; the call must be
        // infallible from the caller's perspective.
        assert!(!dir.path().join(".git").exists());
    }
}

/// Error returned by [`spawn_agent`]. The two variants let the caller
/// distinguish a structured `Command::spawn` failure (binary not on PATH,
/// permission denied, ...) from any other unexpected error during the
/// pre/post-spawn dance. The first turns into a `AgentSpawnFailed` wire
/// envelope so the dashboard can render a precise reason next to the
/// agent card; the second falls through to the existing `AgentStopped`
/// path with a generic `stop_reason`.
#[derive(Debug)]
enum SpawnAgentError {
    /// `Command::spawn` returned `Err`. Carries the OS error and the path
    /// the runner attempted to spawn (i.e. `state.server_bin`) so the
    /// caller can build a structured `AgentSpawnFailed` envelope without
    /// re-resolving them.
    Spawn { io: std::io::Error, command: String },
    /// Any other failure — mkdir, settings.json write, socket-never-
    /// appeared, etc. Kept opaque because the existing `AgentStopped`
    /// path stringifies it verbatim into `stop_reason`.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for SpawnAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnAgentError::Spawn { io, command } => {
                write!(
                    f,
                    "could not spawn {command} ({}): {io}",
                    classify_spawn_io_error(io).1
                )
            }
            SpawnAgentError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl<E: Into<Box<dyn std::error::Error + Send + Sync>>> From<E> for SpawnAgentError {
    fn from(e: E) -> Self {
        SpawnAgentError::Other(e.into())
    }
}

/// Map an `io::Error` from `Command::spawn` onto `(Option<errno>, errno_tag)`.
/// The tag is a short human-readable label ("ENOENT", "EACCES", ...) that
/// the dashboard surfaces inline so operators don't have to look up errnos.
/// `errno` is the raw OS code when available — `None` for io::Errors that
/// didn't carry one. Always returns a non-empty tag so callers don't have
/// to handle the missing-tag case.
fn classify_spawn_io_error(io: &std::io::Error) -> (Option<i32>, String) {
    use std::io::ErrorKind;
    let raw = io.raw_os_error();
    // Prefer the ErrorKind tag (portable) and fall back to a generic
    // "OS_ERROR(<n>)" label so a future kind we haven't enumerated still
    // produces something useful on the dashboard.
    let tag = match io.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::Other => match raw {
            Some(c) => return (Some(c), format!("OS_ERROR({c})")),
            None => "IO_ERROR",
        },
        _ => match raw {
            Some(c) => return (Some(c), format!("OS_ERROR({c})")),
            None => "IO_ERROR",
        },
    };
    (raw, tag.to_string())
}

#[allow(clippy::too_many_arguments)] // Mirrors driver::SpawnOpts on the server side.
async fn spawn_agent(
    state: &Arc<RunnerState>,
    agent_id: &str,
    session_id: &str,
    cwd: &Path,
    driver: &str,
    prompt: &str,
    effort: Option<&str>,
    max_budget_usd: Option<f64>,
    skip_permissions: bool,
    mcp_config: &str,
    settings_json: &str,
) -> Result<(), SpawnAgentError> {
    // Build socket path.
    let sockets_dir = state.cwd.join(".branchwork-runner-sessions");
    std::fs::create_dir_all(&sockets_dir)?;
    let socket_path = sockets_dir.join(format!("{agent_id}.sock"));

    // Materialise `--mcp-config` / `--settings` bodies the server pre-rendered
    // for us. Empty wire field means the driver opted out — skip both the
    // file write and the corresponding flag (matches standalone semantics
    // in `pty_agent::start_pty_agent`).
    //
    // Best-effort write: a failure here means the agent runs without the
    // optional integration but the spawn itself proceeds. The server has
    // no way to recover if either flag is missing, so logging + degrading
    // is the same posture standalone takes (see pty_agent.rs:124-156).
    //
    // File paths are deterministic so the cleanup path can resolve them
    // from `agent_id` alone without a side-channel handle.
    let mcp_config_path = if mcp_config.is_empty() {
        None
    } else {
        let path = sockets_dir.join(format!("{agent_id}.mcp.json"));
        match std::fs::write(&path, mcp_config) {
            Ok(()) => Some(path),
            Err(e) => {
                log_warn!(
                    "[runner] agent {agent_id}: failed to write mcp_config at {}: {e}",
                    path.display()
                );
                None
            }
        }
    };
    let settings_path = if settings_json.is_empty() {
        None
    } else {
        let path = sockets_dir.join(format!("{agent_id}.settings.json"));
        match std::fs::write(&path, settings_json) {
            Ok(()) => Some(path),
            Err(e) => {
                log_warn!(
                    "[runner] agent {agent_id}: failed to write settings_json at {}: {e}",
                    path.display()
                );
                None
            }
        }
    };

    // Build the command to spawn. The session daemon expects:
    // branchwork-server session --socket <path> --cwd <dir> [--cols C --rows R]
    //   [--env K=V ...] -- <cmd...>
    let binary = match driver {
        "claude" => "claude",
        "aider" => "aider",
        "codex" => "codex",
        "gemini" => "gemini",
        "bob" => "bob",
        _ => "claude",
    };

    let mut args = vec![
        "session".to_string(),
        "--socket".to_string(),
        socket_path.display().to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        // Match standalone's supervisor::spawn_session_daemon defaults so
        // the daemon's `ps -ef` shows the same `--cols/--rows` block the
        // dashboard expects (T5.2 acceptance).
        "--cols".to_string(),
        "120".to_string(),
        "--rows".to_string(),
        "40".to_string(),
    ];

    // Driver-specific env vars. Mirrors `AgentDriver::extra_env` from the
    // server's driver registry — kept inline because the runner binary does
    // not pull in the full driver module via `#[path]`. Only Claude declares
    // env vars today (CLAUDE_CODE_SANDBOXED=1, which skips the trust-workspace
    // dialog the binary would otherwise block on in a never-seen folder).
    for (k, v) in extra_env_for_driver(binary) {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }

    args.push("--".to_string());
    args.push(binary.to_string());

    // Claude's full flag set (T5.2 — parity with standalone's
    // `ClaudeDriver::spawn_args`). Order matches the standalone driver so
    // the resulting `ps -ef` line is byte-identical except for the cwd /
    // session_id / temp paths that necessarily differ per spawn.
    if binary == "claude" {
        args.push("--session-id".to_string());
        args.push(session_id.to_string());
        args.push("--add-dir".to_string());
        args.push(cwd.display().to_string());
        args.push("--verbose".to_string());
        if let Some(eff) = effort {
            args.push("--effort".to_string());
            args.push(eff.to_string());
        }
        if skip_permissions {
            args.push("--dangerously-skip-permissions".to_string());
        }
        if let Some(v) = max_budget_usd {
            args.push("--max-budget-usd".to_string());
            args.push(v.to_string());
        }
        if let Some(p) = mcp_config_path.as_deref() {
            args.push("--mcp-config".to_string());
            args.push(p.display().to_string());
        }
        if let Some(p) = settings_path.as_deref() {
            args.push("--settings".to_string());
            args.push(p.display().to_string());
        }
    } else if binary == "bob" {
        // Bob Shell flags (mirrors BobDriver::spawn_args from driver.rs)
        if skip_permissions {
            args.push("--yolo".to_string());
        }
        if let Some(p) = mcp_config_path.as_deref() {
            args.push("--mcp-config".to_string());
            args.push(p.display().to_string());
        }
    } else {
        // Non-claude drivers keep their pre-T5.2 minimal flag set — this
        // task only widens the surface for Claude. Per-driver follow-up
        // tasks (T5.x of `dashboard-stability`) extend the others.
        if let Some(eff) = effort {
            args.push("--effort".to_string());
            args.push(eff.to_string());
        }
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

    // `Command::spawn` is the load-bearing call this whole task is about
    // (Task 1.1, runner-install-and-spawn-reliability plan). On Err, hand
    // the io::Error + the command path back through `SpawnAgentError::Spawn`
    // so the caller can emit a structured `AgentSpawnFailed` envelope with
    // errno + errno_tag. Other failure modes (mkdir, file write, the
    // socket-didn't-appear timeout below) still flow through the generic
    // `SpawnAgentError::Other` path and the dashboard sees only the
    // free-form `stop_reason` string from `AgentStopped`.
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(io) => {
            return Err(SpawnAgentError::Spawn {
                io,
                command: state.server_bin.display().to_string(),
            });
        }
    };
    let fork_parent_pid = child.id();

    log_info!(
        "[runner] spawned session daemon (fork-parent pid={fork_parent_pid}) socket={}",
        socket_path.display()
    );

    // Wait for the socket to appear.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket_path.exists() {
        if tokio::time::Instant::now() > deadline {
            return Err(SpawnAgentError::Other(
                "session daemon socket did not appear within 10s".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Small extra delay for the daemon to start listening.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Resolve the actual long-lived daemon PID. `cmd.spawn()` returns
    // the fork-parent that `_exit`s in `supervisor::run_session`'s
    // double-fork; the daemon continues in the second-fork child after
    // `setsid()`, and writes ITS pid to the `<socket>.pid` file early in
    // `run_daemon`. Pre-T5.13 the runner stored `fork_parent_pid` in
    // `AgentHandle.pid` and `KillAgent` then `libc::kill`d a defunct pid
    // — the no-op killed nothing, the daemon kept running, and the
    // claude child it had spawned via `pty.spawn_command` orphaned cleanly
    // off init. Result: every Finish / Kill click left zombie claudes.
    //
    // Read the pidfile (best-effort — if missing, fall back to the
    // fork-parent pid so AT LEAST we don't NPE; the kill will still be
    // a no-op but that's the pre-T5.13 behaviour we're replacing). The
    // KillAgent handler then signals the WHOLE process group via
    // `kill(-pid)` so the daemon AND its claude child are torn down
    // together. Same pattern as `kill_agent` in standalone uses (see
    // `agents/mod.rs:supervisor_kill`).
    let pidfile_path = sockets_dir.join(format!("{agent_id}.pid"));
    let pid = match std::fs::read_to_string(&pidfile_path) {
        Ok(s) => s
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|p| *p != 0 && *p != fork_parent_pid)
            .unwrap_or(fork_parent_pid),
        Err(_) => fork_parent_pid,
    };
    log_info!("[runner] session daemon real pid={pid} (fork-parent was {fork_parent_pid})");

    // Connect and start I/O forwarding.
    let task_state = state.clone();
    let aid = agent_id.to_string();
    let sock = socket_path.clone();
    let prompt_bytes = prompt.as_bytes().to_vec();
    // Clone the temp-file paths into the task so we can `remove_file` them
    // after the agent's PTY closes — same lifecycle as
    // `pty_agent::on_agent_exit` does in standalone via `session_settings::delete_for_agent`.
    let mcp_cleanup = mcp_config_path.clone();
    let settings_cleanup = settings_path.clone();

    let io_task = tokio::spawn(async move {
        if let Err(e) = forward_agent_io(
            &sock,
            &task_state.ws_tx,
            &task_state.runner_id,
            &aid,
            Some(&prompt_bytes),
        )
        .await
        {
            log_warn!("[runner] agent {aid} I/O error: {e}");
        }

        // Best-effort cleanup of the two temp files written at spawn time.
        // `remove_file` is idempotent enough — anything other than
        // NotFound is logged and ignored, the agent is already gone.
        for p in [mcp_cleanup, settings_cleanup].into_iter().flatten() {
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log_warn!(
                    "[runner] agent {aid}: failed to remove {}: {e}",
                    p.display()
                );
            }
        }

        // Agent exited — report reliably so a WS flap during exit replays
        // the event from the outbox on reconnect (Task 11.7).
        let stopped = WireMessage::AgentStopped {
            agent_id: aid.clone(),
            status: "completed".into(),
            cost_usd: None,
            stop_reason: None,
        };
        send_reliable(&task_state, stopped).await;
    });

    state.agents.lock().await.insert(
        agent_id.to_string(),
        AgentHandle {
            pid,
            socket_path,
            io_task,
            cwd: cwd.to_path_buf(),
            mcp_config_path,
            settings_path,
        },
    );

    Ok(())
}

/// Connect to a session daemon and forward I/O between it and the WebSocket.
///
/// `prompt = Some(bytes)` means "fresh spawn — inject the prompt the first
/// time the daemon shows its readiness glyph (Claude `❯`)." `prompt = None`
/// means "reattach — the daemon was spawned by a previous runner process,
/// has already shown readiness and consumed its prompt; do not inject."
/// The reattach path is what `cleanup_and_reattach` uses on runner restart
/// (Task 11.6).
async fn forward_agent_io(
    socket_path: &Path,
    ws_tx: &mpsc::UnboundedSender<String>,
    runner_id: &str,
    agent_id: &str,
    prompt: Option<&[u8]>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = connect_to_socket(socket_path).await?;

    // Read output from the daemon and forward to SaaS.
    let mut readiness_buf = Vec::with_capacity(16 * 1024);
    // Reattach path skips readiness detection entirely — the daemon is
    // already past the prompt, so we'd inject mid-conversation.
    let mut prompt_injected = prompt.is_none();

    loop {
        match session_protocol::read_frame(&mut stream).await {
            Ok(session_protocol::Message::Output(data)) => {
                // Check for readiness (Claude prompt glyph ❯).
                if !prompt_injected {
                    readiness_buf.extend_from_slice(&data);
                    if readiness_buf.len() > 16 * 1024 {
                        readiness_buf.drain(..readiness_buf.len() - 8 * 1024);
                    }
                    let ready = is_ready(&readiness_buf);
                    if ready {
                        log_info!("[runner] agent {agent_id}: detected readiness");
                    }
                    if ready && let Some(prompt_bytes) = prompt {
                        log_info!(
                            "[runner] agent {agent_id}: injecting prompt ({} bytes)",
                            prompt_bytes.len()
                        );
                        // Mirror standalone (pty_agent::inject_prompt_when_ready):
                        // 500ms settle so claude finished rendering the prompt
                        // line; paste; 1s settle so the bracketed-paste sequence
                        // has fully landed; then `\r` (matches a real Enter
                        // keystroke through a PTY) so claude actually submits.
                        // Without the trailing `\r` SaaS agents hung forever
                        // with the prompt visible but unsent (cbd73b92 repro).
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let paste_msg = session_protocol::Message::Input(prompt_bytes.to_vec());
                        session_protocol::write_frame(&mut stream, &paste_msg).await?;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let enter_msg = session_protocol::Message::Input(b"\r".to_vec());
                        session_protocol::write_frame(&mut stream, &enter_msg).await?;
                        prompt_injected = true;
                        log_info!("[runner] agent {agent_id}: prompt injected successfully");
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

/// Scan `<cwd>/.branchwork-runner-sessions/` for live session-daemon
/// sockets and reattach to each. Mirrors
/// [`crate::agents::AgentRegistry::cleanup_and_reattach`] for the
/// runner-side. Sockets whose owning daemon refuses connection are
/// reaped: the socket file plus its sibling pidfile are unlinked and
/// the orphan counter is incremented (one log line per reap so a
/// pile-up of stale sockets surfaces in the dashboard).
///
/// Idempotent: agents already in `state.agents` are skipped, so calling
/// this on every WebSocket reconnect catches new survivors without
/// double-spawning I/O tasks for ones we already track.
async fn cleanup_and_reattach_runner(state: &Arc<RunnerState>) {
    let sessions_dir = state.cwd.join(".branchwork-runner-sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return, // no dir = no agents, common on first boot
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only look at .sock files; .pid / .log siblings are handled below.
        if path.extension().and_then(|s| s.to_str()) != Some("sock") {
            continue;
        }
        let agent_id = stem.to_string();

        // Already tracking this agent in-process — skip.
        if state.agents.lock().await.contains_key(&agent_id) {
            continue;
        }

        // Probe the socket. A live daemon accepts connections; a dead one
        // either has the socket file lingering on disk (Unix doesn't
        // unlink on process exit) or the kernel returns ECONNREFUSED.
        match connect_to_socket(&path).await {
            Ok(_) => {
                // Live — spawn a forwarder. Drop the probe stream; the
                // forwarder establishes its own connection (cheaper than
                // threading a borrowed handle through `tokio::spawn`).
                let task_state = state.clone();
                let aid = agent_id.clone();
                let sock = path.clone();
                let io_task = tokio::spawn(async move {
                    if let Err(e) = forward_agent_io(
                        &sock,
                        &task_state.ws_tx,
                        &task_state.runner_id,
                        &aid,
                        None,
                    )
                    .await
                    {
                        log_warn!("[runner] reattached agent {aid} I/O error: {e}");
                    }

                    // Daemon exited — report reliably so a WS flap during
                    // exit replays the event from the outbox on reconnect
                    // (Task 11.7).
                    let stopped = WireMessage::AgentStopped {
                        agent_id: aid.clone(),
                        status: "completed".into(),
                        cost_usd: None,
                        stop_reason: None,
                    };
                    send_reliable(&task_state, stopped).await;
                });

                // PID recovery from the daemon's pidfile (T5.13). Pre-
                // T5.13 we hard-coded `pid: 0`, which would have made
                // `KillAgent`'s `libc::kill(-pid, SIGTERM)` signal the
                // RUNNER's own process group ("pid 0" in `kill(2)`
                // means "caller's group"). The previous KillAgent code
                // happened to pass `+pid` so the bug was latent — flip
                // the sign in T5.13 and the runner would suicide on
                // every reattached-agent kill click. Read the pidfile
                // (`<socket>.pid`, written by the supervisor early in
                // `run_daemon`) so we have the real daemon pid; if it's
                // missing, store a fresh `0` sentinel that the
                // KillAgent handler explicitly guards against.
                //
                // Reattach can't recover the original temp-file paths from
                // the previous runner process — they were derived from
                // `agent_id` then, and that derivation is unchanged, so
                // re-resolve from the same scheme used in `spawn_agent`.
                let reattach_sockets_dir = state.cwd.join(".branchwork-runner-sessions");
                let reattach_pidfile = reattach_sockets_dir.join(format!("{agent_id}.pid"));
                let reattach_pid: u32 = std::fs::read_to_string(&reattach_pidfile)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .filter(|p| *p > 0)
                    .unwrap_or(0);
                let reattach_mcp = reattach_sockets_dir.join(format!("{agent_id}.mcp.json"));
                let reattach_settings =
                    reattach_sockets_dir.join(format!("{agent_id}.settings.json"));
                state.agents.lock().await.insert(
                    agent_id.clone(),
                    AgentHandle {
                        pid: reattach_pid,
                        socket_path: path.clone(),
                        io_task,
                        cwd: state.cwd.clone(),
                        mcp_config_path: reattach_mcp.exists().then_some(reattach_mcp),
                        settings_path: reattach_settings.exists().then_some(reattach_settings),
                    },
                );
                log_info!(
                    "[runner] reattached agent {agent_id} via {}",
                    path.display()
                );
            }
            Err(_) => {
                // Dead socket — reap. Best-effort cleanup of socket +
                // pidfile sibling. Increment the 24 h orphan counter so
                // RunnerHealth surfaces this on the dashboard.
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(path.with_extension("pid"));
                runner_health::record_orphan_reaped();
                log_warn!(
                    "[runner] reaped orphan session socket {} (no listener)",
                    path.display()
                );
            }
        }
    }
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

// ── Server-bin self-diagnostic (T1.2) ───────────────────────────────────────

/// Resolution returned by [`resolve_server_bin`]. Carries both the path the
/// runner will hand to `Command::spawn` (`spawn_target`) and a structured
/// diagnostic shipped on `RunnerHello`. They are reported together so the
/// runner can still attempt a doomed spawn (preserving the existing
/// `AgentSpawnFailed` path on the first session) — the chip just turns red
/// up front instead of staying neutral until that first failure.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerBinResolution {
    spawn_target: PathBuf,
    diagnostic: runner_protocol::ServerBinDiagnostic,
}

/// Resolve the `branchwork-server` binary the runner will use to spawn
/// session daemons. Returns both the spawn target (used by
/// `Command::spawn`) and a structured diagnostic surfaced on the
/// dashboard via `RunnerHello.server_bin`.
///
/// Resolution order:
///   1. `--server-bin <path>` (operator override).
///   2. `which("branchwork-server")` on `$PATH`.
///   3. Bare `branchwork-server` literal — likely-broken fallback the
///      pre-T1.2 code accepted silently. Surfaces as `NotFound { reason:
///      "not on $PATH" }` so the dashboard chip catches this case before
///      the first spawn attempt.
///
/// For case 1, missing files / non-files surface as `NotFound` with a
/// matching reason (the operator typed a bad path; the dashboard should
/// say so rather than waiting for the inevitable spawn `ENOENT`).
fn resolve_server_bin(explicit: Option<&Path>) -> ServerBinResolution {
    if let Some(path) = explicit {
        let display = path.display().to_string();
        // `metadata()` follows symlinks; `is_file()` would return false for
        // a symlink whose target doesn't exist, which gives the wrong
        // reason ("not a file" vs "does not exist"). Branch on the metadata
        // error explicitly for that finer-grained signal.
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                let canonical = std::fs::canonicalize(path)
                    .map(|p| p.display().to_string())
                    .unwrap_or(display);
                ServerBinResolution {
                    spawn_target: path.to_path_buf(),
                    diagnostic: runner_protocol::ServerBinDiagnostic::Found { path: canonical },
                }
            }
            Ok(_) => ServerBinResolution {
                spawn_target: path.to_path_buf(),
                diagnostic: runner_protocol::ServerBinDiagnostic::NotFound {
                    searched: display,
                    reason: "path is not a file".to_string(),
                },
            },
            Err(_) => ServerBinResolution {
                spawn_target: path.to_path_buf(),
                diagnostic: runner_protocol::ServerBinDiagnostic::NotFound {
                    searched: display,
                    reason: "path does not exist".to_string(),
                },
            },
        }
    } else if let Some(found) = which("branchwork-server") {
        let canonical = std::fs::canonicalize(&found)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| found.display().to_string());
        ServerBinResolution {
            spawn_target: found,
            diagnostic: runner_protocol::ServerBinDiagnostic::Found { path: canonical },
        }
    } else {
        // Pre-T1.2 behaviour: hand `Command::spawn` the bare literal and let
        // it fail later. Now the dashboard learns about this up front via
        // the structured diagnostic.
        ServerBinResolution {
            spawn_target: PathBuf::from("branchwork-server"),
            diagnostic: runner_protocol::ServerBinDiagnostic::NotFound {
                searched: "branchwork-server".to_string(),
                reason: "not on $PATH".to_string(),
            },
        }
    }
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
        ("codex", "codex", vec![]),
        ("gemini", "gemini", vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
        ("bob", "bob", vec!["ANTHROPIC_API_KEY"]),
    ] {
        let status = driver_auth_status(name, binary, &env_vars);
        drivers.push(DriverAuthInfo {
            name: name.to_string(),
            status,
        });
    }

    drivers
}

fn driver_auth_status(name: &str, binary: &str, env_vars: &[&str]) -> DriverAuthStatus {
    if which(binary).is_none() {
        return DriverAuthStatus::NotInstalled;
    }
    installed_driver_auth_status(name, env_vars)
}

fn installed_driver_auth_status(name: &str, env_vars: &[&str]) -> DriverAuthStatus {
    if name == "codex" {
        // Codex authenticates through its interactive OAuth flow when the
        // spawned agent starts. A runner with the binary installed is
        // therefore capable of starting Codex even when OPENAI_API_KEY is
        // intentionally unset.
        return DriverAuthStatus::Oauth { account: None };
    }

    let has_key = env_vars
        .iter()
        .any(|v| std::env::var(v).ok().is_some_and(|s| !s.trim().is_empty()));
    if has_key {
        DriverAuthStatus::ApiKey
    } else {
        DriverAuthStatus::Unknown
    }
}

/// Detect when the CLI is ready for input. Checks for the Claude prompt glyph.
fn is_ready(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    // Claude prompt glyph (❯), generic REPL prompt (\n> ), or Bob Shell prompt
    // Bob's prompt: "│" followed by ">  Enter your prompt" (note: two spaces after >)
    // ANSI codes appear between characters, so check for both parts separately
    s.contains('❯') || s.contains("\n> ") || (s.contains('│') && s.contains("Enter your prompt"))
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

/// Spawn the zombie reaper task. On Unix the runner spawns session
/// daemons with `setsid` + `pre_exec`, then never `wait()`s them — the
/// dashboard's expectation is that they outlive the runner. When one
/// exits before the runner notices, it lingers as `<defunct>` until
/// reaped. The reaper claims any pending exits every 5 s via
/// `waitpid(-1, WNOHANG)` and emits a single log line per reap (Task
/// 11.6 acceptance: at most one line per reaped zombie).
///
/// Windows has no zombie semantics in the same way (handles, not PIDs,
/// hold the exit code) so the reaper is a Unix-only no-op task there:
/// the function still returns a JoinHandle so the cleanup wiring at the
/// end of `connect_and_run` is uniform.
fn spawn_zombie_reaper() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            const TICK: Duration = Duration::from_secs(5);
            loop {
                tokio::time::sleep(TICK).await;
                loop {
                    let mut status: libc::c_int = 0;
                    // SAFETY: waitpid is async-signal-safe and the syscall
                    // returns immediately under WNOHANG. We pass a valid
                    // pointer for the status output and -1 for "any child."
                    let pid = unsafe { libc::waitpid(-1, &mut status as *mut _, libc::WNOHANG) };
                    if pid > 0 {
                        runner_health::record_orphan_reaped();
                        log_warn!("[runner] reaped zombie session daemon pid={pid}");
                        // Loop to drain any other ready zombies in the
                        // same tick — a burst (e.g. OOM-killer spree)
                        // gets one log line per reap rather than one
                        // per tick.
                        continue;
                    }
                    // 0 = no children ready; -1 = no children at all (ECHILD).
                    break;
                }
            }
        }
        #[cfg(not(unix))]
        {
            // Windows: zombies aren't a thing in the POSIX sense; the
            // runner-side handle is closed on exit. Sleep forever so the
            // JoinHandle stays valid.
            std::future::pending::<()>().await;
        }
    })
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

    /// Regression for the 2026-05-06 trust-dialog incident: the runner-side
    /// spawn path must inject `CLAUDE_CODE_SANDBOXED=1` into the session
    /// daemon's argv so the spawned `claude` skips the trust-workspace
    /// dialog. Mirrors `pty_agent::tests::pty_spawn_args_include_claude_sandboxed_env`.
    #[test]
    fn extra_env_for_driver_returns_sandbox_flag_for_claude() {
        let env = extra_env_for_driver("claude");
        assert!(
            env.iter()
                .any(|(k, v)| k == "CLAUDE_CODE_SANDBOXED" && v == "1"),
            "claude driver must declare CLAUDE_CODE_SANDBOXED=1: {env:?}",
        );
    }

    #[test]
    fn extra_env_for_driver_is_empty_for_other_drivers() {
        // Aider / Codex / Gemini have no portable sandbox flag — return empty.
        // Bob is tested separately since it conditionally passes ANTHROPIC_API_KEY.
        assert!(extra_env_for_driver("aider").is_empty());
        assert!(extra_env_for_driver("codex").is_empty());
        assert!(extra_env_for_driver("gemini").is_empty());
        assert!(extra_env_for_driver("unknown").is_empty());
    }

    #[test]
    fn codex_runner_auth_status_does_not_require_openai_api_key() {
        let status = installed_driver_auth_status("codex", &[]);
        assert!(
            matches!(status, DriverAuthStatus::Oauth { account: None }),
            "codex runner capability must be based on installed CLI/OAuth start flow, not OPENAI_API_KEY: {status:?}",
        );
    }

    #[test]
    fn extra_env_for_driver_passes_anthropic_key_to_bob() {
        // Bob needs ANTHROPIC_API_KEY from the runner's environment.
        // This test only verifies the plumbing — the actual key value
        // comes from the runner's env at runtime.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "test-key-123");
        }
        let env = extra_env_for_driver("bob");
        assert!(
            env.iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "test-key-123"),
            "bob driver must pass through ANTHROPIC_API_KEY: {env:?}",
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        // When ANTHROPIC_API_KEY is not set, bob should return empty.
        let env_empty = extra_env_for_driver("bob");
        assert!(
            env_empty.is_empty(),
            "bob without API key should return empty"
        );
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
        // Use the platform's temp dir (absolute on every OS) so the
        // assertion works on Windows too — `/tmp/...` looks rooted but is
        // NOT absolute on Windows (no drive prefix), and would otherwise
        // fall through to the bare-name branch.
        let absolute = std::env::temp_dir().join("branchwork-test");
        assert!(
            absolute.is_absolute(),
            "temp_dir() must yield an absolute path on this OS: {absolute:?}"
        );
        assert_eq!(
            resolve_runner_path(&absolute.display().to_string()),
            absolute
        );
    }

    #[test]
    fn resolve_runner_path_bare_name_resolves_under_home() {
        // 2026-05-07 fix: bare names land under `$HOME/<name>`, not under
        // the runner process's cwd (which used to be `/<name>` because
        // `mkdir -p` treats `PathBuf::from("myproj")` as relative to cwd).
        let home = dirs::home_dir().expect("test host should have a home dir");
        assert_eq!(resolve_runner_path("myproj"), home.join("myproj"));
        assert_eq!(
            resolve_runner_path("nested/subdir"),
            home.join("nested/subdir")
        );
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

    /// T4.2 cadence plan: a freshly-created project folder gets a
    /// `branchwork.toml` pinning `merge_cadence = "phase"`. The
    /// scaffold lives on the host that actually owns the working tree,
    /// so the runner branch of `CreateFolder` is the canonical place
    /// to verify this in SaaS mode.
    #[test]
    fn check_or_create_folder_scaffolds_branchwork_toml_on_fresh_folder() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-scaffold-{}", uuid::Uuid::new_v4()));
        let (ok, _, error) = check_or_create_folder(&tmp.display().to_string(), true);
        assert!(ok);
        assert!(error.is_none());
        let toml_path = tmp.join("branchwork.toml");
        assert!(
            toml_path.exists(),
            "branchwork.toml must be scaffolded into a freshly-created project folder"
        );
        let body = std::fs::read_to_string(&toml_path).expect("read scaffolded toml");
        assert!(
            body.contains("merge_cadence = \"phase\""),
            "scaffolded body must pin merge_cadence to phase: {body}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Pre-existing project folders keep their on-disk `branchwork.toml`
    /// (or its absence) untouched per the acceptance criterion
    /// "Pre-existing projects keep their explicit (or grandfathered)
    /// setting." We seed the folder with a `merge_cadence = "task"`
    /// override and verify the runner does NOT overwrite it.
    #[test]
    fn check_or_create_folder_does_not_overwrite_existing_branchwork_toml() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-preserve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("seed");
        let toml_path = tmp.join("branchwork.toml");
        let user_body = "[auto_mode]\nmerge_cadence = \"task\"\n";
        std::fs::write(&toml_path, user_body).expect("seed branchwork.toml");
        let (ok, _, error) = check_or_create_folder(&tmp.display().to_string(), true);
        assert!(ok);
        assert!(error.is_none());
        let body = std::fs::read_to_string(&toml_path).expect("read");
        assert_eq!(
            body, user_body,
            "pre-existing branchwork.toml must be preserved verbatim"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Pre-existing project folders without a `branchwork.toml` stay
    /// untouched — the runner only scaffolds when it actually created
    /// the directory. The folder is considered "the user's" and we
    /// don't drop a file into it unannounced.
    #[test]
    fn check_or_create_folder_does_not_scaffold_pre_existing_empty_folder() {
        let tmp =
            std::env::temp_dir().join(format!("branchwork-cof-untouched-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("seed dir");
        let (ok, _, error) = check_or_create_folder(&tmp.display().to_string(), true);
        assert!(ok);
        assert!(error.is_none());
        let toml_path = tmp.join("branchwork.toml");
        assert!(
            !toml_path.exists(),
            "scaffold must not fire on a pre-existing project folder"
        );
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
            plan_cwd: Arc::new(Mutex::new(HashMap::new())),
            ci_cache: Arc::new(Mutex::new(CiAggregateCache::default())),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
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

    // ── Phase 0.4 (auto-mode) handler tests ─────────────────────────────
    //
    // These cover the four high-level wire variants:
    //   - MergeAgentBranch — looks up agent cwd from state.agents,
    //     resolves task branch from HEAD, runs the merge sequence,
    //     pushes when target == default.
    //   - HasGithubActions — globs cwd/.github/workflows/*.yml.
    //   - GetCiRunStatus — Reglyze-shaped aggregation. The pure
    //     `aggregate_runs` is tested directly with stub data so the
    //     test passes on machines without `gh` installed.
    //   - CiFailureLog — re-resolves failing run id from the in-memory
    //     cache when run_id=None.

    /// Insert an `AgentHandle` into `state.agents` without running a real
    /// session daemon — needed because `MergeAgentBranch` /
    /// `HasGithubActions` look up cwd via the agents map. The handle is
    /// inert (pid=0, abortable but already-completed io_task) so dropping
    /// it on test teardown is harmless.
    async fn seed_test_agent(state: &Arc<RunnerState>, agent_id: &str, cwd: &Path) {
        let io_task = tokio::spawn(async {});
        state.agents.lock().await.insert(
            agent_id.to_string(),
            AgentHandle {
                pid: 0,
                socket_path: PathBuf::new(),
                io_task,
                cwd: cwd.to_path_buf(),
                mcp_config_path: None,
                settings_path: None,
            },
        );
    }

    #[tokio::test]
    async fn merge_agent_branch_happy_path_replies_ok_and_targets_default() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        git(dir.path(), &["checkout", "-b", "feature/x"]);
        std::fs::write(dir.path().join("foo.txt"), "hi").unwrap();
        git(dir.path(), &["add", "foo.txt"]);
        git(dir.path(), &["commit", "-m", "add foo"]);
        // Stay on feature/x — auto-mode policy leaves the agent on its
        // task branch on exit, so HEAD is the source of truth.

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-1", &state.cwd.clone()).await;

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeAgentBranch {
                req_id: "req-merge-1".into(),
                agent_id: "agent-1".into(),
                into: None,
            },
        )
        .await;
        match reply {
            WireMessage::AgentBranchMerged {
                req_id,
                ok,
                merged_sha,
                target_branch,
                had_conflict,
                error,
            } => {
                assert_eq!(req_id, "req-merge-1");
                assert!(ok, "expected ok=true, error={error:?}");
                assert_eq!(target_branch, "master");
                assert!(!had_conflict);
                assert_eq!(merged_sha.unwrap_or_default().len(), 40);
                assert!(error.is_none());
            }
            other => panic!("expected AgentBranchMerged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_agent_branch_unknown_agent_replies_error_sentinel() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeAgentBranch {
                req_id: "req-merge-2".into(),
                agent_id: "ghost".into(),
                into: None,
            },
        )
        .await;
        match reply {
            WireMessage::AgentBranchMerged {
                ok,
                error,
                merged_sha,
                ..
            } => {
                assert!(!ok);
                assert!(merged_sha.is_none());
                assert_eq!(error.as_deref(), Some("agent_not_found_on_runner"));
            }
            other => panic!("expected AgentBranchMerged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_agent_branch_empty_branch_reports_empty() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        // Branch from master at the same SHA — zero commits ahead.
        git(dir.path(), &["checkout", "-b", "feature/empty"]);

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-2", &state.cwd.clone()).await;

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeAgentBranch {
                req_id: "req-merge-3".into(),
                agent_id: "agent-2".into(),
                into: None,
            },
        )
        .await;
        match reply {
            WireMessage::AgentBranchMerged {
                ok,
                had_conflict,
                error,
                ..
            } => {
                assert!(!ok);
                assert!(!had_conflict);
                assert_eq!(error.as_deref(), Some("empty_branch"));
            }
            other => panic!("expected AgentBranchMerged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_agent_branch_conflict_reports_had_conflict() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        std::fs::write(dir.path().join("c.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "base"]);

        git(dir.path(), &["checkout", "-b", "feature/conflict"]);
        std::fs::write(dir.path().join("c.txt"), "branch\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "branch"]);

        // Diverge master.
        git(dir.path(), &["checkout", "master"]);
        std::fs::write(dir.path().join("c.txt"), "master\n").unwrap();
        git(dir.path(), &["add", "c.txt"]);
        git(dir.path(), &["commit", "-m", "master"]);

        // Back to feature so HEAD is the task branch when MergeAgentBranch fires.
        git(dir.path(), &["checkout", "feature/conflict"]);

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-3", &state.cwd.clone()).await;

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::MergeAgentBranch {
                req_id: "req-merge-4".into(),
                agent_id: "agent-3".into(),
                into: None,
            },
        )
        .await;
        match reply {
            WireMessage::AgentBranchMerged {
                had_conflict,
                ok,
                merged_sha,
                ..
            } => {
                assert!(had_conflict, "expected conflict reply");
                assert!(!ok);
                assert!(merged_sha.is_none());
            }
            other => panic!("expected AgentBranchMerged, got {other:?}"),
        }
        // No leftover MERGE_HEAD — runner aborted cleanly.
        assert!(!dir.path().join(".git/MERGE_HEAD").exists());
    }

    #[tokio::test]
    async fn has_github_actions_present_when_workflow_yml_exists() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("ci.yml"), "name: ci\n").unwrap();

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-gh-1", &state.cwd.clone()).await;

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::HasGithubActions {
                req_id: "req-gh-1".into(),
                agent_id: "agent-gh-1".into(),
            },
        )
        .await;
        match reply {
            WireMessage::GithubActionsDetected { req_id, present } => {
                assert_eq!(req_id, "req-gh-1");
                assert!(present);
            }
            other => panic!("expected GithubActionsDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn has_github_actions_absent_when_no_workflow_dir() {
        let dir = TempDir::new().unwrap();
        git_init_with_commit(dir.path(), "master");

        let (state, mut rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-gh-2", &state.cwd.clone()).await;

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::HasGithubActions {
                req_id: "req-gh-2".into(),
                agent_id: "agent-gh-2".into(),
            },
        )
        .await;
        match reply {
            WireMessage::GithubActionsDetected { present, .. } => {
                assert!(!present);
            }
            other => panic!("expected GithubActionsDetected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn has_github_actions_unknown_agent_replies_present_false() {
        // No agent in registry → no cwd → defensively reply false.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::HasGithubActions {
                req_id: "req-gh-3".into(),
                agent_id: "ghost".into(),
            },
        )
        .await;
        match reply {
            WireMessage::GithubActionsDetected { present, .. } => assert!(!present),
            other => panic!("expected GithubActionsDetected, got {other:?}"),
        }
    }

    /// Build the Reglyze fixture: three runs for one SHA where `tests` failed,
    /// `lint` passed, and `deploy` was skipped because tests failed.
    fn reglyze_fixture() -> Vec<GhRunDetail> {
        vec![
            GhRunDetail {
                database_id: 100,
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 101,
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                created_at: Some("2026-04-12T10:00:01Z".into()),
            },
            GhRunDetail {
                database_id: 102,
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                created_at: Some("2026-04-12T10:00:02Z".into()),
            },
        ]
    }

    #[test]
    fn aggregate_runs_reglyze_fixture_marks_skipped_due_to_upstream() {
        // Pass `None` for the explicit allowlist so the runner falls
        // through to the level-4 classifier — `tests` and `lint` block,
        // `deploy` is informational. The Reglyze rule still poisons the
        // aggregate because the failing run (`tests`) is in the blocking
        // subset, and `deploy.skipped_due_to_upstream` is set by
        // `mark_upstream_skips` regardless of the partition decision.
        let aggregate = aggregate_runs(reglyze_fixture(), None);
        assert_eq!(aggregate.status, "completed");
        assert_eq!(aggregate.conclusion.as_deref(), Some("failure"));
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("100"));

        let by_workflow: HashMap<&str, &CiRunSummary> = aggregate
            .runs
            .iter()
            .map(|s| (s.workflow_name.as_str(), s))
            .collect();
        assert!(!by_workflow["tests"].skipped_due_to_upstream);
        assert!(!by_workflow["lint"].skipped_due_to_upstream);
        assert!(
            by_workflow["deploy"].skipped_due_to_upstream,
            "deploy must be marked skipped_due_to_upstream when tests failed in same SHA"
        );
        // Classifier puts `deploy` in the informational subset.
        assert!(
            by_workflow["deploy"].informational,
            "deploy must be flagged informational by the level-4 classifier"
        );
        assert!(!by_workflow["tests"].informational);
        assert!(!by_workflow["lint"].informational);
    }

    #[test]
    fn aggregate_runs_all_success_reports_success_and_no_failing_run() {
        let runs = vec![
            GhRunDetail {
                database_id: 200,
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 201,
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("skipped".into()),
                created_at: Some("2026-04-12T10:00:01Z".into()),
            },
        ];
        let aggregate = aggregate_runs(runs, None);
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
        assert!(aggregate.failing_run_id.is_none());
        // deploy.skipped_due_to_upstream must be false — no failure in set.
        assert!(!aggregate.runs.iter().any(|r| r.skipped_due_to_upstream));
    }

    /// Pre-1.2 invariant: a *blocking* in-progress run holds the gate.
    /// The classifier puts `tests` in the blocking subset, so its
    /// in_progress status must propagate to `aggregate.status`.
    #[test]
    fn aggregate_runs_in_progress_when_blocking_run_pending() {
        let runs = vec![
            GhRunDetail {
                database_id: 300,
                workflow_name: "tests".into(),
                status: "in_progress".into(),
                conclusion: None,
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 301,
                workflow_name: "deploy".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                created_at: Some("2026-04-12T10:00:01Z".into()),
            },
        ];
        let aggregate = aggregate_runs(runs, None);
        assert_eq!(aggregate.status, "in_progress");
        assert!(aggregate.conclusion.is_none());
    }

    /// Phase-1.2 partition: an in-progress *informational* run does NOT
    /// hold the gate. The classifier puts `deploy` in the informational
    /// subset, so a still-running deploy alongside a green `tests` must
    /// resolve to `completed`/`success`.
    #[test]
    fn aggregate_runs_informational_in_progress_does_not_hold_gate() {
        let runs = vec![
            GhRunDetail {
                database_id: 300,
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 301,
                workflow_name: "deploy".into(),
                status: "in_progress".into(),
                conclusion: None,
                created_at: Some("2026-04-12T10:00:01Z".into()),
            },
        ];
        let aggregate = aggregate_runs(runs, None);
        assert_eq!(
            aggregate.status, "completed",
            "informational deploy must not block the gate"
        );
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
        let by_workflow: HashMap<&str, &CiRunSummary> = aggregate
            .runs
            .iter()
            .map(|s| (s.workflow_name.as_str(), s))
            .collect();
        assert!(by_workflow["deploy"].informational);
        assert!(!by_workflow["tests"].informational);
    }

    #[test]
    fn aggregate_runs_failing_run_id_picks_earliest_by_created_at() {
        // Two failing runs, second created_at chronologically earlier.
        // Both names ("later", "earlier") are blocking by the classifier.
        let runs = vec![
            GhRunDetail {
                database_id: 500,
                workflow_name: "later".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                created_at: Some("2026-04-12T11:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 501,
                workflow_name: "earlier".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
        ];
        let aggregate = aggregate_runs(runs, None);
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("501"));
    }

    /// Explicit-allowlist path: the wire field carried `Some(["tests"])`
    /// from the server. `lint` becomes informational because it's not in
    /// the allowlist (this is precedence level 1-3, NOT the classifier),
    /// so a green-tests + red-lint run set reports success.
    #[test]
    fn aggregate_runs_explicit_allowlist_demotes_lint_to_informational() {
        let runs = vec![
            GhRunDetail {
                database_id: 700,
                workflow_name: "tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
                created_at: Some("2026-04-12T10:00:00Z".into()),
            },
            GhRunDetail {
                database_id: 701,
                workflow_name: "lint".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                created_at: Some("2026-04-12T10:00:01Z".into()),
            },
        ];
        let allowlist = vec!["tests".to_string()];
        let aggregate = aggregate_runs(runs, Some(&allowlist));
        assert_eq!(aggregate.status, "completed");
        assert_eq!(aggregate.conclusion.as_deref(), Some("success"));
        assert!(aggregate.failing_run_id.is_none());
        let by_workflow: HashMap<&str, &CiRunSummary> = aggregate
            .runs
            .iter()
            .map(|s| (s.workflow_name.as_str(), s))
            .collect();
        assert!(!by_workflow["tests"].informational);
        assert!(by_workflow["lint"].informational);
    }

    /// Acceptance criterion for task 1.2: runner-side and standalone-side
    /// aggregates must match for the same SHA + same allowlist + same
    /// input runs. Tests both the explicit-allowlist path and the
    /// classifier-fallback path.
    #[test]
    fn aggregate_runs_matches_dispatch_aggregate_for_same_input_and_filter() {
        // Reuse the cross-mode parity check via a shared helper expressed
        // entirely in `CiRunSummary` so we don't take a `gh` shell-out
        // dependency. The runner's `aggregate_runs` is a superset of the
        // server's `aggregate_with_resolution`: it sorts + lifts
        // `GhRunDetail` to `CiRunSummary` first, then runs the same
        // filter+compute pipeline.
        let detail_runs = reglyze_fixture();

        // Explicit allowlist case.
        let allowlist = vec!["tests".to_string(), "lint".to_string()];
        let runner_agg = aggregate_runs(detail_runs.clone(), Some(&allowlist));
        let summaries: Vec<CiRunSummary> = detail_runs
            .iter()
            .map(|r| CiRunSummary {
                run_id: r.database_id.to_string(),
                workflow_name: r.workflow_name.clone(),
                status: if r.status.is_empty() {
                    "completed".to_string()
                } else {
                    r.status.clone()
                },
                conclusion: r.conclusion.clone(),
                skipped_due_to_upstream: false,
                informational: false,
            })
            .collect();
        let mut server_summaries = summaries.clone();
        ci::aggregate::mark_upstream_skips(&mut server_summaries);
        // Server-side filter via the same pipeline (mark_upstream_skips +
        // compute_with_filter on the explicit allowlist).
        let filter = ci::aggregate::BlockingFilter::config(&allowlist);
        let server_agg = ci::aggregate::compute_with_filter(&server_summaries, Some(&filter));
        assert_eq!(
            runner_agg, server_agg,
            "runner aggregate must match server aggregate byte-for-byte for same allowlist"
        );

        // Classifier fallback case (None on the wire).
        let runner_agg_default = aggregate_runs(detail_runs, None);
        // Server side simulates the same fallback: classifier over the
        // discovered workflow names.
        let classifier_allowlist: Vec<String> = server_summaries
            .iter()
            .map(|s| s.workflow_name.clone())
            .filter(|name| ci::aggregate::is_workflow_blocking_by_default(name))
            .collect();
        let classifier_filter = ci::aggregate::BlockingFilter::classifier(&classifier_allowlist);
        let server_agg_default =
            ci::aggregate::compute_with_filter(&server_summaries, Some(&classifier_filter));
        assert_eq!(
            runner_agg_default, server_agg_default,
            "runner classifier-fallback aggregate must match server's"
        );
    }

    #[tokio::test]
    async fn get_ci_run_status_caches_and_records_plan_sha() {
        // Drive the handler with a SHA the cache has been pre-seeded for —
        // skipping the real `gh` shell-out entirely. This proves the cache
        // hit path returns the seeded aggregate AND that the plan→sha
        // side-table is updated.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        // Pre-seed the cache so the handler doesn't shell out.
        let aggregate = aggregate_runs(reglyze_fixture(), None);
        {
            let mut cache = state.ci_cache.lock().await;
            cache.put("merged-sha-1".to_string(), Some(aggregate.clone()));
        }

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GetCiRunStatus {
                req_id: "req-ci-1".into(),
                plan_name: "demo-plan".into(),
                task_number: "1.2".into(),
                merged_sha: "merged-sha-1".into(),
                blocking_workflows: None,
            },
        )
        .await;
        match reply {
            WireMessage::CiRunStatusResolved {
                req_id,
                aggregate: Some(agg),
            } => {
                assert_eq!(req_id, "req-ci-1");
                assert_eq!(agg.conclusion.as_deref(), Some("failure"));
                assert_eq!(agg.failing_run_id.as_deref(), Some("100"));
            }
            other => panic!("expected CiRunStatusResolved, got {other:?}"),
        }

        // plan→sha was recorded so CiFailureLog{run_id:None} can re-resolve.
        let cache = state.ci_cache.lock().await;
        assert_eq!(
            cache.latest_sha_for_plan("demo-plan").as_deref(),
            Some("merged-sha-1")
        );
    }

    #[tokio::test]
    async fn ci_failure_log_with_no_known_aggregate_replies_log_none() {
        // No cached aggregate for any plan → nothing to re-resolve →
        // log:None, run_id_used:None. This is the "loop polled before
        // a CI run completed" branch.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::CiFailureLog {
                req_id: "req-cl-1".into(),
                plan_name: "demo-plan".into(),
                run_id: None,
            },
        )
        .await;
        match reply {
            WireMessage::CiFailureLogResolved {
                req_id,
                log,
                run_id_used,
            } => {
                assert_eq!(req_id, "req-cl-1");
                assert!(log.is_none());
                assert!(run_id_used.is_none());
            }
            other => panic!("expected CiFailureLogResolved, got {other:?}"),
        }
    }

    /// **Reglyze regression test (acceptance criterion).**
    ///
    /// Walks the full auto-mode loop path: pre-seed the cache with the
    /// three-runs Reglyze fixture, fire `GetCiRunStatus` to confirm the
    /// aggregator output, then fire `CiFailureLog { run_id: None }` and
    /// assert that the re-resolved run is `tests` (id=100), NOT the
    /// skipped `deploy` (id=102).
    ///
    /// The bug this guards against: the loop, on Red, calls
    /// `CiFailureLog { run_id: None }` and expects the runner to give it
    /// the failing run's log. If the aggregator wrongly treats
    /// `deploy: skipped` as success and picks deploy's id, the loop
    /// gets a "no failures" empty string back instead of the test
    /// failure log — and falsely reports the run as healthy.
    #[tokio::test]
    async fn reglyze_regression_failure_log_resolves_to_tests_run_not_skipped_deploy() {
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        // Pre-seed the cache with a fully-aggregated entry for the SHA.
        // Filter `None` ⇒ the runner runs the level-4 classifier; `tests`
        // is blocking so the failure still poisons the aggregate.
        let aggregate = aggregate_runs(reglyze_fixture(), None);
        // Sanity-check the aggregator before relying on it for the regression.
        assert_eq!(aggregate.failing_run_id.as_deref(), Some("100"));
        let deploy = aggregate
            .runs
            .iter()
            .find(|r| r.workflow_name == "deploy")
            .expect("deploy summary present");
        assert!(
            deploy.skipped_due_to_upstream,
            "deploy must be marked skipped_due_to_upstream"
        );

        {
            let mut cache = state.ci_cache.lock().await;
            cache.put("reglyze-sha".to_string(), Some(aggregate.clone()));
            cache.record_plan_sha("reglyze-plan".to_string(), "reglyze-sha".to_string());
        }

        // Step 1: GetCiRunStatus returns the seeded aggregate verbatim.
        let status_reply = dispatch(
            &state,
            &mut rx,
            WireMessage::GetCiRunStatus {
                req_id: "req-rgz-1".into(),
                plan_name: "reglyze-plan".into(),
                task_number: "0.4".into(),
                merged_sha: "reglyze-sha".into(),
                blocking_workflows: None,
            },
        )
        .await;
        match status_reply {
            WireMessage::CiRunStatusResolved {
                aggregate: Some(agg),
                ..
            } => {
                assert_eq!(agg.conclusion.as_deref(), Some("failure"));
                assert_eq!(agg.failing_run_id.as_deref(), Some("100"));
                let deploy = agg
                    .runs
                    .iter()
                    .find(|r| r.workflow_name == "deploy")
                    .expect("deploy in reply");
                assert!(deploy.skipped_due_to_upstream);
            }
            other => panic!("expected CiRunStatusResolved, got {other:?}"),
        }

        // Step 2: CiFailureLog { run_id: None } re-resolves to tests run.
        // The actual `gh run view --log-failed` shell-out returns None on
        // any test machine (no real run with that id), but `run_id_used`
        // must reflect that we picked tests (100), NOT deploy (102).
        let log_reply = dispatch(
            &state,
            &mut rx,
            WireMessage::CiFailureLog {
                req_id: "req-rgz-2".into(),
                plan_name: "reglyze-plan".into(),
                run_id: None,
            },
        )
        .await;
        match log_reply {
            WireMessage::CiFailureLogResolved {
                req_id,
                run_id_used,
                ..
            } => {
                assert_eq!(req_id, "req-rgz-2");
                assert_eq!(
                    run_id_used.as_deref(),
                    Some("100"),
                    "must re-resolve to the tests run, not the skipped deploy run"
                );
            }
            other => panic!("expected CiFailureLogResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ci_failure_log_with_explicit_run_id_uses_it_directly() {
        // No cache, but explicit run_id is provided — runner must echo
        // it back in `run_id_used` even when the gh shell-out fails.
        let dir = TempDir::new().unwrap();
        let (state, mut rx) = make_test_state(dir.path().to_path_buf());

        let reply = dispatch(
            &state,
            &mut rx,
            WireMessage::CiFailureLog {
                req_id: "req-cl-2".into(),
                plan_name: "demo".into(),
                run_id: Some("999".into()),
            },
        )
        .await;
        match reply {
            WireMessage::CiFailureLogResolved { run_id_used, .. } => {
                assert_eq!(run_id_used.as_deref(), Some("999"));
            }
            other => panic!("expected CiFailureLogResolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shutdown_request_flips_flag_and_drains_agents() {
        // Acceptance pin (server-side): the runner-side handler MUST flip
        // the shutdown flag and drain its in-flight agents, so the SaaS
        // side can treat ShutdownRequest as the cooperative shutdown
        // primitive rather than just a notification.
        let dir = TempDir::new().unwrap();
        let (state, _rx) = make_test_state(dir.path().to_path_buf());
        seed_test_agent(&state, "agent-shutdown", &state.cwd.clone()).await;
        seed_test_agent(&state, "agent-shutdown-2", &state.cwd.clone()).await;
        assert_eq!(state.agents.lock().await.len(), 2);

        let env = Envelope::reliable(
            "server".into(),
            1,
            WireMessage::ShutdownRequest {
                reason: Some("dashboard click".into()),
            },
        );
        handle_server_message(&state, &env).await;

        assert!(state.shutdown_requested.load(AtomicOrdering::Relaxed));
        // Drained — the daemons we seeded had pid=0 so SIGTERM was a no-op
        // syscall, but the agents map MUST be empty so future StartAgent
        // requests don't collide on agent_id reuse during the drain
        // window.
        assert_eq!(state.agents.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn shutdown_request_without_reason_is_accepted() {
        // The dashboard may omit `reason`; the handler must not panic.
        let dir = TempDir::new().unwrap();
        let (state, _rx) = make_test_state(dir.path().to_path_buf());

        let env = Envelope::reliable(
            "server".into(),
            2,
            WireMessage::ShutdownRequest { reason: None },
        );
        handle_server_message(&state, &env).await;

        assert!(state.shutdown_requested.load(AtomicOrdering::Relaxed));
    }

    // ── cleanup_and_reattach_runner (Task 11.6) ────────────────────────────

    #[tokio::test]
    async fn reattach_reaps_socket_with_no_listener() {
        // A bare file in `.branchwork-runner-sessions/` named `<id>.sock`
        // is not a real socket: connect() returns ECONNREFUSED. Reattach
        // must unlink it (plus the .pid sibling) and bump the orphan
        // counter so RunnerHealth surfaces a count > 0 next tick.
        let dir = TempDir::new().unwrap();
        let (state, _rx) = make_test_state(dir.path().to_path_buf());
        let sessions_dir = state.cwd.join(".branchwork-runner-sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let sock = sessions_dir.join("ghost.sock");
        let pid = sessions_dir.join("ghost.pid");
        std::fs::write(&sock, b"").unwrap();
        std::fs::write(&pid, b"99999\n").unwrap();
        let baseline = runner_health::orphans_reaped_24h();

        cleanup_and_reattach_runner(&state).await;

        assert!(!sock.exists(), "stale socket must be unlinked");
        assert!(!pid.exists(), "stale pidfile must be unlinked");
        assert_eq!(
            runner_health::orphans_reaped_24h(),
            baseline + 1,
            "orphan counter must be bumped",
        );
        assert!(
            state.agents.lock().await.is_empty(),
            "no listener = no reattach",
        );
    }

    #[tokio::test]
    async fn reattach_skips_already_tracked_agents() {
        // Idempotency: cleanup_and_reattach_runner runs on every WS
        // reconnect; agents the runner already tracks must NOT be
        // double-reattached (would spawn a second forwarder).
        let dir = TempDir::new().unwrap();
        let (state, _rx) = make_test_state(dir.path().to_path_buf());
        let sessions_dir = state.cwd.join(".branchwork-runner-sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let sock = sessions_dir.join("known.sock");
        // Stale socket file on disk so we can prove the loop visited it.
        std::fs::write(&sock, b"").unwrap();
        // Pre-populate state.agents with a matching entry — like a
        // freshly-spawned agent that's still alive in this process.
        let dummy_task = tokio::spawn(async {});
        state.agents.lock().await.insert(
            "known".to_string(),
            AgentHandle {
                pid: 0,
                socket_path: sock.clone(),
                io_task: dummy_task,
                cwd: state.cwd.clone(),
                mcp_config_path: None,
                settings_path: None,
            },
        );
        let baseline = runner_health::orphans_reaped_24h();

        cleanup_and_reattach_runner(&state).await;

        // Known agent stays — and crucially the socket file was NOT
        // reaped (the reattach loop saw it in state.agents and skipped
        // the connect probe entirely).
        assert!(state.agents.lock().await.contains_key("known"));
        assert!(
            sock.exists(),
            "tracked agent's socket must not be reaped on reattach",
        );
        assert_eq!(
            runner_health::orphans_reaped_24h(),
            baseline,
            "no orphan must be counted for a tracked agent",
        );
    }

    #[tokio::test]
    async fn reattach_no_sessions_dir_is_a_noop() {
        // Fresh runner: the sessions dir doesn't exist yet. Reattach
        // must early-return cleanly — no crash, no spurious counter
        // bump.
        let dir = TempDir::new().unwrap();
        let (state, _rx) = make_test_state(dir.path().to_path_buf());
        // Confirm there's no sessions dir.
        assert!(!state.cwd.join(".branchwork-runner-sessions").exists());

        let baseline = runner_health::orphans_reaped_24h();
        cleanup_and_reattach_runner(&state).await;
        assert_eq!(runner_health::orphans_reaped_24h(), baseline);
        assert!(state.agents.lock().await.is_empty());
    }

    // ── Normal-exit AgentStopped reliability (Task 11.7) ───────────────────

    #[tokio::test]
    async fn normal_exit_agent_stopped_is_outboxed_for_replay() {
        // Pre-fix this path used `Envelope::best_effort` + a direct
        // `ws_tx.send`, so a WS drop between the agent's PTY exit and
        // the SaaS ACK would lose the event and rot the `agents` row to
        // `running` forever. Post-fix, the io_task closure inside
        // `spawn_agent` / `cleanup_and_reattach_runner` calls
        // `send_reliable`, which enqueues the frame in the runner outbox
        // before pushing it on the WS channel. This test mirrors that
        // exact call shape and proves the row is recoverable on
        // reconnect via `replay_runner_events`.
        let dir = TempDir::new().unwrap();
        let (state, rx) = make_test_state(dir.path().to_path_buf());

        // Drop the receiver before the send — this is the strict
        // analogue of "WS down between PTY exit and ACK." The unbounded
        // mpsc still buffers the message internally, but the writer
        // task on the other end of a real connection would have failed
        // to push it onto the wire. The outbox is what makes the event
        // survive, not the mpsc buffer.
        drop(rx);

        let stopped = WireMessage::AgentStopped {
            agent_id: "agent-x".into(),
            status: "completed".into(),
            cost_usd: None,
            stop_reason: None,
        };
        send_reliable(&state, stopped).await;

        // On reconnect the runner sends `Resume { last_seen_seq }` and
        // the server replies with its own `last_seen_seq`; the runner
        // then loops over `replay_runner_events` to re-send any unacked
        // frames. So `replay_runner_events(&conn, 0)` is exactly what
        // a fresh server (or one that had never seen this seq) would
        // get back as the replay payload.
        let conn = state.db.lock().await;
        let pending = outbox::replay_runner_events(&conn, 0);
        assert_eq!(
            pending.len(),
            1,
            "AgentStopped must be in the runner outbox after a WS drop",
        );
        let (seq, event_type, payload) = &pending[0];
        assert!(*seq > 0, "reliable frames must carry a non-zero seq");
        assert_eq!(event_type, "agent_stopped");
        // Payload is the serialized WireMessage; spot-check that the
        // agent_id and status fields round-tripped so the SaaS handler
        // sees the right row to flip to `completed`.
        let parsed: serde_json::Value =
            serde_json::from_str(payload).expect("outbox payload is JSON");
        assert_eq!(parsed["type"], "agent_stopped");
        assert_eq!(parsed["agent_id"], "agent-x");
        assert_eq!(parsed["status"], "completed");
    }

    #[tokio::test]
    async fn outbox_replay_after_reconnect_yields_normal_exit_event() {
        // End-to-end-ish: enqueue AgentStopped while the WS is "down"
        // (rx dropped), then simulate reconnect by reading the outbox
        // exactly the way the post-Resume replay loop does. The frame
        // we get back must serialise into a fresh reliable Envelope
        // with seq preserved — that's what the SaaS server keys ACKs
        // off and what advances `seq_tracker` on the receiving side.
        let dir = TempDir::new().unwrap();
        let (state, rx) = make_test_state(dir.path().to_path_buf());
        drop(rx);

        send_reliable(
            &state,
            WireMessage::AgentStopped {
                agent_id: "agent-y".into(),
                status: "completed".into(),
                cost_usd: Some(0.42),
                stop_reason: None,
            },
        )
        .await;

        // Fresh receiver — the runner wires this on every reconnect.
        // Replay path enqueues via `Envelope::reliable(runner_id, seq,
        // msg)` on the same channel.
        let pending = {
            let conn = state.db.lock().await;
            outbox::replay_runner_events(&conn, 0)
        };
        assert_eq!(pending.len(), 1);
        let (seq, _event_type, payload) = pending.into_iter().next().unwrap();
        let msg: WireMessage = serde_json::from_str(&payload).expect("outbox payload deserialises");
        let env = Envelope::reliable(state.runner_id.clone(), seq, msg);
        let wire = serde_json::to_string(&env).expect("envelope serialises");

        // The replayed envelope carries the same seq (so the SaaS
        // peer can ACK it) and round-trips back to AgentStopped.
        let back: Envelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.seq, Some(seq));
        match back.message {
            WireMessage::AgentStopped {
                agent_id,
                status,
                cost_usd,
                stop_reason,
            } => {
                assert_eq!(agent_id, "agent-y");
                assert_eq!(status, "completed");
                assert_eq!(cost_usd, Some(0.42));
                assert!(stop_reason.is_none());
            }
            other => panic!("expected AgentStopped, got {other:?}"),
        }
    }

    /// Regression for the 2026-05-09 SaaS hang (cbd73b92): when
    /// `forward_agent_io` injects the prompt after readiness it MUST also
    /// send a trailing `\r` Input frame, otherwise the bracketed-paste
    /// sequence sits in the TUI forever and Claude never submits. Mirrors
    /// `pty_agent::inject_prompt_when_ready` in standalone — single source
    /// of truth for this behaviour, see ADR 0007.
    ///
    /// Unix-only: the runner's `connect_to_socket` (and this test's
    /// listener bind) feed a filesystem path straight into
    /// `to_fs_name::<GenericFilePath>`, which on Windows rejects anything
    /// that isn't already a named-pipe path (`\\.\pipe\…`). Mapping the
    /// path the way `agents::supervisor::socket_name` does is the right
    /// long-term fix, but the runner has several other Windows-IPC gaps
    /// (`socket_path.exists()` polling, the `.sock`-suffix scan in
    /// `cleanup_and_reattach_runner`) so a one-off pipe-name patch here
    /// would only paper over the first failure. SaaS deploys to Linux,
    /// which is the regression surface this test guards. Same convention
    /// as the Unix-gated PTY tests in `agents::supervisor::tests`.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn forward_agent_io_sends_prompt_then_carriage_return() {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName};

        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("inject.sock");

        // Server side: bind the listener BEFORE spawning the client so the
        // connect can't race the bind. The supervisor stand-in feeds one
        // readiness frame, then drains Input frames the runner sends back.
        let listener_name = socket_path
            .clone()
            .to_fs_name::<GenericFilePath>()
            .unwrap()
            .into_owned();
        let listener = ListenerOptions::new()
            .name(listener_name)
            .create_tokio()
            .expect("bind listener");

        let (got_tx, mut got_rx) = mpsc::unbounded_channel::<session_protocol::Message>();
        let server_task = tokio::spawn(async move {
            let stream = listener.accept().await.expect("accept");
            let (mut read_half, mut write_half) = stream.split();

            // Trigger the runner's `is_ready` check (the `❯` glyph).
            session_protocol::write_frame(
                &mut write_half,
                &session_protocol::Message::Output(b"\n\xe2\x9d\xaf ".to_vec()),
            )
            .await
            .expect("write readiness frame");

            // Drain exactly the two Input frames the contract promises
            // (paste, then `\r`). Bounded — once both arrive, drop the
            // write half so the runner's read loop sees EOF and
            // `forward_agent_io` returns. Without this, both sides park
            // on `read_frame` forever (cargo's 60s soft-timeout flagged
            // this when a `while let Ok(...)` drain was used here).
            for _ in 0..2 {
                let frame = session_protocol::read_frame(&mut read_half)
                    .await
                    .expect("runner sends Input frame");
                got_tx.send(frame).expect("test rx is alive for 2 frames");
            }
            drop(write_half);
        });

        // Client side: invoke the runner's I/O forwarder with a prompt.
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<String>();
        let prompt = b"hello-from-test";
        let client_task = tokio::spawn({
            let socket_path = socket_path.clone();
            async move {
                forward_agent_io(&socket_path, &ws_tx, "runner-test", "agent-1", Some(prompt))
                    .await
                    .expect("forward_agent_io");
            }
        });

        // Collect the two expected Input frames (paste, then `\r`).
        let first = tokio::time::timeout(Duration::from_secs(5), got_rx.recv())
            .await
            .expect("first frame within 5s")
            .expect("first frame is Some");
        let second = tokio::time::timeout(Duration::from_secs(5), got_rx.recv())
            .await
            .expect("second frame within 5s")
            .expect("second frame is Some");

        match first {
            session_protocol::Message::Input(bytes) => {
                assert_eq!(
                    bytes, prompt,
                    "first Input frame must be the verbatim paste",
                );
            }
            other => panic!("expected Input(prompt), got {other:?}"),
        }
        match second {
            session_protocol::Message::Input(bytes) => {
                assert_eq!(
                    bytes, b"\r",
                    "second Input frame must be `\\r` so claude submits the prompt",
                );
            }
            other => panic!("expected Input(b\"\\r\"), got {other:?}"),
        }

        // Tear down: dropping the client's stream (when forward_agent_io
        // returns after we close the server's write half) ends the server
        // task. Awaiting both keeps the test deterministic.
        drop(got_rx);
        let _ = server_task.await;
        let _ = client_task.await;
    }

    /// Task 1.1 (runner-install-and-spawn-reliability) regression:
    /// `classify_spawn_io_error` maps `io::Error` onto `(errno, tag)`
    /// pairs the runner ships in `AgentSpawnFailed`. The dashboard reads
    /// the tag directly so a future rename here is a wire-visible change
    /// — pin every kind we currently care about so accidental drift trips.
    #[test]
    fn classify_spawn_io_error_maps_not_found_to_enoent() {
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        let (errno, tag) = classify_spawn_io_error(&io);
        assert_eq!(tag, "ENOENT");
        // ErrorKind::NotFound carries no `raw_os_error` by itself — only
        // io::Errors built via `from_raw_os_error` do. The classifier
        // must return whatever `raw_os_error()` reports verbatim.
        assert_eq!(errno, None);
    }

    #[test]
    fn classify_spawn_io_error_maps_permission_denied_to_eacces() {
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let (_, tag) = classify_spawn_io_error(&io);
        assert_eq!(tag, "EACCES");
    }

    #[test]
    fn classify_spawn_io_error_preserves_raw_os_error_with_enoent() {
        // ENOENT from a real exec() call carries libc::ENOENT (2 on Linux,
        // but constant elsewhere). The classifier must surface the code
        // verbatim so an operator running `strace`/`ltrace` can correlate.
        let io = std::io::Error::from_raw_os_error(2);
        let (errno, tag) = classify_spawn_io_error(&io);
        assert_eq!(errno, Some(2));
        // ErrorKind for raw_os_error(2) is NotFound on Unix (matched on
        // libc::ENOENT). On Windows the same code maps differently — the
        // tag falls back to "OS_ERROR(2)" there, which the classifier
        // produces via the `_ => match raw {...}` arm.
        assert!(
            tag == "ENOENT" || tag == "OS_ERROR(2)",
            "unexpected tag for raw os error 2: {tag}",
        );
    }

    #[test]
    fn classify_spawn_io_error_falls_back_to_io_error_when_unknown() {
        // ErrorKind::Other with no raw OS error — typical for io::Errors
        // built from a plain string. Tag must still be non-empty so the
        // dashboard always has something to render.
        let io = std::io::Error::other("synthetic");
        let (errno, tag) = classify_spawn_io_error(&io);
        assert_eq!(errno, None);
        assert_eq!(tag, "IO_ERROR");
    }

    // ── resolve_server_bin (T1.2) ───────────────────────────────────────

    /// `--server-bin` pointing at a real executable file resolves to
    /// `Found` carrying the canonical absolute path.
    #[test]
    fn resolve_server_bin_explicit_existing_file_is_found() {
        // tempfile + Unix executable bit — `is_file()` only checks file-
        // ness, not the executable bit. The diagnostic does NOT validate
        // executability (the OS will surface a clearer EACCES later); we
        // just need a file that exists.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("branchwork-server");
        std::fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();

        let res = resolve_server_bin(Some(&target));
        match res.diagnostic {
            runner_protocol::ServerBinDiagnostic::Found { path } => {
                // Canonical form may differ from `target.display()` (symlinks
                // resolved, trailing components normalized); the path must
                // at minimum point at the file the runner will spawn.
                assert!(
                    path.ends_with("branchwork-server"),
                    "expected canonical path to end with branchwork-server, got {path}"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
        assert_eq!(res.spawn_target, target);
    }

    /// `--server-bin` pointing at a missing path surfaces `NotFound` with
    /// the literal user-supplied string and a `path does not exist` reason.
    /// Distinguishing this from `not on $PATH` lets the operator see at a
    /// glance that they typed a bad path vs forgot to install the binary.
    #[test]
    fn resolve_server_bin_explicit_missing_file_is_not_found() {
        let res = resolve_server_bin(Some(Path::new("/does/not/exist/branchwork-server")));
        match res.diagnostic {
            runner_protocol::ServerBinDiagnostic::NotFound { searched, reason } => {
                assert_eq!(searched, "/does/not/exist/branchwork-server");
                assert_eq!(reason, "path does not exist");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert_eq!(
            res.spawn_target,
            PathBuf::from("/does/not/exist/branchwork-server")
        );
    }

    /// `--server-bin` pointing at a directory (file system entry exists but
    /// isn't a file) lands on `NotFound { reason: "path is not a file" }`.
    /// Operators sometimes hand the runner the build dir by mistake; the
    /// reason should distinguish from a typo.
    #[test]
    fn resolve_server_bin_explicit_directory_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let res = resolve_server_bin(Some(dir.path()));
        match res.diagnostic {
            runner_protocol::ServerBinDiagnostic::NotFound { reason, .. } => {
                assert_eq!(reason, "path is not a file");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// No `--server-bin` flag and no `branchwork-server` on `$PATH`
    /// surfaces `NotFound { searched: "branchwork-server", reason: "not on $PATH" }`.
    /// This is the headline acceptance criterion: installing the runner
    /// without `branchwork-server` on `$PATH` must show the dashboard
    /// "not found" state BEFORE the user clicks Start session.
    #[test]
    fn resolve_server_bin_falls_back_to_not_found_when_path_empty() {
        // Snapshot the host PATH, blank it for the test, restore on exit.
        // `cargo test` is parallel by default; mutating env is unsafe but
        // we're inside a single test fn so the window is small. Use the
        // unsafe-set guard pattern (Edition 2024) and bail on poisoning.
        struct PathGuard(Option<String>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(prev) => std::env::set_var("PATH", prev),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _guard = PathGuard(std::env::var("PATH").ok());
        unsafe {
            std::env::set_var("PATH", "/nonexistent-dir-for-t1-2-tests");
        }

        let res = resolve_server_bin(None);
        match res.diagnostic {
            runner_protocol::ServerBinDiagnostic::NotFound { searched, reason } => {
                assert_eq!(searched, "branchwork-server");
                assert_eq!(reason, "not on $PATH");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        // Fallback spawn_target is the bare literal — preserves the pre-T1.2
        // behaviour (let `Command::spawn` fail with ENOENT on the first
        // session) so the structured diagnostic surfaces independently
        // without changing the spawn-side error path.
        assert_eq!(res.spawn_target, PathBuf::from("branchwork-server"));
    }
}
