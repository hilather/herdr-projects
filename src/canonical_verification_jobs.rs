//! Automatic verification jobs run in their own single-worker lane, one at a
//! time, with their own budget. The verifier records its run under the job key
//! (the operation id); only that stored row can confirm the delivery.
use std::{collections::BTreeMap,path::{Path,PathBuf},os::unix::fs::MetadataExt,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{executor::{Executor,Identity,Lane,Limits,Request,Ticket},runner::{Runner,Cmd,Output,RealRunner},source_tree::Control};
use herdr_projects::{migration,execution_guard::ProjectGuard,domain::{Operation,OperationId},store::SqliteStore,verification,
    operations::{Claim,Outcome,DeliveryState,dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult}}};
const JOB:&str="\0herdr-projects-canonical-verification";
/// Verifier timeout plus checkout and record; the claim lease is the store's 300 s maximum.
const BUDGET:Duration=Duration::from_secs(330);
const LEASE_MS:i64=300_000;
const OWNER:&str="ticker.verification";
#[derive(Clone,Copy,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="snake_case")]
pub enum Mode {Deliver,Observe}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),operation:OperationId,revision:u64,mode:Mode}
#[derive(Deserialize)]
struct Payload {project_store:String,submission_id:String,policy_id:String,policy_digest:String}

pub fn request(path:&Path,operation:&Operation,revision:u64,mode:Mode)->Result<Request> {
    let project=path.canonicalize()?;let metadata=std::fs::metadata(&project)?;
    let input=Input{identity:(metadata.dev(),metadata.ino()),project,operation:operation.id.clone(),revision,mode};
    let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(serde_json::to_string(&input)?);command.deadline=Some(deadline);
    let kind=if mode==Mode::Deliver{"deliver"}else{"observe"};
    Ok(Request{identity:Identity{operation:format!("canonical-verification-{kind}:{}",operation.id.as_str()),revision,project:input.project.display().to_string(),machine:"local-verifier".into(),terminal:None},lane:Lane::Transfer,deadline,command})
}

#[derive(Clone,Copy)]
struct Adapter<'a> {project:&'a Path,payload:&'a Payload,scratch:&'a Path,control:&'a Control}
struct Prepared<'a> {adapter:Adapter<'a>}
impl<'a> DeliveryAdapter for Adapter<'a> {
    type Prepared=Prepared<'a>;
    fn prepare(&mut self,operation:&Operation)->Result<Prepared<'a>> {
        ensure!(operation.kind=="verification.run","not a verification job");self.control.check()?;Ok(Prepared{adapter:*self})
    }
}
impl PreparedDelivery for Prepared<'_> {
    fn revalidate(&mut self,_:&Operation)->Result<()>{self.adapter.control.check()}
    fn deliver(&mut self,operation:&Operation,_:&Claim)->Result<Outcome> {
        let a=self.adapter;let mut db=migration::open_active(a.project)?;
        let job=verification::StoredJob{submission_id:&a.payload.submission_id,policy_id:&a.payload.policy_id,policy_digest:&a.payload.policy_digest,
            key:operation.id.as_str(),scratch:a.scratch,cancellation:a.control.cancellation.clone()};
        let error=match verification::verify_stored(&mut db,&job) {Ok(_)=>None,Err(error)=>Some(error)};
        // Success or not, only a run stored under the key is evidence.
        Ok(match (recorded(&mut db,a.payload,operation)?,error) {
            (Some(run),_)=>Outcome::Confirmed{observed_identity:run},
            (None,_) if a.control.check().is_err()=>Outcome::Retryable{no_effect_evidence:"verifier cancelled before a verdict; no run is recorded under the job key".into()},
            (None,error)=>Outcome::PermanentFailure{diagnostic:format!("verifier stopped before recording a verdict: {}; retry with `result <slug> retry-verification`",
                error.map_or_else(||"no run recorded".into(),|e|format!("{e:#}")).chars().take(2048).collect::<String>())},
        })
    }
}
fn recorded(db:&mut SqliteStore,payload:&Payload,operation:&Operation)->Result<Option<String>> {
    Ok(verification::recorded_run(db,&payload.project_store,operation.id.as_str())?.map(|(run,_)|run))
}

