//! GitHub host API client (Phase 2.3 of `runner-daemon-workspace`).
//!
//! Today this module exposes one helper — [`create_repo`] — for the
//! "create new remote repo" flow on `POST /api/projects { mode: "create",
//! host: "github" }`. It dispatches to either:
//!
//! - `POST /user/repos` when `owner` is `None` (creates the repo under
//!   the PAT owner's account).
//! - `POST /orgs/{owner}/repos` when `owner` is set (creates under an
//!   organisation).
//!
//! Required PAT scopes: `repo` for private repos, or `public_repo` for
//! public repos. The credential record (`credentials.scopes`) carries
//! the host-reported scope list so the UI can filter create-capable
//! credentials before this dispatch fires; this module does NOT
//! re-validate scopes — it lets GitHub reject the request and surfaces
//! the upstream error verbatim via [`CreateRepoOutcome::Failed`].
//!
//! ## Test hook
//!
//! [`api_base`] honors `BRANCHWORK_GITHUB_API_BASE` when set so
//! integration tests can point this module at a local mock without
//! touching real GitHub. Production deployments leave the env var unset
//! and use the canonical `https://api.github.com` base.
//!
//! ## Wire shape — request
//!
//! ```json
//! {"name": "my-new-repo", "private": true, "auto_init": false}
//! ```
//!
//! ## Wire shape — success response (subset)
//!
//! ```json
//! {
//!   "clone_url": "https://github.com/owner/my-new-repo.git",
//!   "html_url": "https://github.com/owner/my-new-repo",
//!   "ssh_url":  "git@github.com:owner/my-new-repo.git"
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Canonical GitHub API base URL. Override via
/// `BRANCHWORK_GITHUB_API_BASE` (test hook only — production leaves it
/// unset).
const GITHUB_API_BASE_DEFAULT: &str = "https://api.github.com";

/// Resolve the GitHub API base URL — env override or hard-coded
/// production default.
pub fn api_base() -> String {
    std::env::var("BRANCHWORK_GITHUB_API_BASE")
        .unwrap_or_else(|_| GITHUB_API_BASE_DEFAULT.to_string())
}

/// Arguments to [`create_repo`].
///
/// `owner` is `None` for "create under the PAT owner's account" or
/// `Some(org)` for "create under an organisation". `pat` is the
/// decrypted credential secret (treat as ephemeral — caller has it
/// only momentarily during dispatch and never persists it).
pub struct CreateRepoRequest<'a> {
    pub name: &'a str,
    pub private: bool,
    pub owner: Option<&'a str>,
    pub pat: &'a str,
}

/// Outcome of [`create_repo`]. Mirrors the standard three-arm shape the
/// rest of the codebase uses (e.g. `CloneDispatchOutcome`): success +
/// host-rejected + transport. Callers can match exhaustively without
/// guessing which error came from where.
#[derive(Debug)]
pub enum CreateRepoOutcome {
    /// Repository was created on GitHub. URLs are taken verbatim from
    /// the response — `clone_url` is the HTTPS form (the canonical
    /// default for `git clone`), `ssh_url` is the `git@github.com:...`
    /// form. The HTTP API caller writes `clone_url` to
    /// `projects.repo_url`.
    Created {
        clone_url: String,
        html_url: String,
        ssh_url: String,
    },
    /// GitHub rejected the request (4xx or 5xx). `http_status` is the
    /// raw status code; `message` is the upstream `message` field when
    /// the body is JSON, falling back to the canonical reason phrase.
    /// `body_excerpt` is the first 512 bytes of the response body —
    /// surfaced as `host_response_excerpt` in the structured error the
    /// HTTP caller returns to the dashboard.
    Failed {
        http_status: u16,
        message: String,
        body_excerpt: String,
    },
    /// Transport failure (DNS, TLS, connection refused, etc.) — no HTTP
    /// status to report. `error` is `format!("{e}")` of the underlying
    /// reqwest error.
    Transport { error: String },
}

#[derive(Serialize)]
struct CreateRepoBody<'a> {
    name: &'a str,
    private: bool,
    /// We intentionally never auto-init — Branchwork's clone-then-init
    /// flow expects to land an empty remote so the local agent's first
    /// commit lands cleanly.
    auto_init: bool,
}

#[derive(Deserialize)]
struct GhRepoResponse {
    clone_url: String,
    html_url: String,
    ssh_url: String,
}

/// Dispatch `POST /user/repos` or `POST /orgs/{owner}/repos` against
/// the GitHub API.
///
/// On host-API rejection the dashboard surfaces the upstream `message`
/// and `body_excerpt` so the operator can act on a scope or naming
/// conflict directly (e.g. `Resource already exists` → 422). This
/// helper does NOT pre-validate scopes; the credential row's
/// `scopes` column is the UI filter and GitHub is the source of truth.
pub async fn create_repo(req: CreateRepoRequest<'_>) -> CreateRepoOutcome {
    let owner = req.owner.map(str::trim).filter(|s| !s.is_empty());
    let url = match owner {
        Some(o) => format!("{}/orgs/{}/repos", api_base(), o),
        None => format!("{}/user/repos", api_base()),
    };
    let body = CreateRepoBody {
        name: req.name,
        private: req.private,
        auto_init: false,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", req.pat))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "branchwork")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return CreateRepoOutcome::Transport {
                error: format!("{e}"),
            };
        }
    };

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let excerpt: String = body_text.chars().take(512).collect();
        // Extract `.message` when the body is JSON; fall back to the
        // canonical reason phrase otherwise.
        let message = serde_json::from_str::<serde_json::Value>(&body_text)
            .ok()
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("github_error")
                    .to_string()
            });
        return CreateRepoOutcome::Failed {
            http_status: status.as_u16(),
            message,
            body_excerpt: excerpt,
        };
    }

    match serde_json::from_str::<GhRepoResponse>(&body_text) {
        Ok(r) => CreateRepoOutcome::Created {
            clone_url: r.clone_url,
            html_url: r.html_url,
            ssh_url: r.ssh_url,
        },
        Err(e) => CreateRepoOutcome::Failed {
            http_status: status.as_u16(),
            message: format!("malformed GitHub response: {e}"),
            body_excerpt: body_text.chars().take(512).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_defaults_to_github_when_env_unset() {
        // Capture the current value so concurrent tests don't bleed
        // into us; restore at the end.
        let prior = std::env::var("BRANCHWORK_GITHUB_API_BASE").ok();
        // SAFETY: tests run in the same process; we restore the env
        // var on every exit path. The actual gh.rs path also lives
        // behind the env var so concurrent tests within this same
        // file are still safe.
        unsafe {
            std::env::remove_var("BRANCHWORK_GITHUB_API_BASE");
        }
        let base = api_base();
        if let Some(prev) = prior {
            unsafe { std::env::set_var("BRANCHWORK_GITHUB_API_BASE", prev) }
        }
        assert_eq!(base, GITHUB_API_BASE_DEFAULT);
    }
}
