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

use base64::{Engine, engine::general_purpose};
use reqwest::Method;
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

// ── Open a "promote learning to CLAUDE.md" PR (Phase 4, Task 4.2) ───────────

/// Inputs to [`open_claude_md_pr`]. The caller resolves `owner` / `repo`
/// from the project's `repo_url` and the decrypted `pat` from the chosen
/// credential; everything else is the rule content + PR metadata.
pub struct OpenPrRequest<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    /// Decrypted PAT — ephemeral, never logged.
    pub pat: &'a str,
    /// File to append to, e.g. `CLAUDE.md`.
    pub file_path: &'a str,
    /// The H2 section the rule lands under, e.g. `## Other rules`.
    pub section_heading: &'a str,
    /// The markdown block to insert (already wrapped in an H3 heading by
    /// the caller).
    pub rule_block: &'a str,
    /// New branch the commit + PR are created on. Must be unique per
    /// promotion so a retry never collides with a half-opened PR.
    pub branch: &'a str,
    pub commit_message: &'a str,
    pub pr_title: &'a str,
    pub pr_body: &'a str,
}

/// Outcome of [`open_claude_md_pr`]. Mirrors the three-arm shape used by
/// [`CreateRepoOutcome`] — success + host-rejected + transport — plus a
/// `step` tag on the failure arms so the HTTP caller (and the operator)
/// can see which leg of the multi-call flow failed.
#[derive(Debug)]
pub enum OpenPrOutcome {
    /// PR opened. `html_url` is the browser URL (stored on the
    /// promotion-candidate row); `number` is the PR number.
    Opened { html_url: String, number: u64 },
    /// GitHub rejected one of the calls (non-2xx). `step` names the leg.
    Failed {
        step: &'static str,
        http_status: u16,
        message: String,
        body_excerpt: String,
    },
    /// Transport failure on one of the calls (DNS / TLS / refused).
    Transport { step: &'static str, error: String },
}

#[derive(Deserialize)]
struct RepoMeta {
    default_branch: String,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Deserialize)]
struct GitRef {
    object: RefObject,
}

#[derive(Deserialize)]
struct ContentsMeta {
    /// base64 body (with embedded newlines per the GitHub contents API).
    content: String,
    /// blob sha — required when the PUT updates an existing file.
    sha: String,
}

#[derive(Serialize)]
struct CreateRefBody<'a> {
    r#ref: String,
    sha: &'a str,
}

#[derive(Serialize)]
struct PutContentsBody<'a> {
    message: &'a str,
    content: String,
    branch: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<&'a str>,
}

#[derive(Serialize)]
struct CreatePrBody<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    body: &'a str,
}

#[derive(Deserialize)]
struct PrResponse {
    html_url: String,
    number: u64,
}

