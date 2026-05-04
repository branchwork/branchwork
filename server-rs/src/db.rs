use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

/// Thread-safe handle to the SQLite database.
pub type Db = Arc<Mutex<Connection>>;

/// Return the set of task numbers whose `task_status` is `completed` or
/// `skipped` for the given plan. Used to evaluate task dependency gates.
pub fn completed_task_numbers(conn: &Connection, plan_name: &str) -> HashSet<String> {
    conn.prepare(
        "SELECT task_number FROM task_status \
         WHERE plan_name = ?1 AND status IN ('completed', 'skipped')",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![plan_name], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()
    })
    .unwrap_or_default()
}

/// Load the recorded learnings for a single task. Most-recent first.
pub fn task_learnings(conn: &Connection, plan_name: &str, task_number: &str) -> Vec<String> {
    conn.prepare(
        "SELECT learning FROM task_learnings \
         WHERE plan_name = ?1 AND task_number = ?2 \
         ORDER BY id DESC",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![plan_name, task_number], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()
    })
    .unwrap_or_default()
}

/// Whether auto-mode should currently act on `plan_name`. True iff the
/// user opted in (`enabled = 1`) AND the loop has not self-paused
/// (`paused_reason IS NULL`). Mirrors `auto_advance_enabled`.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn auto_mode_enabled(db: &Db, plan_name: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled FROM plan_auto_mode \
         WHERE plan_name = ?1 AND paused_reason IS NULL",
        params![plan_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

/// Record that auto-mode has self-paused for `plan_name`. UPSERT so a
/// pause that races a row deletion still lands; `enabled` is left
/// untouched on the conflict path so the user's opt-in state survives.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn auto_mode_pause(db: &Db, plan_name: &str, reason: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, paused_reason, paused_at) \
         VALUES (?1, ?2, datetime('now')) \
         ON CONFLICT(plan_name) DO UPDATE SET \
           paused_reason = excluded.paused_reason, \
           paused_at = excluded.paused_at",
        params![plan_name, reason],
    )
    .ok();
}

/// Clear `paused_reason` / `paused_at` for `plan_name`. No-op if no row
/// exists — there is nothing to unpause.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn auto_mode_resume(db: &Db, plan_name: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = NULL, paused_at = NULL \
         WHERE plan_name = ?1",
        params![plan_name],
    )
    .ok();
}

/// Snapshot of `plan_auto_mode` for `plan_name`. All fields fall back to
/// schema defaults when no row exists (matches `auto_mode_enabled` /
/// `plan_max_fix_attempts` semantics).
#[allow(dead_code)] // wired in by 3.5.3 worktrees gate + later loop callers
#[derive(Debug, Clone)]
pub struct AutoModeConfig {
    pub enabled: bool,
    pub max_fix_attempts: u32,
    pub paused_reason: Option<String>,
    pub parallel: bool,
}

/// Read the full `plan_auto_mode` row for `plan_name`. Defaults to
/// `enabled=false`, `max_fix_attempts=3`, `paused_reason=None`,
/// `parallel=false` when no row exists.
#[allow(dead_code)] // wired in by 3.5.3 worktrees gate + later loop callers
pub fn auto_mode_config(db: &Db, plan_name: &str) -> AutoModeConfig {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled, max_fix_attempts, paused_reason, parallel \
         FROM plan_auto_mode WHERE plan_name = ?1",
        params![plan_name],
        |row| {
            Ok(AutoModeConfig {
                enabled: row.get::<_, i64>(0)? != 0,
                max_fix_attempts: row.get::<_, i64>(1)? as u32,
                paused_reason: row.get::<_, Option<String>>(2)?,
                parallel: row.get::<_, i64>(3)? != 0,
            })
        },
    )
    .unwrap_or(AutoModeConfig {
        enabled: false,
        max_fix_attempts: 3,
        paused_reason: None,
        parallel: false,
    })
}

/// Snapshot of `plan_auto_advance` for `plan_name`. All fields fall back
/// to schema defaults when no row exists.
#[allow(dead_code)] // wired in by 3.5.3 worktrees gate + later loop callers
#[derive(Debug, Clone)]
pub struct AutoAdvanceConfig {
    pub enabled: bool,
    pub parallel: bool,
}

/// Read the full `plan_auto_advance` row for `plan_name`. Defaults to
/// `enabled=false`, `parallel=false` when no row exists.
#[allow(dead_code)] // wired in by 3.5.3 worktrees gate + later loop callers
pub fn auto_advance_config(db: &Db, plan_name: &str) -> AutoAdvanceConfig {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled, parallel FROM plan_auto_advance WHERE plan_name = ?1",
        params![plan_name],
        |row| {
            Ok(AutoAdvanceConfig {
                enabled: row.get::<_, i64>(0)? != 0,
                parallel: row.get::<_, i64>(1)? != 0,
            })
        },
    )
    .unwrap_or(AutoAdvanceConfig {
        enabled: false,
        parallel: false,
    })
}

/// Per-plan retry cap for fix agents. Mirrors the schema default (3) so
/// plans without a `plan_auto_mode` row return the same value the loop
/// would see if one had been UPSERTed with defaults. The auto-mode loop
/// gates each `spawn_fix_agent` on `task_fix_attempt_count >= cap`.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn plan_max_fix_attempts(db: &Db, plan_name: &str) -> u32 {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT max_fix_attempts FROM plan_auto_mode WHERE plan_name = ?1",
        params![plan_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v as u32)
    .unwrap_or(3)
}

