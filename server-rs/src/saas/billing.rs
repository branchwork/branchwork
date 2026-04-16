//! Per-org usage tracking, budget limits, and email alerts.
//!
//! Aggregates `agents.cost_usd` per org per billing period. Enforces org-level
//! budget limits with email alerts at 80% and 100%, plus a kill switch that
//! blocks new agents when the budget is exceeded. Per-user quotas provide
//! cost allocation within an org.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgBudget {
    pub org_id: String,
    pub max_budget_usd: f64,
    pub billing_period: String,
    pub period_start: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuota {
    pub org_id: String,
    pub user_id: String,
    pub max_budget_usd: f64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgUsageSummary {
    pub org_id: String,
    pub period_key: String,
    pub total_cost_usd: f64,
    pub max_budget_usd: Option<f64>,
    pub pct_used: Option<f64>,
    pub kill_switch_active: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserUsage {
    pub user_id: String,
    pub email: String,
    pub cost_usd: f64,
    pub quota_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// No org budget configured.
    NoBudget,
    /// Under 80% of budget.
    Ok,
    /// Between 80% and 100%.
    Warning,
    /// At or over 100%.
    Exceeded,
    /// Kill switch is manually or automatically active.
    Killed,
}

// ── Billing period helpers ─────────────────────────────────────────────────

/// Return the current monthly billing period key, e.g. "2026-04".
pub fn current_period_key() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

/// Return the start/end datetimes for a monthly period key like "2026-04".
fn period_range(period_key: &str) -> (String, String) {
    // Period key is "YYYY-MM"; start = first day of month, end = first day of
    // next month.
    let start = format!("{period_key}-01 00:00:00");
    // Parse year/month and advance to next month.
    let parts: Vec<&str> = period_key.split('-').collect();
    let year: i32 = parts[0].parse().unwrap_or(2026);
    let month: u32 = parts[1].parse().unwrap_or(1);
    let (ny, nm) = if month >= 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let end = format!("{ny:04}-{nm:02}-01 00:00:00");
    (start, end)
}

// ── Cost aggregation ───────────────────────────────────────────────────────

/// Total cost for an org in a given billing period.
pub fn org_cost_for_period(conn: &Connection, org_id: &str, period_key: &str) -> f64 {
    let (start, end) = period_range(period_key);
    conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE org_id = ?1 AND cost_usd IS NOT NULL \
           AND started_at >= ?2 AND started_at < ?3",
        params![org_id, start, end],
        |row| row.get::<_, f64>(0),
    )
    .unwrap_or(0.0)
}

/// Per-user cost breakdown for an org in a given billing period.
pub fn user_costs_for_period(conn: &Connection, org_id: &str, period_key: &str) -> Vec<UserUsage> {
    let (start, end) = period_range(period_key);
    conn.prepare(
        "SELECT a.user_id, COALESCE(u.email, 'unknown'), \
                COALESCE(SUM(a.cost_usd), 0), q.max_budget_usd \
         FROM agents a \
         LEFT JOIN users u ON u.id = a.user_id \
         LEFT JOIN user_quotas q ON q.org_id = a.org_id AND q.user_id = a.user_id \
         WHERE a.org_id = ?1 AND a.cost_usd IS NOT NULL \
           AND a.started_at >= ?2 AND a.started_at < ?3 \
           AND a.user_id IS NOT NULL \
         GROUP BY a.user_id",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![org_id, start, end], |row| {
            Ok(UserUsage {
                user_id: row.get(0)?,
                email: row.get(1)?,
                cost_usd: row.get(2)?,
                quota_usd: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Cost for a single user within an org for a given billing period.
pub fn user_cost_for_period(
    conn: &Connection,
    org_id: &str,
    user_id: &str,
    period_key: &str,
) -> f64 {
    let (start, end) = period_range(period_key);
    conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM agents \
         WHERE org_id = ?1 AND user_id = ?2 AND cost_usd IS NOT NULL \
           AND started_at >= ?3 AND started_at < ?4",
        params![org_id, user_id, start, end],
        |row| row.get::<_, f64>(0),
    )
    .unwrap_or(0.0)
}

// ── Budget reads ───────────────────────────────────────────────────────────

/// Load the org budget config, if any.
pub fn get_org_budget(conn: &Connection, org_id: &str) -> Option<OrgBudget> {
    conn.query_row(
        "SELECT org_id, max_budget_usd, billing_period, period_start, updated_at \
         FROM org_budgets WHERE org_id = ?1",
        params![org_id],
        |row| {
            Ok(OrgBudget {
                org_id: row.get(0)?,
                max_budget_usd: row.get(1)?,
                billing_period: row.get(2)?,
                period_start: row.get(3)?,
                updated_at: row.get(4)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Check whether the kill switch is active for an org.
pub fn is_kill_switch_active(conn: &Connection, org_id: &str) -> bool {
    conn.query_row(
        "SELECT active FROM org_kill_switch WHERE org_id = ?1",
        params![org_id],
        |row| row.get::<_, i32>(0),
    )
    .unwrap_or(0)
        != 0
}

/// Full budget status check for an org.
pub fn check_org_budget(conn: &Connection, org_id: &str) -> BudgetStatus {
    if is_kill_switch_active(conn, org_id) {
        return BudgetStatus::Killed;
    }
    let budget = match get_org_budget(conn, org_id) {
        Some(b) => b,
        None => return BudgetStatus::NoBudget,
    };
    let period = current_period_key();
    let spent = org_cost_for_period(conn, org_id, &period);
    let pct = spent / budget.max_budget_usd;
    if pct >= 1.0 {
        BudgetStatus::Exceeded
    } else if pct >= 0.8 {
        BudgetStatus::Warning
    } else {
        BudgetStatus::Ok
    }
}

/// Check per-user quota. Returns `Err((spent, max))` if exceeded.
pub fn check_user_quota(conn: &Connection, org_id: &str, user_id: &str) -> Result<(), (f64, f64)> {
    let quota: Option<f64> = conn
        .query_row(
            "SELECT max_budget_usd FROM user_quotas WHERE org_id = ?1 AND user_id = ?2",
            params![org_id, user_id],
            |row| row.get::<_, f64>(0),
        )
        .optional()
        .ok()
        .flatten();
    let max = match quota {
        Some(m) => m,
        None => return Ok(()),
    };
    let period = current_period_key();
    let spent = user_cost_for_period(conn, org_id, user_id, &period);
    if spent >= max {
        Err((spent, max))
    } else {
        Ok(())
    }
}

// ── Budget writes ──────────────────────────────────────────────────────────

/// Set or update the org budget. Pass `None` to remove.
pub fn set_org_budget(conn: &Connection, org_id: &str, max_budget_usd: f64) {
    conn.execute(
        "INSERT INTO org_budgets (org_id, max_budget_usd, updated_at) \
         VALUES (?1, ?2, datetime('now')) \
         ON CONFLICT(org_id) DO UPDATE SET \
           max_budget_usd = excluded.max_budget_usd, \
           updated_at = excluded.updated_at",
        params![org_id, max_budget_usd],
    )
    .ok();
}

pub fn delete_org_budget(conn: &Connection, org_id: &str) {
    conn.execute("DELETE FROM org_budgets WHERE org_id = ?1", params![org_id])
        .ok();
}

pub fn set_user_quota(conn: &Connection, org_id: &str, user_id: &str, max_budget_usd: f64) {
    conn.execute(
        "INSERT INTO user_quotas (org_id, user_id, max_budget_usd, updated_at) \
         VALUES (?1, ?2, ?3, datetime('now')) \
         ON CONFLICT(org_id, user_id) DO UPDATE SET \
           max_budget_usd = excluded.max_budget_usd, \
           updated_at = excluded.updated_at",
        params![org_id, user_id, max_budget_usd],
    )
    .ok();
}

pub fn delete_user_quota(conn: &Connection, org_id: &str, user_id: &str) {
    conn.execute(
        "DELETE FROM user_quotas WHERE org_id = ?1 AND user_id = ?2",
        params![org_id, user_id],
    )
    .ok();
}

pub fn list_user_quotas(conn: &Connection, org_id: &str) -> Vec<UserQuota> {
    conn.prepare(
        "SELECT org_id, user_id, max_budget_usd, updated_at \
         FROM user_quotas WHERE org_id = ?1",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![org_id], |row| {
            Ok(UserQuota {
                org_id: row.get(0)?,
                user_id: row.get(1)?,
                max_budget_usd: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Toggle the kill switch. Returns the new state.
pub fn set_kill_switch(conn: &Connection, org_id: &str, active: bool, reason: Option<&str>) {
    conn.execute(
        "INSERT INTO org_kill_switch (org_id, active, reason, toggled_at) \
         VALUES (?1, ?2, ?3, datetime('now')) \
         ON CONFLICT(org_id) DO UPDATE SET \
           active = excluded.active, \
           reason = excluded.reason, \
           toggled_at = excluded.toggled_at",
        params![org_id, active as i32, reason],
    )
    .ok();
}

// ── Budget enforcement (called after cost updates) ─────────────────────────

/// Check the org's budget after a cost update. Returns the thresholds that
/// were newly crossed (empty if none). Inserts into `budget_alerts` to prevent
/// duplicate notifications.
pub fn check_and_record_alerts(conn: &Connection, org_id: &str) -> Vec<u32> {
    let budget = match get_org_budget(conn, org_id) {
        Some(b) => b,
        None => return vec![],
    };
    let period = current_period_key();
    let spent = org_cost_for_period(conn, org_id, &period);
    let pct = spent / budget.max_budget_usd;

    let mut newly_crossed = vec![];
    for threshold in [80, 100] {
        if pct >= (threshold as f64 / 100.0) {
            // Try to insert; if UNIQUE conflict, we already alerted.
            let inserted = conn
                .execute(
                    "INSERT OR IGNORE INTO budget_alerts (org_id, threshold, period_key) \
                     VALUES (?1, ?2, ?3)",
                    params![org_id, threshold, period],
                )
                .unwrap_or(0);
            if inserted > 0 {
                newly_crossed.push(threshold);
            }
        }
    }

    // Auto-activate kill switch at 100%.
    if pct >= 1.0 && !is_kill_switch_active(conn, org_id) {
        set_kill_switch(conn, org_id, true, Some("budget_exceeded_auto"));
    }

    newly_crossed
}

/// Build the org usage summary for the current period.
pub fn org_usage_summary(conn: &Connection, org_id: &str) -> OrgUsageSummary {
    let period = current_period_key();
    let spent = org_cost_for_period(conn, org_id, &period);
    let budget = get_org_budget(conn, org_id);
    let max = budget.as_ref().map(|b| b.max_budget_usd);
    let pct = max.map(|m| if m > 0.0 { spent / m } else { 0.0 });
    let killed = is_kill_switch_active(conn, org_id);
    OrgUsageSummary {
        org_id: org_id.to_string(),
        period_key: period,
        total_cost_usd: spent,
        max_budget_usd: max,
        pct_used: pct,
        kill_switch_active: killed,
    }
}

// ── Email alerts ───────────────────────────────────────────────────────────

/// SMTP configuration, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub from: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl SmtpConfig {
    /// Try to load SMTP config from environment. Returns `None` if
    /// `SMTP_HOST` is not set.
    pub fn from_env() -> Option<Self> {
        let host = std::env::var("SMTP_HOST").ok()?;
        Some(Self {
            host,
            port: std::env::var("SMTP_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(587),
            from: std::env::var("SMTP_FROM").unwrap_or_else(|_| "orchestrai@localhost".to_string()),
            username: std::env::var("SMTP_USERNAME").ok(),
            password: std::env::var("SMTP_PASSWORD").ok(),
        })
    }
}

/// Send a budget alert email. Fire-and-forget; logs errors but doesn't panic.
pub fn send_budget_alert_email(
    smtp: &SmtpConfig,
    to_emails: &[String],
    org_name: &str,
    threshold: u32,
    spent: f64,
    max_budget: f64,
) {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    for email in to_emails {
        let subject = if threshold >= 100 {
            format!("[orchestrAI] Budget exceeded for {org_name}")
        } else {
            format!("[orchestrAI] {threshold}% budget warning for {org_name}")
        };
        let body = if threshold >= 100 {
            format!(
                "Organization \"{org_name}\" has exceeded its monthly budget.\n\n\
                 Spent: ${spent:.2}\n\
                 Budget: ${max_budget:.2}\n\n\
                 New agent spawns are blocked until the budget is increased or \
                 the next billing period begins.\n\n\
                 — orchestrAI"
            )
        } else {
            format!(
                "Organization \"{org_name}\" has reached {threshold}% of its monthly budget.\n\n\
                 Spent: ${spent:.2}\n\
                 Budget: ${max_budget:.2}\n\n\
                 Consider increasing the budget or reviewing agent usage.\n\n\
                 — orchestrAI"
            )
        };

        let msg = match Message::builder()
            .from(
                smtp.from
                    .parse()
                    .unwrap_or_else(|_| "orchestrai@localhost".parse().unwrap()),
            )
            .to(match email.parse() {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("[billing] invalid email address {email}: {e}");
                    continue;
                }
            })
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[billing] failed to build email: {e}");
                continue;
            }
        };

        let transport_builder = SmtpTransport::relay(&smtp.host);
        let transport = match transport_builder {
            Ok(b) => {
                let b = b.port(smtp.port);
                if let (Some(user), Some(pass)) = (&smtp.username, &smtp.password) {
                    b.credentials(Credentials::new(user.clone(), pass.clone()))
                        .build()
                } else {
                    b.build()
                }
            }
            Err(e) => {
                eprintln!("[billing] SMTP relay error for {}: {e}", smtp.host);
                continue;
            }
        };

        match transport.send(&msg) {
            Ok(_) => {
                println!("[billing] sent {threshold}% alert to {email} for org {org_name}");
            }
            Err(e) => {
                eprintln!("[billing] failed to send email to {email}: {e}");
            }
        }
    }
}

/// Collect email addresses for org owners/admins (the people who should
/// receive budget alerts).
pub fn org_alert_recipients(conn: &Connection, org_id: &str) -> Vec<String> {
    conn.prepare(
        "SELECT u.email FROM org_members om \
         JOIN users u ON u.id = om.user_id \
         WHERE om.org_id = ?1 AND om.role IN ('owner', 'admin') \
         ORDER BY u.email",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![org_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Full enforcement routine: check thresholds, record alerts, send emails,
/// activate kill switch. Called after any agent cost update.
pub fn enforce_org_budget(conn: &Connection, org_id: &str, webhook_url: Option<&str>) {
    let budget = match get_org_budget(conn, org_id) {
        Some(b) => b,
        None => return,
    };

    let newly_crossed = check_and_record_alerts(conn, org_id);
    if newly_crossed.is_empty() {
        return;
    }

    let period = current_period_key();
    let spent = org_cost_for_period(conn, org_id, &period);

    // Resolve org name for messages.
    let org_name: String = conn
        .query_row(
            "SELECT name FROM organizations WHERE id = ?1",
            params![org_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| org_id.to_string());

    let recipients = org_alert_recipients(conn, org_id);

    for &threshold in &newly_crossed {
        // Webhook notification (always, if configured).
        if let Some(url) = webhook_url.filter(|u| !u.trim().is_empty()) {
            let text = if threshold >= 100 {
                format!(
                    ":rotating_light: Org *{org_name}* exceeded budget: \
                     ${spent:.2} / ${:.2}. New agents blocked.",
                    budget.max_budget_usd
                )
            } else {
                format!(
                    ":warning: Org *{org_name}* at {threshold}% of budget: \
                     ${spent:.2} / ${:.2}",
                    budget.max_budget_usd
                )
            };
            crate::notifications::notify(Some(url.to_string()), text);
        }

        // Email notification (if SMTP configured).
        if let Some(smtp) = SmtpConfig::from_env() {
            send_budget_alert_email(
                &smtp,
                &recipients,
                &org_name,
                threshold,
                spent,
                budget.max_budget_usd,
            );
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        // Minimal schema for billing tests.
        conn.execute_batch(
            "
            CREATE TABLE organizations (
                id TEXT PRIMARY KEY, name TEXT NOT NULL,
                slug TEXT NOT NULL UNIQUE, created_at TEXT DEFAULT (datetime('now'))
            );
            CREATE TABLE users (
                id TEXT PRIMARY KEY, email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL, created_at TEXT DEFAULT (datetime('now'))
            );
            CREATE TABLE org_members (
                org_id TEXT NOT NULL, user_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member',
                joined_at TEXT DEFAULT (datetime('now')),
                PRIMARY KEY (org_id, user_id)
            );
            CREATE TABLE agents (
                id TEXT PRIMARY KEY, session_id TEXT, pid INTEGER,
                cwd TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'starting',
                mode TEXT NOT NULL DEFAULT 'pty', started_at TEXT DEFAULT (datetime('now')),
                finished_at TEXT, cost_usd REAL, org_id TEXT DEFAULT 'default-org',
                user_id TEXT, plan_name TEXT, task_id TEXT
            );
            CREATE TABLE org_budgets (
                org_id TEXT PRIMARY KEY, max_budget_usd REAL NOT NULL,
                billing_period TEXT NOT NULL DEFAULT 'monthly',
                period_start TEXT, updated_at TEXT DEFAULT (datetime('now'))
            );
            CREATE TABLE user_quotas (
                org_id TEXT NOT NULL, user_id TEXT NOT NULL,
                max_budget_usd REAL NOT NULL, updated_at TEXT DEFAULT (datetime('now')),
                PRIMARY KEY (org_id, user_id)
            );
            CREATE TABLE budget_alerts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                org_id TEXT NOT NULL, threshold INTEGER NOT NULL,
                period_key TEXT NOT NULL, alerted_at TEXT DEFAULT (datetime('now')),
                UNIQUE(org_id, threshold, period_key)
            );
            CREATE TABLE org_kill_switch (
                org_id TEXT PRIMARY KEY, active INTEGER NOT NULL DEFAULT 0,
                reason TEXT, toggled_at TEXT DEFAULT (datetime('now'))
            );
            INSERT INTO organizations (id, name, slug) VALUES ('org1', 'Test Org', 'test-org');
            INSERT INTO users (id, email, password_hash) VALUES ('u1', 'alice@test.com', 'x');
            INSERT INTO users (id, email, password_hash) VALUES ('u2', 'bob@test.com', 'x');
            INSERT INTO org_members (org_id, user_id, role) VALUES ('org1', 'u1', 'owner');
            INSERT INTO org_members (org_id, user_id, role) VALUES ('org1', 'u2', 'member');
            ",
        )
        .unwrap();
        conn
    }

    fn insert_agent(conn: &Connection, id: &str, org_id: &str, user_id: &str, cost: f64) {
        conn.execute(
            "INSERT INTO agents (id, cwd, status, org_id, user_id, cost_usd, started_at) \
             VALUES (?1, '/tmp', 'completed', ?2, ?3, ?4, datetime('now'))",
            params![id, org_id, user_id, cost],
        )
        .unwrap();
    }

    #[test]
    fn period_range_basic() {
        let (start, end) = period_range("2026-04");
        assert_eq!(start, "2026-04-01 00:00:00");
        assert_eq!(end, "2026-05-01 00:00:00");
    }

    #[test]
    fn period_range_december() {
        let (start, end) = period_range("2026-12");
        assert_eq!(start, "2026-12-01 00:00:00");
        assert_eq!(end, "2027-01-01 00:00:00");
    }

    #[test]
    fn org_cost_aggregation() {
        let conn = test_db();
        let period = current_period_key();
        assert_eq!(org_cost_for_period(&conn, "org1", &period), 0.0);

        insert_agent(&conn, "a1", "org1", "u1", 1.50);
        insert_agent(&conn, "a2", "org1", "u2", 0.75);
        insert_agent(&conn, "a3", "other-org", "u1", 10.0);

        let cost = org_cost_for_period(&conn, "org1", &period);
        assert!((cost - 2.25).abs() < 0.001);
        // Other org not counted.
        assert!((org_cost_for_period(&conn, "other-org", &period) - 10.0).abs() < 0.001);
    }

    #[test]
    fn user_cost_aggregation() {
        let conn = test_db();
        let period = current_period_key();
        insert_agent(&conn, "a1", "org1", "u1", 1.00);
        insert_agent(&conn, "a2", "org1", "u1", 0.50);
        insert_agent(&conn, "a3", "org1", "u2", 2.00);

        assert!((user_cost_for_period(&conn, "org1", "u1", &period) - 1.50).abs() < 0.001);
        assert!((user_cost_for_period(&conn, "org1", "u2", &period) - 2.00).abs() < 0.001);
    }

    #[test]
    fn budget_status_no_budget() {
        let conn = test_db();
        assert_eq!(check_org_budget(&conn, "org1"), BudgetStatus::NoBudget);
    }

    #[test]
    fn budget_status_ok() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        insert_agent(&conn, "a1", "org1", "u1", 10.0);
        assert_eq!(check_org_budget(&conn, "org1"), BudgetStatus::Ok);
    }

    #[test]
    fn budget_status_warning_at_80_pct() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        insert_agent(&conn, "a1", "org1", "u1", 80.0);
        assert_eq!(check_org_budget(&conn, "org1"), BudgetStatus::Warning);
    }

    #[test]
    fn budget_status_exceeded_at_100_pct() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        insert_agent(&conn, "a1", "org1", "u1", 100.0);
        assert_eq!(check_org_budget(&conn, "org1"), BudgetStatus::Exceeded);
    }

    #[test]
    fn kill_switch_overrides_budget_status() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        set_kill_switch(&conn, "org1", true, Some("manual"));
        assert_eq!(check_org_budget(&conn, "org1"), BudgetStatus::Killed);
    }

    #[test]
    fn user_quota_enforcement() {
        let conn = test_db();
        // No quota → always ok.
        assert!(check_user_quota(&conn, "org1", "u1").is_ok());

        set_user_quota(&conn, "org1", "u1", 5.0);
        insert_agent(&conn, "a1", "org1", "u1", 3.0);
        assert!(check_user_quota(&conn, "org1", "u1").is_ok());

        insert_agent(&conn, "a2", "org1", "u1", 2.0);
        let err = check_user_quota(&conn, "org1", "u1");
        assert!(err.is_err());
        let (spent, max) = err.unwrap_err();
        assert!((spent - 5.0).abs() < 0.001);
        assert!((max - 5.0).abs() < 0.001);
    }

    #[test]
    fn alerts_deduplicated() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        insert_agent(&conn, "a1", "org1", "u1", 85.0);

        let alerts1 = check_and_record_alerts(&conn, "org1");
        assert_eq!(alerts1, vec![80]);

        // Second call should not re-alert.
        let alerts2 = check_and_record_alerts(&conn, "org1");
        assert!(alerts2.is_empty());
    }

    #[test]
    fn alerts_100_auto_kills() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 100.0);
        insert_agent(&conn, "a1", "org1", "u1", 105.0);

        let alerts = check_and_record_alerts(&conn, "org1");
        assert!(alerts.contains(&80));
        assert!(alerts.contains(&100));
        assert!(is_kill_switch_active(&conn, "org1"));
    }

    #[test]
    fn kill_switch_manual_toggle() {
        let conn = test_db();
        assert!(!is_kill_switch_active(&conn, "org1"));
        set_kill_switch(&conn, "org1", true, Some("maintenance"));
        assert!(is_kill_switch_active(&conn, "org1"));
        set_kill_switch(&conn, "org1", false, None);
        assert!(!is_kill_switch_active(&conn, "org1"));
    }

    #[test]
    fn org_usage_summary_shape() {
        let conn = test_db();
        set_org_budget(&conn, "org1", 50.0);
        insert_agent(&conn, "a1", "org1", "u1", 20.0);

        let summary = org_usage_summary(&conn, "org1");
        assert_eq!(summary.org_id, "org1");
        assert!((summary.total_cost_usd - 20.0).abs() < 0.001);
        assert_eq!(summary.max_budget_usd, Some(50.0));
        assert!((summary.pct_used.unwrap() - 0.4).abs() < 0.001);
        assert!(!summary.kill_switch_active);
    }

    #[test]
    fn alert_recipients_are_owners_and_admins() {
        let conn = test_db();
        let recipients = org_alert_recipients(&conn, "org1");
        // u1 is owner, u2 is member → only u1 gets alerts.
        assert_eq!(recipients, vec!["alice@test.com"]);
    }

    #[test]
    fn user_quotas_crud() {
        let conn = test_db();
        assert!(list_user_quotas(&conn, "org1").is_empty());

        set_user_quota(&conn, "org1", "u1", 10.0);
        set_user_quota(&conn, "org1", "u2", 5.0);
        let quotas = list_user_quotas(&conn, "org1");
        assert_eq!(quotas.len(), 2);

        delete_user_quota(&conn, "org1", "u1");
        assert_eq!(list_user_quotas(&conn, "org1").len(), 1);
    }
}
