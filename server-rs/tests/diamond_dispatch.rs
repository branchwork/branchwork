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
// The `scripted_agent` test binary that this file drives is gated
// behind the `e2e` feature (required-features in Cargo.toml). The
// `Tests / Rust` CI job runs `cargo test --release` without features,
// so `CARGO_BIN_EXE_scripted_agent` still resolves but points to a
// non-existent path → runtime panic on first `fs::metadata`. Gate the
// whole file on the same feature so the non-e2e job skips it cleanly.
#![cfg(feature = "e2e")]

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

/// T1.2 and T1.3 both REPLACE the same line in lib.txt with different
/// content. Standard `git merge` produces an unresolvable conflict —
/// auto-mode must detect this and pause the plan with a clear reason.
fn actions_yaml_unresolvable_conflict() -> &'static str {
    r#""1.1":
  edits:
    - kind: write
      file: lib.txt
      write: "pub trait Diamond { fn run(&self); }\n"
  commit_message: "T1.1 seed Diamond trait"

"1.2":
  edits:
    - kind: replace
      file: lib.txt
      find: "pub trait Diamond { fn run(&self); }\n"
      replace: "pub trait Diamond { fn run(&self) -> i32; }\n"
  commit_message: "T1.2 change run signature to i32"

"1.3":
  edits:
    - kind: replace
      file: lib.txt
      find: "pub trait Diamond { fn run(&self); }\n"
      replace: "pub trait Diamond { fn run(&self) -> String; }\n"
  commit_message: "T1.3 change run signature to String"

"1.4":
  edits:
    - kind: write
      file: main.txt
      write: "consumer\n"
  commit_message: "T1.4 consumer"
"#
}

/// Single-task plan: T1.1 commits but skips the Stop hook (skip_stop_hook).
/// Repros the country-awareness/2.2 pattern from 2026-05-22 — agent
/// finishes its work, the supervisor process exits, but the server's
/// auto-finish path never fires because Stop wasn't POSTed.
fn actions_yaml_kill_mid_cleanup() -> &'static str {
    r#""1.1":
  edits:
    - kind: write
      file: lib.txt
      write: "T1.1 committed but no Stop hook fired\n"
  commit_message: "T1.1 work-but-no-stop"
  skip_stop_hook: true
"#
}

/// T1.1's scripted agent exits non-zero before committing anything.
/// The auto-mode chain has to notice and either retry or mark the
/// task failed — silent stuck-in-running is the failure mode.
fn actions_yaml_crash_before_commit() -> &'static str {
    r#""1.1":
  exit_code: 1
"#
}

/// T1.1 commits work then exits non-zero before the Stop hook fires —
/// matches the country-awareness/2.2 shape from 2026-05-22 (commit
/// 77b0a62 landed, agent SIGKILL'd during stop-hook stage).
fn actions_yaml_crash_after_commit() -> &'static str {
    r#""1.1":
  edits:
    - kind: write
      file: lib.txt
      write: "T1.1 work landed before crash\n"
  commit_message: "T1.1 commit before crash"
  crash_after_commit: true
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
    run_diamond(Fixture::with_actions(actions_yaml_disjoint_files()), "task");
}

/// T1.2 and T1.3 both append to `lib.txt`. Branches share T1.1 as
/// base; auto-mode has to rebase or merge for the second one. Today
/// this is where the duplicate-history pattern lives.
#[test]
fn diamond_same_file_completes_with_clean_history() {
    run_diamond(Fixture::with_actions(actions_yaml_same_file()), "task");
}

/// Phase-cadence variant. With `merge_cadence='phase'` the per-task
/// merge step doesn't fire between tasks — fan-out tasks (T1.2 + T1.3)
/// should be able to run truly concurrently because neither is blocked
/// on the other's merge. The parallelism assertion at the end of
/// `run_diamond` is the canary: if T1.2 and T1.3 don't overlap, the
/// dispatcher is serialising parallelisable work, which is the bug
/// class worktree-per-agent isolation is meant to unblock.
///
/// `#[ignore]` for now because the canary is failing as-of 2026-05-22:
/// the dispatcher serialises T1.2 and T1.3 (single-cwd model, no
/// per-agent worktrees), so this would keep CI red until the fix
/// lands. Run explicitly with
/// `cargo test --features e2e --test diamond_dispatch -- --include-ignored`
/// to verify when the dispatcher learns to parallelise.
#[test]
#[ignore = "documents known parallelism bug; unignore when dispatcher fan-out lands"]
fn diamond_phase_cadence_runs_fan_out_concurrently() {
    run_diamond(
        Fixture::with_actions(actions_yaml_disjoint_files()),
        "phase",
    );
}

