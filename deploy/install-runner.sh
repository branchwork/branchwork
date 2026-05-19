#!/bin/sh
# Branchwork runner installer (idempotent).
#
# Usage:
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- <TOKEN>
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s --                       (update)
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --rotate-token <TOKEN>
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --reset <TOKEN>
#
# The dashboard substitutes its public URL into __SAAS_URL__ when serving
# this script (see server-rs/src/saas/install_runner.rs); the only piece
# the operator pastes is the single-use token returned by
# `GET /api/runners/install-command`.
#
# Two-mode behaviour (T2.1):
#
#   First run (no $HOME/.branchwork-runner/config.toml on disk) — ENROLL.
#     1. Detect uname -s / uname -m → linux-amd64, linux-arm64, darwin-arm64.
#     2. Drop a `branchwork-runner` binary at $HOME/.local/bin.
#     3. Write $HOME/.branchwork-runner/config.toml with the token + SaaS URL.
#     4. Start the runner in the background (nohup + &) so the dashboard's
#        WS receives `runner_connected` and the modal flips to "Connected!".
#
#   Subsequent runs (config.toml already present) — UPDATE.
#     1. Stop the existing runner via $PID_FILE (kill -TERM, then -KILL).
#     2. Re-use $HOME/.branchwork-runner/runner.db so runner_id is preserved
#        and any queued outbox state survives the upgrade.
#     3. Replace the binary in place.
#     4. DO NOT rewrite the token — the stored one stays unless the operator
#        passed `--rotate-token <NEW>` explicitly.
#     5. Restart the runner. Final line reports
#        "updated runner in place (runner_id preserved)".
#
#   `--reset <TOKEN>` is the destructive flow for true re-enrollment: wipe
#     runner.db (drop runner_id + outbox), rewrite config.toml with the new
#     token, then proceed exactly like the first-run path.
#
# Environment overrides (rarely needed):
#   BRANCHWORK_SAAS_URL    — override the URL baked in by the server.
#   BRANCHWORK_BINARY_URL  — direct URL to a runner binary; skips detection.
#   BRANCHWORK_INSTALL_DIR — destination dir, default $HOME/.local/bin.
#
# Binary sources, tried in order:
#   1. $BRANCHWORK_BINARY_URL (operator override).
#   2. GitHub Release asset (https://github.com/branchwork/branchwork/...).
#      404s silently when releases aren't shipped yet.
#   3. `docker create + cp` from ghcr.io/branchwork/branchwork:edge — the
#      multi-arch :edge tag DOES carry both amd64 and arm64 runner binaries
#      today, so this is the working MVP path on Linux.
#
# Limitations:
#   • macOS extract path: the :edge image only carries linux/musl binaries,
#     so docker-extract on Darwin produces a Linux ELF that won't run.
#     On macOS the script falls through to a build-from-source hint.
#   • Windows: not supported in this MVP. Track follow-ups in
#     ~/.claude/plans/backlog/.

set -eu

SAAS_URL="${BRANCHWORK_SAAS_URL:-__SAAS_URL__}"
INSTALL_DIR="${BRANCHWORK_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="$HOME/.branchwork-runner"
CONFIG_FILE="$CONFIG_DIR/config.toml"
DB_FILE="$CONFIG_DIR/runner.db"
LOG_FILE="$CONFIG_DIR/runner.log"
PID_FILE="$CONFIG_DIR/runner.pid"

err()  { printf 'error: %s\n' "$*" >&2; }
ok()   { printf '* %s\n' "$*"; }
note() { printf '  %s\n' "$*"; }

# ── Parse args (T2.1) ───────────────────────────────────────────────────────
# POSIX getopts can't handle long options, so do it by hand. One positional
# argument (the token) plus two long flags. Order is not significant.
TOKEN=""
RESET=0
ROTATE_TOKEN=0

usage() {
    cat <<USAGE
usage:
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- <TOKEN>
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s --                       (update existing)
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --rotate-token <TOKEN>
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --reset <TOKEN>
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --reset)
            RESET=1
            ;;
        --rotate-token)
            ROTATE_TOKEN=1
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --*)
            err "unknown flag: $1"
            usage >&2
            exit 2
            ;;
        *)
            if [ -n "$TOKEN" ]; then
                err "unexpected extra argument: $1"
                usage >&2
                exit 2
            fi
            TOKEN="$1"
            ;;
    esac
    shift
