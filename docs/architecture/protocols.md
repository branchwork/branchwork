# Wire protocols

Branchwork speaks three protocols beyond plain HTTP:

1. **Session IPC** — a Unix-domain-socket / Windows-named-pipe channel between
   the server (or the runner) and a per-agent supervisor daemon. Length-prefixed
   `postcard` frames over [`session_protocol::Message`].
2. **Runner WebSocket** — JSON `Envelope`/`WireMessage` frames between the
   hosted server and remote runners, with a SQLite-backed outbox providing
   at-least-once delivery for state-changing events.
3. **Dashboard WebSocket** — server → browser fan-out (the `agent_*`,
   `task_status_changed`, `plan_updated`, `plan_checked`, auto-mode events).
   The envelope shape and the auto-mode event family are catalogued in §3
   below; the broader event vocabulary is not yet centralised here — call
   sites of [`ws::broadcast_event`](../../server-rs/src/ws.rs) are
   authoritative.

For the binary split that uses these protocols see
[overview.md](overview.md). For the daemon endpoint see
[session-daemon.md](session-daemon.md). For the runner endpoint see
[runner.md](runner.md).

[`session_protocol::Message`]: ../../server-rs/src/agents/session_protocol.rs

---

## 1. Session IPC

**Source:** `server-rs/src/agents/session_protocol.rs`

**Transport:** a single `interprocess` local socket per agent.
On Unix this is a Unix-domain socket at `~/.claude/sessions/<agent-id>.sock`; on
Windows it is a named pipe with the same logical path. The supervisor binds
and listens; the parent (server or runner) connects.

**Framing.** Every frame is:

```
+----------------+-----------------------------+
| 4 bytes BE u32 |   postcard-encoded payload  |
|  payload len   |  (1..=MAX_FRAME_BYTES)      |
+----------------+-----------------------------+
```

`MAX_FRAME_BYTES = 8 MiB` (`session_protocol::MAX_FRAME_BYTES`). Both
[`write_frame`] and [`read_frame`] enforce the cap symmetrically — a peer
that asks for more is dropped with `InvalidData`. There is no application-level
heartbeat in the framing layer itself; the `Ping`/`Pong` messages below ride
inside it.

[`write_frame`]: ../../server-rs/src/agents/session_protocol.rs
[`read_frame`]: ../../server-rs/src/agents/session_protocol.rs

### `Message` variants

