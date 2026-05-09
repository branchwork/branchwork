# Terminal rendering: garbage scrollback / wrong-position output

When the dashboard's agent terminal panel shows characters at wrong
x-positions, frozen spinner frames stuck in scrollback, or a cascade
of duplicated horizontal-rule glyphs (`▀▀▀…`), the cause is almost
always a mismatch between the PTY's column count and what xterm.js
believes the geometry to be. This page captures the failure mode so
the next regression (PTY default-size change, xterm.js major bump,
Claude Code TUI rewrite) does not need to be debugged from scratch.

## Symptom

Live evidence pattern from agent
`8920a61b-d9ac-47a6-93dc-06cedefa72ac` on **2026-05-08**, recorded in
the daemon's `<socket>.log` transcript at
`~/.claude/sessions/8920a61b-d9ac-47a6-93dc-06cedefa72ac.log` (ESC
shown as `^[` per `cat -v` convention):

```text
^[[?2026h^[[H^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B
^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B
^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B
^[[2K^[[1B^[[2K^[[1B^[[2K^[[1B^[[H^[[?2026l
```

```text
^[[38;5;244m▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
^M^[[76C▀^M^[[77C^[[1A▀^M^[[78C^[[1A▀^M^[[79C^[[1A▀^M^[[80C^[[1A▀
^M^[[81C^[[1A▀^M^[[82C^[[1A▀  …  ^M^[[103C^[[1A▀  …  ^M^[[119C^[[1A▀^[[39m
```

Three telling sub-patterns:

- `^[[?2026h … ^[[?2026l` — DEC 2026 *Synchronized Output* begin /
  end. Claude Code's TUI batches its repaint inside this pair so a
  terminal that supports it can swap whole frames atomically. A
  client that does not implement DEC 2026 (or that joins
  mid-sequence) sees the raw inner stream — every cursor move
  surfaces visually as the TUI redraws.
- `^[[NC` — *Cursor Forward N* (CHA / CUF). The transcript above
  steps from `[76C` through `[103C` up to `[119C`: Claude Code
  expects a 120-column viewport and is filling the last column of
  each row of a horizontal rule. On a sub-80-column dashboard panel
  every `[80C…[119C` overshoots the right edge and wraps onto the
  line below, producing the cascading-`▀` corruption.
- `^[[2K^[[1B` repeated — *Erase Line* + *Cursor Down*. The TUI
  first wipes its viewport row by row, then redraws. If the row
  count it wipes (40, the daemon default) is taller than the
  panel, leftover rows scroll into the visible area and freeze
  there as un-overwritten history.

The 8920a61b log holds **10 961** `[?2026h` openings, **2 268**
`[80C` jumps, and **1 668** `[103C` jumps over a single ~10-minute
session — every one of them a paint that landed at the wrong
geometry.

## Root cause

Two layers compose:

1. **Hardcoded 120×40 PTY at spawn time.** The supervisor opens its
   PTY at the clap defaults `--cols 120 --rows 40`
   (`server-rs/src/agents/supervisor.rs:57-62`). The dashboard
   sends a `Resize` over the session socket once xterm.js has
   measured its own viewport, but that round-trip lands tens to
   hundreds of milliseconds *after* the PTY child is already
   running and writing.
2. **Mid-stream join + DEC 2026.** Claude Code emits its first
   frame (banner + status block + prompt) before xterm.js can fit
   and resize. Those bytes are wrapped in DEC 2026, sized for
   120×40, and broadcast to whoever is attached. A late subscriber
   (sidebar collapse, devtools dock toggle, panel reconnect) joins
   the broadcast partway through a `[?2026h … [?2026l` block; xterm
   .js applies the trailing half against whatever buffer state it
   has, and the partially-applied cursor positions stick as
   scrollback.

Both layers are necessary: the PTY size mismatch is the *what*, the
mid-stream DEC 2026 join is the *why it persists* (the corrupted
buffer is never cleared, just appended to).

## Fix layers

This plan landed two complementary fixes; either alone would leave
visible corruption.

- **T4.1 — server-side spawn-time synchronization.**
  `server-rs/src/agents/supervisor.rs:391-419` introduces an
  `INITIAL_RESIZE_GRACE = 500 ms` `sync_channel(1)` gate. The PTY
  reader thread blocks on `gate_rx.recv_timeout(...)` before its
  first `read()`; `handle_client` `try_send`s the gate **after**
  applying `master.resize(cols, rows)` from the dashboard's first
  `Resize` message. Result: Claude Code's first paint is sized for
  the dashboard viewport, not 120×40. Auto-mode / MCP / detached
  spawns never attach, so they fall through after 500 ms — the
  cosmetic geometry does not matter for them. Pinned by
  `pty_reader_held_for_grace_when_no_resize_arrives` and
  `resize_landing_during_grace_unblocks_pty_reader` in
  `server-rs/tests/pty_resize.rs`.

