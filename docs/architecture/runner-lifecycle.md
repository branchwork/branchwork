# Architecture: runner lifecycle

How `branchwork-runner` is supervised on a customer host: which
systemd unit owns it, where its logs go, how upgrades land without
losing the WebSocket, why `loginctl enable-linger` matters on
headless boxes, and the explicit decision to keep project clones in
`$HOME` rather than a dedicated workspace dir.

This page is the daemon-lifecycle reference for the runner. The
in-process architecture (reconnect loop, outbox/ACK, agent spawning,
session-daemon reuse) lives in
[`runner.md`](runner.md). The hands-on enrollment, token rotation,
and platform-specific init-system recipes live in
[`../operations/saas-runner.md`](../operations/saas-runner.md). Read
this page when you want to understand **why** the default install is
the shape it is.

## TL;DR

- The default install (`install-runner.sh` enroll mode) renders a
  **systemd `--user` unit** at
  `~/.config/systemd/user/branchwork-runner.service` and activates it
  with `systemctl --user enable --now branchwork-runner`. The unit
  template ships at
  [`deploy/branchwork-runner.service.in`](../../deploy/branchwork-runner.service.in).
- Logs land in the systemd journal. Tail with
  `journalctl --user -u branchwork-runner -f`.
- Upgrades are in-place: `install-runner.sh --just-binary` swaps the
  binaries and runs `systemctl --user restart branchwork-runner`.
  Session daemons survive the restart because they are detached;
  the runner reattaches to live sockets via
  `cleanup_and_reattach_runner` on reconnect.
- `loginctl enable-linger $USER` is what makes the user instance
  start at boot and survive logout on headless boxes. Without it the
  runner only runs while a login session is active.
- **Project clones stay in `$HOME`.** No dedicated workspace
  directory, no chroot, no per-org isolation. The runner runs as the
  operator's own user and inherits the operator's normal env.

## The unit

The template, verbatim from
[`deploy/branchwork-runner.service.in`](../../deploy/branchwork-runner.service.in):

```ini
[Unit]
Description=Branchwork SaaS runner (user mode)
Documentation=https://github.com/branchwork/branchwork
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/branchwork-runner --server-bin %h/.local/bin/branchwork-server
Restart=on-failure
RestartSec=5
Environment=BRANCHWORK_RUNNER_CONFIG=%h/.branchwork-runner/config.toml
EnvironmentFile=%h/.branchwork-runner/env

[Install]
WantedBy=default.target
```

Three pieces are load-bearing:

- **`%h` is the systemd specifier for the user's home directory** —
  expanded at unit-load time, not at install time. The template
  needs no substitution from the install script; one rendered unit
  works on every operator's host regardless of `$HOME`.