/// Open a pull request that appends `rule_block` under `section_heading`
/// in `file_path` (default `CLAUDE.md`) against the repo's default
/// branch.
///
/// Pure-REST, no clone: GET repo (default branch) → GET ref (base SHA) →
/// GET contents (existing body + blob SHA, or 404 = create new) → create
/// branch ref → PUT the merged file on that branch → open the PR. Each
/// leg short-circuits to [`OpenPrOutcome::Failed`] / `Transport` tagged
/// with the failing `step`, so a scope error or a name collision surfaces
/// verbatim to the operator. Requires a PAT with `repo` (or
/// `public_repo` + `pull_request`) scope; SSH credentials cannot drive
/// the API and are rejected by the caller before this runs.
pub async fn open_claude_md_pr(r: OpenPrRequest<'_>) -> OpenPrOutcome {
    let client = reqwest::Client::new();
    let base = api_base();
    let repo_path = format!("{base}/repos/{}/{}", r.owner, r.repo);

    // 1. Resolve the default branch.
    let (status, body) = match send(req(&client, Method::GET, &repo_path, r.pat), None).await {
        Ok(t) => t,
        Err(e) => return transport("get_repo", e),
    };
    if !status.is_success() {
        return failed("get_repo", status, &body);
    }
    let default_branch = match serde_json::from_str::<RepoMeta>(&body) {
        Ok(m) => m.default_branch,
        Err(e) => {
            return malformed(
                "get_repo",
                status,
                &format!("missing default_branch: {e}"),
                &body,
            );
        }
    };

    // 2. Base SHA of the default branch tip.
    let ref_url = format!("{repo_path}/git/ref/heads/{default_branch}");
    let (status, body) = match send(req(&client, Method::GET, &ref_url, r.pat), None).await {
        Ok(t) => t,
        Err(e) => return transport("get_ref", e),
    };
    if !status.is_success() {
        return failed("get_ref", status, &body);
    }
    let base_sha = match serde_json::from_str::<GitRef>(&body) {
        Ok(g) => g.object.sha,
        Err(e) => {
            return malformed(
                "get_ref",
                status,
                &format!("missing object.sha: {e}"),
                &body,
            );
        }
    };

    // 3. Existing file body + blob SHA (404 → file does not exist yet).
    let contents_path = format!("{repo_path}/contents/{}", r.file_path);
    let get_contents_url = format!("{contents_path}?ref={default_branch}");
    let (status, body) = match send(req(&client, Method::GET, &get_contents_url, r.pat), None).await
    {
        Ok(t) => t,
        Err(e) => return transport("get_contents", e),
    };
    let (existing_body, existing_sha): (String, Option<String>) = if status.as_u16() == 404 {
        (String::new(), None)
    } else if status.is_success() {
        match serde_json::from_str::<ContentsMeta>(&body) {
            Ok(c) => {
                let decoded = match decode_contents(&c.content) {
                    Ok(s) => s,
                    Err(e) => {
                        return malformed(
                            "get_contents",
                            status,
                            &format!("decode failed: {e}"),
                            &body,
                        );
                    }
                };
                (decoded, Some(c.sha))
            }
            Err(e) => {
                return malformed(
                    "get_contents",
                    status,
                    &format!("malformed contents: {e}"),
                    &body,
                );
            }
        }
    } else {
        return failed("get_contents", status, &body);
    };

    // 4. Merge the rule into the file body.
    let new_body = insert_rule_under_heading(&existing_body, r.section_heading, r.rule_block);

    // 5. Create the branch ref from the base SHA.
    let refs_url = format!("{repo_path}/git/refs");
    let create_ref = CreateRefBody {
        r#ref: format!("refs/heads/{}", r.branch),
        sha: &base_sha,
    };
    let create_ref_json = serde_json::to_string(&create_ref).unwrap_or_default();
    let (status, body) = match send(
        req(&client, Method::POST, &refs_url, r.pat),
        Some(create_ref_json),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return transport("create_ref", e),
    };
    if !status.is_success() {
        return failed("create_ref", status, &body);
    }

    // 6. Commit the merged file on the new branch.
    let put_body = PutContentsBody {
        message: r.commit_message,
        content: general_purpose::STANDARD.encode(new_body.as_bytes()),
        branch: r.branch,
        sha: existing_sha.as_deref(),
    };
    let put_json = serde_json::to_string(&put_body).unwrap_or_default();
    let (status, body) = match send(
        req(&client, Method::PUT, &contents_path, r.pat),
        Some(put_json),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return transport("put_contents", e),
    };
    if !status.is_success() {
        return failed("put_contents", status, &body);
    }

    // 7. Open the PR.
    let pulls_url = format!("{repo_path}/pulls");
    let pr = CreatePrBody {
        title: r.pr_title,
        head: r.branch,
        base: &default_branch,
        body: r.pr_body,
    };
    let pr_json = serde_json::to_string(&pr).unwrap_or_default();
    let (status, body) =
        match send(req(&client, Method::POST, &pulls_url, r.pat), Some(pr_json)).await {
            Ok(t) => t,
            Err(e) => return transport("create_pr", e),
        };
    if !status.is_success() {
        return failed("create_pr", status, &body);
    }
    match serde_json::from_str::<PrResponse>(&body) {
        Ok(p) => OpenPrOutcome::Opened {
            html_url: p.html_url,
            number: p.number,
        },
        Err(e) => malformed(
            "create_pr",
            status,
            &format!("missing html_url: {e}"),
            &body,
        ),
    }
}

