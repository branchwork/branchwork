#!/usr/bin/env bash
# E2E regression test: GET /install-runner.sh — substituted-script flow.
#
# Reproduces and pins the T5.22 bug: prior to v0.5.18 the on-disk
# `deploy/install-runner.sh` had the unsubstituted-sentinel check
# `[ "$SAAS_URL" = "__SAAS_URL__" ]` written literally. The server's
# `render_install_script` did a blanket `.replace("__SAAS_URL__", url)`,
# so both line-42 (the SAAS_URL default) AND line-60 (the sentinel
# check) got rewritten to the same string. The check then evaluated
# to `[ "$URL" = "$URL" ]` — always true — and every customer hit
# "this script was not fetched from a Branchwork dashboard" before any
# real work happened. The 5.18 fix splits the line-60 sentinel into
# three concatenated shell literals so the contiguous placeholder
# remains only on line 42.
#
# Strategy:
#  1. Mint a runner token via POST /api/runners/install-command — gives
#     us a real token + the curl command the dashboard shows the user.
#  2. Run that exact command inside a one-shot alpine container so the
#     test cannot touch the host's runner inventory (CLAUDE.md rule).
#  3. Assert the substituted-sentinel error does NOT appear in the
#     output. Anything downstream — binary download, runner connect —
#     is acceptable to fail; only the sentinel branch is regression-
#     critical here.
#
# The alpine container is ephemeral (`docker run --rm`); cleanup is
# automatic and the host runner / agent supervisor remains untouched.
#
# Skip outside saas mode (the runner-token + install-command endpoints
# are saas-only). Inherits BASE_URL + COOKIE_JAR from run.sh.

set -euo pipefail

if [ "${E2E_MODE:-standalone}" != "saas" ]; then
    echo "SKIP test_saas_install_runner_sh.sh (E2E_MODE=$E2E_MODE, need saas)"
    exit 0
fi

: "${BASE_URL:?BASE_URL must be set by tests/e2e/run.sh}"
: "${COOKIE_JAR:?COOKIE_JAR must be set by tests/e2e/run.sh}"

RUNNER_NAME="t522-install-$RANDOM"
echo "=== minting runner token ($RUNNER_NAME) ==="
INSTALL_JSON="$(curl -fsS -b "$COOKIE_JAR" \
    -H 'Content-Type: application/json' \
    -d "{\"runner_name\":\"$RUNNER_NAME\"}" \
    "$BASE_URL/api/runners/install-command")"

TOKEN="$(printf '%s' "$INSTALL_JSON" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
if [ -z "$TOKEN" ]; then
    echo "FAIL: could not extract token from install-command response:"
    echo "$INSTALL_JSON"
    exit 1
fi
echo "  token: ${TOKEN:0:12}…"

echo "=== fetching install-runner.sh through one-shot alpine ==="
# The container reaches the host's e2e server via host-gateway. We pass
# the URL in so the script can be rebuilt in any future variant
# (different ports, different hostnames). Output goes to a file we then
# grep for the regression signature.
OUT_FILE="$(mktemp)"
trap 'rm -f "$OUT_FILE"' EXIT

set +e
docker run --rm \
    --add-host=host.docker.internal:host-gateway \
    -e BW_URL="http://host.docker.internal:${E2E_PORT:-3199}" \
    -e BW_TOKEN="$TOKEN" \
    alpine:latest sh -c '
        apk add --no-cache curl >/dev/null 2>&1
        # `|| true` because the script will fail downstream (binary
        # extraction expects an :edge image with linux/musl runner — the
        # e2e image is one rolling tag and may not match). We only care
        # about the sentinel-check branch.
        curl -fsSL "$BW_URL/install-runner.sh" | sh -s -- "$BW_TOKEN" || true
    ' >"$OUT_FILE" 2>&1
set -e

if grep -qF 'this script was not fetched from a Branchwork dashboard' "$OUT_FILE"; then
    echo "FAIL: sentinel branch fired — T5.22 regression."
    echo "--- script output ---"
    cat "$OUT_FILE"
    exit 1
fi
echo "PASS: rendered install-runner.sh passes the unsubstituted-sentinel check"
