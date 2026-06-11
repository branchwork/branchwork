//! Scoped project practices — the knowledge half of the 2026-06-11 pivot
//! (the agent orchestrates, Branchwork observes / advises / shows).
//!
//! A practice is an org rule with a SCOPE: path globs and/or keywords. When an
//! agent asks for `get_task_context`, every practice whose scope matches the
//! task's `file_paths` or its title/description rides along — the rule reaches
//! the agent at the exact moment it starts the work that needs it. Practices
//! are ADVISORY only: no gate ever reads them (CI remains the enforcement
//! layer; an enforcing Branchwork would be the orchestrator again).
//!
//! Distinct from task learnings: a learning is bound to one task (usually a
//! CI failure post-mortem); a practice is project-wide, reusable, scoped by
//! WHERE it applies rather than where it was discovered. The intended loop:
//! a learning that keeps recurring gets promoted into a practice via
//! `practice_add`.

use rmcp::{ErrorData as McpError, Json, handler::server::wrapper::Parameters, tool, tool_router};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::mcp::BranchworkMcp;

// ── Schemas ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PracticeAddRequest {
    /// Path globs the practice applies to (e.g. `packages/db/drizzle/**`).
    /// `*` matches within a path segment, `**` matches across segments.
    /// Empty = not path-scoped.
    #[serde(default)]
    pub scope_globs: Vec<String>,
    /// Case-insensitive keywords matched against the task title +
    /// description (e.g. `migration`, `DocumentType`). Empty = not
    /// keyword-scoped.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The rule itself, imperative and self-contained — what the agent must
    /// do or check.
    pub rule: String,
    /// Why the rule exists (the incident or constraint behind it). Optional
    /// but strongly recommended: agents follow rules better with the why.
    #[serde(default)]
    pub why: Option<String>,
    /// Where the rule comes from (a memory file, an ADR, a post-mortem).
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PracticeSearchRequest {
    /// Free-text query matched (case-insensitive) against rule, why, source,
    /// keywords and globs. Empty/absent = list everything.
    #[serde(default)]
    pub query: Option<String>,
}

/// One practice as served to agents (in `practice_search` results and in
/// `get_task_context.practices`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PracticeHit {
    pub id: i64,
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub scope_globs: Vec<String>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PracticeAdded {
    pub ok: bool,
    pub id: i64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PracticeList {
    pub practices: Vec<PracticeHit>,
}

// ── Matching ─────────────────────────────────────────────────────────────────

/// Minimal glob matcher: `**` crosses `/` boundaries, `*` stays within one
/// segment, everything else is literal. No char classes — practices don't
/// need them, and no new dependency this way.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn inner(pat: &[char], path: &[char]) -> bool {
        match pat.split_first() {
            None => path.is_empty(),
            Some(('*', rest)) if rest.first() == Some(&'*') => {
                // `**` — swallow any prefix (including `/`). Also absorb an
                // immediately following `/` so `a/**/b` matches `a/b`.
                let rest = &rest[1..];
                let rest_no_slash = if rest.first() == Some(&'/') {
                    &rest[1..]
                } else {
                    rest
                };
                (0..=path.len()).any(|i| inner(rest, &path[i..]))
                    || (0..=path.len()).any(|i| inner(rest_no_slash, &path[i..]))
            }
            Some(('*', rest)) => (0..=path.len())
                .take_while(|&i| i == 0 || path[i - 1] != '/')
                .any(|i| inner(rest, &path[i..])),
            Some((c, rest)) => path.first() == Some(c) && inner(rest, &path[1..]),
        }
    }
    let pat: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    inner(&pat, &path)
}

