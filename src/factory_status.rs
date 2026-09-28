//! Read-only factory status. Counters are bounded and carry no environment, argv, or secrets.
use crate::store::{integrity::{self, IntegrityRecord}, FactoryNumbers, SqliteStore, StoreError, SCHEMA};
use crate::watchdog;
use rusqlite::OpenFlags;
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct QueueAge {
    pub control: Option<i64>,
    /// In-memory transfer queues are not durable. Missing age stays null rather than zero.
    pub transfer: Option<i64>,
    pub verification: Option<i64>,
    pub integration: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Counters {
    pub queue_age_ms: QueueAge,
    pub lock_wait_ms: Option<u64>,
    /// Total decision-path decoding is not yet fully instrumented.
    pub rows_decoded: Option<u64>,
    /// Rows decoded by the status request's first active-inventory page only.
    pub active_inventory_page_rows: u64,
    pub observation_age_ms: Option<i64>,
    pub ambiguous_effects: u64,
    pub retained_slots: u64,
    pub verification_backlog_age_ms: Option<i64>,
    pub integration_backlog_age_ms: Option<i64>,
    pub promotion_conflicts: u64,
    pub coordinator_checkpoint_chars: u64,
}

#[derive(Debug, Serialize)]
pub struct FactoryStatus {
    pub schema: u32,
    pub feature: &'static str,
    pub prepared_dispatch: bool,
    pub factory_admission: String,
    pub sqlite_version: &'static str,
    pub platform: &'static str,
    pub admission_paused: bool,
    pub pause_reason: Option<&'static str>,
    pub blockers: Vec<&'static str>,
    /// The last recorded whole-store check; null until one has run.
    pub integrity: Option<IntegrityRecord>,
    pub counters: Counters,
}

#[derive(Debug)]
pub enum ReportError {
    Store(StoreError),
    Io(String),
    /// `user_version` is 0. The value is the only JSON that may be printed.
    Unsupported(serde_json::Value),
    /// Newer than this binary. The value is the only JSON that may be printed.
    Newer(serde_json::Value),
    /// The whole-store check failed. The value is the only JSON that may be printed.
    Corrupt(serde_json::Value),
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Unsupported(_) => write!(f, "unsupported_schema"),
            Self::Newer(_) => write!(f, "store schema is newer than this binary"),
            Self::Corrupt(_) => write!(f, "store_corrupt"),
        }
    }
}
impl std::error::Error for ReportError {}

fn platform() -> &'static str {
    if cfg!(target_os = "linux") { "linux" } else { "unsupported" }
}

fn refused_schema(version: u32, prepared_dispatch: bool, error: &'static str) -> serde_json::Value {
    serde_json::json!({
        "schema": version,
        "feature": "state-store",
        "prepared_dispatch": prepared_dispatch,
        "sqlite_version": rusqlite::version(),
        "platform": platform(),
        "error": error,
    })
}

fn user_version(path: &Path) -> Result<u32, ReportError> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| ReportError::Io(error.to_string()))?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| ReportError::Store(StoreError::from(error)))
}

fn blockers(numbers: &FactoryNumbers, paused: bool, promotion_pages: bool) -> Vec<&'static str> {
    let mut blockers = Vec::new();
    if paused {
        blockers.push("admission_paused");
    }
    if numbers.integration_reconciliation_stale {
        blockers.push("integration_reconciliation_stale");
    }
    if numbers.retained_at_cap {
        blockers.push("retained_at_cap");
    }
    if promotion_pages {
        blockers.push("promotion_conflict");
    }
    blockers
}

/// Operator rate file, digits only. Absent means report the count and do not page.
fn promotion_pages(project: &Path, conflicts: u64) -> bool {
    let path = project.join(".state/promotion-conflict-rate");
    let Ok(meta) = std::fs::symlink_metadata(&path) else { return false };
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > 16 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    text.parse::<u64>().is_ok_and(|rate| conflicts > rate)
}

fn counters_from(numbers: FactoryNumbers) -> Counters {
    Counters {
        queue_age_ms: QueueAge {
            control: numbers.queue_age_control_ms,
            transfer: None,
            verification: numbers.queue_age_verification_ms,
            integration: numbers.queue_age_integration_ms,
        },
        lock_wait_ms: None,
        rows_decoded: None,
        active_inventory_page_rows: numbers.active_inventory_page_rows,
        observation_age_ms: numbers.observation_age_ms,
        ambiguous_effects: numbers.ambiguous_effects,
        retained_slots: numbers.retained_slots,
        verification_backlog_age_ms: numbers.verification_backlog_age_ms,
        integration_backlog_age_ms: numbers.integration_backlog_age_ms,
        promotion_conflicts: numbers.promotion_conflicts,
        coordinator_checkpoint_chars: numbers.coordinator_checkpoint_chars,
    }
}

/// Read-only status. Does not launch, admit, or copy environment values.
pub fn report(project: &Path, prepared_dispatch: bool) -> Result<FactoryStatus, ReportError> {
    let db_path = project.join(".state/state.db");
    let version = user_version(&db_path)?;
    if version == 0 {
        return Err(ReportError::Unsupported(refused_schema(version, prepared_dispatch, "unsupported_schema")));
    }
    if version > SCHEMA {
        return Err(ReportError::Newer(refused_schema(
            version,
            prepared_dispatch,
            "store schema is newer than this binary",
        )));
    }
    let pause_reason = watchdog::pause_reason(project);
    let mut db = match SqliteStore::open(&db_path) {
        Ok(db) => db,
        Err(StoreError::Corrupt(_)) => {
            let mut value = refused_schema(version, prepared_dispatch, "store_corrupt");
            value["admission_paused"] = pause_reason.is_some().into();
            value["pause_reason"] = serde_json::json!(pause_reason);
            value["integrity"] = serde_json::json!(integrity::load(&db_path));
            return Err(ReportError::Corrupt(value));
        }
        Err(error) => return Err(ReportError::Store(error)),
    };
    let now = jiff::Timestamp::now().as_millisecond();
    let numbers = db.factory_counters(now).map_err(ReportError::Store)?;
    let paused = pause_reason.is_some();
    let promotion_pages = promotion_pages(project, numbers.promotion_conflicts);
    let blockers = blockers(&numbers, paused, promotion_pages);
    let schema = numbers.schema;
    let factory_admission = numbers.factory_admission.clone();
    Ok(FactoryStatus {
        schema,
        feature: "state-store",
        prepared_dispatch,
        factory_admission,
        sqlite_version: rusqlite::version(),
        platform: platform(),
        admission_paused: paused,
        pause_reason,
        blockers,
        integrity: integrity::load(&db_path),
        counters: counters_from(numbers),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    #[test]
    fn status_keeps_missing_decision_measurements_unknown_across_historical_schemas() {
        for version in [25,26,27,42,crate::store::SCHEMA] {
            let temp=tempfile::tempdir().unwrap();
            let project=temp.path().join("project");std::fs::create_dir_all(project.join(".state")).unwrap();
            let path=project.join(".state/state.db");
            drop(SqliteStore::create(&path).unwrap());
            let raw=rusqlite::Connection::open(&path).unwrap();
            crate::store::test_schema::historical(&raw,version).unwrap();drop(raw);
            let status=report(&project,true).unwrap();
            assert_eq!(status.schema,version);assert!(status.counters.rows_decoded.is_none());
            assert!(status.counters.verification_backlog_age_ms.is_none());
        }
    }

}
