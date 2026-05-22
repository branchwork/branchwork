//! Integration tests for `POST /api/projects { mode: "create" }`
//! (Phase 2.3 of `runner-daemon-workspace`).
//!
//! These spin up a mock HTTP listener that impersonates the GitHub
//! REST API (`POST /user/repos` and `POST /orgs/{owner}/repos`), then
//! point the spawned `branchwork-server` at that mock via the
//! `BRANCHWORK_GITHUB_API_BASE` env hook. The clone URL returned by
//! the mock is a `file://` URL pointing at a local bare git repo
//! seeded inside the tempdir so the runner-clone dispatch can succeed
//! end-to-end without network.
//!
//! Acceptance criterion: `POST /api/projects { mode: "create", name:
//! "my-new-repo", host: "github", private: true }` returns 201 once
//! the (mock) GitHub repo exists; the project row carries the clone
//! URL; the project.clone audit row fires within 5s of the response.
//!
//! Failure-path coverage:
//! - mode=create without host -> 400 host_required
//! - mode=create without credentialId -> 400 credential_required
//! - host=bitbucket on create -> 400 unsupported_host_for_create
//! - upstream 422 (name collision) -> 422 host_api_failed with code
//!   `github_422` + message + host_response_excerpt.
//!
//! The mock GitHub server is a hand-rolled `TcpListener` accept loop
//! (no dev-dep) running in a background thread that drains each
//! connection, returns a canned response, and shuts down via a
//! `Drop`-triggered TCP connect to break the accept loop.

mod support;

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use support::TestDashboard;

/// Sign up a fresh user (creates a personal org + default-org
/// membership) and persist the session cookie into `cookie_jar`.
/// Returns the user row parsed from the signup response.
fn signup(d: &TestDashboard, jar: &Path, email: &str, password: &str) -> Value {
    let body = json!({ "email": email, "password": password });
    let (status, value) = http_with_jar(
        "POST",
        &format!("{}/api/auth/signup", d.base_url),
        Some(body),
        Some(jar),
    );
    assert_eq!(status, 201, "signup failed: status={status} body={value}");
    value
}

fn post_authed(d: &TestDashboard, jar: &Path, path: &str, body: Value) -> (u16, Value) {
    http_with_jar(
        "POST",
        &format!("{}{path}", d.base_url),
        Some(body),
        Some(jar),
    )
}

fn http_with_jar(method: &str, url: &str, body: Option<Value>, jar: Option<&Path>) -> (u16, Value) {
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
    ]);
    if let Some(j) = jar {
        cmd.args(["-c", &j.to_string_lossy(), "-b", &j.to_string_lossy()]);
    }
    cmd.arg(url);
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

/// Canned response the mock GitHub server returns. `(status, body)`.
type MockResponse = (u16, String);

/// Captured request the mock GitHub server received. Tests assert
/// against this to verify the right URL / body shape went out.
#[derive(Debug, Default, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    body: String,
    auth_header: Option<String>,
}

