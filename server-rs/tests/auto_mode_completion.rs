//! End-to-end coverage for the auto-mode merge-on-completion hook.
//!
//! The full PTY exit path requires a real `claude` binary on PATH (which
//! CI doesn't have), so we drive the public surface that the hook would
//! otherwise reach: the merge endpoint that the helper invokes, plus
//! `GET /api/plans/:n/config` to read the resulting `pausedReason`. The
//! exhaustive helper-level coverage lives inline in
//! `server-rs/src/auto_mode.rs::tests`; this file is the cross-process
//! confirmation that `auto_mode` is wired into the binary.

mod support;

use serde_json::json;
use support::TestDashboard;

fn minimal_plan(name: &str, project_dir: &std::path::Path) -> String {
    format!(
        "title: {name}\ncontext: ''\nproject: {project}\nphases:\n  - number: 1\n    title: Phase 1\n    description: ''\n    tasks:\n      - number: '1.1'\n        title: Task 1.1\n        description: ''\n        acceptance: ''\n",
        name = name,
        project = project_dir.display()
    )
}

fn seed_completed_agent(d: &TestDashboard, id: &str, plan: &str, task: &str, branch: &str) {
    let conn = rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap();
    conn.execute(
        "INSERT INTO agents \
            (id, session_id, cwd, status, mode, plan_name, task_id, branch, \
             source_branch, org_id) \
         VALUES (?1, ?1, ?2, 'completed', 'pty', ?3, ?4, ?5, 'master', 'default-org')",
        rusqlite::params![id, d.project.to_string_lossy(), plan, task, branch],
    )
    .unwrap();
}

/// Round-trip: enable auto_mode on a plan, then verify GET /config
/// surfaces the opt-in. This pins the public-surface path the auto-mode
/// hook reads (`db::auto_mode_enabled`) — if PUT fails or the GET shape
/// drifts, the hook silently no-ops in production and the helper-level
/// tests in src/auto_mode.rs would not catch it.
#[test]
fn auto_mode_can_be_enabled_via_plan_config_endpoint() {
    let d = TestDashboard::new();
    d.create_plan("am-toggle", &minimal_plan("am-toggle", &d.project));

    let (s, body) = d.put("/api/plans/am-toggle/config", json!({"autoMode": true}));
    assert_eq!(s, 200, "body: {body}");
    assert_eq!(body["autoMode"], true);

    // GET reflects the persisted state — what the auto-mode hook reads.
    let (s, body) = d.get("/api/plans/am-toggle/config");
    assert_eq!(s, 200);
    assert_eq!(body["autoMode"], true);
    assert!(body["pausedReason"].is_null());
}

/// The merge endpoint the auto-mode hook calls into. Drives the same
/// happy path the hook would after a clean PTY exit: real task branch
/// with commits, default-branch merge, 200. The hook's broadcast +
/// audit-log behaviour around this call is covered by the inline
/// helper tests; this is the wire-shape sanity check.
#[test]
fn auto_mode_hook_target_endpoint_merges_real_task_branch() {
    let d = TestDashboard::new();
    d.create_plan("am-merge", &minimal_plan("am-merge", &d.project));

    let br = "branchwork/am-merge/1.1";
    d.create_task_branch(br, /* with_commit */ true);
    seed_completed_agent(&d, "agent-merge", "am-merge", "1.1", br);

    // Enable auto_mode via the public endpoint — defense in depth: a
    // future change that quietly makes auto_mode enable a stricter merge
    // path would fail this assertion.
    let (s, _) = d.put("/api/plans/am-merge/config", json!({"autoMode": true}));
    assert_eq!(s, 200);

    let (s, body) = d.post("/api/agents/agent-merge/merge", json!({}));
    assert_eq!(s, 200, "expected 200, got {s}: {body}");
    assert_eq!(body["ok"], true);

    // Plan must not have been auto-paused on a successful merge.
    let (_, cfg) = d.get("/api/plans/am-merge/config");
    assert!(
        cfg["pausedReason"].is_null(),
        "plan should not be paused after a clean merge: {cfg}"
    );
}

/// Mirror of the inline `standalone_no_commit_pauses_*` test, but
/// driving via the public endpoint. The auto-mode hook in production
/// would observe the same 409 from the merge dispatcher and translate
/// it into a `merge_failed:` pause reason.
#[test]
fn merge_endpoint_surfaces_409_for_no_commit_branch() {
    let d = TestDashboard::new();
    d.create_plan("am-empty", &minimal_plan("am-empty", &d.project));

    let br = "branchwork/am-empty/1.1";
    // Branch with NO commit ahead of master — the unattended-contract
    // violation the auto-mode hook records as merge_failed.
    d.create_task_branch(br, /* with_commit */ false);
    seed_completed_agent(&d, "agent-empty", "am-empty", "1.1", br);

    let (s, body) = d.put("/api/plans/am-empty/config", json!({"autoMode": true}));
    assert_eq!(s, 200, "body: {body}");

    let (s, body) = d.post("/api/agents/agent-empty/merge", json!({}));
    assert_eq!(s, 409, "expected 409, got {s}: {body}");
    let msg = body["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("no commits"),
        "expected 'no commits' in error: {msg}"
    );
}
