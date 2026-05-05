#!/usr/bin/env bash
# E2E test: no-runner-connected and disconnect-mid-call error surface.
#
# Stops the provisioned runner, asserts that the SaaS dispatch endpoints
# (GET /api/folders + POST /api/plans/create) fail with 503
# `no_runner_connected` when the org has a `runners` row in the DB but no
# live WebSocket. Then brings the runner back up and asserts both endpoints
# succeed.
#
# The final phase covers the disconnect-mid-call cleanup path from task 1.4.
# `docker compose pause` SIGSTOPs the runner via the cgroups freezer; then
# we send a request that gets registered in the server's pending map and
# routed over WS but cannot be replied to (the runner is frozen); then
# `docker compose kill` drops TCP, the server's WS reader yields Closed,
# and the cleanup branch runs `runner.pending.lock().await.clear()` —
# our awaiting receiver wakes with RunnerDisconnected → 504 in <1s rather
# than the 8s helper timeout. Pause-then-kill makes the in-flight window
# deterministic: there is no code path where the runner can reply while
# paused, so the only way out of pending is the disconnect drain.
#
# Skips outside saas mode. Inherits state from tests/e2e/run.sh:
#   BASE_URL       server origin
#   COMPOSE_FILE   docker-compose.e2e.yml path
#   COOKIE_JAR     cookie file authenticated as the runner-owning user
#   E2E_MODE       standalone | saas
#
# This test is destructive: it stops the runner mid-run. We restore it on
# exit so it does not break later tests (alphabetically this currently
# runs last, but the cleanup keeps the harness composable).

set -euo pipefail

if [ "${E2E_MODE:-standalone}" != "saas" ]; then
  echo "[skip] test_saas_no_runner only runs under E2E_MODE=saas"
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

PROJECT_NAME=no-runner-test-folder
RUNNER_HOME=/home/runneruser
RUNNER_PATH="$RUNNER_HOME/$PROJECT_NAME"

response_file=$(mktemp)
inflight_meta=$(mktemp)
inflight_body=$(mktemp)

cleanup() {
  # Best-effort cleanup: bring the runner back up so subsequent tests can
  # share this harness. --profile saas is required because the service is
  # gated behind it in docker-compose.e2e.yml.
  docker compose -f "$COMPOSE_FILE" --profile saas start branchwork-runner \
    >/dev/null 2>&1 || true
  rm -f "$response_file" "$inflight_meta" "$inflight_body"
}
trap cleanup EXIT

# ── Helpers ──────────────────────────────────────────────────────────

