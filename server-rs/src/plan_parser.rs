use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTask {
    pub number: String,
    pub title: String,
    pub description: String,
    pub file_paths: Vec<String>,
    pub acceptance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanPhase {
    pub number: u32,
    pub title: String,
    pub description: String,
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedPlan {
    pub name: String,
    pub file_path: String,
    pub title: String,
    pub context: String,
    pub project: Option<String>,
    pub created_at: String,
    pub modified_at: String,
    pub phases: Vec<PlanPhase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub name: String,
    pub title: String,
    pub project: Option<String>,
    pub phase_count: usize,
    pub task_count: usize,
    pub created_at: String,
    pub modified_at: String,
}

// ── File-path extraction ─────────────────────────────────────────────────────

fn extract_file_paths(text: &str) -> Vec<String> {
    let re = Regex::new(
        r"(?m)`([a-zA-Z0-9_./-]+\.[a-zA-Z0-9]+(?::\d+[-–]\d+)?)`|(?:^|\s)((?:/[\w.\-]+){2,}(?:\.\w+)?(?::\d+[-–]\d+)?)"
    ).unwrap();

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for caps in re.captures_iter(text) {
        let p = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
        if seen.insert(p.to_string()) {
            paths.push(p.to_string());
        }
    }
    paths
}

// ── Project inference ────────────────────────────────────────────────────────

/// Cached list of non-hidden directories in $HOME (scanned once).
fn get_project_dirs() -> &'static Vec<(String, std::path::PathBuf)> {
    static DIRS: OnceLock<Vec<(String, std::path::PathBuf)>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let home = dirs::home_dir().unwrap_or_default();
        match std::fs::read_dir(&home) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| {
                    e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let path = e.path();
                    (name, path)
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    })
}

pub fn infer_project(raw: &str) -> Option<String> {
    let home = dirs::home_dir().unwrap_or_default();

    // 1. Absolute paths like /home/user/project-name/
    let abs_re = Regex::new(r"/home/\w+/([\w.\-]+)/").unwrap();
    let mut abs_counts: HashMap<String, usize> = HashMap::new();
    for caps in abs_re.captures_iter(raw) {
        *abs_counts.entry(caps[1].to_string()).or_default() += 1;
    }
    if !abs_counts.is_empty() {
        let mut sorted: Vec<_> = abs_counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let candidate = &sorted[0].0;
        if home.join(candidate).exists() {
            return Some(candidate.clone());
        }
    }

    // 2. Crate directory scanning
    let project_dirs = get_project_dirs();
    let crate_re = Regex::new(r"crates/([\w\-]+)").unwrap();
    let crate_names: HashSet<String> = crate_re
        .captures_iter(raw)
        .map(|c| c[1].to_string())
        .collect();

    for krate in &crate_names {
        for (proj_name, proj_path) in project_dirs {
            if proj_path.join("crates").join(krate).exists() {
                return Some(proj_name.clone());
            }
        }
    }

    // 3. Module name matching (e.g., module-name/src/)
    let module_re = Regex::new(r"\b([\w]+-[\w]+-[\w]+|[\w]+-[\w]+)/src/").unwrap();
    for caps in module_re.captures_iter(raw) {
        let module_name = &caps[1];
        for (proj_name, proj_path) in project_dirs {
            if proj_path.join(module_name).exists() {
                return Some(proj_name.clone());
            }
        }
    }

    // 4. Title/context keyword matching (first 500 chars)
    //    Sort by name length descending so "reglyze" beats "rust" (substring of "trust").
    let header: String = raw.chars().take(500).collect::<String>().to_lowercase();
    let mut sorted_dirs: Vec<_> = project_dirs.iter().collect();
    sorted_dirs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    for (proj_name, _) in sorted_dirs {
        if proj_name.len() >= 4 && header.contains(&proj_name.to_lowercase()) {
            return Some(proj_name.clone());
        }
    }

    None
}

// ── Markdown parser ──────────────────────────────────────────────────────────

struct Section {
    heading: String,
    body: Vec<String>,
}

