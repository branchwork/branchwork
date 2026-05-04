#!/usr/bin/env bash
# E2E test runner for Branchwork.
# Builds the Docker image, starts the container, runs tests, tears down.
#
# Usage:
#   tests/e2e/run.sh [--keep]
#
# Env:
#   E2E_MODE   standalone (default) — server only.
#              saas               — server + branchwork-runner. The driver
#                                   signs up a user and mints a runner token
#                                   via the API, then brings up the runner
#                                   service with the token injected.
#   E2E_PORT   host port for the server (default 3199).
#
# Flags:
#   --keep   Skip teardown after tests (useful for debugging — leaves both
#            containers running so you can poke at them).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/deploy/docker-compose.e2e.yml"

export E2E_PORT="${E2E_PORT:-3199}"
export BASE_URL="http://localhost:${E2E_PORT}"
export E2E_MODE="${E2E_MODE:-standalone}"

KEEP=false
HEALTH_TIMEOUT=30
RUNNER_ONLINE_TIMEOUT=15

for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=true ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

case "$E2E_MODE" in
  standalone|saas) ;;
  *) echo "Unknown E2E_MODE: $E2E_MODE (expected standalone or saas)"; exit 1 ;;
esac

# ── Helpers ──────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "${BOLD}[e2e]${RESET} $*"; }
ok()    { echo -e "${GREEN}[PASS]${RESET} $*"; }
fail()  { echo -e "${RED}[FAIL]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${RESET} $*"; }

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0
FAILED_NAMES=()

# ── Teardown (trap) ─────────────────────────────────────────────────

teardown() {
  if [ "$KEEP" = true ]; then
    warn "Skipping teardown (--keep). Containers still running."
    warn "  Server:   $BASE_URL"
    if [ "$E2E_MODE" = "saas" ]; then
      warn "  Runner:   docker compose -f $COMPOSE_FILE logs branchwork-runner"
    fi
    warn "Clean up manually: docker compose -f $COMPOSE_FILE --profile saas down -v"
    return
  fi
  info "Tearing down containers..."
  # Always pass --profile saas on teardown so the runner is removed even when
  # we ran in standalone mode (cheap; no-op if it never came up).
  docker compose -f "$COMPOSE_FILE" --profile saas down -v 2>/dev/null || true
}

trap teardown EXIT

# ── Idempotency: tear down any leftover container ───────────────────

info "Cleaning up any previous e2e environment..."
docker compose -f "$COMPOSE_FILE" --profile saas down -v 2>/dev/null || true

# ── Build ───────────────────────────────────────────────────────────

info "Building Docker image..."
docker compose -f "$COMPOSE_FILE" build
info "Build complete."

# ── Start server ────────────────────────────────────────────────────

info "Starting branchwork (port $E2E_PORT, mode=$E2E_MODE)..."
docker compose -f "$COMPOSE_FILE" up -d branchwork
info "Server container started."

# ── Health check ────────────────────────────────────────────────────

info "Waiting for /health to return 200 (timeout: ${HEALTH_TIMEOUT}s)..."
elapsed=0
while [ "$elapsed" -lt "$HEALTH_TIMEOUT" ]; do
  if curl -sf "$BASE_URL/health" >/dev/null 2>&1; then
    info "Server is healthy after ${elapsed}s."
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

if [ "$elapsed" -ge "$HEALTH_TIMEOUT" ]; then
  fail "Health check timed out after ${HEALTH_TIMEOUT}s"
  info "Container logs:"
  docker compose -f "$COMPOSE_FILE" logs --tail=50
  exit 1
fi

# ── Provision runner (saas mode only) ───────────────────────────────