# Poll /api/runners until the first runner reports the given status, or
# the timeout elapses. Echoes the final status for callers to assert on.
wait_for_runner_status() {
  local want="$1" timeout="$2"
  local elapsed=0 status=""
  while [ "$elapsed" -lt "$timeout" ]; do
    body=$(curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" 2>/dev/null || echo '{}')
    status=$(printf '%s' "$body" | jq -r '.runners[0].status // empty' 2>/dev/null || true)
    if [ "$status" = "$want" ]; then
      printf '%s' "$status"
      return 0
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done
  printf '%s' "$status"
  return 1
}

# Send one request and assert it returns the expected HTTP status + JSON
# `error` field. Args: <method> <path> <body-or-empty> <expected-status>
# <expected-error>.
assert_request() {
  local method="$1" path="$2" body="$3" want_status="$4" want_error="$5"
  local args=(-sS -o "$response_file" -w '%{http_code}'
              -b "$COOKIE_JAR" -X "$method" "$BASE_URL$path")
  if [ -n "$body" ]; then
    args+=(-H 'Content-Type: application/json' -d "$body")
  fi
  local got_status
  got_status=$(curl "${args[@]}")
  if [ "$got_status" != "$want_status" ]; then
    echo "[fail] $method $path: expected HTTP $want_status, got $got_status"
    cat "$response_file"
    exit 1
  fi
  if [ -n "$want_error" ]; then
    local got_error
    got_error=$(jq -r '.error // empty' < "$response_file")
    if [ "$got_error" != "$want_error" ]; then
      echo "[fail] $method $path: expected error=$want_error, got: $got_error"
      cat "$response_file"
      exit 1
    fi
  fi
  echo "[ok] $method $path → $got_status${want_error:+ ($want_error)}"
}

# ── Phase 0: confirm runner is initially online ──────────────────────
echo "[info] Phase 0: confirm runner is online (run.sh provisioned it)"
if ! wait_for_runner_status online 5 >/dev/null; then
  echo "[fail] runner not online before test starts — run.sh setup broken"
  curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" || true
  exit 1
fi
echo "[ok] runner is online"

# ── Phase 1: stop the runner, wait for server to detect disconnect ───
echo "[info] Phase 1: stop runner, wait for server to detect disconnect"
docker compose -f "$COMPOSE_FILE" stop -t 1 branchwork-runner >/dev/null
final_status=$(wait_for_runner_status offline 10 || true)
if [ "$final_status" = "online" ]; then
  echo "[fail] runner still reports online 10s after stop — disconnect not detected"
  exit 1
fi
echo "[ok] runner reported as $final_status"

# Verify the runners row still exists (org_has_runner must still be true).
runner_count=$(curl -sf -b "$COOKIE_JAR" "$BASE_URL/api/runners" \
  | jq -r '.runners | length')
if [ "$runner_count" -lt 1 ]; then
  echo "[fail] runners row was removed; org_has_runner would be false → test invalid"
  exit 1
fi
echo "[ok] runners row preserved (count=$runner_count) — org_has_runner=true"

# ── Phase 2: GET /api/folders → 503 no_runner_connected ──────────────
echo "[info] Phase 2: GET /api/folders → expect 503 no_runner_connected"
assert_request GET /api/folders '' 503 no_runner_connected

# ── Phase 3: POST /api/plans/create → 503 no_runner_connected ────────
echo "[info] Phase 3: POST /api/plans/create createFolder=false → expect 503"
create_body="{\"folder\":\"~/$PROJECT_NAME\",\"description\":\"e2e no-runner test\",\"createFolder\":false}"
assert_request POST /api/plans/create "$create_body" 503 no_runner_connected

# ── Phase 4: bring runner back, wait online ──────────────────────────
echo "[info] Phase 4: start runner, wait for online"
docker compose -f "$COMPOSE_FILE" --profile saas start branchwork-runner >/dev/null
if ! wait_for_runner_status online 15 >/dev/null; then
  echo "[fail] runner did not come back online within 15s"
  docker compose -f "$COMPOSE_FILE" logs --tail=80 branchwork-runner || true
  exit 1
fi
echo "[ok] runner online again"

# ── Phase 5: both endpoints succeed (round-trip with live runner) ────
echo "[info] Phase 5: GET /api/folders → 200 (round-trip via WS)"
assert_request GET /api/folders '' 200 ''

echo "[info] Phase 5: POST /api/plans/create createFolder=false → 400 folder_not_found"
# 400 with folder_not_found proves the request reached the runner and the
# runner reported the folder is missing — the round-trip works.
assert_request POST /api/plans/create "$create_body" 400 folder_not_found

# ── Phase 6: disconnect-mid-call → 504 runner_unavailable < 2s ───────
# The cleanup path in saas/runner_ws.rs drops every pending oneshot sender
# when the WS closes, so an in-flight runner_request call wakes immediately
# with RunnerDisconnected → 504 instead of waiting the 8s helper timeout.
#
# Constructing a deterministic in-flight window is tricky: ListFolders
# completes in ~10ms on the runner while `docker compose stop` takes
# ~100ms+ via the daemon, so a naive race always loses (the runner replies
# before stop lands).
#
# Trick: `docker compose pause` uses the cgroups freezer to SIGSTOP the
# runner *instantly*. We pause first, send the curl (which registers in
# pending and gets the message queued in the runner's TCP recv buffer —
# the kernel still accepts data on a paused process), then kill the
# container. The kill drops TCP, the WS read loop on the server returns
# Closed, the cleanup branch runs `pending.clear()`, and our awaiting
# receiver wakes with RunnerDisconnected → 504.
#
# Pause-then-kill makes the in-flight window deterministic: there is no
# code path where the runner can reply while paused, so the only way out
# of pending is the cleanup drain.
echo "[info] Phase 6: disconnect-mid-call → expect 504 runner_unavailable < 2s"

# Make sure runner is online before pausing.
if ! wait_for_runner_status online 15 >/dev/null; then
  echo "[fail] runner not online before disconnect-mid-call test"
  exit 1
fi

# Pause the runner — cgroups freezer SIGSTOPs all PIDs in the container.
# WS connection on the kernel side stays alive; runner code can't run.
echo "[info]   pausing runner (cgroups freezer)"
docker compose -f "$COMPOSE_FILE" pause branchwork-runner >/dev/null

# Background the curl. With the runner paused it cannot reply, so this
# request stays in `pending` until something forces it out. --max-time
# 15 keeps the test bounded if the server is wedged.
( curl -sS --max-time 15 \
    -o "$inflight_body" \
    -w '%{time_total} %{http_code}\n' \
    -b "$COOKIE_JAR" \
    "$BASE_URL/api/folders" > "$inflight_meta" 2>/dev/null ) &
curl_pid=$!

# Give the request 200ms to land at the server, register in `pending`,
# and ship the WireMessage over WS to the (paused) runner. This is well
# short of the 8s helper timeout, so we are firmly inside the in-flight
# window when we kill.
sleep 0.2

# Kill the runner. SIGKILL is delivered even to paused processes. TCP
# socket close → server WS read loop yields Closed → cleanup branch
# runs `runner.pending.lock().await.clear()` → our oneshot sender is
# dropped → curl returns 504 within ms.
echo "[info]   killing paused runner"
docker compose -f "$COMPOSE_FILE" kill -s SIGKILL branchwork-runner >/dev/null

# Wait for curl. wait returns the curl exit status; we ignore it because
# curl with --max-time 15 always returns regardless.
wait "$curl_pid" || true

time_total=""; status_code=""
read -r time_total status_code < "$inflight_meta" || true
if [ -z "$status_code" ]; then
  echo "[fail]   curl produced no status (meta='$(cat "$inflight_meta")')"
  cat "$inflight_body" 2>/dev/null || true
  exit 1
fi

echo "[info]   curl returned status=$status_code time=${time_total}s"

if [ "$status_code" != "504" ]; then
  echo "[fail] expected 504 (runner_unavailable), got $status_code"
  cat "$inflight_body" 2>/dev/null || true
  exit 1
fi

# Time bound: the cleanup path should wake the receiver within tens of ms
# of the kill landing. Allow 2s for end-to-end Docker overhead. If it
# took ≥ 2s, the cleanup did NOT fire and we hit the 8s helper timeout
# instead — task 1.4 (drop pending senders on disconnect) is broken.
too_slow=$(awk -v t="$time_total" 'BEGIN { print (t >= 2.0) ? 1 : 0 }')
if [ "$too_slow" = "1" ]; then
  echo "[fail] 504 took ${time_total}s — cleanup path did NOT fire,"
  echo "       request waited for the full 8s helper timeout. Task 1.4"
  echo "       (drop pending senders on disconnect) is broken."
  exit 1
fi

err=$(jq -r '.error // empty' < "$inflight_body")
if [ "$err" != "runner_unavailable" ]; then
  echo "[fail] expected error=runner_unavailable, got: $err"
  cat "$inflight_body"
  exit 1
fi

echo "[ok] disconnect-mid-call: 504 runner_unavailable in ${time_total}s"

echo "[pass] all phases — no_runner_connected (503) and runner_unavailable (504)"
exit 0
