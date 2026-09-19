use super::*;
use crate::operations::{DeliveryState,Outcome,receipts::{ReceiptReport,ReceiptEvidence}};

impl SqliteStore {
    /// Interpret hash-checked imported receipts and optionally persist matching
    /// confirmations in the same transaction. Never releases ambiguity to retry.
    pub fn observe_imported_receipts(&mut self,expected_head:u64,now:i64,apply:bool)->Result<ReceiptReport> {
        super::delivery::now_check(now)?;
        let tx=self.connection.transaction_with_behavior(if apply {TransactionBehavior::Immediate}else{TransactionBehavior::Deferred})?;
        check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let sources=super::import::read_sources(&tx)?;
        let evidence=ReceiptEvidence::new(&sources);
        let operations=read_operations(&tx)?;
        let tasks=read_tasks(&tx)?.into_iter().map(|t|(t.id,t.revision)).collect::<std::collections::BTreeMap<_,_>>();
        let mut observations=Vec::new();let mut confirmed=0;
        for op in operations.iter().filter(|op|matches!(op.kind.as_str(),"legacy.notify"|"legacy.finalize")) {
            let old=super::delivery::delivery(&tx,&op.id)?;
            let mut observation=evidence.observe(op);
            if old.state!=DeliveryState::Ambiguous {
                observation.receipt=None;observation.blocked=Some("delivery is not ambiguous; no observation applied".into());
            } else if tasks.get(&op.task)!=Some(&op.expected_revision) {
                observation.receipt=None;observation.blocked=Some("task revision changed; imported receipt cannot confirm current binding".into());
            }
            if apply {
                if let Some(receipt)=&observation.receipt {
                    super::delivery::update_outcome(&tx,&old,&Outcome::Confirmed{observed_identity:receipt.clone()},now,"imported-receipt-observer")?;
                    confirmed+=1;
                }
            }
            observations.push(observation);
        }
        let result=ReceiptReport{previous_head:expected_head,head:head(&tx)?,confirmed,observations};
        tx.commit()?;Ok(result)
    }
}
