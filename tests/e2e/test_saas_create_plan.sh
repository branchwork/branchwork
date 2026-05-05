#!/usr/bin/env bash
# E2E test: POST /api/plans/create with `createFolder` round-trips through the
# runner — proof that `mkdir` runs on the runner host, not the server host.
#
# Two-phase contract pinned by api/plans.rs::create_plan in saas mode:
#
#   1. `createFolder: false` against a non-existent path → 400 with
#      `error: "folder_not_found"` and `resolvedFolder` echoing the
#      runner-side absolute path. The runner's check_or_create_folder always
#      sets resolved_path even on the not-found arm so the dashboard can
#      render the create-folder prompt without a second round-trip.
#
#   2. `createFolder: true` → 200 with `agentId`, plus the directory must
#      exist on the runner (test -d → 0) and NOT on the server (test -d → 1).
#      The disjoint-filesystem check is the load-bearing assertion: a fake-
#      runner harness in-process can't tell the two apart, but two real
#      containers with separate $HOME volumes can.
#
# Skips outside saas mode. Inherits state from tests/e2e/run.sh:
#   BASE_URL       server origin
#   COMPOSE_FILE   docker-compose.e2e.yml path
#   COOKIE_JAR     cookie file authenticated as the runner-owning user
#   E2E_MODE       standalone | saas

set -euo pipefail

if [ "${E2E_MODE:-standalone}" != "saas" ]; then
  echo "[skip] test_saas_create_plan only runs under E2E_MODE=saas"
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

PROJECT_NAME=new-saas-project
RUNNER_HOME=/home/runneruser
RUNNER_PATH="$RUNNER_HOME/$PROJECT_NAME"
# Server's $HOME differs (USER branchwork in Dockerfile) but the path we test
# for non-existence is the runner-absolute path. Even if the server's home
# were /home/runneruser, no test step ever writes there.

response_file=$(mktemp)

cleanup() {
  docker compose -f "$COMPOSE_FILE" exec -T branchwork-runner \
    rm -rf "$RUNNER_PATH" >/dev/null 2>&1 || true
  rm -f "$response_file"
}
trap cleanup EXIT

# Pre-cleanup so a previous failed run can't preload the dir.
docker compose -f "$COMPOSE_FILE" exec -T branchwork-runner \
  rm -rf "$RUNNER_PATH" >/dev/null 2>&1 || true

# ── Phase 1: createFolder=false against missing path → 400 ───────────
echo "[info] POST /api/plans/create createFolder=false (expect 400)"
status=$(curl -sS -o "$response_file" -w "%{http_code}" \
  -b "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -X POST "$BASE_URL/api/plans/create" \
  -d "{\"folder\":\"~/$PROJECT_NAME\",\"description\":\"e2e create plan test\",\"createFolder\":false}")

if [ "$status" != "400" ]; then
  echo "[fail] createFolder=false returned HTTP $status (expected 400)"
  cat "$response_file"
  exit 1
fi

err=$(jq -r '.error // empty' < "$response_file")
if [ "$err" != "folder_not_found" ]; then
  echo "[fail] expected error=folder_not_found, got: $err"
  cat "$response_file"
  exit 1
fi
echo "[ok] response.error == folder_not_found"

resolved=$(jq -r '.resolvedFolder // empty' < "$response_file")
if [ "$resolved" != "$RUNNER_PATH" ]; then
  echo "[fail] expected resolvedFolder=$RUNNER_PATH, got: $resolved"
  cat "$response_file"
  exit 1
fi
echo "[ok] response.resolvedFolder == $RUNNER_PATH"

# ── Phase 2: createFolder=true → 200 + agentId ───────────────────────
echo "[info] POST /api/plans/create createFolder=true (expect 200)"
status=$(curl -sS -o "$response_file" -w "%{http_code}" \
  -b "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -X POST "$BASE_URL/api/plans/create" \
  -d "{\"folder\":\"~/$PROJECT_NAME\",\"description\":\"e2e create plan test\",\"createFolder\":true}")

if [ "$status" != "200" ]; then
  echo "[fail] createFolder=true returned HTTP $status (expected 200)"
  cat "$response_file"
  exit 1
fi

agent_id=$(jq -r '.agentId // empty' < "$response_file")
if [ -z "$agent_id" ] || [ "$agent_id" = "null" ]; then
  echo "[fail] response missing agentId"
  cat "$response_file"
  exit 1
fi
echo "[ok] response.agentId == $agent_id"

folder=$(jq -r '.folder // empty' < "$response_file")
if [ "$folder" != "$RUNNER_PATH" ]; then
  echo "[fail] expected folder=$RUNNER_PATH, got: $folder"
  cat "$response_file"
  exit 1
fi
echo "[ok] response.folder == $RUNNER_PATH"

# ── Phase 3: dir exists on runner, not on server ─────────────────────
# This is the single most important assertion in the whole test — proof
# the mkdir ran on the right host. test -d returns 0 if dir exists, 1 if
# not. We invert via `if !` so a failure of either branch is loud.
echo "[info] verifying $RUNNER_PATH exists on runner..."
if ! docker compose -f "$COMPOSE_FILE" exec -T branchwork-runner \
    test -d "$RUNNER_PATH" >/dev/null 2>&1; then
  echo "[fail] $RUNNER_PATH does NOT exist on the runner — mkdir didn't land"
  exit 1
fi
echo "[ok] $RUNNER_PATH exists on the runner"

echo "[info] verifying $RUNNER_PATH does NOT exist on server..."
if docker compose -f "$COMPOSE_FILE" exec -T branchwork \
    test -d "$RUNNER_PATH" >/dev/null 2>&1; then
  echo "[fail] $RUNNER_PATH exists on the SERVER — saas dispatch is broken"
  echo "       create_plan should mkdir on the runner, not on the server."
  exit 1
fi
echo "[ok] $RUNNER_PATH does not exist on the server"

# ── Phase 4: agent row carries the runner-resolved cwd ───────────────
# start_agent_via_runner inserts the agents row synchronously before any
# WS dispatch, so it should be visible immediately — but poll briefly
# to absorb any first-request latency on a freshly-started server.
echo "[info] GET /api/agents — looking for agent $agent_id with cwd=$RUNNER_PATH"
agents_file=$(mktemp)
trap 'cleanup; rm -f "$agents_file"' EXIT

found=false
elapsed=0
while [ "$elapsed" -lt 5 ]; do
  curl -sS -o "$agents_file" -b "$COOKIE_JAR" "$BASE_URL/api/agents" >/dev/null
  cwd=$(jq -r --arg id "$agent_id" 'map(select(.id == $id)) | .[0].cwd // empty' \
    < "$agents_file")
  if [ -n "$cwd" ]; then
    found=true
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

if [ "$found" != true ]; then
  echo "[fail] agent $agent_id never appeared in GET /api/agents within 5s"
  cat "$agents_file"
  exit 1
fi

if [ "$cwd" != "$RUNNER_PATH" ]; then
  echo "[fail] expected agent.cwd=$RUNNER_PATH, got: $cwd"
  cat "$agents_file"
  exit 1
fi
echo "[ok] agent.cwd == $RUNNER_PATH"

exit 0