/// Build a request with the standard GitHub headers; attach a JSON body
/// when `body` is `Some`.
fn req(client: &reqwest::Client, method: Method, url: &str, pat: &str) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .header("Authorization", format!("Bearer {pat}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "branchwork")
}

/// Send a request, optionally with a JSON string body, and return
/// `(status, body_text)`.
async fn send(
    builder: reqwest::RequestBuilder,
    body: Option<String>,
) -> Result<(reqwest::StatusCode, String), reqwest::Error> {
    let builder = match body {
        Some(b) => builder.header("Content-Type", "application/json").body(b),
        None => builder,
    };
    let resp = builder.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

fn transport(step: &'static str, e: reqwest::Error) -> OpenPrOutcome {
    OpenPrOutcome::Transport {
        step,
        error: format!("{e}"),
    }
}

fn failed(step: &'static str, status: reqwest::StatusCode, body: &str) -> OpenPrOutcome {
    OpenPrOutcome::Failed {
        step,
        http_status: status.as_u16(),
        message: extract_message(body).unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("github_error")
                .to_string()
        }),
        body_excerpt: body.chars().take(512).collect(),
    }
}

fn malformed(
    step: &'static str,
    status: reqwest::StatusCode,
    message: &str,
    body: &str,
) -> OpenPrOutcome {
    OpenPrOutcome::Failed {
        step,
        http_status: status.as_u16(),
        message: message.to_string(),
        body_excerpt: body.chars().take(512).collect(),
    }
}

