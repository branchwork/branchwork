//! Stage 1 of the dispatch harness (per the country-awareness debrief).
//!
//! Drives a diamond-dependency plan through auto-mode end-to-end using the
//! `scripted_agent` test binary as a deterministic `claude` replacement.
//! Goal is to surface the duplicate-merge / dependency-race / abort-rebase
//! bugs we hit in production today, against a clean scratch repo so each
//! failure mode is isolatable.
//!
//! Topology:
//!
//!   T1.1                 (seed: lib.rs with Diamond trait)
//!    / \
//!   T1.2  T1.3           (impl method A / B in separate files)
//!    \ /
//!   T1.4                 (consumer in main.rs)
//!
//! First pass uses disjoint files for T1.2 / T1.3 so the test exercises
//! fan-out + fan-in dispatch *without* the merge-conflict surface. A
//! later same-file variant lives next to it.

#![cfg(unix)]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PLAN_NAME: &str = "diamond";
const PROJECT_NAME: &str = "project";

fn plan_yaml() -> &'static str {
    r#"title: diamond
project: project
context: ''
phases:
  - number: 1
    title: Diamond
    description: ''
    tasks:
      - number: '1.1'
        title: Diamond trait seed
        description: ''
        acceptance: ''
      - number: '1.2'
        title: Implement method A
        description: ''
        acceptance: ''
        dependencies: ['1.1']
      - number: '1.3'
        title: Implement method B
        description: ''
        acceptance: ''
        dependencies: ['1.1']
      - number: '1.4'
        title: Consumer
        description: ''
        acceptance: ''
        dependencies: ['1.2', '1.3']
"#
}

fn actions_yaml_disjoint_files() -> &'static str {
    r#""1.1":
  edits:
    - kind: write
      file: lib.txt
      write: "trait Diamond\n"
  commit_message: "T1.1 seed Diamond trait"

"1.2":
  edits:
    - kind: write
      file: method_a.txt
      write: "impl Diamond method A\n"
  commit_message: "T1.2 method A"

"1.3":
  edits:
    - kind: write
      file: method_b.txt
      write: "impl Diamond method B\n"
  commit_message: "T1.3 method B"

"1.4":
  edits:
    - kind: write
      file: main.txt
      write: "consumer of A + B\n"
  commit_message: "T1.4 consumer"
"#
}

/// Same plan topology as the disjoint variant, but T1.2 and T1.3 both
/// *append* to `lib.txt`. T1.2's branch and T1.3's branch start from
/// the same base (T1.1's commit); when one merges to master the other
/// has to rebase or merge — exactly the surface the duplicate-history
/// pattern lives in.
fn actions_yaml_same_file() -> &'static str {
    r#""1.1":
  edits:
    - kind: write
      file: lib.txt
      write: "trait Diamond\n"
  commit_message: "T1.1 seed Diamond trait"

"1.2":
  edits:
    - kind: append
      file: lib.txt
      append: "method A\n"
  commit_message: "T1.2 append method A"

"1.3":
  edits:
    - kind: append
      file: lib.txt
      append: "method B\n"
  commit_message: "T1.3 append method B"

"1.4":
  edits:
    - kind: write
      file: main.txt
      write: "consumer of A + B\n"
  commit_message: "T1.4 consumer"
"#
}

struct Fixture {
    dir: tempfile::TempDir,
    project: PathBuf,
    plans_dir: PathBuf,
    db_path: PathBuf,
    base_url: String,
    child: Child,
}

