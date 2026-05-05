#!/usr/bin/env bash
# Three-binary smoke test for the SaaS folder-listing UX (plan task 5.7).
#
# Stands up server + runner + bundled web frontend in a uniquely-named
# Docker Compose project, runs a Playwright spec at
# web/e2e/runner-banners.spec.ts that drives the four scenarios from the
# task brief, and tears the project down via `docker compose down -v`.
#
# Hard constraint (CLAUDE.md, ADR 0005): every container action goes
# through `docker compose -p $PROJECT …`. No host-process scanning,
# no kill-by-pattern. The 2026-05-05 incident — an unscoped host-process
# match killed the agent's own supervisor — is the canonical reason
# this rule exists; see docs/adrs/0005-e2e-tests-must-be-containerized.md
# for the full write-up.
#
# Usage:
#   tests/smoke/run.sh                 # build + run + teardown
#   SMOKE_KEEP=1 tests/smoke/run.sh    # skip teardown (debug)
#
# Outputs:
#   tests/smoke-artifacts/<run-date>/01-folder-suggestions.png … 04-runner-unavailable.png
#   tests/smoke-artifacts/<run-date>/notes.md          (UX-rough-edge follow-up notes)
#   tests/smoke-artifacts/<run-date>/html-report/      (Playwright HTML report)
#   tests/smoke-artifacts/<run-date>/test-results/     (per-test traces/videos on failure)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/deploy/docker-compose.e2e.yml"

if [ ! -f "$COMPOSE_FILE" ]; then
  echo "[smoke] FATAL: $COMPOSE_FILE not found"
  exit 1
fi

# ── Helpers ──────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "${BOLD}[smoke]${RESET} $*"; }
ok()    { echo -e "${GREEN}[ok]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[warn]${RESET} $*"; }
fail()  { echo -e "${RED}[fail]${RESET} $*"; }

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    fail "missing required tool: $1"
    exit 1
  fi
}

require docker
require jq
require curl
require uuidgen
require shuf
require pnpm

# ── Pick unique project name + port ──────────────────────────────────

# uuidgen output is 36 chars with hyphens; first 8 hex chars are unique
# enough across in-flight smoke runs on a single host. Project name must
# match docker-compose's allowed regex ([a-z0-9-]+).
SMOKE_TAG="$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
PROJECT="bw-smoke-$SMOKE_TAG"
E2E_PORT="$(shuf -i 30000-39999 -n 1)"
RUN_DATE="$(date -u +%Y-%m-%dT%H-%M-%SZ)"
ARTIFACTS_DIR="$REPO_ROOT/tests/smoke-artifacts/$RUN_DATE"

mkdir -p "$ARTIFACTS_DIR"

# Persist project + port + run-date to a temp file the trap can re-source
# in case the script's environment was clobbered (it shouldn't be, but
# this is the canonical way to keep teardown idempotent across reentry).
SMOKE_ENV_FILE="$REPO_ROOT/.smoke.env"
{
  echo "PROJECT=$PROJECT"
  echo "E2E_PORT=$E2E_PORT"
  echo "RUN_DATE=$RUN_DATE"
  echo "ARTIFACTS_DIR=$ARTIFACTS_DIR"
} > "$SMOKE_ENV_FILE"

info "project=$PROJECT port=$E2E_PORT run-date=$RUN_DATE"
info "artifacts dir: $ARTIFACTS_DIR"

# ── Teardown (mandatory; trap fires on any exit path) ───────────────

