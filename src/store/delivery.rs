use super::*;
use crate::operations::{Claim,Delivery,DeliveryState,Outcome};

fn text(value:&str)->Result<()> {
    if value.trim().is_empty() || value.len()>8192 { return Err(StoreError::Invalid("nonempty evidence/owner of at most 8192 bytes required".into())); }
    Ok(())
}
pub(super) fn now_check(now:i64)->Result<()> {
    if now<0 || now>i64::MAX-600_000 { return Err(StoreError::Invalid("clock outside supported range".into())); } Ok(())
}
pub(super) fn delivery(db:&Connection,id:&OperationId)->Result<Delivery> {
    let value=db.query_row("SELECT revision,state,epoch,attempts,owner,lease_until_ms,next_due_ms,last_outcome FROM operation_delivery WHERE operation_id=?1",[id.as_str()],|r|{
        Ok(serde_json::json!({"operation":id,"revision":r.get::<_,u64>(0)?,"state":r.get::<_,String>(1)?,"epoch":r.get::<_,u64>(2)?,"attempts":r.get::<_,u32>(3)?,"owner":r.get::<_,Option<String>>(4)?,"lease_until_ms":r.get::<_,Option<i64>>(5)?,"next_due_ms":r.get::<_,i64>(6)?,"last_outcome":r.get::<_,Option<String>>(7)?}))
    })?;
    let mut value=value;
    if let Some(raw)=value["last_outcome"].as_str() { value["last_outcome"]=serde_json::from_str(raw).map_err(|e|StoreError::Corrupt(e.to_string()))?; }
    decode(value)
}
pub(super) fn read_all(db:&Connection)->Result<Vec<Delivery>> {
    let ids={let mut stmt=db.prepare("SELECT operation_id FROM operation_delivery ORDER BY operation_id")?;stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
    ids.into_iter().map(|s|delivery(db,&OperationId::new(s).map_err(StoreError::Corrupt)?)).collect()
}
fn log(tx:&Connection,id:&OperationId,revision:u64,kind:&str,payload:serde_json::Value)->Result<()> {
    tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,id.as_str(),integer(revision)?,payload.to_string()])?;Ok(())
}
fn increment(n:u64)->Result<u64>{n.checked_add(1).filter(|n|*n<=i64::MAX as u64).ok_or_else(||StoreError::Invalid("delivery counter exhausted".into()))}
/// Project-scoped operations cannot borrow an unrelated task's revision fence.
fn binding_current(db:&Connection,id:&OperationId,admission:bool)->Result<bool> {
    let (task,kind,expected):(Option<String>,String,u64)=db.query_row("SELECT task_id,kind,expected_revision FROM operations WHERE id=?1",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    if let Some(task)=task {
        let actual:u64=db.query_row("SELECT revision FROM tasks WHERE id=?1",[task],|r|r.get(0))?;
        return Ok(actual==expected);
    }
    let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<15||kind!="routine.run" {return Err(StoreError::Invalid("unsupported project operation".into()));}
    let control=super::control::read(db)?;
    Ok(control.revision==expected && (!admission || (control.state==ProjectState::Active&&!control.reconciliation_required)))
}
fn payload_valid(db:&Connection,id:&OperationId)->Result<()> {
    let (payload,hash):(String,String)=db.query_row("SELECT payload,payload_hash FROM operations WHERE id=?1",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash {return Err(StoreError::Corrupt("operation payload hash mismatch".into()));}
    Ok(())
}
pub(super) fn update_outcome(tx:&Connection,old:&Delivery,outcome:&Outcome,now:i64,actor:&str)->Result<Delivery> {
    text(outcome.evidence())?;
    if let Outcome::Confirmed{observed_identity}=outcome {super::finalization::apply_receipt(tx,&old.operation,observed_identity)?;}
    let mut state=match outcome { Outcome::Confirmed{..}=>"confirmed",Outcome::Retryable{..}=>"pending",Outcome::Ambiguous{..}=>"ambiguous",Outcome::PermanentFailure{..}=>"permanent_failure" };
    let mut outcome=outcome.clone();
    if state=="pending" && old.attempts>=32 { state="permanent_failure";outcome=Outcome::PermanentFailure{diagnostic:format!("retry budget exhausted after confirmed no effect: {}",outcome.evidence())}; }
    let backoff=1_000_i64*(1_i64<<old.attempts.saturating_sub(1).min(8));
    let due=now+backoff.min(300_000);
    let revision=increment(old.revision)?;
    tx.execute("UPDATE operation_delivery SET revision=?2,state=?3,owner=NULL,lease_until_ms=NULL,next_due_ms=?4,last_outcome=?5 WHERE operation_id=?1",params![old.operation.as_str(),integer(revision)?,state,due,serde_json::to_string(&outcome).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
    log(tx,&old.operation,revision,"operation.outcome",serde_json::json!({"actor":actor,"outcome":outcome,"epoch":old.epoch}))?;
    delivery(tx,&old.operation)
}
impl SqliteStore {
    pub fn deliveries(&mut self)->Result<Vec<Delivery>> {
        let tx=self.connection.transaction()?;check_schema(&tx)?;
        let rows=read_all(&tx)?;
        tx.commit()?;Ok(rows)
    }
    /// Claim commits before external work. Expired claims are NEVER automatically
    /// retried: expire_claims first marks them ambiguous for explicit observation.
    pub fn claim_operation(&mut self,id:&OperationId,expected:u64,owner:&str,now:i64,lease_ms:i64)->Result<Claim> {
        text(owner)?;now_check(now)?;
        if !(1..=300_000).contains(&lease_ms) { return Err(StoreError::Invalid("lease must be 1..300000 ms".into())); }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let old=delivery(&tx,id)?;
        if old.revision!=expected || old.state!=DeliveryState::Pending || old.next_due_ms>now || old.attempts>=32 { return Err(StoreError::Conflict); }
        payload_valid(&tx,id)?;
        if !binding_current(&tx,id,true)? {return Err(StoreError::Conflict);}
        let revision=increment(old.revision)?;let epoch=increment(old.epoch)?;let until=now+lease_ms;
        let kind:String=tx.query_row("SELECT kind FROM operations WHERE id=?1",[id.as_str()],|r|r.get(0))?;
        if kind=="runtime.launch" {super::approvals::consume(&tx,id,revision,epoch,now)?;}
        if kind=="routine.run" {super::routines::check(&tx,id)?;}
        tx.execute("UPDATE operation_delivery SET revision=?2,state='claimed',epoch=?3,attempts=attempts+1,owner=?4,lease_until_ms=?5 WHERE operation_id=?1",params![id.as_str(),integer(revision)?,integer(epoch)?,owner,until])?;
        log(&tx,id,revision,"operation.claimed",serde_json::json!({"owner":owner,"epoch":epoch,"lease_until_ms":until}))?;
        tx.commit()?;Ok(Claim{operation:id.clone(),revision,owner:owner.into(),epoch,lease_until_ms:until})
    }
    /// Last pre-effect fence; callers must separately retain exclusive external
    /// ownership. SQLite fencing cannot retract an effect after this check.
    pub fn validate_claim(&mut self,claim:&Claim,now:i64)->Result<()> {
        now_check(now)?;
        let tx=self.connection.transaction()?;check_schema(&tx)?;
        let old=delivery(&tx,&claim.operation)?;
        if old.state!=DeliveryState::Claimed || old.revision!=claim.revision || old.epoch!=claim.epoch || old.owner.as_deref()!=Some(&claim.owner) || old.lease_until_ms!=Some(claim.lease_until_ms) || now>=claim.lease_until_ms {return Err(StoreError::Conflict);}
        payload_valid(&tx,&claim.operation)?;
        if !binding_current(&tx,&claim.operation,true)? {return Err(StoreError::Conflict);}
        let kind:String=tx.query_row("SELECT kind FROM operations WHERE id=?1",[claim.operation.as_str()],|r|r.get(0))?;
        if kind=="runtime.launch" {super::approvals::validate_use(&tx,claim,now)?;}
        if kind=="routine.run" {super::routines::check(&tx,&claim.operation)?;}
        tx.commit()?;Ok(())
    }
    /// Rechecks owner, epoch, revision and lease in the outcome transaction.
    pub fn finish_operation(&mut self,claim:&Claim,outcome:Outcome,now:i64)->Result<Delivery> {
        now_check(now)?;text(outcome.evidence())?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let old=delivery(&tx,&claim.operation)?;
        if old.state!=DeliveryState::Claimed || old.revision!=claim.revision || old.epoch!=claim.epoch || old.owner.as_deref()!=Some(&claim.owner) || old.lease_until_ms!=Some(claim.lease_until_ms) || now>=claim.lease_until_ms {return Err(StoreError::Conflict);}
        if !binding_current(&tx,&claim.operation,false)? {return Err(StoreError::Conflict);}
        let result=update_outcome(&tx,&old,&outcome,now,&claim.owner)?;tx.commit()?;Ok(result)
    }
    pub fn expire_claims(&mut self,now:i64)->Result<usize> {
        now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let ids={let mut stmt=tx.prepare("SELECT operation_id FROM operation_delivery WHERE state='claimed' AND lease_until_ms<=?1 ORDER BY operation_id")?;stmt.query_map([now],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
        for id in &ids {
            let id=OperationId::new(id.clone()).map_err(StoreError::Corrupt)?;let mut old=delivery(&tx,&id)?;
            old.epoch=increment(old.epoch)?;
            tx.execute("UPDATE operation_delivery SET epoch=?2 WHERE operation_id=?1",params![id.as_str(),integer(old.epoch)?])?;
            let outcome=Outcome::Ambiguous{observation_required:"lease expired; external effect may have occurred; observe identity before retry".into()};
            update_outcome(&tx,&old,&outcome,now,"lease-expiry")?;
        }
        tx.commit()?;Ok(ids.len())
    }
    /// Only explicit reconciliation evidence can release ambiguity. An error,
    /// timeout or unreachable remote is NOT evidence that no effect occurred.
    pub fn observe_operation(&mut self,id:&OperationId,expected:u64,actor:&str,outcome:Outcome,now:i64)->Result<Delivery> {
        now_check(now)?;text(actor)?;text(outcome.evidence())?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        let old=delivery(&tx,id)?;
        if old.state!=DeliveryState::Ambiguous || old.revision!=expected {return Err(StoreError::Conflict);}
        let result=update_outcome(&tx,&old,&outcome,now,actor)?;tx.commit()?;Ok(result)
    }
}

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Retire accepted intent without claiming its external effect is absent.
    /// Claimed work must first expire; retirement never releases resources.
    pub fn retire_operation(&mut self,id:&OperationId,expected_revision:u64,expected_head:u64,reason:&str,now:i64)->Result<Delivery> {
        now_check(now)?;text(reason)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let old=delivery(&tx,id)?;
        if old.revision!=expected_revision || !matches!(old.state,DeliveryState::Pending|DeliveryState::Ambiguous) {return Err(StoreError::Conflict);}
        let outcome=Outcome::PermanentFailure{diagnostic:format!("Operator retired intent; any prior effect remains possible. Reason: {reason}")};
        let result=update_outcome(&tx,&old,&outcome,now,"operator-retirement")?;tx.commit()?;Ok(result)
    }
}