impl Fixture {
    fn with_actions(actions: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let claude_dir = dir.path().join(".claude");
        let plans_dir = claude_dir.join("plans");
        let project = dir.path().join(PROJECT_NAME);
        let stub_bin = dir.path().join("stubbin");
        let actions_path = dir.path().join("actions.yaml");
        for d in [&plans_dir, &project, &stub_bin, &claude_dir] {
            std::fs::create_dir_all(d).unwrap();
        }

        // Seed the scratch project as a git repo with one commit.
        run_git(&project, &["init", "-q", "-b", "master"]);
        run_git(&project, &["config", "user.email", "test@diamond.local"]);
        run_git(&project, &["config", "user.name", "Diamond Test"]);
        std::fs::write(project.join("README.md"), "diamond\n").unwrap();
        run_git(&project, &["add", "README.md"]);
        run_git(&project, &["commit", "-q", "-m", "initial"]);

        // Drop the actions YAML alongside the project.
        std::fs::write(&actions_path, actions).unwrap();

        // Symlink scripted_agent → stubbin/claude. The supervisor's PTY
        // resolves `claude` via PATH; pointing PATH at stubbin/ first
        // catches it. Symlink (rather than copy) so the binary picks up
        // future rebuilds without restarting the test.
        let scripted = PathBuf::from(env!("CARGO_BIN_EXE_scripted_agent"));
        let stub_path = stub_bin.join("claude");
        std::os::unix::fs::symlink(&scripted, &stub_path).expect("symlink stub");
        let mut perms = std::fs::metadata(&scripted).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&scripted, perms).unwrap();

        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");
        let bin = env!("CARGO_BIN_EXE_branchwork-server");

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
            .env("BRANCHWORK_SCRIPTED_ACTIONS_FILE", &actions_path)
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
            db_path,
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// First-pass diamond: T1.1 → {T1.2, T1.3} → T1.4 with disjoint files.
/// Watches for duplicate commits (the bug pattern from this morning's
/// master mess) and confirms T1.4 is dispatched only after both its
/// dependencies merged.
#[test]
fn diamond_no_conflict_completes_with_clean_history() {
    run_diamond(Fixture::with_actions(actions_yaml_disjoint_files()));
}

/// T1.2 and T1.3 both append to `lib.txt`. Branches share T1.1 as
/// base; auto-mode has to rebase or merge for the second one. Today
/// this is where the duplicate-history pattern lives.
#[test]
fn diamond_same_file_completes_with_clean_history() {
    run_diamond(Fixture::with_actions(actions_yaml_same_file()));
}

fn run_diamond(server: Fixture) {
    let plan_path = server.plans_dir.join(format!("{PLAN_NAME}.yaml"));
    std::fs::write(&plan_path, plan_yaml()).unwrap();
    let (s, body) = server.put(
        &format!("/api/plans/{PLAN_NAME}/project"),
        json!({ "project": PROJECT_NAME }),
    );
    assert_eq!(s, 200, "project map failed: {body}");

    let (s, body) = server.put(
        &format!("/api/plans/{PLAN_NAME}/config"),
        json!({ "autoMode": true, "autoAdvance": true }),
    );
    assert_eq!(s, 200, "config failed: {body}");

    {
        let db = server.db();
        db.execute(
            "UPDATE plan_auto_mode SET merge_cadence = 'task' WHERE plan_name = ?1",
            rusqlite::params![PLAN_NAME],
        )
        .expect("pin merge_cadence='task'");
    }

    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/tasks/1.1/status"),
        json!({ "status": "in_progress" }),
    );
    assert_eq!(s, 200);

