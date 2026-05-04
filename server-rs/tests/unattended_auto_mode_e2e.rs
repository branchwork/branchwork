//! End-to-end integration test for the full unattended auto-mode chain.
//!
//! Acceptance criterion 6.5: a plan with auto-mode enabled, multiple
//! phases, and tasks dep-chained sequentially within phases runs from
//! start to finish with zero human input. The chain under test is:
//!
//!   start_task → stub claude commits + Stop hook
//!     → handle_stop_hook → AGENT_AUTO_FINISH audit + auto_finish_triggered
//!     → daemon exit → on_agent_exit → on_task_agent_completed
//!     → run_state_machine → merge → wait_for_ci (NotConfigured) → on_ci_passed
//!     → try_auto_advance → next task spawned
//!
//! The stub claude is a tiny bash script that simulates the agent's
//! "work" (one commit on the task branch) and POSTs to /hooks. The auto-
//! mode chain handles everything else. We DRIVE task-status completion
//! from the test thread (mimicking the MCP `update_task_status` call a
//! real agent would make) — this serializes the chain so the test stays
//! deterministic without a worktree-isolation lock.
//!
//! Unix-only: the stub is a bash script, and the spawn / signal model
//! relies on Unix process semantics. A future Windows port would need
//! a Rust-binary stub registered as a separate `[[bin]]`.

#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Sentinel byte sequence for `~/.claude/settings.json`. The auto-mode
/// chain must not touch this file — only the per-session
/// `~/.claude/sessions/<id>.settings.json` siblings. We assert byte
/// equality before/after to confirm.
const SETTINGS_SENTINEL: &str = "{\n  \"branchwork-test-sentinel\": \"do-not-touch\"\n}\n";

const PLAN_NAME: &str = "unattended-e2e";
const PROJECT_NAME: &str = "project";

/// Plan YAML with two phases:
///   - phase 5 has 5.1 → 5.2 → 5.3 dep-chained
///   - phase 6 has 6.1 with no deps
///
/// `project: project` resolves to `<HOME>/project` at run time. The
/// fixture sets `HOME=<tempdir>` so the project dir lands inside the
/// scratch dir we control.
fn plan_yaml() -> String {
    format!(
        "title: {PLAN_NAME}\n\
         context: ''\n\
         project: {PROJECT_NAME}\n\
         phases:\n  \
         - number: 5\n    \
           title: Phase 5\n    \
           description: ''\n    \
           tasks:\n      \
           - number: '5.1'\n        \
             title: Task 5.1\n        \
             description: ''\n        \
             acceptance: ''\n      \
           - number: '5.2'\n        \
             title: Task 5.2\n        \
             description: ''\n        \
             acceptance: ''\n        \
             dependencies: ['5.1']\n      \
           - number: '5.3'\n        \
             title: Task 5.3\n        \
             description: ''\n        \
             acceptance: ''\n        \
             dependencies: ['5.2']\n  \
         - number: 6\n    \
           title: Phase 6\n    \
           description: ''\n    \
           tasks:\n      \
           - number: '6.1'\n        \
             title: Task 6.1\n        \
             description: ''\n        \
             acceptance: ''\n",
    )
}

/// Bash stub for the `claude` CLI. The supervisor PTY runs this in
/// place of the real claude binary. Behaviour:
///
///   1. Parse `--session-id` and `--add-dir` from argv (other flags
///      Claude takes — `--effort`, `--mcp-config`, `--settings`,
///      `--verbose`, `--dangerously-skip-permissions`, `--max-budget-usd`,
///      `--session-id` — are tolerated and ignored).
///   2. Write a per-session worktree file and commit it on the current
///      branch. After commit the tree is clean, so `handle_stop_hook`'s
///      `check_tree_clean_for_completion` falls into the Clean branch.
///   3. POST a Stop event to `$BRANCHWORK_HOOK_URL` so the server's
///      `handle_stop_hook` fires the auto-finish path (audit row +
///      broadcast + `tokio::spawn(graceful_exit)`).
///   4. Exit 0 — the supervisor's PTY child exits, the daemon's main
///      loop breaks, the pidfile is removed (clean shutdown), the
///      server's reader sees EOF, and `on_agent_exit` enters the
///      `completed` branch (not `supervisor_unreachable`) which is what
///      kicks off `on_task_agent_completed`.
const STUB_SCRIPT: &str = r#"#!/usr/bin/env bash
set -e
sid=""
add_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id) sid="$2"; shift 2 ;;
        --add-dir) add_dir="$2"; shift 2 ;;
        --effort|--mcp-config|--settings|--max-budget-usd) shift 2 ;;
        *) shift ;;
    esac
