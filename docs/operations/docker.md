# Docker runbook

Run Branchwork inside containers — locally with one command, on a
SaaS-shaped Postgres deploy, or as the e2e fixture CI uses to exercise
the runner round-trip. This page is the day-2 surface for the four
compose files under `deploy/`: what each overlay is for, every
environment variable they reference, what the volumes hold, and the
canonical bring-up / tear-down commands.

The image build pipeline itself (per-arch matrix, `cargo-zigbuild`,
GHCR manifest stitching) lives in
[architecture/deploy.md](../architecture/deploy.md); the Hetzner
production deploy that consumes the prod overlay is documented in
[ops/hetzner.md](../ops/hetzner.md). For non-containerised setups —
plain `branchwork-server` under systemd, or the SaaS runner on a
customer host — see [operations/self-hosted.md](self-hosted.md) and
[operations/saas-runner.md](saas-runner.md).

The Helm chart and Terraform module under `deploy/helm/` and
`deploy/terraform/` are out of scope here; pointers in
[operations/helm-terraform.md](helm-terraform.md).

## Image

Public image, multi-arch, single tag:

```sh
docker pull ghcr.io/branchwork/branchwork:edge
```

Tag matrix:

| Tag | Stitched by | Use |
|---|---|---|
| `:edge` | every green `master` push | Rolling latest. What the base and prod overlays default to via `${BRANCHWORK_VERSION:-edge}`. |
| `:<short-sha>` | every green `master` push | Pin a specific commit. |
| `:master` | every green `master` push | Floating alias for the latest `:edge`. |
| `:latest`, `:<version>`, `:<major>.<minor>` | `v*` tag pushes | Released versions. Pin one of these once releases ship. |

The image is `linux/amd64` + `linux/arm64`. The Docker daemon picks
the matching slice from the manifest index automatically — pin a tag,
never a digest. Build details and the consumer contract:
[architecture/deploy.md](../architecture/deploy.md).

The Dockerfile is [`deploy/Dockerfile`](../../deploy/Dockerfile). It
ships a single image that contains both `branchwork-server` and
`branchwork-runner`; the entrypoint and CMD select the server. Override
`CMD` to run the runner instead — the e2e overlay's `branchwork-runner`
service is the canonical example.

## Overlay matrix

