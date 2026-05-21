# Hetzner production runbook

Operator runbook for the `branchwork.dev` production deploy on
Hetzner. Covers the day-2 surface: how to update the running server,
how to back the SQLite DB up, how the host is monitored, where the
logs are, what to hand a customer who wants to attach a runner, and
the known gaps you should not be surprised by.

The build and reverse-proxy pipeline that produces `:edge` and gets
it past Cloudflare is documented separately in
[`../architecture/deploy.md`](../architecture/deploy.md); this file
assumes that pipeline is already in place.

## Layout on the host

```
/opt/branchwork/
├── src/                     # git checkout of branchwork (compose files only)
├── data/                    # bind-mounted to /data inside the container
│   └── branchwork.db        # SQLite, agents + plans + audit log + outbox
└── .env                     # BRANCHWORK_VERSION pin, secrets (mode 0600)
```

The base compose file (`deploy/docker-compose.yml`) and the prod
overlay (`deploy/docker-compose.prod.yml`, task 6.7) live under
`/opt/branchwork/src/`. Everything Branchwork persists is under
`/opt/branchwork/data/` — the named volume is bound to that path
explicitly in the prod overlay so backups and snapshots can use a
plain host filesystem path.

Caddy fronts the container on `branchwork.dev` and reverse-proxies to
`172.17.0.1:3100` (the Docker bridge gateway). The Caddy site block
lives outside this repo, in the `demo-caddy` project — see
`../architecture/deploy.md` for the block content and the
edit-then-restart gotcha.

## CI/CD shape

`.github/workflows/docker.yml` is the only thing that publishes to
GHCR. It fires on three triggers:

- **Push to `master`**, gated on a green CI run via `workflow_run`.
  Publishes a rolling `:edge`, plus `:<short-sha>` and `:master`.
- **Push of a `v*` tag** (e.g. `v1.2.3`). Publishes `:1.2.3`,
  `:1.2`, and `:latest`.
- **Manual `workflow_dispatch`**.

The Hetzner prod compose tracks `:edge` by default
(`image: ghcr.io/branchwork/branchwork:${BRANCHWORK_VERSION:-edge}`).
To pin a specific release, add `BRANCHWORK_VERSION=1.2.3` to
`/opt/branchwork/.env` and re-run the update procedure below. Use a
`:<short-sha>` value (also published from `master`) when you need to
roll forward to a specific master commit without cutting a tag.

## Update procedure

```
cd /opt/branchwork/src && git pull   # only if compose files changed
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml pull
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml up -d
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml logs --tail=100 -f branchwork
```

`pull_policy: always` is set in the prod overlay, so the second
command resolves `:edge` to the most recent multi-arch index pushed
by CI; on amd64 hardware Docker selects the amd64 slice. The third
command rolls the container with the new image; the volume mount on
`/opt/branchwork/data` survives the recreate.

Schema migrations run on startup — `db::init` uses
`CREATE TABLE IF NOT EXISTS` and idempotent `ALTER TABLE … ADD
COLUMN` patterns throughout, so a forward roll is safe to re-apply.

**Rollback** is the same three commands with `BRANCHWORK_VERSION`
set to the previous tag or short-sha:

```
echo 'BRANCHWORK_VERSION=<previous-tag-or-sha>' >> /opt/branchwork/.env
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml pull
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml up -d
```

A container roll cannot reverse a destructive migration (e.g. a
column drop or a backfill that overwrites old values). Restore the
SQLite snapshot from the previous night's backup *before* downgrading
when that's the case. There are no destructive migrations in the
tree as of this writing; flag this section in the release notes if
you ever introduce one.

## Backups

SQLite needs a consistent snapshot. A raw `cp` of `branchwork.db`
during a write can corrupt the copy — use `sqlite3 .backup` instead,
which acquires the right locks and produces a transactionally
consistent file even on a busy DB.

Cron entry on the host (`crontab -e` as the user that runs the
compose stack — usually `root` on Hetzner since `/opt/branchwork` is
root-owned):

```
0 3 * * * docker compose -f /opt/branchwork/src/deploy/docker-compose.yml \
  exec -T branchwork sqlite3 /data/branchwork.db ".backup /data/backup-$(date +\%F).db" \
  && find /opt/branchwork/data -name 'backup-*.db' -mtime +14 -delete
```

The `\%F` escape is required inside `crontab` — `%` is otherwise
interpreted as a newline in command bodies. The retention window is
14 days; tune by editing the `-mtime +14`.

**Off-box step.** The cron above only writes to the same disk as the
live DB, which doesn't survive a host loss. Append an `rsync` to a
second host or a push to S3-compatible storage. The off-box
destination is operator-supplied — a typical line is:

```
… && rsync -a /opt/branchwork/data/backup-$(date +\%F).db \
  backup-host:/srv/branchwork-backups/
```

To **restore** from a snapshot:

```
docker compose -f /opt/branchwork/src/deploy/docker-compose.yml \
               -f /opt/branchwork/src/deploy/docker-compose.prod.yml down
cp /opt/branchwork/data/backup-2026-04-12.db /opt/branchwork/data/branchwork.db
docker compose -f /opt/branchwork/src/deploy/docker-compose.yml \
               -f /opt/branchwork/src/deploy/docker-compose.prod.yml up -d
```

