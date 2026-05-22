//! GitLab host API client (Phase 2.3 of `runner-daemon-workspace`).
//!
//! Today this module exposes one helper — [`create_repo`] — for the
//! "create new remote repo" flow on `POST /api/projects { mode: "create",
//! host: "gitlab" }`. It dispatches to `POST /projects`, sending
//! `{name, visibility, ...}` and (optionally) `namespace_id` resolved
//! from the operator-supplied `owner` field.
//!
//! Required PAT scopes: `api` or `write_repository`. The credential
//! record (`credentials.scopes`) carries the host-reported scope list
//! so the UI can filter create-capable credentials before this
//! dispatch fires; this module does NOT re-validate scopes — it lets
//! GitLab reject the request and surfaces the upstream error verbatim
//! via [`CreateRepoOutcome::Failed`].
//!
//! ## Test hook
//!
//! [`api_base`] honors `BRANCHWORK_GITLAB_API_BASE` when set so
//! integration tests can point this module at a local mock without
//! touching `gitlab.com`. Production deployments leave the env var
//! unset and use the canonical `https://gitlab.com/api/v4` base.
//!
//! ## Owner / namespace resolution
//!
//! GitLab projects live under a *namespace* — either the authenticated
//! user's default namespace, or a group's. When `owner` is provided we
//! issue a `GET /namespaces?search=<owner>` lookup and pick the first
//! exact match (case-sensitive on `path`). When `owner` is `None` we
//! omit `namespace_id` and GitLab defaults to the PAT owner's user
//! namespace.

use serde::{Deserialize, Serialize};

/// Canonical GitLab API base URL. Override via
/// `BRANCHWORK_GITLAB_API_BASE` (test hook only — production leaves it
/// unset).
const GITLAB_API_BASE_DEFAULT: &str = "https://gitlab.com/api/v4";

/// Resolve the GitLab API base URL — env override or hard-coded
/// production default.
pub fn api_base() -> String {
    std::env::var("BRANCHWORK_GITLAB_API_BASE")
        .unwrap_or_else(|_| GITLAB_API_BASE_DEFAULT.to_string())
}

/// Arguments to [`create_repo`].
///
/// `owner` is `None` for "create under the PAT owner's user namespace"
/// or `Some(path)` for "create under a group whose `path` matches".
/// `pat` is the decrypted credential secret.
pub struct CreateRepoRequest<'a> {
    pub name: &'a str,
    pub private: bool,
    pub owner: Option<&'a str>,
    pub pat: &'a str,
}

/// Outcome of [`create_repo`]. Mirrors the GitHub helper's three-arm
/// shape: success + host-rejected + transport.
#[derive(Debug)]
pub enum CreateRepoOutcome {
    /// Project was created on GitLab. URLs come straight from the
    /// response — `clone_url` is the HTTPS form (`http_url_to_repo`),
    /// `ssh_url` is `ssh_url_to_repo`, `html_url` is `web_url`.
    Created {
        clone_url: String,
        html_url: String,
        ssh_url: String,
    },
    /// GitLab rejected the request. `http_status` is the raw status;
    /// `message` is the upstream `message` field (or `error` for some
    /// error shapes) when the body is JSON; `body_excerpt` is the
    /// first 512 bytes of the response body for the structured error
    /// the dashboard renders.
    Failed {
        http_status: u16,
        message: String,
        body_excerpt: String,
    },
    /// Transport failure (DNS, TLS, connection refused, etc.).
    Transport { error: String },
}

#[derive(Serialize)]
struct CreateProjectBody<'a> {
    name: &'a str,
    visibility: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace_id: Option<u64>,
}

#[derive(Deserialize)]
struct GlProjectResponse {
    http_url_to_repo: String,
    ssh_url_to_repo: String,
    web_url: String,
}

#[derive(Deserialize)]
struct GlNamespace {
    id: u64,
    path: String,
}

