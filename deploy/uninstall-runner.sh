#!/bin/sh
# Branchwork runner uninstaller — clean reversal of an install-runner.sh
# enroll/update on this host.
#
# Usage:
#   ./uninstall-runner.sh              (interactive — prompts before config dir)
#   ./uninstall-runner.sh -y           (non-interactive)
#   ./uninstall-runner.sh --system     (uninstall the system-mode unit; needs root)
#   ./uninstall-runner.sh --keep-binaries
#   ./uninstall-runner.sh --keep-config
#
# What it touches (the things install-runner.sh creates):
#   1. systemd unit   `systemctl --user disable --now branchwork-runner`
#                     rm ~/.config/systemd/user/branchwork-runner.service
#                     `systemctl --user daemon-reload`
#                     (or system-mode equivalents with --system, which
#                      reads /etc/systemd/system/branchwork-runner.service
#                      and the bare `systemctl` instance)
#   2. binaries       rm ~/.local/bin/branchwork-runner
#                     rm ~/.local/bin/branchwork-server   (paired install,
#                                                          dropped by T3.1)
#   3. config dir     rm -rf ~/.branchwork-runner/        (prompts unless -y)
#
# What it explicitly DOES NOT TOUCH:
#   • Operator project directories under $HOME — they belong to the
#     operator and may carry uncommitted work that branchwork did not
#     create.
#   • ~/.claude/ — Claude Code's user dir, owned by the user.
#   • `loginctl enable-linger` state — branchwork-runner may not be the
#     only reason the user opted into linger; reversing it is the
#     operator's call.
#   • The systemd journal — `journalctl --user -u branchwork-runner` may
#     still show historical entries after uninstall; vacuuming the
#     journal is up to the operator.
#
# Idempotent: every step degrades to a `note` when the target is already
# absent, so re-running on a partially-clean host (or after a failed
# install) is safe.

set -eu

# Same path constants as install-runner.sh; keep in sync if either side moves.
INSTALL_DIR="${BRANCHWORK_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="$HOME/.branchwork-runner"
RUNNER_BIN="$INSTALL_DIR/branchwork-runner"
SERVER_BIN="$INSTALL_DIR/branchwork-server"

err()  { printf 'error: %s\n' "$*" >&2; }
ok()   { printf '* %s\n' "$*"; }
note() { printf '  %s\n' "$*"; }

YES=0
SYSTEM_INSTALL=0
KEEP_BINARIES=0
KEEP_CONFIG=0