- **`ExecStart` passes `--server-bin` explicitly.** Systemd's
  user-instance `PATH` is not guaranteed to include `~/.local/bin`
  (that depends on `systemctl --user import-environment` and
  `~/.config/environment.d/` state, which install-runner.sh does not
  manipulate). The runner's `which("branchwork-server")` fallback
  would otherwise miss the paired binary that
  [`runner-install-and-spawn-reliability`](https://github.com/branchwork/branchwork/blob/master/docs/architecture/runner.md)
  drops next to the runner under the same `~/.local/bin/`.
- **Secrets travel via `EnvironmentFile=`**, not inline
  `Environment=` lines. `~/.branchwork-runner/env` is a `0600`
  `KEY=VALUE` file written by the install script. The runner picks
  up `BRANCHWORK_SAAS_URL` and `BRANCHWORK_RUNNER_TOKEN` via clap's
  `env=` fallback (see `branchwork_runner::Cli`).
  `BRANCHWORK_RUNNER_CONFIG` is a forward-looking contract — a
  future task will teach the runner to consume `config.toml`
  directly, at which point this env var becomes the load path.

For the system-mode variant (root, `/etc/systemd/system/…`,
`User=` + `Group=`, `WantedBy=multi-user.target`) see
[`saas-runner.md § Linux — systemd unit`](../operations/saas-runner.md#linux--systemd-unit).
The default install does not use system mode; see [Why user mode,
not system mode](#why-user-mode-not-system-mode) below.

## Logs go through the journal

The runner writes to stdout/stderr. Systemd captures both into the
user journal:

```sh
# Follow live:
journalctl --user -u branchwork-runner -f

# Last hour:
journalctl --user -u branchwork-runner --since '1 hour ago'

# Filter by boot, useful for "did it come up after reboot?":
journalctl --user -u branchwork-runner -b
```

The first line of every connect cycle is:

```
[runner] id=runner-xxxxxxxx cwd=/home/<user> db=/home/<user>/.branchwork-runner/runner.db
```

Until you see that line, the binary has not reached its outer
reconnect loop. The most common cause of *not* seeing it is the
`EnvironmentFile=` step failing — check
`cat ~/.branchwork-runner/env` and confirm `BRANCHWORK_SAAS_URL` and
`BRANCHWORK_RUNNER_TOKEN` are both set.

Subsequent lifecycle lines worth knowing:

- `[runner] connecting to wss://…` — outer reconnect-loop tick.
- `[runner] resumed; last_seen_seq=…` — server-side outbox caught up.
- `[runner] shutdown requested by SaaS:` — operator clicked **Drain
  runner** in the dashboard; the runner finishes in-flight ACKs and
  exits, systemd's `Restart=on-failure` then does *not* fire because
  the exit code is zero.

System-mode units (when `install-runner.sh --system` was used) log
to the *system* journal — drop the `--user` flag and pass `-u
branchwork-runner` to `journalctl`.

## Upgrading in place

The canonical upgrade path is:

```sh
curl -fsSL https://branchwork.dev/install-runner.sh \
  | sh -s -- --just-binary
```

`--just-binary` (alias: `--upgrade`) is the binary-swap engine. It:

- Skips the token + config rewrite (existing
  `~/.branchwork-runner/config.toml` stays byte-for-byte identical).
- Downloads BOTH `branchwork-runner` AND `branchwork-server` to
  `~/.local/bin/`. Both must succeed before either is moved into
  place — a partial network failure cannot leave the runner pointing
  at a missing server binary.
- Compares the on-disk and to-install versions via `--version`. If
  both binaries already match the install source, the script exits 0
  with `* already at v0.5.X — nothing to do` and does **not**
  restart the unit. This makes `--just-binary` cheap to invoke from
  the dashboard's one-click **Upgrade** button and the runner's
  periodic version-check poll.
- Otherwise runs `systemctl --user restart branchwork-runner`
  (default) or `sudo systemctl restart branchwork-runner` (with
  `--system`).

The restart drops the WebSocket. What happens next is the load-bearing
property of the daemon model:

1. Session daemons keep running. They are detached children of the
   previous runner process — `setsid` on Unix and
   `DETACHED_PROCESS` on Windows — so the parent's death does not
   kill them. PTYs stay alive, agents keep working, output queues
   into the on-disk log under
   `<cwd>/.branchwork-runner-sessions/<agent-id>.log`.
2. Systemd respawns the runner within `RestartSec=5` (immediate on
   `restart`).
3. On reconnect the runner's `cleanup_and_reattach_runner` walks
   `<cwd>/.branchwork-runner-sessions/` and rebinds to every live
   socket. The dashboard sees the same agents reappear.
4. The outbox flushes anything that was unACKed before the restart.

For manual swaps (no `install-runner.sh` round-trip — e.g. testing a
local build), the equivalent sequence is:

```sh
cp ./branchwork-runner ~/.local/bin/
cp ./branchwork-server ~/.local/bin/
systemctl --user restart branchwork-runner
journalctl --user -u branchwork-runner -f   # watch for [runner] id=…
```

Rollback is symmetric: swap in the older binaries, restart. The
session daemons spawned by the old binary survive the rollback the
same way they survive the forward roll, because they are detached
and do not link against the runner.

## `loginctl enable-linger` and reboots

User-mode systemd has one quirk that bites every operator on first
contact: by default, the user instance **only runs while a login
session is active**. Log out — including SSH disconnect on a
headless box — and the user instance shuts down. Reboot — and the
user instance does not come back up until the operator logs in
again.

For a developer laptop that's logged in graphically, this is fine.
For a headless build box that the operator SSHes into once, kicks
off the install, then walks away from, this is fatal: the next
reboot kills the runner permanently.

`loginctl enable-linger $USER` flips the user instance's lifecycle:

- The user-systemd instance starts at boot, before any login, and
  keeps running across logouts.
- The runner unit (via `WantedBy=default.target` +
  `enable --now`) comes up with it.

`install-runner.sh` runs this for you, best-effort, in the enroll
path:

```sh
if command -v loginctl >/dev/null 2>&1; then
    loginctl enable-linger "$USER" 2>/dev/null \
        || note "loginctl enable-linger failed — the runner will stop on logout"
fi
```

The "best-effort" qualifier matters in three environments:

- **Locked-down corporate hosts** sometimes deny `enable-linger`
  to unprivileged users. The install warns and continues.
  Workaround: `sudo loginctl enable-linger $USER`, or switch to
  system mode with `install-runner.sh --system` (which does not
  need linger at all because the system instance always runs).
- **Containers without a real init.** Some minimal container
  runtimes ship without `loginctl`. The install detects this and
  warns. If you actually need user-mode systemd inside a container
  the host has to mount `/run/user/$UID`.
- **CI runners that intentionally clean up after every job.** Linger
  would defeat that. Use the bare `branchwork-runner …` invocation
  documented in
  [`saas-runner.md`](../operations/saas-runner.md) instead.

To verify linger is on:

```sh
loginctl show-user "$USER" --property=Linger
# Linger=yes
```

To reverse it (rarely needed; uninstall does *not* touch this state
because the operator may have opted into linger for other reasons):

```sh
sudo loginctl disable-linger "$USER"
```

System-mode units skip linger entirely. Use system mode when the
host is fundamentally a service host (no interactive operator), and
user mode when the host has an operator who owns its filesystem.

## Why projects stay in `$HOME`

The original plan for this layer bundled three orthogonal ideas:
daemon-ness, a dedicated workspace directory under
`~/.branchwork-runner/workspace/`, and Branchwork-managed git
credentials. The 2026-05-18 audit (recorded in the
`runner-daemon-workspace` plan context) split them apart and
produced the following verdict:

> - **Keep (1)** — clear win. systemd `--user` unit + `loginctl
>   enable-linger` survives reboot, no operator-visible cost.
> - **Drop (2)** — net loss in this deployment model. Branchwork is
>   one-runner-per-user-per-host; there is no other tenant to
>   isolate against on a single-developer laptop. The "dedicated
>   workspace" pattern is borrowed from multi-user server design
>   where it actually matters.
> - **Keep (3)** — the real security gap. Solve it directly: at the
>   moment a git op needs a credential, the runner writes the
>   branchwork-managed secret to a tmpfs file and points
>   `GIT_SSH_COMMAND` / `GIT_ASKPASS` at it, then unlinks the file
>   after the op.

The concrete costs the audit identified for (2) — i.e. the reasons
projects stay in `$HOME`:

| Cost                                                              | Why it matters                                                                                                |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `code ~/<project>` muscle memory breaks                           | Every operator launches their editor by path. A relocated workspace breaks every one of those keystrokes.     |
| Existing `cwd` paths in `plan_project` rows would need migration  | Branchwork stores absolute project paths in SQLite. A workspace move means rewriting every row.               |
| IDE workspace configs reset                                       | VS Code / JetBrains store per-folder settings keyed on the absolute path. Relocation invalidates all of them. |
| `~/.gitconfig` / `~/.ssh/known_hosts` / `~/.config/gh/` stop just-working | Git, SSH, and `gh` all resolve config relative to `$HOME`. A workspace under `~/.branchwork-runner/` would need explicit `HOME=` overrides or shadow copies. |

The benefit the relocation would have delivered — preventing the
runner from accidentally reading or modifying files outside a
sanctioned subtree — is **not zero**, but on a single-developer
laptop it is solving a problem that does not exist: the operator
already has unrestricted access to their own `$HOME`, and the
runner runs as the same uid. The credential-leak case the move was
really aimed at is solved more directly by the per-credential
hand-off in keep-(3), without paying any of the costs above.

What this means concretely for the lifecycle:

- The systemd unit sets no `WorkingDirectory=` for user mode (it
  inherits the user's home as cwd). The system-mode variant does
  set `WorkingDirectory=$runner_home` for the same reason — both
  resolve to the user's `$HOME`.
- The runner's `--cwd` defaults to `.` which, under systemd, lands
  at `$HOME` — the same place the operator already keeps project
  clones.
- Per-agent session sockets, pidfiles, and logs land under
  `<cwd>/.branchwork-runner-sessions/`. With the default `--cwd`,
  that resolves to `$HOME/.branchwork-runner-sessions/` — a sibling
  of the existing `~/.branchwork-runner/` config dir, not a child.

The per-credential hand-off that keep-(3) called out is itself
implemented in this plan (Phase 3) and documented in the next
section. Multi-tenant isolation between orgs on a single runner
remains out of scope and is tracked separately.

## Credentials: managed-by-name vs. ambient

Phase 3 of this plan landed the per-credential hand-off the
2026-05-18 audit asked for. The shape of it — and the explicit
**ambient-fallback policy** it leaves in place — is load-bearing
for anyone modelling their threat surface against this runner.

### Branchwork-managed credentials are used by name

Branchwork stores SSH keys and personal access tokens in an
encrypted SQLite column (`credentials.encrypted_secret`, AES-256-GCM
with a per-host master key at `~/.claude/.master.key` mode `0600` —
implementation in
[`server-rs/src/crypto.rs`](../../server-rs/src/crypto.rs)). The dashboard's
**Credentials** page (`/credentials`) is the operator's surface for
adding, generating, and revoking them. Each credential carries a
human name, a `kind` (`ssh_key` / `gh_pat` / `gitlab_pat`), and a
host hint (e.g. `github.com`).

An agent **never** reaches into `~/.ssh/` or `~/.config/gh/` to
satisfy a credentialed git op. It asks the server for the op **by
name**:

```text
clone_project(repo=git@github.com:org/repo.git, credential=my-gh-deploy-key)
push_branch(branch=…, credential=my-gh-deploy-key)
```

(The on-the-wire shape today is the
[`CloneProject { …, credential_id }`](protocols.md#project-clone-round-trip)
envelope; fetch / push RPCs follow the same pattern as later phases
add them.)

What happens at the runner under the hood, per
[`credential_material.rs`](../../server-rs/src/credential_material.rs):

1. Server resolves `credential_id` against the encrypted
   `credentials` table (`db::get_credential` + `crypto::decrypt`).
2. The decrypted secret travels on the wire **inside the RPC
   envelope** — only for this op, never persisted on the runner
   host.
3. The runner writes the secret to a fresh tmpfs file (under
   `$XDG_RUNTIME_DIR` if available, `/tmp` otherwise) at mode `0600`
   and points the git child at it via `Command::env`:
   - **`ssh_key`** →
     `GIT_SSH_COMMAND='ssh -i <path> -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new'`.
     `IdentitiesOnly=yes` is the load-bearing bit: it stops `ssh`
     from offering keys from `ssh-agent` ahead of ours, so the
     **named credential is the only thing that authenticates the
     op** — not whatever happens to be loaded into the operator's
     agent.
   - **`gh_pat` / `gitlab_pat`** → `GIT_ASKPASS=<wrapper>` +
     `GIT_TERMINAL_PROMPT=0`. The wrapper is a 4-line shell script
     that `cat`s the token from a sibling 0600 file (the token
     never appears in argv, even briefly during fork+exec).
4. RAII `Drop` unlinks the file(s) on every exit path — clone
   succeeds, git exits non-zero, the runner crashes mid-op, the
   destructor runs and the secret is gone from disk.

Two properties this gives you:

- **The parent process env is untouched.** Mutations land on
  `Command::env`, scoped to the spawned child. The operator's
  `ssh-add -l` listing before and after a credentialed op is
  byte-identical. The runner does not call `std::env::set_var`
  anywhere on this path.
- **The credential is not pinned to a project.** The same
  `my-gh-deploy-key` can serve multiple clones, pushes, fetches.
  Branchwork only knows it by name; the encrypted blob never leaves
  SQLite except inside an in-flight RPC envelope.

### Ambient creds (`~/.ssh/`, `~/.config/gh/`) stay reachable

`branchwork-runner` runs as the operator's own user. It inherits
the operator's `$HOME`, `$PATH`, `$SSH_AUTH_SOCK`, and every config
file under them. Nothing in this plan changes that:

| What                              | Where                                  | Who reads it                       |
| --------------------------------- | -------------------------------------- | ---------------------------------- |
| Operator's interactive SSH keys   | `~/.ssh/id_*`, `~/.ssh/config`         | Any direct shell `git` / `ssh`     |
| `ssh-agent` loaded identities     | `$SSH_AUTH_SOCK`                       | Same as above                      |
| Operator's `gh` auth              | `~/.config/gh/hosts.yml`               | Any direct `gh` shell-out          |
| Operator's git identity / signing | `~/.gitconfig`, `~/.config/git/`       | Every `git` invocation             |
| Known-hosts entries               | `~/.ssh/known_hosts`                   | Every `ssh` invocation             |

This is **intentional**. It is what makes the operator's
day-to-day workflow keep working while Branchwork is running on
the host:

- `cd ~/<project> && git push` from a terminal still works without
  threading a Branchwork credential through.
- `gh pr create` from a terminal still works.
- `ssh github.com` from a terminal still works.
- IDE-integrated git (VS Code's source-control panel, JetBrains'
  Git tool window) still works — it never knew about Branchwork in
  the first place.

The split-brain to internalise: **credentialed RPCs** (the
named-by-the-server hand-off) are the *isolated* surface;
**ambient creds** are everything else. Both coexist on the same
runner host by design — neither displaces the other.

### The trade-off — and what the operator can do about it

There is a sharp edge on this split. An **interactive agent**
(currently `claude` driving a PTY session) that shells out
`git push` directly — instead of asking the server for a
`push_branch(credential=…)` RPC — will use the ambient
`~/.ssh/` / `~/.config/gh/` creds, because that is what the
operator's own shell would use, and the agent is running in the
operator's own shell environment.

In practice that means today's auto-merge path (`merge_agent_branch_inner`
→ `git push origin <branch>`) and the Fix-CI shell-outs both ride
ambient creds, not the Branchwork-managed credential pinned to the
project. The named-credential RPC is what `clone_project` and the
future authenticated push/fetch RPCs use — interactive agents that
escape into `git push` from the PTY do not.

If that matters for your threat model — e.g. you want to be sure
the agent **cannot** push as you from a different machine, or
cannot read a colleague's deploy key that happens to be loaded
in your agent — **the operator must lock down their own
`~/.ssh/`** (and `~/.config/gh/`). Concrete options, none of which
Branchwork implements for you:

- Use a dedicated OS user for the runner with its own minimal
  `~/.ssh/` (and re-issue org-scoped keys to it). This is the
  cleanest separation but it does undo the
  [`code ~/<project>` muscle-memory benefit](#why-projects-stay-in-home)
  for that user.
- Keep your personal keys passphrase-protected and only `ssh-add`
  them when you need them; the agent then has nothing useful in
  `$SSH_AUTH_SOCK`.
- Run the runner under `systemd-run --user --scope` with a
  pruned `Environment=` so it does not inherit `$SSH_AUTH_SOCK`
  at all. (You will lose `ssh-agent`-backed pushes from inside the
  runner cwd, but credentialed RPCs still work because they bring
  their own key.)

What is **in scope here** is the credentialed-RPC surface. That
path is end-to-end isolated: named credential → encrypted column →
in-flight envelope → tmpfs 0600 → `Command::env` → unlink on drop.

What is **out of scope here** is preventing an interactive shell
op from reaching ambient creds. The runner runs as the operator;
the operator's shell env is the operator's shell env. Locking that
down is the operator's call, not Branchwork's.

## Failure modes (lifecycle-only)

The full failure-mode matrix for the runner — network, server, and
agent — lives at
[`runner.md § Failure modes`](runner.md#failure-modes). The two
modes specific to the daemon lifecycle:

- **Runner unit fails to start at boot.** Most likely cause: the
  `EnvironmentFile=` at `~/.branchwork-runner/env` is missing or
  unreadable (e.g. permissions drifted after a manual edit), so the
  runner has no `BRANCHWORK_SAAS_URL` / `BRANCHWORK_RUNNER_TOKEN` and
  exits non-zero. Systemd then loops on `Restart=on-failure` and the
  journal fills with the same clap error every 5 seconds. Check
  `systemctl --user status branchwork-runner` first — the failed
  command's stderr is in the status block.
- **Reboot survives, but linger is off.** Symptom: runner online
  while you are SSHed in, offline a few minutes after you log out.
  `loginctl show-user "$USER" --property=Linger` will report
  `Linger=no`. Fix: `sudo loginctl enable-linger "$USER"`.

## See also

- [`runner.md`](runner.md) — in-process architecture: reconnect
  loop, outbox/ACK, agent spawning, session-daemon reuse, the full
  failure-mode matrix.
- [`session-daemon.md`](session-daemon.md) — why session daemons
  survive a runner restart (detach via `setsid` /
  `DETACHED_PROCESS`).
- [`../operations/saas-runner.md`](../operations/saas-runner.md) —
  operator-facing recipes: token issuance, install-script flags,
  init-system units (Linux/macOS/Windows), troubleshooting.
- [`../ops/hetzner.md`](../ops/hetzner.md) — production runbook for
  the SaaS deploy that *serves* `install-runner.sh` (the
  branchwork.dev side, not the customer side).
- [`server-rs/src/credential_material.rs`](../../server-rs/src/credential_material.rs)
  — the tmpfs-backed, RAII-cleaned credential hand-off that powers
  the credentialed-RPC surface described in
  [Credentials: managed-by-name vs. ambient](#credentials-managed-by-name-vs-ambient).
- [`server-rs/src/api/credentials.rs`](../../server-rs/src/api/credentials.rs)
  — the dashboard-facing REST endpoints (`GET / POST / DELETE
  /api/credentials`) that own the named credentials in the
  encrypted `credentials` table.
- [`deploy/branchwork-runner.service.in`](../../deploy/branchwork-runner.service.in)
  — the user-mode unit template, kept under deploy/ next to the
  install script that renders it.
- [`deploy/install-runner.sh`](../../deploy/install-runner.sh) —
  the install / update / `--just-binary` engine. The systemd
  install logic lives under the `write_user_unit` / `write_system_unit`
  / linger-best-effort blocks near the end of the script.
- [`deploy/uninstall-runner.sh`](../../deploy/uninstall-runner.sh)
  — the clean reversal. Notably does **not** touch linger state or
  operator project directories.
