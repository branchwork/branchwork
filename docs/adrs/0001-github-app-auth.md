# ADR 0001 — GitHub App auth + drop `gh` from the runner

- **Status:** Proposed (2026-05-03)
- **Authors:** cpo
- **Decision driver(s):** customer setup friction, agent-sandbox blast radius, multi-host portability

## Context

Today branchwork interacts with GitHub through the runner shelling out to:

- `git push origin <branch>` (relies on whatever auth is configured on the runner host — typically `gh auth login` having seeded a credential helper, or a pre-existing SSH key on `~/.ssh/`)
- `gh run list --commit … --json …` (CI status poller)
- `gh run view <id> --log-failed` (Fix-CI flow's failure-tail fetch)
- `gh pr create …` (currently invoked from inside the agent's PTY, not from the runner)

This works but has four problems:

1. **Customer setup friction.** A new self-hosted user must `apt install gh && gh auth login` before branchwork can do anything useful. The auth flow is a tab-switch into a browser plus a device code. Documented but not loved.
2. **Agent blast radius.** The PTY agent has access to whatever credentials `gh auth login` left in the host's keychain, plus full `git push` capability via inherited credential helpers. A prompt-injected agent can push to any repo the host user can push to — not just the worktree it was started in.
3. **No multi-host story.** GitLab, Bitbucket, and Gitea support is a "rewrite every shell-out" project, not a "swap an adapter" project, because GitHub-shaped CLI assumptions are scattered across `agents/git_ops.rs`, `saas/runner_protocol.rs`, the runner-side handlers in `crates/runner/`, and the agent driver prompt templates.
4. **Fragmented attribution.** Commits authored from inside the PTY use the host user's `git config user.email`. PRs created via `gh pr create` are attributed to the host user. There is no single "branchwork did this" identity to grant fine-grained permissions to or to revoke.

Reaching feature parity (multi-host, attribution, hardened agent surface) by patching the existing shell-out paths means touching every existing site and adding new ones for every new host — so we want a single boundary instead.

## Decision

Replace the `gh`-based interaction model with a **GitHub-App-based auth model** owned by the runtime, behind a single `GitHubAdapter` abstraction. The agent loses GitHub awareness entirely.

### Trust boundaries

- **PTY agent:** has `git` only (for local commits, `git log`, `git diff`, `git status`). No `gh`. No push credentials. No GitHub API tokens. The agent's git remote is configured such that any push attempted from inside the PTY fails with a credential error — the only legitimate push path is the runner's `PushBranch` RPC.
- **Runner:** holds short-lived install tokens (≤1h TTL, GitHub-issued) just long enough to perform a single push or API call. Tokens are cached in process memory until `expires_at - 60s`. Never persisted to disk.
- **App private key (the long-lived secret):** lives in **one** place per deployment shape:
  - **SaaS:** server-rs holds the global App's private key in its secret store (env var or filesystem, deployment-specific). Server mints install tokens on demand and ships them to the runner over the existing authenticated WSS channel.
  - **Self-hosted:** runner reads the App private key from a local config file (`~/.config/branchwork/github-app.pem` by default), readable only by the runner's user. Mints tokens locally.

### One adapter, two providers

```
trait GitHubTokenProvider {
    async fn fetch_token(&self, installation_id: u64) -> Result<InstallToken>;
}

struct InstallToken { value: String, expires_at: DateTime<Utc> }

// Two impls of the trait:
//   SelfHostedTokenProvider — reads local .pem, signs JWT, exchanges for install token
//   SaasTokenProvider       — sends GetGithubInstallToken WS message, awaits response

// One client built on top:
struct GitHubAdapter<P: GitHubTokenProvider> { ... }
impl<P> GitHubAdapter<P> {
    async fn push_branch(&self, repo: RepoRef, branch: &str) -> Result<()>;
    async fn create_pr(&self, …) -> Result<PullRequest>;
    async fn list_runs_for_sha(&self, …) -> Result<Vec<GhRun>>;
    async fn fetch_failure_log_tail(&self, …) -> Result<Option<String>>;
    async fn create_repo(&self, …) -> Result<Repository>;
}
```

A future `GitLabTokenProvider` slots into the same trait. A `GitLabAdapter` reuses the same call surface. The wire protocol carries opaque short-lived tokens; it does not encode the auth scheme.

### App permission scopes

Minimum scopes, justified one-by-one:

| Scope | Why |
|---|---|
| `contents: write` | `git push` to default + topic branches |
| `pull_requests: write` | `CreatePullRequest` |
| `actions: read` | `GhRunList`, `GhFailureLog` |
| `metadata: read` | Required by GitHub for any App; auto-granted |

`administration: write` is **not** in the core App. Repo creation is a separate App ("Branchwork Repos") that customers install only if they want the fresh-project flow. This keeps the install screen for the common case ("Connect GitHub") far less alarming than asking for `administration` upfront.

### Push mechanics

The runner uses HTTPS push with the install token as the password:

```
https://x-access-token:<install_token>@github.com/<owner>/<repo>.git
```

A custom `git credential.helper` shim points at a small runner-local socket; git fetches a fresh token on each push, with the runner using its `GitHubTokenProvider` to mint or pull from cache. This is preferred over rewriting the worktree's `origin` URL because it (a) doesn't leave a token in `.git/config` if the runner crashes mid-push, (b) lets the helper enforce per-repo scope checks, and (c) is the documented GitHub pattern for App-driven git operations.

### Identity / attribution

- **Commits** continue to use the human's `git config user.email` from inside the PTY (the agent and the human are the source of truth for commit authorship). Branchwork does not rewrite commits.
- **Pushes** use the install token (the App's bot identity is the *pusher*, not the *committer*).
- **PRs created via `CreatePullRequest`** are authored by the App's bot identity ("branchwork[bot]"); the PR body cites the human's identity from the commits it includes.

This separates "who wrote it" (human + agent) from "who shipped it" (branchwork on their behalf), which matches how Dependabot, Renovate, and similar tools operate.

## Consequences

### Positive

- Agent surface shrinks. PTY no longer needs `gh`, no longer holds push creds. Prompt-injection blast radius is "the worktree it can read/write," not "every repo on this host."
- Single "Connect GitHub" install in SaaS, single `branchwork-server github setup` in self-hosted. No per-repo `gh auth login`.
- Multi-host (GitLab, Bitbucket) is now an adapter project, not a fork-the-codebase project.
- Token theft is bounded: ≤1h TTL, scoped to one installation. Past the TTL the token is dead.
- Attribution is clean: branchwork has a bot identity that customer audit logs can grep for.
- We can grant the App fine-grained per-repo scope at install time. A customer can install branchwork on five of their fifty repos.

### Negative

- **Operational complexity at runtime:** a new long-lived secret to manage (the App private key). Rotation needs a runbook (see below). For SaaS, this becomes a tier-1 operational concern.
- **Customer install step has a ceiling:** GitHub's App install UX cannot be deeply customized. Some customers (especially security-conscious ones) will balk at any App install. We mitigate by keeping scopes minimal and splitting Repos creation into a second optional App.
- **Initial implementation effort:** ~Phase 1–4 of the implementation plan, several weeks of engineering. Not a one-week patch.
- **One more deploy-time secret:** the App key has to be deployed to every server-rs instance (SaaS) or distributed to every self-hosted user. Not novel — runner_tokens already crosses the same boundary — but the App key has higher blast radius if compromised.

### Migration path

A `legacy-gh` auth-mode keeps the old shell-out path live behind an opt-in flag for one minor version after this lands, with a startup deprecation warning. Removed in the *next* minor version. Existing self-hosted users get one release cycle to register an App.

## Threat model

**Attacker steals one install token (e.g. memory dump, MITM the WSS):**

- Capability: push to and read from one customer's installed repos for ≤1h.
- Detectable: every push event is logged in the customer's GitHub audit log, attributed to the App.
- Mitigation: TTL caps blast radius. Customer can revoke the install entirely from GitHub's UI in seconds.

**Attacker steals the SaaS App private key:**

- Capability: mint install tokens for *all* SaaS installations until the key is rotated. Read/push to every customer's installed repos. Worst case in this scheme.
- Detectable: anomalous token mint rate; GitHub does not log JWT signing per se but does log every install token issuance, which the SaaS server can correlate against its own mint requests.
- Mitigation:
  1. Store the App key in a real secret store (env var injected at boot from a secret manager; never in source, never in disk-resident config).
  2. Rotate quarterly per the procedure below.
  3. Alarm on mint-rate anomaly.
  4. Customer-side: customers can uninstall the App if branchwork ever publicly discloses a compromise; the App becomes a no-op for them in seconds.

**Attacker compromises a self-hosted runner host:**

- Capability: equivalent to compromising the customer's dev box. Has the App key locally, has filesystem access to all worktrees, can act as the customer for the App's installation scope.
- Mitigation: out of scope for branchwork. The customer's host security is their responsibility. Branchwork stores the App key with mode `0600`, owned by the runner's user, but a host compromise breaks all assumptions.

**Attacker compromises a PTY agent (prompt injection):**

- Capability: read/write the worktree filesystem; create local commits; cannot push (no credentials in agent env); cannot make API calls (no `gh`, no token).
- Mitigation: this is the *primary* hardening this ADR delivers. The runner mediates every network action.

## Rotation procedure

GitHub Apps support up to two active private keys simultaneously. The procedure:

1. Generate a new private key in GitHub UI (App settings → "Generate a private key"). The new key downloads as `branchwork-app.<timestamp>.pem`.
2. Deploy the new key to all server-rs instances (SaaS) or distribute to all self-hosted users (release notes + signed download).
3. Update server-rs config to use the new key as the **active signer** for newly minted JWTs. Old install tokens minted under the previous key remain valid until their natural ≤1h expiry — no session disruption.
4. Wait one full TTL (1h + safety margin → 2h is safe) for any in-flight tokens to expire.
5. Revoke the old key in GitHub UI.

Quarterly rotation is the default. An ad-hoc rotation runs the same procedure on demand, with no minimum cadence between rotations.

## Rejected alternatives

### Personal Access Token (PAT) only

Simpler: one fine-grained PAT, drop into config, done. We rejected this as the *destination* but accept it as a v0 stepping stone. Reasons:

- PATs are tied to a human GitHub account. If the account is suspended (vacation, departure, disabled by GitHub), branchwork breaks.
- Fine-grained PATs cap at one year TTL and require manual rotation. No revocation other than "delete the PAT."
- No clean attribution: pushes appear to be from the human, not from branchwork.
- Customer-side: customer must create a PAT in their account with the right scopes; this has worse UX than a one-click App install for SaaS.

A PAT-shaped `GitHubTokenProvider` impl is fine to ship as v0 (~30 lines) to unblock the agent-side hardening; the App impl supersedes it without breaking the wire protocol.

### Keep `gh`, harden the agent surface differently

We considered keeping `gh` but firewalling the PTY agent from it (e.g., chroot, restricted PATH, AppArmor profile). Rejected because:

- The runner still depends on `gh` being installed and authed on the host, so customer setup friction is unchanged.
- Multi-host is unsolved.
- Harder to test: every CI invocation needs a mocked `gh` binary on PATH; the App + adapter pattern is a normal HTTP client, easy to wiremock.
- Doesn't separate "who shipped" from "who wrote." Still uses the host user's identity for pushes.

### OIDC from the customer's CI

GitHub supports OIDC for ephemeral creds in some flows, but only from inside GitHub Actions runners. Not applicable to a long-running runner on a customer's workstation.

### Customer-registered Apps in SaaS

Each SaaS customer registers their own App and gives branchwork the App ID + key. Rejected because:

- The App creation flow in GitHub UI is multi-step and intimidating for non-engineers.
- We'd still need a global App for branchwork's own marketing pages, demo accounts, etc.
- Per-tenant App key management 10× the operational complexity.
- Reconsider only if a customer has a hard compliance requirement (e.g. "no third-party Apps installed on our org") — then we offer it as an opt-in tier.

## Implementation pointer

The implementation plan that follows from this ADR is in `docs/plans/github-app-auth.md` (created when this ADR is accepted). Phase 0 of that plan is "write this ADR" — the loop is closed when this file is committed at status `Accepted`.

## References

- GitHub Apps documentation: <https://docs.github.com/en/apps/creating-github-apps>
- Authenticating as an installation: <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation>
- Prior art on credential helpers: <https://git-scm.com/docs/gitcredentials>
- Branchwork architecture overview: `docs/architecture/overview.md`
- Branchwork runner protocol (current `gh`/`git` shell-outs): `docs/architecture/runner.md`, `docs/architecture/protocols.md`