| Overlay | File | Image source | Backend | Runner | Use this when… |
|---|---|---|---|---|---|
| **Base** | [`deploy/docker-compose.yml`](../../deploy/docker-compose.yml) | `ghcr.io/branchwork/branchwork:${BRANCHWORK_VERSION:-edge}` | SQLite (`branchwork-data` volume) | _none_ | …you want a one-command self-hosted dashboard on a single host: `docker compose up -d` and the dashboard is on `http://localhost:3100`. This is the containerised counterpart of [operations/self-hosted.md](self-hosted.md). |
| **SaaS overlay** | [`deploy/docker-compose.saas.yml`](../../deploy/docker-compose.saas.yml) | _(layers on the base)_ | Postgres 17 (`pgdata` volume) + SMTP | _none_ | …you are running the **dashboard** for a managed deploy that needs Postgres (multi-org, billing alerts, SSO at scale) and you want budget-alert email. Customers point their `branchwork-runner` at this dashboard from their own infra; the overlay does not run a runner itself. Note: the Rust binary still talks to SQLite today — the Postgres service is wired in but `db::init` is SQLite-only ([reference/configuration.md § DATABASE_URL](../reference/configuration.md#variables-that-look-like-config-but-arent)). Use it as a placeholder for the backend you'll switch on once Postgres support lands; the SMTP env wiring is fully live. |
| **e2e overlay** | [`deploy/docker-compose.e2e.yml`](../../deploy/docker-compose.e2e.yml) | builds [`deploy/Dockerfile`](../../deploy/Dockerfile) from the working tree | SQLite (ephemeral volume) | optional via `--profile saas` | …you are exercising the SaaS round-trip in CI or locally — the dashboard, a runner, and (with `--profile saas`) the WSS handshake between them, all from a freshly-built image. This is the fixture every e2e test reuses ([`tests/e2e/run.sh`](../../tests/e2e/run.sh)) and the canonical answer to the [ADR 0005](../adrs/0005-e2e-tests-must-be-containerized.md) "tests that spawn a live server must run in containers" rule. |
| **Prod overlay** | [`deploy/docker-compose.prod.yml`](../../deploy/docker-compose.prod.yml) | `ghcr.io/branchwork/branchwork:${BRANCHWORK_VERSION:-edge}` with `pull_policy: always` | SQLite, host-bind `/opt/branchwork/data` | _none_ | …you are running the production deploy at `branchwork.dev` on the Hetzner box. Layers on the base, swaps the data path to `/opt/branchwork/data`, binds 3100 to the docker bridge so Caddy can proxy in, and flips on `BRANCHWORK_SECURE_COOKIES`. Documented end-to-end (Caddy site block, `:edge` rollout, log paths) in [ops/hetzner.md](../ops/hetzner.md); included here only because it lives next to the other compose files. |

Compose overlays are layered with repeated `-f` flags — base first,
overlay second:

```sh
# SaaS shape (base + Postgres + SMTP).
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.saas.yml up -d

# Prod shape (base + Hetzner overrides).
docker compose -f deploy/docker-compose.yml \
               -f deploy/docker-compose.prod.yml up -d
```

The e2e overlay is **standalone** — it does not extend the base file.
It builds from the working tree rather than pulling from GHCR, so it
exercises the Dockerfile change in your branch:

```sh
docker compose -f deploy/docker-compose.e2e.yml up -d            # server only
docker compose -f deploy/docker-compose.e2e.yml --profile saas up -d   # + runner
```

## Environment variables

Every variable referenced by any of the four compose files, grouped
by which overlay introduces it. Reference-grade detail (default,
source line, exact behaviour) is in
[reference/configuration.md](../reference/configuration.md); this
table tells you which compose file passes the variable through and
why.

### Base — `docker-compose.yml`

| Variable | Default | Read by | What it controls |
|---|---|---|---|
| `BRANCHWORK_VERSION` | `edge` | `docker compose` (image tag interpolation, not the binary) | Which GHCR tag to resolve. Pin a release tag (`1.2.3`) for production; leave unset on a laptop and float on `:edge`. |
| `BRANCHWORK_PORT` | `3100` | `docker compose` (host-side port mapping, not the binary) | Host port mapped to the container's `3100`. The binary always listens on `3100` inside the container; this only changes where you reach it from. |
| `BRANCHWORK_WEBHOOK_URL` | unset | `branchwork-server` ([`config.rs:61`](../../server-rs/src/config.rs)) | Optional webhook for `agent_completed` / `phase_advanced` events. Slack incoming webhooks (`{"text": "..."}`) and any JSON-accepting endpoint both work. Empty / whitespace is treated as unset. Detail: [reference/configuration.md § Branchwork-specific variables](../reference/configuration.md#branchwork-specific-variables). |

### SaaS overlay — `docker-compose.saas.yml`

The overlay adds a `postgres` service and threads SMTP credentials into
the dashboard. `DATABASE_URL` is **always** set from the same
`POSTGRES_PASSWORD` value the Postgres service is started with, so the
two sides cannot drift.

| Variable | Default | Read by | What it controls |
|---|---|---|---|
| `POSTGRES_PASSWORD` | `changeme` | `postgres` image (`POSTGRES_PASSWORD` init script) **and** the dashboard's `DATABASE_URL` literal in the same file | Password used to bootstrap the `branchwork` Postgres role and to authenticate the dashboard against it. **Change this** before exposing the deploy — the default is the literal string `changeme`. |
| `DATABASE_URL` | derived: `postgres://branchwork:${POSTGRES_PASSWORD}@postgres:5432/branchwork` | not yet read by the binary — `db::init` is SQLite-only ([`db.rs`](../../server-rs/src/db.rs)). The Helm chart [also sets it](../../deploy/helm/branchwork/templates/deployment.yaml) but is similarly a placeholder. | Reserved for the eventual Postgres backend. Today the dashboard ignores it and falls back to SQLite under `--claude-dir`. Tracked in [reference/configuration.md § Variables that look like config but aren't](../reference/configuration.md#variables-that-look-like-config-but-arent). |
| `SMTP_HOST` | unset (disables email) | `branchwork-server` ([`saas/billing.rs:377`](../../server-rs/src/saas/billing.rs)) | Hostname of the relay (e.g. `smtp.sendgrid.net`). The gating variable: if it's unset, `SmtpConfig::from_env` returns `None` and budget-alert email is skipped entirely. |
| `SMTP_PORT` | `587` | `branchwork-server` ([`saas/billing.rs:380`](../../server-rs/src/saas/billing.rs)) | TCP port. Parsed as `u16`; non-numeric values fall back to the default. |
| `SMTP_FROM` | `branchwork@localhost` | `branchwork-server` ([`saas/billing.rs:384`](../../server-rs/src/saas/billing.rs)) | `From:` address on outgoing alerts. Invalid addresses fall back to the default. |
| `SMTP_USERNAME` | unset | `branchwork-server` ([`saas/billing.rs:385`](../../server-rs/src/saas/billing.rs)) | Optional SMTP AUTH user. Used together with `SMTP_PASSWORD`; setting only one of the two skips authentication. |
| `SMTP_PASSWORD_SMTP` | unset | passed through to the container as `SMTP_PASSWORD`, read by `branchwork-server` ([`saas/billing.rs:386`](../../server-rs/src/saas/billing.rs)) | The host-side variable name is `SMTP_PASSWORD_SMTP` so it cannot collide with anything else's `SMTP_PASSWORD` in the operator's shell environment; the compose file rewrites it to `SMTP_PASSWORD` inside the container, which is what the binary reads. **Set this on the host** (or in your env file), not `SMTP_PASSWORD`. |

Email is opt-in: leave `SMTP_HOST` unset and the dashboard runs with
budget-alert email disabled — every other path is unaffected.

### e2e overlay — `docker-compose.e2e.yml`

This overlay builds from the working tree (`build: { context: .., dockerfile: deploy/Dockerfile }`)
so a change to the Dockerfile or a Rust source file shows up the next
time you bring it up. The variables here are shaped for tests, not
production.

| Variable | Default | Read by | What it controls |
|---|---|---|---|
| `E2E_PORT` | `3199` | `docker compose` (host-side port mapping) | Host port the dashboard binds to. Defaults to `3199` to keep the e2e stack out of the way of a real `:3100` install. |
| `BRANCHWORK_DATA_DIR` | `/data` (set in the compose file and by the Dockerfile's `ENV`) | _no source path reads it_ | Documentation marker only — Branchwork itself does not read this variable. The data path is pinned by the Dockerfile's `CMD ["branchwork-server", "--port", "3100", "--claude-dir", "/data"]` flag. The env var is inherited by spawned children but no `branchwork-*` binary acts on it. Listed for completeness; setting it has no effect on the server. |
| `BRANCHWORK_SAAS_URL` (runner only) | `ws://branchwork:3100` | `branchwork-runner` ([`bin/branchwork_runner.rs:423`](../../server-rs/src/bin/branchwork_runner.rs)) | Dashboard URL the runner dials. The runner auto-rewrites `https://` → `wss://` and `http://` → `ws://`; the compose file ships the cleartext `ws://` form because the e2e network is internal-only. |
| `BRANCHWORK_RUNNER_TOKEN` (runner only) | unset (must be provided) | `branchwork-runner` ([`bin/branchwork_runner.rs:427`](../../server-rs/src/bin/branchwork_runner.rs)) | The runner token to authenticate with. Provisioned out-of-band before bringing up the runner profile — `tests/e2e/run.sh` signs up a user via `/api/auth/signup` and `POST`s to `/api/runners/tokens` to mint one. |
| `HOME` (runner only) | `/home/runneruser` | `dirs::home_dir()` (transitively, e.g. [`bin/branchwork_runner.rs:579`](../../server-rs/src/bin/branchwork_runner.rs)) | The runner uses `dirs::home_dir()` to derive its default `--db-path` (`~/.branchwork-runner/runner.db`). The compose file pins it to `/home/runneruser` so the persisted DB lands inside the named `branchwork-e2e-runner` volume. |

The runner service uses `entrypoint: ["tini", "--"]` and
`command: ["branchwork-runner", "--cwd", "/home/runneruser"]`, which
overrides the Dockerfile's default CMD (which runs the *server*). One
image, two binaries, picked by `command:`.

### Prod overlay — `docker-compose.prod.yml`

For completeness; full operational detail is in
[ops/hetzner.md](../ops/hetzner.md).

| Variable | Default | Read by | What it controls |
|---|---|---|---|
| `BRANCHWORK_VERSION` | `edge` | `docker compose` (image tag) | Same as the base overlay. The prod overlay layers `pull_policy: always` so each `up -d` re-resolves the tag against GHCR. |
| `BRANCHWORK_SECURE_COOKIES` | `1` (set inline) | `branchwork-server` ([`auth/sessions.rs:145`](../../server-rs/src/auth/sessions.rs)) | Set to `1` to flip the session cookie's `Secure` flag on. Required for `https://branchwork.dev`. |
| `BRANCHWORK_PUBLIC_URL` | `https://branchwork.dev` (set inline) | `branchwork-server` ([`saas/install_runner.rs:48`](../../server-rs/src/saas/install_runner.rs), [`saas/runner_ws.rs:1180`](../../server-rs/src/saas/runner_ws.rs)) | Public origin the dashboard advertises in install-runner one-liners and used as the SaaS URL hint when the runner enrolls. The prod-side signal that says "you are the SaaS instance, not a self-hosted one." |
| `BRANCHWORK_DATA_DIR` | `/data` (set inline) | _no source path reads it_ | Documentation marker only (same caveat as the e2e overlay). The actual data path is `--claude-dir /data` in the Dockerfile CMD, with the host bind `/opt/branchwork/data:/data` from the prod overlay placing it on persistent storage. |

## Volumes

Each overlay mounts a different persistent area for the dashboard's
data; nothing is shared across overlays today, by design — local-dev,
SaaS staging, and CI all want clean state.

| Compose file | Volume / bind | Mount inside container | Holds |
|---|---|---|---|
| `docker-compose.yml` | named volume `branchwork-data` | `/data` | `branchwork.db` (+ WAL/SHM), `plans/`, `sessions/`. The full `<claude-dir>` layout from [reference/configuration.md § Filesystem layout](../reference/configuration.md#filesystem-layout). |
| `docker-compose.saas.yml` | named volume `pgdata` | `/var/lib/postgresql/data` | Postgres data dir for the bundled Postgres 17 service. Branchwork's binary doesn't read this yet (see `DATABASE_URL` above) — kept separate from `branchwork-data` so a future Postgres-backed dashboard can be cut over without touching the SQLite path. |
| `docker-compose.e2e.yml` | named volume `branchwork-e2e-data` | `/data` (server) | Per-run dashboard state. Test cleanup uses `down -v` to drop it. |
| `docker-compose.e2e.yml` (saas profile) | named volume `branchwork-e2e-runner` | `/home/runneruser` | Runner's home dir — `~/.branchwork-runner/runner.db` (outbox + persisted runner ID) plus `<cwd>/.branchwork-runner-sessions/` for any agents the runner spawns. |
| `docker-compose.prod.yml` | host bind `/opt/branchwork/data` | `/data` | Same shape as `branchwork-data`, but on the Hetzner host's filesystem so backups are visible to host-level tooling. |

The prod overlay declares `volumes: !override [- /opt/branchwork/data:/data]`,
which **replaces** the base file's `branchwork-data` named volume
rather than appending to it. Without `!override` the prod stack would
mount both, which is not what you want. Same for `ports:` — see the
overlay file for the full reasoning.

## Run recipes

The minimum-viable command for each shape:

```sh
# Self-hosted, base overlay, default port.
docker compose up -d
# Dashboard at http://localhost:3100; data in volume `branchwork-data`.

# Self-hosted, base overlay, custom port + Slack webhook.
BRANCHWORK_PORT=3200 \
  BRANCHWORK_WEBHOOK_URL=https://hooks.slack.com/services/T/B/X \
  docker compose up -d

# SaaS shape: base + Postgres + SMTP, with a real password and SMTP relay.
POSTGRES_PASSWORD=$(openssl rand -hex 16) \
  SMTP_HOST=smtp.sendgrid.net \
  SMTP_FROM=alerts@example.com \
  SMTP_USERNAME=apikey \
  SMTP_PASSWORD_SMTP=SG.xxxxxxxxxxxxxxxx \
  docker compose -f deploy/docker-compose.yml \
                 -f deploy/docker-compose.saas.yml up -d

# E2E, server only, ephemeral.
docker compose -f deploy/docker-compose.e2e.yml up -d branchwork

# E2E, with runner. Token must be provisioned first.
BRANCHWORK_RUNNER_TOKEN=bwr_live_xxxxxxxx \
  docker compose -f deploy/docker-compose.e2e.yml --profile saas up -d
```

Tear-down:

```sh
# Stop containers, keep volumes (data survives).
docker compose -f deploy/docker-compose.yml down

# Stop containers and drop volumes (full reset).
docker compose -f deploy/docker-compose.yml down -v
```

For e2e runs, `down -v` against the per-run compose project is the
**only** safe cleanup — see ADR 0005 below.

## Healthcheck

The dashboard service in the base overlay declares:

```yaml
healthcheck:
  test: ["CMD", "wget", "-q", "--spider", "http://127.0.0.1:3100/health"]
  interval: 15s
  timeout: 5s
  retries: 3
  start_period: 5s
```

The image bundles `wget` (alpine base; see
[`deploy/Dockerfile` stage 3](../../deploy/Dockerfile)). The endpoint
is the dashboard's `/health` (200 OK with no body). The e2e overlay
shortens `interval` to `5s` and bumps `start_period` to `10s` because
its tests spin the container up and immediately probe; the Postgres
service in the SaaS overlay has its own `pg_isready` healthcheck so
the dashboard can `depends_on: { postgres: { condition: service_healthy } }`
and not start until Postgres accepts connections.

`docker compose ps` shows `(healthy)` on the dashboard once the probe
passes; `tests/e2e/run.sh` waits on this signal before driving any
HTTP traffic at the container.

## Build vs pull

```sh
# Build the image from the working tree (e2e overlay does this automatically).
docker buildx build -f deploy/Dockerfile -t branchwork:dev .

# Pull a published image instead.
docker pull ghcr.io/branchwork/branchwork:edge
```

The Dockerfile is a three-stage build:

1. **Stage 1 (`web`)** — `node:20-alpine`, builds the React frontend
   once on `$BUILDPLATFORM` (always amd64 in CI).
2. **Stage 2 (`server`)** — `rust:1.88-alpine`, cross-compiles
   `branchwork-server` and `branchwork-runner` for both architectures
   in a single `cargo zigbuild` invocation. This is the load-bearing
   stage for build performance — full reasoning in
   [architecture/deploy.md § Image build pipeline](../architecture/deploy.md#image-build-pipeline)
   and the [build-perf baseline](../build-perf-2026-05-05-baseline.md).
3. **Stage 3 (`runtime`)** — `alpine:3.21`, slices in the per-arch
   binaries from stage 2, adds `git`, `ca-certificates`, and `tini`
   for signal handling, and runs as a non-root `branchwork` user.

The CI publish workflow (`.github/workflows/docker.yml`) builds each
arch in parallel on its own runner and stitches them into a single
manifest index; consumers always pull a tag and let Docker resolve
the slice. There is no QEMU step in CI.

## E2E test fixture rule

ADR 0005 — [`docs/adrs/0005-e2e-tests-must-be-containerized.md`](../adrs/0005-e2e-tests-must-be-containerized.md)
— pins the rule that **any** end-to-end or smoke test that spawns
`branchwork-server` or `branchwork-runner` must run those binaries
inside containers, brought up by Docker Compose with a per-run unique
project name and torn down via `docker compose -p <project> down -v`.
The agent-side reason is that Branchwork agents typically run on the
same host as a production `branchwork-server`; an unscoped
`pgrep -f "branchwork-server"` or `killall branchwork-server` in a
test fixture has, in real history, killed the agent's own supervisor
mid-task.

`deploy/docker-compose.e2e.yml` is the canonical fixture. The `saas`
profile is the only place to look for a complete server+runner setup
in containers — copy from there before writing a new e2e harness.

## See also

- [architecture/deploy.md](../architecture/deploy.md) — the GHCR
  publish pipeline, the per-arch matrix, and the consumer tag
  contract.
- [ops/hetzner.md](../ops/hetzner.md) — production deploy at
  `branchwork.dev`: Caddy site block, prod overlay, `:edge` rollout
  cadence.
- [reference/configuration.md](../reference/configuration.md) —
  every environment variable Branchwork actually reads, with source
  citations.
- [operations/self-hosted.md](self-hosted.md) — the non-containerised
  single-host shape the base overlay packages up.
- [operations/saas-runner.md](saas-runner.md) — running the runner
  binary on a customer host (out of band from any compose file).
- [operations/helm-terraform.md](helm-terraform.md) — Helm chart and
  Terraform module for cloud-native deploys.
- [adrs/0005-e2e-tests-must-be-containerized.md](../adrs/0005-e2e-tests-must-be-containerized.md)
  — why the e2e overlay exists.
- [`tests/e2e/run.sh`](../../tests/e2e/run.sh) — the canonical
  consumer of the e2e overlay; treat as the worked example.
