#!/usr/bin/env bash
# Containerized Playwright runner for the SaaS terminal-renders spec.
#
# Owns the full lifecycle:
#   1. Build + boot deploy/docker-compose.e2e.yml + e2e-stub.yml saas
#      profile under a unique compose project name.
#   2. Sign up a user, mint a runner token, start the runner with that
#      token in the env, wait for `online`.
#   3. `docker cp` the pre-canned plan into /data/plans/.
#   4. Export BRANCHWORK_E2E_BASE_URL/EMAIL/PASSWORD and run
#      Playwright with playwright.containerized.config.ts.
#   5. Tear down (trap EXIT) — `docker compose -p <project> down -v`.
#      Containerised cleanup is intrinsically scoped (CLAUDE.md / ADR
#      0005), so it cannot reach host processes.
#
# Usage:
#   tests/e2e/run-playwright-saas.sh [--keep]
#
# Flags:
#   --keep   Skip teardown so you can poke at the running stack.

set -euo pipefail

# Colors / log helpers — same shape as tests/e2e/run.sh.
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
RESET='\033[0m'
info()  { echo -e "${BOLD}[playwright-saas]${RESET} $*"; }
ok()    { echo -e "${GREEN}[OK]${RESET} $*"; }
fail()  { echo -e "${RED}[FAIL]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${RESET} $*"; }

# ── Args ────────────────────────────────────────────────────────────
KEEP=false
for arg in "$@"; do
    case "$arg" in
        --keep) KEEP=true ;;
        *) fail "unknown arg: $arg"; exit 2 ;;
    esac
done

# ── Paths + per-run identity ────────────────────────────────────────
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
COMPOSE_E2E="$REPO_ROOT/deploy/docker-compose.e2e.yml"
COMPOSE_STUB="$REPO_ROOT/deploy/docker-compose.e2e-stub.yml"
PLAN_FILE="$HERE/saas-terminal-renders.plan.yaml"
PLAN_NAME="saas-terminal-renders"

# Unique compose project name — required by ADR 0005 so concurrent
# runs (or a leftover from a crashed run) do not stomp each other.
PROJECT="bw-pw-$(date +%s)-$$"
E2E_PORT="${E2E_PORT:-3299}"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-60}"
RUNNER_ONLINE_TIMEOUT="${RUNNER_ONLINE_TIMEOUT:-30}"
BASE_URL="http://localhost:${E2E_PORT}"

# ── Teardown ────────────────────────────────────────────────────────
teardown() {
    if [ "$KEEP" = true ]; then
        warn "Skipping teardown (--keep). Stack still running."
        warn "  Project: $PROJECT"
        warn "  URL:     $BASE_URL"
        warn "  Stop:    docker compose -p $PROJECT down -v"
        return
    fi
    info "Tearing down compose project $PROJECT..."
    docker compose -p "$PROJECT" \
        -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" \
        --profile saas down -v 2>/dev/null || true
}
trap teardown EXIT

# ── Pre-flight ──────────────────────────────────────────────────────
command -v docker >/dev/null || { fail "docker not on PATH"; exit 2; }
command -v jq     >/dev/null || { fail "jq not on PATH";     exit 2; }
[ -f "$COMPOSE_E2E"  ] || { fail "missing $COMPOSE_E2E";  exit 2; }
[ -f "$COMPOSE_STUB" ] || { fail "missing $COMPOSE_STUB"; exit 2; }
[ -f "$PLAN_FILE"    ] || { fail "missing $PLAN_FILE";    exit 2; }

# ── Build + start server (no runner yet) ────────────────────────────
info "Building image..."
E2E_PORT="$E2E_PORT" docker compose -p "$PROJECT" \
    -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" build branchwork

info "Starting branchwork (port $E2E_PORT, project=$PROJECT)..."
E2E_PORT="$E2E_PORT" docker compose -p "$PROJECT" \
    -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" up -d branchwork

info "Waiting for /health (timeout: ${HEALTH_TIMEOUT}s)..."
elapsed=0
while [ "$elapsed" -lt "$HEALTH_TIMEOUT" ]; do
    if curl -sf "$BASE_URL/health" >/dev/null 2>&1; then
        ok "Server healthy after ${elapsed}s."
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done
if [ "$elapsed" -ge "$HEALTH_TIMEOUT" ]; then
    fail "Health check timed out after ${HEALTH_TIMEOUT}s"
    docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" logs --tail=80
    exit 1
fi

# ── Sign up + mint runner token ─────────────────────────────────────
COOKIE_JAR="$(mktemp)"
EMAIL="terminal-renders-$(date +%s)-$$@example.test"
PASSWORD="e2e-password-1234"

