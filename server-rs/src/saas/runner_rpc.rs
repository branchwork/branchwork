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

#![allow(dead_code)] // HTTP consumers land in Phase 2 of saas-folder-listing-via-runner

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
    runner_request_with_registry(&state.db, &state.runners, org_id, message, timeout).await
}

/// Same as [`runner_request`] but takes the DB + registry directly instead of
/// going through `&AppState`. Used by dispatchers (e.g. `agents::git_ops`,
/// `ci`) that don't always have an `AppState` in scope — most prominently the
/// CI poller, which is spawned with just `(db, broadcast_tx, plans_dir)`.
pub async fn runner_request_with_registry(
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
        | WireMessage::FolderCreated { req_id, .. }
        | WireMessage::CloneProject { req_id, .. }
        | WireMessage::CloneStarted { req_id, .. }
        | WireMessage::CloneDone { req_id, .. }
        | WireMessage::CloneFailed { req_id, .. }
        | WireMessage::GetDefaultBranch { req_id, .. }
        | WireMessage::DefaultBranchResolved { req_id, .. }
        | WireMessage::ListBranches { req_id, .. }
        | WireMessage::BranchesListed { req_id, .. }
        | WireMessage::MergeBranch { req_id, .. }
        | WireMessage::MergeResult { req_id, .. }
        | WireMessage::DiscardBranch { req_id, .. }
        | WireMessage::BranchDiscarded { req_id, .. }
        | WireMessage::PushBranch { req_id, .. }
        | WireMessage::PushResult { req_id, .. }
        | WireMessage::GhRunList { req_id, .. }
        | WireMessage::GhRunListed { req_id, .. }
        | WireMessage::GhFailureLog { req_id, .. }
        | WireMessage::GhFailureLogFetched { req_id, .. }
        | WireMessage::MergeAgentBranch { req_id, .. }
        | WireMessage::AgentBranchMerged { req_id, .. }
        | WireMessage::HasGithubActions { req_id, .. }
        | WireMessage::GithubActionsDetected { req_id, .. }
        | WireMessage::GetCiRunStatus { req_id, .. }
        | WireMessage::CiRunStatusResolved { req_id, .. }
        | WireMessage::CiFailureLog { req_id, .. }
        | WireMessage::CiFailureLogResolved { req_id, .. } => Some(req_id),
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
                drivers: None,
                pending,
                server_url: "http://localhost:3100".to_string(),
            },
        );
    }

    /// Branch 1 of Task 5.1: happy path. Helper sends, the in-test reader
    /// pulls the envelope off `command_tx`, hands it to the `respond` closure
    /// which resolves the matching pending entry — helper returns the entries.
    #[tokio::test]
    async fn runner_request_happy_path_returns_entries() {
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
        let result =
            runner_request_with_registry(&db, &registry, "org-1", req, Duration::from_millis(500))
                .await
                .expect("runner_request should succeed");
        match result {
            RunnerResponse::FoldersListed(got) => assert_eq!(got, entries),
            other => panic!("expected FoldersListed variant, got {other:?}"),
        }
    }

    /// Branch 2 of Task 5.1: helper sends but no response is injected, so the
    /// receiver parks until the configured timeout fires. 100ms is the spec'd
    /// budget — well under the "in well under a second" acceptance bar.
    #[tokio::test]
    async fn runner_request_timeout_returns_timeout_error() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        // Install a runner that never replies.
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        let req = WireMessage::ListFolders {
            req_id: "req-timeout".into(),
        };
        let started = std::time::Instant::now();
        let result =
            runner_request_with_registry(&db, &registry, "org-1", req, Duration::from_millis(100))
                .await;
        let elapsed = started.elapsed();
        assert!(matches!(result, Err(RunnerRpcError::Timeout)));
        assert!(
            elapsed < Duration::from_secs(1),
            "timeout test took {elapsed:?}, should be well under 1s"
        );
    }

    #[tokio::test]
    async fn runner_request_no_runner_returns_no_connected_runner() {
        let db = empty_db();
        let registry = new_runner_registry();

        let req = WireMessage::ListFolders {
            req_id: "req-norunner".into(),
        };
        let result =
            runner_request_with_registry(&db, &registry, "org-1", req, Duration::from_millis(500))
                .await;
        assert!(matches!(result, Err(RunnerRpcError::NoConnectedRunner)));
    }

    #[tokio::test]
    async fn runner_request_stale_db_row_returns_no_connected_runner() {
        // DB says runner is online, but in-memory registry has nothing.
        // (This is the post-server-restart state for SaaS deployments.)
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();

        let req = WireMessage::ListFolders {
            req_id: "req-stale".into(),
        };
        let result =
            runner_request_with_registry(&db, &registry, "org-1", req, Duration::from_millis(500))
                .await;
        assert!(matches!(result, Err(RunnerRpcError::NoConnectedRunner)));
    }

    #[tokio::test]
    async fn runner_request_without_req_id_returns_invalid_request() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        let req = WireMessage::Ping {};
        let result =
            runner_request_with_registry(&db, &registry, "org-1", req, Duration::from_millis(500))
                .await;
        assert!(matches!(result, Err(RunnerRpcError::InvalidRequest)));
    }

    /// Branch 3 of Task 5.1: a request is parked on the oneshot when the
    /// runner's pending map is cleared (simulating the cleanup the WS reader
    /// runs on disconnect). The receiver wakes immediately with `Closed`,
    /// which the helper maps to `RunnerDisconnected` — well before the 30s
    /// timeout we configure.
    #[tokio::test]
    async fn runner_request_disconnect_returns_runner_disconnected() {
        let db = db_with_online_runner("runner-1", "org-1");
        let registry = new_runner_registry();
        install_echo_runner(&registry, "runner-1", |_msg| None).await;

        // Kick off a request that will park on the oneshot.
        let db_task = db.clone();
        let registry_task = registry.clone();
        let handle = tokio::spawn(async move {
            runner_request_with_registry(
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
    /// The previous test
    /// (`runner_request_disconnect_returns_runner_disconnected`) simulates
    /// the cleanup by hand. This one exercises the production cleanup code
    /// path end-to-end.
    #[tokio::test]
    async fn runner_request_real_ws_disconnect_wakes_receivers() {
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
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
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
            runner_request_with_registry(
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

    /// T4.3 acceptance pin: after the runner sends `RunnerHello`, the
    /// server must reply with a `Resume` whose `server_version` is the
    /// server's compile-time `CARGO_PKG_VERSION`. The server must also
    /// persist `upgrade_available=true` from the Hello onto the
    /// `runners.upgrade_available` column so the dashboard pill can
    /// light up on the next /api/runners poll.
    #[tokio::test]
    async fn ws_handshake_stamps_server_version_on_resume_and_persists_upgrade_available() {
        use futures_util::{SinkExt, StreamExt};
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "test-user-t43";
        let token = "test-token-t43";
        let org_id = "default-org";
        let runner_id = "test-runner-t43";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t43@test", "x"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runner_tokens (token_hash, runner_name, org_id, created_by) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![token, "test-runner-t43", org_id, user_id],
            )
            .unwrap();
        }
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t43"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t43-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");
        // Hello carries upgrade_available=true so we can confirm the
        // server persists the flag through to the runners table.
        let hello = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "test-host".into(),
                version: "0.3.0".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: true,
            },
        );
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Read incoming frames until we see the Resume — the server may
        // also send Ack frames first.
        let resume = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => {
                        let env: Envelope = serde_json::from_str(&t).expect("parse envelope");
                        if let WireMessage::Resume {
                            last_seen_seq,
                            server_version,
                        } = env.message
                        {
                            return (last_seen_seq, server_version);
                        }
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => panic!("ws read error: {e}"),
                    None => panic!("ws closed before Resume"),
                }
            }
        })
        .await
        .expect("Resume did not arrive within 2s");
        let expected = env!("CARGO_PKG_VERSION");
        assert_eq!(
            resume.1.as_deref(),
            Some(expected),
            "server must stamp its CARGO_PKG_VERSION on Resume; got {:?}",
            resume.1
        );

        // Wait for the server to persist the Hello's upgrade_available
        // flag (the handler writes inside the registry-update branch).
        let mut attempts = 0;
        let persisted = loop {
            let v: Option<i64> = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT upgrade_available FROM runners WHERE id = ?1",
                    params![runner_id],
                    |row| row.get(0),
                )
                .ok()
            };
            if let Some(v) = v
                && v != 0
            {
                break v;
            }
            attempts += 1;
            if attempts > 200 {
                panic!(
                    "upgrade_available did not persist after RunnerHello (last observed value: {v:?})"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(persisted, 1, "upgrade_available must round-trip true");

        drop(ws);
        server_handle.abort();
    }

    /// T5.1 acceptance pin: after a runner is soft-deleted via the API,
    /// the next WS upgrade attempt with the now-revoked token must return
    /// 401 `invalid_token` (`validate_runner_token` sees no matching row).
    /// No row updates land on the soft-deleted `runners` row because the
    /// upgrade never proceeds past the handshake.
    #[tokio::test]
    async fn ws_connect_after_revoke_fails_with_invalid_token() {
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::http::StatusCode;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "tester-t51-revoke";
        let token = "test-token-t51-revoke";
        let org_id = "default-org";
        let runner_id = "test-runner-t51-revoke";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t51@test", "x"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runner_tokens \
                 (token_hash, runner_name, org_id, created_by, claimed_runner_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token, "runner-t51", org_id, user_id, runner_id],
            )
            .unwrap();
            // Simulate the runner having connected at least once: a runners
            // row exists with hostname/version set so we can later prove
            // nothing tickled it.
            conn.execute(
                "INSERT INTO runners (id, name, org_id, status, hostname, version, last_seen_at) \
                 VALUES (?1, ?2, ?3, 'online', 'pre-revoke-host', 'pre-revoke-version', \
                         datetime('now'))",
                params![runner_id, "runner-t51", org_id],
            )
            .unwrap();
        }

        // Simulate `delete_runner`'s effect: delete the token row + set
        // `removed_at` on the runners row. Mirrors `api/runners.rs::delete_runner`.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM runner_tokens WHERE claimed_runner_id = ?1",
                params![runner_id],
            )
            .unwrap();
            conn.execute(
                "UPDATE runners SET removed_at = datetime('now') WHERE id = ?1",
                params![runner_id],
            )
            .unwrap();
        }

        // Build a minimal AppState and mount the production WS handler.
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t51"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t51-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        // Connect with the now-revoked token. tokio-tungstenite surfaces the
        // pre-upgrade HTTP error directly because the server returns
        // `(401, "invalid_token")` without honoring the upgrade.
        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let err = tokio_tungstenite::connect_async(&url)
            .await
            .expect_err("connect_async must fail after the token is revoked");
        let status = match err {
            tokio_tungstenite::tungstenite::Error::Http(resp) => resp.status(),
            other => panic!("expected HTTP error from upgrade, got {other:?}"),
        };
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "revoked-token WS connect must return 401"
        );

        // No row updates landed: hostname/version/status untouched by the
        // failed upgrade, removed_at still set, and ConnectedRunner stays
        // absent from the in-memory registry.
        let (hostname, version, status_col, removed_at): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT hostname, version, status, removed_at FROM runners WHERE id = ?1",
                params![runner_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
        };
        assert_eq!(hostname.as_deref(), Some("pre-revoke-host"));
        assert_eq!(version.as_deref(), Some("pre-revoke-version"));
        assert_eq!(status_col.as_deref(), Some("online"));
        assert!(removed_at.is_some(), "removed_at must remain set");
        assert!(
            !runners.lock().await.contains_key(runner_id),
            "no in-memory ConnectedRunner must exist for a revoked runner"
        );

        server_handle.abort();
    }

    /// T5.1 acceptance pin: when a runner is revoked **while** its WS is
    /// still alive, the next inbound message must NOT tickle the
    /// soft-deleted `runners` row. The reader loop's in-memory revoke
    /// check breaks the WS, the `removed_at IS NULL` SQL gates catch any
    /// straggler UPDATEs, and the runner row stays exactly as it was at
    /// the moment of revoke.
    #[tokio::test]
    async fn mid_ws_revoke_breaks_reader_loop_and_does_not_update_runners_row() {
        use futures_util::SinkExt;
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "tester-t51-mid";
        let token = "test-token-t51-mid";
        let org_id = "default-org";
        let runner_id = "test-runner-t51-mid";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t51mid@test", "x"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runner_tokens (token_hash, runner_name, org_id, created_by) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![token, "runner-t51-mid", org_id, user_id],
            )
            .unwrap();
        }

        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t51-mid"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t51-mid-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        // Open a WS and send RunnerHello so the server registers the runner.
        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");
        let hello = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "mid-host".into(),
                version: "mid-version".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Wait for the server to register the runner AND apply the Hello
        // UPDATE so we have a known-good baseline to compare against.
        let mut attempts = 0;
        loop {
            let registered = runners.lock().await.contains_key(runner_id);
            let hostname_set: bool = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT hostname FROM runners WHERE id = ?1",
                    params![runner_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .is_some()
            };
            if registered && hostname_set {
                break;
            }
            attempts += 1;
            assert!(attempts <= 200, "runner did not register + apply Hello");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Snapshot the runners row mid-flight so we can prove it stays put
        // across the revoke + next message.
        let baseline: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT hostname, version, status, last_seen_at FROM runners WHERE id = ?1",
                params![runner_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
        };

        // Simulate `delete_runner`: delete tokens, soft-delete the runner
        // row, drop the in-memory ConnectedRunner. Mirrors the production
        // sequence in `api/runners.rs::delete_runner`.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM runner_tokens WHERE claimed_runner_id = ?1",
                params![runner_id],
            )
            .unwrap();
            conn.execute(
                "UPDATE runners SET removed_at = datetime('now') WHERE id = ?1",
                params![runner_id],
            )
            .unwrap();
        }
        runners.lock().await.remove(runner_id);

        // Send another RunnerHello with DIFFERENT hostname/version. The
        // reader loop's in-memory revoke check must break BEFORE
        // handle_runner_message dispatches the UPDATE; even if the dispatch
        // did fire, the `AND removed_at IS NULL` SQL gate on the RunnerHello
        // arm would no-op the UPDATE.
        let post_revoke_hello = Envelope::reliable(
            runner_id.into(),
            2,
            WireMessage::RunnerHello {
                hostname: "post-revoke-host".into(),
                version: "post-revoke-version".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        let _ = ws
            .send(Message::Text(
                serde_json::to_string(&post_revoke_hello).unwrap().into(),
            ))
            .await;

        // Give the server time to read the message, hit the revoke check,
        // and break the loop. 1s is well under the 30s test timeout.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let after: (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT hostname, version, status, last_seen_at FROM runners WHERE id = ?1",
                params![runner_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap()
        };
        // hostname/version: the post-revoke Hello carried "post-revoke-*"
        // strings; both must stay at the pre-revoke values.
        assert_eq!(
            after.0, baseline.0,
            "hostname must not change after revoke (got {:?}, baseline {:?})",
            after.0, baseline.0
        );
        assert_eq!(
            after.1, baseline.1,
            "version must not change after revoke (got {:?}, baseline {:?})",
            after.1, baseline.1
        );
        // status: the disconnect-cleanup UPDATE also carries
        // `AND removed_at IS NULL`, so even when the WS closes the status
        // column stays at whatever it was at revoke time (not 'offline').
        assert_eq!(
            after.2, baseline.2,
            "status must not change after revoke (got {:?}, baseline {:?})",
            after.2, baseline.2
        );

        // The runner stays out of the in-memory registry (no other
        // connection re-added it).
        assert!(
            !runners.lock().await.contains_key(runner_id),
            "registry must stay empty after mid-WS revoke"
        );

        drop(ws);
        server_handle.abort();
    }

    /// T5.2 acceptance pin: a soft-deleted runner row whose token is still
    /// bound (i.e. legacy state — operator soft-deleted without revoking the
    /// token, OR a re-enrolled host where a fresh token was bound to the
    /// pre-existing runner_id) un-hides on the first message in. After the
    /// WS handshake lands, `removed_at` must be cleared and `status` must be
    /// `'online'`.
    ///
    /// Counterpart to `ws_connect_after_revoke_fails_with_invalid_token`,
    /// which proves the security gate: a *properly* revoked runner (token
    /// row DELETEd) cannot reach this code at all.
    #[tokio::test]
    async fn ws_connect_with_bound_token_unhides_soft_deleted_runner_row() {
        use futures_util::SinkExt;
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "tester-t52-unhide";
        let token = "test-token-t52-unhide";
        let org_id = "default-org";
        let runner_id = "test-runner-t52-unhide";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t52@test", "x"],
            )
            .unwrap();
            // Token row still present and bound to the runner. This models a
            // re-enrolled host (operator issued a fresh token after soft-
            // delete, runner_id persisted via local seq_tracker) or a legacy
            // stale-state row where the token DELETE got skipped somehow.
            conn.execute(
                "INSERT INTO runner_tokens \
                 (token_hash, runner_name, org_id, created_by, claimed_runner_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token, "runner-t52", org_id, user_id, runner_id],
            )
            .unwrap();
            // Runner row pre-exists with removed_at set. status='offline'
            // mirrors the disconnect-cleanup state we'd typically be in after
            // the operator clicked Revoke (and the prior WS dropped).
            conn.execute(
                "INSERT INTO runners (id, name, org_id, status, hostname, version, \
                                     last_seen_at, removed_at) \
                 VALUES (?1, ?2, ?3, 'offline', 'pre-revoke-host', 'pre-revoke-version', \
                         datetime('now', '-1 hour'), datetime('now', '-1 hour'))",
                params![runner_id, "runner-t52", org_id],
            )
            .unwrap();
        }

        // Build a minimal AppState and mount the production WS handler.
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t52"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t52-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        // Connect with the still-valid token and send the first envelope.
        // The first-message branch in `handle_runner_ws` is the one that
        // hits the INSERT/ON-CONFLICT block under test.
        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");
        let hello = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "post-revoke-host".into(),
                version: "post-revoke-version".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Wait for the server to register the runner AND apply the INSERT/
        // ON-CONFLICT clearing removed_at. Polling for in-memory registration
        // is sufficient because that insert happens AFTER the SQL UPDATE in
        // `handle_runner_ws`.
        let mut attempts = 0;
        loop {
            let registered = runners.lock().await.contains_key(runner_id);
            if registered {
                break;
            }
            attempts += 1;
            assert!(
                attempts <= 200,
                "runner did not register after legitimate reconnect"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The headline acceptance: removed_at is cleared AND status flipped
        // back to 'online'. Without the T5.2 UPDATE SET removed_at = NULL,
        // both stay at their soft-deleted baseline.
        let (status_col, removed_at): (Option<String>, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, removed_at FROM runners WHERE id = ?1",
                params![runner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            status_col.as_deref(),
            Some("online"),
            "legitimate reconnect must flip status to online"
        );
        assert!(
            removed_at.is_none(),
            "legitimate reconnect must clear removed_at (got {removed_at:?})"
        );

        drop(ws);
        server_handle.abort();
    }

    /// T5.3 acceptance pin: a ghost reconnect (the runner row was
    /// soft-deleted but its token is still valid) writes exactly one
    /// `runner.ghost_reconnect` audit row keyed by the runner_id, with a
    /// diff that includes the prior `removed_at` value and a token_hash
    /// fingerprint. The structured warning log line is observable in
    /// stderr during the run but we pin only the audit + DB-state half
    /// of the contract here — those two are deterministic; stderr
    /// capture varies across test harnesses.
    ///
    /// Setup mirrors `ws_connect_with_bound_token_unhides_soft_deleted_runner_row`
    /// (T5.2) verbatim — same legacy / stale-state scenario, same
    /// outcome on the `runners` row (un-hide + status='online'), but
    /// this test additionally asserts the forensics row landed.
    #[tokio::test]
    async fn ghost_reconnect_writes_audit_row_and_unhides_row() {
        use futures_util::SinkExt;
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "tester-t53-ghost";
        let token = "test-token-t53-ghost";
        let org_id = "default-org";
        let runner_id = "test-runner-t53-ghost";
        let pre_removed_at = "2026-04-01 12:34:56";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t53@test", "x"],
            )
            .unwrap();
            // Stale-state legacy row: token still bound, `removed_at` set.
            // This is the ghost shape — the security gate (token DELETE in
            // `delete_runner`) did not fire so the WS upgrade still
            // authenticates, but the row was soft-deleted.
            conn.execute(
                "INSERT INTO runner_tokens \
                 (token_hash, runner_name, org_id, created_by, claimed_runner_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token, "runner-t53", org_id, user_id, runner_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runners (id, name, org_id, status, hostname, version, \
                                     last_seen_at, removed_at) \
                 VALUES (?1, ?2, ?3, 'offline', 'pre-revoke-host', 'pre-revoke-version', \
                         datetime('now', '-1 hour'), ?4)",
                params![runner_id, "runner-t53", org_id, pre_removed_at],
            )
            .unwrap();
        }

        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t53"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t53-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");
        let hello = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "post-revoke-host".into(),
                version: "post-revoke-version".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        ws.send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Wait for the server to register the runner so we know the
        // first-message branch (which writes the audit row) has run.
        let mut attempts = 0;
        loop {
            let registered = runners.lock().await.contains_key(runner_id);
            if registered {
                break;
            }
            attempts += 1;
            assert!(
                attempts <= 200,
                "runner did not register after ghost reconnect"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Pin (1): the runner row was un-hidden (T5.2 behavior survives).
        let (status_col, removed_at): (Option<String>, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, removed_at FROM runners WHERE id = ?1",
                params![runner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(status_col.as_deref(), Some("online"));
        assert!(
            removed_at.is_none(),
            "ghost reconnect must still un-hide the row (got {removed_at:?})"
        );

        // Pin (2): exactly one audit row with the right shape.
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT action, resource_type, resource_id, diff, user_email \
                     FROM audit_logs WHERE org_id = ?1 \
                       AND action = 'runner.ghost_reconnect' \
                     ORDER BY id DESC",
                )
                .unwrap();
            stmt.query_map(params![org_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
        };
        assert_eq!(
            rows.len(),
            1,
            "expected exactly one ghost-reconnect audit row, got {rows:?}"
        );
        let (action, resource_type, resource_id, diff, user_email) = &rows[0];
        assert_eq!(action, "runner.ghost_reconnect");
        assert_eq!(resource_type, "runner");
        assert_eq!(resource_id.as_deref(), Some(runner_id));
        assert_eq!(user_email.as_deref(), Some("branchwork-runner-ws"));

        // Pin (3): diff captures the forensics we need to trace this
        // back to a token without leaking the token itself.
        let diff_value: serde_json::Value =
            serde_json::from_str(diff.as_deref().expect("audit row must carry a diff"))
                .expect("diff must be valid JSON");
        assert_eq!(diff_value["runner_id"], runner_id);
        assert_eq!(diff_value["runner_name"], "runner-t53");
        assert_eq!(diff_value["removed_at"], pre_removed_at);
        let prefix = diff_value["token_hash_prefix"]
            .as_str()
            .expect("token_hash_prefix must be a string");
        assert_eq!(
            prefix.len(),
            8,
            "token_hash_prefix must be exactly 8 chars (got {prefix:?})"
        );
        assert!(
            token.starts_with(prefix),
            "token_hash_prefix must be a prefix of the underlying token \
             (token={token:?}, prefix={prefix:?})"
        );
        assert_ne!(
            prefix, token,
            "fingerprint must not equal the raw token value"
        );

        drop(ws);
        server_handle.abort();
    }

    /// T5.4 acceptance pin: a duplicate-seq `RunnerHello` still refreshes
    /// `runners.hostname/version/drivers_json/server_bin/upgrade_available`,
    /// because the runner's outbox persists seqs in
    /// `~/.branchwork-runner/runner.db` and replays previously-ACKed
    /// envelopes after every binary upgrade + reconnect. Pre-T5.4 the
    /// early `return` at the top of `handle_runner_message` short-
    /// circuited the row UPDATE so the dashboard kept showing whatever
    /// version made it through the first-ever hello (the 2026-05-18
    /// incident: dashboard stuck at `v0.3.0` for a runner whose on-disk
    /// binary was `v0.5.x`).
    ///
    /// Acceptance:
    ///   - First hello (seq=1, version=A) lands version=A.
    ///   - Second hello (SAME seq=1, version=B) lands version=B — the
    ///     row UPDATE was hoisted out of the per-envelope match arm.
    ///   - In-memory `ConnectedRunner` snapshot also flips to B.
    ///   - Idempotent-unsafe side effects don't re-fire: `runner_drivers`
    ///     broadcast happens exactly once (after hello#1, suppressed on
    ///     the duplicate hello#2). `runner_connected` fires exactly once
    ///     per WS connect (structural property of the first-message
    ///     branch, included here as a tripwire so a future regression
    ///     that drops the connection-state gate is caught).
    ///
    /// Test setup uses ONE WS connection sending two same-seq hellos
    /// rather than two physical WS connections — same code path
    /// (`is_duplicate_seq == true` via `outbox::advance_peer_seq`), much
    /// simpler harness.
    #[tokio::test]
    async fn duplicate_seq_runner_hello_refreshes_version_but_skips_side_effects() {
        use futures_util::SinkExt;
        use std::path::PathBuf;
        use tokio_tungstenite::tungstenite::Message;

        let tempdir = tempfile::TempDir::new().unwrap();
        let db = crate::db::init(&tempdir.path().join("test.db"));
        let user_id = "tester-t54-version";
        let token = "test-token-t54-version";
        let org_id = "default-org";
        let runner_id = "test-runner-t54-version";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
                params![user_id, "t54@test", "x"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO runner_tokens \
                 (token_hash, runner_name, org_id, created_by, claimed_runner_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token, "runner-t54", org_id, user_id, runner_id],
            )
            .unwrap();
            // Runner row pre-exists with no hostname/version yet. Status
            // 'offline' mirrors the post-disconnect-cleanup state we'd
            // typically be in just before a reconnect.
            conn.execute(
                "INSERT INTO runners (id, name, org_id, status, last_seen_at) \
                 VALUES (?1, ?2, ?3, 'offline', datetime('now', '-1 hour'))",
                params![runner_id, "runner-t54", org_id],
            )
            .unwrap();
        }

        let (broadcast_tx, mut broadcast_rx) = tokio::sync::broadcast::channel::<String>(256);
        let registry = crate::agents::AgentRegistry::new(
            db.clone(),
            broadcast_tx.clone(),
            None,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/branchwork-test-t54"),
            0,
            true,
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
            settings_path: PathBuf::from("/tmp/branchwork-test-t54-settings.json"),
            cancellation_tokens: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            auto_finish_dedupe: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            dirty_tree_watchers: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            started_at: std::time::Instant::now(),
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

        let url = format!("ws://127.0.0.1:{port}/ws/runner?token={token}");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("ws connect");

        // Hello #1: seq=1, version=A. Server advances seq_tracker to 1
        // and writes version=A.
        let hello_a = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "host-a".into(),
                version: "0.3.0".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        ws.send(Message::Text(
            serde_json::to_string(&hello_a).unwrap().into(),
        ))
        .await
        .unwrap();

        // Wait for hello #1 to land. Polling the runners.version column
        // is the canonical signal because the UPDATE is the load-bearing
        // observable both pre- and post-fix.
        let mut attempts = 0;
        loop {
            let v: Option<String> = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT version FROM runners WHERE id = ?1",
                    params![runner_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
            };
            if v.as_deref() == Some("0.3.0") {
                break;
            }
            attempts += 1;
            assert!(attempts <= 200, "first hello did not land version=0.3.0");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Hello #2: SAME seq=1 (duplicate), hostname/version=B. Models a
        // reconnect after a runner restart with an upgraded binary —
        // the runner-side outbox persisted seq=1 across the restart and
        // replays it as the first envelope after the new WS handshake.
        let hello_b = Envelope::reliable(
            runner_id.into(),
            1,
            WireMessage::RunnerHello {
                hostname: "host-b".into(),
                version: "0.5.99".into(),
                drivers: vec![],
                active_agents: vec![],
                server_bin: None,
                upgrade_available: false,
            },
        );
        ws.send(Message::Text(
            serde_json::to_string(&hello_b).unwrap().into(),
        ))
        .await
        .unwrap();

        // Headline acceptance: with T5.4 the runners row flips to B
        // within one hello round-trip. Without the fix the row stays
        // at A forever because the duplicate-seq early-return ran
        // before the per-arm UPDATE.
        let mut attempts = 0;
        loop {
            let (hostname, version): (Option<String>, Option<String>) = {
                let conn = db.lock().unwrap();
                conn.query_row(
                    "SELECT hostname, version FROM runners WHERE id = ?1",
                    params![runner_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
            };
            if hostname.as_deref() == Some("host-b") && version.as_deref() == Some("0.5.99") {
                break;
            }
            attempts += 1;
            assert!(
                attempts <= 200,
                "duplicate-seq hello did not refresh row \
                 (hostname={hostname:?}, version={version:?}) — \
                 the T5.4 fix lifts UPDATE runners out of the per-envelope match arm \
                 so it survives the duplicate-seq early-return"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // In-memory `ConnectedRunner` handle is also refreshed, so
        // `list_drivers_dispatch` and other registry-snapshot readers
        // see fresh metadata without an extra DB round-trip.
        {
            let guard = runners.lock().await;
            let runner = guard.get(runner_id).expect("runner still registered");
            assert_eq!(runner.hostname.as_deref(), Some("host-b"));
            assert_eq!(runner.version.as_deref(), Some("0.5.99"));
        }

        // Drain the broadcast channel and assert the duplicate-skip
        // contract: `runner_connected` fires exactly once per WS
        // (first-message branch gate), and `runner_drivers` fires
        // exactly once (suppressed on the duplicate-seq hello#2 by the
        // post-refresh early-return in `handle_runner_message`).
        let mut runner_connected_count = 0;
        let mut runner_drivers_count = 0;
        loop {
            match broadcast_rx.try_recv() {
                Ok(payload) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("runner_connected") => runner_connected_count += 1,
                            Some("runner_drivers") => runner_drivers_count += 1,
                            _ => {}
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        assert_eq!(
            runner_connected_count, 1,
            "runner_connected must fire exactly once per WS connect, not twice — \
             the first-message branch gate (id_guard.is_none()) keeps this true"
        );
        assert_eq!(
            runner_drivers_count, 1,
            "runner_drivers must be suppressed on duplicate-seq hello — \
             the post-refresh early-return in handle_runner_message is the gate"
        );

        drop(ws);
        server_handle.abort();
    }
}
