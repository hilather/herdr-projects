//! Local-file SQLite repository. No external effects or caller callbacks inside transactions.
use crate::domain::*;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{fmt, fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};

pub const SCHEMA: u32 = 68;
const APPLICATION: u32 = 1_213_222_994;
pub const MIN_SQLITE: i32 = 3_053_004;
pub const MIN_SQLITE_VERSION: &str = "3.53.4";
/// SQLite's busy handler gives up after this. The watchdog treats the resulting
/// `Busy` as past the retry bound and pauses admission.
pub const BUSY_RETRY_BOUND: Duration = Duration::from_millis(250);
const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum StoreError {
    Cancelled,
    Deadline,
    Limit(String),
    Conflict,
    /// Expected plan parent is not the current revision. The value is that revision.
    StalePlanParent(u64),
    Busy,
    DiskFull,
    UnsupportedSchema(u32),
    HistoryUnavailable(u64),
    Invalid(String),
    Corrupt(String),
    Io(String),
}
pub mod controlled;
pub mod attempt_tokens;
pub mod integrity;
pub(crate) mod read_budget;
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "state store: {self:?}") }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => Self::Busy,
            Some(rusqlite::ErrorCode::DiskFull) => Self::DiskFull,
            Some(rusqlite::ErrorCode::TooBig) => Self::Limit("SQLite encoded value or row exceeds limit".into()),
            Some(rusqlite::ErrorCode::ConstraintViolation) => Self::Conflict,
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => Self::Corrupt(error.to_string()),
            _ => Self::Io(error.to_string()),
        }
    }
}
type Result<T> = std::result::Result<T, StoreError>;

