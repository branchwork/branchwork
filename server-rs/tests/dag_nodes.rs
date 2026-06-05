//! End-to-end tests for the DAG board's server surface (Task 3.4 of the
//! dag-based-plan-model plan).
//!
//! Two pieces make the dashboard able to render gate cards:
//!
//! - `GET /api/plans/:name` attaches a `nodes` array for `schema_version: 2`
//!   plans (the phases-based `ParsedPlan` drops gate nodes entirely), with
//!   each node's runtime `status` merged from `node_status` and defaulting to
//!   `pending`. v1 plans omit `nodes`.
//! - `POST /api/plans/:name/gates/:node_id/retry` resets a *failed* gate to
//!   `pending`, audits the action, broadcasts `gate_status_changed`, and
//!   re-enters the DAG scheduler — the "Retry" button on a failed gate card.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use support::TestDashboard;

/// A v2 (DAG) plan: init gate → task → end gate. Project dir is a
/// non-existent path under the server's HOME so nothing actually runs.
fn dag_plan(name: &str) -> String {
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
    checks: [git_repo, clean_tree]
  - id: a
    type: task
    title: "After the gate"
    depends_on: [init]
  - id: end
    type: gate
    title: "Final verification"
    gate_kind: end
    depends_on: [a]
    checks: [all_merged, ci_green]
"#
    )
}

/// A legacy v1 (phase-based) plan — must NOT carry a `nodes` field.
fn v1_plan(name: &str) -> String {
    format!(
        r#"title: "{name} title"
project: branchwork-no-such-dir-{name}
phases:
  - number: 1
    title: "Phase 1"
    tasks:
      - number: "1.1"
        title: "Do a thing"
"#
    )
}

fn db_conn(d: &TestDashboard) -> rusqlite::Connection {
    rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap()
}

fn set_node_status(d: &TestDashboard, plan: &str, node_id: &str, status: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO node_status (plan_name, node_id, status, source, updated_at) \
             VALUES (?1, ?2, ?3, 'manual', datetime('now')) \
             ON CONFLICT(plan_name, node_id) \
             DO UPDATE SET status = excluded.status",
            rusqlite::params![plan, node_id, status],
        )
        .unwrap();
}

fn set_gate_checks(d: &TestDashboard, plan: &str, node_id: &str, results_json: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO gate_checks (plan_name, node_id, results_json, updated_at) \
             VALUES (?1, ?2, ?3, datetime('now')) \
             ON CONFLICT(plan_name, node_id) \
             DO UPDATE SET results_json = excluded.results_json",
            rusqlite::params![plan, node_id, results_json],
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

fn audit_count(d: &TestDashboard, action: &str, plan: &str) -> i64 {
    db_conn(d)
        .query_row(
            "SELECT COUNT(*) FROM audit_logs WHERE action = ?1 AND resource_id = ?2",
            rusqlite::params![action, plan],
            |r| r.get(0),
        )
        .unwrap()
}

fn find_node<'a>(nodes: &'a Value, id: &str) -> &'a Value {
    nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == id)
        .unwrap_or_else(|| panic!("no node '{id}' in {nodes}"))
}

// ── WS collector (copied from gate_approve.rs — separate test binary) ────────

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

// ── GET /api/plans/:name — nodes for v2 plans ────────────────────────────────

#[test]
fn get_plan_exposes_nodes_for_v2_plan_with_merged_status() {
    let d = TestDashboard::new();
    let plan = "dag-nodes-get";
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), dag_plan(plan)).unwrap();

    // Seed two node statuses; the task node `a` is left without a row so it
    // must default to `pending`.
    set_node_status(&d, plan, "init", "completed");
    set_node_status(&d, plan, "end", "failed");

    let (code, body) = d.get(&format!("/api/plans/{plan}"));
    assert_eq!(code, 200, "GET plan must succeed, got {body}");

    let nodes = &body["nodes"];
    assert!(
        nodes.is_array(),
        "v2 plan must carry a `nodes` array: {body}"
    );
    assert_eq!(nodes.as_array().unwrap().len(), 3, "init + a + end");

    let init = find_node(nodes, "init");
    assert_eq!(init["type"], "gate");
    assert_eq!(init["gateKind"], "init");
    assert_eq!(init["status"], "completed");
    // Gate carries its declared checks so the card can render them.
    assert_eq!(init["checks"], json!(["git_repo", "clean_tree"]));

    let a = find_node(nodes, "a");
    assert_eq!(a["type"], "task");
    // No node_status row → default pending (not null).
    assert_eq!(a["status"], "pending");

    let end = find_node(nodes, "end");
    assert_eq!(end["type"], "gate");
    assert_eq!(end["gateKind"], "end");
    assert_eq!(end["status"], "failed");
}

