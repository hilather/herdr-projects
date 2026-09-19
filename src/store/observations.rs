use super::*;
use crate::reconcile::RuntimeObservation;

pub(super) fn read_all(db:&Connection)->Result<Vec<RuntimeObservation>> {
    let mut stmt=db.prepare("SELECT binding_id,binding_revision,task_revision,observed_unix_ms,payload,payload_hash FROM runtime_observations ORDER BY binding_id")?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<u64>>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?;
    rows.map(|row| {
        let(id,revision,task_revision,time,payload,hash)=row?;
        if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash {return Err(StoreError::Corrupt("observation payload hash mismatch".into()));}
        let observation:RuntimeObservation=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid observation payload".into()))?;
        observation.validate().map_err(StoreError::Corrupt)?;
        if observation.binding!=id||observation.binding_revision!=revision||observation.task_revision!=task_revision||observation.observed_unix_ms!=time {return Err(StoreError::Corrupt("observation identity mismatch".into()));}
        Ok(observation)
    }).collect()
}
impl SqliteStore {
    /// Commit a complete collector batch against the exact state it observed.
    /// No external commands run here and no capacity/lifecycle state is changed.
    pub fn record_observations(&mut self,expected_head:u64,observations:&[RuntimeObservation])->Result<u64> {
        if observations.len()>128 {return Err(StoreError::Invalid("observation batch exceeds 128 records".into()));}
        let mut seen=std::collections::BTreeSet::new();
        for observation in observations {observation.validate().map_err(StoreError::Invalid)?;if !seen.insert(&observation.binding){return Err(StoreError::Conflict);}}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let bindings=runtime::read_all(&tx)?;
        if bindings.len()!=observations.len(){return Err(StoreError::Conflict);}
        let tasks=read_tasks(&tx)?.into_iter().map(|t|(t.id,t.revision)).collect::<std::collections::BTreeMap<_,_>>();
        for observation in observations {
            let binding=bindings.iter().find(|b|b.id==observation.binding).ok_or(StoreError::Conflict)?;
            if observation.binding_revision!=binding.revision || observation.task_revision!=binding.task.as_ref().and_then(|id|tasks.get(id).copied()) {return Err(StoreError::Conflict);}
            let old:Option<i64>=tx.query_row("SELECT observed_unix_ms FROM runtime_observations WHERE binding_id=?1",[&binding.id],|r|r.get(0)).optional()?;
            if old.is_some_and(|t|t>observation.observed_unix_ms){return Err(StoreError::Conflict);}
            let payload=serde_json::to_string(observation).map_err(|e|StoreError::Invalid(e.to_string()))?;
            let unchanged:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_observations WHERE binding_id=?1 AND payload=?2)",params![binding.id,payload],|r|r.get(0))?;
            if unchanged {continue;}
            tx.execute("INSERT INTO runtime_observations VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(binding_id) DO UPDATE SET binding_revision=excluded.binding_revision,task_revision=excluded.task_revision,observed_unix_ms=excluded.observed_unix_ms,payload=excluded.payload,payload_hash=excluded.payload_hash",params![binding.id,integer(binding.revision)?,observation.task_revision.map(integer).transpose()?,observation.observed_unix_ms,payload,format!("{:x}",Sha256::digest(payload.as_bytes()))])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.observed',?1,?2,1,?3)",params![binding.id,integer(binding.revision)?,payload])?;
        }
        let result=head(&tx)?;tx.commit()?;Ok(result)
    }
}
