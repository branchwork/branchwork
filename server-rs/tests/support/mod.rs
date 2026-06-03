//! End-to-end test support: spawn the real `branchwork-server` binary
//! against a scratch git repo and drive it over HTTP.
//!
//! Design:
//! - Each test gets its own `tempdir` with `.claude/` (plans + db),
//!   `project/` (git repo), and a random port.
//! - The server binary is built in release mode by `cargo build` before
//!   `cargo test`, then located via `CARGO_BIN_EXE_branchwork-server`.
//! - `TestDashboard::new()` spawns the server, polls `/api/health` until
//!   ready, and returns a handle with `post`, `get`, `delete` helpers.
//! - `Drop` kills the server. The tempdir auto-cleans.
//!
//! No Claude API calls happen here — these tests verify the state machine
//! and HTTP surface, not agent behaviour.

#![allow(dead_code)] // helpers used across multiple test files

pub mod gh;

// Per-agent worktree primitive, pulled in via `#[path]` so
// `setup_worktree_for_test` below can create worktrees the same way the
// server does at spawn time. The include also compiles + runs
// `worktree.rs`'s own `#[cfg(test)] mod tests` inside every test binary
// that uses `support` — harmless, and matches the
// `tests/worktree_isolation.rs` precedent.
#[path = "../../src/agents/worktree.rs"]
pub mod worktree;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Process-wide cap on how many `TestDashboard` servers BOOT concurrently.
///
/// Every integration test spawns a real `branchwork-server` and
/// [`wait_healthy`] polls `/api/health` until it answers. At cargo's
/// default per-binary parallelism (= core count) that many servers boot
/// simultaneously; on CI's 4-vCPU runners they starve each other during
/// the CPU-heavy boot (DB open + migrations + bind) and one loses the
/// race — `wait_healthy` exhausts its 60 s budget, or an early request
/// comes back `s==0` (that transient is separately absorbed by the
/// retry in [`http`]). It was a ~50% per-run flake on the Rust gate,
/// independent of which test drew the short straw.
///
/// A permit is held only across spawn + `wait_healthy` (see
/// `new_with_env`), so concurrent *boots* are bounded to ~half the cores
/// while healthy servers and the ~1200 cheap non-server unit tests still
/// run at full parallelism — faster than a blanket `--test-threads=2`
/// (which throttles everything) and deadlock-free even for a test that
/// stands up several dashboards. The counter is per-process: each test
/// binary gets its own, and cargo runs binaries sequentially, so this
/// bounds the true peak.
fn server_slots() -> &'static (Mutex<usize>, Condvar) {
    static SLOTS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    SLOTS.get_or_init(|| {
        // ≤ cores/2 — the known-good ratio. The flake appeared at the
        // 1-server-per-core default; halving it was reliable in CI.
        // `BRANCHWORK_TEST_SERVER_SLOTS` overrides for big/small hosts.
        let permits = std::env::var("BRANCHWORK_TEST_SERVER_SLOTS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| {
                (std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(2)
                    / 2)
                .max(1)
            });
        (Mutex::new(permits), Condvar::new())
    })
}

/// RAII boot permit from [`server_slots`]. Acquired before spawn and
/// dropped once the server is healthy, so it bounds concurrent boots
/// without being held across user test code.
struct ServerSlot;

impl ServerSlot {
    fn acquire() -> Self {
        let (lock, cv) = server_slots();
        let mut free = lock.lock().unwrap();
        while *free == 0 {
            free = cv.wait(free).unwrap();
        }
        *free -= 1;
        ServerSlot
    }
}

impl Drop for ServerSlot {
    fn drop(&mut self) {
        let (lock, cv) = server_slots();
        *lock.lock().unwrap() += 1;
        cv.notify_one();
    }
}

