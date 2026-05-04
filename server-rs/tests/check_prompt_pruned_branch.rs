//! Integration regression for the live failure case described in
//! ADR 0004 (`docs/adrs/0004-unify-check-prompts.md`) and reproduced
//! on 2026-05-04 against `portable-agents-and-mcp/0.1`:
//!
//! - Single-task check returned `completed` (file content matched).
//! - Multi-task check returned `pending` because its prompt asserted
//!   the per-task branch existed; after merge + prune the branch was
//!   gone and the agent dutifully marked the task pending.
//!
//! This test recreates the failure state — `Cargo.toml` on master with
//! the matching content, NO `branchwork/<plan>/0.1` branch — and drives
//! the check via the unified `build_check_prompt` builder. With Phase 1
//! of `unify-check-prompts` in place the prompt no longer instructs
//! branch detection, so the verdict comes back `completed`.
//!
//! Reverting Phase 1 (restoring the branch-asserting prompt) flips the
//! fake-agent stub's heuristic into the "git log mentioned → return
//! pending" arm, so the assertion fails with
//! `expected completed, got pending`.
//!
//! Unix-only: the fake claude is a bash script and the spawn / signal
//! model relies on Unix process semantics — same posture as
//! `tests/unattended_auto_mode_e2e.rs`.

#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PLAN_NAME: &str = "pruned-branch-fixture";
const PROJECT_NAME: &str = "project";
const TASK_NUMBER: &str = "0.1";

/// Plan YAML that mirrors `portable-agents-and-mcp/0.1`'s shape:
/// task `0.1`, `file_paths` includes `Cargo.toml`, acceptance mentions
/// `interprocess` v2 and `postcard` v1. The plan name is intentionally
/// distinct from the real plan to avoid grep-collisions; ADR 0004 is
/// the cross-reference for the live incident.
fn plan_yaml() -> String {
    format!(
        "title: Pruned branch fixture\n\
         context: ''\n\
         project: {PROJECT_NAME}\n\
         phases:\n  \
         - number: 0\n    \
           title: Setup\n    \
           description: ''\n    \
           tasks:\n      \
           - number: '{TASK_NUMBER}'\n        \
             title: Add interprocess + postcard deps\n        \
             description: |\n          \
               Add interprocess v2 and postcard v1 to Cargo.toml.\n        \
             file_paths:\n          \
               - Cargo.toml\n        \
             acceptance: |\n          \
               Cargo.toml depends on interprocess v2 and postcard v1.\n",
    )
}

/// `Cargo.toml` content matching the live failing case: declares
/// `interprocess` v2 and `postcard` v1 so the acceptance criterion is
/// satisfied by master alone — no per-task branch needed.
const CARGO_TOML_FIXTURE: &str = "[package]\n\
name = \"fixture\"\n\
version = \"0.1.0\"\n\
edition = \"2024\"\n\
\n\
[dependencies]\n\
interprocess = { version = \"2\", features = [\"tokio\"] }\n\
postcard = { version = \"1\", features = [\"alloc\"] }\n";

/// Bash stub for the `claude` CLI. The check agent runs this in place
/// of the real claude binary. Behaviour:
///
///   1. Drain claude's argv (we ignore everything except `--session-id`).
///   2. Read the prompt JSON from stdin and dump it to
///      `$BRANCHWORK_TEST_PROMPT_CAPTURE` so the test can grep it.
///   3. Decide a verdict: by default `completed` (acceptance is satisfied
///      by `Cargo.toml` alone), but if the prompt mentions `git log` the
///      stub returns `pending` instead — that's the "pre-Phase-1 prompt
///      was reintroduced" tripwire that flips the test diagnostic into
///      `expected completed, got pending`.
///   4. Emit two stream-json lines to stdout: a `system/init` marker and
///      a `result/success` event whose `result` field carries the
///      verdict JSON. `check_agent.rs`'s extractor walks `agent_output`
///      rows in reverse and parses the first line whose text contains
///      `"status"`.
const STUB_SCRIPT: &str = r#"#!/usr/bin/env bash
set -e
sid=""
while [ $# -gt 0 ]; do
    case "$1" in
        --session-id) sid="$2"; shift 2 ;;
        --add-dir|--effort|--input-format|--output-format|--permission-mode|--allowedTools|--mcp-config|--settings|--max-budget-usd) shift 2 ;;
        *) shift ;;
    esac
