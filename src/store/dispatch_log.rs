//! Telemetry rows written inside the reservation transaction (contracts §1).
//! Reads are keyed by primary key and bounded; nothing here reads outcomes.
use super::*;
use crate::domain::{classify_task, ContractScope, TASK_TAXONOMY};

/// Reuse revision 1 for `(task, contract revision, taxonomy)` or write it now.
pub(super) fn classify_in_transaction(tx:&Connection,inputs:&LaunchInputs,dependencies:usize,now:i64,budget:Option<&read_budget::ReadBudget>)->Result<String> {
    let task=inputs.task.as_str();let revision=inputs.task_contract.as_ref().map(|c|integer(c.revision)).transpose()?;
    let existing:Option<String>=match revision {
        Some(revision)=>tx.query_row("SELECT classification_id FROM task_classifications WHERE task_id=?1 AND contract_revision=?2 AND taxonomy=?3 AND revision=1",params![task,revision,TASK_TAXONOMY],|r|r.get(0)),
        None=>tx.query_row("SELECT classification_id FROM task_classifications WHERE task_id=?1 AND contract_revision IS NULL AND taxonomy=?2 AND revision=1",params![task,TASK_TAXONOMY],|r|r.get(0)),
    }.optional()?;
    if let Some(id)=existing {return Ok(id);}
    let scope=match revision {Some(revision)=>Some(contract_scope(tx,task,revision,budget)?),None=>None};
    let c=classify_task(task,scope.as_ref(),dependencies,inputs.repositories.len(),now);
    tx.execute("INSERT INTO task_classifications(classification_id,task_id,contract_revision,taxonomy,class,band,features,classifier,revision,reason,created_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,NULL,?9)",
        params![c.id,task,revision,TASK_TAXONOMY,c.class,c.band,c.features,crate::domain::TASK_CLASSIFIER,now])?;
    Ok(c.id)
}

fn contract_scope(tx:&Connection,task:&str,revision:i64,budget:Option<&read_budget::ReadBudget>)->Result<ContractScope> {
    let route:String=tx.query_row("SELECT route FROM task_contracts WHERE task_id=?1 AND contract_revision=?2",params![task,revision],|r|r.get(0)).optional()?.ok_or(StoreError::Conflict)?;
    let mut write_paths=Vec::new();
    // CHECK bounds ordinals to 0..64 and named resources to three names.
    let mut stmt=tx.prepare("SELECT path,certainty FROM contract_scope_paths WHERE task_id=?1 AND contract_revision=?2 AND access='write' ORDER BY ordinal LIMIT 64")?;
    let mut rows=stmt.query(params![task,revision])?;
    while let Some(row)=rows.next()? {if let Some(budget)=budget {budget.row(row,&[])?;}write_paths.push((row.get::<_,String>(0)?,row.get::<_,String>(1)?=="uncertain"));}
    let mut write_named_resources=Vec::new();
    let mut stmt=tx.prepare("SELECT name FROM contract_named_resources WHERE task_id=?1 AND contract_revision=?2 AND access='write' ORDER BY name LIMIT 3")?;
    let mut rows=stmt.query(params![task,revision])?;
    while let Some(row)=rows.next()? {if let Some(budget)=budget {budget.row(row,&[])?;}write_named_resources.push(row.get(0)?);}
    Ok(ContractScope{revision:u64::try_from(revision).map_err(|_|StoreError::Conflict)?,route,write_paths,write_named_resources})
}
