use super::*;
use crate::operations::finalization::{Finalization,FinalizationReceipt};

/// The delivery outcome and task disposition commit in the same transaction.
/// Receipt bytes are verified outside SQL by the guarded filesystem adapter.
pub(super) fn apply_receipt(db:&Connection,id:&OperationId,identity:&str)->Result<()> {
    let kind:String=db.query_row("SELECT kind FROM operations WHERE id=?1",[id.as_str()],|r|r.get(0))?;
    if kind!="runtime.finalization" {return Ok(());}
    let operation=read_operation(db,id)?;
    let payload=Finalization::decode(&operation).map_err(|_|StoreError::Invalid("invalid finalization payload".into()))?;
    let receipt:FinalizationReceipt=serde_json::from_str(identity).map_err(|_|StoreError::Invalid("invalid finalization receipt".into()))?;
    receipt.validate(&operation,&payload).map_err(|_|StoreError::Invalid("finalization receipt mismatch".into()))?;
    let control=super::control::read(db)?;let mut tasks=read_tasks(db)?;
    payload.validate_state(&operation,&control,&tasks,&read_attempts(db)?,&super::runtime::read_all(db)?).map_err(|_|StoreError::Conflict)?;
    let task=tasks.iter_mut().find(|t|Some(&t.id)==operation.task.as_ref()).ok_or(StoreError::Conflict)?;
    task.revision=task.revision.checked_add(1).ok_or_else(||StoreError::Invalid("task revision exhausted".into()))?;
    task.state=TaskState::AwaitingReview;
    db.execute("UPDATE tasks SET revision=?2,state='awaiting_review' WHERE id=?1",params![task.id.as_str(),integer(task.revision)?])?;
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('task.changed',?1,?2,1,?3)",params![task.id.as_str(),integer(task.revision)?,serde_json::to_string(task).map_err(|e|StoreError::Invalid(e.to_string()))?])?;
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.artifacts_finalized',?1,?2,1,?3)",params![payload.binding,integer(payload.binding_revision)?,identity])?;
    Ok(())
}

impl SqliteStore {
    pub fn observe_finalization(&mut self,id:&OperationId,expected_revision:u64,expected_head:u64,receipt:&FinalizationReceipt,now:i64)->Result<crate::operations::Delivery> {
        super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let old=super::delivery::delivery(&tx,id)?;
        if old.revision!=expected_revision||old.state!=crate::operations::DeliveryState::Ambiguous{return Err(StoreError::Conflict);}
        let kind:String=tx.query_row("SELECT kind FROM operations WHERE id=?1",[id.as_str()],|r|r.get(0))?;
        if kind!="runtime.finalization" {return Err(StoreError::Invalid("not a finalization intent".into()));}
        let identity=serde_json::to_string(receipt).map_err(|e|StoreError::Invalid(e.to_string()))?;
        let result=super::delivery::update_outcome(&tx,&old,&crate::operations::Outcome::Confirmed{observed_identity:identity},now,"artifact-receipt-observer")?;tx.commit()?;Ok(result)
    }
}