    let (s, body) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200, "start-task failed: {body}");

    // Drive each task's MCP-completed PUT after its Stop hook fires +
    // its branch merges (matching the unattended-mode pattern). The
    // outer loop polls for *any* of the still-pending tasks to reach
    // the auto-finish state, so fan-out tasks (1.2 + 1.3) can be
    // observed in whatever order auto-mode actually dispatches them.
    let all_tasks = ["1.1", "1.2", "1.3", "1.4"];
    let mut completed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while completed.len() < all_tasks.len() {
        if Instant::now() >= deadline {
            panic!("diamond plan timed out; completed so far: {:?}", completed);
        }
        let pending: Vec<&str> = all_tasks
            .iter()
            .copied()
            .filter(|t| !completed.contains(t))
            .collect();
        let mut progressed = false;
        for task in pending {
            let db = server.db();
            let Some(agent_id) = agent_id_for_task(&db, PLAN_NAME, task) else {
                continue;
            };
            let auto_finish_seen = audit_rows_for_action(&db, "agent.auto_finish")
                .iter()
                .any(|(rid, _)| rid.as_deref() == Some(&agent_id));
            if !auto_finish_seen {
                continue;
            }
            let agent_completed =
                matches!(agent_status(&db, &agent_id).as_deref(), Some("completed"));
            if !agent_completed {
                continue;
            }
            let merged_seen = audit_rows_for_action(&db, "auto_mode.merged")
                .iter()
                .any(|(rid, _)| rid.as_deref() == Some(&agent_id));
            if !merged_seen {
                continue;
            }
            drop(db);
            let (s, body) = server.put(
                &format!("/api/plans/{PLAN_NAME}/tasks/{task}/status"),
                json!({ "status": "completed" }),
            );
            assert_eq!(s, 200, "PUT {task}=completed failed: {body}");
            completed.insert(task);
            progressed = true;
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // ── Assertions on the resulting state ─────────────────────────────

    let db = server.db();

    // Every task ended in `completed` — no failures, no stranded rows.
    for task in all_tasks {
        assert_eq!(
            task_status(&db, PLAN_NAME, task).as_deref(),
            Some("completed"),
            "task_status[{task}] should be completed"
        );
    }

    // No agent ended up `killed` or `orphaned` — the kill-mid-cleanup
    // pattern we saw on country-awareness/2.2 would surface here.
    let bad_statuses: Vec<(String, String)> = {
        let mut stmt = db
            .prepare(
                "SELECT id, status FROM agents \
                 WHERE plan_name = ?1 AND status IN ('killed','orphaned')",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![PLAN_NAME], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    };
    assert!(
        bad_statuses.is_empty(),
        "agents with killed/orphaned status: {bad_statuses:?}"
    );

    // Plan never paused — auto_mode pauses on conflict / dirty tree /
    // fix-cap. None of those should fire on the no-conflict diamond.
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
        "plan paused unexpectedly: {paused_reason:?}"
    );

    drop(db);

    // ── git-history invariants on the scratch project's master ───────

    let log = git_log_subjects(&server.project);

    // Each task's commit message appears exactly once. Today's
    // duplicate-merge bug (parallel-history merge bringing the same
    // patch in twice with different hashes) would surface as count > 1.
    for task in all_tasks {
        let needle = format!("T{task} ");
        let hits = log.iter().filter(|m| m.contains(&needle)).count();
        assert_eq!(
            hits, 1,
            "expected exactly one commit matching 'T{task} ', found {hits}: {log:?}"
        );
    }

    // T1.4's commit must be ancestor-reachable from master, and both
    // T1.2 and T1.3 must be ancestors of T1.4 — this is the diamond's
    // "fan-in saw both inputs" invariant.
    let t14 = commit_for_subject(&server.project, "T1.4 ");
    let t12 = commit_for_subject(&server.project, "T1.2 ");
    let t13 = commit_for_subject(&server.project, "T1.3 ");
    assert!(
        is_ancestor(&server.project, &t12, &t14),
        "T1.2 ({t12}) is not an ancestor of T1.4 ({t14})"
    );
    assert!(
        is_ancestor(&server.project, &t13, &t14),
        "T1.3 ({t13}) is not an ancestor of T1.4 ({t14})"
    );

    let _ = &server.dir; // suppress unused on debug-only field
}

// ── helpers ─────────────────────────────────────────────────────────────

fn run_git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
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
        .unwrap_or_else(|| panic!("bad curl: {stdout}"));
    let status: u16 = status_str.trim().parse().unwrap_or(0);
    let value: Value = if body_str.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body_str).unwrap_or(Value::String(body_str.to_string()))
    };
    (status, value)
}

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

fn git_log_subjects(cwd: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["log", "--pretty=format:%s", "master"])
        .current_dir(cwd)
        .output()
        .expect("git log");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn commit_for_subject(cwd: &Path, needle: &str) -> String {
    let out = Command::new("git")
        .args(["log", "--pretty=format:%H %s", "master"])
        .current_dir(cwd)
        .output()
        .expect("git log");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no commit matching '{needle}'"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