/// Mock GitHub server. Handles N requests in a row (each test
/// usually only fires one). Uses a nonblocking listener + an atomic
/// shutdown flag so `Drop` can stop the accept loop deterministically
/// without depending on a self-connect to unblock the thread.
struct MockGithub {
    base_url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockGithub {
    /// Spawn a mock listener that returns `response` for every
    /// incoming request. Returns immediately once the listener is
    /// bound and the accept loop is running.
    fn spawn(response: MockResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock github");
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on mock listener");
        let addr = listener.local_addr().expect("local_addr");
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = std::thread::spawn(move || {
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }
                let accept_result = listener.accept();
                let mut s = match accept_result {
                    Ok((stream, _)) => stream,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                // Switch the accepted connection to blocking so we
                // can drain the request without a busy-loop.
                let _ = s.set_nonblocking(false);
                {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 1024];
                    let mut content_length: usize = 0;
                    let mut headers_done = false;
                    // Read until we have the full headers + (if any) body.
                    loop {
                        let n = match s.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if !headers_done {
                            if let Some(idx) = find_subslice(&buf, b"\r\n\r\n") {
                                headers_done = true;
                                let header_part = &buf[..idx];
                                let header_str = String::from_utf8_lossy(header_part);
                                for line in header_str.split("\r\n") {
                                    if let Some(rest) = line
                                        .strip_prefix("Content-Length: ")
                                        .or_else(|| line.strip_prefix("content-length: "))
                                    {
                                        content_length = rest.trim().parse().unwrap_or(0);
                                    }
                                }
                                let body_have = buf.len() - (idx + 4);
                                if body_have >= content_length {
                                    break;
                                }
                            }
                        } else {
                            // Body still streaming; check if we have it all.
                            if let Some(idx) = find_subslice(&buf, b"\r\n\r\n") {
                                let body_have = buf.len() - (idx + 4);
                                if body_have >= content_length {
                                    break;
                                }
                            }
                        }
                    }

                    // Parse request line + headers + body.
                    let raw = String::from_utf8_lossy(&buf);
                    let mut method = String::new();
                    let mut path = String::new();
                    let mut auth_header: Option<String> = None;
                    let mut body = String::new();
                    if let Some((header_part, body_part)) = raw.split_once("\r\n\r\n") {
                        let mut lines = header_part.split("\r\n");
                        if let Some(req_line) = lines.next() {
                            let mut parts = req_line.split_whitespace();
                            method = parts.next().unwrap_or("").to_string();
                            path = parts.next().unwrap_or("").to_string();
                        }
                        for line in lines {
                            if let Some(rest) = line
                                .strip_prefix("Authorization: ")
                                .or_else(|| line.strip_prefix("authorization: "))
                            {
                                auth_header = Some(rest.trim().to_string());
                            }
                        }
                        body = body_part.to_string();
                    }

                    // The accept loop also receives the shutdown
                    // self-connect, which sends nothing. Bail
                    // before recording / responding so the shutdown
                    // is a no-op.
                    if method.is_empty() {
                        continue;
                    }

                    captured_clone.lock().unwrap().push(CapturedRequest {
                        method,
                        path,
                        body,
                        auth_header,
                    });

                    let (status, body_text) = &response;
                    let reason = match *status {
                        200 => "OK",
                        201 => "Created",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        404 => "Not Found",
                        422 => "Unprocessable Entity",
                        500 => "Internal Server Error",
                        502 => "Bad Gateway",
                        _ => "OK",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body_text.len(),
                        body_text
                    );
                    let _ = s.write_all(resp.as_bytes());
                    let _ = s.flush();
                }
            }
        });

