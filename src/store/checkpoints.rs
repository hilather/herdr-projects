//! Coordinator sessions and checkpoints. Snapshot FK is required; cursor advances only on ack.
use super::*;
use crate::domain::{CoordinatorCheckpoint, CoordinatorSession, CheckpointSizes};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<20 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
impl SqliteStore {
    pub fn upsert_coordinator_session(&mut self,herdr_session:&str,now_unix_ms:i64)->Result<CoordinatorSession> {
        if herdr_session.is_empty()||herdr_session.len()>128||herdr_session.chars().any(char::is_control) {
            return Err(invalid("invalid coordinator session"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let existing:Option<(String,i64,String,Option<String>,u64)>=tx.query_row(
            "SELECT id,created_unix_ms,herdr_session,last_checkpoint_id,cursor_seq FROM coordinator_sessions WHERE herdr_session=?1",
            [herdr_session],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
        ).optional()?;
        let session=if let Some((id,created,herdr,last,cursor))=existing {
            CoordinatorSession{id,created_unix_ms:created,herdr_session:herdr,last_checkpoint_id:last,cursor_seq:cursor}
        } else {
            let id=format!("sess-{:x}",Sha256::digest(herdr_session.as_bytes()));
            tx.execute("INSERT INTO coordinator_sessions VALUES(?1,?2,?3,NULL,0)",params![id,now_unix_ms,herdr_session])?;
            CoordinatorSession{id,created_unix_ms:now_unix_ms,herdr_session:herdr_session.into(),last_checkpoint_id:None,cursor_seq:0}
        };
        tx.commit()?;Ok(session)
    }
    pub fn coordinator_session(&mut self,herdr_session:&str)->Result<Option<CoordinatorSession>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,created_unix_ms,herdr_session,last_checkpoint_id,cursor_seq FROM coordinator_sessions WHERE herdr_session=?1",
            [herdr_session],|r|Ok(CoordinatorSession{id:r.get(0)?,created_unix_ms:r.get(1)?,herdr_session:r.get(2)?,last_checkpoint_id:r.get(3)?,cursor_seq:r.get(4)?})
        ).optional()?)
    }
    pub fn coordinator_checkpoint(&mut self,id:&str)->Result<Option<CoordinatorCheckpoint>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,session_id,kind,snapshot_id,from_seq,through_seq,manifest_hash,full_chars,delta_chars,created_unix_ms,acked FROM coordinator_checkpoints WHERE id=?1",
            [id],|r|Ok(CoordinatorCheckpoint{
                id:r.get(0)?,session_id:r.get(1)?,kind:r.get(2)?,snapshot_id:r.get(3)?,from_seq:r.get(4)?,through_seq:r.get(5)?,
                manifest_hash:r.get(6)?,full_chars:r.get(7)?,delta_chars:r.get(8)?,created_unix_ms:r.get(9)?,acked:r.get::<_,i64>(10)?==1,
            })
        ).optional()?)
    }
    pub fn insert_coordinator_checkpoint(&mut self,row:&CoordinatorCheckpoint)->Result<()> {
        if row.id.is_empty()||row.id.len()>128||row.kind!="full"&&row.kind!="delta"||row.manifest_hash.len()!=64 {
            return Err(invalid("invalid coordinator checkpoint"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        tx.execute(
            "INSERT INTO coordinator_checkpoints VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![row.id,row.session_id,row.kind,row.snapshot_id,integer(row.from_seq)?,integer(row.through_seq)?,row.manifest_hash,integer(row.full_chars)?,integer(row.delta_chars)?,row.created_unix_ms,if row.acked {1}else{0}]
        )?;
        tx.execute("UPDATE coordinator_sessions SET last_checkpoint_id=?2 WHERE id=?1",params![row.session_id,row.id])?;
        tx.commit()?;Ok(())
    }
    pub fn ack_coordinator_checkpoint(&mut self,id:&str)->Result<CoordinatorCheckpoint> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let row:Option<(String,String,u64,i64)>=tx.query_row(
            "SELECT session_id,kind,through_seq,acked FROM coordinator_checkpoints WHERE id=?1",[id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
        ).optional()?;
        let Some((session,_,_,acked))=row else {return Err(invalid("checkpoint missing"));};
        if acked==1 {
            // idempotent ack
        } else {
            tx.execute("UPDATE coordinator_checkpoints SET acked=1 WHERE id=?1 AND acked=0",[id])?;
            if tx.changes()!=1 {return Err(invalid("checkpoint ack conflict"));}
        }
        let through:u64=tx.query_row("SELECT through_seq FROM coordinator_checkpoints WHERE id=?1",[id],|r|r.get(0))?;
        tx.execute("UPDATE coordinator_sessions SET last_checkpoint_id=?2,cursor_seq=?3 WHERE id=?1",params![session,id,integer(through)?])?;
        tx.commit()?;
        self.coordinator_checkpoint(id)?.ok_or_else(||invalid("checkpoint missing after ack"))
    }
    pub fn last_checkpoint_sizes(&mut self)->Result<Option<CheckpointSizes>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(tx.query_row(
            "SELECT id,full_chars,delta_chars,created_unix_ms FROM coordinator_checkpoints ORDER BY created_unix_ms DESC,id DESC LIMIT 1",
            [],|r|Ok(CheckpointSizes{checkpoint_id:r.get(0)?,full_chars:r.get(1)?,delta_chars:r.get(2)?,created_unix_ms:r.get(3)?})
        ).optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema19_upgrade_adds_empty_checkpoint_tables() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; UPDATE store_meta SET schema_version=19; PRAGMA user_version=19;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=22;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.last_checkpoint_sizes().unwrap().is_none());
        db.integrity_check().unwrap();
    }
    #[test]
    fn schema16_upgrade_reaches_schema20_without_inventing_checkpoints() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; DROP TABLE memory_subscriptions; DROP TABLE snapshot_entries; DROP TABLE memory_snapshots; DROP TABLE memory_validity; DROP TABLE memory_dependencies; DROP TABLE memory_heads; DROP TABLE memory_revisions; DROP TABLE memory_records; DROP TABLE objects; DROP TABLE authority_denials; DROP TABLE memory_policies; UPDATE store_meta SET schema_version=16; PRAGMA user_version=16;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=22;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.last_checkpoint_sizes().unwrap().is_none());
        db.integrity_check().unwrap();
    }
}
