# Local development

Conventions for working **on** Branchwork (editing the server, runner,
or dashboard inside this repo) rather than **with** it. Page is also
the home for repo-local policies that don't fit cleanly under
[operations/self-hosted.md](self-hosted.md) (single-host deployment)
or [quickstart.md](../quickstart.md) (five-minute install).

For the contract every test and tool here has to honour — agents run
on the same host as the production `branchwork-server` instance
supervising them, so unscoped process-pattern kills will take down the
supervisor — see
[CLAUDE.md](../../CLAUDE.md) and
[adrs/0005-e2e-tests-must-be-containerized.md](../adrs/0005-e2e-tests-must-be-containerized.md).

## `.mcp.json`: tracked, frozen

The repo ships [`.mcp.json`](../../.mcp.json) at its root. It registers
the dashboard's MCP server with any Claude Code session opened in
this working copy:

```json
{
  "mcpServers": {
    "branchwork": {
      "type": "http",
      "url": "http://localhost:3100/mcp"
    }
  }
}
```

That URL is the standalone server's default listen address
(`branchwork-server --port` defaults to `3100`, see
[reference/cli.md](../reference/cli.md)). It is identical for every
contributor; nothing here is per-developer state.

### Decision

The file is **tracked and treated as frozen**. Contributors clone the
repo and Claude Code immediately knows how to reach the local MCP
server — no first-run script, no template-copy step, no make target.

Considered but rejected: `git rm --cached .mcp.json` plus a
`.mcp.json.example` template and a `make .mcp.json` target. That
trades a one-line file no one ever needs to edit for an extra step
during onboarding. Not worth it.

### Rules of engagement

- **Do not commit edits to `.mcp.json` from your working copy.** It
  has shipped exactly once (the initial commit) and has no business
  drifting. If you see it in `git status` after a typical session,
  treat that as a tool misbehaving and revert it
  (`git checkout -- .mcp.json`).
- **No code path in Branchwork itself rewrites this file.** The
  per-agent `<agent-id>.mcp.json` files under
  `~/.claude/sessions/` are separate — they are owned by
  `start_pty_agent` (see
  [reference/drivers.md](../reference/drivers.md) and
  [architecture/session-daemon.md](../architecture/session-daemon.md))
  and have no relationship to the root `.mcp.json`.
- **Running on a non-default port?** Don't edit the tracked
  `.mcp.json`. Either pass `--mcp-config` to your Claude Code
  invocation pointing at a private file, or override at the user
  level in `~/.claude.json`. Both are documented in
  [bob-shell-integration.md](../bob-shell-integration.md) — Bob and
  Claude Code both honour these layered configs.

### Acceptance signal

Run server + runner + dashboard for an hour of normal work and then:

```sh
git diff .mcp.json
```

Empty output. If it ever isn't, the offending tool needs to be
identified and either fixed or scoped away from this file — not
worked around by un-tracking. The
[dirty-tree-check](https://github.com/branchwork/branchwork) plan
that introduced this rule was driven by exactly that class of
nuisance dirtying the auto-mode loop.
