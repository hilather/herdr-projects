use super::*;
use crate::reconcile::RuntimeObservation;

pub(super) fn read_all(db:&Connection)->Result<Vec<RuntimeObservation>> {read_all_with_budget(db,None)}
pub(super) fn read_all_with_budget(db:&Connection,budget:Option<&read_budget::ReadBudget>)->Result<Vec<RuntimeObservation>> {
    read_selected(db,None,budget)
}
pub(super) fn read_binding(db:&Connection,id:&str,budget:Option<&read_budget::ReadBudget>)->Result<Option<RuntimeObservation>> {
    Ok(read_selected(db,Some(id),budget)?.pop())
}
fn read_selected(db:&Connection,id:Option<&str>,budget:Option<&read_budget::ReadBudget>)->Result<Vec<RuntimeObservation>> {
    let mut stmt=db.prepare(if id.is_some() {"SELECT binding_id,binding_revision,task_revision,observed_unix_ms,payload,payload_hash FROM runtime_observations WHERE binding_id=?1"}else{"SELECT binding_id,binding_revision,task_revision,observed_unix_ms,payload,payload_hash FROM runtime_observations ORDER BY binding_id"})?;
    let mut rows=if let Some(id)=id {stmt.query([id])?}else{stmt.query([])?};
    let mut result=Vec::new();
    while let Some(r)=rows.next()? {
        if let Some(budget)=budget {budget.row(r,&[(4,1)])?;}
        let values=(r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<u64>>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?);
        let(id,revision,task_revision,time,payload,hash)=values;
        if format!("{:x}",Sha256::digest(payload.as_bytes()))!=hash {return Err(StoreError::Corrupt("observation payload hash mismatch".into()));}
        let observation:RuntimeObservation=serde_json::from_str(&payload).map_err(|_|StoreError::Corrupt("invalid observation payload".into()))?;
        observation.validate().map_err(StoreError::Corrupt)?;
        if observation.binding!=id||observation.binding_revision!=revision||observation.task_revision!=task_revision||observation.observed_unix_ms!=time {return Err(StoreError::Corrupt("observation identity mismatch".into()));}
        result.push(observation);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owned_fixture(stale:bool,padding:usize)->(tempfile::TempDir,SqliteStore,RuntimeObservation) {
        let root=tempfile::tempdir().unwrap();
        let mut db=SqliteStore::create(&root.path().join("state.db")).unwrap();
        let binding=db.create_runtime(None,None,0,&RuntimeRoute::default()).unwrap().binding;
        let owned=RuntimeOwnership {
            binding:binding.id.clone(),revision:1,binding_revision:binding.revision+u64::from(stale),
            identity_digest:super::super::ownership::identity_digest(&binding).unwrap(),origin:"adopted".into(),
            attempt:None,session:None,worktree:None,agent:None,config_digest:None,observed_unix_ms:1,
        };
        let mut value=serde_json::to_value(owned).unwrap();
        if padding>0 {value["historical_extension"]=serde_json::Value::String("x".repeat(padding));}
        let payload=value.to_string();
        db.connection.execute("INSERT INTO runtime_ownership VALUES(?1,1,?2,NULL,?3,?4)",params![binding.id,integer(binding.revision+u64::from(stale)).unwrap(),payload,format!("{:x}",Sha256::digest(payload.as_bytes()))]).unwrap();
        db.connection.execute("UPDATE project_control SET state='active',reconciliation_required=0 WHERE singleton=1",[]).unwrap();
        let observation=RuntimeObservation {binding:binding.id,binding_revision:binding.revision,observed_unix_ms:1,collector:"herdr-git-v2".into(),..Default::default()};
        (root,db,observation)
    }
    #[test]
    fn observation_ownership_generation_mismatch_invalidates_control() {
        for stale in [false,true] {
            let(_root,mut db,observation)=owned_fixture(stale,0);
            let head=db.current_head().unwrap();
            db.record_observations(head,&[observation]).unwrap();
            assert_eq!(db.project_control().unwrap().unwrap().reconciliation_required,stale);
        }
    }
    #[test]
    fn observation_ownership_budget_failure_rolls_back_publication() {
        let(_root,mut db,observation)=owned_fixture(false,17*1024*1024);
        let head=db.current_head().unwrap();
        let budget=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(10),Default::default()));
        let error=db.record_observations_with_budget(head,&[observation],Some(&budget)).unwrap_err();
        assert!(matches!(error,StoreError::Limit(_)),"{error}");
        assert_eq!(db.current_head().unwrap(),head);
        assert_eq!(db.connection.query_row("SELECT count(*) FROM runtime_observations",[],|row|row.get::<_,u64>(0)).unwrap(),0);
        assert!(!db.project_control().unwrap().unwrap().reconciliation_required);
    }
}
impl SqliteStore {
    /// Commit a complete collector batch against the exact state it observed.
    /// No external commands run here and no capacity/lifecycle state is changed.
    pub fn record_observations(&mut self,expected_head:u64,observations:&[RuntimeObservation])->Result<u64> {
        self.record_observations_with_budget(expected_head,observations,None)
    }
    pub(super) fn record_observations_with_budget(&mut self,expected_head:u64,observations:&[RuntimeObservation],budget:Option<&read_budget::ReadBudget>)->Result<u64> {
        if let Some(budget)=budget {budget.check()?;}
        let mut seen=std::collections::BTreeSet::new();
        for observation in observations {observation.validate().map_err(StoreError::Invalid)?;if !seen.insert(&observation.binding){return Err(StoreError::Conflict);}}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;check_schema(&tx)?;
        if head(&tx)?!=expected_head{return Err(StoreError::Conflict);}
        let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        // Schema 41 records the active inventory. A retired binding is not a hole in that inventory,
        // and a complete active batch may be larger than the old 128-binding refusal.
        let bindings=if schema>=41 {
            super::active_work::sync_projection_with_budget(&tx,budget)?;
            let mut indexed=Vec::new();
            let mut stmt=tx.prepare("SELECT binding_id FROM active_work_index ORDER BY ordinal")?;
            let mut rows=stmt.query([])?;
            while let Some(row)=rows.next()? {if let Some(budget)=budget {budget.row(row,&[])?;}indexed.push(row.get::<_,String>(0)?);}
            drop(rows);drop(stmt);
            if indexed.len()!=observations.len()||indexed.iter().any(|id|!seen.contains(id)) {return Err(StoreError::Conflict);}
            super::active_work::recorded_bindings(&tx,&indexed,budget)?
        } else {
            if observations.len()>128 {return Err(StoreError::Invalid("observation batch exceeds 128 records".into()));}
            let bindings=runtime::read_all_with_budget(&tx,budget)?;
            if bindings.len()!=observations.len(){return Err(StoreError::Conflict);}
            bindings
        };
        // Only tasks referenced by this complete active binding inventory can
        // affect its revision fence. Retained unrelated tasks are not decoded.
        let mut tasks=std::collections::BTreeMap::new();
        for id in bindings.iter().filter_map(|binding|binding.task.as_ref()) {
            if !tasks.contains_key(id) {
                let task=read_task_with_budget(&tx,id.as_str(),budget)?;
                tasks.insert(task.id,task.revision);
            }
        }
        let bindings=bindings.iter().map(|binding|(binding.id.as_str(),binding)).collect::<std::collections::BTreeMap<_,_>>();
        let by_binding=observations.iter().map(|observation|(observation.binding.as_str(),observation)).collect::<std::collections::BTreeMap<_,_>>();
        for observation in observations {
            let binding=bindings.get(observation.binding.as_str()).ok_or(StoreError::Conflict)?;
            if observation.binding_revision!=binding.revision || observation.task_revision!=binding.task.as_ref().and_then(|id|tasks.get(id).copied()) {return Err(StoreError::Conflict);}
            let old:Option<i64>=tx.query_row("SELECT observed_unix_ms FROM runtime_observations WHERE binding_id=?1",[&binding.id],|r|r.get(0)).optional()?;
            if old.is_some_and(|t|t>observation.observed_unix_ms){return Err(StoreError::Conflict);}
            let payload=serde_json::to_string(observation).map_err(|e|StoreError::Invalid(e.to_string()))?;
            let unchanged:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_observations WHERE binding_id=?1 AND payload=?2)",params![binding.id,payload],|r|r.get(0))?;
            if unchanged {continue;}
            tx.execute("INSERT INTO runtime_observations VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(binding_id) DO UPDATE SET binding_revision=excluded.binding_revision,task_revision=excluded.task_revision,observed_unix_ms=excluded.observed_unix_ms,payload=excluded.payload,payload_hash=excluded.payload_hash",params![binding.id,integer(binding.revision)?,observation.task_revision.map(integer).transpose()?,observation.observed_unix_ms,payload,format!("{:x}",Sha256::digest(payload.as_bytes()))])?;
            tx.execute("INSERT INTO events(kind,entity,revision,payload_version,payload) VALUES('runtime.observed',?1,?2,1,?3)",params![binding.id,integer(binding.revision)?,payload])?;
        }
        let schema:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
        if schema>=9&&super::control::read(&tx)?.state==ProjectState::Active {
            let mut changed=false;
            for owned in super::ownership::read_all_with_budget(&tx,budget)? {
                if schema>=41 && !bindings.contains_key(owned.binding.as_str()) {changed=true;continue;}
                if let Some(binding)=bindings.get(owned.binding.as_str()).filter(|binding|binding.revision==owned.binding_revision) {
                    let valid=match by_binding.get(binding.id.as_str()) {Some(o)=>super::ownership::observed(binding,binding.task.as_ref().and_then(|id|tasks.get(id).copied()),o,o.observed_unix_ms,o.config_digest.as_deref())&&super::ownership::matches(&owned,binding,o)?,None=>false};
                    if !valid{changed=true;}
                } else {
                    // A claim from another binding generation cannot certify
                    // the current inventory, even if the endpoint looks live.
                    changed=true;
                }
            }
            if changed {super::control::invalidate(&tx)?;}
        }
        let result=head(&tx)?;tx.commit()?;Ok(result)
    }
}
