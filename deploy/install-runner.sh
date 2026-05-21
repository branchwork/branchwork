#!/bin/sh
# BRANCHWORK_SERVER_VERSION=__SERVER_VERSION__
# Branchwork runner installer (idempotent).
#
# The line above is a sentinel parsed by the runner's periodic
# version-drift poll (T4.3): the server substitutes its own
# CARGO_PKG_VERSION for __SERVER_VERSION__ on every GET so a runner
# that hasn't received a `Resume` recently can still discover the
# version the dashboard would have it install. DO NOT rename or move
# this line — `version_poll::SERVER_VERSION_MARKER` in
# `server-rs/src/bin/branchwork_runner.rs` greps for the literal
# prefix.
#
# Usage:
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- <TOKEN>
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s --                       (update)
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --rotate-token <TOKEN>
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --reset <TOKEN>
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --force-replace [TOKEN]
#   curl -fsSL <SAAS_URL>/install-runner.sh | sh -s -- --just-binary [--system]
#
# The dashboard substitutes its public URL into __SAAS_URL__ when serving
# this script (see server-rs/src/saas/install_runner.rs); the only piece
# the operator pastes is the single-use token returned by
# `GET /api/runners/install-command`.
#
# Two-mode behaviour (T2.1, T1.2 systemd lifecycle):
#
#   First run (no $HOME/.branchwork-runner/config.toml on disk) — ENROLL.
#     1. Detect uname -s / uname -m → linux-amd64, linux-arm64, darwin-arm64.
#     2. Drop BOTH `branchwork-runner` and `branchwork-server` binaries at
#        $HOME/.local/bin. The runner shells out to `branchwork-server
#        session …` for every per-agent supervisor; shipping the paired
#        binary alongside the runner means Start session works without
#        any operator-supplied --server-bin hint (T3.1).
#     3. Write $HOME/.branchwork-runner/config.toml with the token + SaaS URL,
#        plus a sidecar `env` file in `KEY=VALUE` syntax for systemd's
#        `EnvironmentFile=` directive (T1.2).
#     4. Render the user-mode systemd unit into
#        `~/.config/systemd/user/branchwork-runner.service` (from the
#        deploy/branchwork-runner.service.in template), then
#        `systemctl --user daemon-reload && enable --now branchwork-runner`.
#        Finally `loginctl enable-linger "$USER"` so the runner survives
#        logouts and reboots without an active login session.
#
#   Subsequent runs (config.toml already present) — UPDATE.
#     1. Detect any legacy nohup-launched runner via $PID_FILE and stop it
#        (one-time migration from pre-T1.2 installs).
#     2. Re-use $HOME/.branchwork-runner/runner.db so runner_id is preserved
#        and any queued outbox state survives the upgrade.
#     3. Replace the binary in place.
#     4. DO NOT rewrite the token — the stored one stays unless the operator
#        passed `--rotate-token <NEW>` explicitly. The sidecar env file is
#        rewritten unconditionally so a pre-T1.2 host migrates naturally.
#     5. Re-render the unit (idempotent), `daemon-reload`, and
#        `enable --now` — `enable` is idempotent if already enabled, and
#        `--now` restarts the binary. Final line reports
#        "updated runner in place (runner_id preserved)".
#
#   --system flips the unit installation from user-mode systemd
#   (`~/.config/systemd/user/`) to system-mode (`/etc/systemd/system/`),
#   using `systemctl` (no `--user`) and skipping linger. Requires root
#   for enroll/update/reset; `--just-binary --system` is unprivileged
#   because it delegates the restart to `sudo systemctl restart`.
#
#   `--reset <TOKEN>` is the destructive flow for true re-enrollment: wipe
#     runner.db (drop runner_id + outbox), rewrite config.toml with the new
#     token, then proceed exactly like the first-run path.
#
#   `--just-binary` (alias: `--upgrade`) is the binary-swap engine for
#     hosts where systemd already owns the runner (see
#     [[runner-daemon-workspace]]). It:
#       • Skips the token check at the top of the script — no enrollment.
#       • Skips writing `$HOME/.branchwork-runner/config.toml` — the
#         existing config (token + saas_url) stays byte-for-byte
#         identical.
#       • Skips any systemd-unit installation / `loginctl enable-linger`
#         dance — the unit is assumed to be installed and enabled
#         already (the runner-daemon-workspace plan owns that path).
#       • Downloads BOTH `branchwork-runner` and `branchwork-server`
#         (the T3.1 paired-install step) into `$HOME/.local/bin/`.
#       • After the swap, runs `systemctl --user restart
#         branchwork-runner` (default) or
#         `sudo systemctl restart branchwork-runner` when `--system`
#         is passed.
#     Idempotency: if the on-disk binaries already match the version
#     the install source would provide, the script exits 0 with
#     `* already at v0.5.X — nothing to do` and does NOT restart the
#     unit. This makes `--just-binary` safe for the dashboard's
#     one-click Upgrade button (task 4.1) and the periodic
#     version-check fallback (task 4.3) to invoke on every tick.
#
# Foreign-runner safety (T2.2):
#
#   Before installing the binary the script scans for any running
#   `branchwork-runner` process whose PID we don't manage (the PID in
#   $PID_FILE belongs to *this* install; anything else is foreign).
#   Detection is kernel-truth — we readlink /proc/<pid>/exe on Linux and
#   ask `lsof -p <pid>` on macOS, never pattern-match command lines (per
#   ADR 0005, pgrep -f on a host running the production Branchwork
#   supervisor is unsafe). On hit, we exit 1 with
#
#     another runner is already running as pid 12345 from /opt/other-runner;
#     pass --force-replace to take over
#
#   so the operator knows what's already running before a second runner
#   competes for the same SaaS slot. Passing `--force-replace` SIGTERMs
#   the foreign PID (5s grace, then SIGKILL) and proceeds. This prevents
#   the accidental two-runners-one-config-dir state where one host runs
#   both a system-package branchwork-runner and a curl-piped one.
#
# Environment overrides (rarely needed):
#   BRANCHWORK_SAAS_URL           — override the URL baked in by the server.
#   BRANCHWORK_BINARY_URL         — direct URL to a runner binary; skips
#                                   detection for the runner only.
#   BRANCHWORK_SERVER_BINARY_URL  — direct URL to a server binary; skips
#                                   detection for the server only. Set
#                                   alongside BRANCHWORK_BINARY_URL when
#                                   dogfooding a custom build pair.
#   BRANCHWORK_INSTALL_DIR        — destination dir, default $HOME/.local/bin.
#
# Binary sources, tried in order (per binary — runner and server are
# resolved independently, so partial success from an earlier source is
# allowed; only an unresolved binary falls through to the next source):
#   1. $BRANCHWORK_BINARY_URL / $BRANCHWORK_SERVER_BINARY_URL
#      (operator overrides, per binary).
#   2. GitHub Release asset (https://github.com/branchwork/branchwork/...).
#      404s silently when releases aren't shipped yet.
#   3. `docker create + cp` from ghcr.io/branchwork/branchwork:edge — the
#      multi-arch :edge tag carries both binaries under /usr/local/bin/
#      (see deploy/Dockerfile stage 3), so a single container creation
#      yields both runner and server. Linux only — the image is musl,
#      not Darwin Mach-O.
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

