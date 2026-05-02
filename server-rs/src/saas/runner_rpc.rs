//! Request/response pattern over the runner WebSocket.
//!
//! See `architecture/protocols.md#requestresponse-frames` for the design.
//! An HTTP handler that needs a synchronous answer from the runner calls
//! [`runner_request`], which:
//!
//! 1. Picks an online runner for the org (DB ∩ in-memory registry).
//! 2. Registers a `tokio::sync::oneshot` keyed by the request's `req_id`
//!    in the runner's per-connection `pending` map.
//! 3. Sends the request frame **best-effort** (no `seq`, no outbox).
//! 4. Awaits the receiver against a short timeout.
//! 5. On timeout removes the entry; on disconnect the cleanup path in
//!    `handle_runner_ws` clears the whole map, so the receiver wakes
//!    with `Closed` and the caller observes [`RunnerRpcError::RunnerDisconnected`].
//!
//! Late replies (post-timeout or post-reconnect) find nothing waiting in
//! the map and are silently discarded by `handle_runner_message`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::db::Db;
use crate::state::AppState;

use super::runner_protocol::{Envelope, WireMessage};
use super::runner_ws::{RunnerRegistry, RunnerResponse};

/// Errors returned by [`runner_request`].
#[derive(Debug)]
pub enum RunnerRpcError {
    /// No runner is currently connected (and registered in DB as `online`)
    /// for this org.
    NoConnectedRunner,
    /// The provided `WireMessage` does not carry a `req_id` — programmer
    /// error in the caller.
    InvalidRequest,
    /// The runner's command channel was closed (writer task aborted) before
    /// we could enqueue the request frame.
    SendFailed,
    /// The runner did not reply within `timeout`.
    Timeout,
    /// The runner disconnected while the request was in flight. Detected by
    /// the cleanup path in `handle_runner_ws` clearing the pending map,
    /// which drops the oneshot sender and wakes the receiver with `Closed`.
    RunnerDisconnected,
}

impl std::fmt::Display for RunnerRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConnectedRunner => f.write_str("no connected runner for this org"),
            Self::InvalidRequest => f.write_str("request message has no req_id"),
            Self::SendFailed => f.write_str("failed to send request to runner"),
            Self::Timeout => f.write_str("runner did not reply within the timeout"),
            Self::RunnerDisconnected => f.write_str("runner disconnected before replying"),
        }
    }
}

impl std::error::Error for RunnerRpcError {}

/// Send a request frame to a connected runner and await its reply.
///
/// `message` must be a request variant carrying a `req_id` (e.g.
/// [`WireMessage::ListFolders`] or [`WireMessage::CreateFolder`]).
/// Generate `req_id` with `uuid::Uuid::new_v4().to_string()` at the
/// call site so the same id can be tied back to the originating HTTP
/// caller (e.g. for tracing) before invoking this helper.
pub async fn runner_request(
    state: &AppState,
    org_id: &str,
    message: WireMessage,
    timeout: Duration,
) -> Result<RunnerResponse, RunnerRpcError> {
    runner_request_inner(&state.db, &state.runners, org_id, message, timeout).await
}

async fn runner_request_inner(
    db: &Db,
    runners: &RunnerRegistry,
    org_id: &str,
    message: WireMessage,
    timeout: Duration,
) -> Result<RunnerResponse, RunnerRpcError> {
    let req_id = req_id_for(&message)
        .ok_or(RunnerRpcError::InvalidRequest)?
        .to_string();

    // Cross-reference the DB (org_id filter, online status) with the
    // in-memory registry to find a runner we can both authorize and reach.
    let online_ids: Vec<String> = {
        let conn = db.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id FROM runners WHERE org_id = ?1 AND status = 'online' \
             ORDER BY last_seen_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Err(RunnerRpcError::NoConnectedRunner),
        };
        stmt.query_map(params![org_id], |row| row.get::<_, String>(0))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };

    let (command_tx, pending) = {
        let registry = runners.lock().await;
        online_ids
            .into_iter()
            .find_map(|id| {
                registry
                    .get(&id)
                    .map(|r| (r.command_tx.clone(), r.pending.clone()))
            })
            .ok_or(RunnerRpcError::NoConnectedRunner)?
    };

    send_and_wait(command_tx, pending, req_id, message, timeout).await
}