fn parse_json_list(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// Every practice whose scope matches the task: a path glob matches one of
/// `file_paths`, OR a keyword appears (case-insensitive) in `text`
/// (title + description). A practice with an EMPTY scope matches nothing —
/// scope-less rules belong in CLAUDE.md, not here.
pub fn practices_for_task(
    conn: &Connection,
    file_paths: &[String],
    text: &str,
) -> Vec<PracticeHit> {
    let text_lc = text.to_lowercase();
    all_practices(conn)
        .into_iter()
        .filter(|p| {
            let glob_hit = p
                .scope_globs
                .iter()
                .any(|g| file_paths.iter().any(|f| glob_match(g, f)));
            let keyword_hit = p
                .keywords
                .iter()
                .any(|k| !k.trim().is_empty() && text_lc.contains(&k.to_lowercase()));
            glob_hit || keyword_hit
        })
        .collect()
}

fn all_practices(conn: &Connection) -> Vec<PracticeHit> {
    let mut stmt = match conn
        .prepare("SELECT id, scope_globs, keywords, rule, why, source FROM practices ORDER BY id")
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| {
        Ok(PracticeHit {
            id: row.get(0)?,
            scope_globs: parse_json_list(&row.get::<_, String>(1)?),
            keywords: parse_json_list(&row.get::<_, String>(2)?),
            rule: row.get(3)?,
            why: row.get(4)?,
            source: row.get(5)?,
        })
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = practices_router, vis = "pub")]
impl BranchworkMcp {
    #[tool(description = "Record a project practice: an org rule scoped by path \
                       globs and/or keywords, served automatically inside \
                       get_task_context whenever a task's file_paths or \
                       title/description match. Advisory only — practices \
                       never gate anything. Use it to promote a recurring \
                       learning into a standing rule.")]
    pub async fn practice_add(
        &self,
        Parameters(req): Parameters<PracticeAddRequest>,
    ) -> Result<Json<PracticeAdded>, McpError> {
        let rule = req.rule.trim();
        if rule.is_empty() {
            return Err(McpError::invalid_params("rule must not be empty", None));
        }
        if req.scope_globs.iter().all(|g| g.trim().is_empty())
            && req.keywords.iter().all(|k| k.trim().is_empty())
        {
            return Err(McpError::invalid_params(
                "a practice needs a scope (globs and/or keywords) — scope-less \
                 rules belong in the project's agent instructions, not here",
                None,
            ));
        }
        let conn = self.ctx.db.lock().unwrap();
        conn.execute(
            "INSERT INTO practices (scope_globs, keywords, rule, why, source)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                serde_json::to_string(&req.scope_globs).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&req.keywords).unwrap_or_else(|_| "[]".into()),
                rule,
                req.why,
                req.source,
            ],
        )
        .map_err(|e| McpError::internal_error(format!("failed to insert practice: {e}"), None))?;
        Ok(Json(PracticeAdded {
            ok: true,
            id: conn.last_insert_rowid(),
        }))
    }

    #[tool(description = "Search project practices (free-text across rule, why, \
                       source, keywords, globs). No query lists everything.")]
    pub async fn practice_search(
        &self,
        Parameters(req): Parameters<PracticeSearchRequest>,
    ) -> Result<Json<PracticeList>, McpError> {
        let conn = self.ctx.db.lock().unwrap();
        let mut practices = all_practices(&conn);
        if let Some(q) = req
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            let q = q.to_lowercase();
            practices.retain(|p| {
                p.rule.to_lowercase().contains(&q)
                    || p.why.as_deref().unwrap_or("").to_lowercase().contains(&q)
                    || p.source
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&q)
                    || p.keywords.iter().any(|k| k.to_lowercase().contains(&q))
                    || p.scope_globs.iter().any(|g| g.to_lowercase().contains(&q))
            });
        }
        Ok(Json(PracticeList { practices }))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_stays_in_segment() {
        assert!(glob_match("packages/*/src", "packages/db/src"));
        assert!(!glob_match("packages/*/src", "packages/db/drizzle/src"));
    }

    #[test]
    fn glob_doublestar_crosses_segments() {
        assert!(glob_match(
            "packages/db/drizzle/**",
            "packages/db/drizzle/0072_x.sql"
        ));
        assert!(glob_match(
            "packages/db/drizzle/**",
            "packages/db/drizzle/meta/_journal.json"
        ));
        assert!(glob_match("**/i18n.ts", "apps/web/src/lib/i18n.ts"));
        assert!(!glob_match(
            "packages/db/drizzle/**",
            "packages/db/src/schema.ts"
        ));
    }

    #[test]
    fn glob_literal_and_suffix() {
        assert!(glob_match("apps/web/**/*.tsx", "apps/web/src/app/page.tsx"));
        assert!(!glob_match("apps/web/**/*.tsx", "apps/web/src/lib/api.ts"));
    }

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE practices (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                scope_globs TEXT NOT NULL DEFAULT '[]',
                keywords    TEXT NOT NULL DEFAULT '[]',
                rule        TEXT NOT NULL,
                why         TEXT,
                source      TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .expect("practices ddl");
        conn
    }

    fn seed(conn: &Connection, globs: &[&str], keywords: &[&str], rule: &str) {
        conn.execute(
            "INSERT INTO practices (scope_globs, keywords, rule) VALUES (?1, ?2, ?3)",
            params![
                serde_json::to_string(globs).unwrap(),
                serde_json::to_string(keywords).unwrap(),
                rule
            ],
        )
        .unwrap();
    }

    #[test]
    fn matches_by_glob_against_task_file_paths() {
        let conn = test_conn();
        seed(
            &conn,
            &["packages/db/drizzle/**"],
            &[],
            "bump the journal `when` past the latest future-dated floor",
        );
        let hits = practices_for_task(
            &conn,
            &["packages/db/drizzle/0073_x.sql".into()],
            "add a column",
        );
        assert_eq!(hits.len(), 1);
        let misses = practices_for_task(&conn, &["apps/web/src/page.tsx".into()], "ui work");
        assert!(misses.is_empty());
    }

    #[test]
    fn matches_by_keyword_against_title_and_description() {
        let conn = test_conn();
        seed(
            &conn,
            &[],
            &["DocumentType"],
            "full 16-package typecheck before pushing a union extension",
        );
        let hits = practices_for_task(
            &conn,
            &[],
            "Extend the DocumentType union with reference-first types",
        );
        assert_eq!(hits.len(), 1);
        assert!(practices_for_task(&conn, &[], "unrelated work").is_empty());
    }

    #[test]
    fn empty_scope_never_matches() {
        let conn = test_conn();
        seed(&conn, &[], &[], "a rule with no scope");
        assert!(practices_for_task(&conn, &["any/file.rs".into()], "any text").is_empty());
    }
}
