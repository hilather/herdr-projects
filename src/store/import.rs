use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedSource {
    pub path: String,
    pub kind: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}
impl SqliteStore {
    /// Explicit schema upgrade; never part of opening an existing database.
    pub fn upgrade_v1(&mut self) -> Result<()> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version == 1 { tx.execute_batch(include_str!("../../migrations/0002_legacy_import.sql"))?; }
        if version <= 2 { tx.execute_batch(include_str!("../../migrations/0003_operation_delivery.sql"))?; }
        if version<=3 {
            tx.execute_batch(include_str!("../../migrations/0004_canonical_inbox.sql"))?;
            super::inbox::import_sources(&tx)?;
        }
        if version<=4 {
            tx.execute_batch(include_str!("../../migrations/0005_runtime_bindings.sql"))?;
            super::runtime::import_sources(&tx)?;
        }
        if version<=5 {tx.execute_batch(include_str!("../../migrations/0006_runtime_observations.sql"))?;}
        if version<=6 {tx.execute_batch(include_str!("../../migrations/0007_project_control.sql"))?;super::control::import_status(&tx)?;}
        if version<=7 {tx.execute_batch(include_str!("../../migrations/0008_canonical_runtime.sql"))?;}
        if version<=8 {tx.execute_batch(include_str!("../../migrations/0009_runtime_ownership.sql"))?;}
        if version<=9 {tx.execute_batch(include_str!("../../migrations/0010_scheduler_queue.sql"))?;}
        if version<=10 {
            let unsupported:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='runtime.launch')",[],|r|r.get(0))?;
            if unsupported {return Err(StoreError::Invalid("upgrade refused: preexisting runtime.launch intents lack sealed attempt inputs; reconcile unsupported intents before upgrading".into()));}
            tx.execute_batch(include_str!("../../migrations/0011_attempt_inputs.sql"))?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Initial import only, into an unpublished empty store. Raw bytes retain
    /// unknown fields and pending obligations; none are marked handled/delivered.
    pub fn import_legacy(&mut self, digest: &str, sources: &[ImportedSource], tasks: &[Task]) -> Result<()> {
        self.import_legacy_with_operations(digest,sources,tasks,&[])
    }
    pub fn import_legacy_with_operations(&mut self,digest:&str,sources:&[ImportedSource],tasks:&[Task],operations:&[Operation])->Result<()> {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(StoreError::Invalid("invalid import digest".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version != SCHEMA { return Err(StoreError::UnsupportedSchema(version)); }
        let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks UNION ALL SELECT 1 FROM events UNION ALL SELECT 1 FROM migration_receipt UNION ALL SELECT 1 FROM legacy_sources)",[],|r|r.get(0))?;
        if occupied { return Err(StoreError::Conflict); }
        for source in sources {
            if !safe_relative(&source.path) || !matches!(source.kind.as_str(), "task"|"thread"|"runtime"|"inbox") || hash_bytes(&source.bytes) != source.digest {
                return Err(StoreError::Invalid("invalid legacy source".into()));
            }
            tx.execute("INSERT INTO legacy_sources VALUES(?1,?2,?3,?4)",params![source.path,source.kind,source.digest,source.bytes])?;
            let event = serde_json::json!({"path":source.path,"digest":source.digest,"kind":source.kind});
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('legacy.imported',?1,1,1,?2)",params![source.path,event.to_string()])?;
        }
        super::inbox::import_sources(&tx)?;
        for task in tasks {
            if task.revision != 1 || task.active_attempt.is_some() { return Err(StoreError::Invalid("legacy task must start at revision one without an active attempt".into())); }
            tx.execute("INSERT INTO tasks VALUES(?1,1,?2,?3,NULL)", params![task.id.as_str(),task.state.as_str(),task.title])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.imported',?1,1,1,?2)",params![task.id.as_str(),serde_json::to_string(task).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
        }
        super::runtime::import_sources(&tx)?;
        super::control::import_status(&tx)?;
        for op in operations {
            if op.expected_revision!=1 || op.payload_version!=1 || !matches!(op.kind.as_str(),"legacy.inbox"|"legacy.notify"|"legacy.finalize") { return Err(StoreError::Invalid("invalid imported operation".into())); }
            let payload=serde_json::to_string(&op.payload).map_err(|e|StoreError::Invalid(e.to_string()))?;
            tx.execute("INSERT INTO operations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![op.id.as_str(),op.task.as_str(),op.kind,op.target,op.payload_version,payload,hash_bytes(payload.as_bytes()),integer(op.expected_revision)?,op.due_unix_ms,op.idempotency_key])?;
            let attempts=op.payload.get("retry").and_then(|r|r.get("attempts")).map(|v|v.as_u64().filter(|n|*n<=u32::MAX as u64).ok_or_else(||StoreError::Invalid("invalid imported retry count".into()))).transpose()?.unwrap_or(0);
            let outcome=serde_json::json!({"kind":"ambiguous","observation_required":"legacy import; observe prior delivery before retry"});
            tx.execute("UPDATE operation_delivery SET state='ambiguous',attempts=?2,last_outcome=?3 WHERE operation_id=?1",params![op.id.as_str(),integer(attempts)?,outcome.to_string()])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('operation.imported',?1,1,1,?2)",params![op.id.as_str(),serde_json::to_string(op).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
        }
        tx.execute("INSERT INTO migration_receipt VALUES(1,?1,?2,?3,1,?4)",params![digest,integer(sources.len() as u64)?,integer(tasks.len() as u64)?,integer(operations.len() as u64)?])?;
        tx.commit()?;
        Ok(())
    }
    pub fn imported_sources(&self) -> Result<Vec<ImportedSource>> {
        check_schema(&self.connection)?;
        read_sources(&self.connection)
    }

    pub fn has_import(&self) -> Result<bool> {
        check_schema(&self.connection)?;
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM migration_receipt)",[],|r|r.get(0))?)
    }
    pub fn import_operation_count(&self)->Result<u64> {
        check_schema(&self.connection)?;
        let version:u32=self.connection.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version<3 {return Ok(0);}
        Ok(self.connection.query_row("SELECT operation_count FROM migration_receipt WHERE singleton=1",[],|r|r.get(0))?)
    }
    pub fn import_receipt(&self) -> Result<(String, u64, u64)> {
        check_schema(&self.connection)?;
        Ok(self.connection.query_row("SELECT source_digest,source_count,task_count FROM migration_receipt WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
    }
}

fn hash_bytes(bytes:&[u8])->String { format!("{:x}",Sha256::digest(bytes)) }
fn safe_relative(path:&str)->bool {
    !path.is_empty() && !path.contains('\\') && std::path::Path::new(path).components().all(|p|matches!(p,std::path::Component::Normal(_)))
}

pub(super) fn read_sources(db:&Connection)->Result<Vec<ImportedSource>> {
        let mut stmt = db.prepare("SELECT path,kind,digest,bytes FROM legacy_sources ORDER BY path")?;
        let rows = stmt.query_map([],|r|Ok(ImportedSource { path:r.get(0)?,kind:r.get(1)?,digest:r.get(2)?,bytes:r.get(3)? }))?;
        rows.map(|row| { let row = row?; if hash_bytes(&row.bytes) != row.digest { return Err(StoreError::Corrupt(format!("imported source hash mismatch: {}",row.path))); } Ok(row) }).collect()
}
