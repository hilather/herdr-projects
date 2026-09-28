//! Telemetry rows written inside the reservation transaction (contracts §1–§3).
//! Reads are keyed by primary key and bounded; nothing here reads outcomes.
use super::*;
use crate::domain::{agent_configuration, classify_task, excerpt, AgentConfiguration, ContractScope, DispatchContext, EligibleProfile, OPERATOR_REASONS, PreparedDelegatedReservation, TASK_TAXONOMY};

fn invalid(message:&str)->StoreError {StoreError::Invalid(message.into())}

/// Contracts §4 lifecycle mark in the transition's own transaction. `source`
/// names the transition function; a state keeps the first time it was reached.
/// Stores before 0051 have no mark table and record nothing.
pub(super) fn mark(tx:&Connection,attempt:&Attempt,now:i64,source:&str)->Result<()> {
    let version:u32=tx.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<51 {return Ok(());}
    tx.execute("INSERT INTO attempt_lifecycle VALUES(?1,?2,?3,?4,?5) ON CONFLICT(attempt_id,state) DO NOTHING",
        params![attempt.id.as_str(),attempt.state.as_str(),integer(attempt.revision)?,now,source])?;
    Ok(())
}

/// The contracts §3 decision for a new attempt, after its `attempts` row. The
/// context only describes the choice: its chosen entry must be the reserved profile.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_decision(tx:&Connection,inputs:&LaunchInputs,attempt:&AttemptId,task_revision:u64,classification:Option<&str>,dispatch:&DispatchContext,delegated:Option<&PreparedDelegatedReservation>,now:i64)->Result<()> {
    let profile=inputs.effective_profile.as_ref().ok_or_else(||invalid("dispatch requires an effective profile"))?;
    let chosen=agent_configuration(profile);
    let only=||vec![EligibleProfile{configuration:chosen.clone(),profile_digest:inputs.profile.digest.clone(),status:"chosen"}];
    let (kind,principal,mut reasons,note,eligible)=match (dispatch,delegated) {
        (DispatchContext::Operator{reason,note},None)=>{
            let reason=reason.as_deref().unwrap_or("unspecified");
            if !OPERATOR_REASONS.contains(&reason) {return Err(invalid("unknown dispatch reason"));}
            let home=std::env::var("HOME").ok();
            ("operator",format!("approval:{}",inputs.approval.digest),vec![reason],note.as_deref().and_then(|n|excerpt(n,home.as_deref())),only())
        }
        (DispatchContext::Automatic{eligible},None)=>{
            let mut picked=eligible.iter().filter(|e|e.status=="chosen");
            let valid=eligible.len()<=256&&eligible.iter().all(|e|matches!(e.status,"chosen"|"no_knowledge"|"no_approval"|"not_evaluated"));
            if !valid||!matches!((picked.next(),picked.next()),(Some(e),None) if e.configuration==chosen&&e.profile_digest==inputs.profile.digest) {
                return Err(invalid("dispatch eligible set differs from the reserved profile"));
            }
            ("automatic_admission","rule:automatic-admission.v1".to_owned(),vec!["first_matching_approval"],None,eligible.clone())
        }
        (DispatchContext::Delegated,Some(delegated))=>("delegated",format!("grant:{}",delegated.request.grant_id),vec!["delegated_grant"],None,only()),
        _=>return Err(invalid("dispatch context differs from the reservation path")),
    };
    if kind!="operator"&&eligible.len()==1 {reasons.push("only_eligible");}
    reasons.sort_unstable();reasons.dedup();
    for entry in &eligible {insert_configuration(tx,&entry.configuration,now)?;}
    let eligible=serde_json::Value::Array(eligible.iter().map(|e|serde_json::json!({"configuration_id":e.configuration.id,
        "probability_ppm":if e.status=="chosen"{1_000_000}else{0},"profile_digest":e.profile_digest,"status":e.status})).collect()).to_string();
    let contract=inputs.task_contract.as_ref().map(|c|integer(c.revision)).transpose()?;
    tx.execute("INSERT INTO dispatch_decisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,?12)",params![attempt.as_str(),inputs.task.as_str(),integer(task_revision)?,contract,
        classification,chosen.id,eligible,kind,principal,serde_json::json!(reasons).to_string(),note,now])?;
    Ok(())
}

/// Content-addressed and immutable: an existing ID must carry identical bytes.
fn insert_configuration(tx:&Connection,configuration:&AgentConfiguration,now:i64)->Result<()> {
    tx.execute("INSERT INTO agent_configurations VALUES(?1,?2,?3) ON CONFLICT(configuration_id) DO NOTHING",params![configuration.id,configuration.canonical_json,now])?;
    let stored:String=tx.query_row("SELECT canonical_json FROM agent_configurations WHERE configuration_id=?1",[&configuration.id],|r|r.get(0))?;
    if stored!=configuration.canonical_json {return Err(StoreError::Corrupt("agent configuration bytes differ".into()));}
    Ok(())
}

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

impl SqliteStore {
    /// One read transaction for a read-only telemetry projection; nothing is written.
    pub(crate) fn telemetry_read<T>(&mut self,read:impl FnOnce(&Connection)->rusqlite::Result<T>)->Result<T> {
        let tx=self.connection.transaction()?;check_schema(&tx)?;
        let value=read(&tx)?;tx.commit()?;Ok(value)
    }
}