done

# Hold the PTY open long enough for the parent server to:
#   (a) detect the supervisor's pidfile (polled every 100ms, up to 5s),
#   (b) UPDATE agents SET status='running' WHERE id=?,
#   (c) connect_and_wire + register the in-process ManagedAgent.
# Without this delay the stub commits + posts Stop and exits while
# the agent row is still 'starting', and `handle_stop_hook`'s
# `status != "running"` early-return drops the auto-finish on the
# floor. 1.5s is well above the observed end-to-end latency for
# steps (a)-(c) on Linux + WSL2.
sleep 1.5

if [ -n "$add_dir" ] && [ -d "$add_dir" ] && [ -n "$sid" ]; then
    fname="$add_dir/work-$sid.txt"
    printf 'auto-mode work for %s\n' "$sid" > "$fname"
    git -C "$add_dir" add "work-$sid.txt"
    git -C "$add_dir" commit -q -m "stub: $sid"
fi

if [ -n "$BRANCHWORK_HOOK_URL" ] && [ -n "$sid" ]; then
    body=$(printf '{"session_id":"%s","hook_event_name":"Stop"}' "$sid")
    curl -sS --max-time 5 -X POST "$BRANCHWORK_HOOK_URL" \
        -H 'Content-Type: application/json' \
        -d "$body" >/dev/null 2>&1 || true
fi

exit 0
"#;

/// Custom server fixture. Differs from `support::TestDashboard` in two
/// ways: (a) we prepend a stub-bin dir to PATH so the spawned daemon
/// resolves `claude` to our bash script, and (b) we set
/// `BRANCHWORK_HOOK_URL` so the stub knows where to POST. The existing
/// fixture doesn't allow either.
struct E2EServer {
    dir: tempfile::TempDir,
    /// Scratch project dir — used only inside `new()` to set up the
    /// initial git repo, but kept on the struct for debugging access
    /// when `TEST_SERVER_LOG` is set.
    #[allow(dead_code)]
    project: PathBuf,
    plans_dir: PathBuf,
    settings_path: PathBuf,
    db_path: PathBuf,
    port: u16,
    base_url: String,
    child: Child,
}

impl E2EServer {
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

        // Sentinel ~/.claude/settings.json — the test asserts this is
        // byte-equal before vs. after the run, so the per-session
        // settings writer never touches it.
        let settings_path = claude_dir.join("settings.json");
        std::fs::write(&settings_path, SETTINGS_SENTINEL).unwrap();

        // Initialise the scratch project as a git repo with one commit
        // so branch / merge paths have a baseline trunk to reason about.
        run_git(&project, &["init", "-q", "-b", "master"]);
        run_git(&project, &["config", "user.email", "test@e2e.local"]);
        run_git(&project, &["config", "user.name", "E2E Test"]);
        std::fs::write(project.join("README.md"), "test project\n").unwrap();
        run_git(&project, &["add", "README.md"]);
        run_git(&project, &["commit", "-q", "-m", "initial"]);

        // Drop the stub claude in stubbin/. Make it executable; without
        // +x the supervisor's `CommandBuilder` would EACCES.
        let stub_path = stub_bin.join("claude");
        std::fs::write(&stub_path, STUB_SCRIPT).unwrap();
        let mut perms = std::fs::metadata(&stub_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_path, perms).unwrap();

        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");
        let bin = env!("CARGO_BIN_EXE_branchwork-server");

