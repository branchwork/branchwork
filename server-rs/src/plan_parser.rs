use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

// ── Types ────────────────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanTask {
    pub number: String,
    pub title: String,
    pub description: String,
    pub file_paths: Vec<String>,
    pub acceptance: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub produces_commit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci: Option<crate::ci::CiStatus>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
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

// ── YAML schema types ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct YamlPlanTask {
    number: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    file_paths: Vec<String>,
    #[serde(default)]
    acceptance: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    produces_commit: bool,
}

#[derive(Serialize, Deserialize)]
struct YamlPlanPhase {
    number: u32,
    title: String,
    #[serde(default)]
    description: String,
    tasks: Vec<YamlPlanTask>,
}

#[derive(Serialize, Deserialize)]
struct YamlPlan {
    title: String,
    #[serde(default)]
    context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    phases: Vec<YamlPlanPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verification: Option<String>,
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
        sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
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
    sorted_dirs.sort_by_key(|b| std::cmp::Reverse(b.0.len()));
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

    // Verification section (optional)
    let verification = sections
        .iter()
        .find(|s| s.heading.to_lowercase().starts_with("verification"))
        .map(|s| s.body.join("\n").trim().to_string())
        .filter(|v| !v.is_empty());

    // Phase regex patterns
    let phase_re = Regex::new(r"(?i)^(?:Phase|Step)\s+(\d+\w?)[:\s.—\-]+(.+)").unwrap();
    let numbered_re = Regex::new(r"^(\d+)[.)]\s+(.+)").unwrap();
    let impl_re = Regex::new(r"(?i)^(changes|implementation|approach|design|the change)").unwrap();

    let mut phases: Vec<PlanPhase> = Vec::new();

