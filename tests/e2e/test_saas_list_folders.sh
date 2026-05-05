#!/usr/bin/env bash
# E2E test: GET /api/folders dispatches to the runner in saas mode.
#
# Pre-seeds disjoint directories in the runner ($HOME=/home/runneruser) and
# server ($HOME=/home/branchwork — set by USER branchwork in the Dockerfile)
# containers, then asserts that GET /api/folders returns the runner's
# listing. The marker on the server side proves the dispatch went over the
# WebSocket round-trip rather than reading the server's local FS — a
# fake-runner harness can't tell server-fs from runner-fs apart, but two
# real containers with disjoint filesystems can.
#
# Skips outside saas mode. Inherits state from tests/e2e/run.sh:
#   BASE_URL       server origin
#   COMPOSE_FILE   docker-compose.e2e.yml path
#   COOKIE_JAR     cookie file authenticated as the runner-owning user
#   E2E_MODE       standalone | saas

set -euo pipefail

if [ "${E2E_MODE:-standalone}" != "saas" ]; then
  echo "[skip] test_saas_list_folders only runs under E2E_MODE=saas"
  exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "[fail] jq is required on PATH"
  exit 1
fi

if [ -z "${COOKIE_JAR:-}" ] || [ ! -s "$COOKIE_JAR" ]; then
  echo "[fail] COOKIE_JAR not set or empty (run.sh should provision it)"
  exit 1
fi

if [ -z "${COMPOSE_FILE:-}" ] || [ ! -f "$COMPOSE_FILE" ]; then
  echo "[fail] COMPOSE_FILE not set or missing"
  exit 1
fi

RUNNER_NAMES=(saas-test-alpha saas-test-bravo saas-test-charlie)
SERVER_MARKER=server-only-marker
RUNNER_HOME=/home/runneruser
SERVER_HOME=/home/branchwork

cleanup() {
  for name in "${RUNNER_NAMES[@]}"; do
    docker compose -f "$COMPOSE_FILE" exec -T branchwork-runner \
      rm -rf "$RUNNER_HOME/$name" >/dev/null 2>&1 || true
  done
  docker compose -f "$COMPOSE_FILE" exec -T branchwork \
    rm -rf "$SERVER_HOME/$SERVER_MARKER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── Pre-seed runner container ────────────────────────────────────────
echo "[info] seeding runner $RUNNER_HOME with: ${RUNNER_NAMES[*]}"
for name in "${RUNNER_NAMES[@]}"; do
  docker compose -f "$COMPOSE_FILE" exec -T branchwork-runner \
    mkdir -p "$RUNNER_HOME/$name"
done

# ── Pre-seed server container ────────────────────────────────────────
# This folder lives on the server's local FS. The dispatch in
# api/settings.rs::list_folders must NOT read it — if it does, the
# assertion below catches the regression.
echo "[info] seeding server $SERVER_HOME with: $SERVER_MARKER"
docker compose -f "$COMPOSE_FILE" exec -T branchwork \
  mkdir -p "$SERVER_HOME/$SERVER_MARKER"

# ── Call /api/folders ────────────────────────────────────────────────
echo "[info] GET $BASE_URL/api/folders"
response_file=$(mktemp)
trap 'cleanup; rm -f "$response_file"' EXIT
status=$(curl -sS -o "$response_file" -w "%{http_code}" \
  -b "$COOKIE_JAR" \
  "$BASE_URL/api/folders")

if [ "$status" != "200" ]; then
  echo "[fail] /api/folders returned HTTP $status (expected 200)"
  cat "$response_file"
  exit 1
fi

# ── Assertions ───────────────────────────────────────────────────────
fail=0

# (a) Every runner-side folder must appear in the response.
for name in "${RUNNER_NAMES[@]}"; do
  if jq -e --arg n "$name" 'map(.name) | index($n) != null' \
      < "$response_file" >/dev/null; then
    echo "[ok] response contains runner-side folder: $name"
  else
    echo "[fail] response missing runner-side folder: $name"
    fail=1
  fi
done

# (b) Server-only marker must NOT appear — proves runner dispatch.
if jq -e --arg n "$SERVER_MARKER" 'map(.name) | index($n) != null' \
    < "$response_file" >/dev/null; then
  echo "[fail] response contains server-only folder: $SERVER_MARKER"
  echo "       /api/folders read the server's local FS instead of"
  echo "       dispatching to the runner — saas dispatch is broken."
  fail=1
else
  echo "[ok] response does not contain server-only marker"
fi

exit "$fail"
