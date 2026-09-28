//! Automatic integration jobs. They share the single-worker result lane with
//! verification jobs, one job at a time, with their own budget. The integrator
//! publishes only through its expected-old compare-and-swap; only an integrated
//! operation under the job key (the operation id) can confirm the delivery.
//! Unlike verification, the whole job keeps project ownership: its candidate
//! checks run inside the integrator between the merge and the publication.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{canonical_verification_jobs::Mode,executor::{Identity,Lane,Request},runner::{Cmd,Output},source_tree::Control};
use herdr_projects::{migration,execution_guard::ProjectGuard,domain::{Operation,OperationId},verification,
    integration::{self,IntegrateOutcome,IntegrateRequest,Refused},
    operations::{Claim,Outcome,DeliveryState,dispatch::{self,DeliveryAdapter,PreparedDelivery,DispatchRequest,DispatchResult}}};
pub const JOB:&str="\0herdr-projects-canonical-integration";
/// Merge, candidate checks (each policy within `integration::POLICY_TIMEOUT`, at most
/// `integration::MAX_POLICIES`) and publication; the claim lease is the store's 300 s maximum.
const BUDGET:Duration=Duration::from_secs(240);
const LEASE_MS:i64=300_000;
const OWNER:&str="ticker.integration";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:PathBuf,identity:(u64,u64),operation:OperationId,revision:u64,mode:Mode}
#[derive(Deserialize)]
struct Payload {submission_id:String,result_id:String,repository:PathBuf}

pub fn request(path:&Path,operation:&Operation,revision:u64,mode:Mode)->Result<Request> {
    let project=path.canonicalize()?;let metadata=std::fs::metadata(&project)?;
    let input=Input{identity:(metadata.dev(),metadata.ino()),project,operation:operation.id.clone(),revision,mode};
    let deadline=Instant::now()+BUDGET;let mut command=Cmd::new(JOB,BUDGET).stdin(serde_json::to_string(&input)?);command.deadline=Some(deadline);
    let kind=if mode==Mode::Deliver{"deliver"}else{"observe"};
    Ok(Request{identity:Identity{operation:format!("canonical-integration-{kind}:{}",operation.id.as_str()),revision,project:input.project.display().to_string(),machine:"local-integrator".into(),terminal:None},lane:Lane::Transfer,deadline,command})
}

const RETRY:&str="once resolved, retry with `result <slug> retry-integration <operation> --expected-revision <revision>`";
/// Terminal states end the job; anything else resumes under the same key.
fn classify(outcome:IntegrateOutcome)->Outcome {
    match outcome.state.as_str() {
        "integrated"=>Outcome::Confirmed{observed_identity:outcome.operation_id},
        "blocked"|"discarded"|"reconciliation_required"=>{
            let failed=outcome.policies.iter().filter(|check|!check.passed).map(|check|format!(" (policy {} failed on the integrated candidate)",check.policy_id)).collect::<String>();
            Outcome::PermanentFailure{diagnostic:format!("integration {}: {}{failed}; blocked for an operator or replan; {RETRY}",outcome.state,outcome.reason.as_deref().unwrap_or("no reason"))}
        },
        state=>Outcome::Retryable{no_effect_evidence:format!("integration {state} under the job key; the ref was not moved; resume with the same key")},
    }
}
/// Deterministic scratch beside `.state`; any leftover is removed first and after.
fn with_scratch<T>(scratch:&Path,run:impl FnOnce()->Result<T>)->Result<T> {
    use std::os::unix::fs::DirBuilderExt;
    verification::clear_scratch(scratch)?;
    let parent=scratch.parent().context("integration scratch has no parent")?;
    if let Err(error)=std::fs::DirBuilder::new().mode(0o700).create(parent) {ensure!(error.kind()==std::io::ErrorKind::AlreadyExists,error);}
    ensure!(std::fs::symlink_metadata(parent)?.is_dir(),"integration scratch parent is not a directory");
    std::fs::DirBuilder::new().mode(0o700).create(scratch).context("integration scratch directory")?;
    let result=run();let cleanup=verification::clear_scratch(scratch);
    let result=result?;cleanup.context("integration recorded but scratch cleanup failed")?;Ok(result)
}

