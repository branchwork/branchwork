//! Rust test-only `claude` replacement for end-to-end harness work.
//!
//! Put on PATH ahead of the real `claude` binary so the supervisor's PTY
//! spawns this instead. Mirrors the existing
//! `tests/unattended_auto_mode_e2e.rs` bash `STUB_SCRIPT` (parses
//! `--session-id` / `--add-dir`, sleeps long enough for the server to
//! flip the agent to `running`, edits + commits, POSTs the Stop hook,
//! exits 0) — plus per-task scripted actions loaded from a YAML pointed
//! at by `BRANCHWORK_SCRIPTED_ACTIONS_FILE`.
//!
//! Task identity is resolved from the worktree's branch name
//! (`branchwork/<plan>/<task>` per the agent spawn convention). The
//! scripted-actions YAML is keyed by task number — example:
//!
//! ```yaml
//! "1.1":
//!   edits:
//!     - file: lib.rs
//!       write: |
//!         pub trait Diamond { ... }
//!   commit_message: "T1.1 add Diamond trait"
//! "1.2":
//!   edits:
//!     - file: lib.rs
//!       append: |
//!         impl Diamond for Foo { fn a() {} }
//!   commit_message: "T1.2 add method A"
//!   sleep_before_commit_ms: 500   # optional, forces races
//!   skip_stop_hook: false          # optional, repros killed-mid-cleanup
//! ```
//!
//! When the YAML has no entry for the resolved task, the binary falls
//! back to STUB_SCRIPT behaviour (single `work-<sid>.txt` file + commit)
//! so existing tests can swap the bash stub for this binary without
//! authoring a YAML.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, serde::Deserialize)]
struct TaskAction {
    #[serde(default)]
    edits: Vec<Edit>,
    commit_message: Option<String>,
    #[serde(default)]
    sleep_before_commit_ms: u64,
    #[serde(default)]
    skip_stop_hook: bool,
    /// Skip the commit step entirely. Repros agents that finish without
    /// landing a clean tree.
    #[serde(default)]
    skip_commit: bool,
    /// Exit non-zero before committing. Repros crashed-mid-task.
    #[serde(default)]
    exit_code: Option<i32>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Edit {
    Write {
        file: String,
        write: String,
    },
    Append {
        file: String,
        append: String,
    },
    Replace {
        file: String,
        find: String,
        replace: String,
    },
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let session_id = arg_value(&args, "--session-id").unwrap_or_default();
    let add_dir = arg_value(&args, "--add-dir").map(PathBuf::from);

    // Same 1.5s settle as STUB_SCRIPT: lets the server flip the row to
    // `running` before we commit + POST Stop, otherwise handle_stop_hook's
    // `status != "running"` early-return drops the auto-finish.
    std::thread::sleep(Duration::from_millis(1500));

    let cwd = match add_dir.as_ref() {
        Some(p) if p.is_dir() => p.clone(),
        _ => {
            // No usable cwd — match STUB_SCRIPT's silent-exit shape.
            std::process::exit(0);
        }
    };

    let task_id = task_id_from_branch(&cwd);
    let actions_path = std::env::var("BRANCHWORK_SCRIPTED_ACTIONS_FILE").ok();
    let action = task_id
        .as_ref()
        .zip(actions_path.as_ref())
        .and_then(|(tid, path)| load_action(Path::new(path), tid));

    if let Some(action) = action {
        if let Some(code) = action.exit_code {
            std::process::exit(code);
        }
        for edit in &action.edits {
            apply_edit(&cwd, edit);
        }
        if action.sleep_before_commit_ms > 0 {
            std::thread::sleep(Duration::from_millis(action.sleep_before_commit_ms));
        }
        if !action.skip_commit {
            git_add_all(&cwd);
            let msg = action.commit_message.clone().unwrap_or_else(|| {
                format!("scripted: {}", task_id.as_deref().unwrap_or(&session_id))
            });
            git_commit(&cwd, &msg);
        }
        if !action.skip_stop_hook {
            post_stop_hook(&session_id);
        }
    } else {
        // Fallback: STUB_SCRIPT parity — one file + commit + Stop hook.
        let fname = cwd.join(format!("work-{session_id}.txt"));
        std::fs::write(&fname, format!("auto-mode work for {session_id}\n")).ok();
        git_add_all(&cwd);
        git_commit(&cwd, &format!("stub: {session_id}"));
        post_stop_hook(&session_id);
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn task_id_from_branch(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Branch shape per the agent spawn convention: branchwork/<plan>/<task>.
    let tail = branch.rsplit('/').next()?;
    if tail.is_empty() {
        return None;
    }
    Some(tail.to_string())
}

fn load_action(actions_path: &Path, task_id: &str) -> Option<TaskAction> {
    let raw = std::fs::read_to_string(actions_path).ok()?;
    let map: BTreeMap<String, TaskAction> = serde_yaml::from_str(&raw).ok()?;
    map.into_iter().find(|(k, _)| k == task_id).map(|(_, v)| v)
}

fn apply_edit(cwd: &Path, edit: &Edit) {
    match edit {
        Edit::Write { file, write } => {
            let path = cwd.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(path, write).expect("scripted_agent: write edit");
        }
        Edit::Append { file, append } => {
            let path = cwd.join(file);
            let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
            existing.push_str(append);
            std::fs::write(path, existing).expect("scripted_agent: append edit");
        }
        Edit::Replace {
            file,
            find,
            replace,
        } => {
            let path = cwd.join(file);
            let original = std::fs::read_to_string(&path).expect("scripted_agent: replace target");
            let replaced = original.replace(find, replace);
            std::fs::write(path, replaced).expect("scripted_agent: replace edit");
        }
    }
}

fn git_add_all(cwd: &Path) {
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .status()
        .ok();
}

fn git_commit(cwd: &Path, message: &str) {
    Command::new("git")
        .args(["commit", "-q", "--allow-empty", "-m", message])
        .current_dir(cwd)
        .status()
        .ok();
}

fn post_stop_hook(session_id: &str) {
    let Ok(url) = std::env::var("BRANCHWORK_HOOK_URL") else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    // Match the existing bash stub: tiny curl with 5s cap, errors ignored.
    let body = format!(r#"{{"session_id":"{session_id}","hook_event_name":"Stop"}}"#);
    let _ = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "5",
            "-X",
            "POST",
            &url,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ])
        .output();
}
