//! Versioned changes to intended work. This is not executable task authority.
use super::*;
use crate::domain::VersionedReference;
use std::collections::{BTreeMap,BTreeSet};

#[derive(Debug,Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    schema_version:u32,
    project_id:String,
    store_incarnation:String,
    request_id:String,
    idempotency_key:String,
    actor_id:String,
    delegation:Option<VersionedReference>,
    expected_plan_revision:u64,
    payload_digest:String,
}

#[derive(Debug,Clone,Deserialize,Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IntentReference {
    proposal_id:String,
    plan_revision:u64,
    payload_digest:String,
}

#[derive(Debug,Clone,Deserialize,Serialize)]
#[serde(tag="kind",rename_all="snake_case",deny_unknown_fields)]
pub(super) enum Change {
    CreateContract {task_id:TaskId},
    SupersedeUnstarted {task_id:TaskId,expected:IntentReference},
    AddDependencies {task_id:TaskId,expected:IntentReference,dependencies:Vec<Dependency>},
    RequestCancellation {task_id:TaskId,expected:IntentReference,reason:String},
}
impl Change {
    fn task(&self)->&TaskId {match self {
        Self::CreateContract{task_id}|Self::SupersedeUnstarted{task_id,..}|Self::AddDependencies{task_id,..}|Self::RequestCancellation{task_id,..}=>task_id,
    }}
    fn expected(&self)->Option<&IntentReference> {match self {
        Self::CreateContract{..}=>None,
        Self::SupersedeUnstarted{expected,..}|Self::AddDependencies{expected,..}|Self::RequestCancellation{expected,..}=>Some(expected),
    }}
}

