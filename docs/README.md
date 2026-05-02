# Branchwork Documentation

Branchwork ships as three cooperating binaries: the **dashboard server**
(`branchwork-server`), a per-session **supervisor daemon** (`branchwork-server
session`, also installable as the standalone `session_daemon`), and — in
SaaS mode only — the **runner** (`branchwork-runner`) that executes agents
on behalf of a remote dashboard. This index links every planned page so you
can find what you need without reading the source.

Pages marked _(stub)_ do not exist yet — they are tracked by the
`architecture-docs` plan and will be filled in over the following phases.

## Start here

- [quickstart.md](quickstart.md) — five-minute self-hosted path:
  install, run, open the dashboard, create your first plan, watch an
  agent survive a server restart.
- [user-guide.md](user-guide.md) — complete walkthrough of the
  dashboard, plan authoring, agent lifecycle, and common workflows.

## Architecture

The three-binary split, wire protocols, and storage model.

- [architecture/overview.md](architecture/overview.md) — three-binary
  diagram, data flow, self-hosted vs. SaaS deployment shapes.
- [architecture/server.md](architecture/server.md) — dashboard
  server: HTTP API, WebSocket fan-out, file watcher, auto-status, hooks.
- [architecture/session-daemon.md](architecture/session-daemon.md) —
  per-session supervisor: fork+setsid on Unix, DETACHED_PROCESS on
  Windows, PTY I/O, log-replay on reattach.
- [architecture/runner.md](architecture/runner.md) — SaaS runner:
  authenticated WebSocket upstream, runner ID persistence, driver auth
  reporting, outbox/ACK with at-least-once delivery, local agent
  spawning via the shared session daemon.
- [architecture/protocols.md](architecture/protocols.md) _(stub)_ — session
  IPC frames, SaaS `WireMessage` JSON, dashboard WS events, hook POST
  shape.
- [architecture/persistence.md](architecture/persistence.md) _(stub)_ —
  SQLite/Postgres schema, org multi-tenancy, outbox tables, migrations,
  what survives restart.

## Reference

Flag-level detail, file layouts, schemas.

- [reference/cli.md](reference/cli.md) _(stub)_ — every flag and subcommand
  across `branchwork-server`, `branchwork-server session`, and
  `branchwork-runner`.
- [reference/configuration.md](reference/configuration.md) _(stub)_ —
  `~/.claude/` layout, environment variables (SMTP, budgets, auth cookie,
  etc.), `branchwork.toml`.
- [reference/plan-schema.md](reference/plan-schema.md) _(stub)_ — canonical
  YAML plan schema; supersedes the root `plan.yaml` sample.
- [reference/drivers.md](reference/drivers.md) _(stub)_ — `AgentDriver`
  trait, `DriverCapabilities`, authoring a new driver, registering in
  `DriverRegistry`, MCP auto-injection.

## Operations

Deployment, upgrades, day-2 ops.

- [operations/self-hosted.md](operations/self-hosted.md) _(stub)_ — single
  binary on a laptop or server, SQLite, local agents.
- [operations/saas-runner.md](operations/saas-runner.md) _(stub)_ —
  runner token issuance, connection URL, reconnect/backoff, PATH
  requirements, systemd unit.
- [operations/docker.md](operations/docker.md) _(stub)_ —
  `deploy/Dockerfile`, compose overlays, GHCR images.
- [operations/helm-terraform.md](operations/helm-terraform.md) _(stub)_ —
  Helm chart (`sqlite` vs `postgres` modes), Terraform ECS Fargate module.
- [operations/upgrades-and-migrations.md](operations/upgrades-and-migrations.md)
  _(stub)_ — upgrade path, SQLite→Postgres migration, rollback, backups.

## Troubleshooting & glossary

- [troubleshooting.md](troubleshooting.md) _(stub)_ — common failures:
  empty-branch merge guard, stale merge banner, auto-status false
  positives, supervisor heartbeat, MCP connection issues.
- [glossary.md](glossary.md) _(stub)_ — plan / phase / task / agent /
  driver / runner / org / verdict — the vocabulary used throughout.

## Historical design & repro notes

These are evidence artifacts from past bug investigations. They stay
alongside the architecture docs and are linked from the relevant
troubleshooting or architecture pages as those land.

- [design-produces-commit.md](design-produces-commit.md) — design note for
  the per-task `produces_commit` field that gates the Merge button.
- [repro-navbar-false-completion.md](repro-navbar-false-completion.md) —
  auto-status file-existence heuristic false positives.
- [repro-plan-done-drift.md](repro-plan-done-drift.md) — frontend
  `doneCount` drift in `patchTaskStatus`.
- [repro-stale-merge-button.md](repro-stale-merge-button.md) — Merge
  banner firing on task branches with zero commits.
