//! End-to-end test for cross-plan WS events + re-evaluation (DAG-based plan
//! model, Phase 4, Task 4.2).
//!
//! Pins the headline acceptance: two plans in the same project, plan B
//! declares an input produced by plan A. When plan A completes (its End gate
//! passes and records the output), plan B's Init gate auto-unblocks.
//!
//! The full chain exercised here through the real server + its always-on
//! cross-plan listener (`artifacts::spawn_listener`):
//!
//!   POST /api/plans/a/gates/end/retry
//!     → try_dag_advance(a) executes A's End gate (vacuous pass: no agents,
//!       no workflows, no branchwork.toml)
//!     → record_and_notify_outputs records A's `schema` output + broadcasts
//!       `plan_output_produced { plan_name: a, artifact_name: schema }`
//!     → the listener reset_dependent_init_gates(a) flips B's blocked Init
//!       gate from `in_progress` back to `pending` (and re-enters the
//!       scheduler for B, a no-op here since B has no auto-advance).
//!
//! Unit coverage lives in `gates.rs::tests` (the producer-side broadcast) and
//! `artifacts.rs::tests` (the listener handler + parse); this file pins the
//! wiring end-to-end through the real broadcast channel + spawned listener.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use support::TestDashboard;

/// Producer plan A: a single End gate that declares one cross-plan output.
/// The project dir is a non-existent path under the server's HOME so the End
/// gate's checks (`all_merged` / `compiles` / `ci_green`) all pass vacuously
/// (no agents, no branchwork.toml, no .github/workflows).
fn producer_plan(name: &str, output: &str) -> String {
    format!(
        r#"schema_version: 2
title: "{name} producer"
project: branchwork-no-such-dir-{name}
outputs:
  - name: {output}
nodes:
  - id: end
    type: gate
    title: "Final verification"
    gate_kind: end
"#
    )
}

/// Consumer plan B: an Init gate gated on `from_plan`'s `artifact` output.
fn consumer_plan(name: &str, from_plan: &str, artifact: &str) -> String {
    format!(
        r#"schema_version: 2
title: "{name} consumer"
project: branchwork-no-such-dir-{name}
inputs:
  - name: {artifact}
    fromPlan: {from_plan}
nodes:
  - id: init
    type: gate
    title: "Precondition gate"
    gate_kind: init
  - id: work
    type: task
    title: "After the gate"
    depends_on: [init]
"#
    )
}

fn db_conn(d: &TestDashboard) -> rusqlite::Connection {
    rusqlite::Connection::open(d.dir.path().join(".claude/branchwork.db")).unwrap()
}

fn enable_auto_advance(d: &TestDashboard, plan: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1) \
             ON CONFLICT(plan_name) DO UPDATE SET enabled = 1",
            rusqlite::params![plan],
        )
        .unwrap();
}

fn seed_node_status(d: &TestDashboard, plan: &str, node_id: &str, status: &str) {
    db_conn(d)
        .execute(
            "INSERT INTO node_status (plan_name, node_id, status) VALUES (?1, ?2, ?3) \
             ON CONFLICT(plan_name, node_id) DO UPDATE SET status = excluded.status",
            rusqlite::params![plan, node_id, status],
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

/// Subscribe to the dashboard WS in a background thread, accumulating every
/// text frame. Returns once the socket reports `connected`. Mirrors the
/// collector in `gate_approve.rs`.
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

#[test]
fn producer_completion_auto_unblocks_consumer_init_gate() {
    let d = TestDashboard::new();
    let producer = "xplan-producer";
    let consumer = "xplan-consumer";
    std::fs::write(
        d.plans_dir.join(format!("{producer}.yaml")),
        producer_plan(producer, "schema"),
    )
    .unwrap();
    std::fs::write(
        d.plans_dir.join(format!("{consumer}.yaml")),
        consumer_plan(consumer, producer, "schema"),
    )
    .unwrap();

    // The producer auto-advances so its End gate executes on retry. The
    // consumer deliberately has NO auto-advance, so the listener's spawned
    // try_dag_advance(consumer) is a no-op — leaving the reset (in_progress →
    // pending) as the observable "unblocked" signal.
    enable_auto_advance(&d, producer);

    // The consumer's Init gate is currently blocked, waiting on the producer's
    // `schema` output (claimed → in_progress).
    seed_node_status(&d, consumer, "init", "in_progress");

    // Subscribe before triggering so we can't miss the cross-plan broadcast.
    let (events, stop) = spawn_ws_collector(d.ws_url());

    // "Plan A completes": drive its End gate via retry (re-enters
    // try_dag_advance, which claims + executes the gate). It passes vacuously
    // and records its `schema` output.
    let (code, body) = d.post(&format!("/api/plans/{producer}/gates/end/retry"), json!({}));
    assert_eq!(code, 200, "retry must succeed, got {body}");

    // The producer broadcasts the cross-plan output event.
    let produced = wait_for_event(&events, "plan_output_produced", Duration::from_secs(5));
    assert_eq!(produced["data"]["plan_name"], producer);
    assert_eq!(produced["data"]["artifact_name"], "schema");

    // Headline acceptance: the consumer's blocked Init gate auto-unblocks —
    // the listener flips it back to `pending` (ready to re-run, now past the
    // inputs check).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node_status(&d, consumer, "init").as_deref() != Some("pending")
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        node_status(&d, consumer, "init").as_deref(),
        Some("pending"),
        "consumer's Init gate must auto-unblock once the producer records its output"
    );

    // And the producer's End gate itself completed.
    assert_eq!(
        node_status(&d, producer, "end").as_deref(),
        Some("completed"),
        "producer's End gate should have passed"
    );

    stop.store(true, Ordering::SeqCst);
}

#[test]
fn unrelated_producer_output_does_not_unblock_consumer() {
    let d = TestDashboard::new();
    let producer = "xplan-other-producer";
    let consumer = "xplan-other-consumer";
    // The consumer waits on a DIFFERENT plan ("upstream"), not on `producer`.
    std::fs::write(
        d.plans_dir.join(format!("{producer}.yaml")),
        producer_plan(producer, "schema"),
    )
    .unwrap();
    std::fs::write(
        d.plans_dir.join(format!("{consumer}.yaml")),
        consumer_plan(consumer, "some-upstream-plan", "schema"),
    )
    .unwrap();
    enable_auto_advance(&d, producer);
    seed_node_status(&d, consumer, "init", "in_progress");

    let (events, stop) = spawn_ws_collector(d.ws_url());
    let (code, _) = d.post(&format!("/api/plans/{producer}/gates/end/retry"), json!({}));
    assert_eq!(code, 200);

    // The producer still broadcasts its output…
    wait_for_event(&events, "plan_output_produced", Duration::from_secs(5));
    // …but the consumer depends on a different plan, so it stays blocked.
    // Give the listener a beat to (not) act, then assert no change.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        node_status(&d, consumer, "init").as_deref(),
        Some("in_progress"),
        "a consumer of a different producer must NOT be unblocked"
    );

    stop.store(true, Ordering::SeqCst);
}
