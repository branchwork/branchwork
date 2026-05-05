# ADR 0005 — E2E and smoke tests must run inside containers

- **Status:** Accepted (2026-05-05)
- **Authors:** cpo
- **Decision driver(s):** 2026-05-05 incident — an agent-driven smoke test ran `pgrep -f "branchwork-server session" | xargs -r kill` to clean up its sandbox, killed the agent's own supervisor (`status=failed` at 09:45:09), and silently broke the auto-mode chain for the entire `saas-folder-listing-via-runner` plan; rule needed to prevent recurrence.

## Context

Branchwork's auto-mode loop spawns task agents on the same host that runs the production `branchwork-server`. Each agent's PTY is owned by a detached supervisor process visible to `pgrep` as `branchwork-server session …` on the host's process list. Every other agent's supervisor is *also* visible.

When a task spawns the same binary as part of its work — typically because it's smoke-testing the server + runner against a real binary — the test's own cleanup must distinguish "processes the test spawned" from "processes that already existed on the host". A pattern-match cleanup that doesn't make that distinction is lethal.

### 2026-05-05 incident

While executing `saas-folder-listing-via-runner` task 5.7 ("three-binary smoke test"), the agent ran:

```sh
pgrep -f "branchwork-server session" | xargs -r kill
sleep 1
pgrep -f "branchwork-server.*--port 3199" | xargs -r kill
```

The first `pgrep` is unscoped: it matches **every** session daemon on the host, including the agent's own supervisor. The first `kill` killed the supervisor mid-task. Database state at 09:45:09:

- `agents` row for the task: `status='failed'`, `supervisor_socket=NULL`.
- `task_status` row for `5.7`: `status='in_progress'`, never advanced.
- Plan: `autoMode=true`, `pausedReason=null` — silent dead zone, because `on_task_agent_completed` only fires for clean exits and there is no separate "agent failed" handler that pauses the loop.

By chance no other task agents were running concurrently, so collateral damage was zero. The incident is recoverable but the failure mode is real and easy to reproduce.

The intended cleanup wanted to scope to a single test instance (port `3199`), but the *first* line of the cleanup was unscoped and ran first. The `--port 3199` filter on the second line was effectively a placebo.

## Decision

End-to-end and smoke tests that need a live `branchwork-server` and/or `branchwork-runner` MUST run those binaries inside Docker containers, brought up by Docker Compose, scoped to a per-run unique compose project name. The repo already ships `deploy/docker-compose.e2e.yml` with a `saas` profile that spins up server + runner with a healthcheck — that fixture is the canonical entry point.

Cleanup is `docker compose -p <project> down -v`. The compose project name is the only handle the test needs; it cannot reach host processes.

The forbidden / allowed patterns are codified in the repo-root `CLAUDE.md` so every agent picks them up as context automatically.

## Consequences

- **Tests that previously ran binaries on the host are migrated** to compose-based fixtures. Existing `tests/e2e/run.sh` already follows this pattern (it brings up `deploy/docker-compose.e2e.yml`); new e2e specs reuse it via `--profile saas` and a per-attempt project name.
- **`pgrep -f "branchwork-server"` is forbidden in test code.** A CI grep gate (`grep -rE "pgrep -f .branchwork-server.|killall branchwork-(server|runner)" tests/ web/e2e/`) fails any PR that introduces it. Filed as a follow-up task; not gating this ADR.
- **Per-run unique project name + random port** prevent concurrent smoke tests from colliding on the same host. Ad-hoc shape: `PROJECT="bw-smoke-$(uuidgen | cut -c1-8)"`, `E2E_PORT=$(shuf -i 30000-39999 -n 1)`.
- **Trap-on-EXIT** for cleanup: tests wrap their compose lifecycle in `trap "docker compose -p $PROJECT down -v" EXIT` so an interrupted run still cleans up.
- **Auto-mode silent-stall on failed agents** is a separate, related bug (the 2026-05-05 incident's secondary failure mode — the loop didn't pause when the agent died, just stalled). Tracked separately; not part of this ADR.

## Rejected alternatives

- **Allow host-mode binaries with stricter pgrep patterns.** Rejected: any pattern is a footgun. A future change to the binary's argv (e.g. a new flag landing on the production server) can change the match set in unpredictable ways. Containers are categorical; pattern filters are heuristic.
- **A single shared host port + lock file.** Rejected: still collides with the production server, doesn't scope cleanup, and adds a separate failure mode (stale lock file from a crashed test).
- **Run tests on a separate dedicated host.** Out of scope for this ADR — would solve the problem but the team mostly tests locally; we want local-host parity. The container constraint achieves that.
- **A `BRANCHWORK_TEST_TAG=<uuid>` env var on test binaries + `pgrep -f <uuid>`.** Listed as an *allowed* fallback in `CLAUDE.md` for cases where Docker is genuinely unavailable, but rejected as the default because it requires every test author to remember the convention; containers enforce scoping by construction.
