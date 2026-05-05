# Build & deploy

How the public `ghcr.io/branchwork/branchwork` image is produced and
how consumers (the Hetzner production deploy, e2e test fixtures,
arbitrary `docker pull` users) reach the right per-arch slice from a
single tag.

## Image build pipeline

`.github/workflows/docker.yml` is the only thing that publishes to
GHCR. It fires on three triggers:

- Every `master` push, gated on a green CI run via `workflow_run`.
  The metadata action attaches a rolling `:edge`, plus
  `:<short-sha>` and `:master`.
- Every `v*` tag push, attaching `:<version>`, `:<major>.<minor>`,
  and `:latest`.
- Manual `workflow_dispatch`.

The workflow has two jobs:

1. **`build` (matrix: `amd64`, `arm64`).** Each entry runs on
   `ubuntu-latest` (amd64 hardware) and invokes `docker buildx build
   --platform linux/${{ matrix.arch.docker }}` against
   `deploy/Dockerfile`, pushing a per-arch image **by digest only**
   (`outputs: type=image,push-by-digest=true,name-canonical=true`).
   No human-readable tag is attached at this stage. The two matrix
   entries run in parallel on independent runners; per-arch GHA
   cache scope is keyed by `${{ matrix.arch.suffix }}`, so an amd64
   cache miss does not invalidate arm64.
2. **`manifest` (after both `build` jobs succeed).** Downloads the
   two digest artifacts and runs `docker buildx imagetools create
   --tag …` to stitch them into a single multi-arch image index,
   attaching every human-readable tag from
   `docker/metadata-action@v5`. This is the contract surface
   consumers see: each public tag (`:edge`, `:latest`, `:1.2.3`,
   `:<short-sha>`) resolves to an index that lists both
   `linux/amd64` and `linux/arm64` digests.

Stage 2 of `deploy/Dockerfile` cross-compiles both architectures
natively from a single amd64 build host via `cargo-zigbuild` against
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. There
is **no QEMU step**: the workflow does not pull
`tonistiigi/binfmt`, and stage 2 is pinned to `--platform=$BUILDPLATFORM`
(amd64) so the Rust toolchain runs at native speed for both targets.

See [`build-perf-2026-05-05-baseline.md`](../build-perf-2026-05-05-baseline.md)
for the measurements that motivated this shape — the cold-cache wall
clock dropped to ~25% of the original QEMU-based baseline.

## Consumer contract

Consumers pin a tag, never a digest. Docker's pull machinery
resolves the tag to the multi-arch index produced by the `manifest`
job and selects the matching slice using the host's `runtime.GOARCH`
(`linux/amd64` or `linux/arm64`). This is the same pattern Docker
has supported since manifest lists shipped — splitting the workflow
into per-arch build jobs plus a stitching `manifest` job does not
change what the consumer sees.

Concrete consumers:

- **Hetzner production deploy.** Phase 6 of the
  `saas-folder-listing-via-runner` plan (in `~/.claude/plans/`)
  brings the production overlay
  `deploy/docker-compose.prod.yml` (created in task 6.7) which
  pins
  `image: ghcr.io/branchwork/branchwork:${BRANCHWORK_VERSION:-edge}`
  with `pull_policy: always`. No digest, no per-arch logic — the
  Hetzner host's amd64 daemon resolves `:edge` to the amd64 slice
  on every `docker compose up -d`.
- **Local Docker Desktop on Apple Silicon.** Same tag, the daemon
  reports `linux/arm64` and pulls the arm64 slice transparently.
- **Helm chart and Terraform module** under `deploy/helm/` and
  `deploy/terraform/`. Both reference an image tag that flows
  through the same manifest index.

The consumer-side change set for the docker-build-perf plan is
**empty**: no compose file, no Helm value, no Terraform variable
needed an edit when the workflow shape changed. The only externally
visible difference is in CI duration (faster builds) and in GHCR's
"OS / Arch" listing (still both `linux/amd64` and `linux/arm64`).

## Pull-and-run smoke fixture

The acceptance criterion for this contract is "pull the published
image, run `branchwork-server --help`, expect exit 0 on each arch."
Two equivalent fixtures cover it:

- **Local build + run via the e2e suite.** `tests/e2e/run.sh` brings
  up `deploy/docker-compose.e2e.yml`, which builds
  `deploy/Dockerfile` from the working tree and runs the resulting
  `branchwork` container against a real HTTP/WS surface. Because
  the e2e suite uses the same Dockerfile that CI ships from, a
  green e2e run is direct evidence that the image is functional on
  the host's arch.
- **Anonymous `docker pull` against a published `:edge`.** Once the
  GHCR package is flipped to public (saas plan task 6.2), any host
  can run
  `docker run --rm ghcr.io/branchwork/branchwork:edge \
   /usr/local/bin/branchwork-server --help`. This is the closest
  analogue to the Hetzner consumer's pull-and-run path.

The first fixture is the canonical one for CI; the second is the
external-side check used during a deploy verification (e.g. the
runbook in saas plan task 6.10).

## Production reverse proxy (Caddy)