pub(super) fn project_id(path:&str)->String {sha256_hex(format!("project-store\0{path}").as_bytes())}
fn digest(value:&str)->bool {value.len()==64 && value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn payload_digest(document:&ProposalFile)->Result<String> {
    let mut payload=serde_json::json!({"contracts":document.contracts,"planner":document.planner,"changes":document.changes});
    payload.sort_all_objects();
    Ok(sha256_hex(&serde_json::to_vec(&payload).map_err(|_|invalid("cannot encode proposal payload"))?))
}

pub(super) fn validate_document(document:&ProposalFile)->Result<()> {
    let envelope=document.envelope.as_ref().ok_or_else(||invalid("version 3 requires a proposal envelope"))?;
    let planner=document.planner.as_ref().ok_or_else(||invalid("version 3 requires a planner session"))?;
    if envelope.schema_version!=1 || !digest(&envelope.project_id) || !digest(&envelope.store_incarnation)
        || !identifier(&envelope.request_id) || !identifier(&envelope.idempotency_key)
        || envelope.actor_id!=planner.session_id || envelope.delegation.is_some()
        || envelope.expected_plan_revision>i64::MAX as u64 || !digest(&envelope.payload_digest)
        || envelope.payload_digest!=payload_digest(document)?
        || document.changes.is_empty() || document.changes.len()>64 || document.changes.len()!=document.contracts.len() {
        return Err(invalid("invalid version-3 proposal envelope or payload"));
    }
    let mut tasks=BTreeSet::new();
    for change in &document.changes {
        if !tasks.insert(change.task()) || !document.contracts.iter().any(|c|&c.task_id==change.task()) {
            return Err(invalid("each changed task requires exactly one resulting contract intent"));
        }
        if let Some(reference)=change.expected() {
            if reference.plan_revision==0 || reference.plan_revision>envelope.expected_plan_revision
                || !digest(&reference.proposal_id) || !digest(&reference.payload_digest) {return Err(invalid("invalid expected plan intent reference"));}
        }
        match change {
            Change::AddDependencies{dependencies,..} if dependencies.is_empty()||dependencies.len()>256=>return Err(invalid("invalid added dependency inventory")),
            Change::RequestCancellation{reason,..} if !plain(reason,4000)=>return Err(invalid("invalid cancellation reason")),
            _=>{},
        }
    }
    Ok(())
}

pub(super) fn validate_identity(db:&Connection,proposal:&ParsedProposal,path:&str,key:&str,parent:u64,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let Some(envelope)=&proposal.envelope else{return Ok(());};
    let version:u32=db.query_row("PRAGMA user_version",[],|r|r.get(0))?;
    if version<43 {return Err(StoreError::UnsupportedSchema(version));}
    let incarnation:String=read_budget::one(db,"SELECT incarnation FROM active_work_meta WHERE singleton=1",[],budget,&[],|r|r.get(0))?;
    if envelope.project_id!=project_id(path) || envelope.store_incarnation!=incarnation
        || envelope.idempotency_key!=key || envelope.expected_plan_revision!=parent {return Err(StoreError::Conflict);}
    let existing:Option<String>=read_budget::optional(db,"SELECT proposal_id FROM plan_proposal_requests WHERE request_id=?1",[&envelope.request_id],budget,&[],|r|r.get(0))?;
    if existing.is_some() {return Err(StoreError::Conflict);}
    Ok(())
}

pub(super) fn validate_replay(db:&Connection,proposal:&ParsedProposal,path:&str,key:&str,parent:u64,id:&str,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let Some(envelope)=&proposal.envelope else{return Ok(());};
    let incarnation:String=read_budget::one(db,"SELECT incarnation FROM active_work_meta WHERE singleton=1",[],budget,&[],|r|r.get(0))?;
    let linked:bool=read_budget::one(db,"SELECT EXISTS(SELECT 1 FROM plan_proposal_requests WHERE request_id=?1 AND proposal_id=?2)",params![envelope.request_id,id],budget,&[],|r|r.get(0))?;
    if envelope.project_id!=project_id(path) || envelope.store_incarnation!=incarnation || envelope.idempotency_key!=key
        || envelope.expected_plan_revision!=parent || !linked {return Err(StoreError::Conflict);}
    Ok(())
}

fn current(db:&Connection,task:&TaskId,expected:&IntentReference,budget:Option<&read_budget::ReadBudget>)->Result<ProposedContract> {
    let row:Option<(String,u64,String,Vec<u8>,String,Option<String>)>=read_budget::optional(db,
        "SELECT i.proposal_id,i.plan_revision,p.payload_digest,p.payload,i.dependencies,i.cancellation_reason FROM plan_task_intents i JOIN plan_proposals p ON p.proposal_id=i.proposal_id WHERE i.task_id=?1",
        [task.as_str()],budget,&[(3,1),(4,1)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let Some((id,revision,digest,raw,dependencies,cancellation_reason))=row else{return Err(StoreError::Conflict);};
    if id!=expected.proposal_id || revision!=expected.plan_revision || digest!=expected.payload_digest {return Err(StoreError::Conflict);}
    if sha256_hex(&raw)!=digest {return Err(StoreError::Corrupt("prior plan intent digest mismatch".into()));}
    let prior=parse_proposal(&raw)?.contracts.into_iter().find(|c|&c.task_id==task).ok_or_else(||StoreError::Corrupt("prior plan intent is missing".into()))?;
    let dependencies:Vec<Dependency>=serde_json::from_str(&dependencies).map_err(|_|StoreError::Corrupt("invalid prior plan dependencies".into()))?;
    if prior.dependencies!=dependencies || prior.cancellation_reason!=cancellation_reason {return Err(StoreError::Corrupt("prior plan intent projection mismatch".into()));}
    Ok(prior)
}
fn unstarted(db:&Connection,task:&TaskId,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    let started:bool=read_budget::one(db,"SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND (active_attempt IS NOT NULL OR state NOT IN ('draft','queued'))) OR EXISTS(SELECT 1 FROM attempts WHERE task_id=?1 AND termination_observed=0)",[task.as_str()],budget,&[],|r|r.get(0))?;
    if started {return Err(invalid("running or completed task requires reviewed rework, not unstarted supersession"));}Ok(())
}
fn dependencies(values:&[Dependency])->Result<BTreeMap<&TaskId,&'static str>> {
    let mut result=BTreeMap::new();
    for value in values {if result.insert(&value.predecessor,value.requirement.as_str()).is_some() {return Err(invalid("duplicate dependency"));}}
    Ok(result)
}

pub(super) fn validate_changes(db:&Connection,proposal:&ParsedProposal,budget:Option<&read_budget::ReadBudget>)->Result<()> {
    for change in &proposal.changes {
        if let Some(budget)=budget {budget.check()?;}
        let next=proposal.contracts.iter().find(|c|&c.task_id==change.task()).ok_or_else(||invalid("missing resulting intent"))?;
        if let Change::CreateContract{task_id}=change {
            let exists:bool=read_budget::one(db,"SELECT EXISTS(SELECT 1 FROM plan_task_intents WHERE task_id=?1) OR EXISTS(SELECT 1 FROM task_contracts WHERE task_id=?1)",[task_id.as_str()],budget,&[],|r|r.get(0))?;
            if exists || next.cancellation_reason.is_some() {return Err(StoreError::Conflict);}unstarted(db,task_id,budget)?;continue;
        }
        let prior=current(db,change.task(),change.expected().unwrap(),budget)?;
        match change {
            Change::SupersedeUnstarted{task_id,..}=>{unstarted(db,task_id,budget)?;if next.cancellation_reason.is_some(){return Err(invalid("supersession cannot request cancellation"));}},
            Change::AddDependencies{dependencies:added,..}=>{
                if prior.cancellation_reason.is_some() || next.cancellation_reason.is_some() || next.text!=prior.text {return Err(invalid("dependency addition cannot change contract text or cancellation"));}
                let mut expected=dependencies(&prior.dependencies)?;
                for (id,requirement) in dependencies(added)? {if expected.insert(id,requirement).is_some(){return Err(invalid("added dependency already exists"));}}
                if expected!=dependencies(&next.dependencies)? {return Err(invalid("resulting dependencies do not match additions"));}
            },
            Change::RequestCancellation{reason,..}=>{
                if prior.cancellation_reason.is_some() || next.text!=prior.text || next.dependencies!=prior.dependencies || next.cancellation_reason.as_ref()!=Some(reason) {return Err(invalid("cancellation must preserve contract intent and record the exact reason"));}
            },
            Change::CreateContract{..}=>unreachable!(),
        }
    }
    Ok(())
}

pub(super) fn record(db:&Connection,proposal:&ParsedProposal,id:&str)->Result<()> {
    if let Some(envelope)=&proposal.envelope {
        db.execute("INSERT INTO plan_proposal_requests(request_id,proposal_id) VALUES(?1,?2)",params![envelope.request_id,id])?;
    }Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(session:&PlannerSession,source:&PlanProposalReceipt,kind:&str)->Vec<u8> {
        let mut contract=serde_json::json!({"task_id":"task","text":"retained work","dependencies":[]});
        let mut change=serde_json::json!({"kind":kind,"task_id":"task","expected":{"proposal_id":source.proposal_id,"plan_revision":source.plan_revision,"payload_digest":source.digest}});
        if kind=="request_cancellation" {contract["cancellation_reason"]=serde_json::json!("review requested");change["reason"]=contract["cancellation_reason"].clone();}
        let value=serde_json::json!({"version":3,"planner":{"session_id":"change","input_digest":session.input_digest,"rationale":"reviewed intent"},"contracts":[contract],"changes":[change],"envelope":{"schema_version":1,"project_id":project_id(&session.input.project_store),"store_incarnation":session.input.store_incarnation,"request_id":"change","idempotency_key":"change","actor_id":"change","delegation":null,"expected_plan_revision":source.plan_revision,"payload_digest":"0".repeat(64)}});
        let mut document:ProposalFile=serde_json::from_value(value).unwrap();
        document.envelope.as_mut().unwrap().payload_digest=payload_digest(&document).unwrap();
        serde_json::to_vec(&document).unwrap()
    }

    #[test]
    fn cancellation_publication_rolls_back_and_never_releases_a_running_attempt() {
        let root=tempfile::tempdir().unwrap();let mut db=SqliteStore::create(&root.path().join("state.db")).unwrap();
        let original=br#"{"version":1,"contracts":[{"task_id":"task","text":"retained work","dependencies":[]}]}"#;
        let source=db.apply_plan_proposal(original,0,"initial").unwrap();
        db.connection.execute_batch("BEGIN; INSERT INTO tasks VALUES('task',1,'running','task',NULL); INSERT INTO attempts VALUES('attempt','task',1,'running',NULL,'capacity',0); UPDATE tasks SET active_attempt='attempt' WHERE id='task'; COMMIT;").unwrap();
        let budget=read_budget::ReadBudget::new(controlled::ReadControl::new(std::time::Instant::now()+Duration::from_secs(5),Default::default()));
        let session=db.create_planner_session("change","reviewed intent",&[],source.plan_revision,db.current_head().unwrap(),&budget).unwrap();
        let supersede=proposal(&session,&source,"supersede_unstarted");let head=db.current_head().unwrap();
        assert!(matches!(db.apply_plan_proposal(&supersede,1,"change"),Err(StoreError::Invalid(message)) if message.contains("running or completed")));
        let cancellation=proposal(&session,&source,"request_cancellation");
        db.connection.execute_batch("CREATE TEMP TRIGGER fail_request BEFORE INSERT ON plan_proposal_requests BEGIN SELECT RAISE(ABORT,'fixture publication failure'); END;").unwrap();
        assert!(db.apply_plan_proposal(&cancellation,1,"change").is_err());
        assert_eq!(db.current_head().unwrap(),head);assert_eq!(current_revision(&db.connection).unwrap(),1);
        let projection:(String,Option<String>)=db.connection.query_row("SELECT proposal_id,cancellation_reason FROM plan_task_intents",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(projection,(source.proposal_id,None));
        assert_eq!(db.connection.query_row("SELECT count(*) FROM planner_proposal_inputs",[],|r|r.get::<_,u64>(0)).unwrap(),0);
        db.connection.execute_batch("DROP TRIGGER fail_request").unwrap();
        db.apply_plan_proposal(&cancellation,1,"change").unwrap();
        let attempt:(String,bool,String)=db.connection.query_row("SELECT state,termination_observed,reservation FROM attempts",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(attempt,("running".into(),false,"capacity".into()));
        let task:(String,String)=db.connection.query_row("SELECT state,active_attempt FROM tasks",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(task,("running".into(),"attempt".into()));
        assert!(db.apply_plan_proposal(&cancellation,1,"change").unwrap().replayed);
        assert!(db.connection.execute("DELETE FROM plan_proposal_requests",[]).is_err());
    }
}