- **T4.2 — client-side reset + Ctrl+L on resize / reconnect.**
  `web/src/components/PtyTerminal.tsx:58-72,93-99` extracts a
  single `resync()` closure called from both `ws.onopen` and the
  `ResizeObserver` callback. Order is non-negotiable:
  1. `term.reset()` — drops xterm.js scrollback, alt-screen state,
     SGR state.
  2. `ws.send("\x0c")` — Ctrl+L (form feed) tells the Claude Code
     TUI to repaint the current frame.
  3. `ws.send({type:"resize", cols, rows})` — applies the new
     geometry server-side; the queued repaint then emerges at the
     right size.

  Reset before Ctrl+L so the repaint lands in a fresh buffer.
  Resize last so the repaint emerges at the new geometry. Trade-
  off: scrolled-up history is dropped on every resize. That is
  worse than it sounds, but corrupted scrollback is worse —
  a server-side terminal emulator (out of scope here) is the
  right long-term answer.

T4.1 prevents the wrong-geometry first paint. T4.2 catches the
mid-stream-join class (sidebar toggle, devtools dock, reconnect)
that T4.1's grace window cannot reach.

## Repro recipe

```text
# Force a small dashboard panel
# Open dashboard, collapse sidebar, dock devtools right.
# Start an agent; click into its terminal pane.
# Look for: characters at wrong x-positions, frozen
# spinner frames in scrollback, line-wrap chaos.
```

Toggling the sidebar back open (or undocking devtools) without
T4.2 leaves the corrupted scrollback on screen until the panel
unmounts — that is the diagnostic signal. With T4.2 the panel
clears and repaints cleanly on every geometry change.

## What to check first if it regresses

1. **Did the PTY spawn defaults change?** Grep
   `server-rs/src/agents/`:
   ```sh
   grep -rn "cols 120\|rows 40" server-rs/src/agents/
   ```
   The clap defaults at `supervisor.rs:57-62` (`--cols 120`,
   `--rows 40`) are the values the daemon opens its PTY at before
   the dashboard's first `Resize` lands. If a refactor lowered
   them (e.g. to 80×24) the visual symptom changes — the cursor-
   forward sequences shrink — but the class of bug does not.

2. **Was xterm.js bumped?** Check `web/package.json` for
   `@xterm/xterm`. The doc was written against `^6.0.0`; a major-
   version bump is the most likely place for DEC 2026
   (Synchronized Output) implementation behaviour to regress, and
   for the `term.reset()` semantics in `resync()` to drift
   (alt-screen handling, scrollback ownership). Re-run the repro
   recipe after any major bump.

3. **Is the grace window still 500 ms?** Look at
   `INITIAL_RESIZE_GRACE` in `server-rs/src/agents/supervisor.rs`
   and confirm the dashboard's first `Resize` arrives inside it.
   If xterm.js / `FitAddon` start measuring later (heavier React
   tree, slower bootstrap, async font load) the resize can land
   *after* the grace expires — the PTY reader unblocks at 120×40
   defaults and the symptom comes back. Either bump the grace or
   move the dashboard-side `fit()` earlier in the WS open path
   (`web/src/components/PtyTerminal.tsx:66-72`).

## See also

- [`server-rs/src/agents/supervisor.rs`](../../server-rs/src/agents/supervisor.rs)
  — the PTY spawn + grace gate.
- [`web/src/components/PtyTerminal.tsx`](../../web/src/components/PtyTerminal.tsx)
  — the xterm.js mount and `resync()` closure.
- [`server-rs/tests/pty_resize.rs`](../../server-rs/tests/pty_resize.rs)
  — server-side regression tests for T4.1.
- [`web/src/components/PtyTerminal.test.tsx`](../../web/src/components/PtyTerminal.test.tsx)
  — client-side regression tests for T4.2.
- [architecture/session-daemon.md](../architecture/session-daemon.md)
  — full PTY + transcript-replay model the renderer sits on top of.
- [troubleshooting.md § Session terminal shows blank after reconnect](../troubleshooting.md#session-terminal-shows-blank-after-reconnect)
  — the related "no output" failure mode (different cause: dropped
  broadcast frames vs. wrong-geometry paint).