| Variant | Direction | When sent |
|---|---|---|
| `Input(Vec<u8>)` | client → supervisor | Bytes typed by a dashboard user (or piped by the runner) that should reach the agent's stdin via the PTY master. Empty payloads are legal. |
| `Output(Vec<u8>)` | supervisor → client | A chunk read from the PTY master (the agent's stdout/stderr). Forwarded to every connected client and appended to `<socket>.log` for post-mortem replay. |
| `Resize { cols, rows }` | client → supervisor | The browser resized its xterm.js view. The supervisor forwards the new window size to the PTY so the agent re-paints. |
| `Kill` | client → supervisor | The dashboard requested termination. The supervisor sends `SIGTERM` to the agent, then `SIGKILL` after the grace period, then exits. |
| `Ping` | either direction | Keepalive probe. Used by the server's heartbeat task to detect a silently dead supervisor (see [session-daemon.md](session-daemon.md) — pidfile presence is the canonical crash signal; missed `Pong` is the 45 s fallback). |
| `Pong` | either direction | Reply to `Ping`. |

`Resize`, `Kill`, `Ping`, `Pong` are all unit-ish — they carry no opaque
payload, only their well-typed fields — so they are safe to add tolerance for
in older peers (see versioning below).

### Lifecycle

1. The parent spawns the supervisor (`branchwork-server session …`); the
   supervisor `bind()`s its socket and starts the agent under a PTY.
2. The parent connects, optionally sends an initial `Resize`, then enters a
   read/write loop.
3. The supervisor multiplexes PTY output as `Output` frames to **every**
   currently connected client. Disconnected clients are reaped silently.
4. On reconnect, the parent re-opens the socket and resumes — there is no
   handshake. Backfill is supplied by the **server-side** SQLite
   `agent_output` table (see [persistence.md](persistence.md)) plus
   the live broadcast; the on-disk `<socket>.log` is post-mortem only.
5. `Kill`, supervisor crash, or agent exit closes the socket; the parent
   detects EOF and reconciles agent state.

---

## 2. Runner WebSocket

**Source:** `server-rs/src/saas/runner_protocol.rs`,
`server-rs/src/saas/runner_ws.rs`,
`server-rs/src/saas/outbox.rs`,
`server-rs/src/bin/branchwork_runner.rs`.

**Transport:** a single WebSocket per runner.
Runner dials `wss://<saas>/ws/runner?token=<api_token>`; the server validates
the token against `runner_tokens` (SHA-256 lookup; tokens are 256-bit random
hex, see [runner.md](runner.md) for the storage caveat) and either upgrades
the connection or returns 401.

**Encoding:** every frame is one JSON `Envelope`. The body uses serde's
`#[serde(tag = "type", rename_all = "snake_case")]` tagged-union form, so a
frame on the wire looks like:

```json
{ "seq": 17, "runner_id": "runner-…", "type": "agent_started",
  "agent_id": "…", "plan_name": "…", "task_id": "1.2",
  "driver": "claude", "cwd": "/work" }
```

`Envelope.runner_id` is set on **every** frame so the server can demux without
relying on connection state — this matters during the brief window between a
runner reconnecting and the server learning the id from the first
`RunnerHello`. The server sends frames with `runner_id: "server"`.

### Reliable vs best-effort

`WireMessage::is_best_effort()` is the single source of truth for which frames
ride the outbox:

```
best-effort  = AgentOutput | AgentInput | Ping | Pong
reliable     = everything else
```

- **Best-effort frames** carry `seq: null` (the field is omitted in JSON via
  `skip_serializing_if`). They are written straight to the socket and dropped
  on disconnect. There is no ACK and no replay — by the time the connection
  is back, terminal bytes are stale.
- **Reliable frames** are persisted to the sender's outbox table
  (`runner_outbox` on the runner, `inbox_pending` on the server, both keyed
  by an autoincrement `seq`), then sent. The receiver ACKs after persisting,
  the sender prunes. On reconnect the receiver issues `Resume { last_seen_seq }`
  and the sender replays everything strictly greater than that seq.

Receivers de-duplicate via `outbox::advance_peer_seq` (a per-peer high-water
mark in `seq_tracker`). A duplicate frame is **always** ACKed — even though
its side-effects are skipped — so a sender whose previous ACK was lost still
makes progress on the next attempt.

### `WireMessage` variants

#### Runner → SaaS

| Variant | Reliable? | When sent |
|---|---|---|
| `RunnerHello { hostname, version, drivers }` | yes | First message after the WS upgrade and after every reconnect. The server uses it to update the `runners` row and broadcast the dashboard's Drivers panel. |
| `AgentStarted { agent_id, plan_name, task_id, driver, cwd }` | yes | The runner's session daemon successfully spawned an agent; the server inserts an `agents` row in `mode='remote'` and fans out `agent_started` to dashboards. |
| `AgentOutput { agent_id, data }` | **no** (best-effort) | A chunk of base64-encoded PTY bytes. Forwarded to dashboards as `agent_output`. Loss is acceptable: the SQLite `agent_output` table on the server captures the durable transcript. |
| `AgentStopped { agent_id, status, cost_usd?, stop_reason? }` | yes (with one wart) | Agent process exited or was killed. Server updates the `agents` row, runs org-budget enforcement if `cost_usd` is set, and broadcasts `agent_stopped`. **Wart:** the spawn-failure path sends this reliably; the normal-exit path in `branchwork_runner.rs` currently sends it best-effort (`Envelope::best_effort`). See [runner.md](runner.md). |
| `TaskStatusChanged { plan_name, task_number, status, reason? }` | yes | The agent reported a status change via MCP, or the runner detected one. Server upserts `task_status` with `source='manual'`, optionally appends `task_learnings`, and broadcasts `task_status_changed`. |
| `DriverAuthReport { drivers }` | yes | Driver auth state changed (e.g. user finished an OAuth flow). Re-broadcast as `runner_drivers`. Also implicitly delivered by the `RunnerHello` `drivers` field. |
| `FoldersListed { req_id, entries }` | **no** — req/resp | Reply to `ListFolders`. Carries one `FolderEntry { name, path }` per directory under the runner's home (one level deep, mirroring the local-mode listing in `api::settings::list_folders`). Server matches `req_id` to a pending oneshot to resolve the originating HTTP caller; late replies (post-timeout or post-reconnect) are silently discarded. See [Request/response frames](#requestresponse-frames). |
| `FolderCreated { req_id, ok, resolved_path?, error? }` | **no** — req/resp | Reply to `CreateFolder`. `ok=true` ⇒ `resolved_path` is the canonical absolute path the runner created; `ok=false` ⇒ `error` carries a human-readable reason (permission denied, path traversal, etc.). Same `req_id` correlation as `FoldersListed`. |
| `DefaultBranchResolved { req_id, branch? }` | **no** — req/resp | Reply to `GetDefaultBranch`. `branch=None` ⇒ no candidate resolved (no `origin/HEAD` symref and neither `master` nor `main` exists locally — see [Branch and merge round-trips](#branch-and-merge-round-trips)). |
| `BranchesListed { req_id, branches }` | **no** — req/resp | Reply to `ListBranches`. Alphabetically sorted `git branch --format='%(refname:short)'` output for the requested cwd; the server filters out the default branch and the live task branch before returning the dropdown payload. |
| `MergeResult { req_id, outcome }` | **no** — req/resp | Reply to `MergeBranch`. `outcome` is a tagged enum on `kind` with five arms — `ok { merged_sha }`, `empty_branch`, `checkout_failed { stderr }`, `conflict { stderr }`, `other { stderr }` — so the dispatcher can map each one to the HTTP response code that the local `merge_agent_branch` would have returned (409 for empty/conflict, 500 for other). |
| `PushResult { req_id, ok, stderr? }` | **no** — req/resp | Reply to `PushBranch`. `ok=false` ⇒ `stderr` carries the captured `git push` error so the server can log it; the dashboard does not surface push failures to the user (CI will retry on the next merge). |
| `GhRunListed { req_id, run? }` | **no** — req/resp | Reply to `GhRunList`. `run=None` ⇒ no workflow has fired yet for that commit (or `gh` is unavailable). Carries the gh-spelled `databaseId` field verbatim — see [CI status round-trips](#ci-status-round-trips). |
| `GhFailureLogFetched { req_id, log? }` | **no** — req/resp | Reply to `GhFailureLog`. `log` is the trailing ~8 KB of `gh run view --log-failed` output (the same tail size `ci.rs::fetch_failure_log` keeps when running locally); `None` when the run has no failure log (still pending, no auth, etc). |
| `AgentBranchMerged { req_id, ok, merged_sha?, target_branch, had_conflict, error? }` | **no** — req/resp | Reply to `MergeAgentBranch`. Flat payload shaped like `api::agents::MergeOutcome` minus `task_branch` — auto-mode reads `had_conflict` (pause with `merge_conflict`) before falling through to `merged_sha.is_some()` (advance) and finally `error` (pause with `merge_failed: <msg>`). `target_branch` is the empty string for early-return failures (agent not found on the runner, no task branch). See [Auto-mode round-trips](#auto-mode-round-trips). |
| `GithubActionsDetected { req_id, present }` | **no** — req/resp | Reply to `HasGithubActions`. `true` ⇒ `cwd/.github/workflows/*.{yml,yaml}` matched at least one file; gates the auto-mode CI wait branch. |
| `CiRunStatusResolved { req_id, aggregate? }` | **no** — req/resp | Reply to `GetCiRunStatus`. `aggregate=None` ⇒ no workflow run exists yet for the SHA (still polling) or `gh` unavailable. The aggregate's `failing_run_id` is pre-computed runner-side so the loop pulls failure logs from the **root-cause** run, not a downstream `skipped` step. See [Auto-mode round-trips](#auto-mode-round-trips) for the Reglyze-fix aggregation rule. |
| `CiFailureLogResolved { req_id, log?, run_id_used? }` | **no** — req/resp | Reply to `CiFailureLog`. `log` is the same ~8 KB tail as `GhFailureLogFetched`; `run_id_used` echoes the run id the runner actually inspected, especially useful when the request was made with `run_id: None` (auto-mode 3.1 re-resolves from the runner-side cache). |

#### SaaS → Runner

| Variant | Reliable? | When sent |
|---|---|---|
| `StartAgent { agent_id, plan_name, task_id, prompt, cwd, driver, effort?, max_budget_usd? }` | yes | Dashboard user clicked "Start" (or "Continue" / "Retry"). Runner spawns the agent via the same `branchwork-server session` supervisor as self-hosted. |
| `KillAgent { agent_id }` | yes | Dashboard user clicked "Kill". Runner forwards a `Kill` over the session IPC. |
| `ResizeTerminal { agent_id, cols, rows }` | yes | Browser resized its xterm.js view. Runner forwards a `Resize` over the session IPC. **Note:** semantically idempotent — duplicated replays are harmless. |
| `AgentInput { agent_id, data }` | **no** (best-effort) | Keystrokes from the browser (base64). Runner forwards as `Input` over the session IPC. Loss on disconnect matches user expectations: typed characters that never reached the agent are not replayed. |
| `TerminalReplay { agent_id, from_offset }` | yes | A reconnecting browser asked the server for backfill from a byte offset; the runner serves the missing range from its local `<socket>.log`. |
| `ListFolders { req_id }` | **no** — req/resp | Dashboard hit a synchronous folder-listing HTTP endpoint and the org has a runner attached. Server mints `req_id`, registers a oneshot, sends best-effort, awaits with a timeout. See [Request/response frames](#requestresponse-frames). |
| `CreateFolder { req_id, path, create_if_missing? }` | **no** — req/resp | Dashboard hit a synchronous folder-creation HTTP endpoint with a target path. Same correlation pattern as `ListFolders`; the runner replies with `FolderCreated`. `create_if_missing=false` (the default for older runners that omit the field) means existence-check only; `true` does `mkdir -p`. |
| `GetDefaultBranch { req_id, cwd }` | **no** — req/resp | Resolve the canonical default branch for `cwd` on the runner host. The runner runs the same three-step algo as the local `git_default_branch` helper: `git symbolic-ref --short refs/remotes/origin/HEAD` → strip `origin/`; fall back to `git rev-parse --verify --quiet master` then `main`; otherwise `None`. Used to seed the merge-target dropdown and to gate `should_record_ci_run` after a merge. See [Branch and merge round-trips](#branch-and-merge-round-trips). |
| `ListBranches { req_id, cwd }` | **no** — req/resp | Enumerate local branches in `cwd` so the dashboard can populate the merge-target chevron dropdown. The runner runs `git branch --format='%(refname:short)'`, sorts alphabetically, and replies with the full list — the server filters out the default branch (implicit) and the agent's task branch (self-merge nonsense) before returning the JSON to the browser. |
| `MergeBranch { req_id, cwd, target, task_branch }` | **no** — req/resp | Server-internal follow-up after the user clicks Merge. The runner runs the same five-step git sequence the local `merge_agent_branch` performs (`rev-list --count` → `checkout target` → `merge task_branch --no-edit` → best-effort `branch -d` → `rev-parse HEAD`) and replies with `MergeResult`. The server pre-resolves `target` via `resolve_merge_target` (a pure function) before sending. |
| `PushBranch { req_id, cwd, branch }` | **no** — req/resp | Server-internal follow-up to a successful `MergeBranch`, conditionally sent when `ci::should_record_ci_run(target, default_branch) == true`. The runner runs `git push origin <branch>` and replies with `PushResult`. Split out from `MergeBranch` so the gate stays a pure function on the SaaS side and the side effect (and `gh` dependency) stays on the runner side. |
| `GhRunList { req_id, cwd, sha }` | **no** — req/resp | The CI poller's per-row probe. The runner runs `gh run list --commit <sha> -L 1 --json databaseId,status,conclusion,url` and replies with `GhRunListed`. Sent from a background tokio task on a ~30 s cadence, not a live HTTP caller — see [CI status round-trips](#ci-status-round-trips). |
| `GhFailureLog { req_id, cwd, run_id }` | **no** — req/resp | The dashboard's "view CI failure" path. The runner runs `gh run view <run_id> --log-failed`, tail-trims to ~8 KB (mirroring `ci.rs::fetch_failure_log`), and replies with `GhFailureLogFetched`. Used by the Fix-CI flow to embed the failure tail in the recovery agent's prompt. |
| `MergeAgentBranch { req_id, agent_id, into? }` | **no** — req/resp | Auto-mode loop (or HTTP merge button via the dispatch shim) asked the runner to merge `agent_id`'s task branch. The runner already knows the agent's `cwd` (it spawned it), so the wire only carries the agent id and an optional dropdown override; the runner runs the same target-resolution + 5-step git sequence as `merge_agent_branch_inner` and replies with `AgentBranchMerged`. Naming note: `MergeAgentBranch` is the high-level agent-aware variant; the lower-level `MergeBranch` (cwd / target / task_branch) is the per-merge git primitive used by the same merge button in the merge-target plan. See [Auto-mode round-trips](#auto-mode-round-trips). |
| `HasGithubActions { req_id, agent_id }` | **no** — req/resp | Auto-mode loop's CI gate probe. The runner globs `cwd/.github/workflows/*.{yml,yaml}` and replies with `GithubActionsDetected`. Used to decide whether to wait for CI or advance to the next task immediately after merge. |
| `GetCiRunStatus { req_id, plan_name, task_number, merged_sha }` | **no** — req/resp | Auto-mode loop polls per-SHA aggregate CI status. The runner runs `gh run list` + per-skipped-run `gh run view` to enumerate workflow runs and inspect job-level skip causes (so a `deploy` `skipped` *because* `tests` failed upstream isn't read as success — the **Reglyze fix**), then applies the aggregation rule and replies with `CiRunStatusResolved`. Cached runner-side for ~10 s so a tight poll loop doesn't hammer `gh`. See [Auto-mode round-trips](#auto-mode-round-trips). |
| `CiFailureLog { req_id, plan_name, run_id? }` | **no** — req/resp | Auto-mode loop asks for the failure log. With `run_id=Some(id)` the runner shells `gh run view <id> --log-failed` directly (also the path the existing UI tooltip uses); with `run_id=None` the runner re-resolves the latest aggregate's `failing_run_id` from its in-memory cache (auto-mode 3.1, where the loop has lost the run id by the time it sees a `Red` outcome). Reply is `CiFailureLogResolved`. |

#### Bidirectional

| Variant | Reliable? | When sent |
|---|---|---|
| `Ack { ack_seq }` | best-effort | Acknowledges a single reliable frame after the receiver has persisted it. The sender then marks that outbox row `acked=1`. Always sent on duplicates too, so a lost ACK self-heals. |
| `Ping {}` | best-effort | Heartbeat probe. The runner sends one every ~15 s; the server replies inline. Missed pongs are not directly observable — the WS-level close on read timeout is what triggers reconnect. |
| `Pong {}` | best-effort | Reply to `Ping`. |
| `Resume { last_seen_seq }` | best-effort | Sent immediately after a (re)connect by **both** sides. Tells the peer "replay every reliable frame whose `seq > last_seen_seq`." `last_seen_seq` is read from the local `seq_tracker` row for the peer (or `0` for a fresh runner). |

### Request/response frames

`ListFolders`/`FoldersListed` and `CreateFolder`/`FolderCreated` form
**request/response pairs** over the otherwise event-stream-only WS — a
deliberate deviation from the rest of the catalogue, where every
state-changing frame is reliable and every stream frame is fire-and-forget.
These four are best-effort *and* explicitly correlated by `req_id`.

The pattern:

1. The dashboard hits a synchronous HTTP endpoint (e.g. the folder picker
   for "new project on runner").
2. The server mints a fresh `req_id`, registers a `tokio::sync::oneshot`
   sender in a per-runner pending-requests map keyed by that `req_id`, and
   sends the request frame **best-effort** (no outbox, no `seq`).
3. The server `await`s the oneshot with a short timeout — a few seconds,
   so the blocked HTTP caller hasn't given up yet.
4. When the runner's reply arrives, the WS handler matches `req_id` to a
   pending oneshot, removes the entry from the map, and forwards the
   payload through the oneshot. The HTTP handler unblocks and returns.
5. On timeout *or* on a successful reply, the entry is dropped from the
   map. A late reply (e.g. delivered after a WS reconnect) finds nothing
   waiting and is silently discarded.

**Why not outbox-backed delivery?** The reliable variants above
(`AgentStarted`, `TaskStatusChanged`, …) are durable side effects: the
dashboard learns about them eventually even if the runner reconnects
between sender and receiver. A folder listing is the opposite — it is
tied to a *live HTTP caller* whose request will time out in single-digit
seconds. Replaying it across a 30-second reconnect window would deliver
an answer to a caller that is no longer there, while the new browser tab
has already retried and gotten a fresher answer. The req-id-correlated
best-effort design fails fast and lets the caller retry, which matches
the user's actual expectation of a "Refresh" button.

Today only the SaaS side initiates these pairs. The pattern is
direction-agnostic — a runner-initiated request would mirror the same
pending-requests map on the runner side.

### Branch and merge round-trips

These six variants — `GetDefaultBranch`/`DefaultBranchResolved`,
`ListBranches`/`BranchesListed`, `MergeBranch`/`MergeResult`,
`PushBranch`/`PushResult` (eight wire variants total) — implement the
merge button + branch-picker dropdown when the org's project lives on a
runner instead of the SaaS host. They all follow the
[request/response pattern](#requestresponse-frames); only the
SaaS-side glue and the runner-side side effects are specific.

End-to-end flow for a single merge:

1. **Dropdown population.** The dashboard hits
   `GET /api/agents/<id>/merge-targets`. The server checks
   `dispatch::org_has_runner`; if true it sends `GetDefaultBranch` and
   `ListBranches` to the runner (in parallel — the two oneshots have
   independent `req_id`s) and folds the replies into the JSON response
   `{ default, available[] }`.
2. **User clicks Merge.** The dashboard `POST`s
   `/api/agents/<id>/merge` with an optional `{ "into": "<branch>" }`
   body. Server resolves the actual target via the pure
   `resolve_merge_target(explicit, default, cwd)` function — explicit
   wins if it resolves, otherwise the default, otherwise `"main"`.
3. **MergeBranch.** Server sends `MergeBranch { target, task_branch }`
   to the runner. The runner walks the five-step sequence
   (`merge_agent_branch` mirror) and replies with a tagged
   `MergeResult.outcome`. The server maps the arms:
   - `ok { merged_sha }` → 200, broadcast `agent_branch_merged`, fall
     through to step 4.
   - `empty_branch` → 409 with the canonical "task branch has no
     commits" message.
   - `conflict { stderr }` → 409 with the captured conflict text.
   - `checkout_failed { stderr }` / `other { stderr }` → 500.
4. **PushBranch (gated).** Only if
   `ci::should_record_ci_run(target, default_branch) == true` — i.e. the
   merge landed on the canonical default branch — does the server send
   `PushBranch`. The runner runs `git push origin <branch>` and replies
   with `PushResult`. The server logs failures but does not surface
   them to the dashboard (CI will retry on the next merge).
5. **ci_runs row + poller arming.** Server inserts the `ci_runs` row
   keyed on `merged_sha`. The CI poller picks it up on the next 30 s
   tick and starts the [CI status round-trips](#ci-status-round-trips)
   below.

The split between `MergeBranch` and `PushBranch` is deliberate:
`should_record_ci_run` is a pure function over the resolved target plus
the canonical default, so it stays on the SaaS side; the side effect
(and the `git push` dependency) stays on the runner side. The runner
never decides whether to push.

### CI status round-trips

These four variants — `GhRunList`/`GhRunListed` and
`GhFailureLog`/`GhFailureLogFetched` — move the last `gh`-CLI
dependencies off the SaaS host and onto the runner. They follow the
same [request/response pattern](#requestresponse-frames), with one
twist: the senders are not live HTTP callers, so the rationale for
best-effort delivery is different.

| Pair | Sender | Cadence |
|---|---|---|
| `GhRunList` / `GhRunListed` | the CI poller in `ci.rs::poll_once` | one frame per pending `ci_runs` row, every `POLL_INTERVAL_SECS` (30 s) |
| `GhFailureLog` / `GhFailureLogFetched` | `api/ci.rs::failure_log` (HTTP `GET /api/ci/<run_id>/failure-log`) | on demand, with a write-through cache in `ci_runs.failure_log` |

`GhRunList` is best-effort because the next 30 s tick will retry the
same query — outbox-backed delivery would buy at most one cycle of
latency at the cost of replaying stale probes after a long
disconnect. `GhFailureLog` is tied to a live HTTP caller exactly like
the folder pairs.

The runner-side handlers shell out to `gh` directly and stream the
JSON / log bytes back unchanged. `GhRun` carries gh's native
`databaseId` field name across the wire so a runner that piped
`gh run list ...` straight to the WS would also work — the SaaS
`fetch_run` keeps the same struct definition, so there's no
double-translation. `GhFailureLogFetched.log` is tail-trimmed on the
runner to the same 8 KiB cap (`CAP_BYTES`) the local `fetch_failure_log`
uses; further trimming on the SaaS side is unnecessary.

Cache and persistence semantics are unchanged from the local path:
`ci_runs.status` advances through `pending` → terminal on the next
poll cycle that sees a non-`in_progress` `conclusion`, and the failure
log is written through to `ci_runs.failure_log` on the first successful
`gh` call so subsequent dashboard fetches return immediately without a
runner round-trip.

### Auto-mode round-trips

These eight variants — `MergeAgentBranch`/`AgentBranchMerged`,
`HasGithubActions`/`GithubActionsDetected`,
`GetCiRunStatus`/`CiRunStatusResolved`,
`CiFailureLog`/`CiFailureLogResolved` — back the auto-mode loop's
merge → CI gate → fix sequence. They follow the same
[request/response pattern](#requestresponse-frames) as the folder
pairs (single live SaaS-side caller awaiting a oneshot, late replies
silently dropped), with two distinguishing properties:

1. **Agent-aware addressing.** Where the merge-target plan's
   `MergeBranch` carries `cwd / target / task_branch`,
   `MergeAgentBranch` carries only `agent_id` plus an optional `into`
   override. The runner already knows the agent's `cwd` (it spawned
   it) and resolves the merge target itself. This keeps the auto-mode
   server-side dispatcher trivial — it forwards a single id — and
   lets the runner reuse the `merge_agent_branch_inner` logic without
   the server first round-tripping a bunch of agent metadata.

2. **CI aggregation is runner-side.** `CiRunStatusResolved` carries a
   pre-computed [`CiAggregate`](#ciaggregate) instead of a list of raw
   `GhRun` rows. The aggregation rule (below) is the **Reglyze fix**:
   a multi-workflow CI where a downstream `deploy` is `skipped`
   because an upstream `tests` failed must aggregate to `failure`,
   never `success`. Computing this on the runner means the SaaS
   server doesn't reapply the same heuristic in two places, and the
   `failing_run_id` field points at the actual root-cause run so the
   loop pulls the right log.

#### `CiAggregate`

```
status:       queued | in_progress | completed
conclusion:   success | failure | cancelled | timed_out | ... | None
runs:         [CiRunSummary]
failing_run_id: Option<String>
```

#### Aggregation rule

- If any run has `conclusion="failure" | "cancelled" | "timed_out"` ⇒
  aggregate `conclusion="failure"`.
- If any run is `status!="completed"` and there is no already-failing
  run ⇒ aggregate `status="in_progress"` (still polling).
- If all runs are `conclusion="success"` OR `conclusion="skipped"`
  with `skipped_due_to_upstream=false` ⇒ aggregate
  `conclusion="success"`.
- A run with `conclusion="skipped"` and `skipped_due_to_upstream=true`
  is **never** treated as success — the runner walks the workflow-
  graph in dependency order so an upstream failure poisons every
  downstream skip.
- `failing_run_id` is the first non-skipped failing run in
  workflow-graph order. The runner fills `skipped_due_to_upstream` by
  inspecting `gh run view <id> --json jobs` for each `skipped` run
  (every job `skipped` and at least one job's `steps[]` reports the
  skip was caused by `needs:` failure ⇒ upstream-poisoned).

#### `CiFailureLog`'s two modes

- `run_id: Some(id)` is the existing UI tooltip path: the runner
  shells `gh run view <id> --log-failed` against that specific id.
- `run_id: None` is the auto-mode 3.1 path: by the time the loop sees
  a `Red` outcome it has dropped the run id, so the runner
  re-resolves `failing_run_id` from the latest cached aggregate for
  `plan_name`. `CiFailureLogResolved.run_id_used` echoes back which
  run was actually inspected so the audit log stays honest.

#### Best-effort rationale

All eight variants are best-effort. Live HTTP callers in the merge
button path and the auto-mode loop's wait-and-poll cadence both treat
a missed reply the same way: time out, retry on the next tick. Outbox
replay would deliver stale answers after the caller had already moved
on.

### `DriverAuthStatus` states

`DriverAuthInfo.status` (carried by `RunnerHello.drivers` and
`DriverAuthReport.drivers`) is itself a tagged union on `"state"`. The
dashboard renders each state with a different affordance.

| State | When sent |
|---|---|
| `not_installed` | The driver's CLI binary is missing from `PATH` on the runner — the dashboard offers an install link instead of a Start button. |
| `unauthenticated { help? }` | The CLI is installed but no credentials are configured. `help` is a one-line hint shown to the user (e.g. "run `claude auth login`"). |
| `oauth { account? }` | OAuth-style auth is in place; `account` is the email or login the dashboard shows next to the driver name. |
| `api_key` | An API key is configured. The dashboard shows a green check; we deliberately don't echo any key fragment. |
| `cloud_provider { provider }` | Auth is delegated to a cloud SDK (e.g. AWS Bedrock); `provider` names the SDK so the dashboard can pick the right icon. |
| `unknown` | Default fallback when the runner can't classify the driver's auth — also used by older runners after a server-side enum addition. |

### Connection establishment, in order

```
runner ─► server : HTTP upgrade /ws/runner?token=…
server validates token; on success completes upgrade and begins reader loop

runner ─► server : RunnerHello       (reliable, seq=1)
runner ─► server : DriverAuthReport  (reliable, seq=2)
runner ─► server : Resume{last_seen_seq=N}   (best-effort, server's last seq runner saw)
server ─► runner : Resume{last_seen_seq=M}   (best-effort, runner's last seq server saw)
…both sides replay outbox entries > M / > N…
…steady-state event flow…
```

`runner_ws.rs` sends its `Resume` and replays `inbox_pending` from inside the
"first message" branch, so the runner's `RunnerHello` doubles as the trigger
for both. The runner mirror is in `branchwork_runner.rs` around the
`send_reliable(hello)` / `send_reliable(auth_report)` block.

---

## 3. Dashboard WebSocket

**Source:** `server-rs/src/ws.rs`.

**Transport:** a single WebSocket per dashboard tab, mounted at
`GET /ws`. Connections are read-only from the browser's side — the
server ignores any inbound frames except WS-level pings — and consume
a `tokio::sync::broadcast` channel of capacity 256 created once at
boot. Per-client backpressure is best-effort: a slow consumer that
falls behind by more than 256 frames is **lagged** by the broadcast
channel and the next read returns `RecvError::Lagged(skipped)`; the
handler logs the gap and keeps streaming. There is no replay.

### Envelope

Every frame the server emits is JSON in the canonical
[`broadcast_event`](../../server-rs/src/ws.rs) shape:

```json
{
  "type": "<event-name>",
  "data": { ... },
  "timestamp": "2026-05-04T12:34:56.789Z"
}
```

- `type` is the discriminator the SPA dispatches on
  (`web/src/stores/ws-store.ts::handleWsMessage` is one big
  `switch (msg.type)`).
- `data` is the per-event payload.
- `timestamp` is RFC-3339 UTC, set at broadcast time. It is not
  used for ordering — the broadcast channel preserves emission order
  for a given subscriber — only for display.

A separate `{"type": "connected", "timestamp": "..."}` greeting is
sent once on upgrade with no `data` field; the SPA treats it as a
no-op.

### Auto-mode events

The auto-mode loop and the Stop-hook handler emit a small family of
events for the dashboard's auto-mode pill, audit log, and (Phase 6.1)
in-flight notifications. The unattended-auto-mode plan adds
`auto_finish_triggered`; the rest are listed for context.

| `type`                  | Source                                          | `data` shape (notable fields) |
|---|---|---|
| `auto_mode_paused`      | `auto_mode.rs::run_merge_step`, `hooks.rs::handle_stop_hook` | `{ plan, task?, reason, target? }` (merge path) or `{ plan, task?, reason, files }` (Stop-hook dirty-tree path). `reason` is one of `agent_left_uncommitted_work`, `merge_conflict`, `merge_failed: <msg>`, …; `files` carries up to 5 dirty paths. |
| `auto_mode_merged`      | `auto_mode.rs::run_merge_step`                  | `{ plan, task, sha, target }` — emitted after a successful merge before the CI gate. |
| `auto_mode_resumed`     | `api/plans.rs::auto_mode_resume`                | `{ plan, last_completed_task }` — user clicked Resume; `pausedReason` cleared server-side. |
| `auto_finish_triggered` | `hooks.rs::handle_stop_hook` (this plan, T2.5)  | `{ agent_id, plan, task, trigger }` — see below. |
| `task_advanced`         | `agents/mod.rs::try_auto_advance` (intra-phase) | `{ plan, from_task, to_tasks: [<num>, …] }` — at least one ready sibling task was claimed and spawned in the same phase. |
| `phase_advanced`        | `agents/mod.rs::try_auto_advance` (next-phase)  | `{ plan_name, from_phase, to_phase }` — note the field is `plan_name`, not `plan`, preserved verbatim from existing dashboards. |

#### `auto_finish_triggered`

Emitted exactly once per agent the moment the unattended Stop-hook
handler decides to finalize that agent (clean tree, auto-mode on,
agent still `running`). The handler also writes an `agent.auto_finish`
audit row with the same `trigger` value in its diff and spawns
`AgentRegistry::graceful_exit`; the `agent_stopped` broadcast still
follows once the PTY actually closes.

```json
{
  "type": "auto_finish_triggered",
  "data": {
    "agent_id": "<uuid>",
    "plan": "<plan-name>",
    "task": "<task-number>",
    "trigger": "stop_hook"
  },
  "timestamp": "2026-05-04T12:34:56.789Z"
}
```

| Field | Type | Notes |
|---|---|---|
| `agent_id` | string (UUID) | The agent that just signalled it was done. Matches `agents.id`. |
| `plan` | string | Plan name (file stem). Matches the `plan` field used by `auto_mode_*` and `task_advanced`, **not** the `plan_name` field used by `phase_advanced`. |
| `task` | string | Task number as it appears in the plan, e.g. `"2.5"`. |
| `trigger` | enum string | `"stop_hook"` for the Claude `Stop`-hook path or `"idle_timeout"` for the idle-poller fallback (gated on `BRANCHWORK_AUTO_FINISH_IDLE=1`; see [reference/configuration.md](../reference/configuration.md#auto-mode-idle-finish) and [ADR 0003](../adrs/0003-unattended-auto-mode.md)). |

The handler dedupes per-agent (`AppState::auto_finish_dedupe`), so
duplicate Stop deliveries collapse to one broadcast. The dirty-tree
branch never emits this event — it emits `auto_mode_paused` instead
and leaves the agent running so the user can intervene.

Phase 6.1 is the consumer plan: the SPA will fold this into the
auto-mode pill and the audit-log "Auto-finished agent" row
(`web/src/components/AuditLog.tsx::ACTION_LABELS`). Until then the
event ships and is silently ignored by the SPA's default switch arm.

---

## 4. Versioning policy

The protocols are pre-1.0 and not yet under a formal compatibility contract,
but the wire format already gives us enough room to evolve safely if we
follow these rules.

### What's safe

- **Adding a new `WireMessage` variant.** Both sides match exhaustively, but
  the server's match arm has a default `{}` for variants it doesn't expect
  from runners (see the trailing arm in `handle_runner_message`). A new
  variant is silently ignored by an older peer; the new peer must tolerate
  the silence (treat the message as best-effort or fall back to a
  pre-existing variant).
- **Adding an `Option<…>` field** with `#[serde(skip_serializing_if = "Option::is_none")]`.
  Older peers ignore unknown fields by default (`serde_json` is lenient);
  newer peers see `None` from older senders. `AgentStopped.cost_usd`,
  `AgentStopped.stop_reason`, `StartAgent.effort`, `StartAgent.max_budget_usd`
  were all added this way.
- **Adding a new `DriverAuthStatus` state.** Same reasoning — the tagged
  union over `state` admits new tags as long as both sides treat unknown tags
  as `Unknown`/no-op.
- **Adding a new session `Message` variant** at the tail of the enum. `postcard`
  encodes the discriminant as a varint, so appending is binary-stable. An
  older peer that receives a new variant will fail `decode` with `InvalidData`
  — so adders must gate use behind a capability check (see below) until the
  rollout is complete.

### What's not safe

- **Reordering or removing variants** in either enum. `postcard` keys on the
  declaration order for the discriminant; reordering changes wire bytes
  silently. Removal breaks decode for in-flight outbox replays. Always append.
- **Changing a field's type** (e.g. `u16` → `u32`, `String` → `Vec<u8>`).
  Both encodings reject mismatched shapes.
- **Removing or renaming a JSON tag.** `WireMessage` discriminates on
  `"type": "snake_case"`; renaming breaks every older peer mid-flight.
- **Tightening `MAX_FRAME_BYTES` downward.** Existing senders may already be
  emitting frames near the cap.

### Forward/backward compatibility in practice

- **Newer server, older runner.** Server must not depend on runner-side
  fields it added. Reliable messages still flow because the outbox layer is
  field-agnostic. New SaaS→runner variants are gated on the `version` string
  in `RunnerHello` — the dashboard either disables the corresponding UI or
  falls back to a pre-existing variant.
- **Older server, newer runner.** The runner sends best-effort the moment it
  starts; reliable frames it sends with new fields are accepted by the
  older server (unknown fields ignored). The runner must tolerate **not**
  receiving new SaaS→runner messages and must not require any handshake
  beyond `RunnerHello`/`Resume` to make progress.
- **Newer server, older session daemon (or vice versa).** The detached
  daemon is upgraded out of band — when the server is upgraded the running
  daemons keep using the old binary they were spawned from. Compatibility
  is maintained by the rules above (append-only `Message` variants, no
  field-type changes); a fresh daemon is launched on the next agent start.
  See [session-daemon.md](session-daemon.md) for the lifecycle.

The versioning hooks we already have:

- `RunnerHello.version` — server-side capability gating per-runner.
- `Envelope.runner_id` plus the per-peer `seq_tracker` — reconnect across
  upgrades is invisible to consumers.
- `outbox` tables persist across server restarts — an upgrade-restart-replay
  cycle delivers nothing twice and loses nothing reliable.

When we cut 1.0 we will freeze the on-wire shape and add an explicit
`protocol_version: u32` field to `RunnerHello` and to the session-IPC
handshake. Until then, follow the append-only rules above.