# ── Parse args (T2.1 / T3.3) ────────────────────────────────────────────────
# POSIX getopts can't handle long options, so do it by hand. One positional
# argument (the token) plus a handful of long flags. Order is not
# significant.
TOKEN=""
RESET=0
ROTATE_TOKEN=0
FORCE_REPLACE=0
JUST_BINARY=0
SYSTEM_INSTALL=0

usage() {
    cat <<USAGE
usage:
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- <TOKEN>
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s --                       (update existing)
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --rotate-token <TOKEN>
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --reset <TOKEN>
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --force-replace [TOKEN]
  curl -fsSL $SAAS_URL/install-runner.sh | sh -s -- --just-binary [--system]

  --system is also valid alongside any of the above (root only) to install
  the systemd unit at /etc/systemd/system/ instead of the user instance at
  ~/.config/systemd/user/. No linger needed for system units.
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
        --force-replace)
            FORCE_REPLACE=1
            ;;
        --just-binary|--upgrade)
            JUST_BINARY=1
            ;;
        --system)
            SYSTEM_INSTALL=1
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

# ── Decide enroll vs update vs reset vs just_binary (T2.1 / T3.3) ──────────
# --just-binary is a separate, top-priority mode: when it is set we skip
# every enroll/update concern (no token, no config rewrite, no foreign-
# runner scan — systemd owns the runner here). We still validate that the
# host actually has an existing install to upgrade.
if [ "$JUST_BINARY" = "1" ]; then
    if [ -n "$TOKEN" ]; then
        err "--just-binary takes no token (it does not enroll, only swaps binaries)"
        usage >&2
        exit 2
    fi
    if [ "$RESET" = "1" ]; then
        err "--just-binary and --reset are mutually exclusive"
        exit 2
    fi
    if [ "$ROTATE_TOKEN" = "1" ]; then
        err "--just-binary and --rotate-token are mutually exclusive"
        exit 2
    fi
    if [ "$FORCE_REPLACE" = "1" ]; then
        err "--just-binary and --force-replace are mutually exclusive"
        exit 2
    fi
    if [ ! -f "$CONFIG_FILE" ]; then
        err "--just-binary requires an existing install at $CONFIG_FILE"
        note "to enroll a new runner, run without --just-binary and pass the token"
        exit 1
    fi
    if ! command -v systemctl >/dev/null 2>&1; then
        err "--just-binary requires systemctl (host has no systemd)"
        note "this mode is for systemd-managed installs only — see runner-daemon-workspace"
        exit 1
    fi
    MODE=just_binary