if [ "$E2E_MODE" = "saas" ]; then
  if ! command -v jq >/dev/null 2>&1; then
    fail "saas mode requires jq on PATH (apt-get install jq / brew install jq)"
    exit 1
  fi

  COOKIE_JAR="$(mktemp)"
  E2E_USER_EMAIL="e2e-$(date +%s)-$$@example.test"
  E2E_USER_PASSWORD="e2e-password-1234"

  info "Signing up e2e user $E2E_USER_EMAIL..."
  signup_status=$(curl -sS -o /tmp/e2e-signup.json -w "%{http_code}" \
    -c "$COOKIE_JAR" \
    -H "Content-Type: application/json" \
    -X POST "$BASE_URL/api/auth/signup" \
    -d "{\"email\":\"$E2E_USER_EMAIL\",\"password\":\"$E2E_USER_PASSWORD\"}")
  if [ "$signup_status" != "201" ]; then
    fail "Signup returned $signup_status (expected 201)"
    cat /tmp/e2e-signup.json
    exit 1
  fi

  info "Minting runner token via /api/runners/tokens..."
  token_status=$(curl -sS -o /tmp/e2e-token.json -w "%{http_code}" \
    -b "$COOKIE_JAR" \
    -H "Content-Type: application/json" \
    -X POST "$BASE_URL/api/runners/tokens" \
    -d '{"runner_name":"e2e-runner"}')
  if [ "$token_status" != "201" ]; then
    fail "Token creation returned $token_status (expected 201)"
    cat /tmp/e2e-token.json
    exit 1
  fi

  RUNNER_TOKEN=$(jq -r .token < /tmp/e2e-token.json)
  if [ -z "$RUNNER_TOKEN" ] || [ "$RUNNER_TOKEN" = "null" ]; then
    fail "Could not extract token from /api/runners/tokens response"
    cat /tmp/e2e-token.json
    exit 1
  fi
  export BRANCHWORK_RUNNER_TOKEN="$RUNNER_TOKEN"

  info "Starting branchwork-runner..."
  docker compose -f "$COMPOSE_FILE" --profile saas up -d branchwork-runner
  info "Runner container started."

  info "Waiting for runner to report online (timeout: ${RUNNER_ONLINE_TIMEOUT}s)..."
  online=false
  elapsed=0
  while [ "$elapsed" -lt "$RUNNER_ONLINE_TIMEOUT" ]; do
    body=$(curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" 2>/dev/null || echo '{}')
    status=$(printf '%s' "$body" | jq -r '.runners[0].status // empty' 2>/dev/null || true)
    if [ "$status" = "online" ]; then
      online=true
      info "Runner is online after ${elapsed}s."
      break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done

  if [ "$online" = false ]; then
    fail "Runner did not report online within ${RUNNER_ONLINE_TIMEOUT}s"
    info "GET /api/runners response:"
    curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" || true
    info "Runner container logs:"
    docker compose -f "$COMPOSE_FILE" logs --tail=80 branchwork-runner || true
    exit 1
  fi
fi

# ── Run test scripts ────────────────────────────────────────────────

run_test() {
  local script="$1"
  local name
  name="$(basename "$script" .sh)"

  TESTS_RUN=$((TESTS_RUN + 1))
  info "Running: $name"

  if BASE_URL="$BASE_URL" bash "$script"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    ok "$name"
  else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    FAILED_NAMES+=("$name")
    fail "$name"
  fi
}

# Discover and run test_*.sh files in sorted order
test_scripts=()
while IFS= read -r -d '' f; do
  test_scripts+=("$f")
done < <(find "$SCRIPT_DIR" -maxdepth 1 -name 'test_*.sh' -print0 | sort -z)

if [ ${#test_scripts[@]} -eq 0 ]; then
  warn "No test scripts found (tests/e2e/test_*.sh). Nothing to run."
else
  for script in "${test_scripts[@]}"; do
    run_test "$script"
  done
fi

# ── Summary ─────────────────────────────────────────────────────────

echo ""
info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if [ "$TESTS_FAILED" -gt 0 ]; then
  fail "$TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
  for name in "${FAILED_NAMES[@]}"; do
    fail "  - $name"
  done
  info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  exit 1
elif [ "$TESTS_RUN" -eq 0 ]; then
  info "No tests executed. Infrastructure is working."
  info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  exit 0
else
  ok "All $TESTS_PASSED/$TESTS_RUN tests passed"
  info "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  exit 0
fi
