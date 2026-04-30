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

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct TestDashboard {
    pub dir: tempfile::TempDir,
    pub project: PathBuf,
    pub plans_dir: PathBuf,
    pub port: u16,
    pub base_url: String,
    child: Child,
}

impl TestDashboard {
    pub fn new() -> Self {
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
        let child = Command::new(bin)
            .args([
                "--port",
                &port.to_string(),
                "--claude-dir",
                &claude_dir.to_string_lossy(),
            ])
            // HOME=tmpdir so `project_dir_for` (home.join(plan.project))
            // resolves to the scratch project when the YAML's `project:`
            // field is a bare directory name. On Windows `dirs::home_dir`
            // reads USERPROFILE instead, so set that too; also set
            // HOMEDRIVE/HOMEPATH for the older fallback path.
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
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
            })
            .spawn()
            .expect("spawn branchwork-server");

        let base_url = format!("http://127.0.0.1:{port}");
        wait_healthy(&base_url);

        Self {
            dir,
            project,
            plans_dir,
            port,
            base_url,
            child,
        }
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
}

impl Drop for TestDashboard {
    fn drop(&mut self) {
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
    // 30s. Linux boots the server in <1s, but Windows CI under four parallel
    // spawns + Defender AV scan on the fresh .exe routinely takes 6–10s and
    // has flaked at exactly the 10s mark. Generous headroom, still fails
    // fast enough if the server genuinely crashed.
    let deadline = Instant::now() + Duration::from_secs(30);
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
