//! A removable work index, never a substitute for validated cleanup evidence.
use super::*;

pub(super) enum Scan { Clear, Blocked, Incomplete }

fn receipt(db:&Connection,d:&RoutineDefinition,o:&RoutineOccurrence,id:&OperationId,budget:Option<&read_budget::ReadBudget>)->Result<Option<RoutineReceipt>> {
    let mut stmt=db.prepare("SELECT revision,payload_version,payload FROM events WHERE kind='routine.completed' AND entity=?1 ORDER BY sequence LIMIT 2")?;
    let mut rows=stmt.query([id.as_str()])?;
    let Some(row)=rows.next()? else{return Ok(None);};
    if let Some(budget)=budget {budget.row(row,&[(2,1)])?;}
    let revision:u64=row.get(0)?;let version:u32=row.get(1)?;
    let payload:String=row.get(2)?;
    if payload.len()>MAX_RECORD_BYTES {return Err(corrupt("routine completion bound exceeded"));}
    let (receipt,hash):(RoutineReceipt,String)=serde_json::from_str(&payload).map_err(|_|corrupt("invalid routine completion"))?;
    if rows.next()?.is_some()||version!=1||receipt.operation!=*id||revision!=receipt.claim.revision||encoded(&receipt)?.1!=hash {return Err(corrupt("routine completion identity mismatch"));}
    validate_receipt(&receipt,d,o)?;
    Ok(Some(receipt))
}

fn blocked(delivery:&crate::operations::Delivery,receipt:Option<&RoutineReceipt>)->bool {
    let cleaned=receipt.is_some_and(|r|r.cleanup_verified&&r.claim.epoch==delivery.epoch)
        &&delivery.attempts==1&&matches!(delivery.state,DeliveryState::Confirmed|DeliveryState::PermanentFailure);
    !cleaned&&(delivery.attempts>0||matches!(delivery.state,DeliveryState::Pending|DeliveryState::Claimed|DeliveryState::Ambiguous))
}

/// Clear at most 64 obsolete entries per wake. An unfinished scan never means
/// no overlap; the caller commits its progress and retries before scheduling.
pub(super) fn scan(db:&Connection,name:&str,budget:Option<&read_budget::ReadBudget>)->Result<Scan> {
    let mut stmt=db.prepare("SELECT operation_id FROM routine_overlap_work WHERE name=?1 ORDER BY operation_id LIMIT 65")?;
    let mut rows=stmt.query([name])?;let mut ids=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[])?;}
        ids.push(row.get::<_,String>(0)?);
    }
    drop(rows);drop(stmt);
    for (index,id) in ids.iter().take(64).enumerate() {
        let id=OperationId::new(id).map_err(StoreError::Corrupt)?;
        let(d,o)=selected_occurrence(db,&id,budget)?;
        if d.name!=name {return Err(corrupt("routine overlap index identity mismatch"));}
        let delivery=super::super::delivery::delivery_with_budget(db,&id,budget)?;
        let receipt=receipt(db,&d,&o,&id,budget)?;
        if blocked(&delivery,receipt.as_ref()){return Ok(Scan::Blocked);}
        db.execute("DELETE FROM routine_overlap_work WHERE operation_id=?1",[id.as_str()])?;
        // Large valid output receipts can consume much of the shared budget.
        // Preserve completed work before another row would exceed it; retrying
        // the same oversized page forever would starve an otherwise safe run.
        if index+1<ids.len()&&budget.is_some_and(|budget|budget.remaining_units()<25*1024*1024) {return Ok(Scan::Incomplete);}
    }
    Ok(if ids.len()>64 {Scan::Incomplete}else{Scan::Clear})
}

/// Explicit migration may inspect history once. Validate it before pruning the
/// new index, so a long-lived upgraded project does not need thousands of wakes.
pub(super) fn backfill(db:&Connection)->Result<()> {
    let(definitions,occurrences)=read_all(db)?;
    let receipts=read_receipts(db,&definitions,&occurrences)?;
    let receipts:BTreeMap<_,_>=receipts.iter().map(|r|(&r.operation,r)).collect();
    for occurrence in occurrences {
        if let Some(id)=occurrence.operation {
            let delivery=super::super::delivery::delivery(db,&id)?;
            if !blocked(&delivery,receipts.get(&id).copied()) {
                db.execute("DELETE FROM routine_overlap_work WHERE operation_id=?1",[id.as_str()])?;
            }
        }
    }
    Ok(())
}