/// Number of fix attempts already recorded for `(plan_name, task_number)`.
/// The loop compares this against `plan_auto_mode.max_fix_attempts`
/// before spawning another fix agent.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn task_fix_attempt_count(db: &Db, plan_name: &str, task_number: &str) -> u32 {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM task_fix_attempts \
         WHERE plan_name = ?1 AND task_number = ?2",
        params![plan_name, task_number],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as u32
}

/// Insert a fix-attempt row. The `(plan_name, task_number, attempt)` PK
/// makes the call idempotent on retry: a duplicate triple is ignored,
/// not overwritten, so the original `started_at` is preserved.
/// `outcome` and `finished_at` stay NULL until the agent stops; a later
/// helper will close the row out.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn record_fix_attempt(
    db: &Db,
    plan_name: &str,
    task_number: &str,
    attempt: u32,
    agent_id: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO task_fix_attempts \
           (plan_name, task_number, attempt, agent_id, started_at) \
         VALUES (?1, ?2, ?3, ?4, datetime('now')) \
         ON CONFLICT(plan_name, task_number, attempt) DO NOTHING",
        params![plan_name, task_number, attempt as i64, agent_id],
    )
    .ok();
}

/// Close a fix-attempt row out with a final `outcome` ("green" / "red" /
/// "stalled" / "merge_failed"). Idempotent — a second call updates the
/// outcome string (the loop never re-uses an attempt id, so the only way
/// this fires twice is during a manual retry or testing).
#[allow(dead_code)] // wired into the auto-mode fix-completion handler
pub fn close_fix_attempt(db: &Db, plan_name: &str, task_number: &str, attempt: u32, outcome: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE task_fix_attempts \
            SET outcome = ?4, finished_at = datetime('now') \
          WHERE plan_name = ?1 AND task_number = ?2 AND attempt = ?3",
        params![plan_name, task_number, attempt as i64, outcome],
    )
    .ok();
}

/// Recover the `(task_number, attempt)` mapping for a fix agent — the
/// original task id is stored on the row alongside the fix agent's id, so
/// the auto-mode completion handler can find both from the agent_id alone
/// without parsing the `-fix-<n>` suffix off the fix task id.
#[allow(dead_code)] // wired into the auto-mode fix-completion handler
pub fn fix_attempt_for_agent(db: &Db, plan_name: &str, agent_id: &str) -> Option<(String, u32)> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT task_number, attempt FROM task_fix_attempts \
          WHERE plan_name = ?1 AND agent_id = ?2",
        params![plan_name, agent_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32)),
    )
    .ok()
}

/// Open (or create) the database at `db_path` and run migrations.
pub fn init(db_path: &Path) -> Db {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create db directory");
    }

    let conn = Connection::open(db_path)
        .unwrap_or_else(|e| panic!("failed to open database at {}: {e}", db_path.display()));

    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .expect("failed to set journal_mode");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("failed to enable foreign keys");

    migrate(&conn);

    Arc::new(Mutex::new(conn))
}