pub fn parse_plan_markdown(raw: &str, name: &str, file_path: &str) -> ParsedPlan {
    let lines: Vec<&str> = raw.lines().collect();

    // Title: first # heading
    let title = lines
        .iter()
        .find(|l| l.starts_with("# "))
        .map(|l| l.strip_prefix("# ").unwrap().trim().to_string())
        .unwrap_or_else(|| name.to_string());

    // Split into ## sections
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;

    for line in &lines {
        if line.starts_with("## ") {
            if let Some(s) = current.take() {
                sections.push(s);
            }
            current = Some(Section {
                heading: line.strip_prefix("## ").unwrap().trim().to_string(),
                body: Vec::new(),
            });
        } else if let Some(ref mut s) = current {
            s.body.push(line.to_string());
        }
    }
    if let Some(s) = current {
        sections.push(s);
    }

    // Context section
    let context = sections
        .iter()
        .find(|s| s.heading.to_lowercase().starts_with("context"))
        .map(|s| s.body.join("\n").trim().to_string())
        .unwrap_or_default();

    // Phase regex patterns
    let phase_re = Regex::new(r"(?i)^(?:Phase|Step)\s+(\d+\w?)[:\s.—\-]+(.+)").unwrap();
    let numbered_re = Regex::new(r"^(\d+)[.)]\s+(.+)").unwrap();
    let impl_re = Regex::new(r"(?i)^(changes|implementation|approach|design|the change)").unwrap();

    let mut phases: Vec<PlanPhase> = Vec::new();

    for section in &sections {
        let (phase_num, phase_title) = if let Some(caps) = phase_re.captures(&section.heading) {
            let num: u32 = caps[1].trim_end_matches(char::is_alphabetic).parse().unwrap_or(0);
            (Some(num), caps[2].trim().to_string())
        } else if let Some(caps) = numbered_re.captures(&section.heading) {
            let num: u32 = caps[1].parse().unwrap_or(0);
            (Some(num), caps[2].trim().to_string())
        } else if impl_re.is_match(&section.heading) {
            (Some(phases.len() as u32), section.heading.clone())
        } else {
            (None, String::new())
        };

        let Some(phase_num) = phase_num else {
            continue;
        };

        let body = section.body.join("\n");

        // Parse ### task sub-headings
        let mut tasks = parse_tasks_from_headings(&body);

        // Fallback: bold bullet points
        if tasks.is_empty() {
            tasks = parse_tasks_from_bullets(&body, phase_num);
        }

        // Last resort: entire phase body as one task
        if tasks.is_empty() && !body.trim().is_empty() {
            tasks.push(PlanTask {
                number: format!("{phase_num}.1"),
                title: phase_title.clone(),
                description: body.trim().to_string(),
                file_paths: extract_file_paths(&body),
                acceptance: String::new(),
                status: None,
                status_updated_at: None,
            });
        }

        let description = body.split("###").next().unwrap_or("").trim().to_string();

        phases.push(PlanPhase {
            number: phase_num,
            title: phase_title,
            description,
            tasks,
        });
    }

    ParsedPlan {
        name: name.to_string(),
        file_path: file_path.to_string(),
        title,
        context,
        project: infer_project(raw),
        created_at: String::new(),
        modified_at: String::new(),
        phases,
    }
}