## Health monitoring

The container serves a liveness endpoint at `/health` (handler in
`server-rs/src/main.rs`, route registered for both `/health` and
`/api/health`). It returns `200 OK` with a tiny JSON body whenever
the HTTP listener is up.

Configure an external uptime check at one-minute granularity:

- **URL.** `https://branchwork.dev/health`
- **Cadence.** Every 60 s
- **Pass condition.** HTTP 200
- **Recommended providers.** UptimeRobot (free tier), healthchecks.io,
  or any cron host that can fire a curl. The check probes the public
  URL through Cloudflare + Caddy + the container, so it doubles as a
  TLS / reverse-proxy / container-up signal in one.

The compose file's own `healthcheck` covers the in-container loopback
(`wget http://127.0.0.1:3100/health` every 15 s). It will mark the
container `unhealthy` and let `restart: unless-stopped` cycle it on a
hung process, but it does not see TLS or Cloudflare problems —
that's why the external check is also required.

## Logs

```
docker compose -f /opt/branchwork/src/deploy/docker-compose.yml \
               -f /opt/branchwork/src/deploy/docker-compose.prod.yml \
               logs branchwork --since 1h
```

Drop `--since 1h` for the full backlog. Add `-f` to follow.

Caddy access logs for `branchwork.dev` land at
`/var/log/caddy/branchwork.dev.log`. They're rotated by Caddy's
default policy (size-based). Search for a session cookie or the
upstream `172.17.0.1:3100` to filter Branchwork traffic out of the
shared Caddy instance.

There is no separate application log file — Branchwork writes to
stdout/stderr, which Docker captures and `docker compose logs`
exposes. The journald driver is the default on Hetzner's stock
docker package, so `journalctl -u docker` also surfaces the same
lines if the compose CLI is unavailable.

## Runner setup for end users

The branchwork.dev deploy serves the install one-liner at
`https://branchwork.dev/install-runner.sh`. End users go through the
dashboard's **Add runner** modal (`/runners` page), which mints a
single-use token and renders a copy-paste install command of the
form:

```
curl -fsSL https://branchwork.dev/install-runner.sh \
  | sh -s -- bwr_live_xxxxxxxxxxxxxxxxxxxx
```

The script drops both `branchwork-runner` and `branchwork-server`
at `~/.local/bin/`, renders a user-mode systemd unit, and runs
`systemctl --user enable --now branchwork-runner` so the runner
survives reboots. Full lifecycle (the unit, journal logs, in-place
upgrades, `loginctl enable-linger`, and the explicit decision to
keep project clones in `$HOME`) is documented at
[`../architecture/runner-lifecycle.md`](../architecture/runner-lifecycle.md).
Token issuance, the `POST /api/runners/install-command` endpoint,
the system-mode unit variant, and platform-specific recipes for
macOS / Windows live at
[`../operations/saas-runner.md`](../operations/saas-runner.md).

The runner is outbound-only — no port to open, no inbound TLS to
manage on the customer side. It reattaches automatically after
network blips and after restart (the `seq_tracker` row in
`runner.db` keeps the same runner-id between sessions, so the
dashboard sees a reattach, not a new runner).

## Known gaps

- **Postgres in `docker-compose.saas.yml` is not wired.**
  `server-rs/src/db.rs` only links `rusqlite` (`features =
  ["bundled"]`), so the `DATABASE_URL` env var that
  `docker-compose.saas.yml` would set is read by nothing today.
  Revisit if a Postgres backend is added — the compose file is
  scaffolding, not a working configuration.
- **No rate limiting or abuse controls in the application.** Caddy
  can absorb crude bursts via its `rate_limit` plugin if needed, but
  the app itself trusts authenticated requests. Don't expose
  `/api/auth/signup` to the open internet without a captcha shim or
  IP-based limiter if you start seeing abuse.
- **Cookie `Secure` flag is conditional.** Production sets
  `BRANCHWORK_SECURE_COOKIES=1` in the prod overlay (introduced in
  task 6.4); without that env var, session cookies omit `Secure`,
  which would let a downgrade attack strip them. Verify with a
  signup probe (the T6.8 smoke test in
  `../architecture/deploy.md#first-run-smoke-test` is the canonical
  command) after any compose change.
- **Single-host SQLite only.** The deploy is intentionally one
  host. There is no read replica, no fail-over, and the backup
  cron is the only off-box durability. A multi-host SaaS topology
  is in `~/.claude/plans/backlog/` as a future plan, not in this
  runbook.

## See also

- [`../architecture/deploy.md`](../architecture/deploy.md) — image
  build pipeline, multi-arch publish, Caddy fronting, first-run
  and real-runner smoke tests.
- [`../reference/cli.md`](../reference/cli.md) — flag reference for
  `branchwork-server` and `branchwork-runner`.
- [`../reference/configuration.md`](../reference/configuration.md) —
  environment variables (`BRANCHWORK_SECURE_COOKIES`,
  `BRANCHWORK_PUBLIC_URL`, SMTP, etc.).
- `deploy/docker-compose.prod.yml` — the prod overlay this runbook
  drives.