        // Prepend stubbin to PATH so the daemon (which inherits PATH
        // from the server) resolves `claude` to our stub.
        let mut path_var = stub_bin.to_string_lossy().to_string();
        if let Ok(existing) = std::env::var("PATH") {
            path_var.push(':');
            path_var.push_str(&existing);
        }

        let child = Command::new(bin)
            .args([
                "--port",
                &port.to_string(),
                "--claude-dir",
                &claude_dir.to_string_lossy(),
            ])
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("PATH", &path_var)
            .env("BRANCHWORK_HOOK_URL", format!("{base_url}/hooks"))
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
            .expect("spawn branchwork-server");

        wait_healthy(&base_url);

        let db_path = claude_dir.join("branchwork.db");
        Self {
            dir,
            project,
            plans_dir,
            settings_path,
            db_path,
            port,
            base_url,
            child,
        }
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
}

impl Drop for E2EServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
        .expect("bind")
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

/// Poll `cond` every 100ms until it returns `Some(_)` or `timeout`
/// elapses. Returns the inner value. Panics with `label` on timeout.
fn poll_until<T, F>(label: &str, timeout: Duration, mut cond: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = cond() {
            return v;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {label}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Read all `audit_logs` rows for a given action. Returns `(resource_id, diff)`
/// pairs in insertion order so callers can correlate by row count or by
/// the diff's `trigger` discriminator.
fn audit_rows_for_action(
    db: &rusqlite::Connection,
    action: &str,
) -> Vec<(Option<String>, Option<String>)> {
    let mut stmt = db
        .prepare("SELECT resource_id, diff FROM audit_logs WHERE action = ?1 ORDER BY id")
        .unwrap();
    stmt.query_map(rusqlite::params![action], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

/// Look up the agent_id for a given (plan, task). The auto-mode chain
/// inserts at most one running agent per task at any time; once a task
/// is `completed` the row is the only one with that (plan, task) pair
/// in `agents`.
fn agent_id_for_task(db: &rusqlite::Connection, plan: &str, task: &str) -> Option<String> {
    db.query_row(
        "SELECT id FROM agents WHERE plan_name = ?1 AND task_id = ?2 ORDER BY started_at DESC LIMIT 1",
        rusqlite::params![plan, task],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn agent_status(db: &rusqlite::Connection, agent_id: &str) -> Option<String> {
    db.query_row(
        "SELECT status FROM agents WHERE id = ?1",
        rusqlite::params![agent_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn task_status(db: &rusqlite::Connection, plan: &str, task: &str) -> Option<String> {
    db.query_row(
        "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
        rusqlite::params![plan, task],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Headline acceptance test (6.5): a plan with auto-mode + auto-advance
/// enabled, two phases, and tasks dep-chained sequentially within
/// phase 5 runs end-to-end via the unattended chain.
///
/// We drive the chain from the test thread by PUT'ing each task's
/// `task_status = "completed"` after its agent has fired the Stop
/// hook AND its `auto_mode.merged` audit row has landed. This mirrors
/// the MCP `update_task_status(completed)` call a real agent makes —
/// the brief calls out the agent → MCP step explicitly ("the commit
/// is the agent's 'work'") and our stub's only job is to make that
/// commit and POST the Stop hook. Threading the MCP call through the
/// stub would require parsing the per-session settings JSON and
/// driving the streamable-HTTP MCP transport — overkill for a state-
/// machine test. The PUT path goes through the same
/// `try_auto_advance` helper, so the chain coverage is identical.
#[test]
fn full_unattended_run_chains_through_two_phases() {
    let server = E2EServer::new();

    // 1. Snapshot the global ~/.claude/settings.json bytes — we'll
    // verify byte equality after the run to confirm the per-session
    // settings writer never touched the user's global file.
    let settings_before = std::fs::read(&server.settings_path).expect("read settings sentinel");
    assert_eq!(
        settings_before,
        SETTINGS_SENTINEL.as_bytes(),
        "sentinel mismatch at write time — fix the test fixture"
    );

    // 2. Create the plan YAML in plans_dir + map it to our scratch
    // project via the project endpoint so `project_dir_for` resolves
    // to `<HOME>/project`.
    let plan_path = server.plans_dir.join(format!("{PLAN_NAME}.yaml"));
    std::fs::write(&plan_path, plan_yaml()).unwrap();
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/project"),
        json!({ "project": PROJECT_NAME }),
    );
    assert_eq!(s, 200);

    // 3. Enable auto-mode + auto-advance via the unified config
    // endpoint. Both flags are required by the brief; auto-mode
    // alone would also satisfy `try_auto_advance`'s OR gate.
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/config"),
        json!({ "autoMode": true, "autoAdvance": true }),
    );
    assert_eq!(s, 200);

    // 4. Pre-claim task 5.1 as `in_progress`. The `start-task`
    // endpoint does NOT write `task_status` (it just spawns the
    // agent), but the auto-mode chain — after 5.1 merges — calls
    // `try_auto_advance` which scans intra-phase tasks where
    // status ∈ {pending, failed}. Without this row, 5.1 itself
    // would be eligible (its deps are empty) and `claim_task` would
    // win, double-spawning the same task. In production an agent
    // self-marks via the MCP `update_task_status('in_progress')`
    // call early in its run; the stub doesn't, so we mimic it here.
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/tasks/5.1/status"),
        json!({ "status": "in_progress" }),
    );
    assert_eq!(s, 200);

    // 5. Kick off the chain by spawning task 5.1. From here every
    // subsequent task starts via try_auto_advance (which calls
    // claim_task, which writes its own `in_progress` row), not via
    // the start-task endpoint.
    let (s, body) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 5,
            "taskNumber": "5.1",
        }),
    );
    assert_eq!(s, 200, "start-task failed: {body}");

    // 6. Walk each task's chain in order. For each one:
    //    - wait for AGENT_AUTO_FINISH audit (Stop hook fired in stub)
    //    - wait for the agent row to flip to `completed` (clean exit)
    //    - wait for AUTO_MODE_MERGED audit (auto-mode chain merged
    //      the branch — confirms the merge happened before we PUT
    //      task_status, so the next task spawns from a freshly
    //      merged trunk)
    //    - PUT task_status='completed' so the chain advances
    //
    // The last task (6.1) does NOT need the PUT to advance the
    // chain (no more tasks), but we PUT it anyway so the final
    // state assertion (4 task_status='completed' rows) is uniform.
    let task_chain = ["5.1", "5.2", "5.3", "6.1"];
    for task in task_chain {
        // (a) Stop hook fired.
        poll_until(
            &format!("AGENT_AUTO_FINISH for {task}"),
            Duration::from_secs(30),
            || {
                let db = server.db();
                let agent_id = agent_id_for_task(&db, PLAN_NAME, task)?;
                let rows = audit_rows_for_action(&db, "agent.auto_finish");
                rows.iter()
                    .find(|(rid, _)| rid.as_deref() == Some(&agent_id))
                    .cloned()
            },
        );

        // (b) Agent row flipped to completed (on_agent_exit ran).
        let agent_id = poll_until(
            &format!("agent {task} flipped to completed"),
            Duration::from_secs(30),
            || {
                let db = server.db();
                let id = agent_id_for_task(&db, PLAN_NAME, task)?;
                match agent_status(&db, &id).as_deref() {
                    Some("completed") => Some(id),
                    _ => None,
                }
            },
        );

        // (c) Auto-mode chain merged the branch.
        poll_until(
            &format!("auto_mode.merged for agent {agent_id}"),
            Duration::from_secs(30),
            || {
                let db = server.db();
                let rows = audit_rows_for_action(&db, "auto_mode.merged");
                rows.iter()
                    .find(|(rid, _)| rid.as_deref() == Some(&agent_id))
                    .cloned()
            },
        );

        // (d) PUT task completed — triggers try_auto_advance, which
        // either spawns the next task (intra-phase or next-phase)
        // or, for 6.1, finds nothing more to do.
        let (s, body) = server.put(
            &format!("/api/plans/{PLAN_NAME}/tasks/{task}/status"),
            json!({ "status": "completed" }),
        );
        assert_eq!(s, 200, "PUT {task}=completed failed: {body}");
    }

    // 7. End-state assertions.
    let db = server.db();

    // Every task is `completed`.
    for task in task_chain {
        assert_eq!(
            task_status(&db, PLAN_NAME, task).as_deref(),
            Some("completed"),
            "task_status[{task}] should be completed"
        );
    }

    // Exactly four `agent.auto_finish` rows, one per task, each with
    // diff `{trigger: "stop_hook"}`. The diff field carries the
    // trigger discriminator so the idle-timer fallback (Phase 4)
    // can write the same action with `{trigger: "idle_timeout"}`.
    let auto_finish = audit_rows_for_action(&db, "agent.auto_finish");
    assert_eq!(
        auto_finish.len(),
        4,
        "expected 4 agent.auto_finish rows, got {auto_finish:?}"
    );
    for (resource_id, diff) in &auto_finish {
        assert!(resource_id.is_some(), "auto_finish row missing resource_id");
        let diff = diff.as_deref().unwrap_or("");
        assert!(
            diff.contains("\"trigger\":\"stop_hook\""),
            "diff missing trigger=stop_hook: {diff}"
        );
    }

    // The plan never paused — auto_mode_paused is the canary the
    // chain trips on conflict / dirty tree / fix-cap. None of those
    // should have fired in a clean run.
    let paused_reason: Option<String> = db
        .query_row(
            "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
            rusqlite::params![PLAN_NAME],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    assert!(
        paused_reason.is_none(),
        "plan should not be paused at end of run, got {paused_reason:?}"
    );

    drop(db);

    // 8. Global ~/.claude/settings.json is byte-equal — the per-
    // session writer (writes to ~/.claude/sessions/<id>.settings.json)
    // never touched the global file.
    let settings_after = std::fs::read(&server.settings_path).expect("read settings after run");
    assert_eq!(
        settings_after, settings_before,
        "user's ~/.claude/settings.json must not be touched"
    );

    // 9. Sanity: all four agent rows have a recorded session_id
    // (proves the spawn path threaded through the per-session
    // settings writer at least once per agent — anything that
    // skipped session-id allocation would surface as NULL here).
    let db = server.db();
    let session_ids: Vec<Option<String>> = {
        let mut stmt = db
            .prepare("SELECT session_id FROM agents WHERE plan_name = ?1 ORDER BY started_at")
            .unwrap();
        stmt.query_map(rusqlite::params![PLAN_NAME], |row| {
            row.get::<_, Option<String>>(0)
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    };
    assert_eq!(
        session_ids.len(),
        4,
        "expected 4 agent rows for {PLAN_NAME}, got {}",
        session_ids.len()
    );
    for sid in &session_ids {
        assert!(sid.is_some(), "agent row missing session_id");
    }

    // 10. Per-session settings files are cleaned up by on_agent_exit.
    // None of the four `<HOME>/.claude/sessions/<id>.settings.json`
    // siblings should remain on disk. (The matching `.mcp.json`
    // siblings are *not* deleted by on_agent_exit — they're written
    // by start_pty_agent for the lifetime of the agent process,
    // and the daemon is the consumer; cleanup of those is owned by
    // the supervisor's own crash-sweep, not the per-session
    // settings writer this assertion gates.)
    let sessions_dir = server.dir.path().join(".claude/sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        let leftover: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".settings.json"))
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "per-session settings files leaked: {leftover:?}"
        );
    }

    // Suppress unused warnings on the fixture's port/dir helpers —
    // they aren't read after construction but exist for debugging
    // when TEST_SERVER_LOG is set.
    let _ = (server.port, &server.dir);
}
