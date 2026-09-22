//! Sequential signed memory policies. Non-cutover ops apply to memory rows in the same transaction.
use super::*;

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {
    check_schema(db)?;let n:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if n<17 {return Err(StoreError::UnsupportedSchema(n));}Ok(())
}
pub(super) fn read_all(db:&Connection)->Result<Vec<MemoryPolicy>> {read_all_with_budget(db,None)}
pub(super) fn read_all_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<MemoryPolicy>> {
    let mut statement=db.prepare("SELECT revision,payload,payload_hash FROM memory_policies ORDER BY revision")?;
    let mut rows=statement.query([])?;
    let mut policies=Vec::new();
    while let Some(row)=rows.next()? {
        if let Some(budget)=budget {budget.row(row,&[(1,1)])?;}
        let revision:u64=row.get(0)?;
        let payload:String=row.get(1)?;
        let digest:String=row.get(2)?;
        if policies.len()>=10_000 || payload.len()>MAX_RECORD_BYTES || format!("{:x}",Sha256::digest(payload.as_bytes()))!=digest {
            return Err(StoreError::Corrupt("memory policy history exceeds bounds or hash mismatch".into()));
        }
        let policy:MemoryPolicy=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid memory policy record".into()))?;
        let reference=policy.reference().map_err(StoreError::Corrupt)?;
        if revision!=policies.len() as u64+1 || policy.revision!=revision || reference.digest!=digest {
            return Err(StoreError::Corrupt("memory policy revision or identity mismatch".into()));
        }
        policies.push(policy);
    }
    Ok(policies)
}
pub(super) fn read_denials(db:&Connection)->Result<Vec<AuthorityDenial>> {
    let mut statement=db.prepare("SELECT id,unix_ms,class,command,actor_channel,reason_code,policy_digest,expected_head,actual_head FROM authority_denials ORDER BY unix_ms,id")?;
    let mut rows=statement.query([])?;
    let mut result=Vec::new();
    while let Some(row)=rows.next()? {
        if result.len()>=10_000 {return Err(StoreError::Corrupt("authority denial inventory exceeds bounds".into()));}
        result.push(AuthorityDenial{
            id:row.get(0)?,unix_ms:row.get(1)?,class:row.get(2)?,command:row.get(3)?,
            actor_channel:row.get(4)?,reason_code:row.get(5)?,policy_digest:row.get(6)?,
            expected_head:row.get::<_,Option<i64>>(7)?.map(|n|n as u64),
            actual_head:row.get::<_,Option<i64>>(8)?.map(|n|n as u64),
        });
    }
    Ok(result)
}
impl SqliteStore {
    pub fn install_memory_policy(&mut self,prepared:&PreparedMemoryPolicy,expected_head:u64)->Result<VersionedReference> {
        let policy=&prepared.policy;
        let reference=policy.reference().map_err(|s|invalid(&s))?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        if policy.expected_head!=expected_head {return Err(invalid("memory policy expected_head does not match store head"));}
        let path=tx.path().ok_or_else(||invalid("memory policy requires file-backed store"))?;
        let path=std::fs::canonicalize(path).map_err(|_|invalid("memory policy store unavailable"))?;
        if path.to_str()!=Some(policy.project_store.as_str()) {return Err(invalid("memory policy belongs to a different project"));}
        let history=read_all(&tx)?;
        if history.len()>=10_000 {return Err(invalid("memory policy history limit reached"));}
        if policy.revision!=history.len() as u64+1 {return Err(StoreError::Conflict);}
        let payload=serde_json::to_string(policy).map_err(|_|invalid("memory policy encoding failed"))?;
        tx.execute("INSERT INTO memory_policies VALUES(?1,?2,?3)",params![integer(policy.revision)?,payload,reference.digest])?;
        tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('memory.policy_changed','project-memory-policy',?1,1,?2)",params![integer(policy.revision)?,payload])?;
        if !matches!(policy.op,MemoryPolicyOp::Cutover) {
            let key=policy.record_key.as_deref().ok_or_else(||invalid("memory policy requires record_key"))?;
            super::memory::apply_memory_op_in_tx(&tx,policy.op,key)?;
        }
        tx.commit()?;Ok(reference)
    }
    pub fn insert_denial(&mut self,denial:&AuthorityDenial)->Result<()> {
        if denial.id.is_empty()||denial.id.len()>128||denial.reason_code.is_empty()||denial.reason_code.len()>64
            ||denial.class.len()>32||denial.command.len()>64||denial.policy_digest.len()!=64
            ||!denial.policy_digest.bytes().all(|b|b.is_ascii_hexdigit())
            ||!["cli-owner","unknown-rejected"].contains(&denial.actor_channel.as_str())
            ||!["approval","budget","routine-store","memory"].contains(&denial.class.as_str()) {
            return Err(invalid("invalid authority denial"));
        }
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        tx.execute("INSERT INTO authority_denials VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![
            denial.id,denial.unix_ms,denial.class,denial.command,denial.actor_channel,denial.reason_code,denial.policy_digest,
            denial.expected_head.map(integer).transpose()?,denial.actual_head.map(integer).transpose()?
        ])?;
        tx.commit()?;Ok(())
    }
    pub fn authority_denials(&mut self)->Result<Vec<AuthorityDenial>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let result=read_denials(&tx)?;tx.commit()?;Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy(path:&Path,revision:u64,op:MemoryPolicyOp)->MemoryPolicy {
        MemoryPolicy{version:1,revision,project_store:path.canonicalize().unwrap().display().to_string(),
            authority:VersionedReference{id:"owner-approval-policy".into(),revision:1,digest:"a".repeat(64)},
            expected_head:0,op,record_key:if matches!(op,MemoryPolicyOp::Cutover){None}else{Some("memory/api.md".into())},
            memory_plan_digest:if matches!(op,MemoryPolicyOp::Cutover){Some("b".repeat(64))}else{None},
            expected_memory_owner:if matches!(op,MemoryPolicyOp::Cutover){Some("legacy-markdown".into())}else{None}}
    }
    #[test]
    fn memory_policy_installation_is_atomic_project_bound_and_revision_checked() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        let mut document=policy(&path,1,MemoryPolicyOp::HardRule);
        let before=db.read_snapshot(None).unwrap();
        let mut other=document.clone();other.project_store=temp.path().join("other.db").display().to_string();
        assert!(db.install_memory_policy(&PreparedMemoryPolicy{policy:other},before.head).is_err());
        assert!(db.install_memory_policy(&PreparedMemoryPolicy{policy:document.clone()},before.head+1).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        document.expected_head=before.head;
        db.connection.execute_batch("CREATE TRIGGER fail_memory BEFORE INSERT ON events WHEN NEW.kind='memory.policy_changed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(db.install_memory_policy(&PreparedMemoryPolicy{policy:document.clone()},before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        db.connection.execute_batch("DROP TRIGGER fail_memory;").unwrap();
        let body="ab".repeat(32);let prov="cd".repeat(32);
        db.upsert_object(&body,4).unwrap();db.upsert_object(&prov,4).unwrap();
        db.insert_memory_revision(&crate::domain::NewRevision{
            id:crate::domain::MemoryRecordId::new("api").unwrap(),record_key:"memory/api.md".into(),scope_id:"project".into(),
            kind:crate::domain::MemoryKind::Observation,body_hash:crate::domain::ObjectId::from_hex(body).unwrap(),
            provenance_hash:crate::domain::ObjectId::from_hex(prov).unwrap(),
            applicability:crate::domain::Applicability{domains:vec![],paths:vec!["memory/api.md".into()]},
            dependencies:vec![],expected:None,expiry_unix_ms:None,validity_state:"stale".into(),validity_reason:"unverified_import".into(),
        }).unwrap();
        let before=db.read_snapshot(None).unwrap();document.expected_head=before.head;
        db.install_memory_policy(&PreparedMemoryPolicy{policy:document.clone()},before.head).unwrap();
        let before=db.read_snapshot(None).unwrap();
        assert!(db.install_memory_policy(&PreparedMemoryPolicy{policy:document.clone()},before.head).is_err());
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        document.revision=2;document.op=MemoryPolicyOp::Cutover;document.record_key=None;document.memory_plan_digest=Some("c".repeat(64));document.expected_memory_owner=Some("legacy-markdown".into());document.expected_head=before.head;
        db.install_memory_policy(&PreparedMemoryPolicy{policy:document},before.head).unwrap();
        drop(db);let mut db=SqliteStore::open(&path).unwrap();
        assert_eq!(db.read_snapshot(None).unwrap().memory_policies.len(),2);
        assert!(db.connection.execute("DELETE FROM memory_policies",[]).is_err());
    }
    #[test]
    fn schema16_upgrade_adds_memory_policies_without_inventing_revisions() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE IF EXISTS native_profiles; DROP TABLE IF EXISTS memory_update_receipts; DROP TABLE IF EXISTS memory_delivery_intents; DROP TABLE IF EXISTS memory_import_decisions; DROP TABLE IF EXISTS memory_import_candidates; DROP TABLE IF EXISTS memory_snapshot_inputs; DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; DROP TABLE memory_subscriptions; DROP TABLE snapshot_entries; DROP TABLE memory_snapshots; DROP TABLE memory_validity; DROP TABLE memory_dependencies; DROP TABLE memory_heads; DROP TABLE memory_revisions; DROP TABLE memory_records; DROP TABLE objects; DROP TABLE authority_denials; DROP TABLE memory_policies; UPDATE store_meta SET schema_version=16; PRAGMA user_version=16;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=25;
        assert_eq!(db.read_snapshot(None).unwrap(),before);assert!(before.memory_policies.is_empty());
        db.integrity_check().unwrap();
    }
}
