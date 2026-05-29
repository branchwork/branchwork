use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use crate::repo_config::MergeCadence;

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
///
/// `files` carries the trimmed dirty-tree file list that produced the
/// pause (only set for `agent_left_uncommitted_work` paths today). When
/// `Some`, it is JSON-encoded and stored in `paused_files`; when `None`,
/// the column is cleared so a downstream pause from a different cause
/// can't leak a stale file list into the dashboard. The "5-file trim"
/// lives at the call site (matches the broadcast payload); this helper
/// stores whatever it's handed.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn auto_mode_pause(db: &Db, plan_name: &str, reason: &str, files: Option<&[String]>) {
    let files_json = files.map(|f| serde_json::to_string(f).unwrap_or_else(|_| "[]".to_string()));
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, paused_reason, paused_at, paused_files) \
         VALUES (?1, ?2, datetime('now'), ?3) \
         ON CONFLICT(plan_name) DO UPDATE SET \
           paused_reason = excluded.paused_reason, \
           paused_at = excluded.paused_at, \
           paused_files = excluded.paused_files",
        params![plan_name, reason, files_json],
    )
    .ok();
}

/// Clear `paused_reason` / `paused_at` / `paused_files` for `plan_name`.
/// No-op if no row exists — there is nothing to unpause.
#[allow(dead_code)] // wired in by later auto-mode-loop tasks
pub fn auto_mode_resume(db: &Db, plan_name: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE plan_auto_mode \
         SET paused_reason = NULL, paused_at = NULL, paused_files = NULL \
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
    /// Dirty-tree file list captured at pause time. `None` when the row
    /// has no pause context or the pause reason was non-dirty-tree (e.g.
    /// `merge_conflict`, `ci_failed`). The list is whatever the caller of
    /// [`auto_mode_pause`] handed in — trimmed at the call site to match
    /// the broadcast payload (5 files today).
    pub paused_files: Option<Vec<String>>,
    /// Per-plan merge-cadence override. `None` means "inherit the project
    /// default" (the `[auto_mode] merge_cadence` value from the project's
    /// `branchwork.toml`, which itself defaults to `phase`). `Some(_)` is
    /// an explicit plan-level pin (`task` / `phase` / `plan`).
    pub merge_cadence: Option<MergeCadence>,
}

/// Read the full `plan_auto_mode` row for `plan_name`. Defaults to
/// `enabled=false`, `max_fix_attempts=3`, `paused_reason=None`,
/// `parallel=false`, `paused_files=None`, `merge_cadence=None` when no
/// row exists.
#[allow(dead_code)] // wired in by 3.5.3 worktrees gate + later loop callers
pub fn auto_mode_config(db: &Db, plan_name: &str) -> AutoModeConfig {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT enabled, max_fix_attempts, paused_reason, parallel, paused_files, merge_cadence \
         FROM plan_auto_mode WHERE plan_name = ?1",
        params![plan_name],
        |row| {
            Ok(AutoModeConfig {
                enabled: row.get::<_, i64>(0)? != 0,
                max_fix_attempts: row.get::<_, i64>(1)? as u32,
                paused_reason: row.get::<_, Option<String>>(2)?,
                parallel: row.get::<_, i64>(3)? != 0,
                paused_files: row
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()),
                merge_cadence: row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| parse_merge_cadence(&s)),
            })
        },
    )
    .unwrap_or(AutoModeConfig {
        enabled: false,
        max_fix_attempts: 3,
        paused_reason: None,
        parallel: false,
        paused_files: None,
        merge_cadence: None,
    })
}

/// Parse a stored merge-cadence string (`'task'` / `'phase'` / `'plan'`)
/// into the typed enum. Unknown values collapse to `None` so a corrupted
/// row (manual SQL UPDATE, future schema reshuffle) silently inherits
/// the project default rather than 500ing the config endpoint. Mirrors
/// the lenient parse the runner-failover column uses.
pub fn parse_merge_cadence(s: &str) -> Option<MergeCadence> {
    match s {
        "task" => Some(MergeCadence::Task),
        "phase" => Some(MergeCadence::Phase),
        "plan" => Some(MergeCadence::Plan),
        _ => None,
    }
}

/// Wire form of [`MergeCadence`]: lowercase variant name matching the
/// `[auto_mode] merge_cadence` TOML serialisation and the DB column.
/// Kept in lockstep with [`parse_merge_cadence`].
pub fn merge_cadence_wire(c: MergeCadence) -> &'static str {
    match c {
        MergeCadence::Task => "task",
        MergeCadence::Phase => "phase",
        MergeCadence::Plan => "plan",
    }
}

/// Read the per-plan merge-cadence override. Returns `None` when the
/// row carries no explicit cadence ("inherit project default"). Returns
/// `Some(_)` when an explicit cadence has been written.
#[allow(dead_code)] // wired in by the auto-mode loop in a later task
pub fn plan_merge_cadence(db: &Db, plan_name: &str) -> Option<MergeCadence> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT merge_cadence FROM plan_auto_mode WHERE plan_name = ?1",
        params![plan_name],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .and_then(|s| parse_merge_cadence(&s))
}

/// Persist (or clear) the per-plan merge-cadence override. `Some(_)`
/// UPSERTs the explicit cadence; `None` clears the column back to NULL
/// (inherit the project default). Other columns on the row are left
/// untouched on conflict — partial updates do not clobber sibling
/// settings (matches the partial-update pattern in `put_plan_config`).
#[allow(dead_code)] // wired in by `put_plan_settings` in this same task
pub fn set_plan_merge_cadence(db: &Db, plan_name: &str, cadence: Option<MergeCadence>) {
    let wire: Option<&'static str> = cadence.map(merge_cadence_wire);
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO plan_auto_mode (plan_name, merge_cadence) \
         VALUES (?1, ?2) \
         ON CONFLICT(plan_name) DO UPDATE SET merge_cadence = excluded.merge_cadence",
        params![plan_name, wire],
    )
    .ok();
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

/// Per-runner override of server-wide settings. Both fields are
/// `Option<_>`: `None` means inherit the server-wide default. The dispatch
/// layer resolves override-or-inherit at StartAgent build time and ships
/// the resolved values; the runner does not re-resolve.
#[derive(Debug, Clone, Default)]
pub struct RunnerConfig {
    pub effort: Option<String>,
    pub skip_permissions: Option<bool>,
}

/// Read the per-runner override row. Returns the all-`None` default when
/// no row exists — i.e. the runner inherits both server-wide settings.
#[allow(dead_code)] // wired in by the SaaS dispatch path
pub fn runner_config(db: &Db, runner_id: &str) -> RunnerConfig {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT effort, skip_permissions FROM runner_config WHERE runner_id = ?1",
        params![runner_id],
        |row| {
            Ok(RunnerConfig {
                effort: row.get::<_, Option<String>>(0)?,
                skip_permissions: row.get::<_, Option<i64>>(1)?.map(|v| v != 0),
            })
        },
    )
    .unwrap_or_default()
}

/// Persist a per-runner override. Either field set to `None` clears that
/// override; passing `RunnerConfig::default()` collapses the row to "inherit
/// everything". UPSERT keyed by `runner_id` so the call is idempotent.
#[allow(dead_code)] // wired in by api::runners::put_runner_config
pub fn set_runner_config(db: &Db, runner_id: &str, org_id: &str, cfg: &RunnerConfig) {
    let conn = db.lock().unwrap();
    let skip_int: Option<i64> = cfg.skip_permissions.map(|b| if b { 1 } else { 0 });
    conn.execute(
        "INSERT INTO runner_config (runner_id, effort, skip_permissions, org_id) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(runner_id) DO UPDATE SET \
            effort = excluded.effort, \
            skip_permissions = excluded.skip_permissions, \
            org_id = excluded.org_id",
        params![runner_id, cfg.effort, skip_int, org_id],
    )
    .ok();
}

/// Read the per-plan runner affinity. Returns `Some(runner_id)` when the
/// plan is pinned to a specific runner, or `None` when no row exists (the
/// historic "any online runner" behaviour). Wired into
/// `spawn_ops::start_agent_via_runner` (T11.4) so every agent spawn for
/// the plan honours the pin.
pub fn plan_runner_id(db: &Db, plan_name: &str) -> Option<String> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT runner_id FROM plan_runner_affinity WHERE plan_name = ?1",
        params![plan_name],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Persist (or clear) the per-plan runner affinity. `runner_id = None`
/// deletes the row (back to "any online runner"); `Some` UPSERTs it.
/// Idempotent. The UPSERT path preserves any existing `runner_failover`
/// policy (T11.5) — only the `runner_id` and `updated_at` columns are
/// touched on conflict, so re-pinning a plan that already had
/// failover='sibling' keeps that policy in place.
pub fn set_plan_runner_id(db: &Db, plan_name: &str, org_id: &str, runner_id: Option<&str>) {
    let conn = db.lock().unwrap();
    match runner_id {
        Some(rid) => {
            conn.execute(
                "INSERT INTO plan_runner_affinity (plan_name, runner_id, org_id, updated_at) \
                 VALUES (?1, ?2, ?3, datetime('now')) \
                 ON CONFLICT(plan_name) DO UPDATE SET \
                    runner_id = excluded.runner_id, \
                    updated_at = excluded.updated_at",
                params![plan_name, rid, org_id],
            )
            .ok();
        }
        None => {
            conn.execute(
                "DELETE FROM plan_runner_affinity WHERE plan_name = ?1",
                params![plan_name],
            )
            .ok();
        }
    }
}

/// Read the per-plan runner failover policy (T11.5). Returns `"pause"` for
/// plans without a `plan_runner_affinity` row (the default for unpinned
/// plans where failover doesn't apply anyway), the column value for
/// pinned plans, or `"pause"` on a malformed value to fail safe.
pub fn plan_runner_failover(db: &Db, plan_name: &str) -> String {
    let conn = db.lock().unwrap();
    let raw: String = conn
        .query_row(
            "SELECT runner_failover FROM plan_runner_affinity WHERE plan_name = ?1",
            params![plan_name],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "pause".to_string());
    match raw.as_str() {
        "pause" | "sibling" => raw,
        _ => "pause".to_string(),
    }
}

/// Persist the per-plan runner failover policy (T11.5). Only valid values
/// are `"pause"` (today's behaviour) and `"sibling"` (re-dispatch on
/// offline). Returns `Ok(true)` when an existing pin row was updated,
/// `Ok(false)` when no pin row existed (failover is a no-op without a
/// pin to fail over from — caller should reject the request at the API
/// layer), or `Err(())` when `policy` is not one of the two valid values.
pub fn set_plan_runner_failover(db: &Db, plan_name: &str, policy: &str) -> Result<bool, ()> {
    if policy != "pause" && policy != "sibling" {
        return Err(());
    }
    let conn = db.lock().unwrap();
    let updated = conn
        .execute(
            "UPDATE plan_runner_affinity SET runner_failover = ?2, updated_at = datetime('now') \
             WHERE plan_name = ?1",
            params![plan_name, policy],
        )
        .unwrap_or(0);
    Ok(updated > 0)
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

// ── Pending-learning gate (Phase 1, Task 1.2) ───────────────────────────────

/// A `ci_failure_events` row that is still pending a learning capture.
/// Returned by [`pending_ci_failures`] and embedded in the 409 response
/// payload of [`crate::api::plans::set_task_status`] and the equivalent
/// `McpError` raised by `mcp::tools::status::update_task_status`.
///
/// Wire-shape: camelCase serialization matches the rest of the HTTP API
/// (`runId`, `runUrl`, `observedAt`). Fields are intentionally a strict
/// subset of `ci_failure_events` — `agent_id`, `branch`, and `org_id` are
/// authoring metadata the caller already knows from context.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingCiFailure {
    pub id: i64,
    pub run_id: String,
    pub run_url: Option<String>,
    pub workflow: Option<String>,
    pub conclusion: Option<String>,
    pub summary: Option<String>,
    pub observed_at: String,
}

/// List every `ci_failure_events` row for `(plan_name, task_number)` that
/// has not yet been resolved by a captured learning (`resolved_at IS
/// NULL`). The partial index `idx_ci_failure_events_pending` is the
/// hot-path lookup; the surrounding sort keeps the response stable so a
/// dashboard rendering the failures list does not jitter on poll.
///
/// Returns an empty Vec when the task has no pending failures — that's
/// the signal that the `completed` gate may proceed.
pub fn pending_ci_failures(db: &Db, plan_name: &str, task_number: &str) -> Vec<PendingCiFailure> {
    let conn = db.lock().unwrap();
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, run_id, run_url, workflow, conclusion, summary, observed_at \
           FROM ci_failure_events \
          WHERE plan_name = ?1 \
            AND task_number = ?2 \
            AND resolved_at IS NULL \
          ORDER BY observed_at ASC, id ASC",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![plan_name, task_number], |row| {
        Ok(PendingCiFailure {
            id: row.get(0)?,
            run_id: row.get(1)?,
            run_url: row.get(2)?,
            workflow: row.get(3)?,
            conclusion: row.get(4)?,
            summary: row.get(5)?,
            observed_at: row.get(6)?,
        })
    })
    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    .unwrap_or_default()
}

/// Mark every pending `ci_failure_events` row for `(plan_name,
/// task_number)` as resolved (`resolved_at = datetime('now')`). Returns
/// the number of rows updated, which doubles as a "did we just clear the
/// gate?" signal for callers.
///
/// Idempotent: a second call after the first matches no rows (the
/// `WHERE resolved_at IS NULL` filter is the gate) and returns 0. Callers
/// fire this from any path that captures a learning (HTTP
/// `add_task_learning`, MCP `report_blocker`, MCP `update_task_status`
/// when `reason` is set) so the gate clears as a side effect of the
/// existing learning-write — no new agent contract.
pub fn mark_ci_failures_resolved(db: &Db, plan_name: &str, task_number: &str) -> usize {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE ci_failure_events \
            SET resolved_at = datetime('now') \
          WHERE plan_name = ?1 \
            AND task_number = ?2 \
            AND resolved_at IS NULL",
        params![plan_name, task_number],
    )
    .unwrap_or(0)
}

// ── Per-branch push lock (Phase 2) ──────────────────────────────────────────

/// Default TTL for a `master_push_lock` row. A holder that hasn't touched
/// the row in this long is treated as crashed and its lock can be force-
/// acquired by a fresh caller. Kept short so a wedged auto-mode worker
/// doesn't deadlock CI for the entire pipeline run.
pub const PUSH_LOCK_TTL_SECS: i64 = 30;

/// Snapshot of the current holder of a `master_push_lock` row. Returned
/// from [`peek_push_lock`] and from the `Err` arm of
/// [`try_acquire_push_lock`] so callers can render a useful "lock busy"
/// diagnostic (which kind of caller is holding it, how stale the row is).
#[derive(Debug, Clone)]
pub struct PushLockHolder {
    /// Opaque random token identifying the holder. Required to release.
    pub holder_token: String,
    /// OS pid of the holding process. The server's own pid for in-process
    /// auto-mode callers; the server's pid (NOT the CI runner's) for HTTP
    /// callers (the row lives in the server's DB, so liveness is judged
    /// by TTL, not by PID liveness).
    pub holder_pid: i64,
    /// `"auto_mode"` / `"ci"` / `"manual"` / other — free-form tag used
    /// only for diagnostics + the audit log.
    pub holder_kind: String,
    /// JSON or other free-form metadata the holder wants to surface (e.g.
    /// plan_name, ci_run_id). Optional.
    pub holder_meta: Option<String>,
    /// Wall-clock when the lock was taken, in SQLite `datetime('now')`
    /// format.
    pub taken_at: String,
    /// Server-evaluated `now - taken_at` in seconds. Used by the API
    /// endpoint to render age and by `try_acquire_push_lock` to TTL-evict.
    pub age_secs: i64,
}