elif [ -f "$CONFIG_FILE" ]; then
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

# --system writes a unit at /etc/systemd/system/ and reloads system-mode
# systemd — both require root. just_binary --system instead delegates to
# `sudo systemctl restart`, so it can be invoked unprivileged.
if [ "$SYSTEM_INSTALL" = "1" ] && [ "$MODE" != "just_binary" ]; then
    if [ "$(id -u)" != "0" ]; then
        err "--system requires root (use sudo or run as root)"
        note "the system unit lives at /etc/systemd/system/branchwork-runner.service"
        exit 1
    fi
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
    just_binary)
        # No token check, no config read. Token + saas_url stay on disk and
        # the existing systemd unit re-uses them on restart.
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

# ── Foreign-runner detection (T2.2) ─────────────────────────────────────────
#
# Detect any running branchwork-runner process whose PID isn't tracked by
# this install's $PID_FILE. We use kernel-truth (readlink /proc/<pid>/exe
# on Linux, lsof on macOS) rather than `pgrep -f branchwork-runner` for
# two reasons:
#
#   1. ADR 0005 forbids unscoped pgrep -f patterns: matching command-line
#      substrings can pick up unrelated processes (editors with the file
#      open, scrolled tty output, systemd unit dumps).
#   2. On Linux the kernel truncates /proc/<pid>/comm to 15 chars, so
#      `pgrep -x branchwork-runner` silently misses every match
#      (`branchwork-runner` is 17 chars).
#
# /proc/<pid>/exe is the authoritative answer ("what binary is this PID
# actually running?"). On macOS we fall back to `lsof -p <pid>` because
# /proc doesn't exist there.

# Read our managed PID from $PID_FILE, or empty when no file / non-numeric.
our_managed_pid() {
    [ -f "$PID_FILE" ] || return 0
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    case "$pid" in
        ''|*[!0-9]*) return 0 ;;
        *) printf '%s' "$pid" ;;
    esac
}

# List every PID + binary path whose /proc/<pid>/exe basename is
# `branchwork-runner`. Output one line per process: "<pid> <exe>". Empty
# output when nothing matches.
list_branchwork_runners() {
    if [ -d /proc ]; then
        # Linux path: scan /proc/[0-9]*/exe directly. No pattern matching.
        for proc_pid_dir in /proc/[0-9]*; do
            # Guard against the no-match glob expansion (busybox-friendly).
            [ -d "$proc_pid_dir" ] || continue
            pid="${proc_pid_dir##*/}"
            exe="$(readlink "$proc_pid_dir/exe" 2>/dev/null || true)"
            [ -n "$exe" ] || continue
            case "$(basename "$exe" 2>/dev/null)" in
                branchwork-runner) printf '%s %s\n' "$pid" "$exe" ;;
            esac
        done
        return 0
    fi
    # macOS path: pgrep -x on the full process name (Darwin doesn't
    # truncate comm), then verify each via lsof.
    if command -v pgrep >/dev/null 2>&1 && command -v lsof >/dev/null 2>&1; then
        for pid in $(pgrep -x branchwork-runner 2>/dev/null || true); do
            # lsof txt-type FD is the binary mapping. First one wins.
            exe="$(lsof -p "$pid" 2>/dev/null | awk '$4 == "txt" { print $NF; exit }')"
            [ -n "$exe" ] || continue
            case "$(basename "$exe" 2>/dev/null)" in
                branchwork-runner) printf '%s %s\n' "$pid" "$exe" ;;
            esac
        done
    fi
}

