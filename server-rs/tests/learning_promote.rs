//! Integration test for the "promote learning to CLAUDE.md" flow
//! (Phase 4, Task 4.2 of `learning-hub-ci-failure-capture`).
//!
//! Headline acceptance: clicking Promote on a candidate opens a real PR
//! (with the rule body wrapped in proper headings) and the learning's
//! stored row updates with the PR URL.
//!
//! The test stands up a path-aware mock GitHub REST API and points the
//! spawned `branchwork-server` at it via `BRANCHWORK_GITHUB_API_BASE`.
//! The promote flow is pure-REST (no clone): GET repo → GET ref → GET
//! contents (404 = create new CLAUDE.md) → create branch → PUT file →
//! POST pull. The mock returns canned responses for each leg and records
//! every request so we can assert the PR was actually opened.
//!
//! Seeding: a user signs up (cookie jar) and creates a `gh_pat`
//! credential via the real API (so it is sealed by the server's master
//! key and owned by the caller). The learning + promotion_candidate +
//! project rows are seeded directly into SQLite (all in `default-org`,
//! the active org the auth middleware resolves to). The project's
//! `default_credential_id` points at the created credential.

mod support;

use rusqlite::Connection;
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

// ── curl-with-cookie-jar helpers (mirror projects_create_mode.rs) ───────────

fn signup(d: &TestDashboard, jar: &Path, email: &str, password: &str) -> Value {
    let body = json!({ "email": email, "password": password });
    let (status, value) = http_jar(
        "POST",
        &format!("{}/api/auth/signup", d.base_url),
        Some(body),
        Some(jar),
    );
    assert_eq!(status, 201, "signup failed: status={status} body={value}");
    value
}

fn http_jar(method: &str, url: &str, body: Option<Value>, jar: Option<&Path>) -> (u16, Value) {
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

fn db_conn(d: &TestDashboard) -> Connection {
    Connection::open(d.dir.path().join(".claude/branchwork.db")).expect("open test db")
}

/// Seed a learning + its promotion_candidate row directly. Both land in
/// `default-org` (the active org the cookie's auth resolves to).
fn seed_candidate(d: &TestDashboard, learning_id: &str, slug: &str) {
    let conn = db_conn(d);
    conn.execute(
        "INSERT INTO learnings (id, org_id, kind, category, slug, body_md) \
         VALUES (?1, 'default-org', 'feedback', 'ci', ?2, ?3)",
        rusqlite::params![
            learning_id,
            slug,
            "**Why:** prod recreates kill the WS connection.\n\n**How to apply:** verify the runner is back online after every Hetzner deploy.",
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO promotion_candidates \
            (learning_id, org_id, hit_count, threshold, window_days) \
         VALUES (?1, 'default-org', 6, 5, 30)",
        rusqlite::params![learning_id],
    )
    .unwrap();
}

fn seed_project(d: &TestDashboard, id: &str, repo_url: &str, default_cred: &str) {
    let conn = db_conn(d);
    conn.execute(
        "INSERT INTO projects \
            (id, name, repo_url, host, default_credential_id, workspace_path, org_id) \
         VALUES (?1, ?1, ?2, 'github', ?3, '/tmp/x', 'default-org')",
        rusqlite::params![id, repo_url, default_cred],
    )
    .unwrap();
}

fn promoted_pr_url(d: &TestDashboard, learning_id: &str) -> Option<String> {
    let conn = db_conn(d);
    conn.query_row(
        "SELECT promoted_pr_url FROM promotion_candidates WHERE learning_id = ?1",
        rusqlite::params![learning_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .unwrap()
}

// ── path-aware mock GitHub REST API ─────────────────────────────────────────

#[derive(Debug, Default, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    body: String,
}

struct MockGithub {
    base_url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    pr_html_url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockGithub {
    fn spawn(pr_html_url: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock github");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let pr_url = pr_html_url.to_string();

        let handle = std::thread::spawn(move || {
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }
                let mut s = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));

                let mut buf = Vec::with_capacity(4096);
                let mut tmp = [0u8; 1024];
                let mut content_length: usize = 0;
                let mut headers_done = false;
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
                            let header_str = String::from_utf8_lossy(&buf[..idx]);
                            for line in header_str.split("\r\n") {
                                if let Some(rest) = line
                                    .strip_prefix("Content-Length: ")
                                    .or_else(|| line.strip_prefix("content-length: "))
                                {
                                    content_length = rest.trim().parse().unwrap_or(0);
                                }
                            }
                            if buf.len() - (idx + 4) >= content_length {
                                break;
                            }
                        }
                    } else if let Some(idx) = find_subslice(&buf, b"\r\n\r\n")
                        && buf.len() - (idx + 4) >= content_length
                    {
                        break;
                    }
                }

                let raw = String::from_utf8_lossy(&buf);
                let mut method = String::new();
                let mut path = String::new();
                let mut body = String::new();
                if let Some((header_part, body_part)) = raw.split_once("\r\n\r\n") {
                    if let Some(req_line) = header_part.split("\r\n").next() {
                        let mut parts = req_line.split_whitespace();
                        method = parts.next().unwrap_or("").to_string();
                        path = parts.next().unwrap_or("").to_string();
                    }
                    body = body_part.to_string();
                }
                if method.is_empty() {
                    continue;
                }

                captured_clone.lock().unwrap().push(CapturedRequest {
                    method: method.clone(),
                    path: path.clone(),
                    body,
                });

                let (status, body_text) = route(&method, &path, &pr_url);
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    404 => "Not Found",
                    422 => "Unprocessable Entity",
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
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                break;
            }
        }

        MockGithub {
            base_url: format!("http://{addr}"),
            captured,
            pr_html_url: pr_html_url.to_string(),
            shutdown,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}