/// Inner mechanic shared with the unit tests: register the oneshot, send
/// the envelope best-effort, and await the receiver against the timeout.
async fn send_and_wait(
    command_tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>>,
    req_id: String,
    message: WireMessage,
    timeout: Duration,
) -> Result<RunnerResponse, RunnerRpcError> {
    let (tx, rx) = oneshot::channel::<RunnerResponse>();
    pending.lock().await.insert(req_id.clone(), tx);

    // Server-originated frames carry "server" in the envelope's runner_id
    // field (which identifies the SENDER, not the addressee).
    let envelope = Envelope::best_effort("server".to_string(), message);
    let payload = serde_json::to_string(&envelope).unwrap_or_default();
    if command_tx.send(payload).is_err() {
        pending.lock().await.remove(&req_id);
        return Err(RunnerRpcError::SendFailed);
    }

    tokio::select! {
        result = rx => match result {
            Ok(response) => Ok(response),
            // Sender was dropped without sending — runner disconnected and
            // the cleanup path cleared the pending map.
            Err(_) => Err(RunnerRpcError::RunnerDisconnected),
        },
        _ = tokio::time::sleep(timeout) => {
            // Timed out — remove the entry so a late reply does not match
            // a now-orphan sender.
            pending.lock().await.remove(&req_id);
            Err(RunnerRpcError::Timeout)
        }
    }
}