teardown() {
  local rc=$?
  if [ "${SMOKE_KEEP:-0}" = "1" ]; then
    warn "SMOKE_KEEP=1 — leaving stack up"
    warn "  base url:    http://localhost:$E2E_PORT"
    warn "  project:     $PROJECT"
    warn "  cleanup:     docker compose -p $PROJECT --profile saas down -v"
    warn "  remove env:  rm -f $SMOKE_ENV_FILE"
    return "$rc"
  fi

  info "tearing down compose project $PROJECT (rc=$rc)…"
  # ALWAYS pass --profile saas so the runner is removed even if it never
  # came up. -v drops the named volumes so the next run starts fresh.
  # 2>/dev/null because the down command emits noise on a partially-up
  # stack — we already log success/failure via rc.
  docker compose -p "$PROJECT" -f "$COMPOSE_FILE" --profile saas down -v 2>/dev/null || true
  rm -f "$SMOKE_ENV_FILE"

  # Sanity check: nothing should be left behind for this project label.
  local stragglers
  stragglers=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT" 2>/dev/null || true)
  if [ -n "$stragglers" ]; then
    fail "stragglers remain for project=$PROJECT:"
    echo "$stragglers"
    return 1
  fi
  ok "no stragglers for project=$PROJECT"
  return "$rc"
}
trap teardown EXIT

# ── Pre-cleanup: drop any matching project from a previous failed run ─

docker compose -p "$PROJECT" -f "$COMPOSE_FILE" --profile saas down -v 2>/dev/null || true

# ── Build images (layer cache hits if Cargo.lock + sources unchanged) ─

info "building docker images (layer cache should hit)…"
E2E_PORT="$E2E_PORT" docker compose -p "$PROJECT" -f "$COMPOSE_FILE" build

# ── Bring up the server first so /health is reachable for token mint ─

info "starting branchwork (port $E2E_PORT)…"
E2E_PORT="$E2E_PORT" docker compose -p "$PROJECT" -f "$COMPOSE_FILE" up -d branchwork

BASE_URL="http://localhost:$E2E_PORT"
HEALTH_TIMEOUT=30
elapsed=0
info "waiting for /health…"
while [ "$elapsed" -lt "$HEALTH_TIMEOUT" ]; do
  if curl -sf "$BASE_URL/health" >/dev/null 2>&1; then
    ok "server healthy after ${elapsed}s"
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

if [ "$elapsed" -ge "$HEALTH_TIMEOUT" ]; then
  fail "server health check timed out after ${HEALTH_TIMEOUT}s"
  docker compose -p "$PROJECT" -f "$COMPOSE_FILE" logs --tail=50 branchwork || true
  exit 1
fi

# ── Bootstrap user + runner token (lifted from tests/e2e/run.sh) ─────

COOKIE_JAR="$(mktemp)"
USER_EMAIL="smoke-$RUN_DATE-$$@example.test"
USER_PASSWORD="smoke-password-1234"

