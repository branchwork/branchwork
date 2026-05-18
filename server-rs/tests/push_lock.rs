//! E2E tests for the `/api/git/push-lock` endpoint trio. Verifies the
//! HTTP surface contract for external CI callers (auto-bump):
//!
//! - acquire → release round-trip on an uncontended lock.
//! - acquire is rejected (503) when the lock is held by a live holder
//!   and the wait_secs window expires.
//! - acquire WAITS for the holder to release and then succeeds.
//! - release with a wrong token is a no-op (still 200, but
//!   `released: false`).
//! - peek returns the live holder snapshot (200) or 404 when no row.
//!
//! These tests run the real `branchwork-server` binary; they hit the
//! HTTP surface but they DO NOT exercise an actual push — that's the
//! domain of the in-process `auto_mode::tests` and `git_helpers::tests`
//! suites. The lock is independent of the push; both can be tested
//! separately.

mod support;

use serde_json::json;
use support::TestDashboard;

#[test]
fn acquire_and_release_round_trip() {
    let d = TestDashboard::new();

    let (status, body) = d.post(
        "/api/git/push-lock",
        json!({
            "branch": "master",
            "holderKind": "ci",
            "holderMeta": "github-actions:run-42",
            "waitSecs": 1,
        }),
    );
    assert_eq!(status, 200, "acquire body: {body}");
    assert_eq!(body["branch"].as_str(), Some("master"));
    assert_eq!(body["holderKind"].as_str(), Some("ci"));
    let token = body["token"]
        .as_str()
        .expect("token must be present")
        .to_string();
    assert!(!token.is_empty(), "token must be non-empty");

    let (status, body) = d.post(
        "/api/git/push-lock/release",
        json!({"branch": "master", "token": token}),
    );
    assert_eq!(status, 200, "release body: {body}");
    assert_eq!(body["released"].as_bool(), Some(true));

    // After release the peek endpoint returns 404 (no row).
    let (status, _body) = d.get("/api/git/push-lock/master");
    assert_eq!(status, 404, "expected 404 after release");
}

#[test]
fn acquire_times_out_when_live_holder_exists() {
    let d = TestDashboard::new();

    // First caller acquires — keep its token so the lock stays held.
    let (status, body) = d.post(
        "/api/git/push-lock",
        json!({
            "branch": "master",
            "holderKind": "auto_mode",
            "holderMeta": "plan=p1",
            "waitSecs": 1,
        }),
    );
    assert_eq!(status, 200, "first acquire body: {body}");
    let _token_a = body["token"].as_str().unwrap().to_string();

    // Second caller asks for a 1-second wait; first never releases.
    // Must come back 503 with the holder snapshot.
    let started = std::time::Instant::now();
    let (status, body) = d.post(
        "/api/git/push-lock",
        json!({
            "branch": "master",
            "holderKind": "ci",
            "holderMeta": "github-actions:run-99",
            "waitSecs": 1,
        }),
    );
    let elapsed = started.elapsed();
    assert_eq!(
        status, 503,
        "expected timeout, got status {status} body {body}"
    );
    assert_eq!(body["error"].as_str(), Some("push_lock_busy"));
    assert_eq!(body["branch"].as_str(), Some("master"));
    assert_eq!(body["holder"]["holderKind"].as_str(), Some("auto_mode"));
    assert_eq!(body["holder"]["holderMeta"].as_str(), Some("plan=p1"));
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "must wait the full waitSecs before returning timeout, got {elapsed:?}"
    );
}

#[test]
fn release_with_wrong_token_returns_released_false() {
    let d = TestDashboard::new();

    let (_, body) = d.post(
        "/api/git/push-lock",
        json!({"branch": "master", "waitSecs": 1}),
    );
    assert!(body["token"].as_str().is_some());

    let (status, body) = d.post(
        "/api/git/push-lock/release",
        json!({"branch": "master", "token": "definitely-not-the-real-token"}),
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["released"].as_bool(),
        Some(false),
        "wrong token must NOT delete the row: {body}"
    );

    // The lock is still held by the original token.
    let (status, body) = d.get("/api/git/push-lock/master");
    assert_eq!(status, 200);
    assert_eq!(body["branch"].as_str(), Some("master"));
}

#[test]
fn peek_returns_404_when_no_row_exists() {
    let d = TestDashboard::new();
    let (status, body) = d.get("/api/git/push-lock/some-untouched-branch");
    assert_eq!(status, 404, "body: {body}");
    assert_eq!(body["error"].as_str(), Some("no_lock"));
}

/// The headline acceptance test: with two acquire calls firing in the
/// same window, the second one WAITS for the first to release and then
/// succeeds. This exercises the wait_for_push_lock polling loop end to
/// end through the HTTP surface — the same code path auto-bump CI
/// would hit while auto-mode is mid-push.
#[test]
fn second_acquire_waits_for_release_then_succeeds() {
    let d = TestDashboard::new();

    // Caller A acquires.
    let (status, body) = d.post(
        "/api/git/push-lock",
        json!({"branch": "master", "holderKind": "auto_mode", "waitSecs": 1}),
    );
    assert_eq!(status, 200);
    let token_a = body["token"].as_str().unwrap().to_string();
    let base_url = d.base_url.clone();

    // Spawn a background thread that releases the lock after ~400ms.
    let release_handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        // Use a separate curl invocation to release.
        let body = serde_json::json!({"branch": "master", "token": token_a}).to_string();
        let _ = std::process::Command::new("curl")
            .args([
                "-sS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                &format!("{base_url}/api/git/push-lock/release"),
            ])
            .output();
    });

    // Caller B requests with a long wait. Should block ~400ms then
    // succeed once A releases.
    let started = std::time::Instant::now();
    let (status, body) = d.post(
        "/api/git/push-lock",
        json!({
            "branch": "master",
            "holderKind": "ci",
            "holderMeta": "run-7",
            "waitSecs": 5,
        }),
    );
    let elapsed = started.elapsed();
    release_handle.join().unwrap();

    assert_eq!(
        status, 200,
        "B should succeed after A releases. body: {body}"
    );
    assert_eq!(body["holderKind"].as_str(), Some("ci"));
    assert!(
        elapsed >= std::time::Duration::from_millis(300),
        "B should have actually waited for A — got {elapsed:?} (was the lock not held?)"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "B should NOT have waited the full timeout — got {elapsed:?}"
    );
}
