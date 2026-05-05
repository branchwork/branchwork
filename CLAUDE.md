# Branchwork — agent rules

Project-wide guardrails every agent (and every human) must follow when
working in this repo. Loaded automatically into Claude Code agents'
context.

## E2E and smoke tests must run inside containers

Branchwork agents run on the same host as the production
`branchwork-server` instance that is supervising them. Any test that
spawns long-running `branchwork-server` or `branchwork-runner`
processes — or that touches host processes by pattern (`pgrep -f`,
`killall`, broad `kill`) — risks killing the agent's own supervisor
and breaking the auto-mode chain.

**Rule:** end-to-end and smoke tests that need a live server / runner
**must** spin them up via Docker Compose (the existing
`deploy/docker-compose.e2e.yml` is the canonical fixture; it has a
`saas` profile for the server+runner setup), use a per-run unique
compose project name, and tear down with
`docker compose -p <project> down -v`. Containerised cleanup is
intrinsically scoped — it cannot reach host processes.

**Forbidden patterns in test code** (CI grep gate, see ADR 0005):

- `pgrep -f "branchwork-server"` (any form, including `… session`,
  `… --port …`, etc.). Always matches the host's production
  supervisor.
- `killall branchwork-server` / `killall branchwork-runner`.
- Any kill-by-pattern that matches more than the test's own PIDs.

**Allowed scoping mechanisms** if a test genuinely needs to manage a
process inventory (preferred order):

1. Run the test inside a Docker Compose project — `docker compose -p
   <project> down -v` is the only cleanup needed.
2. Record PIDs the test itself spawned to a temp file
   (`echo $! >> .test.pids`), then `xargs -r kill < that-file` on
   teardown.
3. Filter by a unique env var or argument that only the test
   binaries carry (e.g. `BRANCHWORK_TEST_TAG=<uuid>` and
   `pgrep -f <uuid>`).

The 2026-05-05 incident (an unscoped
`pgrep -f "branchwork-server session"` killed the agent's own
supervisor mid-task, breaking auto-mode for an entire plan) is the
canonical reason this rule exists. See
`docs/adrs/0005-e2e-tests-must-be-containerized.md` for the full
write-up.

## Other rules

(Add more as project conventions are codified.)