fn parse_tasks_from_headings(body: &str) -> Vec<PlanTask> {
    // Split on ### headings, keeping the heading in each block
    let task_re_dotnum = Regex::new(r"^### (\d+[\.\d]*\w?)\s+(.+)").unwrap();
    let task_re_phase = Regex::new(r"(?i)^### (?:Phase|Step)\s+(\w+)[:\s.—\-]+(.+)").unwrap();
    let task_re_generic = Regex::new(r"^### (\S+)\s+(.+)").unwrap();

    let acc_re =
        Regex::new(r"(?is)\*\*Acceptance:?\*\*\s*(.+?)(?:\n\*\*|\n###|\n---|\z|\n$)").unwrap();

    let mut tasks = Vec::new();

    // Split body at ### boundaries
    let blocks: Vec<&str> = body.split("\n### ").collect();
    for (i, block) in blocks.iter().enumerate() {
        let block_text = if i == 0 {
            // First block doesn't start with ### (it's pre-heading content)
            if !block.starts_with("### ") && !block.trim_start().starts_with("### ") {
                continue;
            }
            block.to_string()
        } else {
            format!("### {block}")
        };

        let first_line = block_text.lines().next().unwrap_or("");

        let (task_number, task_title) =
            if let Some(caps) = task_re_dotnum.captures(first_line) {
                (
                    caps[1].trim_end_matches('.').to_string(),
                    caps[2]
                        .trim_start_matches(|c: char| c == '—' || c == ':' || c == '-' || c == ' ')
                        .trim()
                        .to_string(),
                )
            } else if let Some(caps) = task_re_phase.captures(first_line) {
                (
                    caps[1].to_string(),
                    caps[2]
                        .trim_start_matches(|c: char| c == '—' || c == ':' || c == '-' || c == ' ')
                        .trim()
                        .to_string(),
                )
            } else if let Some(caps) = task_re_generic.captures(first_line) {
                (
                    caps[1].trim_end_matches('.').to_string(),
                    caps[2]
                        .trim_start_matches(|c: char| c == '—' || c == ':' || c == '-' || c == ' ')
                        .trim()
                        .to_string(),
                )
            } else {
                continue;
            };

        let task_body: String = block_text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let task_body = task_body.trim().to_string();

        let acceptance = acc_re
            .captures(&task_body)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();

        let file_paths = extract_file_paths(&task_body);

        tasks.push(PlanTask {
            number: task_number,
            title: task_title,
            description: task_body,
            file_paths,
            acceptance,
            status: None,
            status_updated_at: None,
        });
    }
    tasks
}

fn parse_tasks_from_bullets(body: &str, phase_num: u32) -> Vec<PlanTask> {
    let bullet_re = Regex::new(r"(?m)^[-*]\s+\*\*(.+?)\*\*\s*[—:\-]?\s*(.*)").unwrap();
    let mut tasks = Vec::new();
    let mut idx = 1u32;
    for caps in bullet_re.captures_iter(body) {
        let title = caps[1].trim().to_string();
        let desc = caps.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
        let file_paths = extract_file_paths(caps.get(0).unwrap().as_str());
        tasks.push(PlanTask {
            number: format!("{phase_num}.{idx}"),
            title,
            description: desc,
            file_paths,
            acceptance: String::new(),
            status: None,
            status_updated_at: None,
        });
        idx += 1;
    }
    tasks
}

// ── File-level helpers ───────────────────────────────────────────────────────

pub fn parse_plan_file(file_path: &Path) -> std::io::Result<ParsedPlan> {
    let raw = std::fs::read_to_string(file_path)?;
    let name = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let mut plan = parse_plan_markdown(&raw, name, &file_path.to_string_lossy());
    let meta = std::fs::metadata(file_path)?;

    plan.created_at = file_time_iso(&meta, true);
    plan.modified_at = file_time_iso(&meta, false);

    Ok(plan)
}

fn file_time_iso(meta: &std::fs::Metadata, created: bool) -> String {
    use std::time::UNIX_EPOCH;
    let t = if created {
        meta.created().unwrap_or(UNIX_EPOCH)
    } else {
        meta.modified().unwrap_or(UNIX_EPOCH)
    };
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let dt = chrono::DateTime::from_timestamp(dur.as_secs() as i64, dur.subsec_nanos())
        .unwrap_or_default();
    dt.to_rfc3339()
}