done

# Sentinel for "the server forgot to substitute the SAAS_URL placeholder".
# Built from three concatenated string literals so this exact value never
# appears as a contiguous run of bytes in the on-wire script — that's how
# we keep render_install_script's blanket `.replace("__SAAS_URL__", url)`
# from clobbering the check itself (the T5.22 regression).
_UNSUBSTITUTED="__""SAAS_URL""__"
if [ "$SAAS_URL" = "$_UNSUBSTITUTED" ]; then
    err "this script was not fetched from a Branchwork dashboard"
    note "set BRANCHWORK_SAAS_URL or run the curl command from the /runners page"
    exit 2
fi

# Strip a trailing slash so the runner binary stitches its URL correctly.
SAAS_URL="${SAAS_URL%/}"

# ── Decide enroll vs update vs reset (T2.1) ────────────────────────────────
# The presence of $CONFIG_FILE is the load-bearing signal: we have a prior
# install on this host. Without it, we treat --reset as a fresh enroll
# (operator may have run --reset before the first enroll by mistake — note
# but don't fail).
if [ -f "$CONFIG_FILE" ]; then
    if [ "$RESET" = "1" ]; then
        MODE=reset
    else
        MODE=update
    fi
else
    if [ "$RESET" = "1" ]; then
        note "--reset has no effect: no existing config at $CONFIG_FILE"
    fi
    MODE=enroll
fi

# Read a TOML scalar from $CONFIG_FILE without pulling in a TOML parser.
# Matches lines shaped like `key = "value"` with optional surrounding
# whitespace. The runner config we write is two such lines (saas_url + token),
# so this is sufficient — and the function returns the empty string when no
# match is found, which callers branch on.
read_toml_string() {
    sed -n 's/^[[:space:]]*'"$1"'[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' \
        "$CONFIG_FILE" 2>/dev/null | head -n 1
}

case "$MODE" in
    enroll)
        if [ -z "$TOKEN" ]; then
            err "missing token argument"
            usage >&2
            exit 2
        fi
        if [ "$ROTATE_TOKEN" = "1" ]; then
            err "--rotate-token is only meaningful for an existing install"
            note "no $CONFIG_FILE on disk — drop --rotate-token and pass the token positionally"
            exit 2
        fi
        ;;
    reset)
        if [ -z "$TOKEN" ]; then
            err "--reset requires a fresh token argument"
            usage >&2
            exit 2
        fi
        if [ "$ROTATE_TOKEN" = "1" ]; then
            err "--rotate-token and --reset are mutually exclusive (use --reset alone)"
            exit 2
        fi
        ;;
    update)
        if [ "$ROTATE_TOKEN" = "1" ]; then
            if [ -z "$TOKEN" ]; then
                err "--rotate-token requires a new token argument"
                exit 2
            fi
        else
            # Token from config, not the wire. Operators routinely re-paste
            # the same curl-pipe-sh from the dashboard — accept the positional
            # token silently rather than erroring (the dashboard shows the
            # ORIGINAL token at issue time; subsequent rotations are a
            # separate flow).
            if [ -n "$TOKEN" ]; then
                note "ignoring positional token (pass --rotate-token to replace the stored one)"
                TOKEN=""
            fi
            STORED_TOKEN="$(read_toml_string token)"
            if [ -z "$STORED_TOKEN" ]; then
                err "could not read token from $CONFIG_FILE"
                note "if the file is corrupted, re-run with --reset <TOKEN> to drop it and start fresh"
                exit 1
            fi
            TOKEN="$STORED_TOKEN"
            # Honor a stored saas_url when the operator didn't override via
            # env — the dashboard's hostname may have changed (Cloudflare
            # rebind, dev box vs prod), but on a routine binary update the
            # on-disk config is the authoritative SaaS endpoint for this
            # host.
            if [ -z "${BRANCHWORK_SAAS_URL:-}" ]; then
                STORED_SAAS="$(read_toml_string saas_url)"
                if [ -n "$STORED_SAAS" ]; then
                    SAAS_URL="${STORED_SAAS%/}"
                fi
            fi
        fi
        ;;
esac

# ── Detect platform ─────────────────────────────────────────────────────────
os_raw="$(uname -s)"
arch_raw="$(uname -m)"
case "$os_raw" in
    Linux)  os=linux ;;
    Darwin) os=darwin ;;
    *)
        err "unsupported OS: $os_raw"
        note "supported: Linux, macOS"
        exit 2
        ;;
