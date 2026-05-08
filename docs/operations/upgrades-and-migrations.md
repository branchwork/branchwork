# Upgrades and migrations

Upgrade the Branchwork dashboard server, the SaaS runner, and the
per-agent session daemon without losing in-flight agents, plan state,
or the runner outbox. This page is the cross-cutting day-2 reference
that the per-deploy runbooks
([self-hosted](self-hosted.md), [saas-runner](saas-runner.md),
[docker](docker.md), [helm-terraform](helm-terraform.md)) point at when
the operator needs to know **how** the upgrade works, not just **which
command to run**.

Three things to internalise before reading the rest:

1. **`db::migrate` is idempotent and runs on every boot.** Schema is a
   single function in [`server-rs/src/db.rs`](../../server-rs/src/db.rs)
   that does `CREATE TABLE IF NOT EXISTS …` for every table that has
   ever shipped, plus a series of `ALTER TABLE … ADD COLUMN …`
   statements wrapped in `.ok()`. There is no version table, no
   migration directory, and no manual schema-bump step. The
   architecture write-up is in
   [architecture/persistence.md § Schema migrations](../architecture/persistence.md#schema-migrations).
2. **The wire format is append-only and pre-1.0.** Adding a
   `WireMessage` variant or an `Option<…>` field is forward- and
   backward-compatible; reordering, renaming, or changing types is
   not. The contract lives in
   [architecture/protocols.md § Versioning policy](../architecture/protocols.md#4-versioning-policy).
3. **Session daemons outlive their parent.** A `branchwork-server`
   restart does not kill the per-agent supervisor processes
   ([architecture/session-daemon.md](../architecture/session-daemon.md));
   the new binary reattaches via `cleanup_and_reattach` on the way
   back up.

Together those three give Branchwork an upgrade story that is
literally **stop, swap, start** — and the rollback story that is
literally **stop, swap to the old binary, start, accept the
consequences below**.

## Pre-upgrade checklist

Run through this list before swapping any binary.

| Check | Why |
|---|---|
| **Pin a concrete version**, not `:edge` or `master`. Read the diff between your current and target version (`git log v<old>..v<new>` or the GitHub release notes). | `:edge` rolls on every green `master`; mid-upgrade churn is the wrong time to discover a behaviour change. The Docker tag matrix is in [operations/docker.md](docker.md#image). |
| **Snapshot `~/.claude/branchwork.db`** (server) and **`~/.branchwork-runner/runner.db`** (runner). Use `sqlite3 … ".backup …"` for hot snapshots — the recipe is in [self-hosted.md § Backup](self-hosted.md#backup). | Migrations are additive only, but rollback to a binary that pre-dates a column or table leaves it readable but invisible. A snapshot is your only real escape hatch if something in the new release misbehaves on your data. |
| **Snapshot `~/.claude/plans/`** (server). | Plan YAML is the source of truth for plan structure. The DB has only runtime state *about* plans. If the new binary's UI round-trip clobbers an unknown field (see [Rollback](#rollback)), the plan file is the only way back. |
| **Confirm runner ↔ server version skew is acceptable.** The dashboard's runner row carries a `version` field populated from `RunnerHello`; the chip turns yellow on a minor mismatch, red on a major. The classifier is `classify_version_mismatch` in [`saas/runner_ws.rs`](../../server-rs/src/saas/runner_ws.rs). | Branchwork tolerates one direction of skew at a time (see [Binary version skew](#binary-version-skew)) but neither side should be more than one minor away from the other. |
| **Quiesce auto-mode** if you have plans in `auto_mode_enabled = 1`. Toggle them off in the dashboard, or wait for the loop to land on `paused`. | The auto-mode loop runs on the dashboard server. A restart while it is mid-merge cannot lose a merge — the merge is performed by the runner / local git and the result is on disk — but the WS broadcast that tells the SPA the plan advanced is dropped. After restart the next `try_auto_advance` tick picks the work back up. Quiescing first just means fewer surprises in the audit log. |
| **For SaaS deploys, drain in-flight runner reliable frames** by waiting until each runner's `outbox_depth` health metric is `0`. The metric is on the `runners` row and visible in the runner panel. | The outbox is durable and replay-safe (see [architecture/persistence.md](../architecture/persistence.md) and [runner.md § Outbox and replay on reconnect](../architecture/runner.md#outbox-and-replay-on-reconnect)) — draining is not required for correctness, only for a smaller "what's in flight at the moment of upgrade?" surface to reason about. |
| **Note any active fix-CI agents** that started against runs of the failing branch. Their work is on a recovery branch and survives any upgrade, but the link the dashboard renders (`ci_runs.run_url`) is set at insert time and not retried. | Fix-CI is documented in [Dashboard UI audit § Fix CI](../architecture/dashboard-ui-audit.md). The upgrade itself doesn't perturb the recovery branch — a green CI on the next push merges normally. |

If any item is "no" / "skip", write down why before proceeding. The
upgrade is reversible (with caveats) but the audit trail of the
upgrade is not.

## Upgrade by deploy shape

The upgrade procedure depends on **what you used to install**, not on
the version delta. Each runbook has the binary-swap recipe; this page
links out and only adds the cross-cutting commentary.

| Deploy shape | Procedure | Reference |
|---|---|---|
| **Self-hosted (systemd / launchd / NSSM)** | `systemctl stop branchwork`, `install` the new binary over `/usr/local/bin/branchwork-server`, `systemctl start branchwork`. The unit's `Restart=on-failure` covers the rare case where the new binary panics on first boot. | [self-hosted.md § Upgrade procedure](self-hosted.md#upgrade-procedure) |
| **Docker compose** | Bump `BRANCHWORK_VERSION` (or the `image:` tag) in the overlay, then `docker compose -p <project> up -d`. Compose recreates the container in place; the named volume that holds `~/.claude/` carries over. | [operations/docker.md](docker.md) |
| **Helm** | `helm upgrade branchwork ./deploy/helm/branchwork -f my-values.yaml --set image.tag=<new>`. The Deployment rolls the pod; the PVC carries `branchwork.db` across. | [helm-terraform.md § Upgrades](helm-terraform.md#upgrades) |
| **Terraform (ECS Fargate)** | `terraform apply -var image=ghcr.io/branchwork/branchwork:<new>`. The ECS service rolls the task definition; EFS holds `/data`. | [helm-terraform.md § Upgrades](helm-terraform.md#upgrades) |
| **SaaS runner** | Stop the runner unit, swap the `branchwork-runner` binary, start it. The runner reconnects, sends `RunnerHello` with the new version, and the outbox replays anything in flight. | [saas-runner.md](saas-runner.md) |

Two facts are common to every shape:

- **Daemons survive the swap.** The supervisor processes spawned by
  the previous binary are reparented to PID 1 by `setsid` (Unix) or
  detached via `DETACHED_PROCESS` (Windows). They keep running with
  the binary they were spawned from, write to their `<socket>.log`,
  and accept new IPC clients over their local socket / named pipe.
  See [session-daemon.md](../architecture/session-daemon.md).
- **Reattach happens on the way up.** `cleanup_and_reattach`
  ([`agents/mod.rs`](../../server-rs/src/agents/mod.rs)) scans every
  `agents` row whose `status` is `running` or `starting`, checks the
  supervisor socket plus the `<socket>.pid` file, and either rebinds
  the in-memory registry or marks the row `failed` with
  `stop_reason='orphaned'` / `'supervisor_unreachable'`. The browser
  reconnects on its own; orphaned rows can be retried.

## DB migration behavior

The migration model is intentionally low-tech. From
[`server-rs/src/db.rs::migrate`](../../server-rs/src/db.rs):

1. A single `execute_batch` block with `CREATE TABLE IF NOT EXISTS …`
   statements for every table that has ever shipped.
2. Calls into `crate::saas::outbox::init_server_inbox` and
   `init_seq_tracker` to create the runner inbox and the per-peer
   ACK cursor table. Same `IF NOT EXISTS` discipline.
3. A series of `ALTER TABLE … ADD COLUMN … [DEFAULT …]` statements,
   each wrapped in `.ok()` so a duplicate-column error on an
   already-migrated database is silently ignored. Every column added
   after a table was first shipped lives here.
4. `crate::auth::orgs::ensure_default_org` to seed the
   `'default-org'` row and migrate any orphaned `users` / plans into
   it.
5. `cleanup_stale_auto_completed` — a one-time-ish purge of legacy
   bulk auto-inferred `completed` rows from before the navbar
   false-completion fix. Naturally idempotent post-Task-2.2 because
   no new row can satisfy the predicate.

The runner DB has the same shape: `init_runner_outbox` +
`init_seq_tracker` on every boot, both `IF NOT EXISTS`. The runner
keeps no `agents` table — agent state is always demanded from the
server.

What this discipline buys:

- **Every server boot runs the full migration.** No `--migrate` flag,
  no separate command, no manual step. Restart is migration.
- **Adding a column is always backwards-compatible.** Older code
  ignores the new column on `INSERT` (it's not in the column list)
  and on `SELECT` (it's not in the projection); newer rows carry the
  column's `DEFAULT`, older rows from before the migration also
  carry the `DEFAULT` because `ALTER TABLE … ADD COLUMN` populates
  every existing row with it.
- **Adding a table is always backwards-compatible.** Older code never
  references the new table.

What it explicitly does **not** buy:

- **No rollback / down-migration.** There is no symmetric `DROP
  COLUMN` script. Rolling back to a binary that pre-dates a column
  leaves the column on disk; rolling forward to a binary that
  re-introduces it gets caught by `IF NOT EXISTS` and the `.ok()`
  guard, which is fine.
- **No version table.** `db::migrate` cannot tell you which version
  last ran. The `settings` table holds one-shot gates
  (e.g. `ci_backfill_v1_done`) but is not a general schema-version
  ledger.
- **Renaming or removing a column is not a primitive.** Treat it as
  three releases: add the new column + double-write, deploy + backfill,
  drop the old column. Old binaries running concurrently must be able
  to read either shape during the rollout.

A few additive backfills the migration runs on every boot — be
aware they are **persistent state changes** even if they look like
schema changes:

| Backfill | Where | What it does | Idempotent? |
|---|---|---|---|
| `cleanup_stale_auto_completed` | end of `migrate()` | Deletes `task_status` rows for plans whose entire row set is `status='completed' AND source IS NULL` AND has no `agents` rows. | Yes — post-Task-2.2 the predicate has no new candidates, every write writes `source='auto'` or `'manual'`. |
| `ensure_default_org` | end of `migrate()` | Seeds the `'default-org'` row, migrates orphaned `users` and `plans` into it. | Yes. |
| `ci::spawn_backfill` (one-shot at boot, gated on `settings.ci_backfill_v1_done`) | [`server-rs/src/ci.rs`](../../server-rs/src/ci.rs) | Re-polls every `ci_runs` row with a `commit_sha` against the aggregate-aware dispatch path so legacy single-run-path verdicts that stored a passing Docker badge over a failing CI run get flipped. | Yes; the gate row is set unconditionally at the end of the pass. |

## Binary version skew

Branchwork has three independently-upgradable binaries that talk to
each other:

- `branchwork-server` (the dashboard).
- `branchwork-runner` (SaaS only — customer-side process; one runner,
  one outbox).
- `session_daemon` (one process per agent; spawned by either of the
  above; survives parent restart).

Each pair has a different compatibility surface. The contract for the
WS / IPC layers is in
[protocols.md § Versioning policy](../architecture/protocols.md#4-versioning-policy);
the per-axis rules below are the operator's view of the same.

### server ↔ runner (SaaS)

The dashboard's runner row stores the runner's `version` from
`RunnerHello`; `classify_version_mismatch` in
[`saas/runner_ws.rs`](../../server-rs/src/saas/runner_ws.rs) colors
the dashboard chip:

- `ok`: exact match, or one side is unparseable (custom build).
- `patch`: differ on the patch component only — fine in practice.
- `minor`: differ on the minor component — yellow, expect some
  best-effort frames to be silently ignored by the older peer.
- `major`: differ on the major component — red, treat as broken
  until realigned.

Practical recipe:

1. **Upgrade the server first.** New `WireMessage` variants are
   appended; older runners ignore them via the trailing `{}` arm in
   `handle_runner_message`. New SaaS→runner commands silently no-op
   against an older runner (the dashboard either disables the
   corresponding UI or falls back to a pre-existing variant — gated
   on `RunnerHello.version`).
2. **Upgrade runners in a rolling fashion.** Each runner reconnects
   with the new version; the chip flips green; new behaviours light
   up.
3. **Never run the runner ahead of the server in steady state** —
   runner→server frames using new fields are accepted by the older
   server (unknown fields ignored) but the runner cannot tell the
   server has a smaller surface, and any reliable frame using a new
   variant will fail to decode and stick in the runner outbox.

### server ↔ session_daemon

Session daemons are spawned by the server (or runner). The IPC frame
format is `4-byte BE length` + `postcard`-encoded
[`Message`](../../server-rs/src/agents/session_protocol.rs) capped at
`MAX_FRAME_BYTES = 8 MiB`.

Important properties:

- **Daemons are pinned to the binary that spawned them.** A server
  upgrade does not respawn live daemons; the agent in the PTY keeps
  running under the previous binary. The next agent spawned by the
  new server is a fresh daemon at the new version.
- **`postcard` encodes enum discriminants by declaration order.** New
  `Message` variants must be appended; an older peer that receives a
  new variant will fail `decode` with `InvalidData`. The new peer
  must gate use behind a capability check until rollout is complete.
- **The reattach path opens a fresh client to the existing daemon.**
  After server restart, `cleanup_and_reattach` opens
  `<sockets_dir>/<agent-id>.sock` (or the named pipe on Windows) and
  resumes streaming `Output` frames. Mixed-version pairs are normal
  during an upgrade — the discipline above is what keeps them
  working.

### runner ↔ session_daemon (SaaS)

Identical to server ↔ session_daemon: the runner spawns daemons
under `<runner-cwd>/.branchwork-runner-sessions/`, those daemons
survive runner restart, and the next agent the runner spawns picks
up the new binary. `<socket>.log` is the durable PTY transcript on
the customer's filesystem.

### Mixed-version sessions in practice

The matrix that actually matters during a rolling upgrade:

| Pair | What works | What needs care |
|---|---|---|
| New server + old daemons (live agents) | PTY streaming, Ping/Pong, kill on demand. | Any new `Message` variant the new server tries to send to an old daemon will fail `decode` on the daemon side. The default discipline is "the server only adds variants the daemon has been taught to ignore"; if you are introducing a daemon-bound variant, gate use on the daemon's reported version. |
| Old server + new daemons | Should not occur during forward upgrades. Occurs during a **downgrade** while live agents are running. | The new daemon may emit a variant the old server does not know how to decode. PTY output frames are `Output { … }` from day one and stable; the risk is bounded to new variants that should not be on the hot path. |
| New runner + old server | Never run this in steady state. Runner reliable frames using new variants will be rejected and stick in the runner outbox until the server catches up. | If you find yourself here mid-upgrade, finish the server upgrade as the recovery — the outbox is durable. |
| Old runner + new server | Tolerated; this is the normal direction during a rolling SaaS upgrade. | The dashboard chip warns; new SaaS→runner UI is greyed out. |

## Rollback

Rollback is **swap to the old binary, start, accept the
consequences below**. There is no automatic down-migration. The
matching `migrate()` step from the older binary is a no-op against
the newer schema (every statement is `IF NOT EXISTS` or `.ok()`).

The acceptance criterion for this section is being explicit about
what **WILL NOT work** after a downgrade. Read these before clicking.

- **New tables introduced after the rollback target are invisible to
  the old binary.** The data is still on disk. The old binary never
  references the table. Examples that have been added across
  Branchwork's history: `runners`, `runner_tokens`, `inbox_pending`,
  `seq_tracker`, `audit_logs`, `org_kill_switch`, `sso_providers`,
  `sso_accounts`, `sso_auth_state`, `plan_runner_affinity`,
  `task_fix_attempts`, `plan_snapshots`, `settings`. If you used a
  feature backed by one of these between the upgrade and the
  rollback, the **feature is gone** until you roll forward again
  (the rows are not lost — just unreadable).
- **New columns introduced after the rollback target are invisible
  to the old binary.** Same shape: data is on disk, old `INSERT`
  statements don't list the column so writes leave it at the
  `DEFAULT`, old `SELECT` statements don't project it. **`agents.driver`,
  `agents.cost_usd`, `agents.user_id`, `agents.org_id`,
  `agents.stop_reason`, `agents.supervisor_socket`,
  `task_status.source`, `ci_runs.failure_log`, `ci_runs.dismissed_at`,
  `ci_runs.org_id`, `plan_auto_mode.parallel`,
  `plan_auto_advance.parallel`, `plan_project.worktree_isolation_opt_in`,
  `runners.drivers_json`, `runners.removed_at`,
  `runners.outbox_depth` and the rest of the `runners` health
  columns** are all examples. The dashboard surface that surfaces
  these (drivers panel, cost ledger, runner health chip, dismissed
  CI badges) goes blank after the rollback. **Their values are
  preserved** — a future roll-forward sees them again.
- **New `WireMessage` variants are silently dropped.** A SaaS deploy
  that downgrades the server while a newer runner is connected: the
  runner emits frames with variants the older server's
  exhaustive match falls through to its trailing `{}` arm. Reliable
  frames stay durable in the runner outbox until the server is
  back on a compatible version. Best-effort frames (`AgentOutput`,
  `Ping`, …) are dropped. **Plan progress can stall** until the
  versions realign.
- **New session-IPC `Message` variants will fail to decode.**
  `postcard` keys on declaration order; an older peer that meets a
  newer discriminant raises `InvalidData`. In practice the IPC
  layer's hot path is `Input` / `Output` / `Resize` / `Kill` /
  `Ping` / `Pong` (all stable) and new variants are appended at
  the tail. If you introduced a new variant between the upgrade
  and the rollback **and** it is on the hot path, that path is
  broken until the binaries realign — kill and respawn the
  affected agents.
- **New plan YAML fields are silently dropped on UI round-trip.**
  Old binaries `serde_yaml`-deserialise YAML by ignoring unknown
  keys, then re-serialise from `ParsedPlan`. Fields the older
  `ParsedPlan` doesn't know about are **erased on save**. Today's
  list (verified clobber paths): `verification`, per-task
  `produces_commit`, `ci_blocking_workflows`, per-phase
  `phase_verification`. If you need any of these in a rollback,
  edit the YAML on disk and let the file watcher pick it up — do
  **not** re-save through the dashboard's edit form.
- **One-time backfills do not re-run.** The `settings` table holds
  flags like `ci_backfill_v1_done`. Rolling back to a binary that
  doesn't read the flag is harmless. Rolling forward again does
  **not** re-run the backfill — the flag is still `'1'`. If the
  rollback caused you to lose backfilled state and you need it
  recomputed, manually `DELETE FROM settings WHERE
  key='ci_backfill_v1_done'` before the next forward boot.
- **`cleanup_stale_auto_completed` does not undo deletions.** It
  ran during the forward boot, deleted `task_status` rows, and
  left no audit trail. The rollback binary cannot reconstruct
  them. Re-run auto-status from the dashboard if needed; the
  `infer_status` heuristic is now capped at `in_progress` so it
  cannot reintroduce the bug it cleaned up.
- **Old `cleanup_and_reattach` cannot reattach new daemons in
  every case.** A daemon spawned by a binary that wrote
  `agents.supervisor_socket` will be reattachable by an older
  binary only if that binary already understood the column. A
  pre-supervisor binary cannot reattach supervisor-mode daemons at
  all; the old code goes down a tmux-style reattach path that no
  longer exists on disk. Rows show `failed` /
  `stop_reason='orphaned'`; agents themselves keep running on the
  old daemon (you may need `kill <pid>` from `<socket>.pid` to
  stop them).
- **Frontend assets are bundled in the binary.** The browser
  reconnects to the older SPA, which queries older API shapes.
  This is intentional and matches the rest of the rollback story:
  the data on disk that the new SPA depended on is still there
  (auto-finishing pill colour, fix-CI banner, parallel toggle,
  drivers panel), but the older SPA does not render it.

If any of the above hits you mid-rollback, the recovery is the
same: **roll forward to a binary that knows about the column /
table / variant / field**. The disk state is intact.

### Rolling back the runner

The runner has its own DB at `~/.branchwork-runner/runner.db` and
its own version chip on the server's runner panel. The same rules
apply with one caveat: **the `seq_tracker` row that holds the
runner's stable identity is a function of the database file, not of
the binary version.** A runner downgrade preserves the runner_id,
the outbox, and the per-peer ACK cursor; a `DELETE` of `runner.db`
on the runner host (whether intentional or by accident) makes the
runner appear as a fresh entity to the server, and the old row stays
permanently `offline`. If you ever rebuild a runner host, mint a new
token and let the operator clean up the old `runners` row from the
dashboard.

## SQLite → Postgres migration

The Helm chart exposes `database.mode: sqlite | postgres` and wires
`DATABASE_URL` into the pod when `mode: postgres`, **but the Rust
binary only speaks SQLite today**. Nothing in
[`db.rs`](../../server-rs/src/db.rs) reads `DATABASE_URL`; there is
no Postgres driver linked. Setting `mode: postgres` produces a
server that ignores the env var and still opens
`<claude_dir>/branchwork.db` on the (un-mounted) container
filesystem.

Until the Rust code grows a real Postgres backend, the migration
story is:

> **Stay on SQLite.** The Helm `mode: postgres` path is a
> deployment-template stub for a future migration; do not switch
> to it expecting it to work. See
> [architecture/persistence.md § Postgres mode](../architecture/persistence.md#postgres-mode).

When the Postgres backend lands, the documented path will be
`sqlite3 dump → psql restore → repoint the binary at DATABASE_URL`,
with a one-shot tool to translate the SQLite dialect quirks
(`AUTOINCREMENT`, `datetime('now')`, etc.) into the Postgres
equivalents. The schema itself is portable — every `CREATE TABLE`
in `migrate()` uses ANSI-ish types and the only SQLite-specific
syntax is the few `ON CONFLICT` UPSERT clauses, which Postgres
supports identically.

## Backups

The hot-snapshot recipe is in
[self-hosted.md § Backup](self-hosted.md#backup). For deploys that
mount a volume (Docker named volume, Helm PVC, ECS EFS), the
operator's snapshot tooling (Velero, EBS snapshots, `restic`,
whatever your platform provides) wraps the same files: the SQLite
DB, the WAL/SHM siblings, and `~/.claude/plans/`. There is no
backup hook in the binary itself.

For a multi-host SaaS deploy:

- **Server side**, snapshot `~/.claude/branchwork.db` at the
  server's pace.
- **Runner side**, snapshot `~/.branchwork-runner/runner.db` per
  runner — the runner_id and the outbox live there. Losing the
  runner DB means the runner reappears as a new entity to the
  server; the old `runners` row stays `offline`.
- **Project repos** are not Branchwork's concern; back them up
  with your normal repo strategy. Task branches reach the remote
  on Merge.

The "what survives what" matrix in
[architecture/persistence.md § What survives what kind of restart](../architecture/persistence.md#what-survives-what-kind-of-restart)
is the authoritative reference for which artifact lives where.

## See also

- [architecture/persistence.md](../architecture/persistence.md) —
  every table, the migration model, and the four-restart-mode
  matrix this page rests on.
- [architecture/protocols.md § Versioning policy](../architecture/protocols.md#4-versioning-policy)
  — the append-only contract for `WireMessage`, session-IPC
  `Message`, and the `RunnerHello` capability hook.
- [architecture/session-daemon.md](../architecture/session-daemon.md)
  — why a server upgrade does not kill in-flight agents and how
  reattach works on the way back up.
- [architecture/runner.md](../architecture/runner.md) — runner ID
  persistence and outbox replay across runner restarts.
- [operations/self-hosted.md § Upgrade procedure](self-hosted.md#upgrade-procedure)
  — stop / swap / start for systemd, launchd, NSSM.
- [operations/saas-runner.md](saas-runner.md) — runner unit upgrade
  and token rotation.
- [operations/docker.md](docker.md) — image tag bump and the four
  compose overlays.
- [operations/helm-terraform.md § Upgrades](helm-terraform.md#upgrades)
  — `helm upgrade` and `terraform apply` recipes.