usage() {
    cat <<USAGE
usage: $0 [-y|--yes] [--system] [--keep-binaries] [--keep-config] [-h|--help]

Reverses an install-runner.sh enroll/update on this host:
  • stops + disables the systemd unit (\`disable --now\`)
  • removes the unit file
  • removes ~/.local/bin/branchwork-{runner,server}
  • removes ~/.branchwork-runner/ (prompts unless -y is passed)

Operator project directories under \$HOME are NOT touched.

Flags:
  -y, --yes        non-interactive — remove the config dir without prompting
  --system         operate on /etc/systemd/system/branchwork-runner.service
                   (requires root; --user is the default)
  --keep-binaries  leave branchwork-runner + branchwork-server in place
  --keep-config    leave ~/.branchwork-runner/ in place (preserves runner_id
                   + outbox for a future re-enrollment)
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        -y|--yes) YES=1 ;;
        --system) SYSTEM_INSTALL=1 ;;
        --keep-binaries) KEEP_BINARIES=1 ;;
        --keep-config) KEEP_CONFIG=1 ;;
        -h|--help) usage; exit 0 ;;
        --*)
            err "unknown flag: $1"
            usage >&2
            exit 2
            ;;
        *)
            err "unexpected argument: $1"
            usage >&2
            exit 2
            ;;
    esac
    shift
done

# --system writes to /etc/systemd/system/ and reloads the system instance,
# both of which need root. --user runs unprivileged.
if [ "$SYSTEM_INSTALL" = "1" ] && [ "$(id -u)" != "0" ]; then
    err "--system requires root (use sudo or run as root)"
    note "the system unit lives at /etc/systemd/system/branchwork-runner.service"
    exit 1
fi

if [ "$SYSTEM_INSTALL" = "1" ]; then
    ok "uninstall mode: system (root)"
else
    ok "uninstall mode: user"
fi

# ── Stop + disable the systemd unit (best effort) ───────────────────────────
# `disable --now` does both "stop running" and "remove from boot target" in
# one shot. Best-effort: the operator may have already disabled it, or it
# may never have been installed (idempotent uninstall on a stub host).
if command -v systemctl >/dev/null 2>&1; then
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        note "systemctl disable --now branchwork-runner"
        systemctl disable --now branchwork-runner 2>/dev/null || true
    else
        note "systemctl --user disable --now branchwork-runner"
        systemctl --user disable --now branchwork-runner 2>/dev/null || true
    fi
    ok "stopped + disabled branchwork-runner.service (best-effort)"
else
    note "systemctl not on PATH; skipping unit disable"
fi

# ── Remove the unit file ────────────────────────────────────────────────────
if [ "$SYSTEM_INSTALL" = "1" ]; then
    unit_file="/etc/systemd/system/branchwork-runner.service"
else
    unit_file="$HOME/.config/systemd/user/branchwork-runner.service"
fi
if [ -e "$unit_file" ]; then
    rm -f "$unit_file"
    ok "removed $unit_file"
else
    note "no unit file at $unit_file"
fi

# `daemon-reload` so systemd forgets the unit definition. Without it,
# `systemctl status branchwork-runner` keeps reporting "loaded but
# inactive (dead)" for the rest of the login session.
if command -v systemctl >/dev/null 2>&1; then
    if [ "$SYSTEM_INSTALL" = "1" ]; then
        systemctl daemon-reload 2>/dev/null || true
    else
        systemctl --user daemon-reload 2>/dev/null || true
    fi
fi

# ── Remove the binaries ─────────────────────────────────────────────────────
# install-runner.sh drops BOTH `branchwork-runner` AND `branchwork-server`
# (T3.1 paired install — the runner shells out to `branchwork-server
# session …` for every per-agent supervisor). The uninstall reverses both
# so the acceptance criterion "no binary" holds; pass --keep-binaries to
# leave them in place (useful when the operator wants to re-enroll later
# without re-downloading the binaries).
if [ "$KEEP_BINARIES" = "1" ]; then
    note "--keep-binaries: leaving $RUNNER_BIN + $SERVER_BIN in place"
else
    for bin in "$RUNNER_BIN" "$SERVER_BIN"; do
        if [ -e "$bin" ]; then
            rm -f "$bin"
            ok "removed $bin"
        else
            note "no binary at $bin"
        fi
    done
fi

# ── Remove the config dir (with confirmation unless -y) ─────────────────────
# This is the destructive step: the config dir holds the token, runner.db
# (runner_id + outbox queue), the sidecar env file consumed by the systemd
# unit, and the legacy PID file. Removing it means a future install will
# re-enroll from scratch (new runner_id; the server treats this host as a
# brand-new runner). Pass --keep-config to preserve the dir for a future
# re-enrollment (the existing token + runner.db survive).
if [ "$KEEP_CONFIG" = "1" ]; then
    note "--keep-config: leaving $CONFIG_DIR in place (token + runner.db preserved)"
elif [ -d "$CONFIG_DIR" ]; then
    if [ "$YES" = "1" ]; then
        rm -rf "$CONFIG_DIR"
        ok "removed $CONFIG_DIR"
    else
        printf '\nRemove %s? This wipes the token, runner.db (runner_id + outbox), env file, and PID file.\n  [y/N] ' "$CONFIG_DIR"
        reply=""
        # Read from /dev/tty so the prompt works even when this script is
        # invoked through a pipe (stdin would otherwise be the pipe). The
        # `|| reply=""` guards against a missing tty (CI, fully-detached
        # runs): we fall through to "keep" rather than crashing on the
        # `read` failure under `set -e`.
        read -r reply </dev/tty 2>/dev/null || reply=""
        case "$reply" in
            y|Y|yes|YES)
                rm -rf "$CONFIG_DIR"
                ok "removed $CONFIG_DIR"
                ;;
            *)
                note "keeping $CONFIG_DIR (re-run with -y to remove non-interactively)"
                ;;
        esac
    fi
else
    note "no config dir at $CONFIG_DIR"
fi

ok "branchwork-runner uninstalled"
note "operator project directories under \$HOME were not touched"