/// Pull `.message` out of a JSON error body, when present.
fn extract_message(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// Decode a GitHub contents-API `content` field (base64 with embedded
/// newlines) into UTF-8 text. Whitespace is stripped before decoding
/// because the API wraps the base64 at ~60 columns.
fn decode_contents(b64: &str) -> Result<String, String> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("base64: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("utf8: {e}"))
}

/// Parse `owner` / `repo` out of a GitHub clone URL (https / ssh / git
/// forms). Returns `None` for non-github hosts so the caller can surface
/// an `unsupported_host` error rather than dispatching against the wrong
/// API. Mirrors `ci.rs::parse_github_owner_repo` (kept separate so the
/// promote path does not depend on the CI module).
pub fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    let without_host = url
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| url.trim().strip_prefix("https://github.com/"))
        .or_else(|| url.trim().strip_prefix("http://github.com/"))
        .or_else(|| url.trim().strip_prefix("git://github.com/"))
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))?;
    let trimmed = without_host
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let (owner, repo) = trimmed.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Insert `rule_block` into `content` under the H2 `heading` (e.g.
/// `## Other rules`).
///
/// - If `heading` is present, the rule is appended at the END of that
///   section — just before the next `## ` heading, or at EOF when the
///   section is last. This keeps any placeholder text (e.g. "(Add more
///   …)") in place and lands new rules in declaration order.
/// - If `heading` is absent, a fresh `heading` section is appended at the
///   end of the file.
///
/// Spacing is normalised to a single blank line around the inserted
/// block; the original trailing-newline state is preserved.
pub fn insert_rule_under_heading(content: &str, heading: &str, rule_block: &str) -> String {
    let rule = rule_block.trim_end_matches('\n');
    let heading = heading.trim();
    let lines: Vec<&str> = content.lines().collect();
    let heading_idx = lines.iter().position(|l| l.trim() == heading);

    let out = match heading_idx {
        Some(h) => {
            // End of the heading's section: the next column-0 `## ` line
            // after it, else EOF.
            let end = lines[h + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with("## "))
                .map(|rel| h + 1 + rel)
                .unwrap_or(lines.len());
            let mut before: Vec<&str> = lines[..end].to_vec();
            while before.last().is_some_and(|l| l.trim().is_empty()) {
                before.pop();
            }
            let after: &[&str] = &lines[end..];
            let mut s = before.join("\n");
            s.push_str("\n\n");
            s.push_str(rule);
            if !after.is_empty() {
                s.push_str("\n\n");
                s.push_str(&after.join("\n"));
            }
            s
        }
        None => {
            let mut s = content.trim_end_matches('\n').trim_end().to_string();
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(heading);
            s.push_str("\n\n");
            s.push_str(rule);
            s
        }
    };

    // Preserve a single trailing newline (a markdown file should end with
    // one; if the original had none we still add one for cleanliness).
    if out.ends_with('\n') {
        out
    } else {
        format!("{out}\n")
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

    const RULE: &str = "### Always run cargo fmt\n\nRun it before every push.";

    #[test]
    fn insert_appends_at_end_when_heading_is_last_section() {
        let content =
            "# Title\n\n## Other rules\n\n(Add more as project conventions are codified.)\n";
        let out = insert_rule_under_heading(content, "## Other rules", RULE);
        // Placeholder survives; rule lands after it, under the heading.
        assert!(out.contains("(Add more as project conventions are codified.)"));
        let heading_pos = out.find("## Other rules").unwrap();
        let rule_pos = out.find("### Always run cargo fmt").unwrap();
        assert!(rule_pos > heading_pos, "rule sits under the heading");
        assert!(out.ends_with('\n'));
        // One blank line between placeholder and the new H3 (no run-on).
        assert!(out.contains("codified.)\n\n### Always run cargo fmt"));
    }

    #[test]
    fn insert_lands_before_next_section_when_heading_is_not_last() {
        let content =
            "## Other rules\n\nFirst rule here.\n\n## Appendix\n\nUnrelated trailing section.\n";
        let out = insert_rule_under_heading(content, "## Other rules", RULE);
        let rule_pos = out.find("### Always run cargo fmt").unwrap();
        let appendix_pos = out.find("## Appendix").unwrap();
        let first_rule_pos = out.find("First rule here.").unwrap();
        assert!(first_rule_pos < rule_pos, "existing content stays above");
        assert!(
            rule_pos < appendix_pos,
            "new rule lands before next section"
        );
        // The Appendix section is preserved intact.
        assert!(out.contains("Unrelated trailing section."));
    }

    #[test]
    fn insert_appends_new_section_when_heading_absent() {
        let content = "# Project rules\n\n## E2E tests\n\nRun in containers.\n";
        let out = insert_rule_under_heading(content, "## Other rules", RULE);
        assert!(out.contains("## Other rules"), "section created");
        let section_pos = out.find("## Other rules").unwrap();
        let rule_pos = out.find("### Always run cargo fmt").unwrap();
        assert!(rule_pos > section_pos);
        // Original content untouched + above the new section.
        assert!(out.find("Run in containers.").unwrap() < section_pos);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn insert_into_empty_file_creates_the_section() {
        let out = insert_rule_under_heading("", "## Other rules", RULE);
        assert!(out.starts_with("## Other rules\n\n### Always run cargo fmt"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn parse_owner_repo_accepts_common_forms() {
        assert_eq!(
            parse_owner_repo("https://github.com/branchwork/branchwork.git"),
            Some(("branchwork".into(), "branchwork".into()))
        );
        assert_eq!(
            parse_owner_repo("git@github.com:branchwork/branchwork.git"),
            Some(("branchwork".into(), "branchwork".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/o/r"),
            Some(("o".into(), "r".into()))
        );
        assert_eq!(
            parse_owner_repo("https://github.com/o/r/"),
            Some(("o".into(), "r".into()))
        );
    }

    #[test]
    fn parse_owner_repo_rejects_non_github_and_malformed() {
        assert_eq!(parse_owner_repo("https://gitlab.com/o/r"), None);
        assert_eq!(parse_owner_repo("git@bitbucket.org:o/r.git"), None);
        assert_eq!(parse_owner_repo("https://github.com/o"), None);
        assert_eq!(parse_owner_repo("https://github.com//"), None);
    }

    #[test]
    fn decode_contents_strips_wrapping_newlines() {
        // "hello\n" base64 is "aGVsbG8K"; inject newlines/spaces like the
        // GitHub contents API does.
        let wrapped = "aGVs\nbG8K\n";
        assert_eq!(decode_contents(wrapped).unwrap(), "hello\n");
    }
}