        // Wait for the listener to actually be ready (i.e. accepting
        // connections — the listener is nonblocking, so accept() may
        // return WouldBlock immediately after bind; the connect probe
        // also serves as a smoke test that the spawned thread is up).
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        Self {
            base_url: format!("http://{addr}"),
            captured,
            shutdown,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for MockGithub {
    fn drop(&mut self) {
        // Flag the accept loop to stop on its next 20ms WouldBlock
        // tick. The thread polls the flag at every iteration so the
        // worst-case shutdown latency is one tick (<25ms). We then
        // join so the listener's fd closes before the test moves on
        // (otherwise a fast next-test could reuse the same port
        // mid-shutdown and confuse the dashboard's reqwest client).
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Initialise a bare git repo at `path` so the runner-clone dispatch
/// (which shells out to `git clone <url>`) has something real to
/// clone. Returns the `file://` URL the mock GitHub server returns
/// as `clone_url`.
fn seed_bare_origin(path: &Path) -> String {
    let init = Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(path)
        .output()
        .expect("spawn git init --bare");
    assert!(
        init.status.success(),
        "git init --bare: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    format!("file://{}", path.to_string_lossy())
}

/// Acceptance criterion for Phase 2.3: a `repo`-scoped GitHub PAT
/// credential + `POST /api/projects { mode: "create", name:
/// "my-new-repo", host: "github", private: true }` returns 201 once
/// the (mock) repo exists; the project row carries the clone URL;
/// the runner clone RPC fires (we observe the `project.clone` audit
/// row landing within 5s).
#[test]
fn create_mode_happy_path_against_mock_github_returns_201_and_fires_clone() {
    // Seed a bare git repo so the background clone dispatch can succeed.
    let bare_dir = tempfile::TempDir::new().expect("bare tempdir");
    let bare_path = bare_dir.path().join("origin.git");
    let clone_url = seed_bare_origin(&bare_path);

    // Mock GitHub returns the canonical happy-path response shape.
    let resp_body = serde_json::json!({
        "clone_url": clone_url,
        "html_url": "https://github.example.test/me/my-new-repo",
        "ssh_url": format!("git@github.example.test:me/my-new-repo.git"),
    })
    .to_string();
    let gh = MockGithub::spawn((201, resp_body));

    // Spawn the dashboard with BRANCHWORK_GITHUB_API_BASE pointing at
    // the mock. The base URL does not include `/repos` — github.rs
    // appends the path itself.
    let d = TestDashboard::new_with_env(&[("BRANCHWORK_GITHUB_API_BASE", gh.base_url())]);
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    // Create a gh_pat credential (the secret is the PAT the mock will
    // see in the Authorization header).
    let (cred_status, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "my GitHub PAT",
            "kind": "gh_pat",
            "secret": "ghp_test_pat_token",
            "scopes": "repo",
        }),
    );
    assert_eq!(cred_status, 201, "credential: {cred_body}");
    let credential_id = cred_body["id"].as_str().expect("credential id").to_string();

    // Issue the create-mode POST.
    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "my-new-repo",
            "host": "github",
            "private": true,
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 201, "body: {body}");
    assert_eq!(body["name"].as_str(), Some("my-new-repo"));
    assert_eq!(body["host"].as_str(), Some("github"));
    assert_eq!(
        body["repoUrl"].as_str(),
        Some(clone_url.as_str()),
        "project row must carry the host-returned clone URL"
    );
    let project_id = body["id"].as_str().expect("id").to_string();

    // Verify the mock saw the right request.
    let captured = gh.captured();
    assert!(
        !captured.is_empty(),
        "mock GitHub should have received a request"
    );
    let req = &captured[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/user/repos"); // no owner → /user/repos
    assert_eq!(
        req.auth_header.as_deref(),
        Some("Bearer ghp_test_pat_token"),
        "Authorization header must carry the decrypted PAT"
    );
    let req_body: Value = serde_json::from_str(&req.body).expect("mock saw JSON body");
    assert_eq!(req_body["name"].as_str(), Some("my-new-repo"));
    assert_eq!(req_body["private"].as_bool(), Some(true));

    // Poll the audit log for the `project.clone` row that the
    // background clone dispatch writes after the runner RPC fires.
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut clone_audit: Option<(String, String)> = None;
    while Instant::now() < deadline {
        let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
        if let Ok((action, diff)) = conn.query_row(
            "SELECT action, diff FROM audit_logs \
              WHERE resource_id = ?1 AND action = 'project.clone' \
              ORDER BY id DESC LIMIT 1",
            rusqlite::params![project_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            clone_audit = Some((action, diff));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (action, diff) =
        clone_audit.expect("project.clone audit row should fire within 5s of the 201 response");
    assert_eq!(action, "project.clone");
    assert!(diff.contains("\"trigger\":\"create_mode\""), "diff: {diff}");
}

#[test]
fn create_mode_with_owner_targets_orgs_path() {
    let bare_dir = tempfile::TempDir::new().expect("bare tempdir");
    let bare_path = bare_dir.path().join("origin.git");
    let clone_url = seed_bare_origin(&bare_path);

    let resp_body = serde_json::json!({
        "clone_url": clone_url,
        "html_url": "https://github.example.test/acme/lib",
        "ssh_url": "git@github.example.test:acme/lib.git",
    })
    .to_string();
    let gh = MockGithub::spawn((201, resp_body));

    let d = TestDashboard::new_with_env(&[("BRANCHWORK_GITHUB_API_BASE", gh.base_url())]);
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (_, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "PAT",
            "kind": "gh_pat",
            "secret": "ghp_token",
        }),
    );
    let credential_id = cred_body["id"].as_str().unwrap().to_string();

    let (status, _) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "lib",
            "host": "github",
            "owner": "acme",
            "private": false,
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 201);

    let captured = gh.captured();
    assert_eq!(captured[0].path, "/orgs/acme/repos");
    let body: Value = serde_json::from_str(&captured[0].body).unwrap();
    assert_eq!(body["private"].as_bool(), Some(false));
}

#[test]
fn create_mode_upstream_422_returns_structured_error_and_no_project_row() {
    let resp_body = serde_json::json!({
        "message": "Repository creation failed.",
        "errors": [{"resource": "Repository", "code": "custom", "message": "name already exists on this account"}],
    })
    .to_string();
    let gh = MockGithub::spawn((422, resp_body));

    let d = TestDashboard::new_with_env(&[("BRANCHWORK_GITHUB_API_BASE", gh.base_url())]);
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (_, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "PAT",
            "kind": "gh_pat",
            "secret": "ghp_token",
        }),
    );
    let credential_id = cred_body["id"].as_str().unwrap().to_string();

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "already-taken",
            "host": "github",
            "private": true,
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 422, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("host_api_failed"));
    assert_eq!(body["code"].as_str(), Some("github_422"));
    assert_eq!(
        body["message"].as_str(),
        Some("Repository creation failed.")
    );
    assert!(
        body["host_response_excerpt"]
            .as_str()
            .unwrap_or("")
            .contains("Repository creation failed."),
        "host_response_excerpt: {body}"
    );

