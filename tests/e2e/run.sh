#!/usr/bin/env bash
# E2E test runner for Branchwork.
# Builds the Docker image, starts the container, runs tests, tears down.
#
# Usage:
#   tests/e2e/run.sh [--keep]
#
# Flags:
#   --keep   Skip teardown after tests (useful for debugging)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/deploy/docker-compose.e2e.yml"

export E2E_PORT="${E2E_PORT:-3199}"
export BASE_URL="http://localhost:${E2E_PORT}"

KEEP=false
HEALTH_TIMEOUT=30

for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=true ;;
    *) echo "Unknown flag: $arg"; exit 1 ;;
  esac
done

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
    warn "Skipping teardown (--keep). Container still running at $BASE_URL"
    warn "Clean up manually: docker compose -f $COMPOSE_FILE down -v"
    return
  fi
  info "Tearing down containers..."
  docker compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true
}

trap teardown EXIT

# ── Idempotency: tear down any leftover container ───────────────────

info "Cleaning up any previous e2e environment..."
docker compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true

# ── Build ───────────────────────────────────────────────────────────

info "Building Docker image..."
docker compose -f "$COMPOSE_FILE" build
info "Build complete."

# ── Start ───────────────────────────────────────────────────────────

info "Starting container (port $E2E_PORT)..."
docker compose -f "$COMPOSE_FILE" up -d
info "Container started."

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
