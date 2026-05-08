# Branchwork Documentation

Branchwork ships as three cooperating binaries: the **dashboard server**
(`branchwork-server`), a per-session **session daemon** (`branchwork-server
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

## Integrations

- [bob-shell-integration.md](bob-shell-integration.md) — Bob Shell
  integration guide: connect Bob to Branchwork's MCP server, query
  plans and tasks using natural language, update status, and manage
  your workflow through Bob's conversational interface.

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
- [architecture/protocols.md](architecture/protocols.md) — session IPC
  frames (`postcard` over UDS / named pipe), SaaS runner `WireMessage`
  JSON envelopes with reliable/best-effort split and outbox/ACK/replay
  semantics, plus the versioning policy.
- [architecture/persistence.md](architecture/persistence.md) — SQLite
  schema, org multi-tenancy, outbox tables, idempotent migrations, the
  per-agent sibling files, and a four-way restart matrix covering every
  persisted artifact.
- [architecture/deploy.md](architecture/deploy.md) — Docker image
  pipeline (zigbuild stage 2, per-arch parallel build matrix +
  manifest job in `docker.yml`), the consumer tag contract that lets
  the Hetzner deploy and `docker pull` resolve a single tag to the
  right per-arch slice, and the canonical pull-and-run smoke fixtures.

## Reference

Flag-level detail, file layouts, schemas.

- [reference/cli.md](reference/cli.md) — every flag and subcommand
  across `branchwork-server`, `branchwork-server session`, and
  `branchwork-runner`.
- [reference/configuration.md](reference/configuration.md) —
  `~/.claude/` layout, the runner's `~/.branchwork-runner/` and
  `<cwd>/.branchwork-runner-sessions/`, every environment variable
  the source actually reads (`BRANCHWORK_*`, `SMTP_*`, driver API
  keys), and a list of variables that look like config but aren't.
- [reference/plan-schema.md](reference/plan-schema.md) — canonical
  YAML plan schema (every field on `YamlPlan` / `YamlPlanPhase` /
  `YamlPlanTask`, the Markdown fallback's heuristics, `produces_commit`,
  project inference, `created_at`). Supersedes the in-repo sample at
  the root [`plan.yaml`](../plan.yaml).
- [reference/drivers.md](reference/drivers.md) — per-driver reference
  (Claude, Aider, Codex, Gemini): install command, auth probe,
  ready signal, cost parser, graceful-exit sequence, known quirks.
  Plus the `AgentDriver` trait, `DriverCapabilities`, and how to
  author a fifth driver and register it in `DriverRegistry`.

## Operations

Deployment, upgrades, day-2 ops.

- [operations/self-hosted.md](operations/self-hosted.md) — single
  binary on a laptop or server, SQLite, local agents. Includes
  copy-pasteable systemd unit and launchd plist, Windows/NSSM notes,
  log paths, backup, and the stop-swap-start upgrade procedure.
- [operations/saas-runner.md](operations/saas-runner.md) — runner
  token issuance, install one-liner, network requirements
  (outbound WSS only), systemd / launchd / NSSM units, log paths,
  token rotation, multi-runner setups, and connect/reconnect
  troubleshooting.
- [operations/docker.md](operations/docker.md) — `deploy/Dockerfile`,
  the four compose overlays (base, SaaS, e2e, prod) with per-overlay
  "use this when…" guidance, every env var grouped by the file that
  passes it through, the named-volume / host-bind matrix, run and
  tear-down recipes, and the ADR 0005 e2e-fixture rule.
- [operations/helm-terraform.md](operations/helm-terraform.md) —
  Helm chart values reference (image, database mode, persistence,
  ingress, autoscaling, SMTP) and Terraform AWS ECS Fargate module
  variables / outputs, with pointers to `example.tfvars` and the
  Postgres-not-yet-implemented caveat.
- [operations/upgrades-and-migrations.md](operations/upgrades-and-migrations.md)
  — pre-upgrade checklist, idempotent `db::migrate` model, three-axis
  binary version skew (server / runner / session daemon), explicit
  list of what does **not** work after a downgrade, and the
  SQLite→Postgres caveat (Helm stub only — Rust still SQLite-only).

## Troubleshooting & glossary

- [troubleshooting.md](troubleshooting.md) — FAQ-shaped index of
  common failures grouped by symptom: stale merge banner, doneCount
  drift, auto-status false positives, blank session terminal after
  reconnect, plan file edited on disk not picked up, runner won't
  connect, driver auth fails. Links every existing repro/design note
  in `docs/`.
- [glossary.md](glossary.md) — single-page definitions for the
  vocabulary used throughout the docs (server, runner, session daemon,
  driver, plan, phase, task, project, effort, `produces_commit`,
  auto-status, check agent, outbox, supervisor, …).

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
