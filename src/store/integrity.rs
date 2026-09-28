//! The last whole-store check, recorded beside the database.
//!
//! Administrative opens always run the check. Hot-path (scoped) opens run it
//! only when no check is recorded for the current schema version, so a
//! controller poll or a targeted command does not scan the whole database.
//! The ticker repeats it once per interval (`migration::periodic_integrity_check`).
use super::{Result, SqliteStore, StoreError};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const NAME: &str = "integrity-check.json";
const MAX_BYTES: u64 = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityRecord {
    pub checked_unix_ms: i64,
    pub schema: u32,
    /// `ok` or `corrupt`. Details are not retained: they can echo row bytes.
    pub result: String,
}

pub fn record_path(db: &Path) -> PathBuf {
    db.with_file_name(NAME)
}

/// A missing, oversized, linked or unreadable record reads as no record.
pub fn load(db: &Path) -> Option<IntegrityRecord> {
    let path = record_path(db);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > MAX_BYTES {
        return None;
    }
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save(db: &Path, record: &IntegrityRecord) -> Result<()> {
    let io = |error: std::io::Error| StoreError::Io(error.to_string());
    let path = record_path(db);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = db.with_file_name(format!(".{NAME}.{}.{nanos}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(record).map_err(|error| StoreError::Invalid(error.to_string()))?;
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io)
}

pub(crate) fn schema(store: &SqliteStore) -> Result<u32> {
    Ok(store.connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Runs the whole-store check and records a definite outcome. Cancellation,
/// deadline and I/O errors are returned without a record.
pub(crate) fn check_and_record(db: &Path, store: &SqliteStore) -> Result<()> {
    let schema = schema(store)?;
    let result = store.integrity_check();
    let outcome = match &result {
        Ok(()) => "ok",
        Err(StoreError::Corrupt(_)) => "corrupt",
        Err(_) => return result,
    };
    save(db, &IntegrityRecord { checked_unix_ms: jiff::Timestamp::now().as_millisecond(), schema, result: outcome.into() })?;
    result
}

/// Records a failure found before the check could run (for example while opening).
pub(crate) fn record_corrupt(db: &Path, schema: u32) -> Result<()> {
    save(db, &IntegrityRecord { checked_unix_ms: jiff::Timestamp::now().as_millisecond(), schema, result: "corrupt".into() })
}

/// Scoped opens check once after the schema version changes (or while no
/// check is recorded); otherwise they rely on the periodic check.
pub(crate) fn check_if_schema_changed(db: &Path, store: &SqliteStore) -> Result<()> {
    let schema = schema(store)?;
    if load(db).is_some_and(|record| record.schema == schema) {
        return Ok(());
    }
    check_and_record(db, store)
}