    for section in &sections {
        let (phase_num, phase_title) = if let Some(caps) = phase_re.captures(&section.heading) {
            let num: u32 = caps[1]
                .trim_end_matches(char::is_alphabetic)
                .parse()
                .unwrap_or(0);
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
                dependencies: Vec::new(),
                produces_commit: true,
                status: None,
                status_updated_at: None,
                cost_usd: None,
                ci: None,
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
        verification,
        total_cost_usd: None,
        max_budget_usd: None,
    }
}

fn extract_dependencies(text: &str) -> Vec<String> {
    // Matches **Depends on:** 1.1, 1.2  /  **Dependencies:** 1.1, 1.2
    // Capture stops at the next bullet / bold marker / heading / blank line.
    let re = Regex::new(
        r"(?is)\*\*(?:Depends on|Dependencies|Blocked by|Requires):?\*\*\s*(.+?)(?:\n\s*[-*]\s|\n\*\*|\n###|\n---|\n\n|\z|\n$)"
    ).unwrap();
    let Some(caps) = re.captures(text) else {
        return Vec::new();
    };
    caps[1]
        .split([',', ';'])
        .map(|s| s.trim().trim_matches('`').trim_start_matches('#').trim())
        .filter(|s| !s.is_empty() && s.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|s| s.to_string())
        .collect()
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

        let (task_number, task_title) = if let Some(caps) = task_re_dotnum.captures(first_line) {
            (
                caps[1].trim_end_matches('.').to_string(),
                caps[2]
                    .trim_start_matches(['—', ':', '-', ' '])
                    .trim()
                    .to_string(),
            )
        } else if let Some(caps) = task_re_phase.captures(first_line) {
            (
                caps[1].to_string(),
                caps[2]
                    .trim_start_matches(['—', ':', '-', ' '])
                    .trim()
                    .to_string(),
            )
        } else if let Some(caps) = task_re_generic.captures(first_line) {
            (
                caps[1].trim_end_matches('.').to_string(),
                caps[2]
                    .trim_start_matches(['—', ':', '-', ' '])
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
        let dependencies = extract_dependencies(&task_body);

        tasks.push(PlanTask {
            number: task_number,
            title: task_title,
            description: task_body,
            file_paths,
            acceptance,
            dependencies,
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        });
    }
    tasks
}

fn parse_tasks_from_bullets(body: &str, phase_num: u32) -> Vec<PlanTask> {
    let bullet_re = Regex::new(r"(?m)^[-*]\s+\*\*(.+?)\*\*\s*[—:\-]?\s*(.*)").unwrap();
    let mut tasks = Vec::new();
    for (idx, caps) in (1u32..).zip(bullet_re.captures_iter(body)) {
        let title = caps[1].trim().to_string();
        let desc = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        let full = caps.get(0).unwrap().as_str();
        let file_paths = extract_file_paths(full);
        let dependencies = extract_dependencies(full);
        tasks.push(PlanTask {
            number: format!("{phase_num}.{idx}"),
            title,
            description: desc,
            file_paths,
            acceptance: String::new(),
            dependencies,
            produces_commit: true,
            status: None,
            status_updated_at: None,
            cost_usd: None,
            ci: None,
        });
    }
    tasks
}

// ── YAML parser ─────────────────────────────────────────────────────────────

pub fn parse_plan_yaml(raw: &str, name: &str, file_path: &str) -> Result<ParsedPlan, String> {
    let yaml: YamlPlan = serde_yaml::from_str(raw).map_err(|e| e.to_string())?;

    let verification = yaml.verification.clone();
    let phases = yaml
        .phases
        .into_iter()
        .map(|p| {
            let tasks = p
                .tasks
                .into_iter()
                .map(|t| PlanTask {
                    number: t.number,
                    title: t.title,
                    description: t.description,
                    file_paths: t.file_paths,
                    acceptance: t.acceptance,
                    dependencies: t.dependencies,
                    produces_commit: t.produces_commit,
                    status: None,
                    status_updated_at: None,
                    cost_usd: None,
                    ci: None,
                })
                .collect();

            PlanPhase {
                number: p.number,
                title: p.title,
                description: p.description,
                tasks,
            }
        })
        .collect();

    Ok(ParsedPlan {
        name: name.to_string(),
        file_path: file_path.to_string(),
        title: yaml.title,
        context: yaml.context,
        project: yaml.project.or_else(|| infer_project(raw)),
        created_at: yaml.created_at.unwrap_or_default(),
        modified_at: String::new(),
        phases,
        verification,
        total_cost_usd: None,
        max_budget_usd: None,
    })
}

/// Serialize a ParsedPlan to YAML string.
pub fn serialize_plan_yaml(plan: &ParsedPlan) -> Result<String, String> {
    let yaml = YamlPlan {
        title: plan.title.clone(),
        context: plan.context.clone(),
        project: plan.project.clone(),
        created_at: if plan.created_at.is_empty() {
            None
        } else {
            Some(plan.created_at.clone())
        },
        phases: plan
            .phases
            .iter()
            .map(|p| YamlPlanPhase {
                number: p.number,
                title: p.title.clone(),
                description: p.description.clone(),
                tasks: p
                    .tasks
                    .iter()
                    .map(|t| YamlPlanTask {
                        number: t.number.clone(),
                        title: t.title.clone(),
                        description: t.description.clone(),
                        file_paths: t.file_paths.clone(),
                        acceptance: t.acceptance.clone(),
                        dependencies: t.dependencies.clone(),
                        produces_commit: t.produces_commit,
                    })
                    .collect(),
            })
            .collect(),
        verification: plan.verification.clone(),
    };
    serde_yaml::to_string(&yaml).map_err(|e| e.to_string())
}

// ── File-level helpers ───────────────────────────────────────────────────────

pub fn parse_plan_file(file_path: &Path) -> std::io::Result<ParsedPlan> {
    let raw = std::fs::read_to_string(file_path)?;
    let name = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let mut plan = match ext {
        "yaml" | "yml" => parse_plan_yaml(&raw, name, &file_path.to_string_lossy())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        _ => parse_plan_markdown(&raw, name, &file_path.to_string_lossy()),
    };

    let meta = std::fs::metadata(file_path)?;

    // YAML may provide its own created_at; fall back to file metadata
    if plan.created_at.is_empty() {
        plan.created_at = file_time_iso(&meta, true);
    }
    plan.modified_at = file_time_iso(&meta, false);

    Ok(plan)
}

/// Find a plan file by name, checking yaml/yml/md extensions in priority order.
pub fn find_plan_file(plans_dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    for ext in &["yaml", "yml", "md"] {
        let path = plans_dir.join(format!("{name}.{ext}"));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Returns true if the file extension is a supported plan format.
pub fn is_plan_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e, "md" | "yaml" | "yml"))
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

    // Collect plan files, sorted so yaml/yml come before md (for dedup)
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_plan_ext(p))
        .collect();