/// Attempt to acquire the per-branch push lock. Returns `Ok(token)` on
/// success — the caller must pass that token back to
/// [`release_push_lock`]. Returns `Err(holder)` when a live holder
/// already exists (within TTL).
///
/// TTL eviction: if an existing row's `age_secs > ttl_secs`, the holder
/// is treated as crashed and the row is replaced by the new caller. This
/// is the only path that recovers from a server crash mid-push.
///
/// The whole acquire path runs inside a single `IMMEDIATE` transaction so
/// two concurrent callers don't both see "no row" and both insert. SQLite
/// `INSERT INTO ... ON CONFLICT(branch) DO UPDATE WHERE ...` does the
/// atomic check-and-replace in a single statement; we only fall back to
/// the `SELECT` if the conflict path was a no-op (i.e. a live holder
/// already owned the lock).
#[allow(dead_code)] // wired in by Phase 2 callers (auto_mode + /api/git/push-lock)
pub fn try_acquire_push_lock(
    db: &Db,
    branch: &str,
    holder_kind: &str,
    holder_pid: i64,
    holder_meta: Option<&str>,
    ttl_secs: i64,
) -> Result<String, PushLockHolder> {
    let token = uuid::Uuid::new_v4().to_string();
    let conn = db.lock().unwrap();
    // `WHERE` on the UPSERT path: only steal a row if it is past TTL.
    // The row's `taken_at` is the canonical truth; we compute age via
    // SQLite so callers don't have to round-trip the clock.
    let rows_changed = conn
        .execute(
            "INSERT INTO master_push_lock \
                (branch, holder_token, holder_pid, holder_kind, holder_meta, taken_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now')) \
             ON CONFLICT(branch) DO UPDATE SET \
                holder_token = excluded.holder_token, \
                holder_pid   = excluded.holder_pid, \
                holder_kind  = excluded.holder_kind, \
                holder_meta  = excluded.holder_meta, \
                taken_at     = excluded.taken_at \
             WHERE CAST(strftime('%s','now') - strftime('%s', master_push_lock.taken_at) AS INTEGER) > ?6",
            params![branch, token, holder_pid, holder_kind, holder_meta, ttl_secs],
        )
        .unwrap_or(0);

    if rows_changed > 0 {
        return Ok(token);
    }

    // Either the WHERE clause refused the steal (live holder), or the
    // upsert silently no-op'd. Re-read the row to surface the live
    // holder. If even the SELECT fails (race with a concurrent release),
    // synthesize an empty holder so the caller's diagnostic still works.
    match conn.query_row(
        "SELECT holder_token, holder_pid, holder_kind, holder_meta, taken_at, \
                CAST(strftime('%s','now') - strftime('%s', taken_at) AS INTEGER) \
         FROM master_push_lock WHERE branch = ?1",
        params![branch],
        |row| {
            Ok(PushLockHolder {
                holder_token: row.get(0)?,
                holder_pid: row.get(1)?,
                holder_kind: row.get(2)?,
                holder_meta: row.get(3)?,
                taken_at: row.get(4)?,
                age_secs: row.get(5)?,
            })
        },
    ) {
        Ok(h) => Err(h),
        Err(_) => {
            // Concurrent release between our upsert and our SELECT —
            // retry the upsert exactly once. Avoids an unbounded loop
            // and keeps the function constant-time in practice.
            let retry = conn
                .execute(
                    "INSERT OR IGNORE INTO master_push_lock \
                        (branch, holder_token, holder_pid, holder_kind, holder_meta, taken_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                    params![branch, token, holder_pid, holder_kind, holder_meta],
                )
                .unwrap_or(0);
            if retry > 0 {
                Ok(token)
            } else {
                Err(PushLockHolder {
                    holder_token: String::new(),
                    holder_pid: 0,
                    holder_kind: "unknown".to_string(),
                    holder_meta: None,
                    taken_at: String::new(),
                    age_secs: 0,
                })
            }
        }
    }
}

/// Release the push lock for `branch` IFF `token` matches the current
/// holder. Returns `true` if the row was deleted, `false` otherwise
/// (wrong token, row already gone, etc.). Safe to call from the Drop
/// impl of a guard: the worst case is a stale-token call that does
/// nothing.
#[allow(dead_code)] // wired in by Phase 2 callers (auto_mode + /api/git/push-lock)
pub fn release_push_lock(db: &Db, branch: &str, token: &str) -> bool {
    let conn = db.lock().unwrap();
    conn.execute(
        "DELETE FROM master_push_lock WHERE branch = ?1 AND holder_token = ?2",
        params![branch, token],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Read the current holder of `branch`'s push lock if any. Does NOT
/// TTL-evict — that only happens on a real acquire attempt. Used by the
/// HTTP endpoint to render the "lock busy" diagnostic and by tests to
/// observe state.
#[allow(dead_code)] // wired in by Phase 2 callers (api endpoint + tests)
pub fn peek_push_lock(db: &Db, branch: &str) -> Option<PushLockHolder> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT holder_token, holder_pid, holder_kind, holder_meta, taken_at, \
                CAST(strftime('%s','now') - strftime('%s', taken_at) AS INTEGER) \
         FROM master_push_lock WHERE branch = ?1",
        params![branch],
        |row| {
            Ok(PushLockHolder {
                holder_token: row.get(0)?,
                holder_pid: row.get(1)?,
                holder_kind: row.get(2)?,
                holder_meta: row.get(3)?,
                taken_at: row.get(4)?,
                age_secs: row.get(5)?,
            })
        },
    )
    .ok()
}

// ── Credentials (Phase 3.1 of runner-daemon-workspace) ──────────────────

/// Kinds of credentials Branchwork can store. The runner-side dispatch
/// path (later phases) reads the `kind` column to pick the right
/// rendering — SSH agent socket for `SshKey`, https oauth header for the
/// PAT variants. Manual parse / wire helpers below; the schema column
/// stores the snake_case wire form.
#[allow(dead_code)] // consumed by API + dispatch in later phases
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    SshKey,
    GhPat,
    GitlabPat,
}

impl CredentialKind {
    /// Wire-form (also the on-disk column value).
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::SshKey => "ssh_key",
            CredentialKind::GhPat => "gh_pat",
            CredentialKind::GitlabPat => "gitlab_pat",
        }
    }

    /// Parse the wire form. Unknown values return None so a corrupted row
    /// or future schema reshuffle does not panic. Mirrors the lenient parse
    /// used by [`parse_merge_cadence`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ssh_key" => Some(CredentialKind::SshKey),
            "gh_pat" => Some(CredentialKind::GhPat),
            "gitlab_pat" => Some(CredentialKind::GitlabPat),
            _ => None,
        }
    }
}

/// Operator-facing credential record. `secret_plaintext` is the raw secret
/// — supplied at insert time, decrypted at the moment of dispatch, never
/// stored in plaintext. Set both `set_credential` callers and downstream
/// consumers must zeroize after use; the type does not enforce that today
/// because no API surface returns this struct to the wire (a later phase
/// will add a redacted DTO).
#[allow(dead_code)] // consumed by API + dispatch in later phases
#[derive(Debug, Clone)]
pub struct CredentialInput<'a> {
    pub id: &'a str,
    pub owner_user_id: &'a str,
    pub name: &'a str,
    pub kind: CredentialKind,
    pub public_part: Option<&'a str>,
    pub secret_plaintext: &'a [u8],
    pub host_hint: Option<&'a str>,
    pub scopes: Option<&'a str>,
}

/// What `get_credential` returns. `secret_plaintext` is the decrypted
/// blob — callers MUST treat this as ephemeral. Encryption metadata
/// (nonce, tag) is stripped at the crypto boundary.
#[allow(dead_code)] // consumed by API + dispatch in later phases
#[derive(Debug, Clone)]
pub struct DecryptedCredential {
    pub id: String,
    pub owner_user_id: String,
    pub name: String,
    pub kind: CredentialKind,
    pub public_part: Option<String>,
    pub secret_plaintext: Vec<u8>,
    pub host_hint: Option<String>,
    pub scopes: Option<String>,
    pub created_at: String,
    pub archived_at: Option<String>,
}

/// Errors from the credential round-trip path. Crypto failures wrap
/// [`crate::crypto::CryptoError`] verbatim so the caller can match on
/// `DecryptionFailed` to render "this credential was sealed by an older
/// master key — re-enter it" in the UI.
#[allow(dead_code)] // consumed by API + dispatch in later phases
#[derive(Debug)]
pub enum CredentialError {
    NotFound,
    Crypto(crate::crypto::CryptoError),
    Db(rusqlite::Error),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::NotFound => f.write_str("credential not found"),
            CredentialError::Crypto(e) => write!(f, "credential crypto error: {e}"),
            CredentialError::Db(e) => write!(f, "credential db error: {e}"),
        }
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CredentialError::Crypto(e) => Some(e),
            CredentialError::Db(e) => Some(e),
            CredentialError::NotFound => None,
        }
    }
}

impl From<crate::crypto::CryptoError> for CredentialError {
    fn from(e: crate::crypto::CryptoError) -> Self {
        CredentialError::Crypto(e)
    }
}

impl From<rusqlite::Error> for CredentialError {
    fn from(e: rusqlite::Error) -> Self {
        CredentialError::Db(e)
    }
}

/// Encrypt `input.secret_plaintext` under `key` and INSERT the row.
/// Plaintext never reaches the DB. Caller picks the `id` (typically a
/// UUID); duplicate ids surface as `CredentialError::Db(...)`.
///
/// Idempotency: this is a plain INSERT, NOT an UPSERT — rotating the
/// secret should be a separate path (different audit row, different UI
/// affordance). Phase 3.x consumers will add an explicit
/// `rotate_credential` helper.
#[allow(dead_code)] // consumed by the credentials API in later phases
pub fn set_credential(
    db: &Db,
    key: &crate::crypto::MasterKey,
    input: &CredentialInput<'_>,
) -> Result<(), CredentialError> {
    let encrypted = crate::crypto::encrypt(key, input.secret_plaintext)?;
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO credentials \
            (id, owner_user_id, name, kind, public_part, encrypted_secret, host_hint, scopes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.id,
            input.owner_user_id,
            input.name,
            input.kind.as_str(),
            input.public_part,
            encrypted,
            input.host_hint,
            input.scopes,
        ],
    )?;
    Ok(())
}

/// Look up a credential by `id` and decrypt the secret. Returns
/// [`CredentialError::NotFound`] when no row matches, including archived
/// rows (the read filter excludes `archived_at IS NOT NULL` so an
/// archived credential cannot accidentally be dispatched against). To
/// inspect archived rows for the audit log, use a dedicated reader (not
/// yet shipped — Phase 3.x).
///
/// Decryption failure (wrong master key, tampered ciphertext) bubbles up
/// as [`CredentialError::Crypto`]. This is the canonical signal that the
/// operator rotated `.master.key` and existing rows are now sealed; the
/// UI can render an actionable "re-enter this credential" prompt.
#[allow(dead_code)] // consumed by dispatch in later phases
pub fn get_credential(
    db: &Db,
    key: &crate::crypto::MasterKey,
    id: &str,
) -> Result<DecryptedCredential, CredentialError> {
    let conn = db.lock().unwrap();
    let row = conn
        .query_row(
            "SELECT id, owner_user_id, name, kind, public_part, encrypted_secret, \
                    host_hint, scopes, created_at, archived_at \
             FROM credentials \
             WHERE id = ?1 AND archived_at IS NULL",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => CredentialError::NotFound,
            other => CredentialError::Db(other),
        })?;
    drop(conn);

    let (
        id,
        owner_user_id,
        name,
        kind_str,
        public_part,
        encrypted,
        host_hint,
        scopes,
        created_at,
        archived_at,
    ) = row;
    let secret_plaintext = crate::crypto::decrypt(key, &encrypted)?;
    let kind = CredentialKind::parse(&kind_str).ok_or_else(|| {
        // A corrupted kind column is a programmer error, but we map it to
        // Crypto so it surfaces with the same "re-enter this credential"
        // UX the rotation case uses. The alternative — panicking — would
        // 500 the whole credentials list endpoint when one row is bad.
        CredentialError::Crypto(crate::crypto::CryptoError::EncryptionFailed(format!(
            "unknown credential kind on row {id}: {kind_str}"
        )))
    })?;

    Ok(DecryptedCredential {
        id,
        owner_user_id,
        name,
        kind,
        public_part,
        secret_plaintext,
        host_hint,
        scopes,
        created_at,
        archived_at,
    })
}

/// Flip a credential from active to archived (soft-delete). Idempotent —
/// archiving an already-archived row leaves `archived_at` untouched.
/// `get_credential` filters out archived rows automatically.
#[allow(dead_code)] // consumed by the credentials API in later phases
pub fn archive_credential(db: &Db, id: &str) -> Result<bool, CredentialError> {
    let conn = db.lock().unwrap();
    let n = conn.execute(
        "UPDATE credentials SET archived_at = datetime('now') \
         WHERE id = ?1 AND archived_at IS NULL",
        params![id],
    )?;
    Ok(n > 0)
}

// ── Org-shared learnings (Phase 2 of learning-hub-ci-failure-capture) ─────

/// Taxonomy for the `learnings` table. Mirrors the per-agent auto-memory
/// types (user / feedback / project / reference) but pruned to the three
/// kinds that make sense as org-shared lessons: `feedback` (rules learned
/// the hard way), `project` (active context other agents will need), and
/// `reference` (pointers to external systems). `user` is intentionally
/// excluded — it is a property of a single operator, not the org.
#[allow(dead_code)] // consumed by API + dashboard in later phases
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningKind {
    Feedback,
    Project,
    Reference,
}

impl LearningKind {
    /// Wire-form (also the on-disk column value).
    pub fn as_str(self) -> &'static str {
        match self {
            LearningKind::Feedback => "feedback",
            LearningKind::Project => "project",
            LearningKind::Reference => "reference",
        }
    }

    /// Parse the wire form. Unknown values return None so a corrupted row
    /// or future schema reshuffle does not panic. Mirrors the lenient parse
    /// used by [`CredentialKind::parse`] and [`parse_merge_cadence`].
    #[allow(dead_code)] // consumed by API + dashboard in later phases
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "feedback" => Some(LearningKind::Feedback),
            "project" => Some(LearningKind::Project),
            "reference" => Some(LearningKind::Reference),
            _ => None,
        }
    }
}

/// What [`create_learning`] takes — caller-supplied fields. `id` is the
/// stable UUID the caller mints; `slug` is the kebab-case handle that
/// participates in the `(org_id, kind, slug)` uniqueness constraint.
#[allow(dead_code)] // consumed by API + dashboard in later phases
#[derive(Debug, Clone)]
pub struct LearningInput<'a> {
    pub id: &'a str,
    pub org_id: &'a str,
    pub kind: LearningKind,
    pub category: &'a str,
    pub slug: &'a str,
    pub body_md: &'a str,
    pub source_agent_id: Option<&'a str>,
    pub source_ci_run_id: Option<i64>,
}

/// What [`get_learning`] / [`list_learnings_for_org`] return. Fields
/// renamed via `rename_all = "camelCase"` so the same struct works as a
/// JSON wire payload directly when an API handler returns it.
#[allow(dead_code)] // consumed by API + dashboard in later phases
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Learning {
    pub id: String,
    pub org_id: String,
    /// Wire form of [`LearningKind`] — keep as String so a row written by a
    /// future code path with an unknown kind still serialises through.
    pub kind: String,
    pub category: String,
    pub slug: String,
    pub body_md: String,
    pub source_agent_id: Option<String>,
    pub source_ci_run_id: Option<i64>,
    pub created_at: String,
    pub archived_at: Option<String>,
    pub archived_reason: Option<String>,
}

