use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use rusqlite::params;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::agents::{AgentMode, AgentRegistry, ManagedAgent};
use crate::config::Effort;
use crate::ws::broadcast_event;

pub async fn start_pty_agent(
    registry: &AgentRegistry,
    prompt: String,
    cwd: &Path,
    plan_name: Option<&str>,
    task_id: Option<&str>,
    effort: Effort,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    // Insert into DB
    {
        let db = registry.db.lock().unwrap();
        db.execute(
            "INSERT INTO agents (id, session_id, cwd, status, mode, plan_name, task_id, prompt)
             VALUES (?1, ?2, ?3, 'starting', 'pty', ?4, ?5, ?6)",
            params![
                id,
                session_id,
                cwd.to_str().unwrap_or(""),
                plan_name,
                task_id,
                prompt
            ],
        )
        .ok();
    }

    // Create PTY
    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("Failed to open PTY");

    let mut cmd = CommandBuilder::new("claude");
    cmd.arg("--session-id");
    cmd.arg(&session_id);
    cmd.arg("--add-dir");
    cmd.arg(cwd);
    cmd.arg("--verbose");
    cmd.arg("--effort");
    cmd.arg(effort.to_string());
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd).expect("Failed to spawn claude");
    let pid = child.process_id().unwrap_or(0);

    // Update DB with PID
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
            "mode": "pty"
        }),
    );

    let mut reader = pair.master.try_clone_reader().expect("Failed to clone PTY reader");
    let writer = pair.master.take_writer().expect("Failed to take PTY writer");

    let agent = ManagedAgent {
        id: id.clone(),
        session_id: session_id.clone(),
        plan_name: plan_name.map(|s| s.to_string()),
        task_id: task_id.map(|s| s.to_string()),
        mode: AgentMode::Pty,
        pty: Some(child),
        pty_writer: Some(writer),
        pty_master: Some(pair.master),
        terminals: Vec::new(),
    };

    registry.agents.lock().await.insert(id.clone(), agent);

    // Spawn thread to read PTY output
    let db = registry.db.clone();
    let agents = registry.agents.clone();
    let tx = registry.broadcast_tx.clone();
    let id_clone = id.clone();
    let prompt_clone = prompt.clone();

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut ready_detected = false;
        let mut prompt_sent = false;

        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let data = &buf[..n];

                    // Store in DB
                    {
                        let db = db.lock().unwrap();
                        db.execute(
                            "INSERT INTO agent_output (agent_id, message_type, content) VALUES (?1, 'pty', ?2)",
                            params![id_clone, String::from_utf8_lossy(data).to_string()],
                        )
                        .ok();
                    }

                    // Forward to terminals
                    let agents_guard = agents.blocking_lock();
                    if let Some(agent) = agents_guard.get(&id_clone) {
                        for tx in &agent.terminals {
                            tx.send(data.to_vec()).ok();
                        }
                    }
                    drop(agents_guard);

                    // Detect ready signal and send prompt
                    if !ready_detected {
                        let text = String::from_utf8_lossy(data);
                        if text.contains('❯') || text.contains('\u{276f}') {
                            ready_detected = true;
                        }
                    }

                    if ready_detected && !prompt_sent {
                        prompt_sent = true;
                        // Small delay then send prompt
                        let agents_clone = agents.clone();
                        let id_for_write = id_clone.clone();
                        let prompt_for_write = prompt_clone.clone();
                        thread::spawn(move || {
                            thread::sleep(std::time::Duration::from_millis(500));
                            let mut agents_guard = agents_clone.blocking_lock();
                            if let Some(agent) = agents_guard.get_mut(&id_for_write) {
                                if let Some(ref mut writer) = agent.pty_writer {
                                    writer.write_all(prompt_for_write.as_bytes()).ok();
                                    writer.write_all(b"\r").ok();
                                    // Second CR for paste confirmation
                                    thread::sleep(std::time::Duration::from_secs(1));
                                    writer.write_all(b"\r").ok();
                                }
                            }
                        });
                    }
                }
                Err(_) => break,
            }
        }

        // Agent exited
        let db = db.lock().unwrap();
        db.execute(
            "UPDATE agents SET status = 'completed', finished_at = datetime('now') WHERE id = ?",
            params![id_clone],
        )
        .ok();
        drop(db);

        agents.blocking_lock().remove(&id_clone);

        broadcast_event(
            &tx,
            "agent_stopped",
            serde_json::json!({"id": id_clone, "status": "completed", "exit_code": 0}),
        );
    });

    // Fallback: send prompt after 8s if ready signal not detected
    let agents_fallback = registry.agents.clone();
    let id_fallback = id.clone();
    let prompt_fallback = prompt;
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(8)).await;
        let mut agents = agents_fallback.lock().await;
        if let Some(agent) = agents.get_mut(&id_fallback) {
            if let Some(ref mut writer) = agent.pty_writer {
                writer.write_all(prompt_fallback.as_bytes()).ok();
                writer.write_all(b"\r").ok();
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                if let Some(agent) = agents.get_mut(&id_fallback) {
                    if let Some(ref mut writer) = agent.pty_writer {
                        writer.write_all(b"\r").ok();
                    }
                }
            }
        }
    });

    id
}