# Stop a foreign runner using the same SIGTERM-then-SIGKILL ladder we use
# for our own managed runner. No PID_FILE bookkeeping — that belongs to
# whatever install owns that foreign process.
stop_foreign_runner() {
    pid="$1"
    note "force-replace: stopping foreign runner (pid $pid)"
    kill -TERM "$pid" 2>/dev/null || true
    i=0
    while [ $i -lt 5 ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep 1
        i=$((i + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        note "foreign runner did not exit within 5s, sending SIGKILL"
        kill -KILL "$pid" 2>/dev/null || true
        sleep 1
    fi
}

check_foreign_runners() {
    our_pid="$(our_managed_pid)"
    foreign_pid=""
    foreign_exe=""
    # `list_branchwork_runners | while read` would run the loop in a
    # subshell on plain POSIX `sh`, so any variable set inside (i.e.
    # foreign_pid) would be lost. Route through a temp file instead so
    # the while loop's input redirection keeps it in the current shell.
    tmp_runners="$(mktemp 2>/dev/null || mktemp -t bwrunners 2>/dev/null || true)"
    if [ -z "$tmp_runners" ]; then
        tmp_runners="/tmp/.bw-runners.$$"
    fi
    list_branchwork_runners > "$tmp_runners" 2>/dev/null || true
    if [ -s "$tmp_runners" ]; then
        while read -r pid exe; do
            # Skip our own managed runner.
            if [ -n "$our_pid" ] && [ "$pid" = "$our_pid" ]; then
                continue
            fi
            # Defensive: the process may have just exited between
            # listing and now.
            if ! kill -0 "$pid" 2>/dev/null; then
                continue
            fi
            foreign_pid="$pid"
            foreign_exe="${exe:-unknown path}"
            break
        done < "$tmp_runners"
    fi
    rm -f "$tmp_runners"
    if [ -z "$foreign_pid" ]; then
        return 0
    fi
    if [ "$FORCE_REPLACE" = "1" ]; then
        stop_foreign_runner "$foreign_pid"
        return 0
    fi
    err "another runner is already running as pid $foreign_pid from $foreign_exe; pass --force-replace to take over"
    exit 1
}

# --just-binary skips the foreign-runner scan: systemd owns the runner and
# the unit's PID lives in /run/user/<uid>/, not in our $PID_FILE — every
# systemd-managed runner would otherwise be flagged as "foreign". The
# subsequent `systemctl restart` is the safe handoff.
if [ "$MODE" != "just_binary" ]; then
    check_foreign_runners
fi

# ── Helpers to land binaries at $TMP_RUNNER + $TMP_SERVER ──────────────────
#
# Two binaries to install (T3.1): the runner itself and the paired
# `branchwork-server`. They are sourced INDEPENDENTLY — i.e. an env
# override for one does not skip the GitHub-release / docker-extract
# attempt for the other — so an operator dogfooding a custom runner
# build still gets a matching server from the next source. Both must
# land in $TMP_* tmpfiles before either is moved into $INSTALL_DIR, so
# a partial download cannot leave the host with a mismatched pair.
mkdir -p "$INSTALL_DIR"
RUNNER_BIN="$INSTALL_DIR/branchwork-runner"
SERVER_BIN="$INSTALL_DIR/branchwork-server"
TMP_RUNNER="$(mktemp 2>/dev/null || mktemp -t bwrunner)"
TMP_SERVER="$(mktemp 2>/dev/null || mktemp -t bwserver)"
trap 'rm -f "$TMP_RUNNER" "$TMP_SERVER"' EXIT

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

# Download $1=url → $2=output path; verify executable magic before returning
# success. Used per-binary so callers can target either $TMP_RUNNER or
# $TMP_SERVER without dragging in `set` or `eval` indirection.
download_binary_to() {
    url="$1"
    out="$2"
    if curl -fsSL "$url" -o "$out" 2>/dev/null; then
        if [ -s "$out" ] && is_native_executable "$out"; then
            return 0
        fi
    fi
    return 1
}

# Pull both binaries from a single multi-arch :edge container.
# The container is created once and removed in either branch, so we never
# leak a stopped container even on partial cp failure. Both files must
# land successfully — a one-binary extract is a hard fail because the
# release asset fallback already ran (we'd be installing a mismatched
# pair otherwise).
#
# $1=1 if we need the runner, $2=1 if we need the server. The branching
# lets us skip a `docker cp` when an earlier source already provided one
# of the two binaries (e.g. BRANCHWORK_BINARY_URL satisfied the runner,
# BRANCHWORK_SERVER_BINARY_URL did not, fall through to docker for the
# server only).
extract_via_docker() {
    need_runner_arg="$1"
    need_server_arg="$2"
    if [ "$need_runner_arg" -eq 0 ] && [ "$need_server_arg" -eq 0 ]; then
        return 0
    fi
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
    docker_ok=1
    if [ "$need_runner_arg" -eq 1 ]; then
        if ! docker cp "$cid:/usr/local/bin/branchwork-runner" "$TMP_RUNNER" 2>/dev/null; then
            docker_ok=0
        elif ! is_native_executable "$TMP_RUNNER"; then
            docker_ok=0
        fi
    fi
    if [ "$need_server_arg" -eq 1 ] && [ "$docker_ok" -eq 1 ]; then
        if ! docker cp "$cid:/usr/local/bin/branchwork-server" "$TMP_SERVER" 2>/dev/null; then
            docker_ok=0
        elif ! is_native_executable "$TMP_SERVER"; then
            docker_ok=0
        fi
    fi
    docker rm "$cid" >/dev/null 2>&1 || true
    if [ "$docker_ok" -eq 1 ]; then
        return 0
    fi
    return 1
}

# ── Pick a source ──────────────────────────────────────────────────────────
#
# Source resolution order (per binary): env override → GitHub release →
# docker extract. The runner and server flags are tracked independently
# so an earlier source partially satisfying the pair doesn't reset the
# unsatisfied side.
need_runner=1
need_server=1

# Source 1 — env overrides (per binary).
if [ -n "${BRANCHWORK_BINARY_URL:-}" ]; then
    note "trying override: $BRANCHWORK_BINARY_URL"
    if download_binary_to "$BRANCHWORK_BINARY_URL" "$TMP_RUNNER"; then
        need_runner=0
    fi
fi
if [ -n "${BRANCHWORK_SERVER_BINARY_URL:-}" ]; then
    note "trying server override: $BRANCHWORK_SERVER_BINARY_URL"
    if download_binary_to "$BRANCHWORK_SERVER_BINARY_URL" "$TMP_SERVER"; then
        need_server=0
    fi
fi

# Source 2 — GitHub release assets, one URL per binary per platform.
if [ "$need_runner" -eq 1 ]; then
    rel_runner_url="https://github.com/branchwork/branchwork/releases/latest/download/branchwork-runner-${os}-${arch}"
    note "trying release asset: $rel_runner_url"
    if download_binary_to "$rel_runner_url" "$TMP_RUNNER"; then
        need_runner=0
    fi
fi
if [ "$need_server" -eq 1 ]; then
    rel_server_url="https://github.com/branchwork/branchwork/releases/latest/download/branchwork-server-${os}-${arch}"
    note "trying release asset: $rel_server_url"
    if download_binary_to "$rel_server_url" "$TMP_SERVER"; then
        need_server=0
    fi
fi

# Source 3 — docker extract (one container, both binaries copied).
if [ "$need_runner" -eq 1 ] || [ "$need_server" -eq 1 ]; then
    if extract_via_docker "$need_runner" "$need_server"; then
        need_runner=0
        need_server=0
    fi
fi

if [ "$need_runner" -eq 1 ] || [ "$need_server" -eq 1 ]; then
    missing=""
    if [ "$need_runner" -eq 1 ]; then
        missing="branchwork-runner"
    fi
    if [ "$need_server" -eq 1 ]; then
        if [ -n "$missing" ]; then
            missing="$missing and branchwork-server"
        else
            missing="branchwork-server"
        fi
    fi
    err "could not obtain $missing"
    note "options:"
    note "  • set BRANCHWORK_BINARY_URL=<url> (runner) and/or"
    note "    BRANCHWORK_SERVER_BINARY_URL=<url> (server), then re-run"
    note "  • install Docker so the script can extract ghcr.io/branchwork/branchwork:edge"
    if [ "$os" = "darwin" ]; then
        note "  • build from source (no macOS release shipped yet):"
        note "      cargo install --git https://github.com/branchwork/branchwork \\"
        note "                    --bin branchwork-runner --bin branchwork-server"
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

# enroll has no prior PID file; just_binary delegates lifecycle to systemd
# (the unit stop happens implicitly as part of the post-swap restart).
case "$MODE" in
    update|reset) stop_existing_runner ;;
esac

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

# ── Idempotency check for --just-binary (T3.3) ─────────────────────────────
# Compare on-disk vs about-to-install versions BEFORE the mv. If both
# binaries already match what the source would provide, exit 0 without
# touching the unit. The EXIT trap (set at TMP_* creation time) cleans up
# the downloaded tmpfiles automatically.
#
# Version strings are the first line of `<binary> --version` (e.g.
# `branchwork-runner 0.5.67`). An empty probe — old binary missing or
# pre-T3.2 binary without `#[command(version)]` — collapses to "unknown"
# which never matches the new probe, so we fall through to the swap.
short_version() {
    # "branchwork-runner 0.5.67" -> "0.5.67"; an empty input yields the
    # empty string so the caller can branch on it.
    case "$1" in
        '') printf '' ;;
        *)  printf '%s' "${1##* }" ;;
    esac
}