/// Errors from the learning round-trip path. Kept as a small enum so
/// callers can map `NotFound` to 404 and `SlugCollision` to 409 without
/// pattern-matching on rusqlite's `Error` variants directly.
#[allow(dead_code)] // consumed by API in later phases
#[derive(Debug)]
pub enum LearningError {
    NotFound,
    /// `(org_id, kind, slug)` already exists. The unique-index trip is the
    /// canonical signal a caller is re-writing instead of patching.
    SlugCollision,
    Db(rusqlite::Error),
}

impl std::fmt::Display for LearningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LearningError::NotFound => f.write_str("learning not found"),
            LearningError::SlugCollision => {
                f.write_str("learning slug already exists for this org+kind")
            }
            LearningError::Db(e) => write!(f, "learning db error: {e}"),
        }
    }
}

impl std::error::Error for LearningError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LearningError::Db(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for LearningError {
    fn from(e: rusqlite::Error) -> Self {
        // Detect the `(org_id, kind, slug)` unique-index trip up front so the
        // caller does not have to inspect raw SQLite extended codes. SQLite
        // reports the failure by column-name signature (e.g.
        // "UNIQUE constraint failed: learnings.org_id, learnings.kind,
        // learnings.slug"), NOT by the index name — so match on the table-
        // qualified slug column instead.
        if let rusqlite::Error::SqliteFailure(ref err, ref msg) = e
            && err.code == rusqlite::ErrorCode::ConstraintViolation
            && msg.as_deref().is_some_and(|m| {
                m.contains("UNIQUE constraint failed") && m.contains("learnings.slug")
            })
        {
            return LearningError::SlugCollision;
        }
        LearningError::Db(e)
    }
}

/// INSERT a new learning row. `created_at` is filled by the schema
/// default; `archived_at` / `archived_reason` start NULL.
///
/// The `(org_id, kind, slug)` triple must be unique within the org —
/// trying to recreate an existing learning surfaces as
/// [`LearningError::SlugCollision`]. Callers that want "patch the body"
/// semantics should issue a separate UPDATE; this helper is INSERT-only
/// so the audit trail can record creation vs. mutation distinctly.
#[allow(dead_code)] // consumed by API in later phases
pub fn create_learning(db: &Db, input: &LearningInput<'_>) -> Result<(), LearningError> {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO learnings \
            (id, org_id, kind, category, slug, body_md, \
             source_agent_id, source_ci_run_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.id,
            input.org_id,
            input.kind.as_str(),
            input.category,
            input.slug,
            input.body_md,
            input.source_agent_id,
            input.source_ci_run_id,
        ],
    )?;
    Ok(())
}

/// Look up a learning by `id`. Returns archived rows too so the Activity
/// tab can render "this learning was archived because <reason>"; callers
/// that want only active learnings should check `archived_at.is_none()`.
#[allow(dead_code)] // consumed by API in later phases
pub fn get_learning(db: &Db, id: &str) -> Result<Learning, LearningError> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT id, org_id, kind, category, slug, body_md, \
                source_agent_id, source_ci_run_id, created_at, \
                archived_at, archived_reason \
           FROM learnings WHERE id = ?1",
        params![id],
        |row| {
            Ok(Learning {
                id: row.get(0)?,
                org_id: row.get(1)?,
                kind: row.get(2)?,
                category: row.get(3)?,
                slug: row.get(4)?,
                body_md: row.get(5)?,
                source_agent_id: row.get(6)?,
                source_ci_run_id: row.get(7)?,
                created_at: row.get(8)?,
                archived_at: row.get(9)?,
                archived_reason: row.get(10)?,
            })
        },
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => LearningError::NotFound,
        other => LearningError::Db(other),
    })
}

/// List every active (non-archived) learning for `org_id`, newest first.
/// Pass `include_archived = true` to surface tombstones for the audit
/// view. Optionally filter by `kind` so the dashboard can render a
/// per-kind tab.
#[allow(dead_code)] // consumed by API in later phases
pub fn list_learnings_for_org(
    db: &Db,
    org_id: &str,
    kind: Option<LearningKind>,
    include_archived: bool,
) -> Vec<Learning> {
    let conn = db.lock().unwrap();
    // Build the SQL up-front so the optional `kind` filter can splice in
    // without dragging in a query builder dep. Parameter slots remain
    // numbered so we can keep using `params!`.
    let mut sql = String::from(
        "SELECT id, org_id, kind, category, slug, body_md, \
                source_agent_id, source_ci_run_id, created_at, \
                archived_at, archived_reason \
           FROM learnings \
          WHERE org_id = ?1",
    );
    if !include_archived {
        sql.push_str(" AND archived_at IS NULL");
    }
    if kind.is_some() {
        sql.push_str(" AND kind = ?2");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Learning> {
        Ok(Learning {
            id: row.get(0)?,
            org_id: row.get(1)?,
            kind: row.get(2)?,
            category: row.get(3)?,
            slug: row.get(4)?,
            body_md: row.get(5)?,
            source_agent_id: row.get(6)?,
            source_ci_run_id: row.get(7)?,
            created_at: row.get(8)?,
            archived_at: row.get(9)?,
            archived_reason: row.get(10)?,
        })
    };
    let rows = if let Some(k) = kind {
        stmt.query_map(params![org_id, k.as_str()], map_row)
            .and_then(|r| r.collect::<Result<Vec<_>, _>>())
    } else {
        stmt.query_map(params![org_id], map_row)
            .and_then(|r| r.collect::<Result<Vec<_>, _>>())
    };
    rows.unwrap_or_default()
}

/// Soft-delete a learning. Sets `archived_at = datetime('now')` and
/// stores the operator-supplied `reason` so the audit trail can show
/// why a once-canonical lesson is no longer active. Idempotent —
/// archiving an already-archived row leaves the original timestamp
/// and reason untouched.
///
/// Returns `Ok(true)` when a row flipped from active to archived,
/// `Ok(false)` when the row was already archived (or did not exist —
/// the caller should distinguish by reading [`get_learning`] first if
/// that matters).
#[allow(dead_code)] // consumed by API in later phases
pub fn archive_learning(db: &Db, id: &str, reason: &str) -> Result<bool, LearningError> {
    let conn = db.lock().unwrap();
    let n = conn.execute(
        "UPDATE learnings \
            SET archived_at = datetime('now'), \
                archived_reason = ?2 \
          WHERE id = ?1 AND archived_at IS NULL",
        params![id, reason],
    )?;
    Ok(n > 0)
}

// ── Learning hit-counter + promotion (Phase 4, Task 4.1) ────────────────────

/// How many distinct sessions a learning must be rendered into, within
/// [`LEARNING_PROMOTION_WINDOW_DAYS`], before the server flags it as a
/// promotion candidate. The brief's default.
pub const LEARNING_PROMOTION_THRESHOLD: i64 = 5;

/// Rolling window (days) over which [`record_learning_hit`] counts
/// distinct-session hits when deciding whether to emit a
/// `promotion_candidate`.
pub const LEARNING_PROMOTION_WINDOW_DAYS: i64 = 30;

/// Outcome of recording one learning render against one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningHitOutcome {
    /// True when this `(learning_id, agent_id)` pair was new — i.e. the
    /// learning was rendered into a session it had not been rendered into
    /// before. False on a re-render within the same session (the
    /// composite PK swallows the duplicate).
    pub inserted: bool,
    /// Distinct sessions in the last [`LEARNING_PROMOTION_WINDOW_DAYS`]
    /// for this learning, AFTER this hit. Only meaningful when `inserted`
    /// is true; a duplicate render returns 0 without recounting.
    pub recent_hit_count: i64,
    /// True when this hit pushed the learning across the threshold AND a
    /// `promotion_candidates` row was newly written. The caller fires the
    /// `promotion_candidate` broadcast only when this is true — every
    /// later render that re-crosses the (idempotent) threshold returns
    /// false here.
    pub promotion_emitted: bool,
}

