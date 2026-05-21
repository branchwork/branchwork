//! Wire protocol for runner <-> SaaS communication.
//!
//! Every message is JSON-serialized and wrapped in a [`WireMessage`] tagged
//! union. Reliable (outbox-backed) messages carry a monotonically increasing
//! `seq` per sender; best-effort messages (terminal I/O) carry `seq: null`.
//!
//! This module is **self-contained** — no `crate::` dependencies — so it can
//! be `#[path]`-included by the standalone `branchwork_runner` binary.

#![allow(dead_code)] // Both binaries include this module but each uses a different subset.

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
    /// First message after connect. Carries runner metadata, driver
    /// capabilities (so the dashboard can render the Drivers panel), and
    /// — added in T5.14 — the in-memory agent inventory the runner is
    /// still tracking. Pre-T5.14 every prod redeploy / WS reconnect
    /// caused `cleanup_and_reattach` to mark all `mode='remote'` rows as
    /// `killed`/`supervisor_died` ("runner ownership lost"); the runner
    /// then reconnected, picked up its state from the on-disk session
    /// dir, and kept forwarding output — but the row stayed dead and
    /// the dashboard showed the agent as gone forever. Reporting active
    /// ids on Hello lets the server unkill them: if a row is in
    /// `killed`/`supervisor_died` and the runner says "still alive,"
    /// flip it back to `running` and broadcast.
    ///
    /// `#[serde(default)]` keeps older runners (which don't send the
    /// field) wire-compatible — they degrade to the pre-T5.14 behaviour.
    RunnerHello {
        hostname: String,
        version: String,
        drivers: Vec<DriverAuthInfo>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        active_agents: Vec<String>,
        /// T1.2: Self-diagnostic for the `branchwork-server` spawn target.
        /// Set at runner startup based on the result of `--server-bin`
        /// override or `which("branchwork-server")` on `$PATH`. Persisted
        /// on the server and surfaced on the dashboard runner row so the
        /// operator sees a green check + path or red cross + reason BEFORE
        /// clicking Start session — closes the silent gap where a missing
        /// `branchwork-server` only surfaces via `AgentSpawnFailed` after
        /// the first spawn attempt.
        ///
        /// `#[serde(default)]` keeps older runners (which don't send the
        /// field) wire-compatible — they degrade to a neutral chip and
        /// the operator still has to wait for a spawn failure to learn
        /// the bad state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_bin: Option<ServerBinDiagnostic>,
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

    /// `Command::spawn` returned `Err` before the session daemon could
    /// fork. Distinct from `AgentStopped { status: "failed" }` because the
    /// failure is structured and recoverable — the operator can read the
    /// errno and fix the cause (missing binary on PATH, permission denied,
    /// etc.) instead of guessing at a free-form `stop_reason` string. The
    /// server stores the message on the agent row and the dashboard renders
    /// it inline on the agent card so the "Start session" click doesn't go
    /// silent.
    ///
    /// Reliable: the runner pushes this through the outbox so a flapping
    /// WS during a fast spawn-fail still lands on the dashboard.
    AgentSpawnFailed {
        agent_id: String,
        /// The executable path the runner attempted to spawn (i.e.
        /// `state.server_bin.display().to_string()`). Echoed verbatim in
        /// the dashboard error message so the operator sees exactly which
        /// path failed — the runner's `--server-bin` flag isn't visible
        /// from the dashboard otherwise.
        command: String,
        /// Raw OS error code, if `io::Error::raw_os_error()` returned one.
        /// `None` for non-OS errors (e.g. UTF-8 problems building the
        /// command). Frontends should treat the absence as "unknown errno".
        #[serde(skip_serializing_if = "Option::is_none")]
        errno: Option<i32>,
        /// Short human-readable errno tag derived from the OS error
        /// (e.g. "ENOENT", "EACCES"). Falls back to a generic "OS_ERROR"
        /// label when the errno is set but doesn't map to a known tag, or
        /// "IO_ERROR" when no raw OS error is available. Always set so
        /// the dashboard has something to render after the path.
        errno_str: String,
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
    DriverAuthReport { drivers: Vec<DriverAuthInfo> },

    /// One line of the runner process's own stdout/stderr — the diagnostics
    /// a user-on-the-host normally reads via `tail -f` or `journalctl`.
    /// Streamed to the dashboard's `/runners/{id}` tail panel via the
    /// `runner_log` broadcast.
    ///
    /// Best-effort. The runner caps emission at ~200 lines/sec (drop-newest
    /// when the in-flight bound is hit) and prepends a synthetic
    /// `"[truncated N lines]"` line on the next surviving emit so the
    /// dashboard can render the gap. A reconnect mid-burst loses some
    /// lines outright; the dashboard renders a `[reconnect — N lines may
    /// have been missed]` marker on the next render once the WS lifts back
    /// up. No outbox, no replay.
    RunnerLogLine {
        /// RFC3339 / ISO 8601 timestamp, captured at emit time on the
        /// runner. Authoritative — the server forwards verbatim so a
        /// late-arriving line still shows the moment it was printed.
        ts: String,
        /// One of `info` / `warn` / `error`. `println!` maps to `info`
        /// and `eprintln!` maps to `warn`; the dashboard tints the
        /// `<pre>` line accordingly. Free-form so future log levels can
        /// flow through without a wire change.
        level: String,
        /// The line itself, with the trailing newline already stripped
        /// (the runner writes one frame per line). UTF-8; non-UTF-8
        /// bytes are lossy-replaced before emit so the wire stays JSON.
        line: String,
    },

    /// Periodic per-runner health snapshot — outbox depth, 24-hour reconnect
    /// count, CI-poll latency percentiles. Emitted on a ~30 s tick so the
    /// dashboard's `Health` panel on `/runners/{id}` can show live numbers
    /// without a server-side scrape job.
    ///
    /// Best-effort: same family as `Ping`/`Pong`. The "latest snapshot" is
    /// what matters; missed ticks during a flap are replaced by the next
    /// tick after reconnect, so there's no replay window worth paying the
    /// outbox for. Version mismatch detection is **not** in this payload —
    /// the server already knows the runner's version from `RunnerHello` and
    /// compares it against `env!("CARGO_PKG_VERSION")` when serializing
    /// `/api/runners` rows.
    RunnerHealth {
        /// Number of unacked rows in the runner's `runner_outbox` at emit
        /// time. > 100 ⇒ saturation (server is slow to ACK or the WS is
        /// flapping); the dashboard chip turns red past that threshold.
        outbox_depth: u32,
        /// Reconnect events in the trailing 24 h. > 5 ⇒ link instability;
        /// the dashboard chip turns red past that threshold. The runner
        /// trims the in-memory ring on every emit so this value is always
        /// fresh.
        ws_reconnects_24h: u32,
        /// Median wall-clock (in ms) for `compute_ci_aggregate` over the
        /// trailing window — covers the `gh run list` plus per-run
        /// `gh run view` chain. `None` when no CI poll has happened on
        /// this runner yet (fresh enrolments and plans without GitHub
        /// Actions).
        #[serde(skip_serializing_if = "Option::is_none")]
        ci_poll_ms_p50: Option<u32>,
        /// 99th percentile wall-clock (in ms) for `compute_ci_aggregate`
        /// over the trailing window. `None` under the same conditions
        /// as `ci_poll_ms_p50`.
        #[serde(skip_serializing_if = "Option::is_none")]
        ci_poll_ms_p99: Option<u32>,
        /// Number of orphaned session daemons reaped in the trailing 24 h
        /// (Task 11.6 named wart). Bumped from two paths on the runner:
        /// `cleanup_and_reattach_runner` finding a socket with no
        /// listener, and the `waitpid` reaper claiming a detached child
        /// that exited between heartbeats. The dashboard chip surfaces
        /// the value > 0 so the user knows the silent-rot failure mode
        /// happened. Defaults to 0 on older runners that don't ship the
        /// field.
        #[serde(default)]
        orphans_reaped_24h: u32,
    },

    // ── SaaS -> Runner ──────────────────────────────────────────────────
    /// Dashboard user clicked "Start" — spawn an agent.
    ///
    /// `effort` and `skip_permissions` ship the per-runner-resolved values:
    /// the SaaS server has already merged any `runner_config` override over
    /// the server-wide default, and the runner does not re-resolve. Old
    /// runner builds without `skip_permissions` deserialise it as `None`
    /// and fall back to "do not pass `--dangerously-skip-permissions`".
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
        #[serde(skip_serializing_if = "Option::is_none")]
        skip_permissions: Option<bool>,
        /// Server-generated session UUID. Runner passes this verbatim as
        /// Claude's `--session-id` so the dashboard's `agents.session_id`
        /// row matches what the CLI uses (Stop hook routing depends on it).
        /// Older runners that ignore the field generate their own id and
        /// the row stays consistent with what gets logged at the supervisor
        /// level — degraded but functional.
        #[serde(default)]
        session_id: String,
        /// Pre-rendered `.mcp.json` file body (Claude's `mcpServers` map).
        /// Runner writes it to a temp file and points Claude at it via
        /// `--mcp-config`. Empty string means the driver has no MCP
        /// integration — runner skips writing and skips the flag.
        #[serde(default)]
        mcp_config: String,
        /// Pre-rendered per-session settings JSON (Stop hook today, future
        /// keys later). Runner writes it to a temp file and points Claude
        /// at it via `--settings`. Empty string means the driver returned
        /// `None` from `stop_hook_config` — runner skips writing and skips
        /// the flag.
        #[serde(default)]
        settings_json: String,
    },

    /// Kill a running agent.
    KillAgent { agent_id: String },

    /// Operator clicked "Request shutdown" on the dashboard. The runner is
    /// expected to stop accepting new work, gracefully drain its session
    /// daemons (matching the supervisor's existing graceful-exit sequence),
    /// and exit cleanly.
    ///
    /// **Reliable**: outbox-backed and replayed if the runner disconnects
    /// before consuming it. The operator's intent should survive a flap —
    /// a runner that reconnects after a transient network blip should
    /// still honor the shutdown.
    ///
    /// The runner is free to ignore this if it is wedged or refuses; the
    /// server has no way to force-kill (no inbound path). If the runner
    /// ignores, the dashboard's secondary affordance is to revoke the
    /// token via `DELETE /api/runners/{id}`.
    ShutdownRequest {
        /// Free-form human-readable reason emitted by the operator. Echoed
        /// in the runner's structured log line so the host operator (the
        /// only one with kill -9) can correlate.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Operator clicked "Upgrade runner" on the dashboard. The runner is
    /// expected to fetch the SaaS server's installer script and run it in
    /// `--just-binary` mode, which:
    ///   - downloads matching `branchwork-runner` and `branchwork-server`
    ///     binaries into `$HOME/.local/bin/`,
    ///   - skips token / config rewrites (the existing config stays as-is),
    ///   - restarts the systemd-managed unit so the new binary picks up.
    ///
    /// The dashboard only surfaces this button when the version-drift
    /// verdict is amber or red (see `VersionMismatch::Patch|Minor|Major`)
    /// so a healthy runner stays untouched.
    ///
    /// **Reliable**: outbox-backed and replayed if the runner disconnects
    /// before consuming it. A flap mid-click should not silently drop the
    /// operator's intent.
    ///
    /// `install_url` defaults to `<saas_url>/install-runner.sh` (the
    /// server-templated script). It is overridable so a future operator
    /// can pin a specific commit / hand-rolled installer; the dashboard
    /// today never sets the field.
    UpgradeRunner {
        /// Free-form operator-supplied reason; echoed in the runner's
        /// structured log line so the host operator can correlate the
        /// restart with the dashboard click.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Override the install-script URL the runner fetches. `None` ⇒
        /// the runner derives it as `<saas_url>/install-runner.sh` from
        /// its own `--saas-url` flag — same value the dashboard rendered
        /// the script from at enrollment time.
        #[serde(skip_serializing_if = "Option::is_none")]
        install_url: Option<String>,
    },

    /// Resize the agent's PTY.
    ResizeTerminal {
        agent_id: String,
        cols: u16,
        rows: u16,
    },

    /// Forward keyboard input to the agent's PTY.
    /// Best-effort — no ACK. `data` is base64-encoded.
    AgentInput { agent_id: String, data: String },

    /// Request terminal replay from a byte offset (reconnecting browser).
    TerminalReplay { agent_id: String, from_offset: u64 },

    /// Dashboard requested the runner's folder listing (home dir, one level).
    /// Best-effort: tied to a live HTTP caller, so outbox replay is useless.
    ListFolders { req_id: String },

    /// Runner reply with the folder entries.
    FoldersListed {
        req_id: String,
        entries: Vec<FolderEntry>,
    },

    /// Dashboard requested folder creation/check on the runner.
    /// When `create_if_missing` is `false` the runner performs an existence
    /// check only and replies with `ok: false, error: "folder_not_found"` if
    /// the path is not a directory. When `true` it does `mkdir -p`.
    /// Best-effort: tied to a live HTTP caller.
    CreateFolder {
        req_id: String,
        path: String,
        #[serde(default)]
        create_if_missing: bool,
    },

    /// Runner reply with the create result.
    FolderCreated {
        req_id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolved_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Dashboard requested the canonical default branch for a runner-side cwd.
    /// Best-effort: tied to a live HTTP caller, so outbox replay is useless.
    GetDefaultBranch { req_id: String, cwd: String },

    /// Runner reply with the resolved default branch (`None` when no candidate
    /// resolves: no `origin/HEAD` symref and neither `master` nor `main`
    /// exists locally).
    DefaultBranchResolved {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },

    /// Dashboard requested the local branch list for a runner-side cwd.
    /// Best-effort: tied to a live HTTP caller.
    ListBranches { req_id: String, cwd: String },

    /// Runner reply with the local branch list, alphabetically sorted.
    BranchesListed {
        req_id: String,
        branches: Vec<String>,
    },

    /// Dashboard asked the runner to merge `task_branch` into `target` in
    /// `cwd`. The runner runs the same five-step sequence that
    /// `merge_agent_branch` performs locally in api/agents.rs:392-528:
    ///
    ///   1. `git rev-list --count <target>..<task_branch>` (empty-branch
    ///      guard — replies with `MergeOutcome::EmptyBranch`).
    ///   2. `git checkout <target>` (failure → `CheckoutFailed`).
    ///   3. `git merge <task_branch> --no-edit` (conflict →
    ///      `Conflict`; runner already ran `git merge --abort`).
    ///   4. `git branch -d <task_branch>` (best-effort cleanup on success).
    ///   5. `git rev-parse HEAD` to capture `merged_sha` for `Ok`.
    ///
    /// Best-effort: tied to a live HTTP caller, so outbox replay is useless.
    MergeBranch {
        req_id: String,
        cwd: String,
        target: String,
        task_branch: String,
    },

    /// Runner reply with the merge outcome. The outcome is an exhaustively
    /// matchable enum so the server can map each arm to its existing HTTP
    /// response code without parsing free-form strings.
    MergeResult {
        req_id: String,
        outcome: MergeOutcome,
    },

    /// Server asked the runner to spawn a stream-json check agent. Mirrors
    /// the standalone path in `agents/check_agent.rs::start_check_agent`
    /// but routes the `Command::new("claude") -p --output-format
    /// stream-json` invocation to the runner host where `claude` and the
    /// project filesystem actually live. Fire-and-forget: there is no
    /// request/response RPC pair — the runner streams `CheckAgentOutput`
    /// per stdout line back, then emits `AgentStopped` on exit. Server-
    /// side cleanup (UPDATE agents.status, on_check_agent_completed)
    /// piggybacks on the existing `AgentStopped` handler.
    StartCheckAgent {
        agent_id: String,
        session_id: String,
        prompt: String,
        cwd: String,
        effort: String,
    },

    /// Runner forward of one stdout line from a check agent's claude
    /// process. The line is a complete stream-json record (newline-
    /// delimited); the server appends to `agent_output` with
    /// `message_type` derived from the JSON `type` (`stdout` for
    /// regular events, `result` for terminal records). Best-effort.
    CheckAgentOutput { agent_id: String, line: String },

    /// Dashboard asked the runner to discard an agent's task branch. Before
    /// this pair landed `discard_agent_branch` returned 503
    /// `discard_not_supported_for_saas_runners` whenever an org had a
    /// runner; with the pair in place the server hands cwd+target+
    /// task_branch to the runner, which runs the same two-step sequence
    /// `discard_agent_branch` performs locally:
    ///
    ///   1. `git checkout <target>` (failure → `BranchDiscarded { ok=false }`
    ///      with stderr in `error`).
    ///   2. `git branch -D <task_branch>` (failure → same).
    ///
    /// The server-side wrapper still owns the DB write (clear
    /// `agents.branch` for all matching rows), audit entry, and the
    /// `agent_branch_discarded` broadcast — only the git side-effects
    /// move to the runner. Best-effort: tied to a live HTTP caller.
    DiscardBranch {
        req_id: String,
        cwd: String,
        target: String,
        task_branch: String,
    },

    /// Runner reply with the discard outcome. `ok=false` ⇒ `error`
    /// carries the captured stderr (or a synthesized message for
    /// process-spawn failures); the server reflects it back to the
    /// dashboard as a 500. `ok=true` is the only path that triggers the
    /// post-discard server-side bookkeeping.
    BranchDiscarded {
        req_id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Dashboard asked the runner to `git push origin <branch>` in `cwd`
    /// after a successful merge. Split out from `MergeBranch` because the
    /// push is gated by `should_record_ci_run` (`ci.rs`), a pure function
    /// over the default branch + merge target that lives on the server.
    /// Splitting keeps the policy decision on the SaaS side and the side
    /// effect (and `gh` dependency) on the runner side.
    ///
    /// Best-effort: tied to a live HTTP caller, so outbox replay is useless.
    PushBranch {
        req_id: String,
        cwd: String,
        branch: String,
    },

    /// Runner reply with the push result. `ok=false` ⇒ `stderr` carries the
    /// captured error so the server can log it; the dashboard does not
    /// surface push failures to the user (CI will retry on the next merge).
    PushResult {
        req_id: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr: Option<String>,
    },

    /// Server-side CI poller asked the runner for the most recent workflow
    /// run against `sha` in `cwd`. The runner shells out to
    /// `gh run list --commit <sha> -L 1 --json databaseId,status,conclusion,url`
    /// and replies with a [`GhRunListed`]. The poller fires from a
    /// background tokio task, not an HTTP caller — but treating this as
    /// best-effort is still correct: the next poll cycle (~30s later) will
    /// retry, so outbox replay buys nothing.
    GhRunList {
        req_id: String,
        cwd: String,
        sha: String,
    },
    /// Runner reply with the resolved run, or `None` when no workflow has
    /// fired yet for that commit (or when `gh` is unavailable).
    GhRunListed {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run: Option<GhRun>,
    },

    /// Server-side failure-log endpoint asked the runner for the
    /// `--log-failed` output of a finished run. The runner shells out to
    /// `gh run view <run_id> --log-failed`, tail-trims to ~8 KB (mirroring
    /// `ci.rs::fetch_failure_log`), and replies with a [`GhFailureLogFetched`].
    /// Best-effort: tied to a live HTTP caller waiting on the failure log,
    /// so outbox replay would land after the caller has timed out.
    GhFailureLog {
        req_id: String,
        cwd: String,
        run_id: String,
    },
    /// Runner reply with the failure-log tail, or `None` when the run has
    /// no failure log (still pending, gh unavailable, no auth, etc).
    GhFailureLogFetched {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        log: Option<String>,
    },

    // ── Auto-mode round-trips (saas → runner request, runner → saas reply) ──
    //
    // The four pairs below back the auto-mode loop's merge / CI gate / fix
    // sequence (auto-mode plan tasks 0.3 / 0.4 / 0.5). They mirror the
    // folder-ops dispatch pattern: live HTTP / loop caller waits on the
    // reply with a short timeout, so all eight variants are best-effort —
    // outbox replay would land after the caller has given up.
    //
    // Naming note: the auto-mode plan brief described the merge pair as
    // `MergeBranch` / `BranchMerged`, but `MergeBranch` is already the
    // low-level git primitive (cwd / target / task_branch → MergeOutcome)
    // wired by the merge-target plan and used by api/agents.rs's HTTP
    // merge button. The high-level agent-aware variant lives under
    // [`MergeAgentBranch`] / [`AgentBranchMerged`] to keep both pairs
    // available — the auto-mode dispatcher in 0.5 + the runner-side
    // handler in 0.4 use the new high-level pair, and the merge button
    // path is unchanged.
    /// Auto-mode loop (or HTTP merge button via the 0.5 dispatch shim) asked
    /// the runner to merge `agent_id`'s task branch. The runner already
    /// knows `agent_id`'s `cwd` because it spawned the agent, so the wire
    /// only carries the agent id and an optional dropdown override; the
    /// runner runs the same target-resolution + 5-step git sequence as
    /// `merge_agent_branch_inner` (api/agents.rs).
    ///
    /// Best-effort: tied to a live HTTP / loop caller, so outbox replay
    /// would land after the caller has given up.
    MergeAgentBranch {
        req_id: String,
        agent_id: String,
        /// Dropdown override for the merge target. `None` (and empty
        /// strings filtered to `None` server-side) selects the canonical
        /// default branch the runner resolves at merge time.
        #[serde(skip_serializing_if = "Option::is_none")]
        into: Option<String>,
    },
    /// Runner reply with a flat merge outcome shaped to match the
    /// server-side `api::agents::MergeOutcome` struct minus `task_branch`
    /// (which the auto-mode loop doesn't need). `had_conflict` and `error`
    /// are mutually informative — `had_conflict=true` means the runner ran
    /// `git merge --abort`, and the loop pauses with `merge_conflict`;
    /// otherwise `error` carries a human-readable reason for the loop's
    /// `merge_failed: <msg>` pause path.
    AgentBranchMerged {
        req_id: String,
        /// `true` ≡ `merged_sha` is set; `false` ≡ either `had_conflict`
        /// or `error` is set.
        ok: bool,
        /// New HEAD on `target_branch` after the merge commit. `None` for
        /// any failure path.
        #[serde(skip_serializing_if = "Option::is_none")]
        merged_sha: Option<String>,
        /// Branch the merge targeted (resolved canonical default or the
        /// validated `into` override). Empty string for early-return
        /// failures before target resolution (e.g. agent not found on
        /// the runner, agent has no task branch).
        target_branch: String,
        /// `git merge` reported a conflict; the runner already ran
        /// `git merge --abort` so the working tree is clean.
        had_conflict: bool,
        /// Human-readable error message for non-conflict failures.
        /// Sentinel values the dispatcher recognizes:
        /// - `"agent_not_found_on_runner"` — runner can't resolve `agent_id`
        /// - `"empty_branch"` — task branch has no commits ahead of target
        /// - other strings flow through as `merge_failed: <msg>`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    /// Auto-mode loop asked the runner whether the agent's project has any
    /// GitHub Actions workflows. Drives the CI gate decision in
    /// `ci::trigger_after_merge` — without workflows there's no CI to wait
    /// on and the loop advances straight to the next task. The runner
    /// resolves the agent's `cwd` itself; only `agent_id` is on the wire.
    ///
    /// Best-effort: tied to the loop's wait-and-poll cadence.
    HasGithubActions { req_id: String, agent_id: String },
    /// Runner reply: `true` iff `cwd/.github/workflows/*.{yml,yaml}` has
    /// at least one matching file.
    GithubActionsDetected { req_id: String, present: bool },

    /// Auto-mode loop asked the runner for the aggregate CI status across
    /// every workflow run for `merged_sha`. The runner runs `gh run list`
    /// plus per-skipped-run `gh run view` to inspect job-level skip
    /// reasons (Reglyze bug: a downstream `deploy` workflow `skipped`
    /// because `tests` failed upstream is **not** a success), then
    /// applies the aggregation rule in 0.3 to produce a [`CiAggregate`].
    ///
    /// `blocking_workflows` carries the level-1-3 (phase / plan / repo
    /// `branchwork.toml`) explicit allowlist of workflow names that must
    /// gate merge advancement. The runner has no plan YAML access, so
    /// the server resolves these levels and ships the result here:
    ///
    /// - `Some(list)` ⇒ apply the list verbatim via
    ///   [`crate::ci::aggregate::compute_with_filter`] with
    ///   `BlockingSource::Config`. Workflows outside the list are
    ///   informational; their failures do not poison the aggregate.
    /// - `None` ⇒ no explicit allowlist; runner runs the level-4 smart
    ///   classifier (`(?i)docker|deploy|publish|release|bench|fuzz`)
    ///   over the discovered workflow names with `BlockingSource::Classifier`.
    ///
    /// Older runners that don't know the field will still deserialize
    /// thanks to `#[serde(default)]`; they fall through to the
    /// pre-1.2 unfiltered path. After Phase 1.2 ships everywhere, the
    /// filter is uniformly applied on both sides.
    ///
    /// Best-effort: the loop polls on a ~30 s cadence and a missed reply
    /// just retries on the next tick.
    GetCiRunStatus {
        req_id: String,
        plan_name: String,
        task_number: String,
        merged_sha: String,
        /// Server-resolved level-1-3 explicit allowlist; see variant doc
        /// for the apply rule. `None` ≡ field absent on the wire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocking_workflows: Option<Vec<String>>,
    },
    /// Runner reply with the resolved aggregate, or `None` when no
    /// workflow run exists yet for the SHA (still polling) or `gh` is
    /// unavailable on the runner.
    CiRunStatusResolved {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        aggregate: Option<CiAggregate>,
    },

    /// Auto-mode loop asked the runner for the failure log of a CI run.
    ///
    /// When `run_id` is `Some(id)`, the runner shells `gh run view <id>
    /// --log-failed` against that specific id (this is also what the
    /// existing UI failure-log tooltip uses).
    ///
    /// When `run_id` is `None`, the runner re-resolves the `failing_run_id`
    /// from the most recent [`CiAggregate`] it cached for `plan_name`. This
    /// is what auto-mode 3.1 uses — by the time the loop sees a `Red`
    /// outcome it has dropped the run id, and the runner-side cache is
    /// the cheapest place to look it up.
    ///
    /// Best-effort: tied to a live loop caller deciding whether to spawn
    /// a fix agent.
    CiFailureLog {
        req_id: String,
        plan_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    /// Runner reply with the `--log-failed` tail (capped at 8 KB, same as
    /// `ci::fetch_failure_log`) and the run id that was actually inspected.
    /// `run_id_used` lets the caller audit which run the log came from
    /// (especially when the request was made with `run_id: None`).
    CiFailureLogResolved {
        req_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        log: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id_used: Option<String>,
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

// ── Folder entry ────────────────────────────────────────────────────────────

/// One folder returned by `ListFolders`. Same shape as the inline struct in
/// `api/settings.rs` (single-host listing); lives here so both sides share it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderEntry {
    pub name: String,
    pub path: String,
}

// ── GitHub Actions run ──────────────────────────────────────────────────────

/// One workflow run as returned by `gh run list --json
/// databaseId,status,conclusion,url`. Lives in the wire module so the runner
/// (which actually shells out to `gh`) and the SaaS-side poller share a
/// single definition; the poller used to keep this private to `ci.rs`.
///
/// The `databaseId` rename is preserved because that is the spelling `gh`
/// emits — the runner deserializes `gh`'s JSON into this struct directly,
/// then re-serializes it across the wire, and the SaaS side deserializes
/// using the same struct definition. Keeping the rename means the wire
/// representation is byte-for-byte identical to `gh`'s output, with no
/// double-translation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhRun {
    #[serde(rename = "databaseId", skip_serializing_if = "Option::is_none")]
    pub database_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ── Merge outcome ───────────────────────────────────────────────────────────

/// Outcome of a runner-side merge attempt. Mirrors the cases that
/// `merge_agent_branch` already handles in api/agents.rs:392-528 — same
/// branches, same error strings, just transposed across the wire so the
/// dispatcher in the SaaS server can map each arm to the HTTP status it
/// would have returned in the standalone path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MergeOutcome {
    /// `git merge` succeeded; `merged_sha` is the new HEAD on `target`.
    Ok { merged_sha: String },
    /// `git rev-list --count <target>..<task_branch>` returned 0 — the
    /// agent exited without committing. Server returns HTTP 409.
    EmptyBranch,
    /// `git checkout <target>` failed (dirty tree, missing branch, etc).
    /// Server returns HTTP 500 with the captured stderr.
    CheckoutFailed { stderr: String },
    /// `git merge` reported a conflict; the runner already ran
    /// `git merge --abort` so the working tree is clean. Server returns
    /// HTTP 409.
    Conflict { stderr: String },
    /// Anything else that went wrong (process spawn failed, rev-parse
    /// returned no SHA, runtime error). Server returns HTTP 500.
    Other { stderr: String },
}

// ── CI aggregate (auto-mode CI gate) ────────────────────────────────────────

/// Aggregate CI status across **every** workflow run for a SHA, as resolved
/// runner-side for `GetCiRunStatus`. Plain struct (no enum tag) so the wire
/// shape is flat JSON; `status`/`conclusion` strings mirror what `gh run
/// view --json status,conclusion` emits so the auto-mode loop can pattern
/// match without a translation layer.
///
/// **Aggregation rule** (the Reglyze bug — multi-workflow CI where a
/// downstream `deploy` is `skipped` because `tests` failed upstream):
///
/// - If any run has `conclusion="failure" | "cancelled" | "timed_out"` ⇒
///   `conclusion="failure"`.
/// - If any run is `status!="completed"` and there is no already-failing
///   run ⇒ `status="in_progress"` (still polling).
/// - If all runs are `conclusion="success"` OR `conclusion="skipped"` with
///   `skipped_due_to_upstream=false` ⇒ `conclusion="success"`.
/// - A run with `conclusion="skipped"` and `skipped_due_to_upstream=true`
///   is **never** treated as success — the runner walks the workflow-graph
///   in dependency order so an upstream failure poisons every downstream
///   skip.
/// - `failing_run_id` is the first non-skipped failing run in
///   workflow-graph order. Pre-computed runner-side because the runner has
///   the `gh` context and the server shouldn't reapply the same heuristic
///   in two places.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiAggregate {
    /// Aggregate status: `queued | in_progress | completed`.
    pub status: String,
    /// Aggregate conclusion: `success | failure | cancelled | timed_out
    /// | ...`. `None` until at least one run reaches a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// Per-run breakdown so the loop can distinguish "deploy was skipped
    /// because tests failed" from "everything passed and deploy was
    /// skipped on purpose."
    pub runs: Vec<CiRunSummary>,
    /// Run id the loop should pull failure logs from. Set to the first
    /// non-skipped failing run in workflow-graph order; `None` when
    /// nothing failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_run_id: Option<String>,
}

/// One workflow run inside a [`CiAggregate`]. Fields lift from `gh run
/// list --json databaseId,workflowName,status,conclusion` plus the
/// upstream-skip flag the runner derives from per-run `gh run view --json
/// jobs` inspection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiRunSummary {
    /// `databaseId` from `gh`, stringified for ergonomic equality with
    /// `failing_run_id` and the `run_id` argument to `CiFailureLog`.
    pub run_id: String,
    /// Human-readable workflow file name (e.g. `tests.yml`).
    pub workflow_name: String,
    /// `queued | in_progress | completed`.
    pub status: String,
    /// `success | failure | cancelled | skipped | timed_out | ...`.
    /// `None` while the run is still queued / in_progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// `true` ≡ this run was skipped because an upstream `needs:`
    /// dependency failed (vs. skipped because its `if:` evaluated false).
    /// The runner sets this by inspecting `gh run view <id> --json jobs`
    /// — when every job has `conclusion="skipped"` and any job's
    /// `steps[]` reports the skip was caused by `needs:` failure.
    pub skipped_due_to_upstream: bool,
    /// `true` ≡ this run was excluded from the blocking subset by a
    /// per-plan / per-phase / repo-level CI workflow filter (see
    /// `ci::aggregate::compute_with_filter`). Informational runs surface
    /// in the per-run breakdown but never gate `conclusion` or
    /// `failing_run_id`. Defaults to `false` so older runner payloads
    /// (no field) still deserialize cleanly, and so per-run JSON stays
    /// compact when no filter was applied.
    #[serde(default, skip_serializing_if = "is_false")]
    pub informational: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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

// ── Server-bin self-diagnostic (T1.2) ───────────────────────────────────────

/// Result of the runner's startup attempt to resolve the `branchwork-server`
/// binary it will use to spawn session daemons. Shipped on `RunnerHello` so
/// the dashboard runner row can surface a green check + path or red cross +
/// reason BEFORE the operator tries to start a session — saves the next
/// operator from running `strings` on the binary to figure out the spawn
/// target.
///
/// Resolution order on the runner side:
///   1. `--server-bin <path>` CLI flag.
///   2. `which("branchwork-server")` on `$PATH`.
///   3. Bare `branchwork-server` literal (likely-broken fallback that the
///      pre-T1.2 code silently accepted — now surfaces as `NotFound`).
///
/// Cases 1 & 2 land on `Found`; case 3 (and any explicit `--server-bin`
/// pointing at a non-existent file) lands on `NotFound`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerBinDiagnostic {
    /// The runner resolved the spawn target to an existing file. `path` is
    /// the absolute path (canonicalised when possible) so the dashboard
    /// can render it verbatim and grep / `ls -l` against it match.
    Found { path: String },
    /// The runner could NOT resolve the spawn target. `searched` is the
    /// literal string the runner attempted to spawn (e.g.
    /// `"branchwork-server"` for the bare-PATH lookup, or the operator-
    /// supplied `--server-bin` path). `reason` is a short human label
    /// describing how the resolution failed (`"not on $PATH"`,
    /// `"path does not exist"`, `"path is not a file"`); the dashboard
    /// renders it inline next to the red cross.
    NotFound { searched: String, reason: String },
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
                | WireMessage::ListFolders { .. }
                | WireMessage::FoldersListed { .. }
                | WireMessage::CreateFolder { .. }
                | WireMessage::FolderCreated { .. }
                | WireMessage::GetDefaultBranch { .. }
                | WireMessage::DefaultBranchResolved { .. }
                | WireMessage::ListBranches { .. }
                | WireMessage::BranchesListed { .. }
                | WireMessage::MergeBranch { .. }
                | WireMessage::MergeResult { .. }
                | WireMessage::DiscardBranch { .. }
                | WireMessage::BranchDiscarded { .. }
                | WireMessage::StartCheckAgent { .. }
                | WireMessage::CheckAgentOutput { .. }
                | WireMessage::PushBranch { .. }
                | WireMessage::PushResult { .. }
                | WireMessage::GhRunList { .. }
                | WireMessage::GhRunListed { .. }
                | WireMessage::GhFailureLog { .. }
                | WireMessage::GhFailureLogFetched { .. }
                | WireMessage::MergeAgentBranch { .. }
                | WireMessage::AgentBranchMerged { .. }
                | WireMessage::HasGithubActions { .. }
                | WireMessage::GithubActionsDetected { .. }
                | WireMessage::GetCiRunStatus { .. }
                | WireMessage::CiRunStatusResolved { .. }
                | WireMessage::CiFailureLog { .. }
                | WireMessage::CiFailureLogResolved { .. }
                | WireMessage::RunnerLogLine { .. }
                | WireMessage::RunnerHealth { .. }
        )
    }

    /// Short label for logging / DB storage.
    pub fn event_type(&self) -> &'static str {
        match self {
            WireMessage::RunnerHello { .. } => "runner_hello",
            WireMessage::AgentStarted { .. } => "agent_started",
            WireMessage::AgentOutput { .. } => "agent_output",
            WireMessage::AgentStopped { .. } => "agent_stopped",
            WireMessage::AgentSpawnFailed { .. } => "agent_spawn_failed",
            WireMessage::TaskStatusChanged { .. } => "task_status_changed",
            WireMessage::DriverAuthReport { .. } => "driver_auth_report",
            WireMessage::RunnerLogLine { .. } => "runner_log_line",
            WireMessage::RunnerHealth { .. } => "runner_health",
            WireMessage::StartAgent { .. } => "start_agent",
            WireMessage::KillAgent { .. } => "kill_agent",
            WireMessage::ShutdownRequest { .. } => "shutdown_request",
            WireMessage::UpgradeRunner { .. } => "upgrade_runner",
            WireMessage::ResizeTerminal { .. } => "resize_terminal",
            WireMessage::AgentInput { .. } => "agent_input",
            WireMessage::TerminalReplay { .. } => "terminal_replay",
            WireMessage::ListFolders { .. } => "list_folders",
            WireMessage::FoldersListed { .. } => "folders_listed",
            WireMessage::CreateFolder { .. } => "create_folder",
            WireMessage::FolderCreated { .. } => "folder_created",
            WireMessage::GetDefaultBranch { .. } => "get_default_branch",
            WireMessage::DefaultBranchResolved { .. } => "default_branch_resolved",
            WireMessage::ListBranches { .. } => "list_branches",
            WireMessage::BranchesListed { .. } => "branches_listed",
            WireMessage::MergeBranch { .. } => "merge_branch",
            WireMessage::MergeResult { .. } => "merge_result",
            WireMessage::DiscardBranch { .. } => "discard_branch",
            WireMessage::BranchDiscarded { .. } => "branch_discarded",
            WireMessage::StartCheckAgent { .. } => "start_check_agent",
            WireMessage::CheckAgentOutput { .. } => "check_agent_output",
            WireMessage::PushBranch { .. } => "push_branch",
            WireMessage::PushResult { .. } => "push_result",
            WireMessage::GhRunList { .. } => "gh_run_list",
            WireMessage::GhRunListed { .. } => "gh_run_listed",
            WireMessage::GhFailureLog { .. } => "gh_failure_log",
            WireMessage::GhFailureLogFetched { .. } => "gh_failure_log_fetched",
            WireMessage::MergeAgentBranch { .. } => "merge_agent_branch",
            WireMessage::AgentBranchMerged { .. } => "agent_branch_merged",
            WireMessage::HasGithubActions { .. } => "has_github_actions",
            WireMessage::GithubActionsDetected { .. } => "github_actions_detected",
            WireMessage::GetCiRunStatus { .. } => "get_ci_run_status",
            WireMessage::CiRunStatusResolved { .. } => "ci_run_status_resolved",
            WireMessage::CiFailureLog { .. } => "ci_failure_log",
            WireMessage::CiFailureLogResolved { .. } => "ci_failure_log_resolved",
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
                active_agents: vec![],
                server_bin: None,
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
        let env = Envelope::reliable("r1".into(), 1, WireMessage::Ack { ack_seq: 42 });
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.message, WireMessage::Ack { ack_seq: 42 }));
    }

    #[test]
    fn is_best_effort_classification() {
        assert!(
            WireMessage::AgentOutput {
                agent_id: "a".into(),
                data: "x".into()
            }
            .is_best_effort()
        );
        assert!(WireMessage::Ping {}.is_best_effort());
        assert!(
            !WireMessage::AgentStarted {
                agent_id: "a".into(),
                plan_name: "p".into(),
                task_id: "t".into(),
                driver: "d".into(),
                cwd: "/".into(),
            }
            .is_best_effort()
        );
    }

    #[test]
    fn agent_stopped_is_reliable() {
        // Task 11.7 pin: `AgentStopped` must stay out of the best-effort
        // set so the runner-side outbox replay survives a WS flap during
        // agent exit. If anyone re-adds it to `is_best_effort`, this
        // trips and the corresponding sender (`send_reliable` in
        // `branchwork_runner.rs`) will need to flip too.
        let msg = WireMessage::AgentStopped {
            agent_id: "a".into(),
            status: "completed".into(),
            cost_usd: None,
            stop_reason: None,
        };
        assert!(!msg.is_best_effort(), "AgentStopped must be reliable");
        assert_eq!(msg.event_type(), "agent_stopped");
    }

    #[test]
    fn upgrade_runner_round_trip_and_is_reliable() {
        // Task 4.1 pin: `UpgradeRunner` is the dashboard's one-click runner
        // upgrade. Must stay reliable so a transient flap mid-click doesn't
        // silently drop the operator's intent — the install-runner.sh
        // `--just-binary` mode is idempotent so a replayed message is safe.
        let msg = WireMessage::UpgradeRunner {
            reason: Some("operator clicked Upgrade".into()),
            install_url: None,
        };
        assert!(!msg.is_best_effort(), "UpgradeRunner must be reliable");
        assert_eq!(msg.event_type(), "upgrade_runner");

        let env = Envelope::reliable("r1".into(), 8, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"upgrade_runner\""));
        // install_url: None must be omitted on the wire so the runner
        // derives the default from its own --saas-url flag.
        assert!(
            !json.contains("\"install_url\""),
            "install_url=None must be skipped"
        );
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::UpgradeRunner {
                reason,
                install_url,
            } => {
                assert_eq!(reason.as_deref(), Some("operator clicked Upgrade"));
                assert_eq!(install_url, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn upgrade_runner_omits_both_optional_fields_when_none() {
        // Pre-T4.1 wire shape is empty body. Any future operator-supplied
        // reason/url makes the JSON longer — but the *no-extras* form
        // must serialize without trailing nulls so back-compat with
        // older runners (that may not know the fields) holds.
        let msg = WireMessage::UpgradeRunner {
            reason: None,
            install_url: None,
        };
        let env = Envelope::reliable("r1".into(), 1, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"reason\""));
        assert!(!json.contains("\"install_url\""));
    }

    #[test]
    fn agent_spawn_failed_round_trip_and_is_reliable() {
        // Task 1.1 pin: `AgentSpawnFailed` is the structured signal the
        // runner emits when `Command::spawn` returns Err before the
        // session daemon ever runs. Must stay reliable so a fast WS
        // flap during the failure burst still lands on the dashboard.
        let msg = WireMessage::AgentSpawnFailed {
            agent_id: "agent-x".into(),
            command: "/usr/local/bin/branchwork-server".into(),
            errno: Some(2),
            errno_str: "ENOENT".into(),
        };
        assert!(!msg.is_best_effort(), "AgentSpawnFailed must be reliable");
        assert_eq!(msg.event_type(), "agent_spawn_failed");

        let env = Envelope::reliable("r1".into(), 7, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"agent_spawn_failed\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::AgentSpawnFailed {
                agent_id,
                command,
                errno,
                errno_str,
            } => {
                assert_eq!(agent_id, "agent-x");
                assert_eq!(command, "/usr/local/bin/branchwork-server");
                assert_eq!(errno, Some(2));
                assert_eq!(errno_str, "ENOENT");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_hello_server_bin_found_round_trip() {
        // T1.2 pin: the `server_bin` field on `RunnerHello` round-trips
        // through serde and surfaces the resolved absolute path so the
        // dashboard can render a green check next to the runner row.
        let msg = WireMessage::RunnerHello {
            hostname: "laptop".into(),
            version: "0.5.0".into(),
            drivers: vec![],
            active_agents: vec![],
            server_bin: Some(ServerBinDiagnostic::Found {
                path: "/usr/local/bin/branchwork-server".into(),
            }),
        };
        let env = Envelope::reliable("r1".into(), 1, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"server_bin\""));
        assert!(json.contains("\"state\":\"found\""));
        assert!(json.contains("\"path\":\"/usr/local/bin/branchwork-server\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::RunnerHello { server_bin, .. } => {
                assert_eq!(
                    server_bin,
                    Some(ServerBinDiagnostic::Found {
                        path: "/usr/local/bin/branchwork-server".into(),
                    })
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_hello_server_bin_not_found_round_trip() {
        // T1.2 pin: the `NotFound` arm carries the literal string the
        // runner tried to spawn + a short reason label, both rendered
        // verbatim on the dashboard.
        let msg = WireMessage::RunnerHello {
            hostname: "laptop".into(),
            version: "0.5.0".into(),
            drivers: vec![],
            active_agents: vec![],
            server_bin: Some(ServerBinDiagnostic::NotFound {
                searched: "branchwork-server".into(),
                reason: "not on $PATH".into(),
            }),
        };
        let env = Envelope::reliable("r1".into(), 1, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"state\":\"not_found\""));
        assert!(json.contains("\"searched\":\"branchwork-server\""));
        assert!(json.contains("\"reason\":\"not on $PATH\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::RunnerHello { server_bin, .. } => match server_bin {
                Some(ServerBinDiagnostic::NotFound { searched, reason }) => {
                    assert_eq!(searched, "branchwork-server");
                    assert_eq!(reason, "not on $PATH");
                }
                other => panic!("expected NotFound, got: {other:?}"),
            },
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_hello_server_bin_omitted_when_none() {
        // Back-compat: old runners pre-T1.2 don't send the field, so the
        // serializer must skip the key when the value is None. Round-trip
        // from a JSON literal that lacks the key (i.e. an older runner)
        // must deserialise with `server_bin: None`.
        let msg = WireMessage::RunnerHello {
            hostname: "laptop".into(),
            version: "0.5.0".into(),
            drivers: vec![],
            active_agents: vec![],
            server_bin: None,
        };
        let env = Envelope::reliable("r1".into(), 1, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            !json.contains("\"server_bin\""),
            "server_bin must be omitted when None for back-compat with older runners"
        );

        // Older-runner JSON shape (no server_bin key) must deserialise to None.
        let old_json = r#"{"seq":1,"runner_id":"r1","type":"runner_hello","hostname":"old","version":"0.4.0","drivers":[]}"#;
        let back: Envelope = serde_json::from_str(old_json).unwrap();
        match back.message {
            WireMessage::RunnerHello { server_bin, .. } => {
                assert_eq!(
                    server_bin, None,
                    "missing field must default to None for older runners"
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn agent_spawn_failed_omits_errno_when_none() {
        // Non-OS errors (e.g. command-string UTF-8 problems) leave errno
        // as None — the serializer must skip the key so the wire stays
        // tidy and old consumers that don't know the field don't trip
        // on a JSON null they didn't expect.
        let msg = WireMessage::AgentSpawnFailed {
            agent_id: "a".into(),
            command: "x".into(),
            errno: None,
            errno_str: "IO_ERROR".into(),
        };
        let env = Envelope::reliable("r1".into(), 1, msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"errno\""), "errno key should be omitted");
        assert!(json.contains("\"errno_str\":\"IO_ERROR\""));
    }

    #[test]
    fn list_folders_round_trip() {
        let msg = WireMessage::ListFolders {
            req_id: "req-1".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "list_folders");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::ListFolders { req_id } => assert_eq!(req_id, "req-1"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn folders_listed_round_trip() {
        let msg = WireMessage::FoldersListed {
            req_id: "req-2".into(),
            entries: vec![
                FolderEntry {
                    name: "projects".into(),
                    path: "/home/user/projects".into(),
                },
                FolderEntry {
                    name: "docs".into(),
                    path: "/home/user/docs".into(),
                },
            ],
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "folders_listed");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::FoldersListed { req_id, entries } => {
                assert_eq!(req_id, "req-2");
                assert_eq!(
                    entries,
                    vec![
                        FolderEntry {
                            name: "projects".into(),
                            path: "/home/user/projects".into(),
                        },
                        FolderEntry {
                            name: "docs".into(),
                            path: "/home/user/docs".into(),
                        },
                    ]
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn create_folder_round_trip() {
        let msg = WireMessage::CreateFolder {
            req_id: "req-3".into(),
            path: "/home/user/new-project".into(),
            create_if_missing: true,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "create_folder");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CreateFolder {
                req_id,
                path,
                create_if_missing,
            } => {
                assert_eq!(req_id, "req-3");
                assert_eq!(path, "/home/user/new-project");
                assert!(create_if_missing);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn create_folder_default_create_if_missing_is_false() {
        // Older runners may have been built before the create_if_missing field
        // existed — verify the serde default keeps deserialization working.
        let json = r#"{"type":"create_folder","req_id":"req-x","path":"/tmp/x"}"#;
        let msg: WireMessage = serde_json::from_str(json).unwrap();
        match msg {
            WireMessage::CreateFolder {
                req_id,
                path,
                create_if_missing,
            } => {
                assert_eq!(req_id, "req-x");
                assert_eq!(path, "/tmp/x");
                assert!(!create_if_missing);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Pre-T5.2 servers and runners did not carry `session_id` /
    /// `mcp_config` / `settings_json` on `StartAgent`. Ensure deserialising
    /// such an envelope still works (serde_default strings → empty), so
    /// older→newer mixes degrade rather than crash.
    #[test]
    fn start_agent_back_compat_default_session_mcp_settings() {
        let json = r#"{"type":"start_agent","agent_id":"a","plan_name":"p","task_id":"t","prompt":"hi","cwd":"/x","driver":"claude"}"#;
        let msg: WireMessage = serde_json::from_str(json).unwrap();
        match msg {
            WireMessage::StartAgent {
                session_id,
                mcp_config,
                settings_json,
                ..
            } => {
                assert_eq!(session_id, "");
                assert_eq!(mcp_config, "");
                assert_eq!(settings_json, "");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Round-trip the full Phase 5.2 shape: server populates session_id +
    /// mcp_config + settings_json, runner deserialises and uses them
    /// verbatim. Catches accidental field renames or skip_serializing
    /// flips that would silently drop these fields on the wire.
    #[test]
    fn start_agent_round_trip_with_session_mcp_settings() {
        let msg = WireMessage::StartAgent {
            agent_id: "agent-uuid".into(),
            plan_name: "demo-plan".into(),
            task_id: "1.2".into(),
            prompt: "hello".into(),
            cwd: "/home/runner/projects/demo".into(),
            driver: "claude".into(),
            effort: Some("high".into()),
            max_budget_usd: Some(2.5),
            skip_permissions: Some(false),
            session_id: "session-uuid".into(),
            mcp_config:
                r#"{"mcpServers":{"branchwork":{"type":"http","url":"http://127.0.0.1:3100/mcp"}}}"#
                    .into(),
            settings_json: r#"{"hooks":{"Stop":[]}}"#.into(),
        };
        let env = Envelope::reliable("runner-1".into(), 7, msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::StartAgent {
                session_id,
                mcp_config,
                settings_json,
                ..
            } => {
                assert_eq!(session_id, "session-uuid");
                assert!(mcp_config.contains("\"mcpServers\""));
                assert!(settings_json.contains("\"Stop\""));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn folder_created_round_trip_ok() {
        let msg = WireMessage::FolderCreated {
            req_id: "req-4".into(),
            ok: true,
            resolved_path: Some("/home/user/new-project".into()),
            error: None,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "folder_created");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        // `error: None` should be omitted in the wire form.
        assert!(!json.contains("\"error\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::FolderCreated {
                req_id,
                ok,
                resolved_path,
                error,
            } => {
                assert_eq!(req_id, "req-4");
                assert!(ok);
                assert_eq!(resolved_path.as_deref(), Some("/home/user/new-project"));
                assert!(error.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn folder_created_round_trip_err() {
        let msg = WireMessage::FolderCreated {
            req_id: "req-5".into(),
            ok: false,
            resolved_path: None,
            error: Some("permission denied".into()),
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"resolved_path\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::FolderCreated {
                req_id,
                ok,
                resolved_path,
                error,
            } => {
                assert_eq!(req_id, "req-5");
                assert!(!ok);
                assert!(resolved_path.is_none());
                assert_eq!(error.as_deref(), Some("permission denied"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn get_default_branch_round_trip() {
        let msg = WireMessage::GetDefaultBranch {
            req_id: "req-6".into(),
            cwd: "/home/user/proj".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "get_default_branch");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        // Pin the discriminator name so a future rename can't silently break the wire.
        assert!(json.contains("\"type\":\"get_default_branch\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GetDefaultBranch { req_id, cwd } => {
                assert_eq!(req_id, "req-6");
                assert_eq!(cwd, "/home/user/proj");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn default_branch_resolved_round_trip_some() {
        let msg = WireMessage::DefaultBranchResolved {
            req_id: "req-7".into(),
            branch: Some("master".into()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "default_branch_resolved");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"default_branch_resolved\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::DefaultBranchResolved { req_id, branch } => {
                assert_eq!(req_id, "req-7");
                assert_eq!(branch.as_deref(), Some("master"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn default_branch_resolved_round_trip_none() {
        // `None` should be omitted from the wire form.
        let msg = WireMessage::DefaultBranchResolved {
            req_id: "req-8".into(),
            branch: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"branch\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::DefaultBranchResolved { req_id, branch } => {
                assert_eq!(req_id, "req-8");
                assert!(branch.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn list_branches_round_trip() {
        let msg = WireMessage::ListBranches {
            req_id: "req-9".into(),
            cwd: "/home/user/proj".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "list_branches");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"list_branches\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::ListBranches { req_id, cwd } => {
                assert_eq!(req_id, "req-9");
                assert_eq!(cwd, "/home/user/proj");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn branches_listed_round_trip() {
        let msg = WireMessage::BranchesListed {
            req_id: "req-10".into(),
            branches: vec!["feature/x".into(), "main".into(), "master".into()],
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "branches_listed");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"branches_listed\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::BranchesListed { req_id, branches } => {
                assert_eq!(req_id, "req-10");
                assert_eq!(
                    branches,
                    vec![
                        "feature/x".to_string(),
                        "main".to_string(),
                        "master".to_string()
                    ]
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn merge_branch_round_trip() {
        let msg = WireMessage::MergeBranch {
            req_id: "req-11".into(),
            cwd: "/home/user/proj".into(),
            target: "master".into(),
            task_branch: "branchwork/fix/foo/1.2".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "merge_branch");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"merge_branch\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::MergeBranch {
                req_id,
                cwd,
                target,
                task_branch,
            } => {
                assert_eq!(req_id, "req-11");
                assert_eq!(cwd, "/home/user/proj");
                assert_eq!(target, "master");
                assert_eq!(task_branch, "branchwork/fix/foo/1.2");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    fn assert_merge_result_round_trip(req_id: &str, outcome: MergeOutcome) -> MergeOutcome {
        let msg = WireMessage::MergeResult {
            req_id: req_id.into(),
            outcome,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "merge_result");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"merge_result\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::MergeResult {
                req_id: rid,
                outcome,
            } => {
                assert_eq!(rid, req_id);
                outcome
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn merge_result_ok_round_trip() {
        let outcome = assert_merge_result_round_trip(
            "req-12",
            MergeOutcome::Ok {
                merged_sha: "abc123def456".into(),
            },
        );
        assert_eq!(
            outcome,
            MergeOutcome::Ok {
                merged_sha: "abc123def456".into(),
            }
        );
        // Pin the outcome discriminator so a future rename can't silently
        // break the wire (server matches on `kind`, not free-form strings).
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"kind\":\"ok\""));
    }

    #[test]
    fn merge_result_empty_branch_round_trip() {
        let outcome = assert_merge_result_round_trip("req-13", MergeOutcome::EmptyBranch);
        assert_eq!(outcome, MergeOutcome::EmptyBranch);
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"kind\":\"empty_branch\""));
    }

    #[test]
    fn merge_result_checkout_failed_round_trip() {
        let outcome = assert_merge_result_round_trip(
            "req-14",
            MergeOutcome::CheckoutFailed {
                stderr: "error: pathspec 'master' did not match any file(s) known to git".into(),
            },
        );
        assert_eq!(
            outcome,
            MergeOutcome::CheckoutFailed {
                stderr: "error: pathspec 'master' did not match any file(s) known to git".into(),
            }
        );
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"kind\":\"checkout_failed\""));
    }

    #[test]
    fn merge_result_conflict_round_trip() {
        let outcome = assert_merge_result_round_trip(
            "req-15",
            MergeOutcome::Conflict {
                stderr: "Auto-merging README.md\nCONFLICT (content): Merge conflict in README.md"
                    .into(),
            },
        );
        assert_eq!(
            outcome,
            MergeOutcome::Conflict {
                stderr: "Auto-merging README.md\nCONFLICT (content): Merge conflict in README.md"
                    .into(),
            }
        );
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"kind\":\"conflict\""));
    }

    #[test]
    fn merge_result_other_round_trip() {
        let outcome = assert_merge_result_round_trip(
            "req-16",
            MergeOutcome::Other {
                stderr: "Failed to spawn git: No such file or directory".into(),
            },
        );
        assert_eq!(
            outcome,
            MergeOutcome::Other {
                stderr: "Failed to spawn git: No such file or directory".into(),
            }
        );
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"kind\":\"other\""));
    }

    #[test]
    fn push_branch_round_trip() {
        let msg = WireMessage::PushBranch {
            req_id: "req-17".into(),
            cwd: "/home/user/proj".into(),
            branch: "master".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "push_branch");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"push_branch\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::PushBranch {
                req_id,
                cwd,
                branch,
            } => {
                assert_eq!(req_id, "req-17");
                assert_eq!(cwd, "/home/user/proj");
                assert_eq!(branch, "master");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn push_result_round_trip_ok() {
        let msg = WireMessage::PushResult {
            req_id: "req-18".into(),
            ok: true,
            stderr: None,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "push_result");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"push_result\""));
        // `stderr: None` should be omitted in the wire form.
        assert!(!json.contains("\"stderr\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::PushResult { req_id, ok, stderr } => {
                assert_eq!(req_id, "req-18");
                assert!(ok);
                assert!(stderr.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn push_result_round_trip_err() {
        let msg = WireMessage::PushResult {
            req_id: "req-19".into(),
            ok: false,
            stderr: Some("error: failed to push some refs to 'origin'".into()),
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::PushResult { req_id, ok, stderr } => {
                assert_eq!(req_id, "req-19");
                assert!(!ok);
                assert_eq!(
                    stderr.as_deref(),
                    Some("error: failed to push some refs to 'origin'")
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_run_round_trip_preserves_database_id_rename() {
        // The wire format must spell the field `databaseId` (gh's own JSON
        // shape) so a runner that piped gh's stdout straight onto the wire
        // would also work — and so the SaaS-side poller decodes both gh
        // output and runner replies with one struct.
        let run = GhRun {
            database_id: Some(123_456_789),
            status: Some("completed".into()),
            conclusion: Some("success".into()),
            url: Some("https://github.com/o/r/actions/runs/123456789".into()),
        };
        let json = serde_json::to_string(&run).unwrap();
        assert!(json.contains("\"databaseId\":123456789"));
        assert!(!json.contains("\"database_id\""));
        let back: GhRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back, run);
    }

    #[test]
    fn gh_run_decodes_native_gh_output() {
        // Smoke-check the original ci.rs use case: deserialize whatever gh
        // emits today. Empty JSON object decodes to all-None.
        let raw = r#"{"databaseId":42,"status":"in_progress","conclusion":null,"url":"https://x"}"#;
        let run: GhRun = serde_json::from_str(raw).unwrap();
        assert_eq!(run.database_id, Some(42));
        assert_eq!(run.status.as_deref(), Some("in_progress"));
        assert!(run.conclusion.is_none());
        assert_eq!(run.url.as_deref(), Some("https://x"));

        let empty: GhRun = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.database_id, None);
        assert_eq!(empty.status, None);
        assert_eq!(empty.conclusion, None);
        assert_eq!(empty.url, None);
    }

    #[test]
    fn gh_run_list_round_trip() {
        let msg = WireMessage::GhRunList {
            req_id: "req-20".into(),
            cwd: "/home/user/proj".into(),
            sha: "deadbeef".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "gh_run_list");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"gh_run_list\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhRunList { req_id, cwd, sha } => {
                assert_eq!(req_id, "req-20");
                assert_eq!(cwd, "/home/user/proj");
                assert_eq!(sha, "deadbeef");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_run_listed_round_trip_some() {
        let msg = WireMessage::GhRunListed {
            req_id: "req-21".into(),
            run: Some(GhRun {
                database_id: Some(987),
                status: Some("completed".into()),
                conclusion: Some("failure".into()),
                url: Some("https://github.com/o/r/actions/runs/987".into()),
            }),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "gh_run_listed");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"gh_run_listed\""));
        // Must keep the gh-spelled field name across the wire.
        assert!(json.contains("\"databaseId\":987"));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhRunListed { req_id, run } => {
                assert_eq!(req_id, "req-21");
                let run = run.expect("run should round-trip");
                assert_eq!(run.database_id, Some(987));
                assert_eq!(run.status.as_deref(), Some("completed"));
                assert_eq!(run.conclusion.as_deref(), Some("failure"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_run_listed_round_trip_none() {
        // No workflow has fired yet for this commit. `run: None` should be
        // omitted from the wire form.
        let msg = WireMessage::GhRunListed {
            req_id: "req-22".into(),
            run: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"run\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhRunListed { req_id, run } => {
                assert_eq!(req_id, "req-22");
                assert!(run.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_failure_log_round_trip() {
        let msg = WireMessage::GhFailureLog {
            req_id: "req-23".into(),
            cwd: "/home/user/proj".into(),
            run_id: "987654321".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "gh_failure_log");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"gh_failure_log\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhFailureLog {
                req_id,
                cwd,
                run_id,
            } => {
                assert_eq!(req_id, "req-23");
                assert_eq!(cwd, "/home/user/proj");
                assert_eq!(run_id, "987654321");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_failure_log_fetched_round_trip_some() {
        let msg = WireMessage::GhFailureLogFetched {
            req_id: "req-24".into(),
            log: Some("error: cargo test failed at line 42\n".into()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "gh_failure_log_fetched");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"gh_failure_log_fetched\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhFailureLogFetched { req_id, log } => {
                assert_eq!(req_id, "req-24");
                assert_eq!(
                    log.as_deref(),
                    Some("error: cargo test failed at line 42\n")
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn gh_failure_log_fetched_round_trip_none() {
        // Run is still pending or gh is unavailable. `log: None` should be
        // omitted from the wire form.
        let msg = WireMessage::GhFailureLogFetched {
            req_id: "req-25".into(),
            log: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"log\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GhFailureLogFetched { req_id, log } => {
                assert_eq!(req_id, "req-25");
                assert!(log.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    // ── Auto-mode round-trip frames ─────────────────────────────────────

    #[test]
    fn merge_agent_branch_round_trip_with_into() {
        let msg = WireMessage::MergeAgentBranch {
            req_id: "req-30".into(),
            agent_id: "agent-abc".into(),
            into: Some("feature/x".into()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "merge_agent_branch");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"merge_agent_branch\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::MergeAgentBranch {
                req_id,
                agent_id,
                into,
            } => {
                assert_eq!(req_id, "req-30");
                assert_eq!(agent_id, "agent-abc");
                assert_eq!(into.as_deref(), Some("feature/x"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn merge_agent_branch_round_trip_without_into() {
        // `into: None` (default-merge) should be omitted from the wire form
        // so the auto-mode loop's no-override path looks like
        // `{"type":"merge_agent_branch","req_id":"...","agent_id":"..."}`.
        let msg = WireMessage::MergeAgentBranch {
            req_id: "req-31".into(),
            agent_id: "agent-def".into(),
            into: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"into\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::MergeAgentBranch {
                req_id,
                agent_id,
                into,
            } => {
                assert_eq!(req_id, "req-31");
                assert_eq!(agent_id, "agent-def");
                assert!(into.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn agent_branch_merged_round_trip_ok() {
        let msg = WireMessage::AgentBranchMerged {
            req_id: "req-32".into(),
            ok: true,
            merged_sha: Some("deadbeef1234".into()),
            target_branch: "master".into(),
            had_conflict: false,
            error: None,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "agent_branch_merged");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"agent_branch_merged\""));
        // Optionals default to None — both fields stay off the wire.
        assert!(!json.contains("\"error\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::AgentBranchMerged {
                req_id,
                ok,
                merged_sha,
                target_branch,
                had_conflict,
                error,
            } => {
                assert_eq!(req_id, "req-32");
                assert!(ok);
                assert_eq!(merged_sha.as_deref(), Some("deadbeef1234"));
                assert_eq!(target_branch, "master");
                assert!(!had_conflict);
                assert!(error.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn agent_branch_merged_round_trip_conflict() {
        let msg = WireMessage::AgentBranchMerged {
            req_id: "req-33".into(),
            ok: false,
            merged_sha: None,
            target_branch: "master".into(),
            had_conflict: true,
            error: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        // `merged_sha: None` is omitted; the auto-mode loop reads
        // `had_conflict` first and pauses with `merge_conflict`.
        assert!(!json.contains("\"merged_sha\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::AgentBranchMerged {
                req_id,
                ok,
                merged_sha,
                target_branch,
                had_conflict,
                error,
            } => {
                assert_eq!(req_id, "req-33");
                assert!(!ok);
                assert!(merged_sha.is_none());
                assert_eq!(target_branch, "master");
                assert!(had_conflict);
                assert!(error.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn agent_branch_merged_round_trip_error() {
        let msg = WireMessage::AgentBranchMerged {
            req_id: "req-34".into(),
            ok: false,
            merged_sha: None,
            target_branch: String::new(),
            had_conflict: false,
            error: Some("agent_not_found_on_runner".into()),
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::AgentBranchMerged {
                req_id,
                ok,
                merged_sha,
                target_branch,
                had_conflict,
                error,
            } => {
                assert_eq!(req_id, "req-34");
                assert!(!ok);
                assert!(merged_sha.is_none());
                assert_eq!(target_branch, "");
                assert!(!had_conflict);
                assert_eq!(error.as_deref(), Some("agent_not_found_on_runner"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn has_github_actions_round_trip() {
        let msg = WireMessage::HasGithubActions {
            req_id: "req-35".into(),
            agent_id: "agent-abc".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "has_github_actions");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"has_github_actions\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::HasGithubActions { req_id, agent_id } => {
                assert_eq!(req_id, "req-35");
                assert_eq!(agent_id, "agent-abc");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn github_actions_detected_round_trip() {
        for present in [true, false] {
            let msg = WireMessage::GithubActionsDetected {
                req_id: "req-36".into(),
                present,
            };
            assert!(msg.is_best_effort());
            assert_eq!(msg.event_type(), "github_actions_detected");
            let env = Envelope::best_effort("r1".into(), msg);
            let json = serde_json::to_string(&env).unwrap();
            assert!(json.contains("\"type\":\"github_actions_detected\""));
            let back: Envelope = serde_json::from_str(&json).unwrap();
            match back.message {
                WireMessage::GithubActionsDetected { req_id, present: p } => {
                    assert_eq!(req_id, "req-36");
                    assert_eq!(p, present);
                }
                other => panic!("unexpected variant: {other:?}"),
            }
        }
    }

    #[test]
    fn get_ci_run_status_round_trip() {
        let msg = WireMessage::GetCiRunStatus {
            req_id: "req-37".into(),
            plan_name: "auto-mode-merge-ci-fix-loop".into(),
            task_number: "0.3".into(),
            merged_sha: "abcd1234".into(),
            blocking_workflows: None,
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "get_ci_run_status");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"get_ci_run_status\""));
        // None ⇒ field omitted on the wire (skip_serializing_if).
        assert!(!json.contains("blocking_workflows"));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GetCiRunStatus {
                req_id,
                plan_name,
                task_number,
                merged_sha,
                blocking_workflows,
            } => {
                assert_eq!(req_id, "req-37");
                assert_eq!(plan_name, "auto-mode-merge-ci-fix-loop");
                assert_eq!(task_number, "0.3");
                assert_eq!(merged_sha, "abcd1234");
                assert!(blocking_workflows.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn get_ci_run_status_with_blocking_workflows_round_trip() {
        let msg = WireMessage::GetCiRunStatus {
            req_id: "req-37b".into(),
            plan_name: "demo-plan".into(),
            task_number: "1.2".into(),
            merged_sha: "abcd1234".into(),
            blocking_workflows: Some(vec!["tests".into(), "lint".into()]),
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"blocking_workflows\":[\"tests\",\"lint\"]"));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::GetCiRunStatus {
                blocking_workflows, ..
            } => {
                assert_eq!(
                    blocking_workflows,
                    Some(vec!["tests".to_string(), "lint".to_string()])
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// Older runner JSON without the field must still deserialize — pre-1.2
    /// runners don't know about `blocking_workflows` and we want their
    /// payloads to default to `None` (unfiltered behaviour) rather than
    /// reject the message.
    #[test]
    fn get_ci_run_status_legacy_payload_without_field_defaults_to_none() {
        let raw = r#"{"runner_id":"r1","type":"get_ci_run_status","req_id":"legacy","plan_name":"p","task_number":"1.1","merged_sha":"sha"}"#;
        let env: Envelope = serde_json::from_str(raw).expect("legacy payload should parse");
        match env.message {
            WireMessage::GetCiRunStatus {
                blocking_workflows, ..
            } => {
                assert!(
                    blocking_workflows.is_none(),
                    "missing field must default to None for backwards compat"
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_run_status_resolved_round_trip_some() {
        // The Reglyze scenario: tests failed, lint succeeded, deploy was
        // skipped *because* tests failed. Aggregate must call this a
        // failure and pin `failing_run_id` to the tests run.
        let aggregate = CiAggregate {
            status: "completed".into(),
            conclusion: Some("failure".into()),
            runs: vec![
                CiRunSummary {
                    run_id: "1001".into(),
                    workflow_name: "tests.yml".into(),
                    status: "completed".into(),
                    conclusion: Some("failure".into()),
                    skipped_due_to_upstream: false,
                    informational: false,
                },
                CiRunSummary {
                    run_id: "1002".into(),
                    workflow_name: "lint.yml".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    skipped_due_to_upstream: false,
                    informational: false,
                },
                CiRunSummary {
                    run_id: "1003".into(),
                    workflow_name: "deploy.yml".into(),
                    status: "completed".into(),
                    conclusion: Some("skipped".into()),
                    skipped_due_to_upstream: true,
                    informational: false,
                },
            ],
            failing_run_id: Some("1001".into()),
        };
        let msg = WireMessage::CiRunStatusResolved {
            req_id: "req-38".into(),
            aggregate: Some(aggregate.clone()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "ci_run_status_resolved");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"ci_run_status_resolved\""));
        // Pin the upstream-skip flag spelling so a future rename can't
        // silently regress the Reglyze-fix detection.
        assert!(json.contains("\"skipped_due_to_upstream\":true"));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiRunStatusResolved {
                req_id,
                aggregate: agg,
            } => {
                assert_eq!(req_id, "req-38");
                assert_eq!(agg, Some(aggregate));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_run_status_resolved_round_trip_none() {
        // No workflow run exists yet for this SHA — runner sends back
        // `aggregate: None` and the loop polls again on the next tick.
        let msg = WireMessage::CiRunStatusResolved {
            req_id: "req-39".into(),
            aggregate: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"aggregate\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiRunStatusResolved { req_id, aggregate } => {
                assert_eq!(req_id, "req-39");
                assert!(aggregate.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_failure_log_round_trip_with_run_id() {
        let msg = WireMessage::CiFailureLog {
            req_id: "req-40".into(),
            plan_name: "auto-mode-merge-ci-fix-loop".into(),
            run_id: Some("1001".into()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "ci_failure_log");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"ci_failure_log\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiFailureLog {
                req_id,
                plan_name,
                run_id,
            } => {
                assert_eq!(req_id, "req-40");
                assert_eq!(plan_name, "auto-mode-merge-ci-fix-loop");
                assert_eq!(run_id.as_deref(), Some("1001"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_failure_log_round_trip_without_run_id() {
        // `run_id: None` is the auto-mode 3.1 path — the runner re-resolves
        // the latest aggregate's `failing_run_id` from its own cache.
        let msg = WireMessage::CiFailureLog {
            req_id: "req-41".into(),
            plan_name: "auto-mode-merge-ci-fix-loop".into(),
            run_id: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"run_id\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiFailureLog {
                req_id,
                plan_name,
                run_id,
            } => {
                assert_eq!(req_id, "req-41");
                assert_eq!(plan_name, "auto-mode-merge-ci-fix-loop");
                assert!(run_id.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_failure_log_resolved_round_trip_some() {
        let msg = WireMessage::CiFailureLogResolved {
            req_id: "req-42".into(),
            log: Some("error: cargo test failed at line 42\n".into()),
            run_id_used: Some("1001".into()),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "ci_failure_log_resolved");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"type\":\"ci_failure_log_resolved\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiFailureLogResolved {
                req_id,
                log,
                run_id_used,
            } => {
                assert_eq!(req_id, "req-42");
                assert_eq!(
                    log.as_deref(),
                    Some("error: cargo test failed at line 42\n")
                );
                assert_eq!(run_id_used.as_deref(), Some("1001"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn ci_failure_log_resolved_round_trip_none() {
        // Both the log and the run id can be absent — e.g. a `run_id: None`
        // request when the runner never saw a failing run for the plan.
        let msg = WireMessage::CiFailureLogResolved {
            req_id: "req-43".into(),
            log: None,
            run_id_used: None,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"log\""));
        assert!(!json.contains("\"run_id_used\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::CiFailureLogResolved {
                req_id,
                log,
                run_id_used,
            } => {
                assert_eq!(req_id, "req-43");
                assert!(log.is_none());
                assert!(run_id_used.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_log_line_round_trip() {
        let msg = WireMessage::RunnerLogLine {
            ts: "2026-05-07T12:34:56.789Z".into(),
            level: "info".into(),
            line: "[runner] connecting to wss://app.branchwork.dev".into(),
        };
        assert!(msg.is_best_effort());
        assert_eq!(msg.event_type(), "runner_log_line");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        // Pin the discriminator name so a future rename can't silently break the wire.
        assert!(json.contains("\"type\":\"runner_log_line\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::RunnerLogLine { ts, level, line } => {
                assert_eq!(ts, "2026-05-07T12:34:56.789Z");
                assert_eq!(level, "info");
                assert_eq!(line, "[runner] connecting to wss://app.branchwork.dev");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_log_line_truncated_marker_round_trip() {
        // The runner emits a synthetic "[truncated N lines]" frame when the
        // 200 lines/sec cap kicks in. Same wire shape, level=warn.
        let msg = WireMessage::RunnerLogLine {
            ts: "2026-05-07T12:34:56.789Z".into(),
            level: "warn".into(),
            line: "[truncated 42 lines]".into(),
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::RunnerLogLine { line, level, .. } => {
                assert_eq!(level, "warn");
                assert_eq!(line, "[truncated 42 lines]");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_health_round_trip() {
        let msg = WireMessage::RunnerHealth {
            outbox_depth: 3,
            ws_reconnects_24h: 1,
            ci_poll_ms_p50: Some(820),
            ci_poll_ms_p99: Some(2100),
            orphans_reaped_24h: 4,
        };
        assert!(msg.is_best_effort(), "RunnerHealth must be best-effort");
        assert_eq!(msg.event_type(), "runner_health");
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        // Pin the discriminator + field names so a future rename can't
        // silently break the wire.
        assert!(json.contains("\"type\":\"runner_health\""));
        assert!(json.contains("\"outbox_depth\":3"));
        assert!(json.contains("\"ws_reconnects_24h\":1"));
        assert!(json.contains("\"ci_poll_ms_p50\":820"));
        assert!(json.contains("\"ci_poll_ms_p99\":2100"));
        assert!(json.contains("\"orphans_reaped_24h\":4"));
        // Best-effort variants don't carry seq.
        assert!(!json.contains("\"seq\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::RunnerHealth {
                outbox_depth,
                ws_reconnects_24h,
                ci_poll_ms_p50,
                ci_poll_ms_p99,
                orphans_reaped_24h,
            } => {
                assert_eq!(outbox_depth, 3);
                assert_eq!(ws_reconnects_24h, 1);
                assert_eq!(ci_poll_ms_p50, Some(820));
                assert_eq!(ci_poll_ms_p99, Some(2100));
                assert_eq!(orphans_reaped_24h, 4);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn runner_health_omits_unset_ci_latency_fields() {
        // Fresh runners (or plans without GitHub Actions) have no CI poll
        // samples yet — both percentile fields must be omitted from the
        // wire so the dashboard can render them as "—" without seeing a
        // bogus 0 ms.
        let msg = WireMessage::RunnerHealth {
            outbox_depth: 0,
            ws_reconnects_24h: 0,
            ci_poll_ms_p50: None,
            ci_poll_ms_p99: None,
            orphans_reaped_24h: 0,
        };
        let env = Envelope::best_effort("r1".into(), msg);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("\"ci_poll_ms_p50\""));
        assert!(!json.contains("\"ci_poll_ms_p99\""));
    }

    #[test]
    fn runner_health_back_compat_default_orphans_reaped() {
        // Older runner builds that pre-date Task 11.6 don't emit
        // `orphans_reaped_24h`. The server must still parse the frame and
        // default the field to 0 — the `#[serde(default)]` attribute on
        // the variant locks that contract in.
        let json =
            r#"{"runner_id":"r1","type":"runner_health","outbox_depth":2,"ws_reconnects_24h":0}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env.message {
            WireMessage::RunnerHealth {
                orphans_reaped_24h,
                outbox_depth,
                ..
            } => {
                assert_eq!(orphans_reaped_24h, 0, "default must be 0");
                assert_eq!(outbox_depth, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn shutdown_request_is_reliable() {
        // Acceptance criterion: ShutdownRequest must be classified reliable
        // so the outbox replays it on reconnect — a runner that flaps mid-
        // shutdown should still honor the operator's intent.
        let msg = WireMessage::ShutdownRequest { reason: None };
        assert!(!msg.is_best_effort(), "ShutdownRequest must be reliable");
        assert_eq!(msg.event_type(), "shutdown_request");
    }

    #[test]
    fn shutdown_request_round_trip_with_reason() {
        let msg = WireMessage::ShutdownRequest {
            reason: Some("operator clicked Request shutdown".into()),
        };
        let env = Envelope::reliable("server".into(), 7, msg);
        let json = serde_json::to_string(&env).unwrap();
        // Wire shape: tagged union with snake_case discriminator.
        assert!(json.contains("\"type\":\"shutdown_request\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.message {
            WireMessage::ShutdownRequest { reason } => {
                assert_eq!(reason.as_deref(), Some("operator clicked Request shutdown"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn shutdown_request_round_trip_without_reason() {
        // The dashboard may omit `reason` entirely. Older runner builds that
        // predate this variant get a parse error and ignore the frame, which
        // is the same effective outcome as ignoring the request.
        let json = r#"{"seq":3,"runner_id":"server","type":"shutdown_request"}"#;
        let env: Envelope = serde_json::from_str(json).unwrap();
        match env.message {
            WireMessage::ShutdownRequest { reason } => assert!(reason.is_none()),
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