info "signing up smoke user $USER_EMAIL…"
signup_status=$(curl -sS -o /tmp/smoke-signup.json -w "%{http_code}" \
  -c "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -X POST "$BASE_URL/api/auth/signup" \
  -d "{\"email\":\"$USER_EMAIL\",\"password\":\"$USER_PASSWORD\"}")
if [ "$signup_status" != "201" ]; then
  fail "signup returned $signup_status (expected 201)"
  cat /tmp/smoke-signup.json
  exit 1
fi

info "minting runner token via /api/runners/tokens…"
token_status=$(curl -sS -o /tmp/smoke-token.json -w "%{http_code}" \
  -b "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -X POST "$BASE_URL/api/runners/tokens" \
  -d '{"runner_name":"smoke-runner"}')
if [ "$token_status" != "201" ]; then
  fail "token mint returned $token_status (expected 201)"
  cat /tmp/smoke-token.json
  exit 1
fi

RUNNER_TOKEN=$(jq -r .token < /tmp/smoke-token.json)
if [ -z "$RUNNER_TOKEN" ] || [ "$RUNNER_TOKEN" = "null" ]; then
  fail "could not extract token from /api/runners/tokens response"
  cat /tmp/smoke-token.json
  exit 1
fi

# ── Bring up the runner with the token injected ──────────────────────

info "starting branchwork-runner…"
BRANCHWORK_RUNNER_TOKEN="$RUNNER_TOKEN" \
E2E_PORT="$E2E_PORT" \
  docker compose -p "$PROJECT" -f "$COMPOSE_FILE" --profile saas up -d branchwork-runner

# ── Wait for runner to report online ─────────────────────────────────

RUNNER_TIMEOUT=20
elapsed=0
info "waiting for runner to report online (timeout: ${RUNNER_TIMEOUT}s)…"
runner_status=""
while [ "$elapsed" -lt "$RUNNER_TIMEOUT" ]; do
  body=$(curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" 2>/dev/null || echo '{}')
  runner_status=$(printf '%s' "$body" | jq -r '.runners[0].status // empty' 2>/dev/null || true)
  if [ "$runner_status" = "online" ]; then
    ok "runner online after ${elapsed}s"
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

if [ "$runner_status" != "online" ]; then
  fail "runner did not report online within ${RUNNER_TIMEOUT}s (last status=$runner_status)"
  docker compose -p "$PROJECT" -f "$COMPOSE_FILE" logs --tail=80 branchwork-runner || true
  exit 1
fi

# ── Ensure Playwright deps are installed (offline-friendly) ──────────

if [ ! -d "$REPO_ROOT/web/node_modules/@playwright/test" ]; then
  info "installing web deps (one-time; @playwright/test missing)…"
  pnpm --filter @branchwork/web install
fi

# Provision a Playwright-managed browser if our cache lacks the
# pinned chromium revision. This is a no-op when the cache already
# has the right one — Playwright keys browsers on (channel, revision).
PLAYWRIGHT_BIN="$REPO_ROOT/web/node_modules/.bin/playwright"
if [ -x "$PLAYWRIGHT_BIN" ]; then
  "$PLAYWRIGHT_BIN" install chromium >/dev/null 2>&1 || \
    warn "playwright install chromium failed — continuing with whatever is cached"
fi

# ── Run the spec ─────────────────────────────────────────────────────

info "running Playwright spec web/e2e/runner-banners.spec.ts…"
cd "$REPO_ROOT/web"

set +e
BASE_URL="$BASE_URL" \
SMOKE_PROJECT="$PROJECT" \
SMOKE_COMPOSE_FILE="$COMPOSE_FILE" \
SMOKE_USER_EMAIL="$USER_EMAIL" \
SMOKE_USER_PASSWORD="$USER_PASSWORD" \
SMOKE_ARTIFACTS_DIR="$ARTIFACTS_DIR" \
  pnpm test:smoke
spec_rc=$?
set -e

cd "$REPO_ROOT"

# ── Stub a notes.md the human reviewer can fill in ───────────────────

if [ ! -f "$ARTIFACTS_DIR/notes.md" ]; then
  cat > "$ARTIFACTS_DIR/notes.md" <<EOF
# Smoke run $RUN_DATE — UX rough-edge notes

Project: $PROJECT
Base URL: $BASE_URL (torn down)
Spec: web/e2e/runner-banners.spec.ts

Per the plan brief: any UX rough edges noticed in the four screenshots
are NOT blockers — they are follow-up tickets. Common scan targets:

- Banner placement (above the input vs. below)
- Banner copy (no_runner_connected wording, runner_unavailable wording)
- Folder dropdown loading state (does it flash, or stay hidden?)
- "Directory does not exist" prompt: confirm-button label, escape route
- 4. mid-flight kill: does the form re-enable submit after the error?

## Screenshots

- 01-folder-suggestions.png        runner-side dirs in dropdown
- 02-no-runner-banner.png          stopped runner, amber banner
- 03a-create-folder-prompt.png     non-existent path → confirm modal
- 03b-after-create.png             plan creation succeeded
- 04-runner-unavailable.png        kill mid-flight → red error banner

## Notes

(fill in by hand)

EOF
  ok "wrote notes.md template"
fi

# ── Verdict ──────────────────────────────────────────────────────────

if [ "$spec_rc" -ne 0 ]; then
  fail "Playwright spec failed (rc=$spec_rc)"
  fail "trace + screenshots: $ARTIFACTS_DIR"
  exit "$spec_rc"
fi

ok "smoke spec passed; artefacts at $ARTIFACTS_DIR"
exit 0
