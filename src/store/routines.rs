//! Routine revision, cursor, occurrence and outbox writes share one transaction.
use super::*;
use std::collections::BTreeMap;
use crate::operations::DeliveryState;
#[cfg(test)]
mod tests;
fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn corrupt(s:&str)->StoreError {StoreError::Corrupt(s.into())}
fn schema(db:&Connection)->Result<()> {check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;if n<16 {return Err(StoreError::UnsupportedSchema(n));}Ok(())}
fn project_matches(db:&Connection,d:&RoutineDefinition)->Result<()> {
    let path=std::fs::canonicalize(db.path().ok_or_else(||invalid("routine requires file-backed store"))?).map_err(|_|invalid("routine store unavailable"))?;
    if path.to_str()!=Some(d.project_store.as_str()) {return Err(invalid("routine belongs to a different project"));}Ok(())
}
fn encoded(value:&impl serde::Serialize)->Result<(String,String)> {
    let payload=serde_json::to_string(value).map_err(|_|invalid("routine encoding failed"))?;
    let hash=format!("{:x}",Sha256::digest(payload.as_bytes()));Ok((payload,hash))
}
fn decoded<T:serde::de::DeserializeOwned>(payload:&str,hash:&str)->Result<T> {
    if payload.len()>65_536||format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash {return Err(corrupt("routine payload identity mismatch"));}
    serde_json::from_str(payload).map_err(|_|corrupt("invalid routine payload"))
}
fn log(db:&Connection,kind:&str,entity:&str,revision:u64,value:&impl serde::Serialize)->Result<()> {
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,entity,integer(revision)?,encoded(value)?.0])?;Ok(())
}
pub(super) fn read_all(db:&Connection)->Result<(Vec<RoutineDefinition>,Vec<RoutineOccurrence>)> {
    let mut definitions=Vec::new();let mut last=BTreeMap::new();
    let mut stmt=db.prepare("SELECT name,revision,payload,payload_hash FROM routine_revisions ORDER BY name,revision")?;
    for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
        let(name,revision,payload,hash)=row?;let d:RoutineDefinition=decoded(&payload,&hash)?;
        if definitions.len()>=10_000||d.name!=name||d.revision!=revision||revision!=last.get(&name).copied().unwrap_or(0)+1
            ||d.reference().map_err(StoreError::Corrupt)?.digest!=hash {return Err(corrupt("routine history mismatch or bound exceeded"));}
        last.insert(name,revision);definitions.push(d);
    }
    if last.len()>128 {return Err(corrupt("routine identity bound exceeded"));}
    let mut cursors=BTreeMap::new();
    let mut stmt=db.prepare("SELECT name,revision,after_unix_ms FROM routine_cursors")?;
    for row in stmt.query_map([],|r|Ok(((r.get::<_,String>(0)?,r.get::<_,u64>(1)?),r.get::<_,i64>(2)?)))? {let(k,v)=row?;cursors.insert(k,v);}
    let mut expected:BTreeMap<_,_>=definitions.iter().map(|d|((d.name.clone(),d.revision),d.start_unix_ms-1)).collect();
    let index:BTreeMap<_,_>=definitions.iter().map(|d|((d.name.clone(),d.revision),d)).collect();
    let mut occurrences=Vec::new();let mut stmt=db.prepare("SELECT id,name,revision,scheduled_unix_ms,payload,payload_hash,operation_id FROM routine_occurrences ORDER BY name,revision,scheduled_unix_ms")?;
    for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?)))? {
        let(id,name,revision,instant,payload,hash,operation)=row?;let o:RoutineOccurrence=decoded(&payload,&hash)?;
        let key=(name,revision);let d=index.get(&key).ok_or_else(||corrupt("orphan routine occurrence"))?;
        let cursor=expected.get_mut(&key).ok_or_else(||corrupt("missing routine cursor"))?;
        // Historical instants remain authoritative across timezone database updates.
        if occurrences.len()>=100_000||o.id!=id||RoutineOccurrence::identity(d,instant).map_err(StoreError::Corrupt)?!=id
            ||o.routine!=d.reference().map_err(StoreError::Corrupt)?||o.scheduled_unix_ms!=instant||o.first_unix_ms<=*cursor||o.first_unix_ms<d.start_unix_ms
            ||instant<o.first_unix_ms||instant>o.observed_unix_ms||o.slots==0||o.slots>i64::MAX as u64||o.control_revision==0||o.control_revision>i64::MAX as u64
            ||o.operation.as_ref().map(|id|id.as_str())!=operation.as_deref()
            ||(o.disposition==RoutineDisposition::Enqueued)!=o.operation.is_some() {return Err(corrupt("routine occurrence binding mismatch"));}
        if let Some(op)=&o.operation {
            let operation=read_operation(db,op)?;
            if op.as_str()!=id||operation.task.is_some()||operation.kind!="routine.run"||operation.target!=format!("routine:{}",d.name)
                ||operation.expected_revision!=o.control_revision||operation.payload_version!=1||operation.payload!=serde_json::to_value(&o).map_err(|_|corrupt("invalid occurrence"))?
                ||operation.due_unix_ms!=o.observed_unix_ms||operation.idempotency_key!=id {return Err(corrupt("routine outbox binding mismatch"));}
        }
        *cursor=instant;occurrences.push(o);
    }
    if expected!=cursors {return Err(corrupt("routine cursor does not match immutable occurrence history"));}
    Ok((definitions,occurrences))
}
pub(super) fn check(db:&Connection,id:&OperationId)->Result<()> {
    schema(db)?;let(definitions,occurrences)=read_all(db)?;
    let occurrence=occurrences.iter().find(|o|o.operation.as_ref()==Some(id)).ok_or_else(||invalid("routine operation lacks an approved occurrence"))?;
    let definition=definitions.iter().rev().find(|d|format!("routine-{}",d.name)==occurrence.routine.id).ok_or_else(||invalid("routine definition missing"))?;
    if !definition.enabled||definition.reference().map_err(|s|invalid(&s))?!=occurrence.routine {return Err(invalid("routine revision withdrawn"));}
    project_matches(db,definition)?;
    crate::routines::validate_current(definition).map_err(|_|invalid("routine authority or script changed"))?;
    let control=super::control::read(db)?;
    if control.config_digest!=definition.config.digest {return Err(invalid("routine configuration is not acknowledged"));}
    Ok(())
}
impl SqliteStore {
    pub fn install_routine(&mut self,prepared:&PreparedRoutine,expected_head:u64)->Result<VersionedReference> {
        let d=&prepared.definition;let reference=d.reference().map_err(|s|invalid(&s))?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        let path=std::fs::canonicalize(tx.path().ok_or_else(||invalid("routine requires file-backed store"))?).map_err(|_|invalid("routine store unavailable"))?;
        if path.to_str()!=Some(&d.project_store) {return Err(invalid("routine belongs to a different project"));}
        let(definitions,occurrences)=read_all(&tx)?;
        let current=definitions.iter().rev().find(|old|old.name==d.name);
        if d.revision!=current.map(|old|old.revision).unwrap_or(0)+1 {return Err(StoreError::Conflict);}
        if definitions.len()>=10_000||current.is_none()&&definitions.iter().map(|d|&d.name).collect::<std::collections::BTreeSet<_>>().len()>=128 {return Err(invalid("routine history bound reached"));}
        let(payload,hash)=encoded(d)?;
        tx.execute("INSERT INTO routine_revisions VALUES(?1,?2,?3,?4)",params![d.name,integer(d.revision)?,payload,hash])?;
        tx.execute("INSERT INTO routine_cursors VALUES(?1,?2,?3)",params![d.name,integer(d.revision)?,d.start_unix_ms-1])?;
        // A pending zero-claim intent proves no adapter entered. Other history
        // retains overlap ownership even after an operator retires the delivery.
        for o in occurrences.iter().filter(|o|o.routine.id==reference.id) {
            if let Some(id)=&o.operation {
                let delivery=super::delivery::delivery(&tx,id)?;
                if delivery.state==DeliveryState::Pending&&delivery.attempts==0 {
                    super::delivery::update_outcome(&tx,&delivery,&crate::operations::Outcome::PermanentFailure{diagnostic:"signed routine revision replaced before any claim".into()},o.observed_unix_ms,"routine-policy")?;
                }
            }
        }
        log(&tx,"routine.revision_installed",&d.name,d.revision,d)?;tx.commit()?;Ok(reference)
    }
    pub fn schedule_routine(&mut self,prepared:&PreparedRoutineTick,expected_head:u64)->Result<Option<RoutineOccurrence>> {
        let d=&prepared.definition;let now=prepared.now;super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        let(definitions,occurrences)=read_all(&tx)?;
        if definitions.iter().rev().find(|old|old.name==d.name)!=Some(d) {return Err(StoreError::Conflict);}
        project_matches(&tx,d)?;
        let control=super::control::read(&tx)?;
        if !d.enabled {tx.commit()?;return Ok(None);}
        if control.state!=ProjectState::Active||control.reconciliation_required||control.config_digest!=d.config.digest {return Err(invalid("routine project is not admitted"));}
        let after:i64=tx.query_row("SELECT after_unix_ms FROM routine_cursors WHERE name=?1 AND revision=?2",params![d.name,integer(d.revision)?],|r|r.get(0))?;
        let schedule=crate::schedule::parse_schedule(&d.schedule).map_err(|_|invalid("invalid routine schedule"))?;
        let zone=jiff::tz::TimeZone::get(&d.timezone).map_err(|_|invalid("invalid routine timezone"))?;
        let Some(due)=crate::schedule::due_window(&schedule,&zone,d.start_unix_ms,after,now).map_err(|_|invalid("invalid routine clock"))? else {tx.commit()?;return Ok(None);};
        if occurrences.len()>=100_000 {return Err(invalid("routine occurrence bound reached"));}
        let reference=d.reference().map_err(|s|invalid(&s))?;
        let mut overlap=false;
        for o in occurrences.iter().filter(|o|o.routine.id==reference.id) {if let Some(id)=&o.operation {
            let delivery=super::delivery::delivery(&tx,id)?;
            if delivery.attempts>0||matches!(delivery.state,DeliveryState::Pending|DeliveryState::Claimed|DeliveryState::Ambiguous) {overlap=true;}
        }}
        let disposition=if due.slots>1&&d.missed==MissedRunPolicy::Skip {RoutineDisposition::SkippedMissed}else if overlap {RoutineDisposition::SkippedOverlap}else{RoutineDisposition::Enqueued};
        let id=RoutineOccurrence::identity(d,due.last_unix_ms).map_err(|s|invalid(&s))?;
        let operation=if disposition==RoutineDisposition::Enqueued {Some(OperationId::new(id.clone()).map_err(|s|invalid(&s))?)}else{None};
        let record=RoutineOccurrence{id:id.clone(),routine:reference,first_unix_ms:due.first_unix_ms,scheduled_unix_ms:due.last_unix_ms,slots:due.slots,observed_unix_ms:now,control_revision:control.revision,disposition,operation};
        let(payload,hash)=encoded(&record)?;
        if let Some(op)=&record.operation {
            tx.execute("INSERT INTO operations VALUES(?1,NULL,'routine.run',?2,1,?3,?4,?5,?6,?1)",params![op.as_str(),format!("routine:{}",d.name),payload,hash,integer(control.revision)?,now])?;
        }
        tx.execute("INSERT INTO routine_occurrences VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,d.name,integer(d.revision)?,due.last_unix_ms,payload,hash,record.operation.as_ref().map(OperationId::as_str)])?;
        tx.execute("UPDATE routine_cursors SET after_unix_ms=?3 WHERE name=?1 AND revision=?2",params![d.name,integer(d.revision)?,due.last_unix_ms])?;
        log(&tx,"routine.occurrence_recorded",&id,d.revision,&record)?;
        tx.commit()?;Ok(Some(record))
    }
}
