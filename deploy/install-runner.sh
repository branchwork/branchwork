#!/bin/sh
# Branchwork runner installer.
#
# Usage:
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- <TOKEN>
#
# The dashboard substitutes its public URL into __SAAS_URL__ when serving
# this script (see server-rs/src/saas/install_runner.rs); the only piece
# the operator pastes is the single-use token returned by
# `GET /api/runners/install-command`.
#
# What this does:
#   1. Detect uname -s / uname -m → linux-amd64, linux-arm64, darwin-arm64
#      (the only triples published today; darwin-amd64 falls back to source).
#   2. Drop a `branchwork-runner` binary at $HOME/.local/bin.
#   3. Write $HOME/.branchwork-runner/config.toml with the token + SaaS URL.
#   4. Start the runner in the background (nohup + &) so the dashboard's
#      WS receives `runner_connected` and the modal flips to "Connected!".
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
TOKEN="${1:-}"
INSTALL_DIR="${BRANCHWORK_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="$HOME/.branchwork-runner"
CONFIG_FILE="$CONFIG_DIR/config.toml"
LOG_FILE="$CONFIG_DIR/runner.log"
PID_FILE="$CONFIG_DIR/runner.pid"

err()  { printf 'error: %s\n' "$*" >&2; }
ok()   { printf '* %s\n' "$*"; }
note() { printf '  %s\n' "$*"; }

if [ -z "$TOKEN" ]; then
    err "missing token argument"
    note "usage: curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- <TOKEN>"
    exit 2
fi

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
ok "detected platform: ${os}-${arch}"

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

chmod 0755 "$TMP_BIN"
mv "$TMP_BIN" "$BIN"
trap - EXIT
ok "installed $BIN"

# ── Write config ────────────────────────────────────────────────────────────
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

# ── Start runner in the background ──────────────────────────────────────────
# nohup + & detaches so the curl|sh pipeline returns. The runner persists
# its runner_id in $HOME/.branchwork/runner.db (under seq_tracker) so a
# restart preserves identity and the same token re-authenticates the same
# runner.
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

cat <<NEXT

  Next steps:
    • Check status:  curl -fsSL $SAAS_URL/api/runners
    • Tail log:      tail -f $LOG_FILE
    • Stop runner:   kill \$(cat $PID_FILE)

  For a long-running service (systemd user unit / launchd plist), see
  the Branchwork docs: $SAAS_URL/docs/runner-as-a-service
NEXT
