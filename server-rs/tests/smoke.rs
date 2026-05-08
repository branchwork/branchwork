//! Smoke test for the scratch-repo harness.
//!
//! Acceptance criterion for dashboard-stability T3.1: spawn a scratch
//! `branchwork-server` against a clean tempdir, hit `/api/health`, and
//! return cleanly. If this regresses, every other test in this binary
//! will fail in the same way — keeping the smallest possible probe at
//! the top of the file makes a broken harness visible as a single
//! red test instead of a wall of noise.

mod support;

use support::TestDashboard;

#[test]
fn scratch_server_responds_healthy() {
    let d = TestDashboard::new();
    let (status, body) = d.get("/api/health");
    assert_eq!(status, 200, "GET /api/health: {body}");
    // The `/api/health` body shape is asserted lightly: the freshness
    // canary is the 200, not the field set. Tighter contract belongs
    // in the boot-time normalization tests (Phase 2).
    assert!(
        body.is_object() || body.is_string(),
        "unexpected body shape: {body}"
    );
}

#[test]
fn scratch_dashboard_exposes_project_paths() {
    let d = TestDashboard::new();
    assert!(d.project.is_dir(), "project dir should exist");
    assert!(d.plans_dir.is_dir(), "plans dir should exist");
    assert!(
        d.project.join(".git").is_dir(),
        "project should be a git repo"
    );
    assert!(
        d.project.join("README.md").exists(),
        "initial commit should have committed README"
    );
    let ws = d.ws_url();
    assert!(
        ws.starts_with("ws://127.0.0.1:") && ws.ends_with("/ws"),
        "ws_url should point at the dashboard WS path: {ws}"
    );
}

#[test]
fn scratch_dashboard_without_gh_gate_skips_repo() {
    // The default test environment is not opted into gh-backed tests
    // (no `BRANCHWORK_TEST_GH_REPOS=1`), so `with_github_repo` must
    // return `None` — and importantly must NOT shell out to gh or
    // create real repos under the user's account.
    //
    // We deliberately scrub the gate inside this test in case a
    // developer left it set in their shell — the harness must keep
    // the default path inert regardless of ambient env.
    // SAFETY: tests in the same integration binary share a process, so
    // unsetting the env var here will affect concurrent tests. Two
    // mitigations: (1) no other test in this file reads the var; (2) we
    // only unset, never set, so callers that opted in see their setting
    // disappear once and reappear on the next process run — acceptable
    // for an opt-in side-effect gate.
    unsafe {
        std::env::remove_var("BRANCHWORK_TEST_GH_REPOS");
    }
    let mut d = TestDashboard::new();
    let repo = d.with_github_repo();
    assert!(
        repo.is_none(),
        "harness must default to no GitHub repo when the gate is off"
    );
}