esac
case "$arch_raw" in
    x86_64|amd64)   arch=amd64 ;;
    aarch64|arm64)  arch=arm64 ;;
    *)
        err "unsupported architecture: $arch_raw"
        exit 2
        ;;
esac
ok "detected platform: ${os}-${arch} (mode: $MODE)"

# ── Helpers to land a binary at $TMP_BIN ────────────────────────────────────
mkdir -p "$INSTALL_DIR"
BIN="$INSTALL_DIR/branchwork-runner"
TMP_BIN="$(mktemp 2>/dev/null || mktemp -t bwrunner)"
trap 'rm -f "$TMP_BIN"' EXIT

is_native_executable() {
    # ELF magic for Linux, Mach-O magic for macOS. The Mach-O check covers
    # all three flavors (cffaedfe / cefaedfe / 0xfeedfacf) by matching the
    # common "feedfa" suffix; xxd portability across BSD/GNU is shaky so we
    # use od + the printable-ASCII detector head for ELF.
    head -c 4 "$1" 2>/dev/null | od -An -c 2>/dev/null | grep -qE 'E   L   F|177  E  L  F|^[[:space:]]+(\\312\\376\\272\\276|\\317\\372\\355\\376)' && return 0
    # Fallback: use file(1) when available (more reliable than od matching).
    if command -v file >/dev/null 2>&1; then
        file "$1" 2>/dev/null | grep -qE 'ELF|Mach-O' && return 0
    fi
    return 1
}

download_binary() {
    url="$1"
    if curl -fsSL "$url" -o "$TMP_BIN" 2>/dev/null; then
        if [ -s "$TMP_BIN" ] && is_native_executable "$TMP_BIN"; then
            return 0
        fi
    fi
    return 1
}

extract_via_docker() {
    if ! command -v docker >/dev/null 2>&1; then
        return 1
    fi
    if [ "$os" != "linux" ]; then
        # The :edge image only ships linux/musl binaries — extracting on
        # Darwin would produce a Linux ELF that can't execute.
        return 1
    fi
    image="ghcr.io/branchwork/branchwork:edge"
    plat="linux/$arch"
    note "extracting $image ($plat) via docker"
    cid="$(docker create --platform="$plat" "$image" 2>/dev/null)" || return 1
    if docker cp "$cid:/usr/local/bin/branchwork-runner" "$TMP_BIN" 2>/dev/null; then
        docker rm "$cid" >/dev/null 2>&1 || true
        return 0
    fi
    docker rm "$cid" >/dev/null 2>&1 || true
    return 1
}

# ── Pick a source ──────────────────────────────────────────────────────────
found=0
if [ -n "${BRANCHWORK_BINARY_URL:-}" ]; then
    note "trying override: $BRANCHWORK_BINARY_URL"
    download_binary "$BRANCHWORK_BINARY_URL" && found=1
fi
if [ "$found" -eq 0 ]; then
    rel_url="https://github.com/branchwork/branchwork/releases/latest/download/branchwork-runner-${os}-${arch}"
    note "trying release asset: $rel_url"
    download_binary "$rel_url" && found=1
fi
if [ "$found" -eq 0 ]; then
    extract_via_docker && found=1
fi

if [ "$found" -eq 0 ]; then
    err "could not obtain a runner binary"
    note "options:"
    note "  • set BRANCHWORK_BINARY_URL=<url> and re-run"
    note "  • install Docker so the script can extract ghcr.io/branchwork/branchwork:edge"
    if [ "$os" = "darwin" ]; then
        note "  • build from source (no macOS release shipped yet):"
        note "      cargo install --git https://github.com/branchwork/branchwork \\"
        note "                    --bin branchwork-runner"
    fi
    exit 1
fi

