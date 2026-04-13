use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use tokio::sync::broadcast;

use crate::plan_parser;
use crate::ws::broadcast_event;

/// Start watching `plans_dir/*.{md,yaml,yml}` for changes. Broadcasts `plan_updated` events.
/// Returns a handle that keeps the watcher alive — drop it to stop watching.
pub fn start(plans_dir: &Path, tx: broadcast::Sender<String>) -> notify::Result<impl Drop> {
    let plans_dir = plans_dir.to_path_buf();

    // Ensure directory exists
    std::fs::create_dir_all(&plans_dir).ok();

    let tx_clone = tx.clone();
    let plans_dir_clone = plans_dir.clone();
    let start_time = std::time::Instant::now();

    // Track file modification times to avoid duplicate events
    let mtimes: Mutex<HashMap<PathBuf, std::time::SystemTime>> = Mutex::new(HashMap::new());

    let mut debouncer = new_debouncer(
        std::time::Duration::from_millis(500),
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            let events = match res {
                Ok(events) => events,
                Err(e) => {
                    eprintln!("[watcher] error: {e}");
                    return;
                }
            };

            // Ignore events during first 3 seconds (startup noise)
            if start_time.elapsed().as_secs() < 3 {
                return;
            }

            for event in events {
                let path = &event.path;

                // Only handle plan files (.md, .yaml, .yml) in the plans directory
                if !plan_parser::is_plan_ext(path)
                    || path.parent() != Some(plans_dir_clone.as_path())
                {
                    continue;
                }

                // Check if the file's mtime actually changed
                let mut mtimes = mtimes.lock().unwrap();
                if path.exists() {
                    if let Ok(meta) = std::fs::metadata(path)
                        && let Ok(mtime) = meta.modified()
                    {
                        let prev = mtimes.get(path);
                        if prev == Some(&mtime) {
                            continue; // mtime unchanged, skip
                        }
                        mtimes.insert(path.clone(), mtime);
                    }
                } else {
                    mtimes.remove(path);
                }
                drop(mtimes);

                handle_event(&event.kind, path, &tx_clone);
            }
        },
    )?;

    debouncer
        .watcher()
        .watch(&plans_dir, notify::RecursiveMode::NonRecursive)?;

    println!("[watcher] Watching {}", plans_dir.display());

    Ok(debouncer)
}

fn handle_event(kind: &DebouncedEventKind, path: &Path, tx: &broadcast::Sender<String>) {
    if kind == &DebouncedEventKind::Any {
        if path.exists() {
            match plan_parser::parse_plan_file(path) {
                Ok(plan) => {
                    println!("[watcher] Plan changed: {}", path.display());
                    broadcast_event(
                        tx,
                        "plan_updated",
                        serde_json::json!({
                            "action": "changed",
                            "plan": plan,
                        }),
                    );
                }
                Err(e) => {
                    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    eprintln!("[watcher] Failed to parse {}: {e}", path.display());
                    broadcast_event(
                        tx,
                        "plan_warning",
                        serde_json::json!({
                            "name": name,
                            "file": path.to_string_lossy(),
                            "error": e.to_string(),
                        }),
                    );
                }
            }
        } else {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            println!("[watcher] Plan removed: {}", path.display());
            broadcast_event(
                tx,
                "plan_updated",
                serde_json::json!({
                    "action": "removed",
                    "name": name,
                }),
            );
        }
    }
}
