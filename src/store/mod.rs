//! Local-file SQLite repository. No external effects or caller callbacks inside transactions.
use crate::domain::*;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{fmt, fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};

const SCHEMA: u32 = 15;
const APPLICATION: u32 = 1_213_222_994;
const MIN_SQLITE: i32 = 3_053_004;
const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum StoreError {
    Conflict,
    Busy,
    DiskFull,
    UnsupportedSchema(u32),
    HistoryUnavailable(u64),
    Invalid(String),
    Corrupt(String),
    Io(String),
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "state store: {self:?}") }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => Self::Busy,
            Some(rusqlite::ErrorCode::DiskFull) => Self::DiskFull,
            Some(rusqlite::ErrorCode::ConstraintViolation) => Self::Conflict,
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => Self::Corrupt(error.to_string()),
            _ => Self::Io(error.to_string()),
        }
    }
}
type Result<T> = std::result::Result<T, StoreError>;

pub struct SqliteStore { connection: Connection }
impl SqliteStore {
    /// Explicit initialization only. Parent must already exist on local storage.
    /// Never replaces an existing database or switches legacy project authority.
    pub fn create(path: &Path) -> Result<Self> {
        engine_check()?;
        let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        file.sync_all().map_err(|e| StoreError::Io(e.to_string()))?;
        let mut connection = connect(path)?;
        {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(include_str!("../../migrations/0001_project_store.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0002_legacy_import.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0003_operation_delivery.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0004_canonical_inbox.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0005_runtime_bindings.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0006_runtime_observations.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0007_project_control.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0008_canonical_runtime.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0009_runtime_ownership.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0010_scheduler_queue.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0011_attempt_inputs.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0012_effective_profiles.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0013_scoped_approvals.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0014_admission_budgets.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0015_project_operations.sql"))?;
            tx.commit()?;
        }
        // Persist the initial directory entry as well as SQLite's own commit.
        std::fs::File::open(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
            .and_then(|parent| parent.sync_all()).map_err(|e| StoreError::Io(e.to_string()))?;
        enable_wal(&connection)?;
        Ok(Self { connection })
    }
    /// Existing, recognized schemas only. Unknown versions are inspected before
    /// setting WAL or doing any application writes; migrations are never implicit.
    pub fn open(path: &Path) -> Result<Self> {
        engine_check()?;
        let connection = connect(path)?;
        check_schema(&connection)?;
        enable_wal(&connection)?;
        let store = Self { connection };
        store.integrity_check()?;
        Ok(store)
    }
    pub fn integrity_check(&self) -> Result<()> {
        let check: String = self.connection.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" { return Err(StoreError::Corrupt(check)); }
        let mut stmt = self.connection.prepare("PRAGMA foreign_key_check")?;
        if stmt.query([])?.next()?.is_some() { return Err(StoreError::Corrupt("foreign key violation".into())); }
        Ok(())
    }
    /// Current consistent snapshot. Earlier heads fail explicitly: history
    /// reconstruction is not implemented by schema v1.
    pub fn read_snapshot(&mut self, at: Option<u64>) -> Result<Snapshot> {
        let tx = self.connection.transaction()?;
        check_schema(&tx)?;
        let head = head(&tx)?;
        if let Some(at) = at { if at != head { return Err(StoreError::HistoryUnavailable(at)); } }
        let tasks = read_tasks(&tx)?;
        let attempts = read_attempts(&tx)?;
        let operations = read_operations(&tx)?;
        let events = read_events(&tx)?;
        let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        let deliveries=if schema>=3 {delivery::read_all(&tx)?}else{Vec::new()};
        let inbox=if schema>=4 {inbox::read_all(&tx)?}else{Vec::new()};
        let runtime_bindings=if schema>=5 {runtime::read_all(&tx)?}else{Vec::new()};
        let observations=if schema>=6 {observations::read_all(&tx)?}else{Vec::new()};
        let ownership=if schema>=9 {ownership::read_all(&tx)?}else{Vec::new()};
        let control=if schema>=7 {Some(control::read(&tx)?)}else{None};
        let scheduler=if schema>=10 {Some(scheduler::read(&tx)?)}else{None};
        let attempt_inputs=if schema>=11 {reservations::read_inputs(&tx)?}else{Vec::new()};
        let cancellations=if schema>=11 {reservations::read_cancellations(&tx)?}else{Vec::new()};
        let approvals=if schema>=13 {approvals::read_all(&tx)?}else{Vec::new()};
        let budget_policies=if schema>=14 {budget::read_all(&tx)?}else{Vec::new()};
        tx.commit()?;
        Ok(Snapshot { schema_version:schema, head, tasks, attempts, operations, deliveries, inbox, runtime_bindings, observations, ownership, control, scheduler, attempt_inputs, cancellations, approvals, budget_policies, events })
    }
    /// All mutations, generated audit events and durable intents commit together.
    /// Revisions start at one and advance by exactly one. A stale head or record
    /// rolls back the entire batch. Callers cannot perform I/O inside this API.
    pub fn commit(&mut self, commit: Commit) -> Result<u64> {
        if commit.mutations.len() > 1000 { return Err(StoreError::Invalid("batch exceeds 1000 mutations".into())); }
        // Serialize and validate untrusted payloads before obtaining the write lock.
        let mut encoded = Vec::with_capacity(commit.mutations.len());
        let mut seen = std::collections::BTreeSet::new();
        let mut batch_bytes = 0usize;
        for mutation in &commit.mutations {
            let identity = match mutation {
                Mutation::Task { next, .. } => ("task", next.id.as_str()),
                Mutation::Attempt { next, .. } => ("attempt", next.id.as_str()),
                Mutation::Enqueue(next) => ("operation", next.id.as_str()),
            };
            if !seen.insert(identity) { return Err(StoreError::Conflict); }
            let value = match mutation {
                Mutation::Task { expected, next } => { revision(*expected, next.revision)?; serde_json::to_value(next) },
                Mutation::Attempt { expected, next } => { revision(*expected, next.revision)?; serde_json::to_value(next) },
                Mutation::Enqueue(next) => {
                    if next.kind=="runtime.launch" || next.task.is_none() || next.kind=="routine.run" {return Err(StoreError::Invalid("launch and project routine intents require their sealed service".into()));}
                    integer(next.expected_revision)?;
                    if next.expected_revision == 0 || next.payload_version == 0 { return Err(StoreError::Invalid("zero operation revision/version".into())); }
                    serde_json::to_value(next)
                },
            }.map_err(|e| StoreError::Invalid(e.to_string()))?;
            let value = serde_json::to_string(&value).map_err(|e| StoreError::Invalid(e.to_string()))?;
            if value.len() > MAX_RECORD_BYTES { return Err(StoreError::Invalid("record exceeds 1 MiB".into())); }
            batch_bytes += value.len();
            if batch_bytes > 16 * MAX_RECORD_BYTES { return Err(StoreError::Invalid("batch exceeds 16 MiB".into())); }
            encoded.push(value);
        }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        if head(&tx)? != commit.expected_head { return Err(StoreError::Conflict); }
        for (mutation, encoded) in commit.mutations.iter().zip(encoded.iter()) {
            let (kind, entity, rev) = match mutation {
                Mutation::Task { expected, next } => {
                    let id = next.id.as_str();
                    let active = next.active_attempt.as_ref().map(AttemptId::as_str);
                    let count = match expected {
                        None => tx.execute("INSERT INTO tasks(id,revision,state,title,active_attempt) VALUES(?1,?2,?3,?4,?5)", params![id, integer(next.revision)?, next.state.as_str(), next.title, active])?,
                        Some(rev) => tx.execute("UPDATE tasks SET revision=?2,state=?3,title=?4,active_attempt=?5 WHERE id=?1 AND revision=?6", params![id, integer(next.revision)?, next.state.as_str(), next.title, active, integer(*rev)?])?,
                    };
                    if count != 1 { return Err(StoreError::Conflict); }
                    ("task.changed", id, next.revision)
                },
                Mutation::Attempt { expected, next } => {
                    let id = next.id.as_str();
                    let count = match expected {
                        None => tx.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id, next.task.as_str(), integer(next.revision)?, next.state.as_str(), next.snapshot, next.reservation, next.termination_observed])?,
                        Some(rev) => tx.execute("UPDATE attempts SET revision=?3,state=?4,snapshot=?5,reservation=?6,termination_observed=?7 WHERE id=?1 AND task_id=?2 AND revision=?8", params![id, next.task.as_str(), integer(next.revision)?, next.state.as_str(), next.snapshot, next.reservation, next.termination_observed, integer(*rev)?])?,
                    };
                    if count != 1 { return Err(StoreError::Conflict); }
                    ("attempt.changed", id, next.revision)
                },
                Mutation::Enqueue(next) => {
                    let task=next.task.as_ref().ok_or_else(||StoreError::Invalid("project operation requires sealed service".into()))?;
                    let current: Option<i64> = tx.query_row("SELECT revision FROM tasks WHERE id=?1", [task.as_str()], |r| r.get(0)).optional()?;
                    if current != Some(integer(next.expected_revision)?) { return Err(StoreError::Conflict); }
                    let payload = serde_json::to_string(&next.payload).map_err(|e| StoreError::Invalid(e.to_string()))?;
                    let hash = format!("{:x}", Sha256::digest(payload.as_bytes()));
                    tx.execute("INSERT INTO operations(id,task_id,kind,target,payload_version,payload,payload_hash,expected_revision,due_unix_ms,idempotency_key) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![next.id.as_str(), task.as_str(), next.kind, next.target, next.payload_version, payload, hash, integer(next.expected_revision)?, next.due_unix_ms, next.idempotency_key])?;
                    ("operation.enqueued", next.id.as_str(), next.expected_revision)
                },
            };
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)", params![kind, entity, integer(rev)?, encoded])?;
        }
        // A later mutation in the same batch must not stale an earlier intent.
        for mutation in &commit.mutations {
            if let Mutation::Enqueue(next) = mutation {
                let task=next.task.as_ref().ok_or(StoreError::Conflict)?;
                let current: i64 = tx.query_row("SELECT revision FROM tasks WHERE id=?1", [task.as_str()], |r| r.get(0))?;
                if current != integer(next.expected_revision)? { return Err(StoreError::Conflict); }
            }
        }
        let head = head(&tx)?;
        tx.commit()?;
        Ok(head)
    }
    /// Bounded passive checkpoint; never waits indefinitely for active readers.
    /// Returns (busy, WAL pages, checkpointed pages).
    pub fn checkpoint(&self) -> Result<(u32, u32, u32)> {
        Ok(self.connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    }
}
use rusqlite::OptionalExtension;
fn engine_check() -> Result<()> {
    if rusqlite::version_number() < MIN_SQLITE { return Err(StoreError::Invalid(format!("SQLite 3.53.4+ required; found {}", rusqlite::version()))); }
    Ok(())
}
fn connect(path: &Path) -> Result<Connection> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| StoreError::Io(e.to_string()))?;
    if !metadata.is_file() { return Err(StoreError::Invalid("database must be a regular local file, not a symlink".into())); }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    db.busy_timeout(Duration::from_millis(250))?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL;")?;
    Ok(db)
}
fn check_schema(db: &Connection) -> Result<()> {
    let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 || version > SCHEMA { return Err(StoreError::UnsupportedSchema(version)); }
    let application: u32 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if application != APPLICATION { return Err(StoreError::Corrupt("unrecognized application identity".into())); }
    let stored_version: u32 = db.query_row("SELECT schema_version FROM store_meta WHERE singleton=1", [], |r| r.get(0))?;
    if stored_version != version { return Err(StoreError::Corrupt("schema metadata mismatch".into())); }
    Ok(())
}
fn enable_wal(db: &Connection) -> Result<()> {
    let mode: String = db.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if mode != "wal" { return Err(StoreError::Invalid("WAL unavailable; local storage required".into())); }
    db.execute_batch("PRAGMA wal_autocheckpoint=1000;")?;
    Ok(())
}
fn integer(value: u64) -> Result<i64> { i64::try_from(value).map_err(|_| StoreError::Invalid("integer exceeds SQLite range".into())) }
fn revision(expected: Option<u64>, next: u64) -> Result<()> {
    integer(next)?;
    if expected.unwrap_or(0).checked_add(1) != Some(next) { return Err(StoreError::Invalid("revision must advance exactly once".into())); }
    Ok(())
}
fn head(db: &Connection) -> Result<u64> { Ok(db.query_row("SELECT coalesce(max(sequence),0) FROM events", [], |r| r.get(0))?) }
fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| StoreError::Corrupt(e.to_string()))
}
fn read_tasks(db: &Connection) -> Result<Vec<Task>> {
    let mut stmt = db.prepare("SELECT id,revision,state,title,active_attempt FROM tasks ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"revision":r.get::<_,u64>(1)?,"state":r.get::<_,String>(2)?,"title":r.get::<_,String>(3)?,"active_attempt":r.get::<_,Option<String>>(4)?})))?;
    rows.map(|r| decode(r?)).collect()
}
fn read_attempts(db: &Connection) -> Result<Vec<Attempt>> {
    let mut stmt = db.prepare("SELECT id,task_id,revision,state,snapshot,reservation,termination_observed FROM attempts ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"task":r.get::<_,Option<String>>(1)?,"revision":r.get::<_,u64>(2)?,"state":r.get::<_,String>(3)?,"snapshot":r.get::<_,Option<String>>(4)?,"reservation":r.get::<_,String>(5)?,"termination_observed":r.get::<_,bool>(6)?})))?;
    rows.map(|r| decode(r?)).collect()
}
fn read_operations(db: &Connection) -> Result<Vec<Operation>> {read_operations_matching(db,None)}
fn read_operation(db:&Connection,id:&OperationId)->Result<Operation> {read_operations_matching(db,Some(id))?.into_iter().next().ok_or(StoreError::Conflict)}
fn read_operations_matching(db:&Connection,id:Option<&OperationId>)->Result<Vec<Operation>> {
    let query=if id.is_some(){"SELECT id,task_id,kind,target,payload_version,payload,expected_revision,due_unix_ms,idempotency_key,payload_hash FROM operations WHERE id=?1"}else{"SELECT id,task_id,kind,target,payload_version,payload,expected_revision,due_unix_ms,idempotency_key,payload_hash FROM operations WHERE ?1 IS NULL ORDER BY id"};
    let mut stmt = db.prepare(query)?;
    let rows = stmt.query_map([id.map(OperationId::as_str)], |r| Ok((serde_json::json!({"id":r.get::<_,String>(0)?,"task":r.get::<_,Option<String>>(1)?,"kind":r.get::<_,String>(2)?,"target":r.get::<_,String>(3)?,"payload_version":r.get::<_,u32>(4)?,"expected_revision":r.get::<_,u64>(6)?,"due_unix_ms":r.get::<_,i64>(7)?,"idempotency_key":r.get::<_,String>(8)?}), r.get::<_,String>(5)?, r.get::<_,String>(9)?)))?;
    rows.map(|row| {
        let (mut value, payload, hash) = row?;
        if format!("{:x}", Sha256::digest(payload.as_bytes())) != hash { return Err(StoreError::Corrupt("operation payload hash mismatch".into())); }
        value["payload"] = serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        decode(value)
    }).collect()
}
fn read_events(db: &Connection) -> Result<Vec<Event>> {
    let mut stmt = db.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events ORDER BY sequence")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?,r.get::<_,u32>(4)?,r.get::<_,String>(5)?)))?;
    rows.map(|row| { let (sequence,kind,entity,revision,payload_version,payload) = row?;
        Ok(Event { sequence,kind,entity,revision,payload_version,payload:serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))? })
    }).collect()
}

#[cfg(test)]
mod tests;

mod import;
pub use import::ImportedSource;

mod delivery;

mod inbox;
mod receipts;

mod runtime;

mod observations;

mod control;

mod finalization;

pub(crate) mod ownership;
pub use ownership::OwnershipChange;

mod scheduler;

mod reservations;
mod approvals;
mod budget;
