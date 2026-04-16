use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use rusqlite::params;

use crate::agents::{AgentRegistry, git_head_sha};
use crate::config::Effort;
use crate::ws::broadcast_event;

pub async fn start_check_agent(
    registry: &AgentRegistry,
    prompt: String,
    cwd: &Path,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    effort: Effort,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();
    let base_commit = git_head_sha(cwd);

    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt, base_commit)
             VALUES (?1, ?2, ?3, 'starting', 'stream-json', ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                cwd.to_str().unwrap_or(""),
                plan_name,
                task_id,
                prompt,
                base_commit
            ],
        )
        .ok();
    }

    let mut child = match Command::new("claude")
        .args([
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--session-id",
            &session_id,
            "--add-dir",
            &cwd.to_string_lossy(),
            "--permission-mode",
            "plan",
            "--allowedTools",
            "Read,Glob,Grep,Bash(git:*)",
            "--effort",
            &effort.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[check-agent {}] Failed to spawn claude: {e}", &id[..8]);
            let db = registry.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'failed', finished_at = datetime('now') WHERE id = ?",
                params![id],
            )
            .ok();
            broadcast_event(
                &registry.broadcast_tx,
                "agent_stopped",
                serde_json::json!({"id": id, "status": "failed", "error": e.to_string()}),
            );
            return id;
        }
    };

    let pid = child.id();

    // Send initial prompt via stdin
    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        let msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}]
            }
        });
        writeln!(stdin, "{}", msg).ok();
        // Close stdin for check agents
        drop(child.stdin.take());
    }

    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "UPDATE agents SET pid = ?1, status = 'running' WHERE id = ?2",
            params![pid as i64, id],
        )
        .ok();
    }

    broadcast_event(
        &registry.broadcast_tx,
        "agent_started",
        serde_json::json!({
            "id": id,
            "sessionId": session_id,
            "planName": plan_name,
            "taskId": task_id,
            "pid": pid,
            "mode": "stream-json"
        }),
    );

    // Read stdout in a thread
    let stdout = child.stdout.take().unwrap();
    let db = registry.db.clone();
    let tx = registry.broadcast_tx.clone();
    let id_clone = id.clone();
    let plan_name_owned = plan_name.map(|s| s.to_string());
    let task_id_owned = task_id.map(|s| s.to_string());
    let webhook = registry.webhook_url.clone();

    thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let msg_type = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("type")?.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "raw".to_string());

            {
                let db = db.lock().unwrap();
                db.execute(
                    "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, ?2, ?3)",
                    params![id_clone, msg_type, line],
                )
                .ok();
            }

            broadcast_event(
                &tx,
                "agent_output",
                serde_json::json!({
                    "agent_id": id_clone,
                    "message_type": msg_type,
                }),
            );
        }

        // Wait for exit
        let status = child.wait().ok();
        let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);
        let agent_status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };

        // Extract cost from the result event
        let cost_usd: Option<f64> = {
            let db_guard = db.lock().unwrap();
            let mut stmt = db_guard
                .prepare("SELECT content FROM agent_output WHERE agent_id = ? AND message_type = 'result' ORDER BY id DESC LIMIT 1")
                .unwrap();
            stmt.query_map(params![id_clone], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .find_map(|content| {
                    serde_json::from_str::<serde_json::Value>(&content)
                        .ok()
                        .and_then(|v| v.get("total_cost_usd")?.as_f64())
                })
        };

        {
            let db = db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = ?1, finished_at = datetime('now'), cost_usd = ?3 WHERE id = ?2",
                params![agent_status, id_clone, cost_usd],
            )
            .ok();

            // Org budget enforcement after cost update.
            if cost_usd.is_some() {
                let org_id: Option<String> = db
                    .query_row(
                        "SELECT org_id FROM agents WHERE id = ?1",
                        params![id_clone],
                        |row| row.get::<_, String>(0),
                    )
                    .ok();
                if let Some(org_id) = org_id {
                    crate::saas::billing::enforce_org_budget(&db, &org_id, webhook.as_deref());
                }
            }
        }

        // Parse verdict. Per-task checks have Some(task_id); plan-level checks
        // have task_id == None and plan_name == Some, and write to plan_verdicts
        // / broadcast `plan_checked` instead of the task-scoped equivalents.
        if plan_name_owned.is_some() {
            let db_guard = db.lock().unwrap();
            let mut stmt = db_guard
                .prepare("SELECT content FROM agent_output WHERE agent_id = ? ORDER BY id")
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map(params![id_clone], |row| row.get(0))
                .unwrap()
                .flatten()
                .collect();

            for row in rows.iter().rev() {
                if let Ok(outer) = serde_json::from_str::<serde_json::Value>(row) {
                    let mut text = String::new();
                    if let Some(result) = outer.get("result").and_then(|v| v.as_str()) {
                        text = result.to_string();
                    } else if let Some(content) = outer
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                                && let Some(t) = block.get("text").and_then(|t| t.as_str())
                            {
                                text.push_str(t);
                            }
                        }
                    }

                    // Look for verdict JSON — find {"status": "..."} and extract valid JSON
                    if let Some(start) = text.find(r#""status""#) {
                        // Walk back to find the opening {
                        let json_start = text[..start].rfind('{').unwrap_or(start);
                        // Try progressively longer substrings to find valid JSON
                        let remainder = &text[json_start..];
                        let verdict_json = (1..=remainder.len())
                            .filter(|&i| remainder.as_bytes().get(i - 1) == Some(&b'}'))
                            .find_map(|i| {
                                serde_json::from_str::<serde_json::Value>(&remainder[..i]).ok()
                            });

                        if let Some(verdict) = verdict_json {
                            let v_status = verdict
                                .get("status")
                                .and_then(|s| s.as_str())
                                .filter(|s| ["completed", "in_progress", "pending"].contains(s))
                                .unwrap_or("pending");
                            let v_reason =
                                verdict.get("reason").and_then(|s| s.as_str()).unwrap_or("");

                            if let Some(ref task_id) = task_id_owned {
                                db_guard.execute(
                                    "INSERT INTO task_status (plan_name, task_number, status, updated_at)
                                     VALUES (?1, ?2, ?3, datetime('now'))
                                     ON CONFLICT(plan_name, task_number)
                                     DO UPDATE SET status = excluded.status, updated_at = datetime('now')",
                                    params![plan_name_owned, task_id, v_status],
                                ).ok();

                                broadcast_event(
                                    &tx,
                                    "task_checked",
                                    serde_json::json!({
                                        "plan_name": plan_name_owned,
                                        "task_number": task_id,
                                        "status": v_status,
                                        "reason": v_reason,
                                        "agent_id": id_clone,
                                    }),
                                );
                            } else {
                                db_guard.execute(
                                    "INSERT INTO plan_verdicts (plan_name, verdict, reason, agent_id, checked_at)
                                     VALUES (?1, ?2, ?3, ?4, datetime('now'))
                                     ON CONFLICT(plan_name) DO UPDATE SET
                                       verdict = excluded.verdict,
                                       reason = excluded.reason,
                                       agent_id = excluded.agent_id,
                                       checked_at = datetime('now')",
                                    params![plan_name_owned, v_status, v_reason, id_clone],
                                ).ok();

                                broadcast_event(
                                    &tx,
                                    "plan_checked",
                                    serde_json::json!({
                                        "plan_name": plan_name_owned,
                                        "verdict": v_status,
                                        "reason": v_reason,
                                        "agent_id": id_clone,
                                    }),
                                );
                            }
                            break;
                        }
                    }
                }
            }
        }

        broadcast_event(
            &tx,
            "agent_stopped",
            serde_json::json!({"id": id_clone, "status": agent_status, "exit_code": exit_code}),
        );
    });

    id
}
