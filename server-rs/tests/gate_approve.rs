//! End-to-end tests for `POST /api/plans/:name/gates/:node_id/approve`
//! (Phase 3 of the dag-based-plan-model plan, Task 3.3).
//!
//! Pins the gate approval HTTP surface + its WS-event side effects:
//!
//! - approving a blocked `init`/`approval` gate → 200, the node's
//!   `node_status` flips to `completed`, a `gate_approvals` row is written,
//!   and the `gate_approved` + `gate_status_changed` WS events fire (the
//!   headline Task 3.3 acceptance: gate approval fires a WS event the
//!   dashboard receives).
//! - the downstream node the gate was blocking becomes ready (claimed →
//!   `in_progress`) once auto-advance is on (parent Task 3.2 acceptance).
//! - unknown node → 404 `gate_not_found`; a task node → 409 `not_a_gate`;
//!   a double-approve is an idempotent 200.
//!
//! The per-transition `gate_status_changed` from the scheduler and the
//! `gate_awaiting_approval` payload are unit-tested in
//! `dag_scheduler.rs::tests` / `gates.rs::tests`; this file pins the
//! approve endpoint end-to-end through the real server + WS broadcast.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use support::TestDashboard;

/// A v2 (DAG) plan with an `init` gate and a task gated behind it. The
/// project dir is a non-existent path under the server's HOME so the
/// downstream task's worktree setup fails fast — but the node claim (the
/// signal we assert) still lands.
fn dag_plan_with_gate(name: &str) -> String {
    // Raw string with literal column-0 indentation — a `\<newline>` format!
    // continuation would strip the leading whitespace and flatten the YAML.
    format!(
        r#"schema_version: 2
title: "{name} title"
project: branchwork-no-such-dir-{name}
nodes:
  - id: init
    type: gate
    title: "Precondition gate"
    gate_kind: init
  - id: a
    type: task
    title: "After the gate"
    depends_on: [init]
"#
    )
}

fn db_conn(d: &TestDashboard) -> rusqlite::Connection {
    rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap()
}

/// Enable plan-level auto-advance so the post-approve `try_dag_advance`
/// actually schedules the downstream node.
fn enable_auto_advance(d: &TestDashboard, plan: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1) \
             ON CONFLICT(plan_name) DO UPDATE SET enabled = 1",
            rusqlite::params![plan],
        )
        .unwrap();
}

fn node_status(d: &TestDashboard, plan: &str, node_id: &str) -> Option<String> {
    db_conn(d)
        .query_row(
            "SELECT status FROM node_status WHERE plan_name = ?1 AND node_id = ?2",
            rusqlite::params![plan, node_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
}

fn gate_approval_count(d: &TestDashboard, plan: &str, node_id: &str) -> i64 {
    db_conn(d)
        .query_row(
            "SELECT COUNT(*) FROM gate_approvals WHERE plan_name = ?1 AND node_id = ?2",
            rusqlite::params![plan, node_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// Subscribe to the dashboard WS in a background thread, accumulating every
/// text frame into `events`. Returns once the socket reports `connected`
/// (so the caller can act knowing no broadcast will be missed).
fn spawn_ws_collector(ws_url: String) -> (Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let connected = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (ev, conn, stp) = (events.clone(), connected.clone(), stop.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for WS collector");
        rt.block_on(async move {
            use futures_util::StreamExt;
            use tokio_tungstenite::tungstenite::Message;
            let (ws, _resp) = match tokio_tungstenite::connect_async(&ws_url).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[ws collector] connect failed: {e}");
                    return;
                }
            };
            let (_write, mut read) = ws.split();
            conn.store(true, Ordering::SeqCst);
            while !stp.load(Ordering::SeqCst) {
                match tokio::time::timeout(Duration::from_millis(100), read.next()).await {
                    Ok(Some(Ok(Message::Text(t)))) => ev.lock().unwrap().push(t.to_string()),
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(_))) | Ok(None) => break,
                    Err(_) => continue,
                }
            }
        });
    });
    // Wait until the collector is actually subscribed so the broadcast
    // can't race ahead of the WS handshake.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !connected.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        connected.load(Ordering::SeqCst),
        "WS collector never connected"
    );
    (events, stop)
}