impl Drop for MockGithub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Nudge the accept loop awake with a throwaway connection.
        if let Ok(addr) = self.base_url.trim_start_matches("http://").parse() {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(100));
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Route a mock request to its canned response. Order matters — more
/// specific paths are checked first. The contents GET returns 404 so the
/// flow exercises the "create new CLAUDE.md" path (no base64 in-test).
fn route(method: &str, path: &str, pr_url: &str) -> (u16, String) {
    match (method, path) {
        ("POST", p) if p.contains("/pulls") => {
            (201, json!({ "html_url": pr_url, "number": 42 }).to_string())
        }
        ("POST", p) if p.contains("/git/refs") => (
            201,
            json!({ "ref": "refs/heads/branchwork/promote-learning/x" }).to_string(),
        ),
        ("PUT", p) if p.contains("/contents/") => (
            201,
            json!({ "commit": { "sha": "newcommitsha" } }).to_string(),
        ),
        ("GET", p) if p.contains("/contents/") => {
            // File does not exist yet → the flow creates a fresh CLAUDE.md.
            (404, json!({ "message": "Not Found" }).to_string())
        }
        ("GET", p) if p.contains("/git/ref/heads/") => (
            200,
            json!({ "object": { "sha": "basesha123" } }).to_string(),
        ),
        ("GET", _) => (200, json!({ "default_branch": "main" }).to_string()),
        _ => (404, json!({ "message": "unexpected" }).to_string()),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── the test ────────────────────────────────────────────────────────────────

#[test]
fn promote_opens_a_pr_and_stamps_the_learning_row() {
    let mock = MockGithub::spawn("https://github.com/testowner/testrepo/pull/42");
    let d = TestDashboard::new_with_env(&[("BRANCHWORK_GITHUB_API_BASE", &mock.base_url)]);
    let jar = d.dir.path().join("cookies.txt");

    // Sign up so we have an authenticated owner for the credential.
    signup(&d, &jar, "promoter@example.com", "hunter2hunter2");

    // Create a GitHub PAT credential via the real API (sealed by the
    // server's master key, owned by the caller).
    let (status, cred) = http_jar(
        "POST",
        &format!("{}/api/credentials", d.base_url),
        Some(json!({ "name": "test PAT", "kind": "gh_pat", "secret": "ghp_testtoken" })),
        Some(&jar),
    );
    assert_eq!(status, 201, "credential create failed: {cred}");
    let cred_id = cred["id"].as_str().expect("credential id").to_string();

    // Seed the project (github host, repo URL, default credential) + the
    // learning + its promotion candidate.
    seed_project(
        &d,
        "proj-1",
        "https://github.com/testowner/testrepo.git",
        &cred_id,
    );
    seed_candidate(&d, "learning-abc", "check-runner-after-deploy");

    // The candidate shows up in the list with a diff preview + no PR yet.
    let (status, list) = http_jar(
        "GET",
        &format!("{}/api/learnings/promotion-candidates", d.base_url),
        None,
        Some(&jar),
    );
    assert_eq!(status, 200, "candidates list: {list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["learningId"], "learning-abc");
    assert!(items[0]["promotedPrUrl"].is_null());
    let preview = items[0]["appendPreview"].as_str().unwrap();
    assert!(
        preview.starts_with("### Check runner after deploy"),
        "preview wraps the body in an H3 heading: {preview}"
    );
    assert!(preview.contains("**Why:** prod recreates"));

    // Promote → opens the PR + stamps the row.
    let (status, body) = http_jar(
        "POST",
        &format!("{}/api/learnings/learning-abc/promote", d.base_url),
        Some(json!({ "projectId": "proj-1" })),
        Some(&jar),
    );
    assert_eq!(status, 200, "promote failed: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["prUrl"], mock.pr_html_url);
    assert_eq!(body["prNumber"], 42);

    // The stored candidate row carries the PR URL (acceptance criterion).
    assert_eq!(
        promoted_pr_url(&d, "learning-abc").as_deref(),
        Some(mock.pr_html_url.as_str())
    );

    // A real PR was opened: the mock saw the full 6-call flow ending in a
    // POST to /pulls.
    let reqs = mock.requests();
    assert!(
        reqs.iter()
            .any(|r| r.method == "POST" && r.path.contains("/pulls")),
        "mock must have received a POST /pulls: {reqs:?}"
    );
    let put = reqs
        .iter()
        .find(|r| r.method == "PUT" && r.path.contains("/contents/CLAUDE.md"))
        .expect("a PUT to contents/CLAUDE.md must have fired");
    // The committed file content is base64 in the PUT body — we don't
    // decode it here (no base64 dev-dep), but the branch + commit message
    // travel as plain JSON and prove the rule is being committed.
    assert!(
        put.body.contains("branchwork/promote-learning/"),
        "PUT targets the promotion branch: {}",
        put.body
    );

    // The candidates list now reflects the promotion.
    let (_s, list) = http_jar(
        "GET",
        &format!("{}/api/learnings/promotion-candidates", d.base_url),
        None,
        Some(&jar),
    );
    assert_eq!(
        list["items"][0]["promotedPrUrl"], mock.pr_html_url,
        "promoted candidate now carries the PR url in the list"
    );

    // Promoting again is refused (409) with the existing URL.
    let (status, body) = http_jar(
        "POST",
        &format!("{}/api/learnings/learning-abc/promote", d.base_url),
        Some(json!({ "projectId": "proj-1" })),
        Some(&jar),
    );
    assert_eq!(status, 409, "second promote must 409: {body}");
    assert_eq!(body["error"], "already_promoted");
    assert_eq!(body["prUrl"], mock.pr_html_url);
}