pub struct SqliteStore { connection: Connection }
// Replay recording composes existing services under one outer transaction.
enum MutationTransaction<'a> {
    Transaction(rusqlite::Transaction<'a>),
    Savepoint(rusqlite::Savepoint<'a>),
}
impl std::ops::Deref for MutationTransaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection { match self { Self::Transaction(tx) => tx, Self::Savepoint(tx) => tx } }
}
impl MutationTransaction<'_> {
    fn commit(self) -> rusqlite::Result<()> { match self { Self::Transaction(tx) => tx.commit(), Self::Savepoint(tx) => tx.commit() } }
}
fn mutation_transaction(connection: &mut Connection) -> rusqlite::Result<MutationTransaction<'_>> {
    if connection.is_autocommit() {
        Ok(MutationTransaction::Transaction(connection.transaction_with_behavior(TransactionBehavior::Immediate)?))
    } else {
        Ok(MutationTransaction::Savepoint(connection.savepoint()?))
    }
}
impl SqliteStore {
    /// Internal composition of store operations only; no external effects.
    pub(crate) fn atomic_replay<T>(&mut self, record: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = record(self);
        match result {
            Ok(value) => {
                if let Err(error) = self.connection.execute_batch("COMMIT") {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
                Ok(value)
            }
            Err(error) => { self.connection.execute_batch("ROLLBACK")?; Err(error) }
        }
    }
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
            tx.execute_batch(include_str!("../../migrations/0016_durable_routines.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0017_command_authority.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0018_memory_revisions.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0019_memory_snapshots.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0020_coordinator_checkpoints.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0021_memory_proposals.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0022_memory_reviews.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0023_memory_inputs_and_candidates.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0024_memory_receipts.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0025_native_profiles.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0026_factory_results.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0027_verification_runs.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0028_integration.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0029_feedback.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0030_dependency_satisfaction.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0031_plan_revisions.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0032_contract_scope.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0033_capability_evidence.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0034_delegation_grants.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0035_resource_claims.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0036_waits.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0037_consumer_bindings.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0038_memory_read_sets.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0039_update_packages.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0040_barriers.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0041_active_work.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0042_wait_subscription.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0043_admission_indexes.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0044_result_automation.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0045_result_integration.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0046_integration_policy_checks.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0047_result_job_revisions.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0048_effect_row_indexes.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0049_task_classifications.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0050_dispatch_decisions.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0051_attempt_lifecycle.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0052_collector_bindings.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0053_candidate_groups.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0054_review_capture.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0055_finding_triage.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0056_fix_attribution.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0057_review_protocols.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0058_seeded_defects.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0059_review_ledger.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0060_attempt_supersessions.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0061_reviewer_authority.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0062_review_launch.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0063_review_opportunity_ledger.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0064_replay_suite.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0065_assignment_policies.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0066_assignment_revocations.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0067_telemetry_read_indexes.sql"))?;
            tx.execute_batch(include_str!("../../migrations/0068_verification_metadata.sql"))?;
            tx.commit()?;
        }
        // Persist the initial directory entry as well as SQLite's own commit.
        std::fs::File::open(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
            .and_then(|parent| parent.sync_all()).map_err(|e| StoreError::Io(e.to_string()))?;
        enable_wal(&connection)?;
        let store = Self { connection };
        // A new store starts with a recorded check for its schema version.
        integrity::check_and_record(path, &store)?;
        Ok(store)
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
    /// Hot paths: like `open`, but the whole-store check runs only when none is
    /// recorded for this schema version (see `integrity`).
    pub(crate) fn open_scoped(path: &Path) -> Result<Self> {
        engine_check()?;
        let connection = connect(path)?;
        check_schema(&connection)?;
        enable_wal(&connection)?;
        let store = Self { connection };
        integrity::check_if_schema_changed(path, &store)?;
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
        self.read_snapshot_with_budget(at, None)
    }
    fn read_snapshot_with_budget(&mut self, at: Option<u64>, budget: Option<&read_budget::ReadBudget>) -> Result<Snapshot> {
        let tx = if self.connection.is_autocommit() {
            MutationTransaction::Transaction(self.connection.transaction()?)
        } else {
            MutationTransaction::Savepoint(self.connection.savepoint()?)
        };
        check_schema(&tx)?;
        let head = head(&tx)?;
        if let Some(at) = at { if at != head { return Err(StoreError::HistoryUnavailable(at)); } }
        let tasks = read_tasks_with_budget(&tx, budget)?;
        let attempts = read_attempts_with_budget(&tx, budget)?;
        let operations = read_operations_matching_with_budget(&tx, None, budget)?;
        let events = read_events_with_budget(&tx, budget)?;
        let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        let deliveries=if schema>=3 {delivery::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let inbox=if schema>=4 {inbox::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let runtime_bindings=if schema>=5 {runtime::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let observations=if schema>=6 {observations::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let ownership=if schema>=9 {ownership::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let control=if schema>=7 {Some(control::read_with_budget(&tx,budget)?)}else{None};
        let scheduler=if schema>=10 {Some(scheduler::read_with_tasks(&tx,&tasks,budget)?)}else{None};
        let attempt_inputs=if schema>=11 {reservations::read_inputs_with_budget(&tx,budget)?}else{Vec::new()};
        let cancellations=if schema>=11 {reservations::read_cancellations_with_budget(&tx,budget)?}else{Vec::new()};
        let approvals=if schema>=13 {approvals::read_all_with(&tx,&attempt_inputs,budget)?}else{Vec::new()};
        let budget_policies=if schema>=14 {budget::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let memory_policies=if schema>=17 {memory_policy::read_all_with_budget(&tx,budget)?}else{Vec::new()};
        let (routine_revisions,routine_occurrences)=if schema>=16 {routines::read_all_with_budget(&tx,budget)?}else{(Vec::new(),Vec::new())};
        let routine_receipts=if schema>=16 {routines::read_receipts_with_budget(&tx,&routine_revisions,&routine_occurrences,budget)?}else{Vec::new()};
        tx.commit()?;
        Ok(Snapshot { schema_version:schema, head, tasks, attempts, operations, deliveries, inbox, runtime_bindings, observations, ownership, control, scheduler, attempt_inputs, cancellations, approvals, budget_policies, memory_policies, routine_revisions, routine_occurrences, routine_receipts, events })
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
                    if matches!(next.kind.as_str(),"runtime.launch"|"runtime.worker_brief") || next.task.is_none() || next.kind=="routine.run" {return Err(StoreError::Invalid("worker lifecycle and project routine intents require their sealed service".into()));}
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
        let tx = mutation_transaction(&mut self.connection)?;
        check_schema(&tx)?;
        if head(&tx)? != commit.expected_head { return Err(StoreError::Conflict); }
        let now = jiff::Timestamp::now().as_millisecond();
        // Check before and after the whole batch: clearing/replacing an attempt
        // in the same commit must not erase its outstanding memory obligations.
        for mutation in &commit.mutations {
            if let Mutation::Task { expected: Some(_), next } = mutation {
                if next.state == TaskState::Succeeded { memory_barrier::enforce(&tx,next.id.as_str(),now)?; }
            }
        }
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
                    let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
                    if schema>=11 && tx.query_row("SELECT EXISTS(SELECT 1 FROM attempt_inputs WHERE attempt_id=?1)",[id],|r|r.get::<_,bool>(0))? {
                        return Err(StoreError::Invalid("sealed launch attempts require the lifecycle service".into()));
                    }
                    let count = match expected {
                        None => tx.execute("INSERT INTO attempts(id,task_id,revision,state,snapshot,reservation,termination_observed) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id, next.task.as_str(), integer(next.revision)?, next.state.as_str(), next.snapshot, next.reservation, next.termination_observed])?,
                        Some(rev) => tx.execute("UPDATE attempts SET revision=?3,state=?4,snapshot=?5,reservation=?6,termination_observed=?7 WHERE id=?1 AND task_id=?2 AND revision=?8", params![id, next.task.as_str(), integer(next.revision)?, next.state.as_str(), next.snapshot, next.reservation, next.termination_observed, integer(*rev)?])?,
                    };
                    if count != 1 { return Err(StoreError::Conflict); }
                    // A newly inserted terminated attempt does not change the retained-row
                    // fingerprint, but it does remove a previously attempt-less binding.
                    active_work::invalidate(&tx)?;
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
        for mutation in &commit.mutations {
            if let Mutation::Task { next, .. } = mutation {
                if next.state == TaskState::Succeeded { memory_barrier::enforce(&tx,next.id.as_str(),now)?; }
            }
        }
        consumer_bindings::reconcile_active(&tx)?;
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
    if rusqlite::version_number() < MIN_SQLITE { return Err(StoreError::Invalid(format!("SQLite {MIN_SQLITE_VERSION}+ required; found {}", rusqlite::version()))); }
    Ok(())
}
fn connect(path: &Path) -> Result<Connection> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| StoreError::Io(e.to_string()))?;
    if !metadata.is_file() { return Err(StoreError::Invalid("database must be a regular local file, not a symlink".into())); }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    db.busy_timeout(BUSY_RETRY_BOUND)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL;")?;
    Ok(db)
}
pub(crate) fn check_schema(db: &Connection) -> Result<()> {
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
fn read_tasks(db: &Connection) -> Result<Vec<Task>> { read_tasks_with_budget(db, None) }
fn read_tasks_with_budget(db: &Connection, budget: Option<&read_budget::ReadBudget>) -> Result<Vec<Task>> {
    let mut stmt = db.prepare("SELECT id,revision,state,title,active_attempt FROM tasks ORDER BY id")?;
    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(budget) = budget { budget.row(r, &[])?; }
        let value = serde_json::json!({"id":r.get::<_,String>(0)?,"revision":r.get::<_,u64>(1)?,"state":r.get::<_,String>(2)?,"title":r.get::<_,String>(3)?,"active_attempt":r.get::<_,Option<String>>(4)?});
        result.push(decode(value)?);
    }
    Ok(result)
}
/// An indexed prerequisite lookup; unrelated task history is not decoded.
fn read_task(db: &Connection, id: &str) -> Result<Task> { read_task_with_budget(db,id,None) }
fn read_task_with_budget(db: &Connection, id: &str, budget: Option<&read_budget::ReadBudget>) -> Result<Task> {
    let mut stmt = db.prepare("SELECT id,revision,state,title,active_attempt FROM tasks WHERE id=?1")?;
    let mut rows = stmt.query([id])?;
    let r = rows.next()?.ok_or(StoreError::Conflict)?;
    if let Some(budget)=budget {budget.row(r,&[])?;}
    decode(serde_json::json!({"id":r.get::<_,String>(0)?,"revision":r.get::<_,u64>(1)?,"state":r.get::<_,String>(2)?,"title":r.get::<_,String>(3)?,"active_attempt":r.get::<_,Option<String>>(4)?}))
}
fn read_attempt(db:&Connection,id:&AttemptId)->Result<Attempt> {
    read_attempt_with_budget(db,id,None)
}
fn read_attempt_with_budget(db:&Connection,id:&AttemptId,budget:Option<&read_budget::ReadBudget>)->Result<Attempt> {
    let mut stmt=db.prepare("SELECT id,task_id,revision,state,snapshot,reservation,termination_observed FROM attempts WHERE id=?1")?;
    let mut rows=stmt.query([id.as_str()])?;
    let r=rows.next()?.ok_or(StoreError::Conflict)?;
    if let Some(budget)=budget {budget.row(r,&[])?;}
    decode(serde_json::json!({"id":r.get::<_,String>(0)?,"task":r.get::<_,String>(1)?,"revision":r.get::<_,u64>(2)?,"state":r.get::<_,String>(3)?,"snapshot":r.get::<_,Option<String>>(4)?,"reservation":r.get::<_,String>(5)?,"termination_observed":r.get::<_,bool>(6)?}))
}
fn read_retained_attempts_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<Attempt>> {
    let attempts=read_attempt_rows(db, "SELECT id,task_id,revision,state,snapshot,reservation,termination_observed FROM attempts WHERE termination_observed=0 ORDER BY id LIMIT 1025", params![], budget)?;
    if attempts.len()>1024 {return Err(StoreError::Limit("retained admission attempt limit exceeded".into()));}
    Ok(attempts)
}
fn read_attempts(db: &Connection) -> Result<Vec<Attempt>> { read_attempts_with_budget(db, None) }
fn read_attempts_with_budget(db: &Connection, budget: Option<&read_budget::ReadBudget>) -> Result<Vec<Attempt>> {
    read_attempt_rows(db, "SELECT id,task_id,revision,state,snapshot,reservation,termination_observed FROM attempts ORDER BY id", params![], budget)
}
fn read_attempt_rows(db: &Connection, sql: &str, params: impl rusqlite::Params, budget: Option<&read_budget::ReadBudget>) -> Result<Vec<Attempt>> {
    let mut stmt = db.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(budget) = budget { budget.row(r, &[])?; }
        let value = serde_json::json!({"id":r.get::<_,String>(0)?,"task":r.get::<_,Option<String>>(1)?,"revision":r.get::<_,u64>(2)?,"state":r.get::<_,String>(3)?,"snapshot":r.get::<_,Option<String>>(4)?,"reservation":r.get::<_,String>(5)?,"termination_observed":r.get::<_,bool>(6)?});
        result.push(decode(value)?);
    }
    Ok(result)
}
fn read_operations(db: &Connection) -> Result<Vec<Operation>> {read_operations_matching(db,None)}
fn read_operation(db:&Connection,id:&OperationId)->Result<Operation> {read_operation_with_budget(db,id,None)}
fn read_operation_with_budget(db:&Connection,id:&OperationId,budget:Option<&read_budget::ReadBudget>)->Result<Operation> {
    read_operations_matching_with_budget(db,Some(id),budget)?.into_iter().next().ok_or(StoreError::Conflict)
}
fn read_operations_matching(db:&Connection,id:Option<&OperationId>)->Result<Vec<Operation>> {read_operations_matching_with_budget(db,id,None)}
fn read_operations_matching_with_budget(db:&Connection,id:Option<&OperationId>,budget:Option<&read_budget::ReadBudget>)->Result<Vec<Operation>> {
    let query=if id.is_some(){"SELECT id,task_id,kind,target,payload_version,payload,expected_revision,due_unix_ms,idempotency_key,payload_hash FROM operations WHERE id=?1"}else{"SELECT id,task_id,kind,target,payload_version,payload,expected_revision,due_unix_ms,idempotency_key,payload_hash FROM operations WHERE ?1 IS NULL ORDER BY id"};
    let mut stmt = db.prepare(query)?;
    let mut rows = stmt.query([id.map(OperationId::as_str)])?;
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(budget) = budget { budget.row(r, &[(5, 2)])?; }
        let (mut value, payload, hash) = (serde_json::json!({"id":r.get::<_,String>(0)?,"task":r.get::<_,Option<String>>(1)?,"kind":r.get::<_,String>(2)?,"target":r.get::<_,String>(3)?,"payload_version":r.get::<_,u32>(4)?,"expected_revision":r.get::<_,u64>(6)?,"due_unix_ms":r.get::<_,i64>(7)?,"idempotency_key":r.get::<_,String>(8)?}), r.get::<_,String>(5)?, r.get::<_,String>(9)?);
        if format!("{:x}", Sha256::digest(payload.as_bytes())) != hash { return Err(StoreError::Corrupt("operation payload hash mismatch".into())); }
        value["payload"] = serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        result.push(decode(value)?);
    }
    Ok(result)
}
fn read_events_with_budget(db: &Connection, budget: Option<&read_budget::ReadBudget>) -> Result<Vec<Event>> {
    let mut stmt = db.prepare("SELECT sequence,kind,entity,revision,payload_version,payload FROM events ORDER BY sequence")?;
    let mut rows = stmt.query([])?;
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        if let Some(budget) = budget { budget.row(r, &[(5, 1)])?; }
        let (sequence,kind,entity,revision,payload_version,payload) = (r.get::<_,u64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?,r.get::<_,u32>(4)?,r.get::<_,String>(5)?);
        result.push(Event { sequence,kind,entity,revision,payload_version,payload:serde_json::from_str(&payload).map_err(|e| StoreError::Corrupt(e.to_string()))? });
    }
    Ok(result)
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

pub(crate) mod reservations;
mod launch;
mod collector_binding;
pub use collector_binding::CollectorBinding;
mod worktrees;
mod worker_brief;
mod worker_termination;
mod approvals;
mod budget;
mod memory_policy;
mod objects;
mod memory;
mod checkpoints;
mod proposals;
mod reviews;
mod read_set;
mod routines;

pub mod identity_inventory;
pub mod controller_hint;

mod memory_candidates;

mod memory_delivery;
mod consumer_bindings;
mod memory_receipts;
mod memory_supersession;
pub use memory_supersession::{WorkerUpdateSupersession, WorkerUpdateSupersessionReceipt};
mod update_packages;
mod memory_barrier;
mod barriers;
mod active_work;
pub use active_work::{ActiveCoverage, ActiveWorkItem, ActiveWorkPage, ActiveWorkRun, ACTIVE_WORK_PAGE};
pub use barriers::{BarrierMember, FrozenBarrier, ProposalDisposition, BarrierStopTurn, service_project_barrier_stops};
#[cfg(test)]
pub(crate) use barriers::tests::install_worker_barrier_fixture;
pub use consumer_bindings::ConsumerBinding;
pub use update_packages::{PackageAckReceipt, WorkerPackageAckReceipt, UpdatePackage, UpdatePackageAck,UpdatePackageManifest,UpdatePackageMember};
mod memory_invalidation;
mod memory_reconciliation;
mod worker_knowledge;
mod targeted;
pub(crate) mod effect_rows;
pub use effect_rows::{BindingOutputRows, FinalizationRows, NotificationRows};
pub use targeted::{hot_path_uses_snapshot, targeted_mismatch_count, HotPathRead, HOT_PATH_READ};
mod observability;
pub use observability::FactoryNumbers;

#[cfg(target_os = "linux")]
mod native_profiles;

mod results;
pub use results::{show_results, submit_untrusted_result, SUBMISSION_OBJECT_LIMIT};
pub(crate) use results::{record_spool_denial, submit_untrusted_result_bytes};

mod feedback;
pub use feedback::{claim_feedback, show_feedback};

mod satisfaction;
mod contract_binding;
pub(crate) mod admission_read;
mod admission_policy;

mod plans;
pub use plans::{PlanIntent,PlanIntentPage,inspect_project_plan,PlannerSession,PlannerInput,PlannerEventInput,create_project_planner_session,show_project_planner_session,AutoReplanControl,ReplanTurn,set_project_auto_replans,service_project_replans,PlanProposalReceipt, propose_plan,WaitTurn,register_project_wait,register_project_wait_with_deadline,register_project_wait_with_trigger,rearm_project_wait,replay_project_wait,request_project_replan,service_project_waits};

mod verification_jobs;
pub use verification_jobs::{ResultAutomationControl,VerificationJob,VerificationJobTurn,set_project_result_automation,service_project_verification_jobs,project_verification_jobs,reset_project_verification_job,reset_project_integration_job};
mod integration_jobs;
pub use integration_jobs::{IntegrationJobTurn,service_project_integration_jobs};

mod capabilities;

mod delegation;
mod delegated_reservation;
mod dispatch_log;
mod candidate_groups;
pub use candidate_groups::{CandidateArm,CandidateGroup,CandidateSelection,SelectionChoice,NO_SELECTION_REASONS,SELECTED_REASONS};
pub use candidate_groups::{ArmOutcome,arm_outcome};
mod review_capture;
pub use review_capture::{ReviewAssignment,ReviewAssignmentChoice,ReviewCompletion,ReviewOpportunity,ReviewOpportunitySpec,ReviewSession};
mod finding_triage;
pub use finding_triage::{FindingEvent,FindingState,FindingSummary,FindingTarget,TriageOutcome,TriageRequest,finding_state};
mod fix_attribution;
pub use fix_attribution::{CREDIT_UNIT,CreditShareSpec,DEFAULT_REPAIR_HORIZON_MS,FixState,IntroductionRequest,RepairAssignment,credit_text,fix_state};
mod review_protocols;
pub use review_protocols::{ProtocolState,protocol_state};
mod seeded_defects;
pub use seeded_defects::{EvaluationArm,EvaluationOpportunity,SeedSpec,SeedState,seed_state};
mod replay_cases;
pub use replay_cases::{REPLAY_PRINCIPAL,ReplayCaseRecord,ReplayCandidateStatus,ReplaySource,ReplaySuiteRecord,replay_brief_payloads,replay_candidate,replay_candidate_statuses,replay_contract_raw,replay_retirements,replay_runs,replay_sources,replay_suite};
mod review_ledger;
mod supersessions;
pub use supersessions::{SupersessionRecord,SupersessionRequest,SUPERSESSION_OUTCOMES,SUPERSESSION_REASONS};
mod review_authority;
pub use review_authority::{PreparedReviewAcceptance,PreparedReviewAuthority,PreparedReviewRevocation,ReviewAcceptance,ReviewAuthorityInstall,review_authority_state};
mod review_launch;
mod assignment_policy;
pub use assignment_policy::{AssignmentSettings,PreparedAssignmentAuthority,PreparedAssignmentRevocation,assignment_state};
pub use review_launch::{ReviewBrief,ReviewBriefBinding,ReviewVisibility,review_visibility};
pub use delegation::DelegationReserve;

#[cfg(target_os = "linux")]
pub(crate) mod verification;
#[cfg(target_os = "linux")]
pub(crate) mod integration;

#[cfg(test)]
pub(crate) mod test_schema;

pub use worker_termination::service_project_result_completions;