# `curl -o` writes tmpfiles 0644 by umask; the idempotency probe needs to
# execute them, so chmod up front. The mv'd-in-place chmod below is now
# a no-op for the just_binary path but stays for the other modes (the
# overall `chmod 0755 "$TMP_RUNNER" "$TMP_SERVER"` is cheap and idempotent).
chmod 0755 "$TMP_RUNNER" "$TMP_SERVER"

if [ "$MODE" = "just_binary" ]; then
    current_runner_version=""
    current_server_version=""
    if [ -x "$RUNNER_BIN" ]; then
        current_runner_version="$("$RUNNER_BIN" --version 2>/dev/null | head -n1 || true)"
    fi
    if [ -x "$SERVER_BIN" ]; then
        current_server_version="$("$SERVER_BIN" --version 2>/dev/null | head -n1 || true)"
    fi
    new_runner_version="$("$TMP_RUNNER" --version 2>/dev/null | head -n1 || true)"
    new_server_version="$("$TMP_SERVER" --version 2>/dev/null | head -n1 || true)"
    if [ -n "$current_runner_version" ] \
       && [ -n "$current_server_version" ] \
       && [ -n "$new_runner_version" ] \
       && [ -n "$new_server_version" ] \
       && [ "$current_runner_version" = "$new_runner_version" ] \
       && [ "$current_server_version" = "$new_server_version" ]; then
        # The em-dash (U+2014) is the canonical separator — matched
        # byte-for-byte by the install-runner Rust tests so the runbook's
        # grep keeps working. Singular `v0.5.X` per the acceptance
        # criterion: both binaries are at the same version on a
        # well-formed paired install, and the runner is the primary
        # subject of the banner.
        ok "already at v$(short_version "$current_runner_version") — nothing to do"
        # Tmpfiles are removed by the EXIT trap; the systemd unit is not
        # restarted.
        exit 0
    fi
