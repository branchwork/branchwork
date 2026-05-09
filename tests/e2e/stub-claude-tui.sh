#!/bin/sh
# TUI stub for the SaaS terminal-renders e2e. Stands in for `claude`
# inside the runner container; emits a deterministic byte stream that
# exercises the xterm.js rendering path (Task 4.1 + 4.2 regression
# pin) without needing the real Anthropic CLI.
#
# Behaviour:
#   - Paint once immediately (clear screen, cursor home, sentinel +
#     newline).
#   - Repaint every 500 ms forever. Why a poll rather than handling
#     Ctrl+L from stdin: parsing single bytes off stdin requires
#     bash's `read -n1` (busybox sh doesn't have it) and the alpine
#     runtime image carries no bash. The poll is good enough for the
#     spec — after a resize/sidebar toggle the dashboard's `resync()`
#     fires `term.reset()`, and within 500 ms the next paint lands in
#     the fresh xterm buffer.
#   - Drain stdin in the background so the prompt the supervisor
#     injects after `\r` doesn't fill the kernel pipe and stall
#     anything upstream.
#   - Sleep loop after the painter so we never exit (early exit
#     would flip the agent to `finished` and tear down before the
#     spec can assert).

SENTINEL='BRANCHWORK-TUI-READY'

paint() {
    # \033[2J = clear screen, \033[H = cursor home.
    printf '\033[2J\033[H%s\r\n' "$SENTINEL"
}

# Drain stdin so injected prompt bytes don't accumulate.
( cat >/dev/null ) &

paint
while :; do
    sleep 0.5
    paint
done