/// Returns the `req_id` correlator for request/response WireMessage variants.
/// Reply variants are listed too so callers passing a reply by mistake get a
/// consistent `req_id` extraction, but `runner_request` is only meaningful
/// for the request variants.
fn req_id_for(msg: &WireMessage) -> Option<&str> {
    match msg {
        WireMessage::ListFolders { req_id }
        | WireMessage::CreateFolder { req_id, .. }
        | WireMessage::FoldersListed { req_id, .. }
        | WireMessage::FolderCreated { req_id, .. } => Some(req_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex as StdMutex;

    use rusqlite::Connection;

    use crate::saas::runner_protocol::FolderEntry;
    use crate::saas::runner_ws::{ConnectedRunner, new_runner_registry};

    /// In-memory DB with the `runners` table and a single row for `runner_id`
    /// in the given org, marked online.
    fn db_with_online_runner(runner_id: &str, org_id: &str) -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, \
               name TEXT, \
               org_id TEXT, \
               status TEXT, \
               hostname TEXT, \
               version TEXT, \
               last_seen_at TEXT, \
               created_at TEXT \
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
             VALUES (?1, 'test', ?2, 'online', datetime('now'))",
            params![runner_id, org_id],
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    fn empty_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runners ( \
               id TEXT PRIMARY KEY, \
               name TEXT, \
               org_id TEXT, \
               status TEXT, \
               hostname TEXT, \
               version TEXT, \
               last_seen_at TEXT, \
               created_at TEXT \
             );",
        )
        .unwrap();
        Arc::new(StdMutex::new(conn))
    }

    /// Build a registered ConnectedRunner whose command_tx reads outgoing
    /// envelopes and resolves them via the supplied closure.
    async fn install_echo_runner<F>(registry: &RunnerRegistry, runner_id: &str, respond: F)
    where
        F: Fn(WireMessage) -> Option<RunnerResponse> + Send + Sync + 'static,
    {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<RunnerResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();

        // Reader task: parse each outgoing envelope, hand the message to the
        // responder, and resolve the matching pending entry.
        tokio::spawn(async move {
            while let Some(payload) = cmd_rx.recv().await {
                let envelope: Envelope = match serde_json::from_str(&payload) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let req_id = match req_id_for(&envelope.message) {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                if let Some(reply) = respond(envelope.message)
                    && let Some(tx) = pending_clone.lock().await.remove(&req_id)
                {
                    let _ = tx.send(reply);
                }
            }
        });

        registry.lock().await.insert(
            runner_id.to_string(),
            ConnectedRunner {
                command_tx: cmd_tx,
                hostname: None,
                version: None,
                pending,
            },
        );
    }

    #[tokio::test]
    async fn success_path_returns_entries() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();

        let entries = vec![FolderEntry {
            name: "projects".into(),
            path: "/home/u/projects".into(),
        }];
        let entries_for_responder = entries.clone();
        install_echo_runner(&registry, "runner-1", move |msg| match msg {
            WireMessage::ListFolders { .. } => {
                Some(RunnerResponse::FoldersListed(entries_for_responder.clone()))
            }
            _ => None,
        })
        .await;

        let req = WireMessage::ListFolders {
            req_id: "req-success".into(),
        };
        let result = runner_request_inner(&db, &registry, "org-1", req, Duration::from_millis(500))
            .await
            .expect("runner_request should succeed");
        match result {
            RunnerResponse::FoldersListed(got) => assert_eq!(got, entries),
            other => panic!("expected FoldersListed variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_path_returns_timeout() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        // Install a runner that never replies.
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        let req = WireMessage::ListFolders {
            req_id: "req-timeout".into(),
        };
        let result =
            runner_request_inner(&db, &registry, "org-1", req, Duration::from_millis(50)).await;
        assert!(matches!(result, Err(RunnerRpcError::Timeout)));
    }

    #[tokio::test]
    async fn no_runner_path_returns_no_connected_runner() {
        let db = empty_db();
        let registry = new_runner_registry();

        let req = WireMessage::ListFolders {
            req_id: "req-norunner".into(),
        };
        let result =
            runner_request_inner(&db, &registry, "org-1", req, Duration::from_millis(500)).await;
        assert!(matches!(result, Err(RunnerRpcError::NoConnectedRunner)));
    }

    #[tokio::test]
    async fn db_has_runner_but_registry_is_empty_returns_no_connected_runner() {
        // DB says runner is online, but in-memory registry has nothing.
        // (This is the post-server-restart state for SaaS deployments.)
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();

        let req = WireMessage::ListFolders {
            req_id: "req-stale".into(),
        };
        let result =
            runner_request_inner(&db, &registry, "org-1", req, Duration::from_millis(500)).await;
        assert!(matches!(result, Err(RunnerRpcError::NoConnectedRunner)));
    }

    #[tokio::test]
    async fn invalid_request_without_req_id() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        let req = WireMessage::Ping {};
        let result =
            runner_request_inner(&db, &registry, "org-1", req, Duration::from_millis(500)).await;
        assert!(matches!(result, Err(RunnerRpcError::InvalidRequest)));
    }

    #[tokio::test]
    async fn runner_disconnect_mid_call_does_not_deadlock() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        // Kick off a request that will park on the oneshot.
        let db_task = db.clone();
        let registry_task = registry.clone();
        let handle = tokio::spawn(async move {
            runner_request_inner(
                &db_task,
                &registry_task,
                "org-1",
                WireMessage::ListFolders {
                    req_id: "req-disconnect".into(),
                },
                Duration::from_secs(30),
            )
            .await
        });

        // Yield so the spawned task gets to register its sender in `pending`
        // before we simulate the cleanup path.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Simulate handle_runner_ws cleanup: remove the runner and clear its
        // pending senders. Dropping each sender wakes the awaiting receiver.
        if let Some(runner) = registry.lock().await.remove("runner-1") {
            runner.pending.lock().await.clear();
        }

        // The helper should now resolve quickly with RunnerDisconnected,
        // well before the 30s timeout.
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("runner_request did not complete after disconnect")
            .expect("task panicked");
        assert!(matches!(result, Err(RunnerRpcError::RunnerDisconnected)));
    }

    /// Integration test for Task 1.4 acceptance criteria. Mounts the real
    /// `runner_ws_handler` in an in-process Axum server, drives it with a
    /// tokio-tungstenite WS client, and verifies that dropping the WS while a
    /// `runner_request` is in flight wakes the awaiting receiver via the
    /// `pending`-map drain in the `handle_runner_ws` cleanup block.
    ///
    /// The previous test (`runner_disconnect_mid_call_does_not_deadlock`)
    /// simulates the cleanup by hand. This one exercises the production
    /// cleanup code path end-to-end.
    #[tokio::test]
    async fn real_ws_disconnect_drains_pending_senders_and_wakes_receivers() {
        use futures_util::SinkExt;
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        // Full-schema DB on a tempfile so all tables (runners, runner_tokens,
        // users, seq_tracker, …) exist for the production handler.
        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));

        let user_id = "test-user";
        let token = "test-token-real-ws-disconnect";
        let org_id = "default-org"; // seeded by db::init -> ensure_default_org
        let runner_id = "test-runner-real-ws";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "test@test", "x"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runner_tokens (token_hash, runner_name, org_id, created_by) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![token, "test-runner", org_id, user_id],
            )
            .unwrap();
        }

        // Minimal AppState wired only with what runner_ws_handler reads from.
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test"),
            0,
        );
        let runners = new_runner_registry();
        let state = crate::state::AppState {
            db: db.clone(),
            plans_dir: PathBuf::from("/tmp"),
            port: 0,
            effort: Arc::new(StdMutex::new(crate::config::Effort::Medium)),
            broadcast_tx,
            registry,
            runners: runners.clone(),
        };
        let app = axum::Router::new()
            .route(
                "/ws/runner",
                axum::routing::get(crate::saas::runner_ws::runner_ws_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Connect a fake runner WS and send RunnerHello so the server
        // registers us in the in-memory `runners` map.
        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");
        let hello = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "test-host".into(),
                version: "0.0.0".into(),
                drivers: vec![],
            },
        );
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Wait for the server to register the runner.
        let mut attempts = 0;
        while attempts < 200 {
            if runners.lock().await.contains_key(runner_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            attempts += 1;
        }
        assert!(
            runners.lock().await.contains_key(runner_id),
            "runner did not register after RunnerHello"
        );

        // Park a runner_request on the oneshot. Long timeout (30s) so a test
        // failure caused by the cleanup *not* waking the receiver cannot be
        // masked by a coincidental timeout.
        let db_clone = db.clone();
        let runners_clone = runners.clone();
        let req_handle = tokio::spawn(async move {
            runner_request_inner(
                &db_clone,
                &runners_clone,
                org_id,
                WireMessage::ListFolders {
                    req_id: "req-real-ws-disconnect".into(),
                },
                Duration::from_secs(30),
            )
            .await
        });

        // Wait for runner_request to register its sender in pending.
        let mut attempts = 0;
        loop {
            let inserted = {
                let registry_guard = runners.lock().await;
                match registry_guard.get(runner_id) {
                    Some(runner) => !runner.pending.lock().await.is_empty(),
                    None => false,
                }
            };
            if inserted {
                break;
            }
            attempts += 1;
            assert!(
                attempts <= 200,
                "runner_request never inserted a pending entry"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Drop the WS — handle_runner_ws's reader loop breaks and runs the
        // cleanup block, which drains the pending map.
        let t0 = std::time::Instant::now();
        drop(ws);

        // The receiver should wake quickly with Closed, mapped to
        // RunnerDisconnected. A 2-second budget is far below the 30s
        // configured timeout, so a wake via timeout would not satisfy this.
        let result = tokio::time::timeout(Duration::from_secs(2), req_handle)
            .await
            .expect("runner_request did not complete after disconnect")
            .expect("task panicked");
        let elapsed = t0.elapsed();
        assert!(
            matches!(result, Err(RunnerRpcError::RunnerDisconnected)),
            "expected RunnerDisconnected, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "disconnect-wake took {elapsed:?}, should be < 2s"
        );

        server_handle.abort();
    }
}