done

prompt_json=$(cat)
if [ -n "$BRANCHWORK_TEST_PROMPT_CAPTURE" ]; then
    printf '%s' "$prompt_json" > "$BRANCHWORK_TEST_PROMPT_CAPTURE"
fi

status="completed"
reason="Cargo.toml lists interprocess v2 and postcard v1 - acceptance satisfied."
if printf '%s' "$prompt_json" | grep -q 'git log'; then
    status="pending"
    reason="prompt instructs branch verification but the branch does not exist"
fi

cat <<EOF
{"type":"system","subtype":"init","session_id":"$sid"}
{"type":"result","subtype":"success","session_id":"$sid","total_cost_usd":0.0,"result":"{\"status\":\"$status\",\"reason\":\"$reason\"}"}
EOF
exit 0
"#;

struct Fixture {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    project: PathBuf,
    db_path: PathBuf,
    capture_path: PathBuf,
    base_url: String,
    child: Child,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let claude_dir = dir.path().join(".claude");
        let plans_dir = claude_dir.join("plans");
        let project = dir.path().join(PROJECT_NAME);
        let stub_bin = dir.path().join("stubbin");
        let capture_path = dir.path().join("captured-prompt.json");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&stub_bin).unwrap();

        // Init the project as a git repo with `Cargo.toml` on master.
        // NO `branchwork/<plan>/0.1` branch is created — the entire point
        // is to reproduce the post-merge / post-prune state where the
        // file content satisfies acceptance but the per-task branch is
        // gone.
        run_git(&project, &["init", "-q", "-b", "master"]);
        run_git(
            &project,
            &["config", "user.email", "test@check-prompt.local"],
        );
        run_git(&project, &["config", "user.name", "Check Prompt Test"]);
        std::fs::write(project.join("Cargo.toml"), CARGO_TOML_FIXTURE).unwrap();
        run_git(&project, &["add", "Cargo.toml"]);
        run_git(
            &project,
            &["commit", "-q", "-m", "add interprocess + postcard"],
        );

        // Drop the stub claude in stubbin/ and chmod +x so the supervisor's
        // `Command::new("claude")` resolves and execs it.
        let stub_path = stub_bin.join("claude");
        std::fs::write(&stub_path, STUB_SCRIPT).unwrap();
        let mut perms = std::fs::metadata(&stub_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub_path, perms).unwrap();

        std::fs::write(plans_dir.join(format!("{PLAN_NAME}.yaml")), plan_yaml()).unwrap();

        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");
        let bin = env!("CARGO_BIN_EXE_branchwork-server");

        // Prepend `stubbin/` to PATH so the spawned check agent's
        // `Command::new("claude")` resolves to our shim.
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
            // HOME=tempdir so `dirs::home_dir().join(plan.project)` lands
            // inside our scratch dir.
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("PATH", &path_var)
            .env("BRANCHWORK_TEST_PROMPT_CAPTURE", &capture_path)
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
            db_path,
            capture_path,
            base_url,
            child,
        }
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        http("POST", &format!("{}{path}", self.base_url), Some(body))
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.db_path).expect("open db")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn pruned_branch_task_verdict_is_completed() {
    let fx = Fixture::new();

    // Sanity: the per-task branch must NOT exist. That's the entire
    // point of the fixture — a task whose work has been merged + the
    // branch pruned should still verify as completed.
    let branches = run_git_capture(&fx.project, &["branch", "--format=%(refname:short)"]);
    let task_branch = format!("branchwork/{PLAN_NAME}/{TASK_NUMBER}");
    assert!(
        !branches.lines().any(|b| b.trim() == task_branch),
        "test setup error: task branch {task_branch} should not exist; got: {branches}"
    );

    let (status, body) = fx.post(
        &format!("/api/plans/{PLAN_NAME}/tasks/{TASK_NUMBER}/check"),
        json!({}),
    );
    assert_eq!(status, 200, "check_task should accept: body={body}");
    let agent_id = body
        .get("agentId")
        .and_then(|v| v.as_str())
        .expect("response carries agentId");
    assert!(!agent_id.is_empty(), "agentId is empty");

    // Poll task_status until the verdict is recorded. Initially the
    // row is `checking`; the bg thread inside `start_check_agent`
    // overwrites it after the bash stub exits (typically <1s, deadline
    // generous to absorb CI jitter).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_status: Option<String> = None;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let conn = fx.db();
        let row: Option<String> = conn
            .query_row(
                "SELECT status FROM task_status
                 WHERE plan_name = ?1 AND task_number = ?2",
                rusqlite::params![PLAN_NAME, TASK_NUMBER],
                |r| r.get::<_, String>(0),
            )
            .ok();
        drop(conn);
        if let Some(ref s) = row
            && s != "checking"
        {
            last_status = row;
            break;
        }
        last_status = row;
        std::thread::sleep(Duration::from_millis(50));
    }

    // Primary regression signal: the verdict must be `completed`. The
    // stub flips to `pending` when its prompt-content heuristic fires
    // (see STUB_SCRIPT), so a Phase 1 reversion that reintroduces
    // `git log` in `build_check_prompt` surfaces here as
    // `expected completed, got pending`.
    let last = last_status.unwrap_or_default();
    assert_eq!(
        last, "completed",
        "expected completed, got {last}; check prompt likely regressed to a branch-asserting form (see ADR 0004)"
    );

    // Defence-in-depth: the captured prompt must not contain branch /
    // git-log / committed vocabulary even if the verdict happens to
    // come back `completed` for some other reason. Mirrors the unit
    // guard in `check_prompt_tests::prompt_shape_has_no_lifecycle_vocabulary`
    // (task 2.1), but pinned against a real working-tree fixture.
    assert!(
        fx.capture_path.exists(),
        "stub claude did not capture the prompt at {}",
        fx.capture_path.display()
    );
    let captured = std::fs::read_to_string(&fx.capture_path).unwrap();
    let prompt_text = extract_prompt_text(&captured);
    for needle in [
        "git log",
        "branchwork/",
        "task branch",
        "--not master",
        "--not main",
    ] {
        assert!(
            !prompt_text.contains(needle),
            "build_check_prompt regressed: prompt contains forbidden vocabulary `{needle}`. \
             unify-check-prompts Phase 1 dropped branch verification — see ADR 0004."
        );
    }
}

/// Pull the user-prompt text out of the captured stdin envelope.
/// `start_check_agent` writes a single line of the form
/// `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<prompt>"}]}}`.
/// We just need the value of the first `text` field; if the envelope
/// shape ever changes, fall back to the raw bytes so the substring
/// assertions still catch regressions.
fn extract_prompt_text(captured: &str) -> String {
    let parsed: Result<Value, _> = serde_json::from_str(captured.trim());
    if let Ok(v) = parsed
        && let Some(text) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
    {
        return text.to_string();
    }
    captured.to_string()
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

fn run_git_capture(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

fn wait_healthy(base_url: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_status: u16 = 0;
    while Instant::now() < deadline {
        let (s, _) = http("GET", &format!("{base_url}/api/health"), None);
        if s == 200 {
            return;
        }
        last_status = s;
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server at {base_url} never became healthy (last status={last_status})");
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