fi

# ── Install the new binaries ────────────────────────────────────────────────
# Both must be in place before we clear the EXIT trap so an interrupt
# mid-install removes both tmpfiles rather than leaving one orphaned
# next to a half-installed pair. The chmod above already ensured 0755;
# repeat the call so the move-in-place path stays explicit for readers.
chmod 0755 "$TMP_RUNNER" "$TMP_SERVER"
mv "$TMP_RUNNER" "$RUNNER_BIN"
mv "$TMP_SERVER" "$SERVER_BIN"
trap - EXIT

# T3.2 probe both binaries with --version so the success banner can name
# them and confirm Start session readiness before the operator opens the
# UI. Cached here (rather than re-probed at banner time) so a corrupted
# download surfaces alongside the "installed" lines instead of after the
# runner-start log line. `|| true` suppresses POSIX `set -e` even though
# command-substitution assignments don't trigger it — belt-and-braces for
# the run-as-sh-stream invocation. Empty string == probe failed.
runner_version="$("$RUNNER_BIN" --version 2>/dev/null | head -n1 || true)"
server_version="$("$SERVER_BIN" --version 2>/dev/null | head -n1 || true)"
ok "installed $RUNNER_BIN${runner_version:+ ($runner_version)}"
ok "installed $SERVER_BIN${server_version:+ ($server_version)}"

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

# ── Sidecar env file for the systemd unit (T1.2) ───────────────────────────
# The user-mode unit references `%h/.branchwork-runner/env` via
# `EnvironmentFile=`; write a `KEY=VALUE` file with the same saas_url +
# token the runner needs. Refreshed on every install run (enroll / update /
# reset / rotate-token) so a pre-T1.2 host with config.toml but no env
# file naturally migrates on its next install. just_binary skips this —
# the unit's existing env file is byte-for-byte preserved alongside the
# stored config.toml.
if [ "$MODE" != "just_binary" ]; then
    mkdir -p "$CONFIG_DIR"
    chmod 0700 "$CONFIG_DIR"
    old_umask="$(umask)"
    umask 077
    cat > "$CONFIG_DIR/env" <<ENV
BRANCHWORK_SAAS_URL=$SAAS_URL
BRANCHWORK_RUNNER_TOKEN=$TOKEN
ENV
    umask "$old_umask"
    ok "wrote $CONFIG_DIR/env"
fi

# ── Start (or restart) the runner ───────────────────────────────────────────
# Lifecycle is owned by systemd for ALL non-just_binary modes (T1.2):
#
#  • enroll / update / reset — render the unit into
#    `~/.config/systemd/user/branchwork-runner.service` (user mode) or
#    `/etc/systemd/system/branchwork-runner.service` (--system), then
#    `daemon-reload` and `enable --now`. User mode also runs
#    `loginctl enable-linger "$USER"` so the runner survives logouts and
#    reboots without an active login session.
#
#  • just_binary — the unit is already installed; a `systemctl restart`
#    swaps in the new binary image. No PID_FILE, no LOG_FILE writes —
#    systemd's journal owns that surface.
#
# A legacy nohup-launched runner from a pre-T1.2 install is detected via
# `$PID_FILE` and SIGTERMed before the systemd unit comes up; the file is
# then removed so subsequent runs don't try to kill a stale PID.