#[derive(Clone,Copy)]
struct Adapter<'a> {project:&'a Path,payload:&'a Payload,scratch:&'a Path,control:&'a Control}
struct Prepared<'a> {adapter:Adapter<'a>}
impl<'a> DeliveryAdapter for Adapter<'a> {
    type Prepared=Prepared<'a>;
    fn prepare(&mut self,operation:&Operation)->Result<Prepared<'a>> {
        ensure!(operation.kind=="integration.run","not an integration job");self.control.check()?;Ok(Prepared{adapter:*self})
    }
}
impl PreparedDelivery for Prepared<'_> {
    fn revalidate(&mut self,_:&Operation)->Result<()>{self.adapter.control.check()}
    fn deliver(&mut self,operation:&Operation,_:&Claim)->Result<Outcome> {
        let a=self.adapter;let mut db=migration::open_active(a.project)?;
        let request=IntegrateRequest{result_id:a.payload.result_id.clone(),idempotency_key:operation.id.as_str().into(),repository:a.payload.repository.clone(),work_dir:a.scratch.to_path_buf(),fault:Default::default()};
        Ok(match with_scratch(a.scratch,||integration::integrate_job(&mut db,&request,&a.payload.submission_id)) {
            Ok(outcome)=>classify(outcome),
            Err(error)=>match error.downcast_ref::<Refused>() {
                Some(Refused::CheckedOut)=>Outcome::Retryable{no_effect_evidence:format!("{error}: the target is checked out in a user worktree; nothing was published")},
                Some(refused@Refused::TargetMoved{..})=>Outcome::PermanentFailure{diagnostic:format!("{refused}; {RETRY}")},
                Some(refused@Refused::TooManyPolicies{..})=>Outcome::PermanentFailure{diagnostic:refused.to_string()},
                None=>Outcome::Ambiguous{observation_required:format!("integration stopped: {}; reconcile by key before any new effect",format!("{error:#}").chars().take(2048).collect::<String>())},
            },
        })
    }
}

fn execute(input:&Input,control:&Control)->Result<()> {
    control.check()?;let guard=ProjectGuard::acquire(&input.project)?;guard.check_project(&input.project)?;
    let metadata=std::fs::metadata(&input.project)?;ensure!((metadata.dev(),metadata.ino())==input.identity,"integration project changed");
    let mut db=migration::open_active(&input.project)?;
    let (_,rows)=db.operation_rows(&input.operation,None)?;let (operation,delivery)=rows.context("integration job missing")?;
    ensure!(operation.kind=="integration.run"&&delivery.revision==input.revision,"integration job is stale");
    let payload:Payload=serde_json::from_value(operation.payload.clone())?;
    let scratch=input.project.join(".integrate-scratch").join(operation.id.as_str());
    let now=||jiff::Timestamp::now().as_millisecond();
    if input.mode==Mode::Observe {
        // A lost reply: classify the operation under the key before any new effect.
        ensure!(delivery.state==DeliveryState::Ambiguous,"integration observation is stale");
        let outcome=match integration::observe_job(&mut db,&payload.repository,operation.id.as_str())? {
            Some(outcome)=>classify(outcome),
            None=>{verification::clear_scratch(&scratch)?;Outcome::Retryable{no_effect_evidence:"no integration operation under the job key; scratch removed; redeliver with the same key".into()}},
        };
        db.observe_operation(&operation.id,input.revision,OWNER,outcome,now())?;return Ok(());
    }
    if let Err(error)=verification::isolation_available() {
        // Candidate checks never run unsandboxed: stay pending with a visible reason.
        db.note_verification_paused(&operation.id,&format!("{error:#}"))?;return Err(error.context("integration paused"));
    }
    let mut adapter=Adapter{project:&input.project,payload:&payload,scratch:&scratch,control};
    match dispatch::dispatch_one(&mut db,DispatchRequest{operation:&input.operation,expected_revision:input.revision,owner:OWNER,lease_ms:LEASE_MS},&mut adapter,now)? {
        DispatchResult::Recorded(_)=>Ok(()),
        DispatchResult::Unrecorded{..}=>anyhow::bail!("integration outcome unrecorded; retain claim and observe after expiry"),
    }
}
/// Entered from the result lane's runner for `JOB` commands.
pub fn run(command:&Cmd)->Result<Output> {
    let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid integration budget");
    let text=command.stdin.as_deref().context("integration input missing")?;ensure!(text.len()<=64*1024,"integration input exceeds bounds");
    let control=Control{deadline:command.deadline.context("integration deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("integration cancellation missing")?};
    execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
}