/// Look up a namespace's id by `path`. Returns `None` when no exact
/// match is found, or on error. Errors are intentionally swallowed —
/// the caller falls through to "create under the user's namespace"
/// rather than 5xx-ing the whole request on a transient lookup
/// failure. (The downstream `POST /projects` will then surface a real
/// error if the user actually wanted a group.)
async fn resolve_namespace_id(owner: &str, pat: &str) -> Option<u64> {
    let url = format!(
        "{}/namespaces?search={}",
        api_base(),
        urlencoding_path_segment(owner)
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("PRIVATE-TOKEN", pat)
        .header("User-Agent", "branchwork")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Vec<GlNamespace> = resp.json().await.ok()?;
    body.into_iter().find(|n| n.path == owner).map(|n| n.id)
}

/// Dispatch `POST /projects` against the GitLab API. Resolves `owner`
/// to a `namespace_id` via a lookup call when set; otherwise creates
/// under the PAT owner's user namespace.
pub async fn create_repo(req: CreateRepoRequest<'_>) -> CreateRepoOutcome {
    let owner = req.owner.map(str::trim).filter(|s| !s.is_empty());
    let namespace_id = if let Some(o) = owner {
        resolve_namespace_id(o, req.pat).await
    } else {
        None
    };

    let url = format!("{}/projects", api_base());
    let body = CreateProjectBody {
        name: req.name,
        visibility: if req.private { "private" } else { "public" },
        namespace_id,
    };

    let client = reqwest::Client::new();
    let resp = match client
        .post(&url)
        .header("PRIVATE-TOKEN", req.pat)
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
        // GitLab error shapes vary — sometimes `{"message": ...}`,
        // sometimes `{"error": ...}`. Try both before falling back to
        // the canonical reason phrase.
        let parsed = serde_json::from_str::<serde_json::Value>(&body_text).ok();
        let message = parsed
            .as_ref()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error"))
                    .and_then(|m| m.as_str())
            })
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("gitlab_error")
                    .to_string()
            });
        return CreateRepoOutcome::Failed {
            http_status: status.as_u16(),
            message,
            body_excerpt: excerpt,
        };
    }

    match serde_json::from_str::<GlProjectResponse>(&body_text) {
        Ok(r) => CreateRepoOutcome::Created {
            clone_url: r.http_url_to_repo,
            html_url: r.web_url,
            ssh_url: r.ssh_url_to_repo,
        },
        Err(e) => CreateRepoOutcome::Failed {
            http_status: status.as_u16(),
            message: format!("malformed GitLab response: {e}"),
            body_excerpt: body_text.chars().take(512).collect(),
        },
    }
}

/// Minimal percent-encoder for path-segment safe characters. GitLab
/// namespace paths are restricted to alphanumerics + `-_./`; this
/// helper only escapes the characters that would actually break the
/// URL (space, `?`, `&`, `#`). Anything else passes through.
fn urlencoding_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_defaults_to_gitlab_when_env_unset() {
        let prior = std::env::var("BRANCHWORK_GITLAB_API_BASE").ok();
        // SAFETY: tests in this file share process env; we restore on
        // every path. Concurrent code paths read the env var; tests in
        // this same module that override it set+restore around their
        // own asserts.
        unsafe {
            std::env::remove_var("BRANCHWORK_GITLAB_API_BASE");
        }
        let base = api_base();
        if let Some(prev) = prior {
            unsafe { std::env::set_var("BRANCHWORK_GITLAB_API_BASE", prev) }
        }
        assert_eq!(base, GITLAB_API_BASE_DEFAULT);
    }

    #[test]
    fn urlencoding_path_segment_passes_through_safe_chars() {
        assert_eq!(urlencoding_path_segment("my-org"), "my-org");
        assert_eq!(
            urlencoding_path_segment("alpha_beta.gamma"),
            "alpha_beta.gamma"
        );
    }

    #[test]
    fn urlencoding_path_segment_encodes_special_chars() {
        assert_eq!(urlencoding_path_segment("a b"), "a%20b");
        assert_eq!(urlencoding_path_segment("a?b"), "a%3Fb");
        assert_eq!(urlencoding_path_segment("a&b"), "a%26b");
    }
}
