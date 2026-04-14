use std::path::Path;
use std::process::Command;

/// Check if a file path (possibly relative, possibly just a filename) exists in the project.
pub fn find_file_in_project(project_dir: &Path, file_path: &str) -> bool {
    // Strip line number suffixes like :609-664 or :42
    let clean = file_path.split(':').next().unwrap_or(file_path).trim();

    // Direct path check
    let direct = project_dir.join(clean);
    if direct.exists() {
        return true;
    }

    // If it's just a filename (no directory separator) or has "...", search for it
    if !clean.contains('/') || clean.contains("...") {
        let filename = Path::new(clean)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(clean);

        if let Ok(output) = Command::new("find")
            .arg(project_dir)
            .arg("-name")
            .arg(filename)
            .arg("-not")
            .arg("-path")
            .arg("*/node_modules/*")
            .arg("-not")
            .arg("-path")
            .arg("*/.git/*")
            .arg("-not")
            .arg("-path")
            .arg("*/target/*")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return !stdout.trim().is_empty();
        }
    }

    false
}

/// Check git log for commits matching keywords.
#[allow(dead_code)] // Retained for future use — keyword grep disabled due to false positives
pub fn check_git_for_task(project_dir: &Path, keywords: &[&str]) -> usize {
    let git_dir = project_dir.join(".git");
    if !git_dir.exists() {
        return 0;
    }

    let mut hits = 0;
    for kw in keywords {
        if kw.len() < 4 {
            continue;
        }
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(project_dir)
            .arg("log")
            .arg("--oneline")
            .arg("--all")
            .arg("-5")
            .arg(format!("--grep={kw}"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                hits += 1;
            }
        }
    }
    hits
}

/// Determine task status based on file existence.
///
/// Git history matching by task title is unreliable (common keywords like
/// "server", "driver", "agent" match too many existing commits and produce
/// false positives). We use it only as a weak tie-breaker: a single-word grep
/// is never enough to mark anything as done.
///
/// Policy:
/// - No file paths listed for the task → status is "pending". We can't infer
///   progress from keywords alone.
/// - All referenced files missing → "pending" regardless of git hits.
/// - ≥80% of files exist → "completed".
/// - Some files exist → "in_progress".
/// - No files exist → "pending".
pub fn infer_status(
    project_dir: &Path,
    file_paths: &[String],
    _title_words: &[&str],
) -> (&'static str, String) {
    let total_checked = file_paths.len();

    if total_checked == 0 {
        // No anchor to verify against — don't guess.
        return ("pending", "no file paths to check".into());
    }

    let found_count = file_paths
        .iter()
        .filter(|fp| find_file_in_project(project_dir, fp))
        .count();

    let ratio = found_count as f64 / total_checked as f64;
    if ratio >= 0.8 {
        (
            "completed",
            format!("{found_count}/{total_checked} files exist"),
        )
    } else if found_count > 0 {
        (
            "in_progress",
            format!("{found_count}/{total_checked} files exist"),
        )
    } else {
        ("pending", format!("0/{total_checked} files exist"))
    }
}
