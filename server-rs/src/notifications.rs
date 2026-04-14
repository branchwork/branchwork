//! Webhook / Slack notifications for agent and plan milestones.
//!
//! Accepts any JSON-friendly endpoint. Slack incoming webhooks want
//! `{"text": "..."}`, which is also harmless for generic receivers, so that's
//! the single payload shape we use.

use serde_json::json;

/// Fire-and-forget a Slack-compatible notification. No-ops when `webhook_url`
/// is `None`; logs-and-swallows any delivery error so callers don't have to
/// care about network hiccups.
pub fn notify(webhook_url: Option<String>, text: String) {
    let Some(url) = webhook_url.filter(|u| !u.trim().is_empty()) else {
        return;
    };
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[notify] failed to build http client: {e}");
                return;
            }
        };
        match client
            .post(&url)
            .json(&json!({ "text": text }))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                eprintln!(
                    "[notify] webhook returned status {} for {}",
                    resp.status(),
                    redact_url(&url),
                );
            }
            Err(e) => {
                eprintln!("[notify] webhook POST failed: {e}");
            }
        }
    });
}

/// Hide the secret token portion of a webhook URL for logging.
fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}/…")
        }
        None => "<invalid-url>".to_string(),
    }
}

/// Format an agent-completion message.
pub fn agent_completion_message(
    plan_name: Option<&str>,
    task_id: Option<&str>,
    agent_id: &str,
    status: &str,
    branch: Option<&str>,
    cost_usd: Option<f64>,
) -> String {
    let short = &agent_id[..8.min(agent_id.len())];
    let mut s = match (plan_name, task_id) {
        (Some(p), Some(t)) => format!("Agent {short} finished task {t} ({p}) — *{status}*"),
        (Some(p), None) => format!("Agent {short} finished ({p}) — *{status}*"),
        _ => format!("Agent {short} finished — *{status}*"),
    };
    if let Some(b) = branch {
        s.push_str(&format!("\nBranch: `{b}`"));
    }
    if let Some(c) = cost_usd {
        s.push_str(&format!("\nCost: ${c:.4}"));
    }
    s
}

/// Format a phase-advance message.
pub fn phase_advance_message(
    plan_name: &str,
    from_phase: u32,
    to_phase: u32,
    spawned: usize,
) -> String {
    format!(
        "Plan *{plan_name}*: phase {from_phase} complete → advancing to phase {to_phase} ({spawned} task(s) spawned)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_none_is_noop() {
        notify(None, "hi".into());
        notify(Some("   ".into()), "hi".into());
    }

    #[test]
    fn redact_url_strips_path() {
        assert_eq!(
            redact_url("https://hooks.slack.com/services/T00/B11/XXX"),
            "https://hooks.slack.com/…"
        );
        assert_eq!(redact_url("no-scheme"), "<invalid-url>");
    }

    #[test]
    fn agent_completion_message_shape() {
        let m = agent_completion_message(
            Some("my-plan"),
            Some("1.2"),
            "abcdef1234",
            "completed",
            Some("orchestrai/my-plan/1.2"),
            Some(0.1234),
        );
        assert!(m.contains("abcdef12"));
        assert!(m.contains("task 1.2"));
        assert!(m.contains("my-plan"));
        assert!(m.contains("completed"));
        assert!(m.contains("$0.1234"));
    }

    #[test]
    fn phase_advance_message_shape() {
        let m = phase_advance_message("p", 1, 2, 3);
        assert!(m.contains("phase 1"));
        assert!(m.contains("phase 2"));
        assert!(m.contains("3 task"));
    }
}
