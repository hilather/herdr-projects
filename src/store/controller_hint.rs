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
    super::delivery::now_check(now)?;
    super::identity_inventory::read_published(path,publication,budget,|tx,budget|{
        let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;ensure!(version>=9,"upgrade-store required for controller effects");
        // A missing operation has no trustworthy kind. Treat any unresolved
        // dangling delivery as unknown rather than filtering it out via JOIN.
        let dangling:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation_delivery d LEFT JOIN operations o ON o.id=d.operation_id WHERE d.state IN ('pending','ambiguous') AND o.id IS NULL)",[],|r|r.get(0))?;
        ensure!(!dangling,"controller hint has an unresolved dangling delivery");
        let mut stmt=tx.prepare("SELECT o.id,o.kind,d.revision,d.state FROM operation_delivery d JOIN operations o ON o.id=d.operation_id WHERE (d.state='pending' AND d.next_due_ms<=?1 AND o.kind IN ('runtime.notification','runtime.finalization')) OR (d.state='ambiguous' AND o.kind='runtime.finalization')")?;
        let mut rows=stmt.query([now])?;let mut candidates=Vec::new();
        while let Some(row)=rows.next()? {
            budget.record()?;measure(row,&[0,1,3],512,budget)?;budget.charge(32)?;
            let id=OperationId::new(row.get::<_,String>(0)?).map_err(anyhow::Error::msg)?;let kind:String=row.get(1)?;let revision:u64=row.get(2)?;ensure!(revision>0,"invalid controller hint delivery revision");let state:String=row.get(3)?;
            candidates.push((id,kind,revision,if state=="ambiguous"{EffectMode::Observe}else{EffectMode::Deliver}));
        }
        drop(rows);drop(stmt);budget.check()?;
        if candidates.is_empty(){return Ok(None);}
        candidates.sort_by(|a,b|a.0.cmp(&b.0));
        let(id,kind,revision,mode)=&candidates[(turn%candidates.len() as u64) as usize];
        let operation=operation(tx,id,budget)?;ensure!(operation.kind==*kind,"controller operation kind changed");
        let socket=if kind=="runtime.notification"{Some(notification_socket(tx,&operation,budget)?)}else{None};
        budget.charge(8)?;Ok(Some(ControllerEffectHint{head:super::head(tx)?,operation,delivery_revision:*revision,mode:*mode,notification_socket:socket}))
    })
}