fn execute(input:&Input,control:&Control)->Result<()> {
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;guard.check_project(&input.project)?;
    let metadata=std::fs::metadata(&input.project)?;ensure!((metadata.dev(),metadata.ino())==input.identity,"verification project changed");
    let mut db=migration::open_active(&input.project)?;
    let (_,rows)=db.operation_rows(&input.operation,None)?;let (operation,delivery)=rows.context("verification job missing")?;
    ensure!(operation.kind=="verification.run"&&delivery.revision==input.revision,"verification job is stale");
    let payload:Payload=serde_json::from_value(operation.payload.clone())?;
    // Beside, not inside, `.state`: the verifier refuses a checkout within the store directory.
    let scratch=input.project.join(".verify-scratch").join(operation.id.as_str());
    let now=||jiff::Timestamp::now().as_millisecond();
    if input.mode==Mode::Observe {
        // A lost reply: confirm a recorded run, or prove there is none and redeliver under the same key.
        ensure!(delivery.state==DeliveryState::Ambiguous,"verification observation is stale");
        let outcome=match recorded(&mut db,&payload,&operation)? {
            Some(run)=>Outcome::Confirmed{observed_identity:run},
            None=>{verification::clear_scratch(&scratch)?;Outcome::Retryable{no_effect_evidence:"no verification run is recorded under the job key; scratch removed; redeliver with the same key".into()}},
        };
        db.observe_operation(&operation.id,input.revision,OWNER,outcome,now())?;return Ok(());
    }
    if let Err(error)=verification::isolation_available() {
        // Never run unsandboxed: stay pending with a visible reason.
        db.note_verification_paused(&operation.id,&format!("{error:#}"))?;return Err(error.context("verification paused"));
    }
    let mut adapter=Adapter{project:&input.project,payload:&payload,scratch:&scratch,control};
    match dispatch::dispatch_one(&mut db,DispatchRequest{operation:&input.operation,expected_revision:input.revision,owner:OWNER,lease_ms:LEASE_MS},&mut adapter,now)? {
        DispatchResult::Recorded(_)=>Ok(()),
        DispatchResult::Unrecorded{..}=>anyhow::bail!("verification outcome unrecorded; retain claim and observe after expiry"),
    }
}
pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,command:&Cmd)->Result<Output> {
        if command.program!=JOB{return self.inner.run(command);}
        let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid verification budget");
        let text=command.stdin.as_deref().context("verification input missing")?;ensure!(text.len()<=64*1024,"verification input exceeds bounds");
        let control=Control{deadline:command.deadline.context("verification deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("verification cancellation missing")?};
        execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,path:&Path,line:&str,timeout:Duration)->Result<String>{self.inner.socket_request(path,line,timeout)}
}

/// The verifier lane: its own executor with one worker, one offer and one ticket.
pub struct VerifierLane {executor:Arc<Executor>,offer:Option<Request>,pending:Option<(Identity,Ticket)>,cooldown:BTreeMap<String,Instant>}
impl VerifierLane {
    pub fn new()->Result<Self> {
        let limits=Limits{workers:[1,1],outstanding:[1,1],per_project:1,per_machine:1};
        Ok(Self{executor:Arc::new(Executor::new(limits,Arc::new(JobRunner{inner:Arc::new(RealRunner)}))?),offer:None,pending:None,cooldown:BTreeMap::new()})
    }
    pub fn offer(&mut self,work:Request) {
        let held=self.pending.as_ref().is_some_and(|(identity,_)|identity.operation==work.identity.operation);
        if !held&&self.cooldown.get(&work.identity.operation).is_none_or(|until|Instant::now()>=*until) {self.offer=Some(work);}
    }
    pub fn offered(&self)->bool {self.offer.is_some()}
    pub fn offered_where(&self,allowed:impl Fn(&str)->bool)->bool {self.offer.as_ref().is_some_and(|work|allowed(&work.identity.project))}
    pub fn pending(&self)->bool {self.pending.is_some()}
    pub fn pending_project(&self,project:&str)->bool {self.pending.as_ref().is_some_and(|(identity,_)|identity.project==project)}
    pub fn admit(&mut self)->Vec<String> {
        if self.pending.is_some(){return Vec::new();}
        let Some(work)=self.offer.take() else {return Vec::new();};let identity=work.identity.clone();
        match self.executor.submit(work) {Ok(ticket)=>{self.pending=Some((identity,ticket));Vec::new()},Err(error)=>vec![format!("{} {}: verifier admission: {error:#}",identity.project,identity.operation)]}
    }
    pub fn drain(&mut self)->Vec<String> {
        let Some((identity,ticket))=&self.pending else {return Vec::new();};
        let result=match ticket.try_recv() {
            Ok(None)=>return Vec::new(),
            Ok(Some(completion))=>completion.result.and_then(|output|{ensure!(completion.identity==*identity&&output.success(),"verifier job failed");Ok(())}),
            Err(error)=>Err(error),
        };
        let (identity,_)=self.pending.take().unwrap();let now=Instant::now();self.cooldown.retain(|_,until|*until>now);
        match result {Ok(())=>Vec::new(),Err(error)=>{self.cooldown.insert(identity.operation.clone(),now+Duration::from_secs(30));vec![format!("{} {}: verifier queue: {error:#}",identity.project,identity.operation)]}}
    }
}
impl Drop for VerifierLane {fn drop(&mut self){if let Some((_,ticket))=&self.pending{ticket.cancel();}}}