    paths.sort_by_key(|p| match p.extension().and_then(|e| e.to_str()) {
        Some("yaml") => 0,
        Some("yml") => 1,
        _ => 2,
    });

    let mut seen = HashSet::new();
    let mut summaries = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !seen.insert(name) {
            continue; // prefer yaml over md for same plan name
        }
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
        assert!(
            plan.phases[0].tasks[0]
                .file_paths
                .contains(&"src/model.rs".to_string())
        );

        assert_eq!(plan.phases[1].number, 2);
        assert_eq!(plan.phases[1].tasks.len(), 1);
        assert_eq!(plan.phases[1].tasks[0].number, "2.1");
        assert!(
            plan.phases[1].tasks[0]
                .file_paths
                .contains(&"src/App.tsx".to_string())
        );
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
        assert!(
            plan.phases[0].tasks[1]
                .file_paths
                .contains(&"src/ast.rs".to_string())
        );
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
        assert!(
            plan.phases[0].tasks[0]
                .file_paths
                .contains(&"src/lib.rs".to_string())
        );
    }

    #[test]
    fn markdown_extracts_dependencies() {
        let md = "\
# Plan

## Phase 1: Work

### 1.1 First

- **What:** do the thing
- **Acceptance:** it works

### 1.2 Second

- **What:** do another thing
- **Depends on:** 1.1
- **Acceptance:** it works

### 1.3 Third

- **What:** combine
- **Depends on:** 1.1, 1.2
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert!(plan.phases[0].tasks[0].dependencies.is_empty());
        assert_eq!(plan.phases[0].tasks[1].dependencies, vec!["1.1"]);
        assert_eq!(plan.phases[0].tasks[2].dependencies, vec!["1.1", "1.2"]);
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
        assert_eq!(
            plan.phases[0].tasks[0].acceptance,
            "Tests pass and binary runs."
        );
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
        let Some(plans_dir) = dirs::home_dir().map(|h| h.join(".claude/plans")) else {
            return;
        };
        if !plans_dir.exists() {
            return;
        }

        let summaries = list_plans(&plans_dir);
        assert!(
            !summaries.is_empty(),
            "expected plan files in {}",
            plans_dir.display()
        );

        for s in &summaries {
            assert!(!s.title.is_empty(), "plan {} has empty title", s.name);
        }
    }

    #[test]
    fn rust_rewrite_plan_structure() {
        let plan_path = match dirs::home_dir() {
            Some(h) => h.join(".claude/plans/branchwork-rust-rewrite.md"),
            None => return,
        };
        if !plan_path.exists() {
            return;
        }

        let plan = parse_plan_file(&plan_path).unwrap();
        assert_eq!(plan.title, "Branchwork: Rust Server Rewrite");
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
            ("branchwork-rust-rewrite", Some("Branchwork")),
            ("warm-waddling-catmull", None),
            ("witty-wishing-nygaard", None),
        ];

        let Some(plans_dir) = dirs::home_dir().map(|h| h.join(".claude/plans")) else {
            return;
        };
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

    // ── YAML parser tests ────────────────────────────────────────────────────

    #[test]
    fn yaml_basic_plan() {
        let yaml = "\
title: My YAML Plan
context: |
  Some background.
phases:
  - number: 1
    title: Backend Models
    description: Set up models
    tasks:
      - number: \"1.1\"
        title: Create model
        description: Create the model
        file_paths:
          - src/model.rs
        acceptance: Model compiles
        dependencies: []
      - number: \"1.2\"
        title: Add tests
        description: Write tests for the model.
        file_paths: []
        acceptance: Tests pass
        dependencies:
          - \"1.1\"
  - number: 2
    title: Frontend
    description: Build UI
    tasks:
      - number: \"2.1\"
        title: Add component
        description: Build the component.
        file_paths:
          - src/App.tsx
        acceptance: Component renders
        dependencies:
          - \"1.1\"
          - \"1.2\"
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();

        assert_eq!(plan.title, "My YAML Plan");
        assert_eq!(plan.context, "Some background.\n");
        assert_eq!(plan.phases.len(), 2);

        assert_eq!(plan.phases[0].number, 1);
        assert_eq!(plan.phases[0].title, "Backend Models");
        assert_eq!(plan.phases[0].tasks.len(), 2);
        assert_eq!(plan.phases[0].tasks[0].number, "1.1");
        assert_eq!(plan.phases[0].tasks[0].title, "Create model");
        assert!(
            plan.phases[0].tasks[0]
                .file_paths
                .contains(&"src/model.rs".to_string())
        );
        assert_eq!(plan.phases[0].tasks[0].acceptance, "Model compiles");
        assert!(plan.phases[0].tasks[0].dependencies.is_empty());

        assert_eq!(plan.phases[0].tasks[1].number, "1.2");
        assert_eq!(plan.phases[0].tasks[1].dependencies, vec!["1.1"]);

        assert_eq!(plan.phases[1].number, 2);
        assert_eq!(plan.phases[1].tasks[0].number, "2.1");
        assert_eq!(plan.phases[1].tasks[0].dependencies, vec!["1.1", "1.2"]);
        assert!(
            plan.phases[1].tasks[0]
                .file_paths
                .contains(&"src/App.tsx".to_string())
        );
    }

    #[test]
    fn yaml_with_created_at() {
        let yaml = "\
title: Plan With Date
created_at: \"2025-01-15T10:30:00Z\"
context: Test
phases:
  - number: 0
    title: Setup
    description: \"\"
    tasks:
      - number: \"0.1\"
        title: Init
        description: Initialize
        file_paths: []
        acceptance: Done
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        assert_eq!(plan.created_at, "2025-01-15T10:30:00Z");
    }

    #[test]
    fn yaml_minimal_fields() {
        let yaml = "\
title: Minimal Plan
phases:
  - number: 0
    title: Only Phase
    tasks:
      - number: \"0.1\"
        title: Only Task
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        assert_eq!(plan.title, "Minimal Plan");
        assert_eq!(plan.context, "");
        assert_eq!(plan.project, None);
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].tasks[0].description, "");
        assert!(plan.phases[0].tasks[0].file_paths.is_empty());
        assert_eq!(plan.phases[0].tasks[0].acceptance, "");
        assert!(plan.phases[0].tasks[0].dependencies.is_empty());
    }

    #[test]
    fn yaml_invalid_returns_error() {
        let bad_yaml = "not: [valid: yaml: plan";
        let result = parse_plan_yaml(bad_yaml, "test", "/tmp/test.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn yaml_and_md_produce_same_shape() {
        let yaml = "\
title: My Plan
context: Some background.
phases:
  - number: 1
    title: Backend Models
    description: \"\"
    tasks:
      - number: \"1.1\"
        title: Create model
        description: |
          - **What:** Create the model
          - **Where:** `src/model.rs`
          - **Acceptance:** Model compiles
        file_paths:
          - src/model.rs
        acceptance: Model compiles
";
        let md = "\
# My Plan

## Context

Some background.

## Phase 1: Backend Models

### 1.1 Create model

- **What:** Create the model
- **Where:** `src/model.rs`
- **Acceptance:** Model compiles
";
        let yaml_plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        let md_plan = parse_plan_markdown(md, "test", "/tmp/test.md");

        assert_eq!(yaml_plan.title, md_plan.title);
        assert_eq!(yaml_plan.context, md_plan.context);
        assert_eq!(yaml_plan.phases.len(), md_plan.phases.len());
        assert_eq!(yaml_plan.phases[0].number, md_plan.phases[0].number);
        assert_eq!(yaml_plan.phases[0].title, md_plan.phases[0].title);
        assert_eq!(
            yaml_plan.phases[0].tasks.len(),
            md_plan.phases[0].tasks.len()
        );
        assert_eq!(
            yaml_plan.phases[0].tasks[0].number,
            md_plan.phases[0].tasks[0].number
        );
        assert_eq!(
            yaml_plan.phases[0].tasks[0].title,
            md_plan.phases[0].tasks[0].title
        );
        assert_eq!(
            yaml_plan.phases[0].tasks[0].acceptance,
            md_plan.phases[0].tasks[0].acceptance
        );
    }

    #[test]
    fn parse_plan_file_dispatches_on_extension() {
        let dir = tempfile::tempdir().unwrap();

        // Write a YAML plan
        let yaml_path = dir.path().join("test-plan.yaml");
        std::fs::write(
            &yaml_path,
            "\
title: YAML Plan
phases:
  - number: 0
    title: Setup
    tasks:
      - number: \"0.1\"
        title: Init
",
        )
        .unwrap();

        // Write a markdown plan
        let md_path = dir.path().join("md-plan.md");
        std::fs::write(
            &md_path,
            "\
# MD Plan

## Phase 1: Work

### 1.1 Do stuff

Do the thing.
",
        )
        .unwrap();

        let yaml_plan = parse_plan_file(&yaml_path).unwrap();
        assert_eq!(yaml_plan.title, "YAML Plan");
        assert_eq!(yaml_plan.name, "test-plan");

        let md_plan = parse_plan_file(&md_path).unwrap();
        assert_eq!(md_plan.title, "MD Plan");
        assert_eq!(md_plan.name, "md-plan");
    }

    #[test]
    fn find_plan_file_priority() {
        let dir = tempfile::tempdir().unwrap();

        // Only .md exists
        std::fs::write(dir.path().join("plan-a.md"), "# A").unwrap();
        let found = find_plan_file(dir.path(), "plan-a").unwrap();
        assert_eq!(found.extension().unwrap(), "md");

        // Only .yaml exists
        std::fs::write(dir.path().join("plan-b.yaml"), "title: B\nphases: []").unwrap();
        let found = find_plan_file(dir.path(), "plan-b").unwrap();
        assert_eq!(found.extension().unwrap(), "yaml");

        // Both exist — yaml wins
        std::fs::write(dir.path().join("plan-c.md"), "# C").unwrap();
        std::fs::write(dir.path().join("plan-c.yaml"), "title: C\nphases: []").unwrap();
        let found = find_plan_file(dir.path(), "plan-c").unwrap();
        assert_eq!(found.extension().unwrap(), "yaml");

        // Nothing exists
        assert!(find_plan_file(dir.path(), "nonexistent").is_none());
    }

    #[test]
    fn list_plans_includes_yaml() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("alpha.md"),
            "\
# Alpha Plan

## Phase 1: Work

### 1.1 Task

Do stuff.
",
        )
        .unwrap();

        std::fs::write(
            dir.path().join("beta.yaml"),
            "\
title: Beta Plan
phases:
  - number: 0
    title: Setup
    tasks:
      - number: \"0.1\"
        title: Init
",
        )
        .unwrap();

        let summaries = list_plans(dir.path());
        let names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "missing alpha: {names:?}");
        assert!(names.contains(&"beta"), "missing beta: {names:?}");
    }

    #[test]
    fn list_plans_deduplicates_yaml_over_md() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("dup.md"),
            "\
# MD Version

