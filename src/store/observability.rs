//! Bounded counters for factory status. Aggregates only: no payloads, argv, or environment.
use super::{Result, SqliteStore, StoreError};
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryNumbers {
    pub schema: u32,
    pub factory_admission: String,
    pub rows_decoded: u64,
    pub queue_age_control_ms: Option<i64>,
    pub queue_age_verification_ms: Option<i64>,
    pub queue_age_integration_ms: Option<i64>,
    pub observation_age_ms: Option<i64>,
    pub ambiguous_effects: u64,
    pub retained_slots: u64,
    pub verification_backlog_age_ms: Option<i64>,
    pub integration_backlog_age_ms: Option<i64>,
    pub promotion_conflicts: u64,
    pub coordinator_checkpoint_chars: u64,
    pub retained_at_cap: bool,
    pub integration_reconciliation_stale: bool,
}

const RECONCILE_ALERT_MS: i64 = 15 * 60 * 1000;
const RETAINED_CAP_ALERT_MS: i64 = 30 * 60 * 1000;

fn age_ms(now: i64, stamp: Option<i64>) -> Option<i64> {
    stamp.map(|stamp| now.saturating_sub(stamp))
}

fn optional_i64(db: &Connection, sql: &str) -> Result<Option<i64>> {
    db.query_row(sql, [], |row| row.get(0)).map_err(StoreError::from)
}

fn count(db: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = db.query_row(sql, [], |row| row.get(0))?;
    u64::try_from(value.max(0)).map_err(|_| StoreError::Invalid("counter is negative".into()))
}

impl SqliteStore {
    /// Counters for one project. `rows_decoded` is the active-work page, not a snapshot of retired rows.
    pub fn factory_counters(&mut self, now: i64) -> Result<FactoryNumbers> {
        let schema: u32 = self.connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let rows_decoded = match self.hot_path_rows_decoded(now) {
            Ok(rows) => rows,
            Err(StoreError::UnsupportedSchema(_)) => 0,
            Err(error) => return Err(error),
        };
        let db = &self.connection;
        let factory_admission = if schema >= 30 {
            db.query_row(
                "SELECT factory_admission FROM project_control WHERE singleton=1",
                [],
                |row| row.get(0),
            )?
        } else {
            "absent".to_string()
        };
        let queue_age_control_ms = if schema >= 10 {
            age_ms(now, optional_i64(db, "SELECT MIN(enqueued_unix_ms) FROM task_queue")?)
        } else {
            None
        };
        let verification_oldest = if schema >= 26 {
            optional_i64(
                db,
                "SELECT MIN(created_unix_ms) FROM result_submissions WHERE NOT EXISTS (SELECT 1 FROM verification_runs WHERE verification_runs.submission_id = result_submissions.submission_id)",
            )?
        } else {
            None
        };
        let integration_oldest = if schema >= 28 {
            optional_i64(
                db,
                "SELECT MIN(created_unix_ms) FROM integration_operations WHERE state IN ('effect_pending', 'candidate_prepared', 'validating')",
            )?
        } else {
            None
        };
        let reconcile_oldest = if schema >= 28 {
            optional_i64(
                db,
                "SELECT MIN(created_unix_ms) FROM integration_operations WHERE state = 'reconciliation_required'",
            )?
        } else {
            None
        };
        let observation_newest = if schema >= 6 {
            optional_i64(db, "SELECT MAX(observed_unix_ms) FROM runtime_observations")?
        } else {
            None
        };
        let ambiguous_effects = if schema >= 3 {
            count(db, "SELECT COUNT(*) FROM operation_delivery WHERE state = 'ambiguous'")?
        } else {
            0
        };
        let retained_slots = count(db, "SELECT COUNT(*) FROM attempts WHERE termination_observed = 0")?;
        let promotion_conflicts = if schema >= 17 {
            count(
                db,
                "SELECT COUNT(*) FROM authority_denials WHERE class = 'memory' AND command IN ('promote', 'review') AND reason_code = 'stale_head'",
            )?
        } else {
            0
        };
        let coordinator_checkpoint_chars = if schema >= 20 {
            let sizes: Option<(i64, i64)> = db
                .query_row(
                    "SELECT full_chars, delta_chars FROM coordinator_checkpoints ORDER BY created_unix_ms DESC, id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            sizes
                .map(|(full, delta)| u64::try_from(full.max(0)).unwrap_or(0).saturating_add(u64::try_from(delta.max(0)).unwrap_or(0)))
                .unwrap_or(0)
        } else {
            0
        };
        let cap: i64 = if schema >= 10 {
            db.query_row("SELECT max_active_workers FROM scheduler_policy WHERE singleton = 1", [], |row| row.get(0))?
        } else {
            0
        };
        let observation_age_ms = age_ms(now, observation_newest);
        let retained_at_cap = cap > 0
            && i64::try_from(retained_slots).unwrap_or(i64::MAX) >= cap
            && observation_age_ms.is_some_and(|age| age > RETAINED_CAP_ALERT_MS);
        let integration_age = age_ms(now, reconcile_oldest);
        let integration_reconciliation_stale = integration_age.is_some_and(|age| age > RECONCILE_ALERT_MS);
        Ok(FactoryNumbers {
            schema,
            factory_admission,
            rows_decoded,
            queue_age_control_ms,
            queue_age_verification_ms: age_ms(now, verification_oldest),
            queue_age_integration_ms: age_ms(now, integration_oldest),
            observation_age_ms,
            ambiguous_effects,
            retained_slots,
            verification_backlog_age_ms: age_ms(now, verification_oldest),
            integration_backlog_age_ms: age_ms(now, integration_oldest),
            promotion_conflicts,
            coordinator_checkpoint_chars,
            retained_at_cap,
            integration_reconciliation_stale,
        })
    }
}