/// Task 3.6: a persisted End-gate `gate_checks` row is surfaced on the gate
/// node as `gateChecks` so the dashboard renders the per-check verdicts on
/// reload (not just from the live `gate_check_results` WS event).
#[test]
fn get_plan_exposes_persisted_gate_checks_on_the_end_gate() {
    let d = TestDashboard::new();
    let plan = "dag-gate-checks";
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), dag_plan(plan)).unwrap();
    set_node_status(&d, plan, "end", "failed");
    set_gate_checks(
        &d,
        plan,
        "end",
        r#"[{"name":"all_merged","status":"passed","detail":"2/2 branches merged"},
            {"name":"compiles","status":"failed","detail":"check 'build' failed (exit 7)","output":"COMPILE_MARKER"},
            {"name":"ci_green","status":"skipped","detail":"not run — an earlier check failed"}]"#,
    );

    let (code, body) = d.get(&format!("/api/plans/{plan}"));
    assert_eq!(code, 200, "GET plan must succeed, got {body}");

    let end = find_node(&body["nodes"], "end");
    let checks = end["gateChecks"]
        .as_array()
        .unwrap_or_else(|| panic!("end gate must carry gateChecks: {end}"));
    assert_eq!(checks.len(), 3);
    assert_eq!(checks[0]["name"], "all_merged");
    assert_eq!(checks[0]["status"], "passed");
    assert_eq!(checks[0]["detail"], "2/2 branches merged");
    assert_eq!(checks[1]["name"], "compiles");
    assert_eq!(checks[1]["status"], "failed");
    // Acceptance: the failure output is visible on reload.
    assert_eq!(checks[1]["output"], "COMPILE_MARKER");

    // The init gate (no gate_checks row) carries no gateChecks field.
    let init = find_node(&body["nodes"], "init");
    assert!(
        init.get("gateChecks").is_none(),
        "init gate without a gate_checks row must not carry gateChecks: {init}"
    );
}

#[test]
fn get_plan_omits_nodes_for_v1_plan() {
    let d = TestDashboard::new();
    let plan = "v1-no-nodes";
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), v1_plan(plan)).unwrap();

    let (code, body) = d.get(&format!("/api/plans/{plan}"));
    assert_eq!(code, 200, "GET plan must succeed, got {body}");
    assert!(
        body.get("nodes").is_none(),
        "v1 phase-based plan must not carry a `nodes` field: {body}"
    );
    // It still serves phases as before.
    assert!(body["phases"].is_array());
}

// ── POST /api/plans/:name/gates/:node_id/retry ───────────────────────────────

#[test]
fn retry_failed_gate_resets_to_pending_and_broadcasts() {
    let d = TestDashboard::new();
    let plan = "dag-retry";
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), dag_plan(plan)).unwrap();

    // The end gate has failed; the operator clicks Retry.
    set_node_status(&d, plan, "end", "failed");

    let (events, stop) = spawn_ws_collector(d.ws_url());

    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/end/retry"), json!({}));
    assert_eq!(code, 200, "retry must succeed, got {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["nodeId"], "end");
    assert_eq!(body["gateKind"], "end");
    assert_eq!(body["status"], "pending");

    // The gate flipped back to pending synchronously before the 200.
    assert_eq!(node_status(&d, plan, "end").as_deref(), Some("pending"));

    // The action is audited.
    assert_eq!(audit_count(&d, "gate.retry", plan), 1);

    // The dashboard receives the reset so the failed card clears immediately.
    let changed = wait_for_event(&events, "gate_status_changed", Duration::from_secs(3));
    assert_eq!(changed["data"]["node_id"], "end");
    assert_eq!(changed["data"]["status"], "pending");
    assert_eq!(changed["data"]["gate_kind"], "end");

    stop.store(true, Ordering::SeqCst);
}

#[test]
fn retry_unknown_node_404s_and_task_node_409s() {
    let d = TestDashboard::new();
    let plan = "dag-retry-errs";
    std::fs::write(d.plans_dir.join(format!("{plan}.yaml")), dag_plan(plan)).unwrap();

    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/nope/retry"), json!({}));
    assert_eq!(code, 404, "unknown node id, got {body}");
    assert_eq!(body["error"], "gate_not_found");

    // `a` is a task node, not a gate.
    let (code, body) = d.post(&format!("/api/plans/{plan}/gates/a/retry"), json!({}));
    assert_eq!(code, 409, "task node is not a gate, got {body}");
    assert_eq!(body["error"], "not_a_gate");

    let (code, body) = d.post("/api/plans/no-such-plan/gates/end/retry", json!({}));
    assert_eq!(code, 404, "unknown plan, got {body}");
    assert_eq!(body["error"], "plan_not_found");
}