## Phase 1: Old

### 1.1 Old task

Old.
",
        )
        .unwrap();

        std::fs::write(
            dir.path().join("dup.yaml"),
            "\
title: YAML Version
phases:
  - number: 0
    title: New
    tasks:
      - number: \"0.1\"
        title: New task
",
        )
        .unwrap();

        let summaries = list_plans(dir.path());
        let dup_plans: Vec<_> = summaries.iter().filter(|s| s.name == "dup").collect();
        assert_eq!(dup_plans.len(), 1, "expected exactly one 'dup' plan");
        assert_eq!(dup_plans[0].title, "YAML Version");
    }

    #[test]
    fn is_plan_ext_check() {
        assert!(is_plan_ext(std::path::Path::new("foo.md")));
        assert!(is_plan_ext(std::path::Path::new("foo.yaml")));
        assert!(is_plan_ext(std::path::Path::new("foo.yml")));
        assert!(!is_plan_ext(std::path::Path::new("foo.txt")));
        assert!(!is_plan_ext(std::path::Path::new("foo.json")));
        assert!(!is_plan_ext(std::path::Path::new("foo")));
    }

    #[test]
    fn yaml_verification_round_trip() {
        let yaml = "\
title: Plan With Verification
phases:
  - number: 0
    title: Setup
    tasks:
      - number: \"0.1\"
        title: Init
verification: |-
  1. Step one
  2. Step two
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        assert_eq!(
            plan.verification.as_deref(),
            Some("1. Step one\n2. Step two")
        );

        let serialized = serialize_plan_yaml(&plan).unwrap();
        assert!(
            serialized.contains("verification:"),
            "serialized YAML should contain verification key: {serialized}"
        );

        let reparsed = parse_plan_yaml(&serialized, "test", "/tmp/test.yaml").unwrap();
        assert_eq!(reparsed.verification, plan.verification);
    }

    #[test]
    fn markdown_extracts_verification_section() {
        let md = "\
# Plan

## Context

Background.

## Phase 1: Work

### 1.1 Do it

Do stuff.

## Verification

1. Run the tests
2. Check the output
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(
            plan.verification.as_deref(),
            Some("1. Run the tests\n2. Check the output")
        );
    }

    #[test]
    fn markdown_without_verification_is_none() {
        let md = "\
# Plan

## Phase 1: Work

### 1.1 Do it

Do stuff.
";
        let plan = parse_plan_markdown(md, "test", "/tmp/test.md");
        assert_eq!(plan.verification, None);
    }

    // ── produces_commit tests ─────────────────────────────────────────────

    #[test]
    fn yaml_produces_commit_defaults_to_true() {
        let yaml = "\
title: Plan
phases:
  - number: 0
    title: Setup
    tasks:
      - number: \"0.1\"
        title: Do stuff
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        assert!(
            plan.phases[0].tasks[0].produces_commit,
            "produces_commit should default to true when the key is absent"
        );
    }

    #[test]
    fn yaml_produces_commit_false_override() {
        let yaml = "\
title: Plan
phases:
  - number: 0
    title: Setup
    tasks:
      - number: \"0.1\"
        title: Investigate bug
        produces_commit: false
";
        let plan = parse_plan_yaml(yaml, "test", "/tmp/test.yaml").unwrap();
        assert!(
            !plan.phases[0].tasks[0].produces_commit,
            "produces_commit should be false when explicitly set"
        );
    }

    #[test]
    fn produces_commit_round_trip_serialization() {
        // Build a plan with one task produces_commit=false, another =true
        let plan = ParsedPlan {
            name: "rt".to_string(),
            file_path: "/tmp/rt.yaml".to_string(),
            title: "Round-trip".to_string(),
            context: String::new(),
            project: None,
            created_at: String::new(),
            modified_at: String::new(),
            phases: vec![PlanPhase {
                number: 0,
                title: "P0".to_string(),
                description: String::new(),
                tasks: vec![
                    PlanTask {
                        number: "0.1".to_string(),
                        title: "No commit".to_string(),
                        description: String::new(),
                        file_paths: Vec::new(),
                        acceptance: String::new(),
                        dependencies: Vec::new(),
                        produces_commit: false,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    },
                    PlanTask {
                        number: "0.2".to_string(),
                        title: "Has commit".to_string(),
                        description: String::new(),
                        file_paths: Vec::new(),
                        acceptance: String::new(),
                        dependencies: Vec::new(),
                        produces_commit: true,
                        status: None,
                        status_updated_at: None,
                        cost_usd: None,
                        ci: None,
                    },
                ],
            }],
            verification: None,
            total_cost_usd: None,
            max_budget_usd: None,
        };

        let yaml_str = serialize_plan_yaml(&plan).unwrap();

        // produces_commit: false must appear for task 0.1
        assert!(
            yaml_str.contains("produces_commit: false"),
            "serialized YAML should contain 'produces_commit: false': {yaml_str}"
        );

        // The key should NOT appear for the true task (skip_serializing_if = "is_true")
        // Count occurrences — should be exactly 1 (only the false one)
        let count = yaml_str.matches("produces_commit").count();
        assert_eq!(
            count, 1,
            "produces_commit should appear exactly once (for the false task), found {count} in: {yaml_str}"
        );

        // Re-parse and verify values survive the round-trip
        let reparsed = parse_plan_yaml(&yaml_str, "rt", "/tmp/rt.yaml").unwrap();
        assert!(
            !reparsed.phases[0].tasks[0].produces_commit,
            "round-tripped task 0.1 should have produces_commit=false"
        );
        assert!(
            reparsed.phases[0].tasks[1].produces_commit,
            "round-tripped task 0.2 should have produces_commit=true (default)"
        );
    }

    #[test]
    fn project_inference_all_plans() {
        let Some(plans_dir) = dirs::home_dir().map(|h| h.join(".claude/plans")) else {
            return;
        };
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