pub struct TestDashboard {
    pub dir: tempfile::TempDir,
    pub project: PathBuf,
    pub plans_dir: PathBuf,
    pub port: u16,
    pub base_url: String,
    /// Optional scratch GitHub repo handle. Populated by
    /// `with_github_repo()`; `Drop` deletes the repo before the
    /// server child is killed so a stuck `gh repo delete` cannot
    /// be silently dropped on the floor when the server exits.
    gh_repo: Option<gh::GhRepo>,
    child: Child,
}

impl TestDashboard {
    pub fn new() -> Self {
        Self::new_with_env(&[])
    }

    /// Same as [`Self::new`] but passes extra `KEY=VALUE` env vars
    /// through to the spawned server. Used by tests that need to
    /// redirect the host-API base URLs at a local mock (Phase 2.3 of
    /// `runner-daemon-workspace`: `BRANCHWORK_GITHUB_API_BASE`,
    /// `BRANCHWORK_GITLAB_API_BASE`).
    pub fn new_with_env(extras: &[(&str, &str)]) -> Self {
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let claude_dir = dir.path().join(".claude");
        let plans_dir = claude_dir.join("plans");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&plans_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        // Initialise the scratch project as a git repo with one commit so
        // branch/merge paths have something to reason about.
        run("git", &["init", "-q", "-b", "master"], &project);
        run(
            "git",
            &["config", "user.email", "test@branchwork.local"],
            &project,
        );
        run("git", &["config", "user.name", "Branchwork Test"], &project);
        std::fs::write(project.join("README.md"), "test project").unwrap();
        run("git", &["add", "README.md"], &project);
        run("git", &["commit", "-q", "-m", "initial"], &project);

        let port = free_port();
        let bin = env!("CARGO_BIN_EXE_branchwork-server");
        let mut cmd = Command::new(bin);
        cmd.args([
            "--port",
            &port.to_string(),
            "--claude-dir",
            &claude_dir.to_string_lossy(),
        ])
        // HOME=tmpdir so `project_dir_for` (home.join(plan.project))
        // resolves to the scratch project when the YAML's `project:`
        // field is a bare directory name. USERPROFILE is set as a
        // best-effort hedge for any caller that does read it, but
        // `dirs::home_dir()` on Windows goes through
        // `SHGetKnownFolderPath(FOLDERID_Profile)` and ignores both
        // env vars — tests that need to redirect home-dir scans on
        // Windows must use absolute paths or skip (see
        // `tests/recovery.rs:16` and `tests/folders.rs`).
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        // Pin the server's per-agent worktrees (worktrees default ON) to
        // a tempdir inside this test's own scratch dir, so a spawned
        // agent never writes into the operator's real
        // `~/.branchwork/worktrees/`. The server resolves this per call
        // via `worktree_base()`, so a child-process env is sufficient;
        // the test process's own env is untouched (no race with the
        // `worktree_base_is_always_absolute` unit test).
        .env(
            "BRANCHWORK_WORKTREE_BASE",
            dir.path().join(".branchwork-worktrees"),
        )
        // Silence server stdout/stderr during tests unless the user
        // asked for it. Route to piped so we can drain on drop.
        .stdout(if std::env::var("TEST_SERVER_LOG").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stderr(if std::env::var("TEST_SERVER_LOG").is_ok() {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        for (k, v) in extras {
            cmd.env(*k, *v);
        }

        // Bound how many servers BOOT at once so a boot storm can't form.
        // The permit is released the moment this server is healthy (before
        // `new` returns), NOT held for the test's lifetime — so a test that
        // stands up several dashboards never holds two permits and can't
        // deadlock, while concurrent boots stay capped.
        let child = {
            let _boot = ServerSlot::acquire();
            let child = cmd.spawn().expect("spawn branchwork-server");
            wait_healthy(&format!("http://127.0.0.1:{port}"));
            child
        };

        let base_url = format!("http://127.0.0.1:{port}");

        Self {
            dir,
            project,
            plans_dir,
            port,
            base_url,
            gh_repo: None,
            child,
        }
    }

    /// Try to provision a private scratch GitHub repo for this test
    /// (`gh repo create --private`) and push the current `master`. No-op
    /// when `BRANCHWORK_TEST_GH_REPOS=1` is unset or `gh` is unavailable
    /// — callers that need the remote should check `gh_repo()` and
    /// branch on `Some`/`None`. The handle is owned by the dashboard
    /// so `Drop` (i.e. test teardown) deletes the repo automatically.
    pub fn with_github_repo(&mut self) -> &Option<gh::GhRepo> {
        if self.gh_repo.is_none() {
            self.gh_repo = gh::maybe_create(&self.project);
        }
        &self.gh_repo
    }

    /// Returns the currently-attached scratch repo, if any.
    pub fn gh_repo(&self) -> Option<&gh::GhRepo> {
        self.gh_repo.as_ref()
    }

    /// WebSocket URL for the broadcast channel. Returned as a string so
    /// individual tests can pick the WS client they want without forcing
    /// a tokio-tungstenite dev-dep on every consumer of `support::`.
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/ws", self.port)
    }

    pub fn post(&self, path: &str, body: Value) -> (u16, Value) {
        http("POST", &format!("{}{path}", self.base_url), Some(body))
    }

    pub fn put(&self, path: &str, body: Value) -> (u16, Value) {
        http("PUT", &format!("{}{path}", self.base_url), Some(body))
    }

    pub fn get(&self, path: &str) -> (u16, Value) {
        http("GET", &format!("{}{path}", self.base_url), None)
    }

    pub fn delete(&self, path: &str) -> (u16, Value) {
        http("DELETE", &format!("{}{path}", self.base_url), None)
    }

    /// Write a YAML plan file + tell the server the plan's project dir
    /// maps to our scratch repo via `plan_project`. Returns the plan name.
    pub fn create_plan(&self, name: &str, yaml: &str) -> String {
        let file = self.plans_dir.join(format!("{name}.yaml"));
        std::fs::write(&file, yaml).unwrap();
        // `plan_project` is a DB override that makes `project_dir_for`
        // resolve to our scratch project. Set it via the `PUT /api/plans/:name/project`
        // endpoint if available; otherwise rely on the YAML's `project:` field.
        name.to_string()
    }

    /// Create a branch at the current HEAD, optionally with an extra
    /// committed file. Returns the branch name.
    pub fn create_task_branch(&self, branch: &str, with_commit: bool) {
        run("git", &["checkout", "-q", "-b", branch], &self.project);
        if with_commit {
            std::fs::write(self.project.join("work.txt"), format!("work from {branch}")).unwrap();
            run("git", &["add", "work.txt"], &self.project);
            run(
                "git",
                &["commit", "-q", "-m", &format!("work on {branch}")],
                &self.project,
            );
        }
        run("git", &["checkout", "-q", "master"], &self.project);
    }

    /// Write `.github/workflows/ci.yml` with a stub workflow and commit
    /// it on the *current* branch. Callers should run this on `master`
    /// before creating task branches so descendant branches inherit the
    /// workflow file (otherwise `has_github_actions` returns false on
    /// the task branch and `trigger_after_merge` bails before reaching
    /// the `should_record_ci_run` gate this test is meant to exercise).
    pub fn setup_github_actions(&self) {
        std::fs::create_dir_all(self.project.join(".github").join("workflows")).unwrap();
        std::fs::write(
            self.project.join(".github").join("workflows").join("ci.yml"),
            "name: ci\non: [push]\njobs:\n  noop:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
        )
        .unwrap();
        run("git", &["add", ".github/workflows/ci.yml"], &self.project);
        run(
            "git",
            &["commit", "-q", "-m", "add ci workflow"],
            &self.project,
        );
    }

    /// Initialise a bare repo inside the tempdir and add it as `origin`.
    /// Local bare repos accept pushes without auth, so `git push origin
    /// <target>` inside `trigger_after_merge` succeeds without network.
    pub fn setup_origin_remote(&self) {
        let origin = self.dir.path().join("origin.git");
        let init = Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&origin)
            .output()
            .expect("spawn git init --bare");
        assert!(
            init.status.success(),
            "git init --bare: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        run(
            "git",
            &["remote", "add", "origin", &origin.to_string_lossy()],
            &self.project,
        );
    }

    /// Return all local branches in the scratch project.
    pub fn local_branches(&self) -> Vec<String> {
        let out = Command::new("git")
            .args(["branch", "--format=%(refname:short)"])
            .current_dir(&self.project)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Create a per-agent worktree against this dashboard's scratch repo,
    /// pinned to this test's own tempdir base (never the operator's
    /// `~/.branchwork/worktrees`). Wraps [`worktree::setup_worktree_in`]
    /// — the explicit-base variant — so the helper never touches the
    /// process-wide `BRANCHWORK_WORKTREE_BASE`, which would race
    /// `worktree.rs`'s `worktree_base_is_always_absolute` unit test
    /// (that module is `#[path]`-included above and its tests run in
    /// this binary). Returns the absolute worktree path; panics on
    /// failure so callers read like the other fixture helpers.
    pub fn setup_worktree_for_test(
        &self,
        plan: &str,
        task: &str,
        agent_id: &str,
        branch: &str,
    ) -> PathBuf {
        let base = self.dir.path().join(".branchwork-worktrees");
        let slug = self
            .project
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        worktree::setup_worktree_in(
            &base,
            &self.project,
            slug,
            plan,
            task,
            agent_id,
            branch,
            None,  // base off HEAD
            false, // fresh start, not a continue
        )
        .expect("setup_worktree_for_test: git worktree add should succeed")
    }
}

impl Drop for TestDashboard {
    fn drop(&mut self) {
        // Delete the scratch GitHub repo first so the operator gets the
        // stderr ping (if any) before the server child noise. The Drop
        // on the inner GhRepo runs on take(), and propagates errors as
        // best-effort log lines.
        let _ = self.gh_repo.take();
        // SIGTERM first so on-disk state gets a chance to flush; SIGKILL
        // as a safety net if the server ignores it.
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    // 60s. Linux boots the server in <1s, but Windows CI under four parallel
    // spawns + Defender AV scan on the fresh .exe routinely takes 6–10s and
    // has flaked at exactly the 10s mark. plan_config has 14 tests so cargo
    // runs them four-at-a-time; runs that hit the perfect storm of slow
    // runner + AV scan + parallel spawns have exhausted the 30s budget too
    // (see CI run 25335717345 where 8/14 tests failed with status=0). 60s
    // still fails fast enough if the server genuinely crashed.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_status: u16 = 0;
    let mut last_body = serde_json::Value::Null;
    while Instant::now() < deadline {
        let (s, body) = http("GET", &format!("{base_url}/api/health"), None);
        if s == 200 {
            return;
        }
        last_status = s;
        last_body = body;
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "server at {base_url} never became healthy (last status={last_status}, body={last_body})"
    );
}

fn run(cmd: &str, args: &[&str], cwd: &Path) {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn {cmd}: {e}"));
    if !out.status.success() {
        panic!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Minimal HTTP client: curl shelled out. Avoids pulling reqwest into
/// dev-dependencies just for tests. Returns (status_code, parsed_body).
///
/// Retries on status 0 — curl's `000`, meaning no HTTP response was
/// received at all (connection refused/reset before the server answered).
/// `wait_healthy` already proved the server is up, so a 0 here is a
/// transient connection blip under parallel-spawn + AV-scan load (the
/// `s==0` flake class, e.g. `get_returns_404_for_unknown_phase` failing
/// with `left: 0`). Because status 0 means the request never landed,
/// retrying can't double-execute a mutation.
fn http(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
    let mut last = (0u16, Value::Null);
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50 * attempt as u64));
        }
        let (s, body_val) = http_once(method, url, body.clone());
        if s != 0 {
            return (s, body_val);
        }
        last = (s, body_val);
    }
    last
}

fn http_once(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
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