# ── Stop existing runner (update / reset) ──────────────────────────────────
# We only ever target the specific PID this installer wrote to $PID_FILE;
# never `pgrep -f branchwork-runner` — the host may run the production
# Branchwork supervisor in another shell session (CLAUDE.md ADR 0005 rule).
stop_existing_runner() {
    [ -f "$PID_FILE" ] || return 0
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [ -z "$pid" ]; then
        return 0
    fi
    # Numeric-PID guard. The file is on disk and may have been tampered with
    # or corrupted; refusing a non-numeric value keeps us from passing
    # arbitrary strings to `kill`.
    case "$pid" in
        ''|*[!0-9]*)
            note "ignoring non-numeric pid '$pid' in $PID_FILE"
            return 0
            ;;
    esac
    if ! kill -0 "$pid" 2>/dev/null; then
        note "existing runner (pid $pid) was not running; cleaning up pid file"
        rm -f "$PID_FILE"
        return 0
    fi
    note "stopping existing runner (pid $pid)"
    kill -TERM "$pid" 2>/dev/null || true
    i=0
    while [ $i -lt 5 ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        sleep 1
        i=$((i + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        note "runner did not exit within 5s, sending SIGKILL"
        kill -KILL "$pid" 2>/dev/null || true
        sleep 1
    fi
    rm -f "$PID_FILE"
}

if [ "$MODE" != "enroll" ]; then
    stop_existing_runner
fi

# ── Reset wipes runner.db (and SQLite WAL/SHM sidecars) ────────────────────
# Only fires on --reset. The acceptance criterion for update mode is that
# runner_id is preserved across runs; reset is the opt-in escape hatch when
# the operator explicitly wants a fresh runner_id (true re-enrollment).
if [ "$MODE" = "reset" ]; then
    if [ -f "$DB_FILE" ]; then
        rm -f "$DB_FILE" "$DB_FILE-shm" "$DB_FILE-wal"
        ok "wiped $DB_FILE (runner_id will be regenerated on next start)"
    fi
fi

# ── Install the new binary ──────────────────────────────────────────────────
chmod 0755 "$TMP_BIN"
mv "$TMP_BIN" "$BIN"
trap - EXIT
ok "installed $BIN"

# ── Write config (enroll / reset / explicit rotate) ────────────────────────
# Update mode (no --rotate-token) deliberately skips this step: the on-disk
# config.toml is authoritative for the token + saas_url, and rewriting it
# would clobber any operator hand-edits.
if [ "$MODE" = "enroll" ] || [ "$MODE" = "reset" ] || [ "$ROTATE_TOKEN" = "1" ]; then
    mkdir -p "$CONFIG_DIR"
    chmod 0700 "$CONFIG_DIR"
    # Token is sensitive — write 0600 so other shell users on the box cannot
    # read it. The runner reads from this file via --token / --saas-url args
    # below; the file is for re-launch only.
    old_umask="$(umask)"
    umask 077
    cat > "$CONFIG_FILE" <<TOML
# Branchwork runner config — written by install-runner.sh.
# The token is single-use until the runner connects; the server then
# binds it to this runner's id so a re-paste from another host fails.
saas_url = "$SAAS_URL"
token    = "$TOKEN"
TOML
    umask "$old_umask"
    ok "wrote $CONFIG_FILE"
fi

# ── Start runner in the background ──────────────────────────────────────────
# nohup + & detaches so the curl|sh pipeline returns. The runner persists
# its runner_id in $HOME/.branchwork-runner/runner.db (under seq_tracker) so
# update mode preserves identity across binary replacements; reset mode
# wiped runner.db above so a fresh runner_id will be generated here.
note "starting runner in the background"
nohup "$BIN" --saas-url "$SAAS_URL" --token "$TOKEN" \
    >"$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"
sleep 1
if ! kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    err "runner exited immediately — last lines of $LOG_FILE:"
    tail -n 20 "$LOG_FILE" >&2 || true
    exit 1
fi
ok "runner started (pid $(cat "$PID_FILE"))"

# ── Mode-aware completion line ──────────────────────────────────────────────
# The acceptance criterion for Task 2.1 is that a second install run reports
# "updated runner in place (runner_id preserved)" verbatim — that string is
# what the prod dashboard's deploy runbook greps for.
case "$MODE" in
    enroll) ok "enrolled runner with the dashboard" ;;
    update) ok "updated runner in place (runner_id preserved)" ;;
    reset)  ok "re-enrolled runner (runner_id reset)" ;;
esac

cat <<NEXT

  Next steps:
    • Check status:  curl -fsSL $SAAS_URL/api/runners
    • Tail log:      tail -f $LOG_FILE
    • Stop runner:   kill \$(cat $PID_FILE)

  For a long-running service (systemd user unit / launchd plist), see
  the Branchwork docs: $SAAS_URL/docs/runner-as-a-service
NEXT