    // No project row should have been inserted.
    let db_path = d.dir.path().join(".claude").join("branchwork.db");
    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM projects WHERE name = ?1",
            rusqlite::params!["already-taken"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "no project row should exist on host-API failure");
}

#[test]
fn create_mode_without_host_returns_400_host_required() {
    // No mock — we never reach the host API for this validation path.
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (_, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "PAT",
            "kind": "gh_pat",
            "secret": "ghp_token",
        }),
    );
    let credential_id = cred_body["id"].as_str().unwrap().to_string();

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "x",
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("host_required"));
}

#[test]
fn create_mode_without_credential_returns_400_credential_required() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "x",
            "host": "github",
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("credential_required"));
}

#[test]
fn create_mode_with_unsupported_host_returns_400() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (_, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "PAT",
            "kind": "gh_pat",
            "secret": "ghp_token",
        }),
    );
    let credential_id = cred_body["id"].as_str().unwrap().to_string();

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "x",
            "host": "bitbucket",
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("unsupported_host_for_create"));
}

#[test]
fn create_mode_kind_mismatch_returns_400() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    // Create a gitlab_pat (wrong shape for host=github).
    let (_, cred_body) = post_authed(
        &d,
        &jar,
        "/api/credentials",
        json!({
            "name": "Gitlab PAT",
            "kind": "gitlab_pat",
            "secret": "glpat_token",
        }),
    );
    let credential_id = cred_body["id"].as_str().unwrap().to_string();

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "create",
            "name": "x",
            "host": "github",
            "credentialId": credential_id,
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("credential_kind_mismatch"));
}

#[test]
fn create_mode_invalid_mode_returns_400() {
    let d = TestDashboard::new();
    let jar = d.dir.path().join("cookies.txt");
    signup(&d, &jar, "alice@branchwork.local", "supersecret-pw");

    let (status, body) = post_authed(
        &d,
        &jar,
        "/api/projects",
        json!({
            "mode": "bogus",
            "name": "x",
            "repo_url": "https://example.com/x.git",
        }),
    );
    assert_eq!(status, 400, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("invalid_mode"));
}
