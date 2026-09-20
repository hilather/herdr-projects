//! Immutable review decisions and atomic promotion. Delivery ack is T06.3.
use super::*;
use crate::domain::{PromotionReceipt, ReviewDecision};
use rusqlite::OptionalExtension;

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<22 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
impl SqliteStore {
    pub fn insert_review_decision(&mut self,row:&ReviewDecision)->Result<()> {
        if row.id.is_empty()||row.id.len()>128||!matches!(row.decision.as_str(),"approve"|"reject"|"narrow")
            || row.payload_digest.len()!=64 || row.reason.is_empty()||row.reason.len()>4096 {
            return Err(invalid("invalid review decision"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        tx.execute(
            "INSERT INTO review_decisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![row.id,row.proposal_id,row.payload_digest,row.decision,row.classification,row.reviewed_heads,row.reason,row.created_unix_ms]
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.review_recorded',?1,1,1,?2)",
            params![row.id,serde_json::json!({"proposal":row.proposal_id,"decision":row.decision}).to_string()])?;
        tx.commit()?;Ok(())
    }
    pub fn review_decision(&mut self,id:&str)->Result<Option<ReviewDecision>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,proposal_id,payload_digest,decision,classification,reviewed_heads,reason,created_unix_ms FROM review_decisions WHERE id=?1",
            [id],|r|Ok(ReviewDecision{id:r.get(0)?,proposal_id:r.get(1)?,payload_digest:r.get(2)?,decision:r.get(3)?,classification:r.get(4)?,reviewed_heads:r.get(5)?,reason:r.get(6)?,created_unix_ms:r.get(7)?})
        ).optional()?)
    }
    pub fn memory_promotion(&mut self,proposal_id:&str)->Result<Option<PromotionReceipt>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT proposal_id,decision_id,sequence,change_ids FROM memory_promotions WHERE proposal_id=?1",
            [proposal_id],|r| {
                let change_ids:String=r.get(3)?;
                Ok(PromotionReceipt{proposal_id:r.get(0)?,decision_id:r.get(1)?,sequence:r.get(2)?,change_ids:serde_json::from_str(&change_ids).unwrap_or_default(),reused:true})
            }
        ).optional()?)
    }
    pub fn promote_reviewed_proposal(&mut self,proposal_id:&str,decision:&ReviewDecision,changes:&[crate::domain::NewRevision],invalidations:&[(String,String,String,String)],now_unix_ms:i64)->Result<PromotionReceipt> {
        if decision.decision!="approve" {return Err(invalid("promotion requires an approve decision"));}
        if decision.proposal_id!=proposal_id {return Err(invalid("decision does not belong to proposal"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if let Some(existing)=tx.query_row(
            "SELECT proposal_id,decision_id,sequence,change_ids FROM memory_promotions WHERE proposal_id=?1",
            [proposal_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?))
        ).optional()? {
            if existing.1!=decision.id {return Err(invalid("proposal already promoted under a different decision"));}
            tx.commit()?;
            return Ok(PromotionReceipt{proposal_id:existing.0,decision_id:existing.1,sequence:existing.2,change_ids:serde_json::from_str(&existing.3).unwrap_or_default(),reused:true});
        }
        let mut change_ids=Vec::new();
        let mut last_seq=head(&tx)?;
        for next in changes {
            let (head,seq)=super::memory::apply_memory_revision_in_tx(&tx,next)?;
            last_seq=seq;
            change_ids.push(format!("{}:{}",head.record_id.as_str(),head.revision));
        }
        for (id,task_id,record_id,severity) in invalidations {
            if !matches!(severity.as_str(),"informational"|"reconcile_before_completion"|"stop_at_checkpoint") {
                return Err(invalid("invalid invalidation severity"));
            }
            tx.execute(
                "INSERT INTO memory_invalidations VALUES(?1,?2,?3,?4,?5,?6,NULL,?7)",
                params![id,task_id,proposal_id,record_id,severity,integer(last_seq)?,"promoted"]
            )?;
        }
        let encoded=serde_json::to_string(&change_ids).map_err(|e|invalid(&e.to_string()))?;
        tx.execute(
            "INSERT INTO memory_promotions VALUES(?1,?2,?3,?4,?5,?6)",
            params![proposal_id,decision.id,decision.payload_digest,integer(last_seq)?,encoded,now_unix_ms]
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.promoted',?1,1,1,?2)",
            params![proposal_id,serde_json::json!({"decision":decision.id,"sequence":last_seq,"changes":change_ids}).to_string()])?;
        last_seq=head(&tx)?;
        tx.commit()?;
        Ok(PromotionReceipt{proposal_id:proposal_id.into(),decision_id:decision.id.clone(),sequence:last_seq,change_ids,reused:false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema21_upgrade_adds_empty_review_tables() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; UPDATE store_meta SET schema_version=21; PRAGMA user_version=21;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=22;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.memory_promotion("mp-x").unwrap().is_none());
        db.integrity_check().unwrap();
    }
}