/// Poll `events` for a frame whose `type` matches `event_type` (the wire
/// envelope is `{type, data, timestamp}`), up to `timeout`.
fn wait_for_event(events: &Arc<Mutex<Vec<String>>>, event_type: &str, timeout: Duration) -> Value {
    let needle = format!("\"type\":\"{event_type}\"");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        {
            let frames = events.lock().unwrap();
            if let Some(frame) = frames.iter().find(|f| f.contains(&needle)) {
                return serde_json::from_str(frame).unwrap();
            }
        }
        if std::time::Instant::now() >= deadline {
            let frames = events.lock().unwrap();
            panic!("no `{event_type}` frame within {timeout:?}; saw {frames:?}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn approve_init_gate_completes_node_and_fires_ws_events() {
    let d = TestDashboard::new();
    let plan = "gate-approve-init";
    std::fs::write(
        d.plans_dir.join(format!("{plan}.yaml")),
        dag_plan_with_gate(plan),
    )
    .unwrap();
    enable_auto_advance(&d, plan);

    // Subscribe to the WS before approving so we can't miss the broadcast.
    let (events, stop) = spawn_ws_collector(d.ws_url());

    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/init/approve"), json!({}));
    assert_eq!(code, 200, "approve must succeed, got {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["nodeId"], "init");
    assert_eq!(body["gateKind"], "init");
    assert_eq!(body["status"], "completed");

    // The gate node flipped to completed + the approval row is recorded —
    // both happen synchronously in the handler before the 200.
    assert_eq!(node_status(&d, plan, "init").as_deref(), Some("completed"));
    assert_eq!(gate_approval_count(&d, plan, "init"), 1);

    // Headline acceptance: the dashboard receives a `gate_approved` WS event
    // (and the paired `gate_status_changed` carrying the new status + kind).
    let approved = wait_for_event(&events, "gate_approved", Duration::from_secs(3));
    assert_eq!(approved["data"]["plan_name"], plan);
    assert_eq!(approved["data"]["node_id"], "init");

    let status_changed = wait_for_event(&events, "gate_status_changed", Duration::from_secs(3));
    assert_eq!(status_changed["data"]["node_id"], "init");
    assert_eq!(status_changed["data"]["status"], "completed");
    assert_eq!(status_changed["data"]["gate_kind"], "init");

    // Parent T3.2 acceptance: the downstream task the gate was blocking
    // becomes ready (claimed → in_progress) by the detached try_dag_advance.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node_status(&d, plan, "a").as_deref() != Some("in_progress")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        node_status(&d, plan, "a").as_deref(),
        Some("in_progress"),
        "downstream node must become ready after the gate is approved"
    );

    stop.store(true, Ordering::SeqCst);
}

#[test]
fn approve_unknown_node_returns_404() {
    let d = TestDashboard::new();
    let plan = "gate-approve-404";
    std::fs::write(
        d.plans_dir.join(format!("{plan}.yaml")),
        dag_plan_with_gate(plan),
    )
    .unwrap();

    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/nope/approve"), json!({}));
    assert_eq!(code, 404, "unknown node must 404, got {body}");
    assert_eq!(body["error"], "gate_not_found");
}

#[test]
fn approve_task_node_returns_409_not_a_gate() {
    let d = TestDashboard::new();
    let plan = "gate-approve-not-gate";
    std::fs::write(
        d.plans_dir.join(format!("{plan}.yaml")),
        dag_plan_with_gate(plan),
    )
    .unwrap();

    // `a` is a task node, not a gate.
    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/a/approve"), json!({}));
    assert_eq!(code, 409, "approving a task node must 409, got {body}");
    assert_eq!(body["error"], "not_a_gate");
}

#[test]
fn approve_is_idempotent() {
    let d = TestDashboard::new();
    let plan = "gate-approve-idem";
    std::fs::write(
        d.plans_dir.join(format!("{plan}.yaml")),
        dag_plan_with_gate(plan),
    )
    .unwrap();

    let (code1, _) = d.post(&format!("/api/plans/{plan}/gates/init/approve"), json!({}));
    assert_eq!(code1, 200);
    let (code2, body2) = d.post(&format!("/api/plans/{plan}/gates/init/approve"), json!({}));
    assert_eq!(
        code2, 200,
        "a second approve must be a harmless 200, got {body2}"
    );

    // Exactly one approval row survives (ON CONFLICT DO NOTHING), node is
    // still completed.
    assert_eq!(gate_approval_count(&d, plan, "init"), 1);
    assert_eq!(node_status(&d, plan, "init").as_deref(), Some("completed"));
}
