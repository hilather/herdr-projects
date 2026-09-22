//! Immutable memory revisions. Import may insert stale/unverified_import rows.
use super::*;
use crate::domain::{Applicability, MemoryRecord, MemoryRevision, MemoryHead, MemoryValidity, MemoryRecordId, MemoryPolicyOp, ObjectId, parse_kind};

fn invalid(s:&str)->StoreError {StoreError::Invalid(s.into())}
fn schema(db:&Connection)->Result<()> {objects::schema(db)}
fn decode_applicability(raw:&str)->Result<Applicability> {
    let value:Applicability=serde_json::from_str(raw).map_err(|_|StoreError::Corrupt("invalid applicability".into()))?;
    value.validate().map_err(StoreError::Corrupt)?;Ok(value)
}
pub(super) fn record(db:&Connection,id:&str)->Result<Option<MemoryRecord>> {
    let mut stmt=db.prepare("SELECT id,record_key,scope_id,kind,is_hard FROM memory_records WHERE id=?1")?;
    let mut rows=stmt.query([id])?;
    let Some(row)=rows.next()? else {return Ok(None)};
    Ok(Some(MemoryRecord{id:MemoryRecordId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,record_key:row.get(1)?,scope_id:row.get(2)?,kind:parse_kind(&row.get::<_,String>(3)?).map_err(StoreError::Corrupt)?,is_hard:row.get::<_,i64>(4)?==1}))
}
fn insert_event(db:&Connection,kind:&str,entity:&str,revision:u64,payload:&str)->Result<u64> {
    db.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES(?1,?2,?3,1,?4)",params![kind,entity,integer(revision)?,payload])?;
    db.query_row("SELECT sequence FROM events WHERE rowid=last_insert_rowid()",[],|r|r.get(0)).map_err(Into::into)
}
fn load_active_facts(db:&Connection,now:i64)->Result<Vec<crate::domain::ActiveFact>> {
    let mut stmt=db.prepare("SELECT r.id,r.record_key,r.scope_id,r.kind,r.is_hard,v.revision,v.body_hash,v.provenance_hash,v.promoted_seq,v.applicability,q.state,q.reason,q.expiry_unix_ms,q.evaluated_seq FROM memory_records r JOIN memory_heads h ON h.record_id=r.id JOIN memory_revisions v ON v.record_id=r.id AND v.revision=h.revision JOIN memory_validity q ON q.record_id=r.id AND q.revision=h.revision JOIN objects b ON b.hash=v.body_hash WHERE h.status='active' AND q.state='valid' AND b.availability='available' ORDER BY r.id")?;
    let mut rows=stmt.query([])?;
    let mut facts=Vec::new();
    while let Some(row)=rows.next()? {
        let expiry:Option<i64>=row.get(12)?;
        if expiry.is_some_and(|e|now>=e) {continue;}
        if !super::memory_invalidation::dependencies_current(db,&row.get::<_,String>(0)?,row.get(5)?,now)? {continue;}
        facts.push(crate::domain::ActiveFact{
            record:MemoryRecord{id:MemoryRecordId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,record_key:row.get(1)?,scope_id:row.get(2)?,kind:parse_kind(&row.get::<_,String>(3)?).map_err(StoreError::Corrupt)?,is_hard:row.get::<_,i64>(4)?==1},
            revision:MemoryRevision{record_id:MemoryRecordId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,revision:row.get(5)?,body_hash:ObjectId::from_hex(row.get::<_,String>(6)?).map_err(StoreError::Corrupt)?,provenance_hash:ObjectId::from_hex(row.get::<_,String>(7)?).map_err(StoreError::Corrupt)?,promoted_seq:row.get(8)?,applicability:decode_applicability(&row.get::<_,String>(9)?)?},
            validity:MemoryValidity{record_id:MemoryRecordId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,revision:row.get(5)?,state:row.get(10)?,reason:row.get(11)?,expiry_unix_ms:expiry,evaluated_seq:row.get(13)?},
        });
    }
    Ok(facts)
}
fn heads_digest(db:&Connection)->Result<String> {
    let mut stmt=db.prepare("SELECT record_id,revision,status,row_revision FROM memory_heads ORDER BY record_id")?;
    let mut rows=stmt.query([])?;
    let mut items=Vec::new();
    while let Some(row)=rows.next()? {
        items.push(serde_json::json!([row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,String>(2)?,row.get::<_,u64>(3)?]));
    }
    Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&items).map_err(|e|StoreError::Invalid(e.to_string()))?)))
}
fn hops_from_pins(db:&Connection,pins:&[String])->Result<std::collections::BTreeMap<String,u32>> {
    let mut edges:std::collections::BTreeMap<String,Vec<String>>=std::collections::BTreeMap::new();
    let mut stmt=db.prepare("SELECT d.derived_record,d.source_record FROM memory_dependencies d JOIN memory_heads a ON a.record_id=d.derived_record AND a.revision=d.derived_revision AND a.status='active' JOIN memory_heads b ON b.record_id=d.source_record AND b.revision=d.source_revision AND b.status='active'")?;
    let mut rows=stmt.query([])?;
    while let Some(row)=rows.next()? {
        let a:String=row.get(0)?;let b:String=row.get(1)?;
        edges.entry(a).or_default().push(b);
    }
    let mut dist=std::collections::BTreeMap::new();
    let mut queue=std::collections::VecDeque::new();
    for pin in pins { dist.insert(pin.clone(),0); queue.push_back(pin.clone()); }
    while let Some(id)=queue.pop_front() {
        let d=*dist.get(&id).unwrap();
        if d>=4 {continue;}
        for next in edges.get(&id).into_iter().flatten() {
            if dist.contains_key(next) {continue;}
            dist.insert(next.clone(),d+1);
            queue.push_back(next.clone());
        }
    }
    Ok(dist)
}
fn entry_bytes(db:&Connection,fact:&crate::domain::ActiveFact,role:&str)->Result<u64> {
    let size:u64=db.query_row("SELECT size FROM objects WHERE hash=?1",[fact.revision.body_hash.as_str()],|r|r.get(0))?;
    // UTF-8 byte size is a conservative upper bound for character budgets.
    Ok(size +
    serde_json::json!({"id":fact.record.id.as_str(),"key":fact.record.record_key,"revision":fact.revision.revision,"kind":fact.record.kind.as_str(),"body":fact.revision.body_hash.as_str(),"role":role}).to_string().len() as u64)
}
pub(crate) fn apply_memory_revision_in_tx(tx:&rusqlite::Transaction,next:&crate::domain::NewRevision)->Result<(MemoryHead,u64)> {
    next.applicability.validate().map_err(|s|invalid(&s))?;
    if next.record_key.is_empty()||next.record_key.len()>512||next.record_key.chars().any(char::is_control) {return Err(invalid("invalid memory record_key"));}
    if next.scope_id.is_empty()||next.scope_id.len()>128 {return Err(invalid("invalid memory scope"));}
    objects::require_available(tx,next.body_hash.as_str()).map_err(|_|invalid("missing object"))?;
    objects::require_available(tx,next.provenance_hash.as_str()).map_err(|_|invalid("missing object"))?;
    objects::cancel_pending(tx,next.body_hash.as_str())?;objects::cancel_pending(tx,next.provenance_hash.as_str())?;
    let existing=record(tx,next.id.as_str())?;
    if let Some(old)=&existing {
        if old.record_key!=next.record_key||old.scope_id!=next.scope_id||old.kind!=next.kind {return Err(invalid("memory record identity mismatch"));}
    } else {
        tx.execute("INSERT INTO memory_records VALUES(?1,?2,?3,?4,0)",params![next.id.as_str(),next.record_key,next.scope_id,next.kind.as_str()])?;
    }
    let current:Option<(u64,String,u64)>=tx.query_row("SELECT revision,status,row_revision FROM memory_heads WHERE record_id=?1",[next.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    match (next.expected, current.as_ref()) {
        (None, None)=>{},
        (Some(rev), Some((head,status,_))) if *head==rev && status=="active"=>{},
        _=>return Err(StoreError::Conflict),
    }
    let revision=current.as_ref().map(|(h,_,_)|*h).unwrap_or(0).checked_add(1).ok_or_else(||invalid("memory revision exhausted"))?;
    let applicability=serde_json::to_string(&next.applicability).map_err(|_|invalid("applicability encoding failed"))?;
    let payload=serde_json::to_string(&serde_json::json!({"id":next.id.as_str(),"revision":revision,"body":next.body_hash.as_str(),"provenance":next.provenance_hash.as_str()})).map_err(|_|invalid("memory event encoding failed"))?;
    let seq=insert_event(tx,"memory.revision_inserted",next.id.as_str(),revision,&payload)?;
    tx.execute("INSERT INTO memory_revisions VALUES(?1,?2,?3,?4,?5,?6)",params![next.id.as_str(),integer(revision)?,next.body_hash.as_str(),next.provenance_hash.as_str(),integer(seq)?,applicability])?;
    let state=if next.validity_state.is_empty(){"valid"}else{next.validity_state.as_str()};
    let reason=if next.validity_reason.is_empty(){"control_insert"}else{next.validity_reason.as_str()};
    if !matches!(state,"valid"|"stale"|"blocked") {return Err(invalid("invalid validity state"));}
    if reason.is_empty()||reason.len()>64||reason.chars().any(char::is_control) {return Err(invalid("invalid validity reason"));}
    tx.execute("INSERT INTO memory_validity VALUES(?1,?2,?3,?4,?5,?6)",params![next.id.as_str(),integer(revision)?,state,reason,next.expiry_unix_ms,integer(seq)?])?;
    for (source,source_rev,kind) in &next.dependencies {
        if kind.is_empty()||kind.len()>64 {return Err(invalid("invalid memory dependency kind"));}
        tx.execute("INSERT INTO memory_dependencies VALUES(?1,?2,?3,?4,?5)",params![next.id.as_str(),integer(revision)?,source.as_str(),integer(*source_rev)?,kind])?;
    }
    let row_revision=current.as_ref().map(|(_,_,r)|*r).unwrap_or(0).checked_add(1).ok_or_else(||invalid("memory head row exhausted"))?;
    if current.is_some() {
        tx.execute("UPDATE memory_heads SET revision=?2,status='active',row_revision=?3 WHERE record_id=?1",params![next.id.as_str(),integer(revision)?,integer(row_revision)?])?;
    } else {
        tx.execute("INSERT INTO memory_heads VALUES(?1,?2,'active',?3)",params![next.id.as_str(),integer(revision)?,integer(row_revision)?])?;
    }
    if let Some((previous,_,_))=&current {
        super::memory_invalidation::dependents(tx,next.id.as_str(),*previous,seq)?;
    }
    Ok((MemoryHead{record_id:next.id.clone(),revision,status:"active".into(),row_revision},seq))
}
impl SqliteStore {
    pub(crate) fn insert_memory_revision(&mut self,next:&crate::domain::NewRevision)->Result<(MemoryHead,u64)> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let result=apply_memory_revision_in_tx(&tx,next)?;
        tx.commit()?;Ok(result)
    }
    #[cfg(test)]
    pub(crate) fn revoke_memory(&mut self,id:&MemoryRecordId,expected:u64,now:i64)->Result<()> {
        let _=now;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        let current:(u64,String,u64)=tx.query_row("SELECT revision,status,row_revision FROM memory_heads WHERE record_id=?1",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or(StoreError::Conflict)?;
        if current.0!=expected||current.1!="active" {return Err(StoreError::Conflict);}
        let row=current.2.checked_add(1).ok_or_else(||invalid("memory head row exhausted"))?;
        tx.execute("UPDATE memory_heads SET status='revoked',row_revision=?2 WHERE record_id=?1",params![id.as_str(),integer(row)?])?;
        tx.execute("UPDATE memory_validity SET state='blocked',reason='revoked' WHERE record_id=?1 AND revision=?2",params![id.as_str(),integer(expected)?])?;
        let seq=insert_event(&tx,"memory.revoked",id.as_str(),expected,"{}")?;
        super::memory_invalidation::changed(&tx,id.as_str(),expected,seq,"revoke")?;
        tx.commit()?;Ok(())
    }
    #[cfg(test)]
    pub(crate) fn apply_memory_op(&mut self,op:MemoryPolicyOp,record_key:&str,expected_head:u64)->Result<()> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;schema(&tx)?;
        if head(&tx)?!=expected_head {return Err(StoreError::Conflict);}
        apply_memory_op_in_tx(&tx,op,record_key)?;
        insert_event(&tx,"memory.policy_applied",record_key,1,&serde_json::to_string(&op).map_err(|_|invalid("invalid memory policy"))?)?;
        tx.commit()?;Ok(())
    }
    pub fn active_facts(&mut self,now:i64)->Result<Vec<crate::domain::ActiveFact>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let facts=load_active_facts(&tx,now)?;tx.commit()?;Ok(facts)
    }
    pub fn create_memory_snapshot(&mut self,plan:crate::domain::SnapshotPlan)->Result<crate::domain::MemorySnapshot> {
        use crate::domain::{selection_score, SELECTION_POLICY_VERSION, SnapshotId};
        if plan.profile_digest.len()!=64 {return Err(invalid("invalid profile digest"));}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if version<23 {return Err(StoreError::UnsupportedSchema(version));}
        if plan.instructions.len()>MAX_RECORD_BYTES {return Err(invalid("snapshot instructions exceed 1 MiB"));}
        let sequence=head(&tx)?;
        if sequence==0 {return Err(invalid("memory snapshot requires a committed event head"));}
        let control=super::control::read(&tx)?;
        let (task_id,task_revision,subscriber)=if plan.coordinator {
            ("coordinator".into(),control.revision,format!("coordinator:{}",plan.session_id.as_deref().unwrap_or("session")))
        } else {
            let id=plan.request.task_id.clone();
            let revision:u64=tx.query_row("SELECT revision FROM tasks WHERE id=?1",[id.as_str()],|r|r.get(0)).optional()?.ok_or_else(||invalid("snapshot task is missing"))?;
            (id.clone(),revision,format!("task:{id}"))
        };
        let digest=heads_digest(&tx)?;
        if plan.expected_heads_digest.as_ref().is_some_and(|expected| expected!=&digest) {return Err(StoreError::Conflict);}
        let facts=load_active_facts(&tx,plan.now_unix_ms)?;
        let required_ids={
            let mut stmt=tx.prepare("SELECT r.id FROM memory_records r JOIN memory_heads h ON h.record_id=r.id WHERE h.status='active' AND (r.is_hard=1 OR r.kind IN ('constraint','hard_memory')) ORDER BY r.id LIMIT 10001")?;
            stmt.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?
        };
        if required_ids.len()>10000 {return Err(StoreError::Limit("mandatory memory inventory exceeds 10000".into()));}
        for id in required_ids {
            if !facts.iter().any(|f|f.record.id.as_str()==id) {return Err(invalid("mandatory memory is invalid, expired, unavailable or has a stale dependency"));}
        }
        let by_key:std::collections::BTreeMap<_,_>=facts.iter().map(|f|(&f.record.record_key,f)).collect();
        for key in &plan.request.pinned_keys {
            let Some(fact)=by_key.get(key) else {return Err(invalid("required pinned memory is missing or not an active fact"));};
            if fact.validity.reason=="unverified_import" {return Err(invalid("required pinned memory is unverified_import"));}
            if fact.record.kind == crate::domain::MemoryKind::TaskLocal && fact.record.scope_id != format!("task:{}", plan.request.task_id) {
                return Err(invalid("pinned task-local memory belongs to another task"));
            }
        }
        let mut mandatory=Vec::new();
        let mut seen=std::collections::BTreeSet::new();
        for fact in &facts {
            let pinned=plan.request.pinned_keys.iter().any(|k|k==&fact.record.record_key);
            if matches!(fact.record.kind,crate::domain::MemoryKind::Constraint|crate::domain::MemoryKind::HardMemory) || fact.record.is_hard || (!plan.coordinator && pinned) {
                if seen.insert(fact.record.id.as_str().to_string()) { mandatory.push(fact); }
            }
        }
        let task_text:String=if plan.coordinator {"Coordinate the project using this checkpoint.".into()} else {
            tx.query_row("SELECT title FROM tasks WHERE id=?1",[task_id.as_str()],|r|r.get(0))?
        };
        let task_hash=format!("{:x}",Sha256::digest(task_text.as_bytes()));
        let request_json=serde_json::to_string(&plan.request).map_err(|e|invalid(&e.to_string()))?;
        let mut required=plan.instructions.chars().count() as u64 + request_json.chars().count() as u64 + task_text.chars().count() as u64 + 128;
        if plan.estimator == crate::memory::WORKER_BRIEF_ESTIMATOR {
            if plan.coordinator {return Err(invalid("worker brief estimator cannot select coordinator knowledge"));}
            required=required.saturating_add(crate::memory::worker_brief_framing_chars(tx.path().ok_or_else(||invalid("worker snapshot store path missing"))?).map_err(|_|invalid("worker brief framing unavailable"))?);
        }
        let mut entries=Vec::new();
        for fact in &mandatory {
            required=required.saturating_add(entry_bytes(&tx,fact,"mandatory")?);
            entries.push(((*fact).clone(),"mandatory", if fact.record.is_hard {"hard"} else if fact.record.kind==crate::domain::MemoryKind::Constraint {"constraint"} else {"pinned"}));
        }
        if required>plan.budget_chars {
            return Err(StoreError::Limit(format!("required {required} budget {}",plan.budget_chars)));
        }
        let mut optional_bytes=0u64;
        let mut omitted=0u64;
        if !plan.coordinator {
            let dist=hops_from_pins(&tx,&plan.request.pinned_keys.iter().filter_map(|k| by_key.get(k).map(|f|f.record.id.as_str().to_string())).collect::<Vec<_>>())?;
            let mut optional:Vec<_>=facts.iter().filter(|f| {
                if seen.contains(f.record.id.as_str()) || matches!(f.record.kind,crate::domain::MemoryKind::Constraint|crate::domain::MemoryKind::HardMemory) {return false;}
                let hops=dist.get(f.record.id.as_str()).copied();
                if f.record.kind == crate::domain::MemoryKind::TaskLocal && f.record.scope_id != format!("task:{}", plan.request.task_id) { return false; }
                hops.is_some() || crate::domain::scope_matches(&plan.request, &f.revision.applicability)
            }).cloned().collect();
            optional.sort_by(|a,b|{
                let sa=selection_score(&plan.request,a,dist.get(a.record.id.as_str()).copied());
                let sb=selection_score(&plan.request,b,dist.get(b.record.id.as_str()).copied());
                sb.cmp(&sa).then_with(|| a.record.id.as_str().cmp(b.record.id.as_str()))
            });
            let mut remaining=plan.budget_chars.saturating_sub(required);
            for fact in optional {
                let size=entry_bytes(&tx,&fact,"optional")?;
                if size>remaining { omitted+=1; continue; }
                remaining-=size; optional_bytes+=size;
                entries.push((fact,"optional","ranked"));
            }
        }
        let digest_after=heads_digest(&tx)?;
        let sequence_after=head(&tx)?;
        let control_after=super::control::read(&tx)?;
        if sequence_after!=sequence || digest_after!=digest || control_after.revision!=control.revision {return Err(StoreError::Conflict);}
        if !plan.coordinator {
            let revision_after:u64=tx.query_row("SELECT revision FROM tasks WHERE id=?1",[task_id.as_str()],|r|r.get(0))?;
            if revision_after!=task_revision {return Err(StoreError::Conflict);}
        } else if control_after.config_digest!=control.config_digest {return Err(StoreError::Conflict);}
        let scope=serde_json::to_vec(&plan.request).map_err(|e|invalid(&e.to_string()))?;
        let scope_digest=format!("{:x}",Sha256::digest(&scope));
        let manifest=entries.iter().map(|(f,role,reason)| serde_json::json!([f.record.id.as_str(),f.revision.revision,role,reason])).collect::<Vec<_>>();
        let manifest_hash=format!("{:x}",Sha256::digest(serde_json::to_vec(&manifest).map_err(|e|invalid(&e.to_string()))?));
        let instruction_digest=format!("{:x}",Sha256::digest(plan.instructions.as_bytes()));
        let cache=serde_json::json!(["retained-inputs-v1",task_id,task_revision,plan.config_digest,digest,SELECTION_POLICY_VERSION,plan.profile_digest,scope_digest,instruction_digest,manifest_hash,plan.budget_chars,plan.estimator,subscriber,sequence]);
        let id=SnapshotId::new(format!("snap-{:x}",Sha256::digest(serde_json::to_vec(&cache).map_err(|e|invalid(&e.to_string()))?))).map_err(|s|invalid(&s))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_snapshots WHERE id=?1)",[id.as_str()],|r|r.get::<_,bool>(0))? {
            let existing=id.as_str().to_string();
            tx.commit()?;
            return self.read_memory_snapshot(&existing);
        }
        tx.execute("INSERT INTO memory_snapshots VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![
            id.as_str(),task_id,integer(task_revision)?,plan.profile_name,plan.profile_digest,plan.config_digest,
            SELECTION_POLICY_VERSION as i64,plan.estimator,integer(sequence)?,integer(required)?,integer(optional_bytes)?,
            integer(plan.budget_chars)?,integer(omitted)?,manifest_hash,scope_digest
        ])?;
        tx.execute("INSERT INTO memory_snapshot_inputs VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id.as_str(),plan.instructions,instruction_digest,task_text,task_hash,request_json,scope_digest])?;
        for (i,(fact,role,reason)) in entries.iter().enumerate() {
            tx.execute("INSERT INTO snapshot_entries VALUES(?1,?2,?3,?4,?5,?6)",params![id.as_str(),integer((i as u64)+1)?,fact.record.id.as_str(),integer(fact.revision.revision)?,role,reason])?;
        }
        let sub=format!("sub-{:x}",Sha256::digest(format!("{subscriber}\0{}",id.as_str()).as_bytes()));
        tx.execute("INSERT INTO memory_subscriptions VALUES(?1,?2,?3,?4)",params![sub,subscriber,id.as_str(),integer(sequence)?])?;
        tx.commit()?;
        self.read_memory_snapshot(id.as_str())
    }
    pub fn read_memory_snapshot(&mut self,id:&str)->Result<crate::domain::MemorySnapshot> {
        let tx=self.connection.transaction()?;
        let row=tx.query_row("SELECT id,task_id,task_revision,profile_name,profile_digest,config_digest,selection_policy_version,estimator,sequence,required_bytes,optional_bytes,budget_bytes,omitted_optional_count,manifest_hash,scope_digest FROM memory_snapshots WHERE id=?1",[id],|r| Ok((
            r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,
            r.get::<_,Option<String>>(5)?,r.get::<_,u32>(6)?,r.get::<_,String>(7)?,r.get::<_,u64>(8)?,r.get::<_,u64>(9)?,
            r.get::<_,u64>(10)?,r.get::<_,u64>(11)?,r.get::<_,u64>(12)?,r.get::<_,String>(13)?,r.get::<_,String>(14)?,
        )))?;
        let mut stmt=tx.prepare("SELECT ordinal,record_id,revision,role,reason FROM snapshot_entries WHERE snapshot_id=?1 ORDER BY ordinal")?;
        let mut rows=stmt.query([id])?;
        let mut entries=Vec::new();
        while let Some(e)=rows.next()? {
            entries.push(crate::domain::SnapshotEntry{ordinal:e.get(0)?,record_id:MemoryRecordId::new(e.get::<_,String>(1)?).map_err(StoreError::Corrupt)?,revision:e.get(2)?,role:e.get(3)?,reason:e.get(4)?});
        }
        let (subscriber,since):(String,u64)=tx.query_row("SELECT subscriber,since_seq FROM memory_subscriptions WHERE snapshot_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        Ok(crate::domain::MemorySnapshot{
            id:crate::domain::SnapshotId::new(row.0).map_err(StoreError::Corrupt)?,task_id:row.1,task_revision:row.2,profile_name:row.3,
            profile_digest:row.4,config_digest:row.5,selection_policy_version:row.6,estimator:row.7,sequence:row.8,
            required_bytes:row.9,optional_bytes:row.10,budget_bytes:row.11,omitted_optional_count:row.12,manifest_hash:row.13,
            scope_digest:row.14,entries,subscriber,since_seq:since,
        })
    }
    pub fn memory_records(&mut self)->Result<Vec<MemoryRecord>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let mut stmt=tx.prepare("SELECT id,record_key,scope_id,kind,is_hard FROM memory_records ORDER BY id")?;
        let mut rows=stmt.query([])?;
        let mut result=Vec::new();
        while let Some(row)=rows.next()? {
            result.push(MemoryRecord{id:MemoryRecordId::new(row.get::<_,String>(0)?).map_err(StoreError::Corrupt)?,record_key:row.get(1)?,scope_id:row.get(2)?,kind:parse_kind(&row.get::<_,String>(3)?).map_err(StoreError::Corrupt)?,is_hard:row.get::<_,i64>(4)?==1});
        }
        Ok(result)
    }
    pub fn memory_heads_digest(&mut self)->Result<String> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let digest=heads_digest(&tx)?;tx.commit()?;Ok(digest)
    }
    pub(crate) fn shadow_import_replaceable(&mut self,id:&str)->Result<bool> {
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_records r JOIN memory_heads h ON h.record_id=r.id JOIN memory_validity v ON v.record_id=h.record_id AND v.revision=h.revision WHERE r.id=?1 AND r.is_hard=0 AND h.status='active' AND v.state='stale' AND v.reason='unverified_import')",[id],|r|r.get(0))?)
    }
    pub fn memory_record(&mut self,id:&str)->Result<Option<MemoryRecord>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        Ok(record(&tx,id)?)
    }
    pub fn memory_record_by_key(&mut self,key:&str)->Result<Option<MemoryRecord>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let row:Option<(String,String,String,String,i64)>=tx.query_row("SELECT id,record_key,scope_id,kind,is_hard FROM memory_records WHERE record_key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        Ok(match row {
            Some((id,record_key,scope_id,kind,is_hard))=>Some(MemoryRecord{id:MemoryRecordId::new(id).map_err(StoreError::Corrupt)?,record_key,scope_id,kind:parse_kind(&kind).map_err(StoreError::Corrupt)?,is_hard:is_hard==1}),
            None=>None,
        })
    }
    pub fn memory_head(&mut self,id:&str)->Result<Option<MemoryHead>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let row:Option<(String,u64,String,u64)>=tx.query_row("SELECT record_id,revision,status,row_revision FROM memory_heads WHERE record_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        Ok(match row {
            Some((record_id,revision,status,row_revision))=>Some(MemoryHead{record_id:MemoryRecordId::new(record_id).map_err(StoreError::Corrupt)?,revision,status,row_revision}),
            None=>None,
        })
    }
    pub fn memory_revision(&mut self,id:&str,revision:u64)->Result<Option<MemoryRevision>> {
        let tx=self.connection.transaction()?;schema(&tx)?;
        let row:Option<(String,u64,String,String,u64,String)>=tx.query_row("SELECT record_id,revision,body_hash,provenance_hash,promoted_seq,applicability FROM memory_revisions WHERE record_id=?1 AND revision=?2",params![id,integer(revision)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        Ok(match row {
            Some((record_id,revision,body,prov,promoted_seq,applicability))=>Some(MemoryRevision{
                record_id:MemoryRecordId::new(record_id).map_err(StoreError::Corrupt)?,revision,
                body_hash:ObjectId::from_hex(body).map_err(StoreError::Corrupt)?,
                provenance_hash:ObjectId::from_hex(prov).map_err(StoreError::Corrupt)?,
                promoted_seq,applicability:decode_applicability(&applicability)?,
            }),
            None=>None,
        })
    }

}
pub(crate) fn apply_memory_op_in_tx(tx:&rusqlite::Transaction,op:MemoryPolicyOp,record_key:&str)->Result<()> {
    let id:String=tx.query_row("SELECT id FROM memory_records WHERE record_key=?1",[record_key],|r|r.get(0)).optional()?.ok_or_else(||invalid("memory record missing"))?;
    let revision:u64=tx.query_row("SELECT revision FROM memory_heads WHERE record_id=?1 AND status='active'",[id.as_str()],|r|r.get(0)).optional()?.ok_or_else(||invalid("memory head missing"))?;
    match op {
        MemoryPolicyOp::ImportAck=>{ tx.execute("UPDATE memory_validity SET state='valid',reason='import_acked' WHERE record_id=?1 AND revision=?2",params![id.as_str(),integer(revision)?])?; }
        MemoryPolicyOp::HardRule=>{
            tx.execute("UPDATE memory_records SET is_hard=1 WHERE id=?1",[id.as_str()])?;
            tx.execute("UPDATE memory_validity SET state='valid',reason='control_hard_rule' WHERE record_id=?1 AND revision=?2",params![id.as_str(),integer(revision)?])?;
        }
        MemoryPolicyOp::RevokeHead=>{
            tx.execute("UPDATE memory_heads SET status='revoked',row_revision=row_revision+1 WHERE record_id=?1",[id.as_str()])?;
            tx.execute("UPDATE memory_validity SET state='blocked',reason='revoked' WHERE record_id=?1 AND revision=?2",params![id.as_str(),integer(revision)?])?;
        }
        MemoryPolicyOp::Cutover=>return Err(invalid("cutover is not applied in the memory store")),
    }
    let seq=insert_event(tx,"memory.record_policy_changed",&id,revision,&serde_json::to_string(&op).map_err(|_|invalid("invalid policy"))?)?;
    if matches!(op,MemoryPolicyOp::RevokeHead) {super::memory_invalidation::changed(tx,&id,revision,seq,"policy")?;}
    else {super::memory_delivery::record_change(tx,&format!("policy:{seq}"),&id,revision,"stop_at_checkpoint",seq)?;}
    Ok(())
}
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema17_upgrade_adds_empty_memory_tables() {
        let temp=tempfile::tempdir().unwrap();let path=temp.path().join("state.db");
        let mut db=SqliteStore::create(&path).unwrap();
        db.connection.execute_batch("DROP TABLE IF EXISTS native_profiles; DROP TABLE IF EXISTS memory_update_receipts; DROP TABLE IF EXISTS memory_delivery_intents; DROP TABLE IF EXISTS memory_import_decisions; DROP TABLE IF EXISTS memory_import_candidates; DROP TABLE IF EXISTS memory_snapshot_inputs; DROP TABLE memory_invalidations; DROP TABLE memory_promotions; DROP TABLE review_decisions; DROP TABLE proposal_validations; DROP TABLE memory_proposals; DROP TABLE coordinator_checkpoints; DROP TABLE coordinator_sessions; DROP TABLE memory_subscriptions; DROP TABLE snapshot_entries; DROP TABLE memory_snapshots; DROP TABLE memory_validity; DROP TABLE memory_dependencies; DROP TABLE memory_heads; DROP TABLE memory_revisions; DROP TABLE memory_records; DROP TABLE objects; UPDATE store_meta SET schema_version=17; PRAGMA user_version=17;").unwrap();
        let mut before=db.read_snapshot(None).unwrap();db.upgrade_v1().unwrap();before.schema_version=25;
        assert_eq!(db.read_snapshot(None).unwrap(),before);
        assert!(db.memory_records().unwrap().is_empty());
        db.integrity_check().unwrap();
    }
}