fn migrate(conn: &Connection) {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS hook_events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT    NOT NULL,
            hook_type   TEXT    NOT NULL,
            tool_name   TEXT,
            tool_input  TEXT,
            timestamp   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_hook_session ON hook_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_hook_type    ON hook_events(hook_type);

        CREATE TABLE IF NOT EXISTS agents (
            id                TEXT PRIMARY KEY,
            session_id        TEXT,
            pid               INTEGER,
            parent_agent_id   TEXT,
            plan_name         TEXT,
            task_id           TEXT,
            cwd               TEXT NOT NULL,
            status            TEXT NOT NULL DEFAULT 'starting',
            mode              TEXT NOT NULL DEFAULT 'pty',
            prompt            TEXT,
            started_at        TEXT NOT NULL DEFAULT (datetime('now')),
            finished_at       TEXT,
            last_tool         TEXT,
            last_activity_at  TEXT,
            base_commit       TEXT,
            branch            TEXT,
            source_branch     TEXT,
            supervisor_socket TEXT,
            driver            TEXT DEFAULT 'claude',
            FOREIGN KEY (parent_agent_id) REFERENCES agents(id)
        );
        CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);

        CREATE TABLE IF NOT EXISTS agent_output (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT NOT NULL,
            message_type TEXT NOT NULL,
            content      TEXT NOT NULL,
            timestamp    TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (agent_id) REFERENCES agents(id)
        );
        CREATE INDEX IF NOT EXISTS idx_output_agent ON agent_output(agent_id);

        CREATE TABLE IF NOT EXISTS plan_project (
            plan_name  TEXT PRIMARY KEY,
            project    TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS task_status (
            plan_name   TEXT NOT NULL,
            task_number TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'pending',
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (plan_name, task_number)
        );

        CREATE TABLE IF NOT EXISTS plan_verdicts (
            plan_name   TEXT PRIMARY KEY,
            verdict     TEXT NOT NULL,
            reason      TEXT,
            agent_id    TEXT,
            checked_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS plan_budget (
            plan_name      TEXT PRIMARY KEY,
            max_budget_usd REAL NOT NULL,
            updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS plan_auto_advance (
            plan_name  TEXT PRIMARY KEY,
            enabled    INTEGER NOT NULL DEFAULT 0,
            parallel   INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Auto-mode: opt-in flag plus self-pause state. `enabled` is the
        -- user's toggle; `paused_reason` is set by the loop when it self-
        -- pauses (merge conflict, fix-cap reached, etc.). The actionable
        -- check is `enabled = 1 AND paused_reason IS NULL`. `parallel`
        -- is the per-plan opt-in for fan-out spawning; gated to false
        -- until worktree-per-agent isolation ships (Phase 3.5.3).
        CREATE TABLE IF NOT EXISTS plan_auto_mode (
            plan_name        TEXT PRIMARY KEY,
            enabled          INTEGER NOT NULL DEFAULT 0,
            max_fix_attempts INTEGER NOT NULL DEFAULT 3,
            parallel         INTEGER NOT NULL DEFAULT 0,
            paused_reason    TEXT,
            paused_at        TEXT
        );

        -- Per-task fix-agent attempt log. One row per fix run; the count
        -- is what enforces `plan_auto_mode.max_fix_attempts`. PK ensures
        -- the record-then-act flow is idempotent on retry.
        CREATE TABLE IF NOT EXISTS task_fix_attempts (
            plan_name   TEXT    NOT NULL,
            task_number TEXT    NOT NULL,
            attempt     INTEGER NOT NULL,
            agent_id    TEXT,
            started_at  TEXT,
            finished_at TEXT,
            outcome     TEXT,
            PRIMARY KEY (plan_name, task_number, attempt)
        );

        CREATE TABLE IF NOT EXISTS task_learnings (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_name    TEXT    NOT NULL,
            task_number  TEXT    NOT NULL,
            learning     TEXT    NOT NULL,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_learnings_plan_task ON task_learnings(plan_name, task_number);

        CREATE TABLE IF NOT EXISTS users (
            id             TEXT PRIMARY KEY,
            email          TEXT NOT NULL UNIQUE,
            password_hash  TEXT NOT NULL,
            created_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

        CREATE TABLE IF NOT EXISTS sessions (
            token       TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            expires_at  TEXT NOT NULL,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
        CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);

        CREATE TABLE IF NOT EXISTS ci_runs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_name    TEXT    NOT NULL,
            task_number  TEXT    NOT NULL,
            agent_id     TEXT,
            provider     TEXT    NOT NULL DEFAULT 'github',
            commit_sha   TEXT,
            branch       TEXT,
            run_id       TEXT,
            run_url      TEXT,
            status       TEXT    NOT NULL DEFAULT 'pending',
            conclusion   TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_ci_runs_plan_task ON ci_runs(plan_name, task_number);
        CREATE INDEX IF NOT EXISTS idx_ci_runs_status ON ci_runs(status);

        -- Multi-tenancy: organizations and membership
        CREATE TABLE IF NOT EXISTS organizations (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            slug       TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_orgs_slug ON organizations(slug);

        CREATE TABLE IF NOT EXISTS org_members (
            org_id    TEXT NOT NULL,
            user_id   TEXT NOT NULL,
            role      TEXT NOT NULL DEFAULT 'member',
            joined_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (org_id, user_id),
            FOREIGN KEY (org_id)  REFERENCES organizations(id) ON DELETE CASCADE,
            FOREIGN KEY (user_id) REFERENCES users(id)         ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_org_members_user ON org_members(user_id);

        -- Authoritative plan-to-org ownership mapping. Plans discovered
        -- on the filesystem that have no row here are treated as belonging
        -- to the default org (backward-compat).
        CREATE TABLE IF NOT EXISTS plan_org (
            plan_name TEXT PRIMARY KEY,
            org_id    TEXT NOT NULL DEFAULT 'default-org',
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_plan_org_org ON plan_org(org_id);

        -- Remote runners (SaaS foundation)
        CREATE TABLE IF NOT EXISTS runners (
            id           TEXT PRIMARY KEY,
            name         TEXT,
            org_id       TEXT NOT NULL DEFAULT 'default-org',
            status       TEXT NOT NULL DEFAULT 'offline',
            hostname     TEXT,
            version      TEXT,
            last_seen_at TEXT,
            created_at   TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_runners_org ON runners(org_id);

        CREATE TABLE IF NOT EXISTS runner_tokens (
            token_hash   TEXT PRIMARY KEY,
            runner_name  TEXT NOT NULL,
            org_id       TEXT NOT NULL DEFAULT 'default-org',
            created_by   TEXT NOT NULL,
            created_at   TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (created_by) REFERENCES users(id)
        );
        CREATE INDEX IF NOT EXISTS idx_runner_tokens_org ON runner_tokens(org_id);

        -- ── Per-org usage tracking and budgets ────────────────────────────
        CREATE TABLE IF NOT EXISTS org_budgets (
            org_id         TEXT PRIMARY KEY,
            max_budget_usd REAL NOT NULL,
            billing_period TEXT NOT NULL DEFAULT 'monthly',
            period_start   TEXT,
            updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS user_quotas (
            org_id         TEXT NOT NULL,
            user_id        TEXT NOT NULL,
            max_budget_usd REAL NOT NULL,
            updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (org_id, user_id),
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE,
            FOREIGN KEY (user_id) REFERENCES users(id)         ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS budget_alerts (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            org_id     TEXT NOT NULL,
            threshold  INTEGER NOT NULL,
            period_key TEXT NOT NULL,
            alerted_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(org_id, threshold, period_key)
        );

        CREATE TABLE IF NOT EXISTS org_kill_switch (
            org_id     TEXT PRIMARY KEY,
            active     INTEGER NOT NULL DEFAULT 0,
            reason     TEXT,
            toggled_at TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );

        -- ── Audit log ────────────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS audit_logs (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            org_id        TEXT NOT NULL DEFAULT 'default-org',
            user_id       TEXT,
            user_email    TEXT,
            action        TEXT NOT NULL,
            resource_type TEXT NOT NULL,
            resource_id   TEXT,
            diff          TEXT,
            created_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_audit_org_created ON audit_logs(org_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_audit_action ON audit_logs(action);
        CREATE INDEX IF NOT EXISTS idx_audit_resource ON audit_logs(resource_type, resource_id);

        -- ── SSO (SAML/OIDC) ─────────────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS sso_providers (
            id              TEXT PRIMARY KEY,
            org_id          TEXT NOT NULL,
            protocol        TEXT NOT NULL CHECK(protocol IN ('oidc', 'saml')),
            name            TEXT NOT NULL,
            enabled         INTEGER NOT NULL DEFAULT 1,
            email_domains   TEXT,
            issuer_url      TEXT,
            client_id       TEXT,
            client_secret   TEXT,
            idp_entity_id   TEXT,
            idp_sso_url     TEXT,
            idp_certificate TEXT,
            sp_entity_id    TEXT,
            groups_claim    TEXT DEFAULT 'groups',
            group_role_mapping TEXT,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sso_providers_org ON sso_providers(org_id);

        CREATE TABLE IF NOT EXISTS sso_accounts (
            id          TEXT PRIMARY KEY,
            user_id     TEXT NOT NULL,
            provider_id TEXT NOT NULL,
            external_id TEXT NOT NULL,
            email       TEXT NOT NULL,
            groups      TEXT,
            last_login_at TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
            FOREIGN KEY (provider_id) REFERENCES sso_providers(id) ON DELETE CASCADE,
            UNIQUE (provider_id, external_id)
        );
        CREATE INDEX IF NOT EXISTS idx_sso_accounts_user ON sso_accounts(user_id);

        CREATE TABLE IF NOT EXISTS sso_auth_state (
            state       TEXT PRIMARY KEY,
            provider_id TEXT NOT NULL,
            pkce_verifier TEXT,
            nonce       TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );
        ",
    )
    .expect("failed to run schema migration");

    // Server-side outbox + seq tracker for runner communication.
    crate::saas::outbox::init_server_inbox(conn);
    crate::saas::outbox::init_seq_tracker(conn);

    // Add columns for existing databases
    conn.execute_batch("ALTER TABLE agents ADD COLUMN base_commit TEXT;")
        .ok(); // ignore error if column already exists
    conn.execute_batch("ALTER TABLE agents ADD COLUMN branch TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE agents ADD COLUMN source_branch TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE agents ADD COLUMN cost_usd REAL;")
        .ok();
    // Path to the session-daemon's local socket / named pipe. NULL for legacy
    // rows written before the tmux → supervisor switch; those are treated as
    // `detached` on first boot post-upgrade.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN supervisor_socket TEXT;")
        .ok();
    // Name of the AgentDriver that spawned this agent (e.g. "claude").
    // NULL on rows written before driver selection existed; readers treat
    // NULL as the default driver.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN driver TEXT DEFAULT 'claude';")
        .ok();
    // Free-form tag explaining why an agent stopped: 'completed', 'killed',
    // 'orphaned' (reconciled on startup, daemon dead), 'supervisor_unreachable'
    // (heartbeat timeout). NULL while the agent is still live. Used for
    // debugging and rendered as a hover-label on the task card.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN stop_reason TEXT;")
        .ok();
    // Cached `gh run view --log-failed` output for a failed CI run. Populated
    // lazily by the failure-log endpoint; bounded at ~8 KB to keep prompts
    // tight when we pass it to a fix-CI agent.
    conn.execute_batch("ALTER TABLE ci_runs ADD COLUMN failure_log TEXT;")
        .ok();
    // Soft-delete marker for CI runs the user dismissed from the dashboard.
    // `latest_per_task` filters rows with non-NULL `dismissed_at` so a stuck
    // red badge can be cleared without affecting the underlying GitHub
    // pipeline or future runs for the same commit.
    conn.execute_batch("ALTER TABLE ci_runs ADD COLUMN dismissed_at TEXT;")
        .ok();

    // ── Multi-tenancy: org_id on every data table ───────────────────────────
    // DEFAULT 'default-org' means pre-existing rows automatically belong to
    // the default org. New rows inserted by org-aware code pass the real
    // org_id explicitly.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE hook_events ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE plan_project ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE task_status ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE task_learnings ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE plan_verdicts ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch("ALTER TABLE plan_budget ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();
    conn.execute_batch(
        "ALTER TABLE plan_auto_advance ADD COLUMN org_id TEXT DEFAULT 'default-org';",
    )
    .ok();
    conn.execute_batch("ALTER TABLE ci_runs ADD COLUMN org_id TEXT DEFAULT 'default-org';")
        .ok();

    // ── Per-org usage tracking ────────────────────────────────────────────
    // Track which user spawned each agent for per-user cost allocation.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN user_id TEXT;")
        .ok();

    // Distinguishes auto-inferred status rows ('auto') from explicit user or
    // agent updates ('manual'). NULL on rows written before this column
    // existed — treated as overwritable by auto_status alongside 'auto' rows
    // so a one-time conservative re-run can correct legacy false positives.
    // Only source='manual' is sticky against re-inference.
    conn.execute_batch("ALTER TABLE task_status ADD COLUMN source TEXT DEFAULT NULL;")
        .ok();

    // Per-plan opt-in for parallel spawn. Default 0 (off) — until
    // worktree-per-agent isolation ships, the spawn loop unconditionally
    // breaks after the first claim (Phase 3.5.1). 3.5.3 will reject toggle
    // attempts to true at the API layer until worktrees land. Stored on
    // both tables so each mode (auto-advance / auto-mode) carries its own
    // knob; the unified config endpoint keeps them in lockstep.
    conn.execute_batch(
        "ALTER TABLE plan_auto_mode ADD COLUMN parallel INTEGER NOT NULL DEFAULT 0;",
    )
    .ok();
    conn.execute_batch(
        "ALTER TABLE plan_auto_advance ADD COLUMN parallel INTEGER NOT NULL DEFAULT 0;",
    )
    .ok();

    // Seed the default org and migrate orphaned users/plans into it.
    crate::auth::orgs::ensure_default_org(conn);

    // Clean up legacy bulk auto-inferred "completed" rows. Naturally
    // idempotent: post-Task-2.2, no new row can satisfy the predicate.
    cleanup_stale_auto_completed(conn);
}

/// Delete `task_status` rows for plans whose entire row set is legacy
/// auto-inferred completions (`status='completed'` AND `source IS NULL`)
/// AND no agent has ever been spawned for the plan. These are the bulk
/// false positives produced by the pre-Task-2.1 `infer_status` heuristic
/// (≥80% file existence ⇒ completed) and never corrected by a real agent
/// or user action.
///
/// Safety: rows with `source='manual'` (explicit user/agent updates) and
/// rows for plans with any agent activity are left untouched. After the
/// rows are deleted, `done_count` collapses to 0 and the plan reverts to
/// the active section of the navbar; the user can re-run auto-status to
/// re-derive `pending` / `in_progress` (capped per Task 2.1).
///
/// Plans with agents but a stuck completed status (e.g. portable-agents-
/// and-mcp) are out of scope here — the user resets them explicitly via
/// `POST /api/plans/:name/reset-status`.
fn cleanup_stale_auto_completed(conn: &Connection) {
    let candidates: Vec<String> = match conn.prepare(
        "SELECT plan_name
           FROM task_status
          GROUP BY plan_name
         HAVING COUNT(*) > 0
            AND SUM(CASE WHEN status = 'completed' AND source IS NULL THEN 1 ELSE 0 END) = COUNT(*)
            AND plan_name NOT IN (
                SELECT DISTINCT plan_name FROM agents WHERE plan_name IS NOT NULL
            )",
    ) {
        Ok(mut stmt) => stmt
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default(),
        Err(_) => return,
    };

    for plan in &candidates {
        match conn.execute(
            "DELETE FROM task_status WHERE plan_name = ?1",
            params![plan],
        ) {
            Ok(n) if n > 0 => {
                eprintln!(
                    "task_status cleanup: purged {n} stale auto-inferred completed row(s) for plan '{plan}'"
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (Db, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = init(&path);
        (db, dir)
    }

    #[test]
    fn creates_all_tables() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(tables.contains(&"hook_events".to_string()));
        assert!(tables.contains(&"agents".to_string()));
        assert!(tables.contains(&"agent_output".to_string()));
        assert!(tables.contains(&"plan_project".to_string()));
        assert!(tables.contains(&"task_status".to_string()));
        assert!(tables.contains(&"task_learnings".to_string()));
        assert!(tables.contains(&"users".to_string()));
        assert!(tables.contains(&"sessions".to_string()));
        assert!(tables.contains(&"plan_verdicts".to_string()));
        assert!(tables.contains(&"plan_auto_mode".to_string()));
        assert!(tables.contains(&"task_fix_attempts".to_string()));
        assert!(tables.contains(&"organizations".to_string()));
        assert!(tables.contains(&"org_members".to_string()));
        assert!(tables.contains(&"plan_org".to_string()));
        assert!(tables.contains(&"audit_logs".to_string()));
    }

    #[test]
    fn insert_and_replace_plan_verdict() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO plan_verdicts (plan_name, verdict, reason, agent_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(plan_name) DO UPDATE SET
               verdict = excluded.verdict,
               reason = excluded.reason,
               agent_id = excluded.agent_id,
               checked_at = datetime('now')",
            params!["p1", "in_progress", "halfway", "agent-a"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plan_verdicts (plan_name, verdict, reason, agent_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(plan_name) DO UPDATE SET
               verdict = excluded.verdict,
               reason = excluded.reason,
               agent_id = excluded.agent_id,
               checked_at = datetime('now')",
            params!["p1", "completed", "all done", "agent-b"],
        )
        .unwrap();

        let (verdict, reason, agent_id): (String, String, String) = conn
            .query_row(
                "SELECT verdict, reason, agent_id FROM plan_verdicts WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(verdict, "completed");
        assert_eq!(reason, "all done");
        assert_eq!(agent_id, "agent-b");
    }

    #[test]
    fn task_learnings_round_trip() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.1", "first learning"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.1", "second learning"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_learnings (plan_name, task_number, learning) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.2", "other task learning"],
        )
        .unwrap();

        let ls = task_learnings(&conn, "plan-a", "1.1");
        // Most-recent first.
        assert_eq!(ls, vec!["second learning", "first learning"]);

        assert_eq!(
            task_learnings(&conn, "plan-a", "1.2"),
            vec!["other task learning"]
        );
        assert!(task_learnings(&conn, "plan-a", "9.9").is_empty());
    }

    #[test]
    fn idempotent_migration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        // Run init twice — should not panic
        let _db1 = init(&path);
        let _db2 = init(&path);
    }

    #[test]
    fn insert_and_query_hook_event() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO hook_events (session_id, hook_type, tool_name) VALUES (?1, ?2, ?3)",
            params!["sess-1", "PostToolUse", "Bash"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM hook_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn completed_task_numbers_gate() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('p1', '1.1', 'completed'),
               ('p1', '1.2', 'skipped'),
               ('p1', '1.3', 'in_progress'),
               ('p1', '1.4', 'pending'),
               ('p2', '1.1', 'completed');",
        )
        .unwrap();

        let done = completed_task_numbers(&conn, "p1");
        assert!(done.contains("1.1"));
        assert!(done.contains("1.2"));
        assert!(!done.contains("1.3"));
        assert!(!done.contains("1.4"));
        assert_eq!(done.len(), 2);

        let empty = completed_task_numbers(&conn, "nonexistent");
        assert!(empty.is_empty());
    }

    #[test]
    fn insert_and_query_task_status() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, ?3)",
            params!["my-plan", "1.1", "completed"],
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM task_status WHERE plan_name = ?1 AND task_number = ?2",
                params!["my-plan", "1.1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn task_status_source_column_defaults_null_and_round_trips() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Pre-source-column legacy write: source defaults to NULL.
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES (?1, ?2, ?3)",
            params!["plan-a", "1.1", "completed"],
        )
        .unwrap();
        let legacy: Option<String> = conn
            .query_row(
                "SELECT source FROM task_status WHERE plan_name=?1 AND task_number=?2",
                params!["plan-a", "1.1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy, None);

        // 'auto' and 'manual' values round-trip.
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, source) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["plan-a", "1.2", "in_progress", "auto"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_status (plan_name, task_number, status, source) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["plan-a", "1.3", "completed", "manual"],
        )
        .unwrap();
        let auto: Option<String> = conn
            .query_row(
                "SELECT source FROM task_status WHERE plan_name=?1 AND task_number=?2",
                params!["plan-a", "1.2"],
                |row| row.get(0),
            )
            .unwrap();
        let manual: Option<String> = conn
            .query_row(
                "SELECT source FROM task_status WHERE plan_name=?1 AND task_number=?2",
                params!["plan-a", "1.3"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(auto.as_deref(), Some("auto"));
        assert_eq!(manual.as_deref(), Some("manual"));
    }

    #[test]
    fn agents_table_has_driver_column() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, driver) VALUES (?1, ?2, ?3, ?4)",
            params!["a1", "/tmp", "running", "claude"],
        )
        .unwrap();
        let drv: Option<String> = conn
            .query_row(
                "SELECT driver FROM agents WHERE id = ?1",
                params!["a1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(drv.as_deref(), Some("claude"));

        // Default when not specified
        conn.execute(
            "INSERT INTO agents (id, cwd, status) VALUES (?1, ?2, ?3)",
            params!["a2", "/tmp", "running"],
        )
        .unwrap();
        let drv2: Option<String> = conn
            .query_row(
                "SELECT driver FROM agents WHERE id = ?1",
                params!["a2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(drv2.as_deref(), Some("claude"));
    }

    #[test]
    fn agents_table_has_supervisor_socket_column() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, supervisor_socket) VALUES (?1, ?2, ?3, ?4)",
            params!["a1", "/tmp", "running", "/tmp/a1.sock"],
        )
        .unwrap();
        let sock: Option<String> = conn
            .query_row(
                "SELECT supervisor_socket FROM agents WHERE id = ?1",
                params!["a1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sock.as_deref(), Some("/tmp/a1.sock"));
    }

    #[test]
    fn insert_agent_with_parent() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO agents (id, cwd, status) VALUES (?1, ?2, ?3)",
            params!["agent-1", "/tmp", "running"],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO agents (id, cwd, status, parent_agent_id) VALUES (?1, ?2, ?3, ?4)",
            params!["agent-2", "/tmp", "running", "agent-1"],
        )
        .unwrap();

        let parent: String = conn
            .query_row(
                "SELECT parent_agent_id FROM agents WHERE id = ?1",
                params!["agent-2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, "agent-1");
    }

    #[test]
    fn wal_mode_enabled() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn db_path_created_if_missing() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a").join("b").join("c").join("test.db");
        let _db = init(&nested);
        assert!(nested.exists());
    }

    /// Re-run `cleanup_stale_auto_completed` (which already ran inside
    /// `init` against an empty DB) after seeding. The function is meant to
    /// be naturally idempotent; the test covers the seeded scenarios.
    fn run_cleanup(conn: &Connection) {
        super::cleanup_stale_auto_completed(conn);
    }

    #[test]
    fn cleanup_purges_legacy_all_completed_no_agents_plan() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Legacy bulk-auto-inferred plan: every row completed with NULL source,
        // no agent ever spawned. This is the prototypical false positive
        // (e.g. the `scheduler` plan in production).
        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('scheduler', '1.1', 'completed'),
               ('scheduler', '1.2', 'completed'),
               ('scheduler', '1.3', 'completed');",
        )
        .unwrap();

        run_cleanup(&conn);

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'scheduler'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "scheduler rows should have been purged");
    }

    #[test]
    fn cleanup_leaves_plans_with_agents_alone() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Plan has an agent row → the all-completed status might be the result
        // of real work (or manual correction predating the source column).
        // Conservative rule: leave it alone. The user can reset explicitly.
        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('portable-agents-and-mcp', '0.1', 'completed'),
               ('portable-agents-and-mcp', '0.2', 'completed'),
               ('portable-agents-and-mcp', '1.1', 'completed');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (id, cwd, status, plan_name, task_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "agent-real-1",
                "/tmp",
                "completed",
                "portable-agents-and-mcp",
                "0.1"
            ],
        )
        .unwrap();

        run_cleanup(&conn);

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'portable-agents-and-mcp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 3, "agent-having plan must not be purged");
    }

    #[test]
    fn cleanup_leaves_mixed_status_plans_alone() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Even with no agents, a mixed-status plan signals deliberate work
        // in flight — never purge.
        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('half-done', '1.1', 'completed'),
               ('half-done', '1.2', 'in_progress'),
               ('half-done', '1.3', 'pending');",
        )
        .unwrap();

        run_cleanup(&conn);

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'half-done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 3, "mixed-status plan must not be purged");
    }

    #[test]
    fn cleanup_leaves_manual_completed_rows_alone() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Post-Task-2.2 manual completions carry source='manual' — must
        // never be purged, even when no agent was ever spawned (the user
        // could have set status by hand via PUT or MCP).
        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status, source) VALUES
               ('hand-marked', '1.1', 'completed', 'manual'),
               ('hand-marked', '1.2', 'completed', 'manual');",
        )
        .unwrap();

        run_cleanup(&conn);

        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'hand-marked'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 2, "manual rows must survive cleanup");
    }

    #[test]
    fn cleanup_only_purges_qualifying_plans() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        // Two plans in the DB: one qualifies, one doesn't. Cleanup must
        // touch only the qualifying one.
        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('purge-me', '1.1', 'completed'),
               ('purge-me', '1.2', 'completed'),
               ('keep-me', '1.1', 'completed'),
               ('keep-me', '1.2', 'pending');",
        )
        .unwrap();

        run_cleanup(&conn);

        let purged: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'purge-me'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'keep-me'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(purged, 0);
        assert_eq!(kept, 2);
    }

    #[test]
    fn cleanup_is_idempotent() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute_batch(
            "INSERT INTO task_status (plan_name, task_number, status) VALUES
               ('scheduler', '1.1', 'completed');",
        )
        .unwrap();

        run_cleanup(&conn);
        run_cleanup(&conn);
        run_cleanup(&conn);

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_status", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    // ── plan_auto_mode helpers ──────────────────────────────────────────

    #[test]
    fn auto_mode_default_off() {
        let (db, _dir) = test_db();
        assert!(!auto_mode_enabled(&db, "p1"));
    }

    #[test]
    fn auto_mode_enabled_after_opt_in() {
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(auto_mode_enabled(&db, "p1"));
        assert!(!auto_mode_enabled(&db, "p2"));
    }

    #[test]
    fn auto_mode_disabled_explicit_zero() {
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 0)",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(!auto_mode_enabled(&db, "p1"));
    }

    #[test]
    fn auto_mode_pause_blocks_enabled() {
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["p1"],
            )
            .unwrap();
        }
        assert!(auto_mode_enabled(&db, "p1"));

        auto_mode_pause(&db, "p1", "merge_conflict");
        assert!(
            !auto_mode_enabled(&db, "p1"),
            "paused plan must report not-enabled"
        );

        // Inspect the row directly: paused_reason and paused_at landed,
        // enabled is preserved.
        let conn = db.lock().unwrap();
        let (enabled, reason, paused_at): (i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT enabled, paused_reason, paused_at \
                 FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(enabled, 1, "pause must not flip enabled");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
        assert!(paused_at.is_some(), "paused_at must be set");
    }

    #[test]
    fn auto_mode_resume_clears_pause_state() {
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["p1"],
            )
            .unwrap();
        }

        auto_mode_pause(&db, "p1", "fix_cap_reached");
        assert!(!auto_mode_enabled(&db, "p1"));

        auto_mode_resume(&db, "p1");
        assert!(
            auto_mode_enabled(&db, "p1"),
            "resume must restore acting state"
        );

        let conn = db.lock().unwrap();
        let (reason, paused_at): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT paused_reason, paused_at \
                 FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, None);
        assert_eq!(paused_at, None);
    }

    #[test]
    fn auto_mode_pause_creates_row_when_missing() {
        // Defensive: if the loop pauses before the user toggled (or after
        // the row was deleted), the UPSERT must still record the reason.
        let (db, _dir) = test_db();
        auto_mode_pause(&db, "p1", "merge_conflict");

        let conn = db.lock().unwrap();
        let (enabled, reason): (i64, Option<String>) = conn
            .query_row(
                "SELECT enabled, paused_reason \
                 FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(enabled, 0, "default-on-insert is 0; user has not opted in");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
    }

    #[test]
    fn auto_mode_resume_no_op_when_missing() {
        let (db, _dir) = test_db();
        auto_mode_resume(&db, "p1");

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "resume must not create a row");
    }

    // ── task_fix_attempts helpers ───────────────────────────────────────

    #[test]
    fn fix_attempt_count_zero_for_no_rows() {
        let (db, _dir) = test_db();
        assert_eq!(task_fix_attempt_count(&db, "p1", "1.1"), 0);
    }

    #[test]
    fn record_fix_attempt_and_count() {
        let (db, _dir) = test_db();

        record_fix_attempt(&db, "p1", "1.1", 1, "agent-a");
        assert_eq!(task_fix_attempt_count(&db, "p1", "1.1"), 1);

        record_fix_attempt(&db, "p1", "1.1", 2, "agent-b");
        record_fix_attempt(&db, "p1", "1.1", 3, "agent-c");
        assert_eq!(task_fix_attempt_count(&db, "p1", "1.1"), 3);

        // Count is scoped per (plan, task).
        assert_eq!(task_fix_attempt_count(&db, "p1", "1.2"), 0);
        assert_eq!(task_fix_attempt_count(&db, "p2", "1.1"), 0);
    }

    #[test]
    fn record_fix_attempt_idempotent_on_pk_conflict() {
        let (db, _dir) = test_db();

        record_fix_attempt(&db, "p1", "1.1", 1, "agent-a");
        // Second insert with the same triple is a no-op; original
        // started_at and agent_id survive.
        record_fix_attempt(&db, "p1", "1.1", 1, "agent-different");
        assert_eq!(task_fix_attempt_count(&db, "p1", "1.1"), 1);

        let conn = db.lock().unwrap();
        let agent: String = conn
            .query_row(
                "SELECT agent_id FROM task_fix_attempts \
                 WHERE plan_name = ?1 AND task_number = ?2 AND attempt = ?3",
                params!["p1", "1.1", 1i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent, "agent-a", "first writer wins");
    }

    #[test]
    fn record_fix_attempt_persists_started_at() {
        let (db, _dir) = test_db();
        record_fix_attempt(&db, "p1", "1.1", 1, "agent-a");

        let conn = db.lock().unwrap();
        let (started_at, finished_at, outcome): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT started_at, finished_at, outcome FROM task_fix_attempts \
                 WHERE plan_name = ?1 AND task_number = ?2 AND attempt = ?3",
                params!["p1", "1.1", 1i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(started_at.is_some(), "started_at must be set on insert");
        assert_eq!(finished_at, None, "finished_at stays NULL until close-out");
        assert_eq!(outcome, None, "outcome stays NULL until close-out");
    }

    #[test]
    fn plan_auto_mode_default_max_fix_attempts_is_three() {
        // The default value is the policy ceiling; the loop reads it
        // when deciding whether to spawn another fix agent. Pin it so
        // future schema edits notice the implicit contract.
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
            params!["p1"],
        )
        .unwrap();
        let max: i64 = conn
            .query_row(
                "SELECT max_fix_attempts FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max, 3);
    }

    #[test]
    fn parallel_default_is_false_for_new_and_missing_rows() {
        let (db, _dir) = test_db();
        // No row at all: helpers fall back to schema defaults.
        let am = auto_mode_config(&db, "missing");
        assert!(!am.parallel, "missing plan_auto_mode row defaults to false");
        let aa = auto_advance_config(&db, "missing");
        assert!(
            !aa.parallel,
            "missing plan_auto_advance row defaults to false"
        );

        // Row exists, parallel column omitted from INSERT: NOT NULL DEFAULT 0
        // takes hold for both fresh and migrated rows.
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
            params!["p1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled) VALUES (?1, 1)",
            params!["p1"],
        )
        .unwrap();
        drop(conn);

        let am = auto_mode_config(&db, "p1");
        assert!(am.enabled);
        assert!(!am.parallel, "fresh row defaults parallel to false");
        let aa = auto_advance_config(&db, "p1");
        assert!(aa.enabled);
        assert!(!aa.parallel, "fresh row defaults parallel to false");
    }

    #[test]
    fn parallel_round_trips_through_config_helpers() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO plan_auto_mode (plan_name, enabled, parallel) VALUES (?1, 1, 1)",
            params!["p1"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plan_auto_advance (plan_name, enabled, parallel) VALUES (?1, 1, 1)",
            params!["p1"],
        )
        .unwrap();
        drop(conn);

        let am = auto_mode_config(&db, "p1");
        assert!(am.parallel);
        let aa = auto_advance_config(&db, "p1");
        assert!(aa.parallel);
    }

    #[test]
    fn parallel_migration_is_idempotent_and_preserves_existing_rows() {
        // Acceptance: re-running `init` on a DB that already carries the
        // parallel column must not error out, drop rows, or reset the
        // value. The ALTER TABLE is wrapped in `.ok()` so the second run
        // becomes a no-op.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");

        {
            let db = init(&path);
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled, parallel) VALUES (?1, 1, 1)",
                params!["plan-a"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plan_auto_advance (plan_name, enabled, parallel) VALUES (?1, 1, 1)",
                params!["plan-a"],
            )
            .unwrap();
        }

        let db2 = init(&path);
        let am = auto_mode_config(&db2, "plan-a");
        assert!(am.parallel, "user-set parallel survives re-init");
        let aa = auto_advance_config(&db2, "plan-a");
        assert!(aa.parallel, "user-set parallel survives re-init");
    }

    #[test]
    fn plan_auto_mode_migration_preserves_data_across_init() {
        // Acceptance: migrations apply on an existing DB without
        // dropping data. Seed both new tables, re-init, observe rows
        // survive.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");

        {
            let db = init(&path);
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled, max_fix_attempts) \
                 VALUES (?1, 1, 5)",
                params!["plan-a"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_fix_attempts \
                   (plan_name, task_number, attempt, agent_id, started_at) \
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                params!["plan-a", "1.1", 1i64, "agent-a"],
            )
            .unwrap();
        }

        // Re-init: idempotent migration must not drop seeded rows.
        let db2 = init(&path);
        assert!(auto_mode_enabled(&db2, "plan-a"));
        assert_eq!(task_fix_attempt_count(&db2, "plan-a", "1.1"), 1);
        let conn = db2.lock().unwrap();
        let max: i64 = conn
            .query_row(
                "SELECT max_fix_attempts FROM plan_auto_mode WHERE plan_name = ?1",
                params!["plan-a"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max, 5, "user-set max_fix_attempts survives re-init");
    }
}
