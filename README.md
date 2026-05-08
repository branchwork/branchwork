# Branchwork

**Your Claude Code sessions, on any screen.** Run Branchwork on your workstation, open the dashboard from your laptop, your phone, a hotel TV — anywhere your browser can reach the host — and you're in a live terminal with a real Claude Code agent working on your codebase.

![Demo](screenshots/demo.gif)

Plans live as YAML in `~/.claude/plans/`. Every task has a Start button. Click it and a Claude agent spins up on a dedicated git branch, you watch it work, type to it, and when it's done you review the diff and merge — all from the browser.

It is a **project-management layer for AI agents**. Like Linear/Jira, except assignees are AI agents (Claude Code today, Aider/Codex/Gemini as drivers), status updates come from the code and git, and "complete a task" means: spawn an agent on a branch, watch it, review the diff, merge. Ships as a single ~15 MB Rust binary. No Node, no Docker, no daemon to install separately.

## Screenshots

### Plan board — collapsible phases, live status
![Plan Board](screenshots/02-plan-board.png)

### Agent terminal — full Claude Code session in the browser
![Agent terminal](screenshots/03-agent-terminal.png)

### New plan — describe, pick a folder, an agent creates the plan
![New Plan](screenshots/05-new-plan.png)

### Sidebar — projects, driver auth status, effort level
![Sidebar](screenshots/01-sidebar.png)

## Install

Pick one. All three land the same `branchwork-server` binary on your `PATH`. See [docs/quickstart.md](docs/quickstart.md) for the full five-minute walkthrough.

```sh
# Shell installer (Linux / macOS)
curl -fsSL https://raw.githubusercontent.com/branchwork/branchwork/master/install.sh | sh

# Build from source (Rust 1.85+, Node.js 20+, pnpm)
pnpm --filter @branchwork/web build && cd server-rs && cargo build --release

# Run
branchwork-server     # binds http://localhost:3100
```

Open <http://localhost:3100> in any browser on your network.

## Documentation

The full documentation set lives under [`docs/`](docs/README.md).

- [**Quickstart**](docs/quickstart.md) — five-minute self-hosted path: install, run, first plan, session-persistence proof.
- [**User guide**](docs/user-guide.md) — every dashboard surface: plans, tasks, agents, drivers, git flow, cost tracking, CI, auto-mode, notifications.
- [**Architecture overview**](docs/architecture/overview.md) — the three binaries (`branchwork-server`, `session_daemon`, `branchwork-runner`), wire protocols, persistence model.
- [**Operations**](docs/README.md#operations) — [self-hosted](docs/operations/self-hosted.md), [SaaS runner](docs/operations/saas-runner.md), [Docker](docs/operations/docker.md), [Helm + Terraform](docs/operations/helm-terraform.md), [upgrades and migrations](docs/operations/upgrades-and-migrations.md).
- [**Reference**](docs/README.md#reference) — [CLI flags](docs/reference/cli.md), [configuration](docs/reference/configuration.md), [plan schema](docs/reference/plan-schema.md), [drivers](docs/reference/drivers.md).
- [**Troubleshooting**](docs/troubleshooting.md) and [**glossary**](docs/glossary.md).
- [**Bob Shell integration**](docs/bob-shell-integration.md) — connect Bob Shell to Branchwork's MCP server.

## Project structure

```
Branchwork/
  server-rs/      Rust server (Axum, rusqlite, portable-pty, interprocess)
  web/            React frontend (Vite, Tailwind, xterm.js, Zustand)
  deploy/         Dockerfile, compose overlays, Helm chart, Terraform module
  docs/           Documentation (start at docs/README.md)
  screenshots/    Dashboard screenshots + demo recording
```

## License

MIT