pub fn list_plans(plans_dir: &Path) -> Vec<PlanSummary> {
    let entries = match std::fs::read_dir(plans_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut summaries = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md") {
            if let Ok(parsed) = parse_plan_file(&path) {
                let task_count: usize = parsed.phases.iter().map(|p| p.tasks.len()).sum();
                summaries.push(PlanSummary {
                    name: parsed.name,
                    title: parsed.title,
                    project: parsed.project,
                    phase_count: parsed.phases.len(),
                    task_count,
                    created_at: parsed.created_at,
                    modified_at: parsed.modified_at,
                });
            }
        }
    }
    summaries
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_step_format() {
        let md = "\
# My Plan

## Context

Some background.

## Step 1: Backend Models

### 1.1 Create model

- **What:** Create the model
- **Where:** `src/model.rs`
- **Acceptance:** Model compiles

### 1.2 Add tests

Write tests for the model.

## Step 2: Frontend

### 2.1 Add component

Build the component in `src/App.tsx`.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");

        assert_eq!(plan.title, "My Plan");
        assert_eq!(plan.context, "Some background.");
        assert_eq!(plan.phases.len(), 2);

        assert_eq!(plan.phases[0].number, 1);
        assert_eq!(plan.phases[0].title, "Backend Models");
        assert_eq!(plan.phases[0].tasks.len(), 2);
        assert_eq!(plan.phases[0].tasks[0].number, "1.1");
        assert_eq!(plan.phases[0].tasks[0].title, "Create model");
        assert!(plan.phases[0].tasks[0].file_paths.contains(&"src/model.rs".to_string()));

        assert_eq!(plan.phases[1].number, 2);
        assert_eq!(plan.phases[1].tasks.len(), 1);
        assert_eq!(plan.phases[1].tasks[0].number, "2.1");
        assert!(plan.phases[1].tasks[0].file_paths.contains(&"src/App.tsx".to_string()));
    }

    #[test]
    fn phase_keyword_format() {
        let md = "\
# Plan

## Phase 0: Scaffolding

### 0.1 Setup

Cargo.toml

### 0.2 Config

`src/config.rs`

## Phase 1: Core

### 1.1 Database

`src/db.rs`
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases.len(), 2);
        assert_eq!(plan.phases[0].number, 0);
        assert_eq!(plan.phases[0].title, "Scaffolding");
        assert_eq!(plan.phases[0].tasks.len(), 2);
        assert_eq!(plan.phases[1].number, 1);
        assert_eq!(plan.phases[1].tasks[0].number, "1.1");
    }

    #[test]
    fn numbered_heading_format() {
        let md = "\
# Plan

## 1) First phase

### 1.1 Task A

Do something.

## 2) Second phase

### 2.1 Task B

Do something else.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases.len(), 2);
        assert_eq!(plan.phases[0].number, 1);
        assert_eq!(plan.phases[0].title, "First phase");
        assert_eq!(plan.phases[1].number, 2);
    }

    #[test]
    fn changes_section_fallback() {
        let md = "\
# Dynamic Topic Routing

## Context

Background info.

## Changes

### 1. Grammar extension

Extend `config_value` in `src/grammar.pest`.

### 2. AST variant

Add `ConfigValue::Concat` in `src/ast.rs`.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].title, "Changes");
        assert_eq!(plan.phases[0].tasks.len(), 2);
        assert_eq!(plan.phases[0].tasks[0].number, "1");
        assert_eq!(plan.phases[0].tasks[0].title, "Grammar extension");
        assert!(plan.phases[0].tasks[1].file_paths.contains(&"src/ast.rs".to_string()));
    }

    #[test]
    fn bold_bullet_fallback() {
        let md = "\
# Plan

## Phase 1: Updates

- **Add logging** — add structured logging to `src/main.rs`
- **Fix timeout** — increase timeout in `src/config.rs`
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases[0].tasks.len(), 2);
        assert_eq!(plan.phases[0].tasks[0].number, "1.1");
        assert_eq!(plan.phases[0].tasks[0].title, "Add logging");
        assert_eq!(plan.phases[0].tasks[1].number, "1.2");
        assert_eq!(plan.phases[0].tasks[1].title, "Fix timeout");
    }

    #[test]
    fn whole_phase_as_task_fallback() {
        let md = "\
# Plan

## Phase 1: Quick fix

Just patch the file at `src/lib.rs` and move on.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases[0].tasks.len(), 1);
        assert_eq!(plan.phases[0].tasks[0].number, "1.1");
        assert_eq!(plan.phases[0].tasks[0].title, "Quick fix");
        assert!(plan.phases[0].tasks[0].file_paths.contains(&"src/lib.rs".to_string()));
    }

    #[test]
    fn acceptance_extraction() {
        let md = "\
# Plan

## Phase 1: Work

### 1.1 Build it

- **What:** Build the thing
- **Where:** `src/main.rs`
- **Acceptance:** Tests pass and binary runs.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.phases[0].tasks[0].acceptance, "Tests pass and binary runs.");
    }

    #[test]
    fn file_path_extraction() {
        let text = "Edit `src/config.rs` and /home/user/project/lib/mod.rs to fix the bug.";
        let paths = extract_file_paths(text);
        assert!(paths.contains(&"src/config.rs".to_string()));
        assert!(paths.contains(&"/home/user/project/lib/mod.rs".to_string()));
    }

    #[test]
    fn file_path_dedup() {
        let text = "See `src/main.rs` and also `src/main.rs` again.";
        let paths = extract_file_paths(text);
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn no_title_uses_name() {
        let md = "Just some text without a heading.";
        let plan = parse_plan_markdown(md, "fallback-name", "/tmp/test.md");
        assert_eq!(plan.title, "fallback-name");
    }

    #[test]
    fn parses_real_plan_files() {
        let plans_dir = std::path::Path::new(env!("HOME")).join(".claude/plans");
        if !plans_dir.exists() {
            return;
        }

        let summaries = list_plans(&plans_dir);
        assert!(!summaries.is_empty(), "expected plan files in {}", plans_dir.display());

        for s in &summaries {
            assert!(!s.title.is_empty(), "plan {} has empty title", s.name);
        }
    }

    #[test]
    fn rust_rewrite_plan_structure() {
        let plan_path = std::path::Path::new(env!("HOME"))
            .join(".claude/plans/orchestrai-rust-rewrite.md");
        if !plan_path.exists() {
            return;
        }

        let plan = parse_plan_file(&plan_path).unwrap();
        assert_eq!(plan.title, "orchestrAI: Rust Server Rewrite");
        assert_eq!(plan.phases.len(), 11);

        // Phase 0: 3 tasks
        assert_eq!(plan.phases[0].number, 0);
        assert_eq!(plan.phases[0].title, "Project Scaffolding");
        assert_eq!(plan.phases[0].tasks.len(), 3);
        assert_eq!(plan.phases[0].tasks[0].number, "0.1");
        assert_eq!(plan.phases[0].tasks[0].title, "Cargo workspace setup");

        // Phase 1: 5 tasks
        assert_eq!(plan.phases[1].number, 1);
        assert_eq!(plan.phases[1].tasks.len(), 5);

        // Phase 5: 4 tasks
        assert_eq!(plan.phases[5].number, 5);
        assert_eq!(plan.phases[5].tasks.len(), 4);
        assert_eq!(plan.phases[5].tasks[0].number, "5.1");

        // Phase 10: 6 tasks (merged from roadmap)
        assert_eq!(plan.phases[10].number, 10);
        assert_eq!(plan.phases[10].tasks.len(), 6);
    }

    #[test]
    fn project_inference_parity() {
        // Expected project assignments from the TypeScript server.
        // Plans with DB overrides are excluded — inference alone may differ.
        let expected: &[(&str, Option<&str>)] = &[
            ("orchestrai-rust-rewrite", Some("orchestrAI")),
            ("warm-waddling-catmull", None),
            ("witty-wishing-nygaard", None),
        ];

        let plans_dir = std::path::Path::new(env!("HOME")).join(".claude/plans");
        if !plans_dir.exists() {
            return;
        }

        for (name, exp_project) in expected {
            let path = plans_dir.join(format!("{name}.md"));
            if !path.exists() {
                continue;
            }
            let plan = parse_plan_file(&path).unwrap();
            assert_eq!(
                plan.project.as_deref(),
                *exp_project,
                "project mismatch for plan {name}: got {:?}, expected {:?}",
                plan.project,
                exp_project,
            );
        }
    }

    #[test]
    fn project_inference_all_plans() {
        let plans_dir = std::path::Path::new(env!("HOME")).join(".claude/plans");
        if !plans_dir.exists() {
            return;
        }

        let summaries = list_plans(&plans_dir);
        // Print all inferred projects for manual review
        let mut with_project = 0;
        for s in &summaries {
            if s.project.is_some() {
                with_project += 1;
            }
        }
        assert!(
            with_project > 0,
            "expected at least some plans to have inferred projects"
        );
    }
}
