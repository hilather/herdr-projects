//! Immutable worker proposals and deterministic validation records. Never authoritative memory.
use super::*;
use crate::domain::{ProposalReceipt, StoredProposal, PROPOSAL_LIMIT};
use rusqlite::OptionalExtension;

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<21 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
impl SqliteStore {
    pub fn insert_proposal(&mut self,id:&str,digest:&str,task_id:&str,attempt_id:&str,snapshot_id:Option<&str>,review_state:&str,payload:&str,reason:&str,observed_heads:&str,now_unix_ms:i64)->Result<ProposalReceipt> {
        if id.is_empty()||id.len()>128||digest.len()!=64||payload.is_empty()||payload.len()>PROPOSAL_LIMIT
            || !matches!(review_state,"validated"|"rejected") {
            return Err(invalid("invalid memory proposal"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let existing:Option<(String,String,String)>=tx.query_row(
            "SELECT payload_digest,review_state,id FROM memory_proposals WHERE id=?1",[id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        ).optional()?;
        if let Some((old_digest,state,_))=existing {
            if old_digest!=digest {return Err(invalid("idempotency key conflict"));}
            let validation:String=tx.query_row(
                "SELECT result FROM proposal_validations WHERE proposal_id=?1 AND payload_digest=?2",
                params![id,digest],|r|r.get(0)
            ).optional()?.unwrap_or_else(|| if state=="validated" {"accepted".into()} else {"rejected".into()});
            let stored_reason:String=tx.query_row(
                "SELECT reason FROM proposal_validations WHERE proposal_id=?1 AND payload_digest=?2",
                params![id,digest],|r|r.get(0)
            ).optional()?.unwrap_or_default();
            tx.commit()?;
            return Ok(ProposalReceipt{proposal_id:id.into(),payload_digest:digest.into(),review_state:state,validation,reason:stored_reason,reused:true});
        }
        let seq=head(&tx)?;
        tx.execute(
            "INSERT INTO memory_proposals VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![id,digest,task_id,attempt_id,snapshot_id,review_state,payload,now_unix_ms]
        )?;
        let result=if review_state=="validated" {"accepted"} else {"rejected"};
        tx.execute(
            "INSERT INTO proposal_validations VALUES(?1,?2,'proposal-validation',1,?3,?4,?5,?6)",
            params![id,digest,result,reason,observed_heads,integer(seq)?]
        )?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.proposal_received',?1,1,1,?2)",
            params![id,serde_json::json!({"id":id,"digest":digest,"state":review_state}).to_string()])?;
        tx.commit()?;
        Ok(ProposalReceipt{proposal_id:id.into(),payload_digest:digest.into(),review_state:review_state.into(),validation:result.into(),reason:reason.into(),reused:false})
    }
    pub fn memory_proposal(&mut self,id:&str)->Result<Option<StoredProposal>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,payload_digest,task_id,attempt_id,snapshot_id,review_state,created_unix_ms FROM memory_proposals WHERE id=?1",
            [id],|r|Ok(StoredProposal{id:r.get(0)?,payload_digest:r.get(1)?,task_id:r.get(2)?,attempt_id:r.get(3)?,snapshot_id:r.get(4)?,review_state:r.get(5)?,created_unix_ms:r.get(6)?})
        ).optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema20_upgrade_adds_empty_proposal_tables() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE proposal_validations; DROP TABLE memory_proposals; UPDATE store_meta SET schema_version=20; PRAGMA user_version=20;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=21;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.memory_proposal("mp-x").unwrap().is_none());
        db.integrity_check().unwrap();
    }
}