/// Record that `learning_id` was rendered into the session identified by
/// `agent_id`, then decide whether the learning has crossed the
/// promotion threshold.
///
/// Idempotency / no-double-count: `learning_hits` has a composite PRIMARY
/// KEY `(learning_id, agent_id)`, so a second render of the same learning
/// within the same session collapses to a no-op `INSERT OR IGNORE`
/// (`inserted = false`) and the counts are left untouched.
///
/// Threshold check (only on a genuinely new hit): count the distinct
/// sessions this learning was rendered into within the last `window_days`
/// (each `(learning, session)` pair is exactly one row, so `COUNT(*)` is
/// the distinct-session count). If that reaches `threshold`, write a
/// `promotion_candidates` row — `INSERT OR IGNORE` on the `learning_id`
/// PRIMARY KEY keeps the emission single: a learning is flagged once, not
/// on every subsequent render past the threshold.
///
/// `window_days` is interpolated into the `datetime('now', '-N days')`
/// modifier because SQLite does not allow binding inside a datetime
/// modifier (same constraint the CI backfill lookback works around). It
/// is an `i64` from a server-side const, never user input, so the
/// `format!` is injection-safe.
pub fn record_learning_hit(
    db: &Db,
    learning_id: &str,
    agent_id: &str,
    org_id: &str,
    threshold: i64,
    window_days: i64,
) -> LearningHitOutcome {
    let conn = db.lock().unwrap();

    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO learning_hits (learning_id, agent_id, org_id) \
             VALUES (?1, ?2, ?3)",
            params![learning_id, agent_id, org_id],
        )
        .unwrap_or(0)
        > 0;

    if !inserted {
        return LearningHitOutcome {
            inserted: false,
            recent_hit_count: 0,
            promotion_emitted: false,
        };
    }

    let recent_hit_count: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM learning_hits \
                  WHERE learning_id = ?1 \
                    AND rendered_at >= datetime('now', '-{window_days} days')"
            ),
            params![learning_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let promotion_emitted = if recent_hit_count >= threshold {
        conn.execute(
            "INSERT OR IGNORE INTO promotion_candidates \
                 (learning_id, org_id, hit_count, threshold, window_days) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                learning_id,
                org_id,
                recent_hit_count,
                threshold,
                window_days
            ],
        )
        .unwrap_or(0)
            > 0
    } else {
        false
    };

    LearningHitOutcome {
        inserted: true,
        recent_hit_count,
        promotion_emitted,
    }
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

        -- Typed log of CI failures observed on a branch owned by a live
        -- agent. One row per (agent_id, run_id) pair — the UNIQUE index
        -- gives us cheap idempotency on retry: re-polling the same red
        -- run for the same agent does NOT duplicate the row. Written by
        -- ci.rs whenever a ci_runs row transitions to status=failure
        -- and a matching agent (status IN running|starting and branch
        -- equal to the ci_runs branch) is found. Out-of-band tools that
        -- accumulate per-agent learnings (Phase 1.2+) consume this
        -- table; rows with no surviving agent are not inserted.
        CREATE TABLE IF NOT EXISTS ci_failure_events (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT    NOT NULL,
            plan_name    TEXT    NOT NULL,
            task_number  TEXT,
            branch       TEXT    NOT NULL,
            run_id       TEXT    NOT NULL,
            run_url      TEXT,
            workflow     TEXT,
            conclusion   TEXT,
            failed_job   TEXT,
            summary      TEXT,
            org_id       TEXT    NOT NULL DEFAULT 'default-org',
            observed_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_ci_failure_events_agent_run
            ON ci_failure_events(agent_id, run_id);
        CREATE INDEX IF NOT EXISTS idx_ci_failure_events_plan_task
            ON ci_failure_events(plan_name, task_number);

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
            token_hash         TEXT PRIMARY KEY,
            runner_name        TEXT NOT NULL,
            org_id             TEXT NOT NULL DEFAULT 'default-org',
            created_by         TEXT NOT NULL,
            created_at         TEXT NOT NULL DEFAULT (datetime('now')),
            claimed_runner_id  TEXT,
            FOREIGN KEY (created_by) REFERENCES users(id)
        );
        CREATE INDEX IF NOT EXISTS idx_runner_tokens_org ON runner_tokens(org_id);

        -- Per-runner override of server-wide settings. Both columns are
        -- nullable: NULL means inherit the server-wide default. The dispatch
        -- layer resolves override-or-inherit at StartAgent build time and
        -- ships the resolved values; the runner does not re-resolve.
        CREATE TABLE IF NOT EXISTS runner_config (
            runner_id        TEXT PRIMARY KEY,
            effort           TEXT,
            skip_permissions INTEGER,
            org_id           TEXT NOT NULL DEFAULT 'default-org',
            FOREIGN KEY (runner_id) REFERENCES runners(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_runner_config_org ON runner_config(org_id);

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

        -- ── Plan snapshots ───────────────────────────────────────────────
        -- Captures the full pre-cascade state of a plan (YAML body +
        -- every plan_name-keyed row) before a destructive primitive
        -- mutates it. Kinds: delete | merge | rename | archive |
        -- rewrite_context. The retention purger (plan-deletion 0.5)
        -- uses `expires_at` to free rows; until then they are the
        -- substrate for the Activity-tab Undo affordance.
        CREATE TABLE IF NOT EXISTS plan_snapshots (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_name     TEXT NOT NULL,
            kind          TEXT NOT NULL,
            created_at    TEXT NOT NULL DEFAULT (datetime('now')),
            expires_at    TEXT NOT NULL,
            org_id        TEXT NOT NULL DEFAULT 'default-org',
            archive_path  TEXT,
            yaml_body     TEXT NOT NULL,
            cascade_json  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_plan_snapshots_expires ON plan_snapshots(expires_at);
        CREATE INDEX IF NOT EXISTS idx_plan_snapshots_plan ON plan_snapshots(plan_name);

        -- Generic key/value table for one-shot startup gates (e.g. the
        -- `ci_backfill_v1_done` flag set after the first aggregate-aware
        -- backfill of legacy `ci_runs` rows). Not org-scoped: gates here
        -- describe migration state for the database as a whole.
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT
        );

        -- Per-branch advisory lock for the merge → push critical section
        -- (Phase 2 of the auto-push-rebase plan). One row at most per
        -- branch name; `holder_token` is the random opaque handle the
        -- holder must present to release. `taken_at` anchors a TTL-based
        -- liveness check so a crashed holder (server SIGKILL mid-push)
        -- can't deadlock the lock indefinitely — a fresh acquire after
        -- TTL_SECS overwrites in place. See `try_acquire_push_lock` /
        -- `release_push_lock` / `peek_push_lock` helpers.
        CREATE TABLE IF NOT EXISTS master_push_lock (
            branch       TEXT PRIMARY KEY,
            holder_token TEXT NOT NULL,
            holder_pid   INTEGER NOT NULL,
            holder_kind  TEXT NOT NULL,
            holder_meta  TEXT,
            taken_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- ── Projects (Phase 2.1 of runner-daemon-workspace) ─────────────────
        -- Server-driven project creation flow: clone existing OR create new
        -- on host. Both paths target $HOME/<name> (or operator-overridden
        -- workspace_path). One row per project. Plans + agents pick up an
        -- optional project_id FK so the dashboard can render which project a
        -- plan belongs to without re-parsing cwd strings.
        --
        -- Fields:
        -- - id is a UUID generated server-side.
        -- - name is operator-friendly (a slug); does NOT need to match the
        --   on-disk directory basename (workspace_path is the source of
        --   truth for the on-disk location).
        -- - repo_url is the clone URL (https:// or git@host:...). Required.
        -- - host enum: github | gitlab | bitbucket | other. Phase 2.3 uses
        --   this to dispatch to the right host API for create mode.
        -- - owner is the host-side owner (org/user); nullable for the other
        --   host where the concept does not apply.
        -- - default_credential_id is a forward-looking FK; the credentials
        --   table lands in Phase 3.1. NULL today.
        -- - workspace_path is the resolved absolute path on the runner host
        --   (or local fs in standalone mode); defaults to $HOME/<name> if
        --   the caller does not override.
        CREATE TABLE IF NOT EXISTS projects (
            id                     TEXT PRIMARY KEY,
            name                   TEXT NOT NULL,
            repo_url               TEXT NOT NULL,
            host                   TEXT NOT NULL DEFAULT 'other',
            owner                  TEXT,
            default_credential_id  TEXT,
            workspace_path         TEXT NOT NULL,
            org_id                 TEXT NOT NULL DEFAULT 'default-org',
            created_at             TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_org_name
            ON projects(org_id, name);
        CREATE INDEX IF NOT EXISTS idx_projects_org ON projects(org_id);

        -- Credentials (Phase 3.1 of runner-daemon-workspace).
        -- Branchwork-managed credentials for git hosts. Plaintext NEVER
        -- hits the DB; encrypted_secret carries AES-GCM ciphertext (12-byte
        -- nonce prepended) keyed by the master key at
        -- BRANCHWORK_DATA_DIR/.master.key. Decryption happens only at the
        -- moment of dispatch. See server-rs/src/crypto.rs for the cipher
        -- impl + rotation contract.
        --
        -- Fields:
        -- - id is a UUID generated server-side.
        -- - owner_user_id is the user who created the credential; FK to
        --   users(id) so deleting a user takes their credentials with them.
        -- - name is operator-friendly. Not globally unique; scoped by
        --   owner.
        -- - kind enumerates the credential shape (ssh_key, gh_pat,
        --   gitlab_pat). Future runner-side dispatch reads kind to pick
        --   the right rendering (SSH agent vs. https oauth header).
        -- - public_part is the operator-visible non-secret half: the SSH
        --   public key, or a label / fingerprint for PATs. NULL when the
        --   credential has no public component worth surfacing.
        -- - encrypted_secret is the AES-GCM blob (nonce + ciphertext +
        --   tag). The plaintext is the raw private key body or the PAT
        --   string. Never logged.
        -- - host_hint is the canonical host the credential is for (e.g.
        --   github.com, gitlab.example.com). Lets the dispatch path pick a
        --   credential without an explicit choice when one matches.
        -- - scopes is the host-reported scope list (e.g. repo,workflow for
        --   a GitHub PAT). Captured at registration time; not refreshed.
        -- - archived_at flips the row from active to tombstoned so the
        --   audit trail survives a delete. NULL means active.
        CREATE TABLE IF NOT EXISTS credentials (
            id                TEXT PRIMARY KEY,
            owner_user_id     TEXT NOT NULL,
            name              TEXT NOT NULL,
            kind              TEXT NOT NULL,
            public_part       TEXT,
            encrypted_secret  BLOB NOT NULL,
            host_hint         TEXT,
            scopes            TEXT,
            created_at        TEXT NOT NULL DEFAULT (datetime('now')),
            archived_at       TEXT,
            FOREIGN KEY (owner_user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_credentials_owner ON credentials(owner_user_id);
        CREATE INDEX IF NOT EXISTS idx_credentials_active
            ON credentials(owner_user_id) WHERE archived_at IS NULL;

        -- Org-shared learnings store (Phase 2 of learning-hub-ci-failure-capture).
        -- One row per durable lesson the org wants future agents to read on
        -- the way into similar work. Body is markdown so the on-disk auto-memory
        -- format under ~/.claude/projects/<project>/memory/<slug>.md survives a
        -- paste in either direction and the dashboard can render it trivially.
        --
        -- Fields:
        -- - id is a UUID generated server-side.
        -- - org_id scopes the row to an organisation (FK CASCADE so a deleted
        --   org takes its learnings with it). Default keeps pre-migration
        --   tooling sane.
        -- - kind enumerates feedback | project | reference, mirroring the
        --   per-agent auto-memory taxonomy. Stored as the snake_case wire
        --   string so reading is grep-friendly and parsing is lenient.
        -- - category is an operator-friendly grouping label (e.g. testing,
        --   deploy, ci) free-form but indexed for org+kind+category lookups.
        -- - slug is a stable kebab-case handle (operator-supplied or derived).
        --   Unique within (org_id, kind) so a repeat write surfaces as a
        --   constraint violation instead of silently shadowing the old row.
        -- - body_md is markdown — never sanitised at store time; consumers
        --   render it.
        -- - source_agent_id / source_ci_run_id link the learning back to its
        --   origin so the Activity tab can render which fix-attempt and red
        --   CI run produced it. Both nullable (a hand-authored learning has
        --   no source).
        -- - archived_at + archived_reason capture soft-delete with intent
        --   (e.g. outdated-by-ADR, no-longer-applicable) so the audit
        --   trail survives a delete.
        CREATE TABLE IF NOT EXISTS learnings (
            id                 TEXT PRIMARY KEY,
            org_id             TEXT NOT NULL DEFAULT 'default-org',
            kind               TEXT NOT NULL,
            category           TEXT NOT NULL,
            slug               TEXT NOT NULL,
            body_md            TEXT NOT NULL,
            source_agent_id    TEXT,
            source_ci_run_id   INTEGER,
            created_at         TEXT NOT NULL DEFAULT (datetime('now')),
            archived_at        TEXT,
            archived_reason    TEXT,
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_learnings_org_kind_slug
            ON learnings(org_id, kind, slug);
        CREATE INDEX IF NOT EXISTS idx_learnings_org_active
            ON learnings(org_id) WHERE archived_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_learnings_source_ci
            ON learnings(source_ci_run_id) WHERE source_ci_run_id IS NOT NULL;

        -- Hit-counter on learnings (Phase 4, Task 4.1). One row per
        -- (learning, session) — `agent_id` is the session proxy. The
        -- composite PRIMARY KEY makes a re-render within the same session
        -- a no-op INSERT OR IGNORE, so distinct-session counts never
        -- double-count. `learning_id` FKs `learnings` so a hard-deleted
        -- learning takes its hits with it; `org_id` FKs `organizations`
        -- so an org delete cascades (mirrors the `learnings` table).
        CREATE TABLE IF NOT EXISTS learning_hits (
            learning_id  TEXT NOT NULL,
            agent_id     TEXT NOT NULL,
            org_id       TEXT NOT NULL DEFAULT 'default-org',
            rendered_at  TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (learning_id, agent_id),
            FOREIGN KEY (learning_id) REFERENCES learnings(id) ON DELETE CASCADE,
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
        );
        -- Windowed COUNT(*) over (learning_id, rendered_at) drives the
        -- promotion-threshold check.
        CREATE INDEX IF NOT EXISTS idx_learning_hits_learning_time
            ON learning_hits(learning_id, rendered_at);

        -- Promotion candidates (Phase 4, Task 4.1). One row per learning,
        -- written the first time its distinct-session hit count crosses
        -- the threshold inside the rolling window. PRIMARY KEY on
        -- `learning_id` + INSERT OR IGNORE keeps the emission idempotent —
        -- a learning is flagged once, not on every subsequent render.
        CREATE TABLE IF NOT EXISTS promotion_candidates (
            learning_id  TEXT PRIMARY KEY,
            org_id       TEXT NOT NULL DEFAULT 'default-org',
            hit_count    INTEGER NOT NULL,
            threshold    INTEGER NOT NULL,
            window_days  INTEGER NOT NULL,
            created_at   TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (learning_id) REFERENCES learnings(id) ON DELETE CASCADE,
            FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE
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

    // Per-project opt-in for worktree-per-agent isolation (ADR 0002). The
    // plan-config PUT (3.5.3) gates `parallel = true` on this column AND
    // the compile-time `WORKTREES_SHIPPED` const. The toggling endpoint
    // ships with the worktree plan; until then the column is forward-
    // compatible storage that can only be 1 via direct SQL. Lives on
    // `plan_project` because plan-name is the project handle in standalone
    // Branchwork.
    conn.execute_batch(
        "ALTER TABLE plan_project ADD COLUMN worktree_isolation_opt_in INTEGER NOT NULL DEFAULT 0;",
    )
    .ok();

    // `restored_at` flips a snapshot row from "available for Undo" to
    // "already restored". Set by `POST /api/snapshots/:id/restore`
    // (plan-deletion 0.4); a NULL value means the snapshot is still
    // recoverable. Kept on the row (rather than deleting it) so the
    // audit trail can prove "this delete was undone at T".
    conn.execute_batch("ALTER TABLE plan_snapshots ADD COLUMN restored_at TEXT;")
        .ok();

    // Last-known driver inventory the runner pushed via `RunnerHello` or
    // `DriverAuthReport`. JSON-encoded `Vec<DriverAuthInfo>` (see
    // `saas::runner_protocol::DriverAuthInfo`). Persisted so the dashboard
    // can surface a runner's drivers + auth state even while the runner is
    // offline; refreshed on every report. NULL until the runner has
    // reported once.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN drivers_json TEXT;")
        .ok();

    // Single-use binding for runner enrolment tokens: `claimed_runner_id`
    // is NULL until the first runner connects with this token, then bound
    // to that runner's id. Subsequent connects with the same token but a
    // different runner_id are rejected by `claim_or_verify_token`. Existing
    // rows pre-migration are left NULL and behave as unclaimed (the first
    // reconnect re-binds them in place).
    conn.execute_batch("ALTER TABLE runner_tokens ADD COLUMN claimed_runner_id TEXT;")
        .ok();

    // Soft-delete marker for revoked runners (DELETE /api/runners/{id}).
    // NULL means the runner is active; a datetime stamp means the operator
    // revoked the runner from the dashboard. We keep the row so historic
    // `agents.runner_id` references stay resolvable, but `list_runners`
    // filters them out so the UI doesn't show ghosts. The companion
    // revoke step deletes every `runner_tokens` row for the runner_name +
    // org so the next reconnect attempt fails the WS upgrade with 401.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN removed_at TEXT;")
        .ok();

    // Per-runner health snapshot (T11.3). Populated from
    // `WireMessage::RunnerHealth` ticks (~30 s cadence, best-effort). The
    // server overwrites in place — only the latest snapshot matters; missed
    // ticks during a flap are replaced by the next surviving tick. Every
    // column nullable so a runner that never reported (offline at first
    // boot) doesn't break `list_runners`.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN outbox_depth INTEGER;")
        .ok();
    conn.execute_batch("ALTER TABLE runners ADD COLUMN ws_reconnects_24h INTEGER;")
        .ok();
    conn.execute_batch("ALTER TABLE runners ADD COLUMN ci_poll_ms_p50 INTEGER;")
        .ok();
    conn.execute_batch("ALTER TABLE runners ADD COLUMN ci_poll_ms_p99 INTEGER;")
        .ok();
    // Wall-clock the most recent `RunnerHealth` was applied. Lets the UI
    // chip distinguish "metrics fresh" from "metrics stale (runner went
    // offline mid-poll)" without a separate query.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN last_health_at TEXT;")
        .ok();

    // Task 11.6: orphans reaped in the trailing 24 h window. Bumped each
    // time the runner finds a stale session socket on startup or reaps a
    // detached zombie via waitpid mid-flight. Stays NULL until the first
    // RunnerHealth tick after the runner upgrades to a 11.6+ build, then
    // is overwritten in place — same write-through pattern as the other
    // RunnerHealth columns.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN orphans_reaped_24h INTEGER;")
        .ok();

    // runner-install-and-spawn-reliability T1.2: server-bin self-diagnostic.
    // Populated from `RunnerHello.server_bin` (added in T1.2) so the
    // dashboard can render a green check + path or red cross + reason next
    // to the runner row, surfacing a missing `branchwork-server` BEFORE
    // the first Start session click. Mutually exclusive in practice (the
    // wire enum is tagged `Found`/`NotFound`), but stored in two columns
    // so a NULL on either side keeps the back-compat path (older runners
    // that don't send the field) trivially representable. Older rows
    // pre-T1.2 leave both NULL and the dashboard renders a neutral chip.
    conn.execute_batch("ALTER TABLE runners ADD COLUMN server_bin_path TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE runners ADD COLUMN server_bin_error TEXT;")
        .ok();

    // T1.3: operator-set override that lifts the version-mismatch dispatch
    // block. When the runner's severity is `Red` the dispatcher refuses to
    // send `StartAgent` unless this column is non-zero. The override is
    // per-runner (not per-org) so an operator can selectively re-enable a
    // known-old runner without weakening the rule everywhere. Cleared
    // implicitly when severity returns to Amber/Green (the dispatcher just
    // doesn't consult the column in that case). 0 = block (default),
    // 1 = "I know what I'm doing — connect anyway".
    conn.execute_batch(
        "ALTER TABLE runners ADD COLUMN version_mismatch_override INTEGER NOT NULL DEFAULT 0;",
    )
    .ok();

    // T4.3: runner-detected upgrade availability. Written by the
    // `RunnerHello` handler from the wire `upgrade_available` field; the
    // runner sets the local flag either after a `Resume` whose embedded
    // `server_version` is higher than its own `CARGO_PKG_VERSION`, or
    // after the periodic `install-runner.sh` poll (for offline-ish
    // runners) discovers a newer binary on offer. Lets the dashboard
    // light up the Upgrade pill without the operator having to spot
    // drift manually. 0 = no upgrade, 1 = upgrade available. NOT NULL
    // with default 0 so pre-T4.3 rows (and runners that don't send the
    // field) stay quiet.
    conn.execute_batch(
        "ALTER TABLE runners ADD COLUMN upgrade_available INTEGER NOT NULL DEFAULT 0;",
    )
    .ok();

    // Per-plan runner affinity (T11.4). One row per pinned plan: presence
    // of the row + `runner_id` set means "this runner only"; absent row
    // means "any online runner" (the historic `pick_runner_for_org`
    // behaviour). Dispatch consults this on every spawn; if the pinned
    // runner is offline, the plan is paused with
    // `paused_reason='runner_offline'`. Lives in its own table to mirror
    // the existing per-plan-config pattern (`plan_auto_mode`,
    // `plan_auto_advance`, `plan_budget`) and avoid touching the
    // `plan_project.project NOT NULL` column for plans without a project
    // override.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plan_runner_affinity (
             plan_name  TEXT PRIMARY KEY,
             runner_id  TEXT NOT NULL,
             org_id     TEXT NOT NULL DEFAULT 'default-org',
             updated_at TEXT NOT NULL DEFAULT (datetime('now'))
         );",
    )
    .ok();

    // Per-plan runner failover policy (T11.5). Sibling-failover opt-in for
    // pinned plans: when the pinned runner goes offline, do we pause the
    // plan (today's behaviour) or re-dispatch to a sibling online runner?
    // Stored on `plan_runner_affinity` rather than `plan_project` because
    // (a) failover is only meaningful when there IS a pin to fail over
    // from, and (b) `plan_project.project` is NOT NULL so plans without a
    // project override can't carry per-plan policy without the empty-
    // string sentinel trap that T11.4 explicitly avoided. NULL row in
    // `plan_runner_affinity` => unpinned => no failover concept (today's
    // pick_runner_for_org behaviour stays). Pinned + 'pause' (default) =>
    // T11.4's pause-on-offline. Pinned + 'sibling' => redirect to a
    // sibling online runner.
    conn.execute_batch(
        "ALTER TABLE plan_runner_affinity ADD COLUMN runner_failover TEXT NOT NULL DEFAULT 'pause';",
    )
    .ok();

    // Dirty-tree file list captured at pause time (T3.1 of the
    // dirty-tree-check plan). JSON-encoded `Vec<String>` so the dashboard
    // can render `pausedFiles: ["server.log", ...]` next to
    // `pausedReason: "agent_left_uncommitted_work"`. NULL when the pause
    // has no file context (every non-dirty-tree reason — merge_conflict,
    // ci_failed, etc.) — same column doubles as a discriminator. Capped
    // at the call site (5 files today) to match the broadcast payload;
    // helper stores whatever it's handed.
    conn.execute_batch("ALTER TABLE plan_auto_mode ADD COLUMN paused_files TEXT;")
        .ok();

    // Per-plan merge-cadence override (Task 1.2 of the
    // ci-cadence-build-vs-test-configurable plan). Nullable: NULL means
    // "inherit the project default from `branchwork.toml` [auto_mode]
    // merge_cadence" (which itself defaults to `phase`). Allowed values:
    // 'task' | 'phase' | 'plan'. The one-shot grandfather migration in
    // `migrations::spawn_grandfather_merge_cadence` writes 'task' on every
    // plan known at upgrade time so legacy auto-mode behaviour (merge after
    // every task) is preserved; plans created after that migration leave
    // the column NULL and inherit the project default ('phase').
    conn.execute_batch("ALTER TABLE plan_auto_mode ADD COLUMN merge_cadence TEXT;")
        .ok();

    // Per-agent cadence-deferral marker (Task 2.2 of the
    // ci-cadence-build-vs-test-configurable plan). NULL = unfilled / not
    // applicable; 'deferred_for_cadence' = the agent completed cleanly
    // but `should_merge_now` returned false at the time, so the merge
    // step was skipped. The next completion that flips
    // `should_merge_now` to true drains every row carrying this marker
    // (in dependency order) before merging itself, so a phase- or
    // plan-cadence boundary produces a single batched master push
    // instead of one push per completed task. Cleared on successful
    // merge (the agent's `branch` column is also cleared by the
    // inner merge helper).
    conn.execute_batch("ALTER TABLE agents ADD COLUMN merge_status TEXT;")
        .ok();

    // Structured spawn-failure message set by the SaaS runner_ws handler
    // when it receives an `AgentSpawnFailed` envelope from the runner
    // (Task 1.1, runner-install-and-spawn-reliability plan). Pre-fix the
    // runner just logged `failed to spawn agent <id>: <err>` and stayed
    // silent — the dashboard showed a `failed` row with no actionable
    // reason. The column carries the user-facing rendering string (e.g.
    // "runner could not spawn: /usr/local/bin/branchwork-server (ENOENT)")
    // so the dashboard surfaces the same copy regardless of which
    // backend recorded it. NULL on every successful spawn and on every
    // failure path that isn't a `Command::spawn` Err (those still flow
    // through the existing `stop_reason` column via `AgentStopped`).
    conn.execute_batch("ALTER TABLE agents ADD COLUMN spawn_error TEXT;")
        .ok();

    // Optional project FK on `agents` and `plan_project` (Phase 2.1 of
    // runner-daemon-workspace). NULL on legacy rows. Future plans created
    // via the new "New Project" flow will carry a non-NULL value; the
    // dashboard can then render "agent X belongs to project Y" without
    // re-parsing `cwd`. No FK constraint declared — SQLite enforces FKs
    // only when `PRAGMA foreign_keys = ON`, and the rest of the schema
    // intentionally does not depend on that pragma. Project deletion
    // logic in `api::projects::delete_project` explicitly nulls these
    // columns (or leaves them dangling for the operator to clean up).
    conn.execute_batch("ALTER TABLE agents ADD COLUMN project_id TEXT;")
        .ok();
    conn.execute_batch("ALTER TABLE plan_project ADD COLUMN project_id TEXT;")
        .ok();

    // Spawning runner identity (Task 5.5,
    // runner-install-and-spawn-reliability plan). Populated by
    // `spawn_ops::start_agent_via_runner` in SaaS mode with the runner
    // that received the `StartAgent` envelope (after sibling-failover
    // resolution); NULL on standalone-mode rows (no runner involved) and
    // on legacy rows that predate this column.
    //
    // Read by `api::agents::finish_agent` so graceful-exit targets the
    // runner that actually owns the session daemon, instead of falling
    // back to `pick_runner_for_org` ("most recently seen online") which
    // could ship `AgentInput` to a runner that has no PTY for this agent.
    // The same column is what the version-mismatch gate at Finish time
    // consults to refuse dispatch when the spawning runner is below the
    // minimum that handles `AgentInput`.
    conn.execute_batch("ALTER TABLE agents ADD COLUMN runner_id TEXT;")
        .ok();

    // Phase 1, Task 1.2: pending-learning gate. A `ci_failure_events` row is
    // "pending" while `resolved_at IS NULL` — it blocks the task from being
    // marked `completed` until a learning is captured (see
    // `pending_ci_failures` and `mark_ci_failures_resolved`). Existing rows
    // pre-1.2 default to NULL (pending) so the gate engages immediately on
    // upgrade; that's safe because the only way to clear NULL is to write a
    // learning, which the agent / dashboard can always do. Partial index
    // accelerates the gate's hot-path lookup (per-task scan filtered to
    // unresolved rows only).
    conn.execute_batch("ALTER TABLE ci_failure_events ADD COLUMN resolved_at TEXT;")
        .ok();
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_ci_failure_events_pending \
            ON ci_failure_events(plan_name, task_number) \
            WHERE resolved_at IS NULL;",
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
        assert!(tables.contains(&"plan_snapshots".to_string()));
        assert!(tables.contains(&"plan_runner_affinity".to_string()));
        assert!(tables.contains(&"master_push_lock".to_string()));
        assert!(tables.contains(&"projects".to_string()));
        assert!(tables.contains(&"credentials".to_string()));
        assert!(tables.contains(&"ci_failure_events".to_string()));
        assert!(tables.contains(&"learnings".to_string()));
        assert!(tables.contains(&"learning_hits".to_string()));
        assert!(tables.contains(&"promotion_candidates".to_string()));
    }

    /// Phase 1, Task 1.1: `ci_failure_events` schema round-trip plus
    /// the load-bearing UNIQUE INDEX on `(agent_id, run_id)`. Inserting
    /// the same pair twice must fail at the DB layer — that is the
    /// substrate `record_ci_failure_observed` rides for idempotency.
    #[test]
    fn ci_failure_events_unique_per_agent_and_run() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id, \
                  run_url, workflow, conclusion, failed_job, summary) \
             VALUES ('agent-1', 'plan-a', '1.1', 'branchwork/x/1.1', \
                     '12345', 'https://gh/.../runs/12345', \
                     'tests.yml', 'failure', NULL, 'CI tripped')",
            [],
        )
        .unwrap();

        let dup = conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id) \
             VALUES ('agent-1', 'plan-a', '1.1', 'branchwork/x/1.1', '12345')",
            [],
        );
        assert!(
            dup.is_err(),
            "second insert with same (agent_id, run_id) must trip the UNIQUE index"
        );

        // Different agent, same run id ⇒ allowed (a fix-agent on the
        // same branch is its own learning author).
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id) \
             VALUES ('agent-2', 'plan-a', '1.1', 'branchwork/x/1.1', '12345')",
            [],
        )
        .expect("different agent_id ⇒ different (agent_id, run_id) tuple ⇒ allowed");

        // Same agent, different run id ⇒ allowed (the agent may
        // observe multiple red runs across time).
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id) \
             VALUES ('agent-1', 'plan-a', '1.1', 'branchwork/x/1.1', '99999')",
            [],
        )
        .expect("different run_id ⇒ allowed");

        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM ci_failure_events WHERE plan_name = 'plan-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 3, "three legitimate rows survived the constraint");
    }

    /// Defaults: `observed_at = datetime('now')`, `org_id = 'default-org'`
    /// when unspecified, optional metadata columns nullable.
    #[test]
    fn ci_failure_events_defaults_round_trip() {
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, branch, run_id) \
             VALUES ('agent-1', 'plan-a', 'branchwork/x/1.1', '12345')",
            [],
        )
        .unwrap();

        type Defaults = (
            String,         // org_id
            Option<String>, // task_number
            Option<String>, // run_url
            Option<String>, // workflow
            Option<String>, // conclusion
            Option<String>, // failed_job
            Option<String>, // summary
            String,         // observed_at
        );
        let (org_id, task_number, run_url, workflow, conclusion, failed_job, summary, observed_at): Defaults = conn
            .query_row(
                "SELECT org_id, task_number, run_url, workflow, \
                        conclusion, failed_job, summary, observed_at \
                   FROM ci_failure_events WHERE agent_id = 'agent-1'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(org_id, "default-org");
        assert_eq!(task_number, None);
        assert_eq!(run_url, None);
        assert_eq!(workflow, None);
        assert_eq!(conclusion, None);
        assert_eq!(failed_job, None);
        assert_eq!(summary, None);
        assert!(
            !observed_at.is_empty(),
            "observed_at must default to datetime('now')"
        );
    }

    // ── Phase 1, Task 1.2: pending-learning gate ───────────────────────────

    /// Seed a `ci_failure_events` row with a given `resolved_at` value so
    /// each pending-learning test can exercise either the pending or
    /// resolved branch directly.
    fn seed_ci_failure(
        db: &Db,
        plan_name: &str,
        task_number: Option<&str>,
        run_id: &str,
        resolved_at: Option<&str>,
    ) -> i64 {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO ci_failure_events \
                 (agent_id, plan_name, task_number, branch, run_id, \
                  run_url, workflow, conclusion, summary, resolved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                format!("agent-{}", run_id),
                plan_name,
                task_number,
                "branchwork/x/0.1",
                run_id,
                format!("https://gh/.../runs/{}", run_id),
                "tests.yml",
                "failure",
                format!("workflow tests.yml failed (run {})", run_id),
                resolved_at,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn pending_ci_failures_returns_only_unresolved_for_the_task() {
        let (db, _dir) = test_db();
        seed_ci_failure(&db, "p", Some("1.1"), "111", None); // pending
        seed_ci_failure(&db, "p", Some("1.1"), "112", Some("2026-05-23T10:00:00")); // resolved
        seed_ci_failure(&db, "p", Some("1.2"), "113", None); // different task
        seed_ci_failure(&db, "p", None, "114", None); // null task — not the target
        let pending = pending_ci_failures(&db, "p", "1.1");
        assert_eq!(pending.len(), 1, "only the unresolved 1.1 row qualifies");
        assert_eq!(pending[0].run_id, "111");
        assert_eq!(pending[0].workflow.as_deref(), Some("tests.yml"));
        assert_eq!(pending[0].conclusion.as_deref(), Some("failure"));
    }

    #[test]
    fn pending_ci_failures_returns_empty_when_no_rows_or_all_resolved() {
        let (db, _dir) = test_db();
        assert!(pending_ci_failures(&db, "p", "1.1").is_empty());
        seed_ci_failure(&db, "p", Some("1.1"), "111", Some("2026-05-23T10:00:00"));
        assert!(
            pending_ci_failures(&db, "p", "1.1").is_empty(),
            "resolved row must not surface"
        );
    }

    #[test]
    fn pending_ci_failures_orders_by_observed_at_ascending() {
        let (db, _dir) = test_db();
        // SQLite datetime('now') resolution is 1s; insert in known order
        // and use the (id) tie-breaker to pin a deterministic sort.
        let id_a = seed_ci_failure(&db, "p", Some("1.1"), "111", None);
        let id_b = seed_ci_failure(&db, "p", Some("1.1"), "222", None);
        let id_c = seed_ci_failure(&db, "p", Some("1.1"), "333", None);
        let pending = pending_ci_failures(&db, "p", "1.1");
        let ids: Vec<i64> = pending.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![id_a, id_b, id_c]);
    }

    #[test]
    fn mark_ci_failures_resolved_flips_all_unresolved_for_the_task() {
        let (db, _dir) = test_db();
        seed_ci_failure(&db, "p", Some("1.1"), "111", None);
        seed_ci_failure(&db, "p", Some("1.1"), "112", None);
        seed_ci_failure(&db, "p", Some("1.2"), "113", None);
        let count = mark_ci_failures_resolved(&db, "p", "1.1");
        assert_eq!(count, 2, "both 1.1 rows resolved, 1.2 untouched");
        assert!(pending_ci_failures(&db, "p", "1.1").is_empty());
        assert_eq!(pending_ci_failures(&db, "p", "1.2").len(), 1);
    }

    #[test]
    fn mark_ci_failures_resolved_is_idempotent() {
        let (db, _dir) = test_db();
        seed_ci_failure(&db, "p", Some("1.1"), "111", None);
        assert_eq!(mark_ci_failures_resolved(&db, "p", "1.1"), 1);
        // Second call matches no pending rows.
        assert_eq!(mark_ci_failures_resolved(&db, "p", "1.1"), 0);
        // And does not unset the resolution.
        assert!(pending_ci_failures(&db, "p", "1.1").is_empty());
    }

    #[test]
    fn mark_ci_failures_resolved_does_not_touch_other_plans_or_tasks() {
        let (db, _dir) = test_db();
        seed_ci_failure(&db, "p", Some("1.1"), "111", None);
        seed_ci_failure(&db, "q", Some("1.1"), "222", None);
        let count = mark_ci_failures_resolved(&db, "p", "1.1");
        assert_eq!(count, 1);
        assert_eq!(
            pending_ci_failures(&db, "q", "1.1").len(),
            1,
            "cross-plan rows must be untouched"
        );
    }

    /// Resolved column must accept NULL on existing pre-1.2 rows (the
    /// ALTER TABLE ADD COLUMN backfills NULL by default) and round-trip
    /// a TEXT timestamp.
    #[test]
    fn ci_failure_events_resolved_at_column_round_trips() {
        let (db, _dir) = test_db();
        let id = seed_ci_failure(&db, "p", Some("1.1"), "111", None);
        let conn = db.lock().unwrap();
        let resolved_at: Option<String> = conn
            .query_row(
                "SELECT resolved_at FROM ci_failure_events WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved_at, None);
        drop(conn);

        // Update via the helper and re-read.
        mark_ci_failures_resolved(&db, "p", "1.1");
        let conn = db.lock().unwrap();
        let resolved_at: Option<String> = conn
            .query_row(
                "SELECT resolved_at FROM ci_failure_events WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            resolved_at.is_some_and(|s| !s.is_empty()),
            "resolved_at must be a non-empty timestamp post-resolve"
        );
    }

    #[test]
    fn plan_snapshots_migration_idempotent_on_existing_db() {
        // Run migrate once, write a row to a pre-existing table, then
        // re-init from the same path. The CREATE TABLE IF NOT EXISTS
        // pattern (and idempotent CREATE INDEX) must leave the seeded
        // data intact and still produce a usable plan_snapshots table.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");

        {
            let db = init(&path);
            let conn = db.lock().unwrap();
            // `source='manual'` so `cleanup_stale_auto_completed`
            // (which purges legacy auto-inferred rows) leaves it
            // alone — we want to verify migration idempotence, not
            // legacy cleanup behaviour.
            conn.execute(
                "INSERT INTO task_status (plan_name, task_number, status, source) \
                 VALUES ('survives', '1.1', 'completed', 'manual')",
                [],
            )
            .unwrap();
        }

        // Second init must not drop existing data and must leave
        // plan_snapshots in place.
        let db = init(&path);
        let conn = db.lock().unwrap();
        let task_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_status WHERE plan_name = 'survives'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(task_count, 1, "existing data was dropped on re-migrate");

        // plan_snapshots must be present and writable.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(tables.contains(&"plan_snapshots".to_string()));

        conn.execute(
            "INSERT INTO plan_snapshots \
                 (plan_name, kind, expires_at, yaml_body, cascade_json) \
             VALUES ('p', 'delete', datetime('now', '+30 days'), 'body', '{}')",
            [],
        )
        .unwrap();

        // Both indexes must exist (used by the purge job + plan lookup).
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' \
                 AND tbl_name='plan_snapshots'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(indexes.contains(&"idx_plan_snapshots_expires".to_string()));
        assert!(indexes.contains(&"idx_plan_snapshots_plan".to_string()));
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
    fn agents_table_has_runner_id_column() {
        // Pinned by Task 5.5 of the runner-install-and-spawn-reliability
        // plan. The column carries the runner that owned the spawn so
        // `finish_agent` can target it (and the version-mismatch gate
        // there can consult it) rather than falling back to
        // `pick_runner_for_org`, which can pick a runner that has no PTY
        // for this agent.
        let (db, _dir) = test_db();
        let conn = db.lock().unwrap();

        conn.execute(
            "INSERT INTO agents (id, cwd, status, runner_id) VALUES (?1, ?2, ?3, ?4)",
            params!["a1", "/tmp", "running", "runner-abc"],
        )
        .unwrap();
        let rid: Option<String> = conn
            .query_row(
                "SELECT runner_id FROM agents WHERE id = ?1",
                params!["a1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rid.as_deref(), Some("runner-abc"));

        // NULL when not provided (e.g. standalone-mode rows / legacy rows).
        conn.execute(
            "INSERT INTO agents (id, cwd, status) VALUES (?1, ?2, ?3)",
            params!["a2", "/tmp", "running"],
        )
        .unwrap();
        let rid2: Option<String> = conn
            .query_row(
                "SELECT runner_id FROM agents WHERE id = ?1",
                params!["a2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rid2, None);
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

        auto_mode_pause(&db, "p1", "merge_conflict", None);
        assert!(
            !auto_mode_enabled(&db, "p1"),
            "paused plan must report not-enabled"
        );

        // Inspect the row directly: paused_reason and paused_at landed,
        // enabled is preserved, and paused_files stays NULL (no file
        // context for a merge_conflict pause).
        let conn = db.lock().unwrap();
        let (enabled, reason, paused_at, paused_files): (
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT enabled, paused_reason, paused_at, paused_files \
                 FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(enabled, 1, "pause must not flip enabled");
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
        assert!(paused_at.is_some(), "paused_at must be set");
        assert!(
            paused_files.is_none(),
            "merge_conflict pause has no file context"
        );
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

        auto_mode_pause(
            &db,
            "p1",
            "agent_left_uncommitted_work",
            Some(&["server.log".to_string(), "runner.log".to_string()]),
        );
        assert!(!auto_mode_enabled(&db, "p1"));

        // Verify pre-resume state: paused_files JSON landed.
        {
            let conn = db.lock().unwrap();
            let files: Option<String> = conn
                .query_row(
                    "SELECT paused_files FROM plan_auto_mode WHERE plan_name = ?1",
                    params!["p1"],
                    |row| row.get(0),
                )
                .unwrap();
            let parsed: Vec<String> =
                serde_json::from_str(files.as_deref().unwrap_or("[]")).unwrap();
            assert_eq!(
                parsed,
                vec!["server.log".to_string(), "runner.log".to_string()]
            );
        }

        auto_mode_resume(&db, "p1");
        assert!(
            auto_mode_enabled(&db, "p1"),
            "resume must restore acting state"
        );

        let conn = db.lock().unwrap();
        let (reason, paused_at, paused_files): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT paused_reason, paused_at, paused_files \
                 FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(reason, None);
        assert_eq!(paused_at, None);
        assert_eq!(paused_files, None, "resume must clear paused_files too");
    }

    #[test]
    fn auto_mode_pause_creates_row_when_missing() {
        // Defensive: if the loop pauses before the user toggled (or after
        // the row was deleted), the UPSERT must still record the reason.
        let (db, _dir) = test_db();
        auto_mode_pause(&db, "p1", "merge_conflict", None);

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

    /// Pause with a file list, then pause again without one (different
    /// reason). The second pause must clear paused_files, not leak the
    /// stale list onto an unrelated pause reason.
    #[test]
    fn auto_mode_pause_overwrites_files_on_subsequent_pause() {
        let (db, _dir) = test_db();
        auto_mode_pause(
            &db,
            "p1",
            "agent_left_uncommitted_work",
            Some(&["server.log".to_string()]),
        );
        auto_mode_pause(&db, "p1", "merge_conflict", None);

        let conn = db.lock().unwrap();
        let (reason, files): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT paused_reason, paused_files FROM plan_auto_mode WHERE plan_name = ?1",
                params!["p1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason.as_deref(), Some("merge_conflict"));
        assert!(
            files.is_none(),
            "stale paused_files must not leak across pause reasons"
        );
    }

    #[test]
    fn auto_mode_config_round_trips_paused_files() {
        let (db, _dir) = test_db();
        auto_mode_pause(
            &db,
            "p1",
            "agent_left_uncommitted_work",
            Some(&[
                "server.log".to_string(),
                "runner.log".to_string(),
                "web-dev.log".to_string(),
            ]),
        );
        let cfg = auto_mode_config(&db, "p1");
        assert_eq!(
            cfg.paused_reason.as_deref(),
            Some("agent_left_uncommitted_work")
        );
        assert_eq!(
            cfg.paused_files,
            Some(vec![
                "server.log".to_string(),
                "runner.log".to_string(),
                "web-dev.log".to_string(),
            ])
        );
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

    // ── merge_cadence (Task 1.2 of the cadence plan) ────────────────────

    #[test]
    fn merge_cadence_defaults_to_none_for_missing_or_unset_rows() {
        // Missing row entirely: helper falls back to None (inherit project default).
        let (db, _dir) = test_db();
        assert_eq!(plan_merge_cadence(&db, "missing"), None);
        assert!(auto_mode_config(&db, "missing").merge_cadence.is_none());

        // Row exists, merge_cadence omitted from INSERT: column NULL by
        // default — also None (inherit project default).
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["plan-no-cadence"],
            )
            .unwrap();
        }
        assert_eq!(plan_merge_cadence(&db, "plan-no-cadence"), None);
        assert!(
            auto_mode_config(&db, "plan-no-cadence")
                .merge_cadence
                .is_none(),
            "NULL merge_cadence column must surface as inherit (None)"
        );
    }

    #[test]
    fn set_plan_merge_cadence_round_trips_each_variant() {
        let (db, _dir) = test_db();
        for cadence in [MergeCadence::Task, MergeCadence::Phase, MergeCadence::Plan] {
            set_plan_merge_cadence(&db, "plan-a", Some(cadence));
            assert_eq!(
                plan_merge_cadence(&db, "plan-a"),
                Some(cadence),
                "set/get must round-trip {cadence:?}"
            );
            // And via the snapshot helper too.
            assert_eq!(auto_mode_config(&db, "plan-a").merge_cadence, Some(cadence));
        }
    }

    #[test]
    fn set_plan_merge_cadence_clear_resets_to_inherit() {
        let (db, _dir) = test_db();
        set_plan_merge_cadence(&db, "plan-a", Some(MergeCadence::Plan));
        assert_eq!(plan_merge_cadence(&db, "plan-a"), Some(MergeCadence::Plan));
        // Explicit None must NULL the column — back to "inherit project default".
        set_plan_merge_cadence(&db, "plan-a", None);
        assert_eq!(plan_merge_cadence(&db, "plan-a"), None);
    }

    #[test]
    fn set_plan_merge_cadence_preserves_sibling_columns() {
        // Critical: the partial-update UPSERT must not clobber `enabled`,
        // `max_fix_attempts`, or `paused_reason` — those belong to other
        // editor flows and the cadence write happens via its own button.
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode \
                   (plan_name, enabled, max_fix_attempts, paused_reason) \
                 VALUES (?1, 1, 7, 'merge_conflict')",
                params!["plan-a"],
            )
            .unwrap();
        }
        set_plan_merge_cadence(&db, "plan-a", Some(MergeCadence::Plan));
        let cfg = auto_mode_config(&db, "plan-a");
        assert!(cfg.enabled, "enabled must survive cadence write");
        assert_eq!(cfg.max_fix_attempts, 7, "max_fix_attempts must survive");
        assert_eq!(cfg.paused_reason.as_deref(), Some("merge_conflict"));
        assert_eq!(cfg.merge_cadence, Some(MergeCadence::Plan));
    }

    #[test]
    fn parse_merge_cadence_accepts_lowercase_and_rejects_others() {
        assert_eq!(parse_merge_cadence("task"), Some(MergeCadence::Task));
        assert_eq!(parse_merge_cadence("phase"), Some(MergeCadence::Phase));
        assert_eq!(parse_merge_cadence("plan"), Some(MergeCadence::Plan));
        // Unknown / mis-cased values collapse to None — caller treats as
        // inherit so a corrupt row never 500s the config endpoint.
        assert_eq!(parse_merge_cadence("Task"), None);
        assert_eq!(parse_merge_cadence("WEEKLY"), None);
        assert_eq!(parse_merge_cadence(""), None);
    }

    #[test]
    fn merge_cadence_wire_round_trips_through_parse() {
        for cadence in [MergeCadence::Task, MergeCadence::Phase, MergeCadence::Plan] {
            assert_eq!(
                parse_merge_cadence(merge_cadence_wire(cadence)),
                Some(cadence)
            );
        }
    }

    #[test]
    fn merge_cadence_migration_is_idempotent_and_preserves_existing_value() {
        // Same idempotency contract as the parallel column: re-running
        // `init` on a DB that already has merge_cadence written must
        // preserve the value (no DROP/RESET).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");

        {
            let db = init(&path);
            set_plan_merge_cadence(&db, "plan-pinned", Some(MergeCadence::Plan));
            // Another plan with NULL — verifies NULL survives unchanged too.
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO plan_auto_mode (plan_name, enabled) VALUES (?1, 1)",
                params!["plan-inherits"],
            )
            .unwrap();
        }

        let db2 = init(&path);
        assert_eq!(
            plan_merge_cadence(&db2, "plan-pinned"),
            Some(MergeCadence::Plan),
            "user-set merge_cadence must survive re-init"
        );
        assert_eq!(
            plan_merge_cadence(&db2, "plan-inherits"),
            None,
            "NULL merge_cadence must remain NULL after re-init"
        );
    }

    // ── runner_config ────────────────────────────────────────────────────

    fn seed_runner(db: &Db, runner_id: &str, org_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO runners (id, name, org_id, status) VALUES (?1, ?2, ?3, 'online')",
            params![runner_id, runner_id, org_id],
        )
        .unwrap();
    }

    #[test]
    fn runner_config_defaults_to_inherit_when_no_row() {
        // No row in `runner_config` => both fields are None, i.e. the
        // dispatcher will fall through to the server-wide defaults.
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        let cfg = runner_config(&db, "runner-a");
        assert!(cfg.effort.is_none());
        assert!(cfg.skip_permissions.is_none());
    }

    #[test]
    fn set_runner_config_round_trips_partial_overrides() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-b", "default-org");

        // Set both fields.
        set_runner_config(
            &db,
            "runner-b",
            "default-org",
            &RunnerConfig {
                effort: Some("max".into()),
                skip_permissions: Some(false),
            },
        );
        let cfg = runner_config(&db, "runner-b");
        assert_eq!(cfg.effort.as_deref(), Some("max"));
        assert_eq!(cfg.skip_permissions, Some(false));

        // UPSERT replaces the row, so passing only one override clears the
        // other (matches the API: callers must read-modify-write).
        set_runner_config(
            &db,
            "runner-b",
            "default-org",
            &RunnerConfig {
                effort: Some("low".into()),
                skip_permissions: None,
            },
        );
        let cfg = runner_config(&db, "runner-b");
        assert_eq!(cfg.effort.as_deref(), Some("low"));
        assert!(cfg.skip_permissions.is_none());
    }

    #[test]
    fn runner_config_cleared_to_all_inherit() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-c", "default-org");
        set_runner_config(
            &db,
            "runner-c",
            "default-org",
            &RunnerConfig {
                effort: Some("high".into()),
                skip_permissions: Some(true),
            },
        );
        // Default => both None => effectively cleared.
        set_runner_config(&db, "runner-c", "default-org", &RunnerConfig::default());
        let cfg = runner_config(&db, "runner-c");
        assert!(cfg.effort.is_none());
        assert!(cfg.skip_permissions.is_none());
    }

    // ── plan_runner_affinity (T11.4) ─────────────────────────────────────

    #[test]
    fn plan_runner_id_returns_none_when_unset() {
        let (db, _dir) = test_db();
        assert_eq!(plan_runner_id(&db, "missing-plan"), None);
    }

    #[test]
    fn set_plan_runner_id_round_trips() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        assert_eq!(
            plan_runner_id(&db, "plan-a"),
            Some("runner-a".to_string()),
            "explicit pin should round-trip via the helper"
        );
    }

    #[test]
    fn set_plan_runner_id_replaces_existing_pin() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        seed_runner(&db, "runner-b", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-b"));
        assert_eq!(plan_runner_id(&db, "plan-a"), Some("runner-b".to_string()));
    }

    #[test]
    fn set_plan_runner_id_clear_deletes_row() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        set_plan_runner_id(&db, "plan-a", "default-org", None);
        assert_eq!(plan_runner_id(&db, "plan-a"), None);

        // Clearing twice (or clearing a non-existent pin) is a silent no-op.
        set_plan_runner_id(&db, "plan-a", "default-org", None);
        let count: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM plan_runner_affinity WHERE plan_name = ?1",
                params!["plan-a"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 0);
    }

    #[test]
    fn plan_runner_affinity_does_not_touch_plan_project_project_column() {
        // Regression: the affinity table is intentionally separate from
        // `plan_project` so we never accidentally clobber a project
        // override (which is NOT NULL on that table) when setting a
        // runner pin for a plan with no project override yet.
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");

        // No plan_project row should be created or modified.
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        let pp_rows: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM plan_project WHERE plan_name = ?1",
                params!["plan-a"],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            pp_rows, 0,
            "setting runner pin must not touch plan_project (the project column is NOT NULL there)"
        );
    }

    // ── plan_runner_failover (T11.5) ─────────────────────────────────────

    #[test]
    fn plan_runner_failover_defaults_to_pause_for_unpinned_plan() {
        let (db, _dir) = test_db();
        // No `plan_runner_affinity` row inserted. The read helper must
        // synthesize the default policy so dispatch code never has to
        // special-case the absent-row path.
        assert_eq!(plan_runner_failover(&db, "unpinned-plan"), "pause");
    }

    #[test]
    fn plan_runner_failover_defaults_to_pause_on_fresh_pin() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        // The schema-level DEFAULT 'pause' kicks in on INSERT — pinning
        // a plan must NOT silently opt the user into sibling failover.
        assert_eq!(plan_runner_failover(&db, "plan-a"), "pause");
    }

    #[test]
    fn set_plan_runner_failover_round_trips_and_validates() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));

        assert_eq!(
            set_plan_runner_failover(&db, "plan-a", "sibling"),
            Ok(true),
            "valid policy on a pinned plan should update one row"
        );
        assert_eq!(plan_runner_failover(&db, "plan-a"), "sibling");

        assert_eq!(
            set_plan_runner_failover(&db, "plan-a", "pause"),
            Ok(true),
            "switching back to pause should also update one row"
        );
        assert_eq!(plan_runner_failover(&db, "plan-a"), "pause");

        // Invalid policies are rejected without touching the row.
        assert_eq!(set_plan_runner_failover(&db, "plan-a", "yolo"), Err(()));
        assert_eq!(plan_runner_failover(&db, "plan-a"), "pause");
    }

    #[test]
    fn set_plan_runner_failover_returns_false_for_unpinned_plan() {
        let (db, _dir) = test_db();
        // No pin row exists; the UPDATE matches zero rows.
        assert_eq!(
            set_plan_runner_failover(&db, "no-pin-plan", "sibling"),
            Ok(false),
            "setting failover without a pin should be a no-op so the API \
             layer can return 409/400 instead of silently writing nothing"
        );
        assert_eq!(plan_runner_failover(&db, "no-pin-plan"), "pause");
    }

    #[test]
    fn re_pinning_preserves_existing_failover_policy() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        seed_runner(&db, "runner-b", "default-org");

        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        assert_eq!(set_plan_runner_failover(&db, "plan-a", "sibling"), Ok(true));

        // Re-pin to a different runner — failover policy must survive.
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-b"));
        assert_eq!(
            plan_runner_failover(&db, "plan-a"),
            "sibling",
            "re-pinning must not silently revert sibling-failover to pause"
        );
    }

    #[test]
    fn clearing_pin_drops_failover_with_the_row() {
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-a", "default-org");
        set_plan_runner_id(&db, "plan-a", "default-org", Some("runner-a"));
        assert_eq!(set_plan_runner_failover(&db, "plan-a", "sibling"), Ok(true));

        set_plan_runner_id(&db, "plan-a", "default-org", None);
        // No row left ⇒ default 'pause' from the read helper.
        assert_eq!(plan_runner_failover(&db, "plan-a"), "pause");
    }

    #[test]
    fn runner_config_cascades_when_runner_deleted() {
        // FK ON DELETE CASCADE: removing a runner row scrubs its config row
        // so an old override doesn't quietly apply if the same id is
        // re-enrolled later.
        let (db, _dir) = test_db();
        seed_runner(&db, "runner-d", "default-org");
        set_runner_config(
            &db,
            "runner-d",
            "default-org",
            &RunnerConfig {
                effort: Some("max".into()),
                skip_permissions: Some(true),
            },
        );

        let cleared: i64 = {
            let conn = db.lock().unwrap();
            // SQLite needs FKs explicitly enabled per-connection. The
            // production code never deletes runners by hand, but make the
            // CASCADE behaviour visible here.
            conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
            conn.execute("DELETE FROM runners WHERE id = ?1", params!["runner-d"])
                .unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM runner_config WHERE runner_id = ?1",
                params!["runner-d"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(cleared, 0, "runner_config row should cascade-delete");
    }

    // ── master_push_lock (Phase 2 advisory lock) ────────────────────────────

    #[test]
    fn push_lock_acquire_returns_fresh_token_when_no_row_exists() {
        let (db, _dir) = test_db();
        let token = try_acquire_push_lock(
            &db,
            "master",
            "auto_mode",
            12345,
            Some("p1"),
            PUSH_LOCK_TTL_SECS,
        )
        .expect("first acquire should succeed");
        assert!(!token.is_empty());

        let holder = peek_push_lock(&db, "master").expect("row must exist after acquire");
        assert_eq!(holder.holder_token, token);
        assert_eq!(holder.holder_pid, 12345);
        assert_eq!(holder.holder_kind, "auto_mode");
        assert_eq!(holder.holder_meta.as_deref(), Some("p1"));
        // SQLite's strftime granularity is whole seconds — fresh row may
        // legitimately report 0s.
        assert!(holder.age_secs >= 0);
    }

    #[test]
    fn push_lock_second_acquire_refused_while_live_holder_exists() {
        let (db, _dir) = test_db();
        let first =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();

        let err = try_acquire_push_lock(&db, "master", "ci", 2, Some("run-7"), PUSH_LOCK_TTL_SECS)
            .expect_err("second acquire must be rejected by the live holder");
        // The error surface reports the EXISTING holder, not the
        // attempted caller's metadata — that's how the API endpoint can
        // tell the CI side who is currently pushing.
        assert_eq!(err.holder_token, first);
        assert_eq!(err.holder_pid, 1);
        assert_eq!(err.holder_kind, "auto_mode");
        // Holder row must still be the original.
        let holder = peek_push_lock(&db, "master").unwrap();
        assert_eq!(holder.holder_token, first);
    }

    #[test]
    fn push_lock_acquire_succeeds_after_ttl_eviction() {
        let (db, _dir) = test_db();
        // Seed a stale row directly so we don't have to wait 30s. Use a
        // very small TTL on the subsequent acquire to force the eviction
        // path inside the same SQLite tick.
        let _stale =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();
        // Backdate the row by 60s so age_secs > our 0-second TTL on
        // re-acquire. (Using 0 means "anything past 0 seconds old can
        // be evicted" — fits inside one SQLite second tick.)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE master_push_lock SET taken_at = datetime('now','-60 seconds') \
                 WHERE branch = ?1",
                params!["master"],
            )
            .unwrap();
        }
        let fresh = try_acquire_push_lock(&db, "master", "ci", 99, Some("run-8"), 0)
            .expect("acquire must succeed once the stale row's age exceeds TTL");
        let holder = peek_push_lock(&db, "master").unwrap();
        assert_eq!(holder.holder_token, fresh);
        assert_eq!(holder.holder_kind, "ci");
        assert_eq!(holder.holder_meta.as_deref(), Some("run-8"));
    }

    #[test]
    fn push_lock_release_with_matching_token_drops_the_row() {
        let (db, _dir) = test_db();
        let token =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();

        assert!(release_push_lock(&db, "master", &token));
        assert!(peek_push_lock(&db, "master").is_none());
    }

    #[test]
    fn push_lock_release_with_wrong_token_leaves_row_intact() {
        let (db, _dir) = test_db();
        let _real =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();

        assert!(!release_push_lock(
            &db,
            "master",
            "definitely-not-the-token"
        ));
        assert!(
            peek_push_lock(&db, "master").is_some(),
            "wrong-token release must NOT delete the row"
        );
    }

    #[test]
    fn push_lock_release_on_absent_row_is_a_silent_no_op() {
        let (db, _dir) = test_db();
        assert!(!release_push_lock(&db, "main", "any-token"));
    }

    #[test]
    fn push_lock_is_per_branch_independent() {
        let (db, _dir) = test_db();
        let t_master =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();
        let t_main =
            try_acquire_push_lock(&db, "main", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();
        assert_ne!(
            t_master, t_main,
            "different branches must get different tokens"
        );
        // Both rows coexist.
        assert!(peek_push_lock(&db, "master").is_some());
        assert!(peek_push_lock(&db, "main").is_some());
    }

    #[test]
    fn push_lock_reacquire_after_release_returns_a_new_token() {
        let (db, _dir) = test_db();
        let first =
            try_acquire_push_lock(&db, "master", "auto_mode", 1, None, PUSH_LOCK_TTL_SECS).unwrap();
        assert!(release_push_lock(&db, "master", &first));
        let second =
            try_acquire_push_lock(&db, "master", "ci", 2, None, PUSH_LOCK_TTL_SECS).unwrap();
        assert_ne!(
            first, second,
            "token must be fresh per acquire (callers rely on this for auth)"
        );
    }

    // ── Credentials (Phase 3.1 of runner-daemon-workspace) ──────────────

    /// Seed a user that the credential FK can reference. Returns the
    /// user id.
    fn seed_user(db: &Db, id: &str, email: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash) VALUES (?1, ?2, ?3)",
            params![id, email, "x"],
        )
        .unwrap();
    }

    fn sample_input<'a>(
        id: &'a str,
        owner_user_id: &'a str,
        secret: &'a [u8],
    ) -> CredentialInput<'a> {
        CredentialInput {
            id,
            owner_user_id,
            name: "test cred",
            kind: CredentialKind::GhPat,
            public_part: Some("fingerprint:ab12"),
            secret_plaintext: secret,
            host_hint: Some("github.com"),
            scopes: Some("repo,workflow"),
        }
    }

    #[test]
    fn credential_kind_round_trips_wire_form() {
        assert_eq!(CredentialKind::SshKey.as_str(), "ssh_key");
        assert_eq!(CredentialKind::GhPat.as_str(), "gh_pat");
        assert_eq!(CredentialKind::GitlabPat.as_str(), "gitlab_pat");
        assert_eq!(
            CredentialKind::parse("ssh_key"),
            Some(CredentialKind::SshKey)
        );
        assert_eq!(CredentialKind::parse("gh_pat"), Some(CredentialKind::GhPat));
        assert_eq!(
            CredentialKind::parse("gitlab_pat"),
            Some(CredentialKind::GitlabPat)
        );
        assert_eq!(CredentialKind::parse("unknown"), None);
        assert_eq!(CredentialKind::parse(""), None);
    }

    /// Headline acceptance test: round-trip set/get returns the original
    /// secret bytes. Also pins that plaintext NEVER lands in the
    /// `encrypted_secret` column.
    #[test]
    fn set_get_credential_round_trip_returns_original_bytes() {
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");

        let key = crate::crypto::MasterKey::generate();
        let secret = b"ghp_supersecrettoken_AAA1234567";
        let input = sample_input("cred-1", "u1", secret);
        set_credential(&db, &key, &input).expect("set_credential");

        let got = get_credential(&db, &key, "cred-1").expect("get_credential");
        assert_eq!(got.id, "cred-1");
        assert_eq!(got.owner_user_id, "u1");
        assert_eq!(got.name, "test cred");
        assert_eq!(got.kind, CredentialKind::GhPat);
        assert_eq!(got.public_part.as_deref(), Some("fingerprint:ab12"));
        assert_eq!(got.host_hint.as_deref(), Some("github.com"));
        assert_eq!(got.scopes.as_deref(), Some("repo,workflow"));
        assert_eq!(got.secret_plaintext.as_slice(), secret);
        assert!(got.archived_at.is_none());

        // Critical: the BLOB on disk must NOT contain the plaintext.
        let conn = db.lock().unwrap();
        let on_disk: Vec<u8> = conn
            .query_row(
                "SELECT encrypted_secret FROM credentials WHERE id = ?1",
                params!["cred-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(on_disk.as_slice(), secret, "plaintext leaked to DB");
        // The first 12 bytes are the nonce; the rest is ciphertext + tag.
        // Tag is 16 bytes for AES-GCM.
        assert!(on_disk.len() >= crate::crypto::NONCE_LEN + secret.len() + 16);
    }

    /// Acceptance test for the rotation knob: encrypting under key A then
    /// re-keying to B must invalidate every existing row. The DB layer
    /// surfaces this as `CredentialError::Crypto(DecryptionFailed)`.
    #[test]
    fn rotating_master_key_invalidates_existing_credential_rows() {
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");

        let key_a = crate::crypto::MasterKey::generate();
        let secret = b"glpat-AAAAAAAAAAAAAAAAAA";
        set_credential(&db, &key_a, &sample_input("cred-1", "u1", secret)).unwrap();

        // Same key — fine.
        let ok = get_credential(&db, &key_a, "cred-1").unwrap();
        assert_eq!(ok.secret_plaintext.as_slice(), secret);

        // Rotated key — fails with DecryptionFailed. AES-GCM does not
        // distinguish wrong-key from tampered-ciphertext, which is the
        // canonical knob the task acceptance pins.
        let key_b = crate::crypto::MasterKey::generate();
        assert_ne!(key_a.as_bytes(), key_b.as_bytes());
        let err = get_credential(&db, &key_b, "cred-1").unwrap_err();
        match err {
            CredentialError::Crypto(crate::crypto::CryptoError::DecryptionFailed) => {}
            other => panic!("expected DecryptionFailed, got {other:?}"),
        }
    }

    #[test]
    fn get_credential_returns_not_found_for_missing_id() {
        let (db, _dir) = test_db();
        let key = crate::crypto::MasterKey::generate();
        let err = get_credential(&db, &key, "does-not-exist").unwrap_err();
        assert!(matches!(err, CredentialError::NotFound));
    }

    #[test]
    fn nonces_differ_across_two_writes_of_the_same_secret() {
        // The encryption layer asserts this, but pin it again at the DB
        // boundary because the wire shape (nonce || ciphertext || tag) is
        // the contract callers will eventually read directly via raw SQL
        // for backup/restore tooling. Two writes of the same plaintext
        // must produce different BLOBs.
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");
        let key = crate::crypto::MasterKey::generate();
        let secret = b"same-secret-bytes";
        set_credential(&db, &key, &sample_input("cred-1", "u1", secret)).unwrap();
        set_credential(&db, &key, &sample_input("cred-2", "u1", secret)).unwrap();

        let conn = db.lock().unwrap();
        let one: Vec<u8> = conn
            .query_row(
                "SELECT encrypted_secret FROM credentials WHERE id = 'cred-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let two: Vec<u8> = conn
            .query_row(
                "SELECT encrypted_secret FROM credentials WHERE id = 'cred-2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(one, two, "fresh nonce must differ per row");
    }

    #[test]
    fn archive_hides_credential_from_get() {
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");
        let key = crate::crypto::MasterKey::generate();
        set_credential(&db, &key, &sample_input("cred-1", "u1", b"x")).unwrap();
        // Visible before archive.
        get_credential(&db, &key, "cred-1").expect("visible pre-archive");

        let flipped = archive_credential(&db, "cred-1").unwrap();
        assert!(flipped, "first archive must report a state change");

        // Idempotent second archive — no state change.
        let flipped_again = archive_credential(&db, "cred-1").unwrap();
        assert!(!flipped_again);

        let err = get_credential(&db, &key, "cred-1").unwrap_err();
        assert!(matches!(err, CredentialError::NotFound));
    }

    #[test]
    fn set_credential_rejects_duplicate_id() {
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");
        let key = crate::crypto::MasterKey::generate();
        set_credential(&db, &key, &sample_input("cred-1", "u1", b"a")).unwrap();
        let err = set_credential(&db, &key, &sample_input("cred-1", "u1", b"b")).unwrap_err();
        // Duplicate PK is a `Db(...)` error — callers translate to 409.
        assert!(matches!(err, CredentialError::Db(_)));
    }

    #[test]
    fn fk_cascade_deletes_credential_when_user_is_deleted() {
        // Deleting a user must take their credentials with them. This is
        // the security floor: a removed user's PATs cannot continue to
        // sit decryptable in the DB.
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");
        let key = crate::crypto::MasterKey::generate();
        set_credential(&db, &key, &sample_input("cred-1", "u1", b"secret")).unwrap();

        {
            let conn = db.lock().unwrap();
            conn.execute("DELETE FROM users WHERE id = ?1", params!["u1"])
                .unwrap();
        }

        let err = get_credential(&db, &key, "cred-1").unwrap_err();
        assert!(matches!(err, CredentialError::NotFound));
    }

    #[test]
    fn corrupted_kind_column_surfaces_as_crypto_error() {
        // If a future schema reshuffle (or a manual SQL edit) stores an
        // unknown `kind` value, get_credential must NOT panic — it maps
        // the parse failure to a crypto error so the credentials list
        // endpoint stays available for the other rows.
        let (db, _dir) = test_db();
        seed_user(&db, "u1", "a@b.com");
        let key = crate::crypto::MasterKey::generate();
        set_credential(&db, &key, &sample_input("cred-1", "u1", b"x")).unwrap();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE credentials SET kind = 'futuristic_oauth' WHERE id = 'cred-1'",
                [],
            )
            .unwrap();
        }
        let err = get_credential(&db, &key, "cred-1").unwrap_err();
        assert!(matches!(
            err,
            CredentialError::Crypto(crate::crypto::CryptoError::EncryptionFailed(_))
        ));
    }

    // ── Org-shared learnings (Phase 2 of learning-hub-ci-failure-capture) ──

    fn sample_learning(id: &str, slug: &str) -> LearningInput<'static> {
        LearningInput {
            id: Box::leak(id.to_string().into_boxed_str()),
            org_id: "default-org",
            kind: LearningKind::Feedback,
            category: "testing",
            slug: Box::leak(slug.to_string().into_boxed_str()),
            body_md: "**Why:** because.\n\n**How to apply:** carefully.",
            source_agent_id: None,
            source_ci_run_id: None,
        }
    }

    #[test]
    fn learning_kind_round_trips_through_wire_form() {
        for k in [
            LearningKind::Feedback,
            LearningKind::Project,
            LearningKind::Reference,
        ] {
            assert_eq!(LearningKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(LearningKind::parse("user"), None);
        assert_eq!(LearningKind::parse(""), None);
        assert_eq!(LearningKind::parse("Feedback"), None); // case-sensitive
    }

    #[test]
    fn create_and_get_learning_round_trips() {
        let (db, _dir) = test_db();
        create_learning(&db, &sample_learning("L-1", "no-mocks-in-integration")).unwrap();

        let got = get_learning(&db, "L-1").unwrap();
        assert_eq!(got.id, "L-1");
        assert_eq!(got.org_id, "default-org");
        assert_eq!(got.kind, "feedback");
        assert_eq!(got.category, "testing");
        assert_eq!(got.slug, "no-mocks-in-integration");
        assert!(got.body_md.starts_with("**Why:**"));
        assert_eq!(got.source_agent_id, None);
        assert_eq!(got.source_ci_run_id, None);
        assert_eq!(got.archived_at, None);
        assert_eq!(got.archived_reason, None);
        assert!(!got.created_at.is_empty());
    }

    #[test]
    fn get_missing_learning_returns_not_found() {
        let (db, _dir) = test_db();
        let err = get_learning(&db, "nope").unwrap_err();
        assert!(matches!(err, LearningError::NotFound));
    }

    #[test]
    fn duplicate_slug_within_same_org_and_kind_collides() {
        let (db, _dir) = test_db();
        create_learning(&db, &sample_learning("L-1", "stable-handle")).unwrap();
        // Same (org_id, kind, slug) ⇒ unique-index trip.
        let err = create_learning(&db, &sample_learning("L-2", "stable-handle")).unwrap_err();
        assert!(
            matches!(err, LearningError::SlugCollision),
            "duplicate slug must surface as SlugCollision, got {err:?}"
        );

        // Same slug but different kind ⇒ allowed (unique is on the triple).
        let mut as_project = sample_learning("L-3", "stable-handle");
        as_project.kind = LearningKind::Project;
        create_learning(&db, &as_project).expect("different kind ⇒ different triple ⇒ allowed");
    }

    #[test]
    fn list_filters_by_org_and_kind_and_archive_state() {
        let (db, _dir) = test_db();
        // Seed two orgs' worth of rows. The default org row is created by
        // `ensure_default_org`; add a second org so the FK is happy.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO organizations (id, name, slug) VALUES (?1, ?2, ?3)",
                params!["org-2", "Second Org", "second-org"],
            )
            .unwrap();
        }

        create_learning(&db, &sample_learning("L-1", "feedback-a")).unwrap();

        let mut project = sample_learning("L-2", "project-a");
        project.kind = LearningKind::Project;
        create_learning(&db, &project).unwrap();

        let mut other_org = sample_learning("L-3", "feedback-a");
        other_org.org_id = "org-2";
        create_learning(&db, &other_org).unwrap();

        // Default org, no kind filter, active only.
        let active_default = list_learnings_for_org(&db, "default-org", None, false);
        let ids: Vec<&str> = active_default.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "two active rows in default-org, got {ids:?}");
        assert!(ids.contains(&"L-1"));
        assert!(ids.contains(&"L-2"));

        // Org filter excludes the other org's row.
        let org_2_rows = list_learnings_for_org(&db, "org-2", None, false);
        assert_eq!(org_2_rows.len(), 1);
        assert_eq!(org_2_rows[0].id, "L-3");

        // Kind filter restricts to feedback within default-org.
        let feedback_only =
            list_learnings_for_org(&db, "default-org", Some(LearningKind::Feedback), false);
        assert_eq!(feedback_only.len(), 1);
        assert_eq!(feedback_only[0].id, "L-1");

        // Archive L-1; active list drops to one, archived-inclusive list
        // restores both.
        assert!(archive_learning(&db, "L-1", "outdated-by-adr-0009").unwrap());
        let active_after = list_learnings_for_org(&db, "default-org", None, false);
        assert_eq!(active_after.len(), 1);
        assert_eq!(active_after[0].id, "L-2");
        let all_after = list_learnings_for_org(&db, "default-org", None, true);
        assert_eq!(all_after.len(), 2);
    }

    #[test]
    fn archive_learning_is_idempotent_and_records_reason() {
        let (db, _dir) = test_db();
        create_learning(&db, &sample_learning("L-1", "to-archive")).unwrap();

        let first = archive_learning(&db, "L-1", "no-longer-applicable").unwrap();
        assert!(first, "first archive flips the row");

        let got = get_learning(&db, "L-1").unwrap();
        assert!(got.archived_at.is_some());
        assert_eq!(got.archived_reason.as_deref(), Some("no-longer-applicable"));

        let original_archived_at = got.archived_at.clone();

        // Second archive: NOT a flip (already archived). Original
        // timestamp + reason MUST survive — operators rely on the first
        // archive timestamp to bound the audit window.
        let second = archive_learning(&db, "L-1", "different-reason-2nd-try").unwrap();
        assert!(!second, "re-archiving an archived row is a no-op flip");

        let got_after = get_learning(&db, "L-1").unwrap();
        assert_eq!(got_after.archived_at, original_archived_at);
        assert_eq!(
            got_after.archived_reason.as_deref(),
            Some("no-longer-applicable"),
            "the original reason must survive the re-archive attempt"
        );
    }

    #[test]
    fn learning_links_back_to_ci_failure_event_row() {
        // Acceptance criterion: "an entry with source_ci_run_id linked
        // back to the originating event row." Seed a ci_failure_events
        // row, capture its autoincrement id, then write a learning
        // pointing at it. Confirm the round-trip preserves the link AND
        // a JOIN-style read recovers the original event row from the
        // learning's source_ci_run_id alone.
        let (db, _dir) = test_db();

        let event_id: i64 = {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO ci_failure_events \
                     (agent_id, plan_name, task_number, branch, run_id, \
                      workflow, conclusion, summary) \
                 VALUES ('agent-1', 'plan-a', '1.1', 'branchwork/x/1.1', \
                         '12345', 'tests.yml', 'failure', 'rustfmt tripped')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        assert!(event_id > 0);

        let mut linked = sample_learning("L-1", "always-run-fmt-locally");
        linked.source_agent_id = Some("agent-1");
        linked.source_ci_run_id = Some(event_id);
        create_learning(&db, &linked).unwrap();

        // Direct field check.
        let got = get_learning(&db, "L-1").unwrap();
        assert_eq!(got.source_ci_run_id, Some(event_id));
        assert_eq!(got.source_agent_id.as_deref(), Some("agent-1"));

        // JOIN-style navigation: from the learning's source_ci_run_id
        // alone, recover the originating event row's run_id + workflow.
        // This is the readback the dashboard's "where did this come
        // from?" affordance will use.
        let (origin_run_id, origin_workflow, origin_branch): (String, String, String) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT e.run_id, e.workflow, e.branch \
                   FROM ci_failure_events e \
                   JOIN learnings l ON l.source_ci_run_id = e.id \
                  WHERE l.id = ?1",
                params!["L-1"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(origin_run_id, "12345");
        assert_eq!(origin_workflow, "tests.yml");
        assert_eq!(origin_branch, "branchwork/x/1.1");
    }

    #[test]
    fn fk_cascade_deletes_learnings_when_org_is_deleted() {
        // Deleting an org must take its learnings with it. Mirrors the
        // FK-cascade contract `credentials` rides for owner_user_id.
        let (db, _dir) = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO organizations (id, name, slug) VALUES (?1, ?2, ?3)",
                params!["org-9", "Doomed Org", "doomed"],
            )
            .unwrap();
        }
        let mut input = sample_learning("L-1", "in-doomed-org");
        input.org_id = "org-9";
        create_learning(&db, &input).unwrap();

        {
            let conn = db.lock().unwrap();
            conn.execute("DELETE FROM organizations WHERE id = ?1", params!["org-9"])
                .unwrap();
        }
        let err = get_learning(&db, "L-1").unwrap_err();
        assert!(matches!(err, LearningError::NotFound));
    }

    #[test]
    fn body_md_is_stored_byte_for_byte() {
        // The brief is explicit that bodies are markdown to match the
        // on-disk memory format. Verify the round-trip preserves bytes
        // — including newlines, backticks, and the [[link]] wikilinks
        // the auto-memory format uses to cross-reference other entries.
        let (db, _dir) = test_db();
        let body = "# Title\n\nSome body with `code` and [[other-memory]].\n";
        let mut input = sample_learning("L-1", "verbatim-body");
        input.body_md = body;
        create_learning(&db, &input).unwrap();

        let got = get_learning(&db, "L-1").unwrap();
        assert_eq!(got.body_md, body);
    }

    // ── Phase 4, Task 4.1: learning hit-counter + promotion ────────────────

    /// Seed an active learning so a `learning_hits` row can satisfy the
    /// FK to `learnings(id)` (`PRAGMA foreign_keys = ON`).
    fn seed_learning_for_hits(db: &Db, id: &str) {
        create_learning(db, &sample_learning(id, &format!("slug-{id}"))).unwrap();
    }

    /// Count of `learning_hits` rows for a learning, ignoring the window.
    fn total_hits(db: &Db, learning_id: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM learning_hits WHERE learning_id = ?1",
            params![learning_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn promotion_row(db: &Db, learning_id: &str) -> Option<(i64, i64, i64)> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT hit_count, threshold, window_days FROM promotion_candidates \
              WHERE learning_id = ?1",
            params![learning_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    #[test]
    fn record_learning_hit_dedups_within_the_same_session() {
        let (db, _dir) = test_db();
        seed_learning_for_hits(&db, "L-1");

        let first = record_learning_hit(&db, "L-1", "agent-A", "default-org", 5, 30);
        assert!(first.inserted, "first render in a session must insert");
        assert_eq!(first.recent_hit_count, 1);
        assert!(!first.promotion_emitted);

        // Same learning, same session → no second row, no recount.
        let second = record_learning_hit(&db, "L-1", "agent-A", "default-org", 5, 30);
        assert!(
            !second.inserted,
            "re-render in same session must not insert"
        );
        assert_eq!(second.recent_hit_count, 0);
        assert!(!second.promotion_emitted);

        assert_eq!(total_hits(&db, "L-1"), 1, "session must count exactly once");
    }

    #[test]
    fn record_learning_hit_counts_distinct_sessions() {
        let (db, _dir) = test_db();
        seed_learning_for_hits(&db, "L-1");

        for (i, expected) in (1..=3).enumerate() {
            let out = record_learning_hit(&db, "L-1", &format!("agent-{i}"), "default-org", 5, 30);
            assert!(out.inserted);
            assert_eq!(out.recent_hit_count, expected, "session {i}");
        }
        assert_eq!(total_hits(&db, "L-1"), 3);
    }

    /// Headline acceptance: after 5 distinct sessions render the learning,
    /// a `promotion_candidates` row exists — emitted on the 5th, exactly
    /// once, and not re-emitted on later renders.
    #[test]
    fn promotion_candidate_emitted_at_threshold_and_only_once() {
        let (db, _dir) = test_db();
        seed_learning_for_hits(&db, "L-1");

        // Sessions 1–4: below threshold, no promotion.
        for i in 0..4 {
            let out = record_learning_hit(&db, "L-1", &format!("agent-{i}"), "default-org", 5, 30);
            assert!(out.inserted);
            assert!(
                !out.promotion_emitted,
                "no promotion before the threshold (session {i})"
            );
        }
        assert!(promotion_row(&db, "L-1").is_none());

        // Session 5: crosses the threshold → emit exactly one row.
        let fifth = record_learning_hit(&db, "L-1", "agent-4", "default-org", 5, 30);
        assert!(fifth.inserted);
        assert_eq!(fifth.recent_hit_count, 5);
        assert!(fifth.promotion_emitted, "5th distinct session must emit");
        assert_eq!(
            promotion_row(&db, "L-1"),
            Some((5, 5, 30)),
            "promotion row records the count, threshold, and window"
        );

        // Session 6: still over threshold, but the candidate already
        // exists → no second emission, row unchanged.
        let sixth = record_learning_hit(&db, "L-1", "agent-5", "default-org", 5, 30);
        assert!(sixth.inserted);
        assert_eq!(sixth.recent_hit_count, 6);
        assert!(
            !sixth.promotion_emitted,
            "promotion is emitted once, not on every render past the threshold"
        );
        assert_eq!(
            promotion_row(&db, "L-1"),
            Some((5, 5, 30)),
            "the original promotion row must not be overwritten"
        );
    }

    /// Hits older than the window must not count toward the threshold.
    #[test]
    fn record_learning_hit_window_excludes_old_hits() {
        let (db, _dir) = test_db();
        seed_learning_for_hits(&db, "L-1");

        // Five sessions rendered the learning 40 days ago — outside a
        // 30-day window.
        {
            let conn = db.lock().unwrap();
            for i in 0..5 {
                conn.execute(
                    "INSERT INTO learning_hits (learning_id, agent_id, org_id, rendered_at) \
                     VALUES ('L-1', ?1, 'default-org', datetime('now', '-40 days'))",
                    params![format!("old-agent-{i}")],
                )
                .unwrap();
            }
        }

        // A single fresh session: only this one is inside the window, so
        // the recent count is 1 — nowhere near the threshold of 5.
        let out = record_learning_hit(&db, "L-1", "fresh-agent", "default-org", 5, 30);
        assert!(out.inserted);
        assert_eq!(
            out.recent_hit_count, 1,
            "old hits outside the window must not count"
        );
        assert!(!out.promotion_emitted);
        assert!(promotion_row(&db, "L-1").is_none());
        // The old rows still physically exist; they just don't count.
        assert_eq!(total_hits(&db, "L-1"), 6);
    }

    /// Defensive: recording a hit for a learning that does not exist is a
    /// no-op (the FK to `learnings(id)` rejects the insert; the error is
    /// swallowed rather than panicking). Production never hits this — we
    /// only record hits for learnings we just selected.
    #[test]
    fn record_learning_hit_for_unknown_learning_is_a_noop() {
        let (db, _dir) = test_db();
        let out = record_learning_hit(&db, "does-not-exist", "agent-A", "default-org", 5, 30);
        assert!(!out.inserted);
        assert_eq!(out.recent_hit_count, 0);
        assert!(!out.promotion_emitted);
        assert_eq!(total_hits(&db, "does-not-exist"), 0);
    }

    /// Hard-deleting the learning cascades its hits and promotion row away
    /// (FK `ON DELETE CASCADE`). Learnings are soft-deleted in practice,
    /// but the cascade keeps the tables consistent if a hard delete ever
    /// lands (e.g. an org wipe).
    #[test]
    fn deleting_a_learning_cascades_hits_and_promotion() {
        let (db, _dir) = test_db();
        seed_learning_for_hits(&db, "L-1");
        for i in 0..5 {
            record_learning_hit(&db, "L-1", &format!("agent-{i}"), "default-org", 5, 30);
        }
        assert_eq!(total_hits(&db, "L-1"), 5);
        assert!(promotion_row(&db, "L-1").is_some());

        {
            let conn = db.lock().unwrap();
            conn.execute("DELETE FROM learnings WHERE id = 'L-1'", [])
                .unwrap();
        }
        assert_eq!(total_hits(&db, "L-1"), 0, "hits must cascade away");
        assert!(
            promotion_row(&db, "L-1").is_none(),
            "promotion row must cascade away"
        );
    }
}