info "Signing up $EMAIL..."
signup_status=$(curl -sS -o /tmp/$$.signup.json -w "%{http_code}" \
    -c "$COOKIE_JAR" \
    -H "Content-Type: application/json" \
    -X POST "$BASE_URL/api/auth/signup" \
    -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}")
if [ "$signup_status" != "201" ]; then
    fail "Signup returned $signup_status (expected 201)"
    cat /tmp/$$.signup.json
    exit 1
fi

info "Minting runner token..."
token_status=$(curl -sS -o /tmp/$$.token.json -w "%{http_code}" \
    -b "$COOKIE_JAR" \
    -H "Content-Type: application/json" \
    -X POST "$BASE_URL/api/runners/tokens" \
    -d '{"runner_name":"e2e-terminal-renders"}')
if [ "$token_status" != "201" ]; then
    fail "Token creation returned $token_status (expected 201)"
    cat /tmp/$$.token.json
    exit 1
fi
RUNNER_TOKEN=$(jq -r .token < /tmp/$$.token.json)
[ -n "$RUNNER_TOKEN" ] && [ "$RUNNER_TOKEN" != "null" ] || {
    fail "Could not extract token"; cat /tmp/$$.token.json; exit 1;
}
ok "Token minted."

# ── Bring up runner with token + stub overlay ───────────────────────
info "Starting branchwork-runner with token..."
BRANCHWORK_RUNNER_TOKEN="$RUNNER_TOKEN" \
E2E_PORT="$E2E_PORT" \
docker compose -p "$PROJECT" \
    -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" \
    --profile saas up -d branchwork-runner

info "Waiting for runner to report online (timeout: ${RUNNER_ONLINE_TIMEOUT}s)..."
elapsed=0
while [ "$elapsed" -lt "$RUNNER_ONLINE_TIMEOUT" ]; do
    online=$(curl -sS -b "$COOKIE_JAR" "$BASE_URL/api/runners" 2>/dev/null \
             | jq -r '.runners[]? | select(.status=="online") | .id' 2>/dev/null \
             | head -1)
    if [ -n "$online" ]; then
        ok "Runner online: $online"
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done
if [ "$elapsed" -ge "$RUNNER_ONLINE_TIMEOUT" ]; then
    fail "Runner did not report online within ${RUNNER_ONLINE_TIMEOUT}s"
    docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" logs --tail=80 branchwork-runner
    exit 1
fi

# ── Drop the plan into /data/plans (file watcher picks it up) ──────
info "Dropping plan into server /data/plans/..."
SERVER_CID=$(docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" ps -q branchwork)
[ -n "$SERVER_CID" ] || { fail "could not resolve server container id"; exit 1; }
docker exec "$SERVER_CID" mkdir -p /data/plans
docker cp "$PLAN_FILE" "$SERVER_CID:/data/plans/${PLAN_NAME}.yaml"
ok "Plan dropped."

# ── Pre-create the project folder in the RUNNER container ──────────
# project_dir_for() on the server joins HOME with the plan's project
# field (here: $HOME/saas-terminal-renders). The server's user is
# `branchwork` with HOME=/home/branchwork, so the path it sends to
# the runner is literally `/home/branchwork/saas-terminal-renders`.
# The runner uses that path verbatim as the agent cwd; the supervisor
# `chdir`s into it before exec'ing the stub. Without this step the
# supervisor dies on `chdir(ENOENT)` and the agent never produces
# bytes — the spec then fails for the wrong reason.
RUNNER_CID=$(docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" ps -q branchwork-runner)
[ -n "$RUNNER_CID" ] || { fail "could not resolve runner container id"; exit 1; }
docker exec -u root "$RUNNER_CID" sh -c \
    "mkdir -p /home/branchwork/${PLAN_NAME} && chown 1000:1000 /home/branchwork/${PLAN_NAME}"
ok "Project dir created in runner container."

# ── Run Playwright ──────────────────────────────────────────────────
info "Running Playwright spec..."
cd "$REPO_ROOT/web"
BRANCHWORK_E2E_BASE_URL="$BASE_URL" \
BRANCHWORK_E2E_EMAIL="$EMAIL" \
BRANCHWORK_E2E_PASSWORD="$PASSWORD" \
pnpm exec playwright test --config playwright.containerized.config.ts
status=$?

if [ "$status" -eq 0 ]; then
    ok "Spec PASSED — terminal renders bytes (Tasks 4.1 + 4.2 are honored)."
else
    fail "Spec FAILED — empty-terminal regression reproduced."
    info "Server logs:"
    docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" logs --tail=80 branchwork
    info "Runner logs:"
    docker compose -p "$PROJECT" -f "$COMPOSE_E2E" -f "$COMPOSE_STUB" logs --tail=80 branchwork-runner
fi
exit $status