/// T1.2 and T1.3 modify the same line of `lib.txt` in incompatible
/// ways. Standard merge of the second branch must fail with a textual
/// conflict. Pinned to `merge_cadence='task'` so the first merge lands
/// cleanly and the second one is the one that conflicts — that's the
/// shape auto-mode's per-task merge pipeline has to handle.
///
/// Expected: auto_mode sets `plan_auto_mode.paused_reason` to a
/// conflict-shaped value (e.g. `merge_conflict`) and stops dispatching.
/// Today's behaviour is what this test discovers.
#[test]
#[ignore = "exploratory: documents how auto-mode handles real merge conflicts"]
fn diamond_real_conflict_pauses_plan() {
    let server = Fixture::with_actions(actions_yaml_unresolvable_conflict());

    setup_plan(&server, "task");
    let (s, body) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200, "start-task failed: {body}");
    drive_task(&server, "1.1", "task");

    // T1.2 and T1.3 should both be eligible after T1.1 merges. One
    // will merge cleanly; the other should hit a conflict and pause
    // the plan. Wait up to 30s for either:
    //  (a) plan_auto_mode.paused_reason becomes non-NULL, OR
    //  (b) one of T1.2/T1.3 reaches status='failed', OR
    //  (c) we time out (current behaviour is the answer)
    let deadline = Instant::now() + Duration::from_secs(30);
    let outcome = loop {
        let db = server.db();
        let paused: Option<String> = db
            .query_row(
                "SELECT paused_reason FROM plan_auto_mode WHERE plan_name = ?1",
                rusqlite::params![PLAN_NAME],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        if let Some(reason) = paused {
            break format!("paused: {reason}");
        }
        let failed: Option<String> = db
            .query_row(
                "SELECT task_number FROM task_status \
                 WHERE plan_name = ?1 AND status = 'failed' LIMIT 1",
                rusqlite::params![PLAN_NAME],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if let Some(t) = failed {
            break format!("task {t} failed");
        }
        drop(db);
        if Instant::now() >= deadline {
            break "timeout (no detection)".to_string();
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!("[conflict test] outcome: {outcome}");
    assert!(
        outcome != "timeout (no detection)",
        "auto-mode never detected the merge conflict within 30s — \
         no paused_reason set, no failed task_status. Outcome: {outcome}"
    );
}

/// Single task whose scripted agent commits its work but skips the
/// Stop hook POST — the country-awareness/2.2 pattern from 2026-05-22.
/// In production that agent ended up `status='killed'` even though the
/// work landed. This test probes what happens in the harness.
#[test]
#[ignore = "exploratory: documents kill-mid-cleanup behaviour"]
fn agent_skipping_stop_hook_reaches_terminal_state() {
    let server = Fixture::with_actions(actions_yaml_kill_mid_cleanup());

    setup_plan(&server, "task");
    let (s, body) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200, "start-task failed: {body}");

    // Poll the agent row until it hits some terminal status. We don't
    // know which one a priori — that's the finding. Cap at 30s so a
    // total no-op doesn't hang the suite.
    let deadline = Instant::now() + Duration::from_secs(30);
    let terminal = loop {
        let db = server.db();
        let row = db
            .query_row(
                "SELECT status FROM agents WHERE plan_name = ?1 AND task_id = '1.1' \
                 ORDER BY started_at DESC LIMIT 1",
                rusqlite::params![PLAN_NAME],
                |row| row.get::<_, String>(0),
            )
            .ok();
        match row.as_deref() {
            Some("completed") | Some("killed") | Some("orphaned") | Some("failed") => {
                break row.unwrap();
            }
            _ => {}
        }
        drop(db);
        if Instant::now() >= deadline {
            break "no_terminal_state".to_string();
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    eprintln!("[kill-mid-cleanup test] terminal status: {terminal}");
    assert_ne!(
        terminal, "no_terminal_state",
        "agent never reached a terminal status; supervisor reaper / on_agent_exit \
         did not fire when Stop hook was skipped"
    );
}

/// Locks in the dep-ordering invariant: T1.4 must not be dispatched
/// before BOTH T1.2 and T1.3 have completed AND their branches have
/// merged. Passing baseline today; protects against a regression
/// where the dispatcher fan-in skipped one of the deps.
#[test]
fn t14_dispatches_strictly_after_both_dependencies_merge() {
    let server = Fixture::with_actions(actions_yaml_disjoint_files());
    setup_plan(&server, "task");
    let (s, _) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200);
    for t in ["1.1", "1.2", "1.3", "1.4"] {
        drive_task(&server, t, "task");
    }

    let db = server.db();
    let (_t12_start, t12_end) = agent_time_window(&db, PLAN_NAME, "1.2")
        .expect("T1.2 must have started/finished timestamps");
    let (_t13_start, t13_end) = agent_time_window(&db, PLAN_NAME, "1.3")
        .expect("T1.3 must have started/finished timestamps");
    let (t14_start, _t14_end) = agent_time_window(&db, PLAN_NAME, "1.4")
        .expect("T1.4 must have started/finished timestamps");

    assert!(
        t14_start.as_str() >= t12_end.as_str(),
        "T1.4 started {t14_start} before T1.2 finished {t12_end} — dep violation"
    );
    assert!(
        t14_start.as_str() >= t13_end.as_str(),
        "T1.4 started {t14_start} before T1.3 finished {t13_end} — dep violation"
    );
}

/// T1.1's scripted agent exits non-zero before committing anything.
/// Auto-mode must either mark the task failed or retry it — silent
/// "still running" is the bug shape this probes.
#[test]
#[ignore = "exploratory: documents agent-crash recovery"]
fn agent_crash_before_commit_reaches_failure_state() {
    let server = Fixture::with_actions(actions_yaml_crash_before_commit());
    setup_plan(&server, "task");
    let (s, _) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200);

    let deadline = Instant::now() + Duration::from_secs(30);
    let outcome = loop {
        let db = server.db();
        let status: Option<String> = db
            .query_row(
                "SELECT status FROM agents WHERE plan_name = ?1 AND task_id = '1.1' \
                 ORDER BY started_at DESC LIMIT 1",
                rusqlite::params![PLAN_NAME],
                |row| row.get::<_, String>(0),
            )
            .ok();
        match status.as_deref() {
            Some("killed") | Some("failed") | Some("orphaned") | Some("completed") => {
                break status.unwrap();
            }
            _ => {}
        }
        drop(db);
        if Instant::now() >= deadline {
            break "no_terminal_state".to_string();
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    eprintln!("[crash-before-commit] terminal status: {outcome}");
    assert_ne!(
        outcome, "no_terminal_state",
        "agent never reached a terminal status after crashing (exit_code=1) — \
         auto-mode silently stuck in 'running'"
    );
    assert_ne!(
        outcome, "completed",
        "crashed agent (exit_code=1) was marked 'completed' — should be \
         killed/failed/orphaned to surface the failure to the operator"
    );
}

/// T1.1 commits its work then exits non-zero before posting the Stop
/// hook. Matches country-awareness/2.2 (the commit lands, the agent
/// dies during cleanup). Auto-mode must NOT mark this "completed" —
/// the operator needs to know the commit landed but the agent died
/// mid-cleanup so they can decide whether to claim the work or retry.
#[test]
#[ignore = "exploratory: documents commit-then-crash recovery"]
fn agent_crash_after_commit_preserves_work_and_surfaces_failure() {
    let server = Fixture::with_actions(actions_yaml_crash_after_commit());
    setup_plan(&server, "task");
    let (s, _) = server.post(
        "/api/actions/start-task",
        json!({
            "planName": PLAN_NAME,
            "phaseNumber": 1,
            "taskNumber": "1.1",
        }),
    );
    assert_eq!(s, 200);

    let deadline = Instant::now() + Duration::from_secs(30);
    let outcome = loop {
        let db = server.db();
        let status: Option<String> = db
            .query_row(
                "SELECT status FROM agents WHERE plan_name = ?1 AND task_id = '1.1' \
                 ORDER BY started_at DESC LIMIT 1",
                rusqlite::params![PLAN_NAME],
                |row| row.get::<_, String>(0),
            )
            .ok();
        match status.as_deref() {
            Some("killed") | Some("failed") | Some("orphaned") | Some("completed") => {
                break status.unwrap();
            }
            _ => {}
        }
        drop(db);
        if Instant::now() >= deadline {
            break "no_terminal_state".to_string();
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    eprintln!("[crash-after-commit] terminal status: {outcome}");
    assert_ne!(
        outcome, "no_terminal_state",
        "agent never reached a terminal status after committing + crashing"
    );
    assert_ne!(
        outcome, "completed",
        "agent that crashed mid-cleanup (after commit) was marked 'completed' — \
         country-awareness/2.2 pattern: work landed but failure must be surfaced"
    );

    // The commit MUST be on the task branch (the agent did land it
    // before crashing). This is the "preserve operator's work" half
    // of the invariant.
    let task_branch = format!("branchwork/{PLAN_NAME}/1.1");
    let out = std::process::Command::new("git")
        .args(["log", "--pretty=format:%s", &task_branch])
        .current_dir(&server.project)
        .output()
        .expect("git log on task branch");
    let log = String::from_utf8_lossy(&out.stdout);
    assert!(
        log.contains("T1.1 commit before crash"),
        "agent's commit was lost — branch log: {log}"
    );
}

/// Two concurrent `start-task` POSTs for the same (plan, task). The
/// server should treat the second as a no-op or 4xx — at most one
/// agent row per task. Catches a class of "user double-clicks the
/// Start button" + "auto-mode races a manual dispatch" bugs.
#[test]
#[ignore = "exploratory: documents concurrent-dispatch idempotency"]
fn concurrent_start_task_for_same_task_is_idempotent() {
    let server = Fixture::with_actions(actions_yaml_disjoint_files());
    setup_plan(&server, "task");

    let body = json!({
        "planName": PLAN_NAME,
        "phaseNumber": 1,
        "taskNumber": "1.1",
    });
    let (r1, r2) = std::thread::scope(|s| {
        let h1 = s.spawn(|| server.post("/api/actions/start-task", body.clone()));
        let h2 = s.spawn(|| server.post("/api/actions/start-task", body.clone()));
        (h1.join().unwrap(), h2.join().unwrap())
    });
    eprintln!("[concurrent dispatch] r1={:?} r2={:?}", r1.0, r2.0);

    // Give the server a beat to settle, then count agent rows.
    std::thread::sleep(Duration::from_secs(3));
    let count: i64 = server
        .db()
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE plan_name = ?1 AND task_id = '1.1'",
            rusqlite::params![PLAN_NAME],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "expected idempotent dispatch (1 agent row), got {count}. \
         Both POSTs returned: r1={} r2={}",
        r1.0, r2.0
    );
}

fn setup_plan(server: &Fixture, cadence: &str) {
    let plan_path = server.plans_dir.join(format!("{PLAN_NAME}.yaml"));
    std::fs::write(&plan_path, plan_yaml()).unwrap();
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/project"),
        json!({ "project": PROJECT_NAME }),
    );
    assert_eq!(s, 200);
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/config"),
        json!({ "autoMode": true, "autoAdvance": true }),
    );
    assert_eq!(s, 200);
    server
        .db()
        .execute(
            "UPDATE plan_auto_mode SET merge_cadence = ?2 WHERE plan_name = ?1",
            rusqlite::params![PLAN_NAME, cadence],
        )
        .expect("pin cadence");
    let (s, _) = server.put(
        &format!("/api/plans/{PLAN_NAME}/tasks/1.1/status"),
        json!({ "status": "in_progress" }),
    );
    assert_eq!(s, 200);
}

fn drive_task(server: &Fixture, task: &str, cadence: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            panic!("drive_task({task}) timed out waiting for completion chain");
        }
        let db = server.db();
        let Some(agent_id) = agent_id_for_task(&db, PLAN_NAME, task) else {
            drop(db);
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        let auto_finish = audit_rows_for_action(&db, "agent.auto_finish")
            .iter()
            .any(|(rid, _)| rid.as_deref() == Some(&agent_id));
        let completed = matches!(agent_status(&db, &agent_id).as_deref(), Some("completed"));
        let merged_ok = if cadence == "task" {
            audit_rows_for_action(&db, "auto_mode.merged")
                .iter()
                .any(|(rid, _)| rid.as_deref() == Some(&agent_id))
        } else {
            true
        };
        drop(db);
        if auto_finish && completed && merged_ok {
            let (s, _) = server.put(
                &format!("/api/plans/{PLAN_NAME}/tasks/{task}/status"),
                json!({ "status": "completed" }),
            );
            assert_eq!(s, 200);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_diamond(server: Fixture, cadence: &str) {
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
            "UPDATE plan_auto_mode SET merge_cadence = ?2 WHERE plan_name = ?1",
            rusqlite::params![PLAN_NAME, cadence],
        )
        .expect("pin merge_cadence");
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
            // With cadence='task' the auto_mode.merged audit row lands
            // per task; with cadence='phase' merges are batched at phase
            // boundary, so we only require auto_finish + completed here.
            if cadence == "task" {
                let merged_seen = audit_rows_for_action(&db, "auto_mode.merged")
                    .iter()
                    .any(|(rid, _)| rid.as_deref() == Some(&agent_id));
                if !merged_seen {
                    continue;
                }
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

    // Under cadence='phase' the per-task merges are deferred to the
    // phase boundary; the merges fire AFTER the task PUTs above. Wait
    // until every task subject is reachable from master before snapping
    // the git log — otherwise the assertion races the merge worker.
    let merge_deadline = Instant::now() + Duration::from_secs(30);
    let log = loop {
        let log = git_log_subjects(&server.project);
        let all_present = all_tasks
            .iter()
            .all(|t| log.iter().any(|m| m.contains(&format!("T{t} "))));
        if all_present {
            break log;
        }
        if Instant::now() >= merge_deadline {
            panic!(
                "timed out waiting for all task commits to land on master under \
                 cadence='{cadence}'; current log: {log:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };

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

    // Parallelism canary: T1.2 and T1.3 are sibling leaves of T1.1 and
    // have no deps on each other, so an ideal dispatcher runs them
    // concurrently. We measure overlap by `started_at` / `finished_at`
    // on the agent rows. Failure here is the signal we're after:
    // "parallelisable tasks are being serialised."
    //
    // Only enforced under `merge_cadence='phase'`; under `'task'` the
    // post-T1.2 merge step blocks T1.3's dispatch by design, so
    // overlap is impossible there and the failure would be noise.
    let db = server.db();
    if cadence == "phase" {
        let t12_window = agent_time_window(&db, PLAN_NAME, "1.2");
        let t13_window = agent_time_window(&db, PLAN_NAME, "1.3");
        if let (Some((s12, e12)), Some((s13, e13))) = (t12_window, t13_window) {
            let overlap = s12 < e13 && s13 < e12;
            assert!(
                overlap,
                "T1.2 ({s12}..{e12}) and T1.3 ({s13}..{e13}) ran serialised \
                 under cadence='phase' — dispatcher is not parallelising \
                 deps-independent tasks"
            );
        } else {
            panic!("missing started_at / finished_at on T1.2 or T1.3 agent row");
        }
    }
    drop(db);

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

fn agent_time_window(
    db: &rusqlite::Connection,
    plan: &str,
    task: &str,
) -> Option<(String, String)> {
    db.query_row(
        "SELECT started_at, finished_at FROM agents \
         WHERE plan_name = ?1 AND task_id = ?2 \
         ORDER BY started_at DESC LIMIT 1",
        rusqlite::params![plan, task],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        },
    )
    .ok()
    .filter(|(s, e)| !s.is_empty() && !e.is_empty())
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