ensure_systemctl() {
    if ! command -v systemctl >/dev/null 2>&1; then
        err "branchwork-runner needs systemd (no systemctl on PATH)"
        note "for non-systemd hosts, see $SAAS_URL/docs/runner-as-a-service"
        exit 1
    fi
}

# Migrate hosts that ran a pre-T1.2 nohup launch (PID_FILE present): stop
# the legacy process so systemd's new unit doesn't compete with it. After
# this runs, PID_FILE is gone and we own the runner via systemd.
stop_legacy_nohup_runner() {
    [ -f "$PID_FILE" ] || return 0
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [ -z "$pid" ]; then
        rm -f "$PID_FILE"
        return 0
    fi
    case "$pid" in
        ''|*[!0-9]*)
            rm -f "$PID_FILE"
            return 0
            ;;
    esac
    if kill -0 "$pid" 2>/dev/null; then
        note "migrating from legacy nohup runner (pid $pid → systemd)"
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
            kill -KILL "$pid" 2>/dev/null || true
            sleep 1
        fi
    fi
    rm -f "$PID_FILE"
}

# Render the user-mode unit verbatim from deploy/branchwork-runner.service.in.
# The body is substituted in by the server at GET-time (see
# render_install_script in server-rs/src/saas/install_runner.rs — the
# placeholder one line below is replaced with the .in file's content).
# `%h` stays literal because the heredoc is quoted — systemd expands `%h`
# at unit-load time, so this template never needs the install script to
# know the home directory at render time.
write_user_unit() {
    user_unit_dir="$HOME/.config/systemd/user"
    user_unit_file="$user_unit_dir/branchwork-runner.service"
    mkdir -p "$user_unit_dir"
    cat > "$user_unit_file" <<'UNIT'
__UNIT_TEMPLATE__
UNIT
    ok "wrote $user_unit_file"
}

# Render the system-mode unit. Differs from user mode in three ways:
#   (a) Path: /etc/systemd/system/ (needs root, checked above).
#   (b) Explicit `User=` + `Group=` so %h expands to that user's home
#       instead of /root.
#   (c) `WantedBy=multi-user.target` so it auto-starts at boot.
# `WorkingDirectory=` is set to the runner user's home so the runner's
# default `--cwd .` resolves to a sensible place (system services
# default to /, which would break agent spawning).
write_system_unit() {
    system_unit_file="/etc/systemd/system/branchwork-runner.service"
    runner_user="${SUDO_USER:-${USER:-root}}"
    runner_home="$(getent passwd "$runner_user" 2>/dev/null | cut -d: -f6)"
    if [ -z "$runner_home" ]; then
        runner_home="$HOME"
    fi
    cat > "$system_unit_file" <<UNIT
# Branchwork SaaS runner — system-mode systemd unit (installed by install-runner.sh).
# Runs as $runner_user; state under $runner_home/.branchwork-runner/.

[Unit]
Description=Branchwork SaaS runner (system mode)
Documentation=https://github.com/branchwork/branchwork
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$runner_user
Group=$runner_user
WorkingDirectory=$runner_home
# --server-bin is explicit so the runner does not depend on systemd
# putting the paired-install dir on PATH.
ExecStart=$runner_home/.local/bin/branchwork-runner --server-bin $runner_home/.local/bin/branchwork-server
Restart=on-failure
RestartSec=5
Environment=BRANCHWORK_RUNNER_CONFIG=$runner_home/.branchwork-runner/config.toml
EnvironmentFile=$runner_home/.branchwork-runner/env

[Install]
WantedBy=multi-user.target
UNIT
    ok "wrote $system_unit_file"
}

if [ "$MODE" = "just_binary" ]; then
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        note "restarting systemd unit (sudo systemctl restart branchwork-runner)"
        if ! sudo systemctl restart branchwork-runner; then
            err "sudo systemctl restart branchwork-runner failed (see above)"
            exit 1
        fi
    else
        note "restarting systemd unit (systemctl --user restart branchwork-runner)"
        if ! systemctl --user restart branchwork-runner; then
            err "systemctl --user restart branchwork-runner failed (see above)"
            exit 1
        fi
    fi
    ok "restarted branchwork-runner via systemd"
