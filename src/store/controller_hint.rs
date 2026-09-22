//! Bounded scheduling hints. These are never effect authority or a partial Snapshot.
use super::*;
use super::identity_inventory::{Budget,Publication};
use anyhow::{Result,ensure,Context};
use crate::operations::notification::Notification;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum EffectMode {Deliver,Observe}
#[derive(Debug)]
pub struct ControllerEffectHint {
    pub head:u64,
    pub operation:Operation,
    pub delivery_revision:u64,
    pub mode:EffectMode,
    /// Only a routing hint; imported provenance and authority are checked by the worker.
    pub notification_socket:Option<String>,
}
// Inspect SQLite-owned values before creating Rust strings or JSON trees.
// SQLITE_LIMIT_LENGTH separately bounds SQLite's encoded row allocation.
fn measure(row:&rusqlite::Row<'_>,columns:&[usize],limit:usize,budget:&mut Budget)->Result<()> {
    for &column in columns {
        let bytes=match row.get_ref(column)? {rusqlite::types::ValueRef::Text(bytes)|rusqlite::types::ValueRef::Blob(bytes)=>bytes.len(),rusqlite::types::ValueRef::Null=>0,_=>anyhow::bail!("invalid controller hint field type")};
        ensure!(bytes<=limit,"controller hint field exceeds bounds");budget.charge(bytes)?;
    }
    Ok(())
}
fn operation(db:&Connection,id:&OperationId,budget:&mut Budget)->Result<Operation> {
    let mut stmt=db.prepare("SELECT id,task_id,kind,target,payload_version,payload,expected_revision,due_unix_ms,idempotency_key,payload_hash FROM operations WHERE id=?1 LIMIT 2")?;
    let mut rows=stmt.query([id.as_str()])?;let row=rows.next()?.context("selected operation disappeared")?;
    measure(row,&[0,1,2,3,5,8,9],MAX_RECORD_BYTES,budget)?;
    let mut value=serde_json::json!({"id":row.get::<_,String>(0)?,"task":row.get::<_,Option<String>>(1)?,"kind":row.get::<_,String>(2)?,"target":row.get::<_,String>(3)?,"payload_version":row.get::<_,u32>(4)?,"expected_revision":row.get::<_,u64>(6)?,"due_unix_ms":row.get::<_,i64>(7)?,"idempotency_key":row.get::<_,String>(8)?});
    let payload:String=row.get(5)?;let hash:String=row.get(9)?;
    ensure!(rows.next()?.is_none(),"duplicate selected controller operation");
    ensure!(format!("{:x}",Sha256::digest(payload.as_bytes()))==hash,"operation payload hash mismatch");
    budget.check()?;value["payload"]=serde_json::from_str(&payload)?;let operation:Operation=super::decode(value)?;
    ensure!(&operation.id==id,"selected controller operation identity mismatch");budget.check()?;Ok(operation)
}
fn notification_socket(db:&Connection,operation:&Operation,budget:&mut Budget)->Result<String> {
    let notice=Notification::decode(operation)?;
    let mut stmt=db.prepare("SELECT id,task_id,revision,source_path,payload,payload_hash FROM runtime_bindings WHERE id='coordinator' LIMIT 2")?;
    let mut rows=stmt.query([])?;let row=rows.next()?.context("notification route missing")?;measure(row,&[0,1,3,4,5],64*1024,budget)?;
    let(id,task,revision,source,payload,hash):(String,Option<String>,u64,Option<String>,String,String)=(row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?);
    ensure!(rows.next()?.is_none(),"duplicate notification route hint");
    ensure!(format!("{:x}",Sha256::digest(payload.as_bytes()))==hash,"notification route hint hash mismatch");
    let binding:RuntimeBinding=serde_json::from_str(&payload)?;
    ensure!(binding.id==id&&binding.task.as_ref().map(TaskId::as_str)==task.as_deref()&&binding.revision==revision&&binding.source_path==source,"notification route hint row mismatch");
    ensure!(binding.id=="coordinator"&&binding.task.is_none()&&binding.revision==notice.binding_revision,"notification route hint revision changed");
    RuntimeRoute::from_identity(&binding.identity).validate().map_err(anyhow::Error::msg)?;
    ensure!(binding.identity.machine.is_empty()&&Path::new(&binding.identity.socket).is_absolute(),"notification route hint must name a local socket");budget.check()?;Ok(binding.identity.socket)
}
pub(crate) fn read(path:&Path,publication:&Publication,budget:&mut Budget,turn:u64,now:i64)->Result<Option<ControllerEffectHint>> {
    read_with_launches(path,publication,budget,turn,now,false)
}
/// Selection only: concrete launch ingress must validate all current authority.
pub(crate) fn read_with_launches(path:&Path,publication:&Publication,budget:&mut Budget,turn:u64,now:i64,include_launches:bool)->Result<Option<ControllerEffectHint>> {
    super::delivery::now_check(now)?;
    super::identity_inventory::read_published(path,publication,budget,|tx,budget|{
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;ensure!(version>=9,"upgrade-store required for controller effects");
        // A missing operation has no trustworthy kind. Treat any unresolved
        // dangling delivery as unknown rather than filtering it out via JOIN.
        let dangling:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation_delivery d LEFT JOIN operations o ON o.id=d.operation_id WHERE d.state IN ('pending','ambiguous') AND o.id IS NULL)",[],|r|r.get(0))?;
        ensure!(!dangling,"controller hint has an unresolved dangling delivery");
        let mut stmt=tx.prepare("SELECT o.id,o.kind,d.revision,d.state FROM operation_delivery d JOIN operations o ON o.id=d.operation_id WHERE (d.state='pending' AND d.next_due_ms<=?1 AND o.kind IN ('runtime.notification','runtime.finalization','runtime.worker_brief')) OR (d.state='ambiguous' AND o.kind='runtime.finalization')")?;
        let mut rows=stmt.query([now])?;let mut candidates=Vec::new();
        while let Some(row)=rows.next()? {
            budget.record()?;measure(row,&[0,1,3],512,budget)?;budget.charge(32)?;
            let id=OperationId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;let kind:String=row.get(1)?;let revision:u64=row.get(2)?;ensure!(revision>0,"invalid controller hint delivery revision");let state:String=row.get(3)?;
            candidates.push((id,kind,revision,if state=="ambiguous"{EffectMode::Observe}else{EffectMode::Deliver},None));
        }
        drop(rows);drop(stmt);budget.check()?;
        if version>=11 {
            let mut stmt=tx.prepare("SELECT a.id,a.revision,
                (a.state='launching' AND NOT EXISTS(SELECT 1 FROM attempt_cancellations c WHERE c.attempt_id=a.id)
                 AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.kind='runtime.worker_brief' AND json_extract(o.payload,'$.attempt')=a.id))
                FROM attempts a JOIN attempt_inputs i ON i.attempt_id=a.id
                WHERE a.termination_observed=0 AND (EXISTS(SELECT 1 FROM events e WHERE e.kind IN ('runtime.launch_started','runtime.launch_target') AND e.entity=i.operation_id AND json_extract(e.payload,'$.version')=2)
                    OR (a.state='reserved' AND EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.worktrees_creation' AND e.entity=i.operation_id)
                        AND NOT EXISTS(SELECT 1 FROM events e WHERE e.kind GLOB 'runtime.launch_*' AND e.entity=i.operation_id)
                        AND (EXISTS(SELECT 1 FROM attempt_cancellations c WHERE c.attempt_id=a.id)
                            OR EXISTS(SELECT 1 FROM operation_delivery d WHERE d.operation_id=i.operation_id AND (d.lease_until_ms<=?1 OR d.state IN ('ambiguous','permanent_failure'))))))")?;
            let mut rows=stmt.query([now])?;
            while let Some(row)=rows.next()? {
                budget.record()?;measure(row,&[0],512,budget)?;budget.charge(32)?;
                let attempt=AttemptId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;let revision:u64=row.get(1)?;
                let id=OperationId::new(format!("terminate-{}",attempt.as_str())).map_err(anyhow::Error::msg)?;
                if row.get::<_,bool>(2)? {
                    budget.record()?;budget.charge(1024)?;
                    let prepare=OperationId::new(format!("prepare-brief-{}",attempt.as_str())).map_err(anyhow::Error::msg)?;
                    candidates.push((prepare,"runtime.worker_brief_prepare".into(),revision,EffectMode::Deliver,Some(attempt.clone())));
                }
                candidates.push((id,"runtime.worker_termination".into(),revision,EffectMode::Observe,Some(attempt)));
            }
        }
        if version>=11 {
            let mut stmt=tx.prepare("SELECT o.id,d.revision FROM operations o JOIN operation_delivery d ON d.operation_id=o.id
                JOIN attempt_inputs i ON i.operation_id=o.id JOIN attempts a ON a.id=i.attempt_id
                WHERE o.kind='runtime.launch' AND d.attempts=1 AND d.state<>'confirmed' AND a.state='reserved' AND a.termination_observed=0
                AND EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.launch_creation' AND e.entity=o.id)
                AND (NOT EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.launch_target' AND e.entity=o.id)
                    OR (EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.launch_release' AND e.entity=o.id)
                        AND NOT EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.launch_started' AND e.entity=o.id)))")?;
            let mut rows=stmt.query([])?;
            while let Some(row)=rows.next()? {
                budget.record()?;measure(row,&[0],512,budget)?;budget.charge(16)?;
                let id=OperationId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;
                let revision:u64=row.get(1)?;ensure!(revision>0,"invalid resource recovery revision");
                candidates.push((id,"runtime.launch".into(),revision,EffectMode::Observe,None));
            }
        }
        if include_launches && version>=13 {
            let mut stmt=tx.prepare("SELECT o.id,d.revision FROM operations o
                JOIN operation_delivery d ON d.operation_id=o.id
                JOIN attempt_inputs i ON i.operation_id=o.id
                JOIN attempts a ON a.id=i.attempt_id
                JOIN tasks t ON t.id=a.task_id
                WHERE o.kind='runtime.launch' AND a.state='reserved' AND a.termination_observed=0
                AND t.active_attempt=a.id
                AND EXISTS(SELECT 1 FROM project_control c WHERE c.singleton=1 AND c.state='active' AND c.reconciliation_required=0)
                AND NOT EXISTS(SELECT 1 FROM attempt_cancellations c WHERE c.attempt_id=a.id)
                AND ((d.state='pending' AND d.attempts=0 AND d.epoch=0 AND d.next_due_ms<=?1)
                    OR (d.state='claimed' AND d.attempts=1 AND d.lease_until_ms>?1))")?;
            let mut rows=stmt.query([now])?;
            while let Some(row)=rows.next()? {
                budget.record()?;measure(row,&[0],512,budget)?;budget.charge(16)?;
                let id=OperationId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;
                let revision:u64=row.get(1)?;ensure!(revision>0,"invalid launch scheduling revision");
                // A live original claim may both have an observation hint and be
                // eligible to advance. Offer it once, preserving rotation fairness.
                candidates.retain(|(old,_,_,_,_)|old!=&id);
                candidates.push((id,"runtime.launch".into(),revision,EffectMode::Deliver,None));
            }
        }
        if candidates.is_empty(){return Ok(None);}
        candidates.sort_by(|a,b|a.0.cmp(&b.0));
        let(id,kind,revision,mode,attempt)=&candidates[(turn%candidates.len() as u64) as usize];
        // Preparation and termination are identity-bound maintenance hints, not
        // outbox entries or authority to send a prompt or signal a PID.
        let operation=if let Some(attempt)=attempt {Operation{id:id.clone(),task:None,kind:kind.clone(),target:attempt.as_str().into(),payload_version:1,
            payload:serde_json::json!({"attempt":attempt}),expected_revision:*revision,due_unix_ms:now,idempotency_key:id.as_str().into()}}
            else {operation(tx,id,budget)?};ensure!(operation.kind==*kind,"controller operation kind changed");
        let socket=if kind=="runtime.notification"{Some(notification_socket(tx,&operation,budget)?)}else{None};
        budget.charge(8)?;Ok(Some(ControllerEffectHint{head:super::head(tx)?,operation,delivery_revision:*revision,mode:*mode,notification_socket:socket}))
    })
}

/// Execution admission only. Signed definition, payload, provenance and claim
/// validation remain the routine worker's responsibility.
#[derive(Debug)]
pub struct RoutineExecutionHint {pub operation:OperationId,pub delivery_revision:u64}
pub(crate) fn read_routine(path:&Path,publication:&Publication,budget:&mut Budget,last:Option<&OperationId>,now:i64)->Result<Option<RoutineExecutionHint>> {
    super::delivery::now_check(now)?;
    super::identity_inventory::read_published(path,publication,budget,|tx,budget|{
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;if version<16{return Ok(None);}
        let mut control=tx.prepare("SELECT revision,epoch,state,reconciliation_required,config_digest FROM project_control WHERE singleton=1 LIMIT 2")?;
        let mut rows=control.query([])?;let row=rows.next()?.context("routine control hint missing")?;measure(row,&[2,4],64,budget)?;
        let revision:u64=row.get(0)?;let epoch:u64=row.get(1)?;let state:String=row.get(2)?;let required:bool=row.get(3)?;let digest:Option<String>=row.get(4)?;
        ensure!(revision>0&&epoch>0&&matches!(state.as_str(),"active"|"paused"|"archived")&&digest.as_ref().is_none_or(|d|d.len()==64&&d.bytes().all(|b|b.is_ascii_hexdigit())),"invalid routine control hint");
        ensure!(rows.next()?.is_none(),"duplicate routine control hint");drop(rows);drop(control);
        if state!="active"||required {return Ok(None);}
        let dangling:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation_delivery d LEFT JOIN operations o ON o.id=d.operation_id WHERE d.state='pending' AND d.attempts=0 AND o.id IS NULL)",[],|r|r.get(0))?;
        ensure!(!dangling,"routine hint has a pending dangling delivery");
        let mut stmt=tx.prepare("SELECT o.id,d.revision FROM operation_delivery d JOIN operations o ON o.id=d.operation_id WHERE d.state='pending' AND d.attempts=0 AND d.next_due_ms<=?1 AND o.kind='routine.run'")?;
        let mut rows=stmt.query([now])?;let mut candidates=Vec::new();
        while let Some(row)=rows.next()? {
            budget.record()?;measure(row,&[0],512,budget)?;budget.charge(16)?;
            let operation=OperationId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;let revision:u64=row.get(1)?;ensure!(revision>0,"invalid routine delivery hint revision");
            candidates.push((operation,revision));
        }
        drop(rows);drop(stmt);budget.check()?;candidates.sort_by(|a,b|a.0.cmp(&b.0));
        ensure!(candidates.windows(2).all(|pair|pair[0].0!=pair[1].0),"duplicate routine candidate hint");
        if candidates.is_empty(){return Ok(None);}
        let selected=last.and_then(|last|candidates.iter().find(|(id,_)|id>last)).unwrap_or(&candidates[0]);
        Ok(Some(RoutineExecutionHint{operation:selected.0.clone(),delivery_revision:selected.1}))
    })
}
