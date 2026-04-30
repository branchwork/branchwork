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
/// File existence is evidence the task may have started (or that the listed
/// files pre-existed) — never evidence it finished. Only explicit agent or
/// user action sets "completed", so this function caps inferred status at
/// "in_progress".
///
/// Policy:
/// - No file paths listed → "pending". No anchor to check against.
/// - At least one file exists → "in_progress".
/// - No files exist → "pending".
pub fn infer_status(
    project_dir: &Path,
    file_paths: &[String],
    _title_words: &[&str],
) -> (&'static str, String) {
    let total_checked = file_paths.len();

    if total_checked == 0 {
        return ("pending", "no file paths to check".into());
    }

    let found_count = file_paths
        .iter()
        .filter(|fp| find_file_in_project(project_dir, fp))
        .count();

    if found_count > 0 {
        (
            "in_progress",
            format!("{found_count}/{total_checked} files exist"),
        )
    } else {
        ("pending", format!("0/{total_checked} files exist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn no_file_paths_is_pending() {
        let dir = tempdir().unwrap();
        let (status, reason) = infer_status(dir.path(), &[], &[]);
        assert_eq!(status, "pending");
        assert_eq!(reason, "no file paths to check");
    }

    #[test]
    fn missing_files_is_pending() {
        let dir = tempdir().unwrap();
        let (status, _) = infer_status(
            dir.path(),
            &["does/not/exist.rs".into(), "also/missing.ts".into()],
            &[],
        );
        assert_eq!(status, "pending");
    }

    #[test]
    fn some_files_present_is_in_progress() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        let (status, _) = infer_status(
            dir.path(),
            &["a.rs".into(), "missing.rs".into()],
            &[],
        );
        assert_eq!(status, "in_progress");
    }

    #[test]
    fn all_files_present_is_in_progress_not_completed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::write(dir.path().join("b.rs"), "").unwrap();
        let (status, _) = infer_status(dir.path(), &["a.rs".into(), "b.rs".into()], &[]);
        assert_eq!(status, "in_progress");
    }

    /// Acceptance criterion for Task 2.1: `infer_status` must never return
    /// `"completed"`. Sweep the cases that previously triggered the ≥80%
    /// branch (1/1, 2/2, 4/4, 4/5) and confirm none of them flip to done.
    #[test]
    fn infer_status_never_returns_completed() {
        let dir = tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            fs::write(dir.path().join(name), "").unwrap();
        }

        let cases: &[Vec<String>] = &[
            vec!["a.rs".into()],
            vec!["a.rs".into(), "b.rs".into()],
            vec!["a.rs".into(), "b.rs".into(), "c.rs".into(), "d.rs".into()],
            vec![
                "a.rs".into(),
                "b.rs".into(),
                "c.rs".into(),
                "d.rs".into(),
                "missing.rs".into(),
            ],
        ];

        for files in cases {
            let (status, _) = infer_status(dir.path(), files, &[]);
            assert_ne!(
                status, "completed",
                "infer_status returned completed for {files:?}"
            );
        }
    }
}