else
    ensure_systemctl
    stop_legacy_nohup_runner
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        write_system_unit
        note "reloading systemd (system mode)"
        if ! systemctl daemon-reload; then
            err "systemctl daemon-reload failed (see above)"
            exit 1
        fi
        note "enabling and starting branchwork-runner"
        if ! systemctl enable --now branchwork-runner; then
            err "systemctl enable --now branchwork-runner failed (see above)"
            note "tail the journal for the failure reason: journalctl -u branchwork-runner -n 50"
            exit 1
        fi
    else
        write_user_unit
        note "reloading systemd (--user)"
        if ! systemctl --user daemon-reload; then
            err "systemctl --user daemon-reload failed (see above)"
            note "ensure a user-systemd session is active (e.g. linger may be off and you have no active session)"
            exit 1
        fi
        note "enabling and starting branchwork-runner"
        if ! systemctl --user enable --now branchwork-runner; then
            err "systemctl --user enable --now branchwork-runner failed (see above)"
            note "tail the journal for the failure reason: journalctl --user -u branchwork-runner -n 50"
            exit 1
        fi
        # `loginctl enable-linger "$USER"` makes the user instance start at
        # boot and survive logouts — without it the runner only runs while a
        # login session is active. Best-effort: some environments disallow
        # linger (locked-down corporate hosts, certain container runtimes);
        # downgrade to a warning rather than failing the whole install.
        if command -v loginctl >/dev/null 2>&1; then
            note "enabling user-linger so the runner survives logouts/reboots"
            if ! loginctl enable-linger "$USER" 2>/dev/null; then
                note "loginctl enable-linger failed — the runner will stop on logout"
                note "this is usually a privilege-restriction (try: sudo loginctl enable-linger $USER)"
            fi
        else
            note "loginctl not found — the runner will stop on logout"
        fi
    fi
    ok "branchwork-runner.service enabled and running"
fi

# ── Confirm Start session readiness (T3.2) ──────────────────────────────────
# The runner shells out to `branchwork-server session …` for every per-agent
# supervisor (see T0.4 spawn-path audit in docs/architecture/session-daemon.md).
# Without a working paired server binary on PATH, Start session fails at
# first spawn — the very thing this plan exists to prevent. Surface the
# resolved path so the operator can confirm the dual-binary model is wired
# before opening the dashboard UI; if `branchwork-server --version` failed
# to produce a version line (binary missing, wrong arch, corrupted
# download), surface the verbatim fallback so the failure mode is named
# at install time rather than discovered at Start time.
if [ -n "$server_version" ]; then
    ok "Start session will use: $SERVER_BIN ($server_version)"
else
    ok "Start session will use: $SERVER_BIN (could not verify — Start session will fail)"
fi

# ── Mode-aware completion line ──────────────────────────────────────────────
# The acceptance criterion for Task 2.1 is that a second install run reports
# "updated runner in place (runner_id preserved)" verbatim — that string is
# what the prod dashboard's deploy runbook greps for.
case "$MODE" in
    enroll)      ok "enrolled runner with the dashboard" ;;
    update)      ok "updated runner in place (runner_id preserved)" ;;
    reset)       ok "re-enrolled runner (runner_id reset)" ;;
    just_binary) ok "upgraded binaries in place (config untouched)" ;;
esac

if [ "$MODE" = "just_binary" ]; then
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        unit_status_cmd="sudo systemctl status branchwork-runner"
        unit_journal_cmd="sudo journalctl -u branchwork-runner -f"
    else
        unit_status_cmd="systemctl --user status branchwork-runner"
        unit_journal_cmd="journalctl --user -u branchwork-runner -f"
    fi
    cat <<NEXT

  Next steps:
    • Check status:  $unit_status_cmd
    • Tail log:      $unit_journal_cmd

  Binary upgrade only — the existing systemd unit and
  $CONFIG_FILE are untouched.
NEXT
else
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        unit_status_cmd="systemctl status branchwork-runner"
        unit_journal_cmd="journalctl -u branchwork-runner -f"
        unit_stop_cmd="systemctl stop branchwork-runner"
    else
        unit_status_cmd="systemctl --user status branchwork-runner"
        unit_journal_cmd="journalctl --user -u branchwork-runner -f"
        unit_stop_cmd="systemctl --user stop branchwork-runner"
    fi
    cat <<NEXT

  Next steps:
    • Check status:  $unit_status_cmd
    • Tail log:      $unit_journal_cmd
    • Stop runner:   $unit_stop_cmd
    • Dashboard:     $SAAS_URL/runners

  systemd owns the runner's lifecycle now — it restarts on crash and
  survives logouts/reboots (user mode uses \`loginctl enable-linger\`).
NEXT
fi
