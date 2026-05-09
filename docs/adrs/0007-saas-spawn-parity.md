# ADR 0007 — SaaS spawn parity with standalone

- **Status:** Proposed (2026-05-09; flipped to Accepted on Phase 5 merge)
- **Authors:** cpo
- **Decision driver(s):** SaaS-routed agents silently hang at the TUI input box with the prompt pasted but never submitted; the runner-side `claude` invocation is missing five of the seven flags the standalone path passes; resume/restart, MCP-tool access, and PTY rendering are all broken on SaaS as a downstream consequence

## Context

Branchwork has two spawn paths for the same `claude` CLI: one
that runs in-process on the standalone server
([`server-rs/src/agents/pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs)),
and one that runs on a remote runner
([`server-rs/src/bin/branchwork_runner.rs::spawn_agent`](../../server-rs/src/bin/branchwork_runner.rs)).
Both end up calling `branchwork-server session … -- claude …`,
but the argv they compose has drifted apart. The drift is silent
because the dashboard doesn't surface argv divergence, MCP-tool
failures, or "claude is alive but the prompt was never submitted"
as anything other than the generic "Agent is working…" overlay.

### Live evidence

Agent `cbd73b92-b7d8-4c3b-81a1-8ca8c5586201` on a SaaS-routed
runner (2026-05-09): the dashboard showed "Agent is working…"
indefinitely while the actual `claude` process sat at its TUI
input box with the bracketed-paste of the task prompt clearly
visible but never submitted. Manually framing one byte (`\r`,
postcard-encoded as `Input(b"\r")`) onto the runner's session
socket immediately unstuck the agent — within seconds the model
was processing the pasted prompt. No flag, env var, or restart
was needed; just one byte.

The same prompt, handed to a standalone-spawned agent on the
same plan/task, submits and starts work on first contact.

### Side-by-side spawn-command comparison

Captured live from `ps -ef` on the host (placeholder paths
substituted for clarity). Copy-pasteable.

**Standalone agent** (e.g. `steady-prancing-squid/0.3` on
localhost):

```
branchwork-server session
  --socket .../sessions/<agent>.sock
  --cwd /home/cpo/cep
  --cols 120 --rows 40
  --env CLAUDE_CODE_SANDBOXED=1
  -- claude
      --session-id <uuid>
      --add-dir /home/cpo/cep
      --verbose
      --effort max
      --dangerously-skip-permissions
      --mcp-config .../sessions/<agent>.mcp.json
      --settings .../sessions/<session>.settings.json
```

**SaaS agent** (`cbd73b92`, runner-spawned):

```
branchwork-server session
  --socket .../runner-sessions/<agent>.sock
  --cwd /home/cpo/aso2
  --env CLAUDE_CODE_SANDBOXED=1
  -- claude
      --effort max
      --dangerously-skip-permissions
```

Diff: the SaaS path is missing `--session-id`, `--add-dir`,
`--verbose`, `--mcp-config`, `--settings`, and the explicit
`--cols`/`--rows`. (The PTY still opens at 120×40 because
[`SessionArgs`](../../server-rs/src/agents/supervisor.rs)
defaults `cols`/`rows` to those values via clap
`default_value_t`, but that's coincidence, not parity — Phase 4
will start passing them explicitly to fix the rendering bug, and
SaaS needs to follow.)

### Where the standalone flags come from

Traced from
[`pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs)
(line numbers as of `branchwork/dashboard-stability/5.1`):

| Flag                                    | Source                                                                                                                                                         |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--session-id <uuid>`                   | `session_id`, generated per spawn (UUID v4) and persisted in `agents.session_id`. Lets `claude` resume the conversation across PTY restart.                    |
| `--add-dir <cwd>`                       | The agent's working directory. Tells `claude` to whitelist that path for tool reads/writes without re-prompting.                                               |
| `--verbose`                             | Hardcoded in `ClaudeDriver::spawn_args` (driver.rs:286). Required for the cost-summary and verdict lines `parse_cost` / `parse_verdict` look for.              |
| `--effort <effort>`                     | Per-plan effort (`Effort::to_string()`) from `SpawnOpts.effort`.                                                                                               |
| `--dangerously-skip-permissions`        | `opts.skip_permissions`, resolved per-runner-then-server-default before spawn.                                                                                 |
| `--mcp-config <path>`                   | Driver writes a `.mcp.json` next to the session socket (pty_agent.rs:124-137); flag points at it. This is what gives `claude` access to the Branchwork tools. |
| `--settings <path>`                     | Driver writes a per-session `<session_id>.settings.json` (pty_agent.rs:142-156); flag points at it. Carries the Stop-hook config for unattended auto-mode.    |
| `--cols 120 --rows 40` (on the daemon) | Hardcoded in `pty_agent.rs:179-180` at the `supervisor::spawn_session_daemon` call site.                                                                        |

`--mcp-config` and `--settings` are passed through
`SpawnOpts.mcp_config_path` and `SpawnOpts.settings_path`,
materialised on disk *before* spawn. The runner never sees those
two files because nothing on the server tells it to write them.

### Prompt injection mechanism (standalone path)

**TUI keystrokes over the session socket, followed by a trailing
`\r`.** The standalone path does *not* use `--print`-mode and
does *not* use `--session-id`-driven resume to pre-seed the
conversation. The injector is
[`pty_agent.rs::inject_prompt_when_ready`](../../server-rs/src/agents/pty_agent.rs)
(lines 743-789):

1. Subscribe to the agent's PTY output broadcast.
2. Watch the rolling 8 KiB tail for Claude's readiness glyph
   (U+276F `❯`). On match, break out of the wait loop. If
   16 seconds elapse with no readiness glyph, break anyway and
   inject regardless.
3. Sleep 500 ms (lets the prompt line finish painting).
4. `command_tx.send(SessionMessage::Input(prompt.into_bytes()))`
   — pushes the prompt bytes onto the supervisor socket. The
   session daemon writes them to the PTY master, `claude` sees
   them as stdin keystrokes (which Claude Code's TUI handles as
   bracketed paste).
5. Sleep 1 second.
6. `command_tx.send(SessionMessage::Input(b"\r".to_vec()))` —
   sends a single `\r` byte. Claude Code's TUI input handler
   treats this as the Enter keystroke and submits the pasted
   prompt.

The mechanism is exactly the same shape as the SaaS path
([`branchwork_runner.rs::forward_agent_io`](../../server-rs/src/bin/branchwork_runner.rs):2201-2260)
— readiness-glyph watcher, then `Input(prompt_bytes)`. The SaaS
path is missing only the trailing `\r`. That single missing byte
is exactly what the live-evidence manual nudge supplied.

So the audit overturns the plan-brief's leading speculation
("likely `--print`-mode or the session-resume protocol that
`--session-id` enables"): standalone is the same TUI-paste shape
as SaaS, but with a one-byte tail. Bringing SaaS to parity does
not require changing the injection mechanism — it requires
finishing it.

## Decision

**Bring SaaS to standalone parity by composing the same flag set
on the runner side and finishing the prompt injection.** Two
deliverables, one each in Phase 5.2 and Phase 5.3:

### 5.2 — Pass the missing flags through the runner's `StartAgent` wire

Extend the `WireMessage::StartAgent` envelope (and the runner
binary's `spawn_agent` argument list) to carry the five fields
the SaaS path is missing today:

- `session_id: String` — already a server-side concept; the
  server already inserts it into `agents.session_id` for SaaS
  rows but never sends it to the runner.
- `add_dir: String` — usually equal to `cwd`, but kept as its
  own field so a future use-case can diverge.
- `verbose: bool` — defaulted true for `claude` driver; runner
  passes `--verbose` when set.
- `mcp_config_json: Option<String>` — the JSON body the server
  would have written next to the standalone socket. Runner
  writes it to `<sockets_dir>/<agent_id>.mcp.json` and passes
  `--mcp-config <path>`.
- `settings_json: Option<String>` — the per-session
  Stop-hook settings body. Runner writes it to
  `<sockets_dir>/<agent_id>.settings.json` and passes
  `--settings <path>`.
- `cols: u16, rows: u16` (Phase 4 + 5.2 compound) — runner
  passes them as `--cols`/`--rows` so the daemon stops
  defaulting silently to 120×40.

The runner-side argv composition in `spawn_agent` mirrors
`ClaudeDriver::spawn_args` field-for-field; the two should not
drift again. A unit test pinned in the runner binary asserts
argv-equality against a fixture for the claude case.

The two MCP/settings JSON bodies travel over the wire as strings
(not file paths) because the runner is on a different host and
cannot share a filesystem with the server. The runner is
responsible for writing them to disk, the server is responsible
for the contents. On agent exit, the runner deletes both files
the same way the standalone path does (best-effort, log on
failure).

### 5.3 — Finish prompt injection on SaaS

In `branchwork_runner.rs::forward_agent_io`, after the
`Input(prompt_bytes)` write that already exists at line 2229,
add the same two-step the standalone path uses:

1. `tokio::time::sleep(Duration::from_secs(1)).await;`
2. `session_protocol::write_frame(&mut stream, &Message::Input(b"\r".to_vec())).await?;`

The 1-second settle is preserved so the trailing `\r` does not
race with whatever rendering Claude does in response to the
paste. (Live evidence: an immediate `\r` racing the paste
sometimes lands inside the bracketed-paste sequence on a slow
runner; the 1-second gate makes that race-free.)

There is no driver-specific divergence here today — the only
driver wired up is `claude`. When a non-Claude driver lands in
SaaS, the trailing `\r` is the right default for any TUI
expecting an Enter keystroke; an explicit opt-out belongs on the
driver trait, not in `forward_agent_io`.

## Consequences

### SaaS resume / restart works again

Today, killing a SaaS agent's process and respawning loses the
conversation: the new `claude` invocation has no `--session-id`,
so it can't pick up where the previous session left off. With
5.2 wired, the same `session_id` already living in
`agents.session_id` flows through to the runner, and Claude
Code's session-resume picks up the prior turn. This also closes
the gap where `cleanup_and_reattach_runner` rediscovers a
running daemon but the dashboard had no way to confirm whether
the resumed `claude` was the same conversation or a fresh one.

### MCP-tool access is wired in SaaS

`mcp__branchwork__*` tools exist for SaaS agents in principle,
but today the SaaS-side `claude` is started with no
`--mcp-config`, so it has no `mcpServers` map and no way to find
the dashboard's `/mcp` endpoint. Every `mcp__branchwork__*`
tool call from a SaaS agent fails silently — silently because
the dashboard does not currently surface MCP-tool failures
anywhere. Wiring `--mcp-config` is enough; the streamable-HTTP
transport that `ClaudeDriver::mcp_config_json` already emits
(driver.rs:314-…) is host-agnostic, so the runner-host `claude`
can reach the SaaS server's `/mcp` endpoint over the public URL.

### PTY rendering improves (Phase 4 compound)

The PTY rendering bug from Phase 4 has SaaS as the harder case
because the SaaS path never explicitly negotiates `--cols`/`--rows`.
Once 5.2 starts threading explicit cols/rows through, Phase 4's
"defer first PTY output until viewport size is known" fix
applies on SaaS too, not just standalone.

### Backward compatibility / migration

There is no usable forward-compatible bridge: a runner built
before 5.2 receives the new fields and quietly ignores them
(serde unknown-field tolerance), so the runner would still spawn
without the flags. The deployment story is therefore "upgrade
the runner first, then the server." Existing in-flight SaaS
agents that lack `session_id` on the wire (because they were
spawned before this change rolled out) need to be killed; the
dashboard already has the orphan-on-server-restart kill path,
which is the right one to use here. The plan brief flagged this:
"likely 'kill them, the dashboard already shows the
orphan-on-server-restart path'." Adopted as-is.

### No new privilege surface

All five new flags carry data the server already had. Nothing in
this ADR exposes new credentials, paths, or tool permissions to
the runner that it did not already see by virtue of running
`claude` against the user's repository.

## Rejected alternatives

### Send `\r` after the paste as a hot-fix

Land just the trailing `\r` (Phase 5.3-only), leave the missing
flags for later. Rejected:

- Brittle. The `\r` and the paste already race today; if the
  runner is loaded enough that the 1-second settle is too short,
  the `\r` lands inside the bracketed-paste sequence and Claude
  Code interprets it as a literal newline inside the prompt,
  not as Enter. Standalone has the same 1-second settle and
  doesn't hit it because standalone is on the same host as the
  server and the supervisor socket is a Unix domain socket; SaaS
  is across a WS hop with variable latency.
- Doesn't fix MCP. Doesn't fix resume. Doesn't fix rendering.
  The audit's whole point is that the divergence is a bundle,
  not a single missing byte; pulling out one byte and shipping
  is the kind of partial fix that earns the third "user found a
  bug five minutes later" bullet from this plan's context.

### Make the SaaS path canonical and back-port to standalone

Reverse the convergence: declare "no flags except effort + skip"
the new minimum, drop `--session-id` / `--mcp-config` /
`--settings` from standalone, hot-fix the prompt injection both
sides. Rejected:

- Standalone works today. The user experience standalone gives
  is the experience we want SaaS to give. Erasing standalone's
  feature set to match SaaS regresses the working path to make
  the broken path "consistent."
- MCP-tool access is load-bearing for the unattended auto-mode
  loop (ADR 0003): the Stop-hook config relies on `--settings`,
  the readiness signal that gates auto-finish lives in the MCP
  surface. Dropping those from standalone would break Phase 6
  retroactively.
- The session-resume guarantee is a real product feature on
  standalone today. Dropping it because SaaS hasn't caught up
  is the tail wagging the dog.

### Run `claude` with `--print <prompt>` and skip TUI injection

Use Claude Code's `--print` mode (single-shot, no TUI) instead
of pasting into the TUI. This was the plan-brief's leading
speculation about how standalone might be working. Rejected on
audit:

- Standalone does *not* use `--print`. The audit traced the
  prompt to TUI keystroke injection in
  `inject_prompt_when_ready`; switching to `--print` would be
  a behavioural change on standalone, not a parity fix on SaaS.
- `--print` discards the interactive turns that ADR 0003's
  unattended auto-mode loop relies on (Stop-hook firing on
  conversation end, readiness glyph as the auto-finish trigger,
  cost summary parsed from the TUI tail).
- The convergence target is "make SaaS behave like standalone."
  That target is exactly the existing TUI-paste-plus-`\r`
  mechanism, not a new shape neither side uses today.

## References

- Plan: `dashboard-stability`, Phase 5 description (this ADR's
  Context section quotes the side-by-side from there verbatim).
- Live agent: `cbd73b92-b7d8-4c3b-81a1-8ca8c5586201` (2026-05-09).
- Standalone spawn entry point:
  [`server-rs/src/agents/pty_agent.rs::start_pty_agent`](../../server-rs/src/agents/pty_agent.rs).
- SaaS spawn entry point:
  [`server-rs/src/bin/branchwork_runner.rs::spawn_agent`](../../server-rs/src/bin/branchwork_runner.rs).
- Standalone driver argv:
  [`server-rs/src/agents/driver.rs::ClaudeDriver::spawn_args`](../../server-rs/src/agents/driver.rs).
- Standalone prompt injector:
  [`server-rs/src/agents/pty_agent.rs::inject_prompt_when_ready`](../../server-rs/src/agents/pty_agent.rs).
- SaaS prompt injector (missing the `\r`):
  [`server-rs/src/bin/branchwork_runner.rs::forward_agent_io`](../../server-rs/src/bin/branchwork_runner.rs).
- Session-daemon argv builder:
  [`server-rs/src/agents/supervisor.rs::build_session_daemon_args`](../../server-rs/src/agents/supervisor.rs).
- Compound rendering bug: ADR-adjacent Phase 4 work; see plan
  `dashboard-stability` Phase 4 description for the
  hardcoded-120×40 + DEC-2026 join discussion.
- Stop-hook + `--settings` provenance: ADR 0003 (unattended
  auto-mode) §1 ("Per-session settings file" and "Hook URL
  contract").