The Hetzner box hosts several Cloudflare-fronted sites
(`varpulis-cep.com`, `openraroc.com`, `reglyze.com`,
`branchwork.dev`). They share a single Caddy instance — the
`demo-caddy` container in the `demo` Docker Compose project at
`/home/cpo/varpulis-demo/repo/deploy/demo/`. Each new site is added
as a block in that project's `Caddyfile`; restarting the `demo-caddy`
container re-establishes the bind mount so caddy picks up the new
content.

DNS for `branchwork.dev` is at Cloudflare with proxy enabled
(orange cloud), so HTTP-01 to Let's Encrypt cannot work — the edge
terminates TLS. Two viable patterns for origin TLS:

- **Cloudflare Origin Certificate** (path used by `varpulis-cep` and
  `openraroc`): minted via the Cloudflare dashboard, installed at
  `/etc/caddy/certs/<name>-origin{,-key}.pem`, served via
  `tls /etc/caddy/certs/<name>-origin.pem
  /etc/caddy/certs/<name>-origin-key.pem`. Requires the Cloudflare
  zone in **Full (Strict)** mode.
- **`tls internal`** (path used by `reglyze` and `branchwork.dev`):
  Caddy serves a self-signed cert from its internal CA. The
  Cloudflare zone TLS mode must be **Full** (not Strict) for the
  edge to accept it. No cert minting or sudo write to
  `/etc/caddy/certs/` needed.

The current `branchwork.dev` site block (matches the reglyze
pattern):

```caddyfile
branchwork.dev, www.branchwork.dev {
    tls internal

    @www host www.branchwork.dev
    redir @www https://branchwork.dev{uri} permanent

    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains; preload"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "strict-origin-when-cross-origin"
    }

    reverse_proxy 172.17.0.1:3100
}
```

`172.17.0.1` is the Docker bridge gateway — the address `demo-caddy`
uses to reach the host's loopback, where the `branchwork-server`
compose stack (introduced in task 6.7) binds port 3100.

Operational gotcha — the `Caddyfile` is mounted into `demo-caddy`
as a single-file bind. Editing it via atomic-rewrite (the default
for most editors) strands the bind on the original inode; the
container keeps reading the old content until you restart it.
Either edit in-place (`cat > Caddyfile`) **and** then run
`docker restart demo-caddy`, or just restart unconditionally after
any edit.

## First-run smoke test

After bringing up the prod overlay (task 6.7) and the Caddy site
block (task 6.6), task 6.8 runs a four-step smoke against the
public URL to confirm the auth + cookie + dashboard surface is
live before pointing a real runner at `wss://branchwork.dev`.
Every assertion is HTTP-only — no browser needed:

```sh
EMAIL="smoke-$(date +%s)@example.com"
PASSWORD="smoketest-pw-1234"

# 1. Signup. Asserts 201 and Set-Cookie carries Secure + HttpOnly.
curl -sS -i -c /tmp/bw-cookies.txt -X POST https://branchwork.dev/api/auth/signup \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}"

# 2. /api/auth/me with the cookie. Asserts 200 + the user row.
curl -sS -i -b /tmp/bw-cookies.txt https://branchwork.dev/api/auth/me

# 3. Dashboard renders. Asserts 200 text/html and the SPA bundle
#    referenced in the index also returns 200.
curl -sS -o /tmp/bw-index.html -w "%{http_code}\n" https://branchwork.dev/

# 4. Issue a runner token. Asserts 201 + JSON containing `token`.
curl -sS -i -b /tmp/bw-cookies.txt -X POST https://branchwork.dev/api/runners/tokens \
  -H 'Content-Type: application/json' \
  -d '{"runner_name":"smoke-test"}'
```

Expected `Set-Cookie` shape from step 1, with the prod overlay's
`BRANCHWORK_SECURE_COOKIES=1` (introduced in task 6.4) applied:

```
branchwork_session=<hex>; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800; Secure
```

Save the token from step 4 — task 6.9 feeds it to a real runner
(`branchwork-runner --saas-url wss://branchwork.dev --token …`)
to prove the dispatch path works end-to-end. The smoke run on
2026-05-05 stashed it at
`~/.config/branchwork/saas-runner-token-smoke.txt` (mode 0600,
out of git); rotate or revoke once the runner smoke is signed
off.

The first signed-up user lands in `default-org` (membership ORDER
BY name puts "Default Organization" before the personal
`<localpart>'s org`). Promotion to `owner` of the personal org is
not needed for the smoke since `/api/runners/tokens` only requires
authentication — the token is bound to whichever org the
middleware resolves, and `default-org` is fine for a first-run
exercise.

## See also

- [`build-perf-2026-05-05-baseline.md`](../build-perf-2026-05-05-baseline.md)
  — the measurement record that motivated the zigbuild +
  per-arch parallel split.
- `saas-folder-listing-via-runner` plan, Phase 6
  (`~/.claude/plans/saas-folder-listing-via-runner.yaml`) —
  Hetzner production deploy plan.
- [`deploy/Dockerfile`](../../deploy/Dockerfile) — the actual
  image definition.
- [`.github/workflows/docker.yml`](../../.github/workflows/docker.yml)
  — the publish workflow.
