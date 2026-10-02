//! Automatic integration jobs. They share the single-worker result lane with
//! verification jobs, one job at a time, with their own budget. The integrator
//! publishes only through its expected-old compare-and-swap; only an integrated
//! operation under the job key (the operation id) can confirm the delivery.
//! Project ownership covers the claim, the merge and the publication; the
//! candidate policy checks hold only the shared root and the job's scratch
//! fence, and the integrator rechecks its inputs before it publishes.
use std::{path::{Path,PathBuf},os::unix::fs::MetadataExt,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Serialize,Deserialize};
use crate::{canonical_verification_jobs::{Mode,Ownership,Slot},executor::{Identity,Lane,Request},runner::{Cmd,Output},source_tree::Control};
use herdr_farm::{migration,execution_guard::{ProjectGuard,Resource},domain::{Operation,OperationId},verification,
    integration::{self,IntegrateOutcome,IntegrateRequest,InputsChanged,Refused},
    operations::{Outcome,DeliveryState}};
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
            let mut failed=outcome.policies.iter().filter(|check|!check.passed).map(|check|format!(" (policy {} failed on the integrated candidate)",check.policy_id)).collect::<String>();
            if outcome.reason.as_deref()==Some("stale_base") {failed.push_str(" (integration target moved since the candidate was built; the ref was not overwritten)");}
            Outcome::PermanentFailure{diagnostic:format!("integration {}: {}{failed}; blocked for an operator or replan; {RETRY}",outcome.state,outcome.reason.as_deref().unwrap_or("no reason"))}
        },
        state=>Outcome::Retryable{no_effect_evidence:format!("integration {state} under the job key; the ref was not moved; resume with the same key")},
    }
}
/// Deterministic scratch beside `.state`; any leftover is removed first.
fn prepare_scratch(scratch:&Path)->Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    verification::clear_scratch(scratch)?;
    let parent=scratch.parent().context("integration scratch has no parent")?;
    if let Err(error)=std::fs::DirBuilder::new().mode(0o700).create(parent) {ensure!(error.kind()==std::io::ErrorKind::AlreadyExists,error);}
    ensure!(std::fs::symlink_metadata(parent)?.is_dir(),"integration scratch parent is not a directory");
    std::fs::DirBuilder::new().mode(0o700).create(scratch).context("integration scratch directory")
}
fn now()->i64 {jiff::Timestamp::now().as_millisecond()}
fn outcome_of(result:Result<IntegrateOutcome>)->Outcome {
    match result {
        Ok(outcome)=>classify(outcome),
        Err(error)=>match error.downcast_ref::<Refused>() {
            Some(Refused::CheckedOut)=>Outcome::Retryable{no_effect_evidence:format!("{error}: the target is checked out in a user worktree; nothing was published")},
            Some(refused@Refused::TargetMoved{..})=>Outcome::PermanentFailure{diagnostic:format!("{refused}; {RETRY}")},
            Some(refused@Refused::TooManyPolicies{..})=>Outcome::PermanentFailure{diagnostic:refused.to_string()},
            None if error.is::<InputsChanged>()||error.is::<verification::FenceChanged>()=>
                Outcome::Retryable{no_effect_evidence:format!("{error:#}; the ref was not moved; resume with the same key")},
            None=>Outcome::Ambiguous{observation_required:format!("integration stopped: {}; reconcile by key before any new effect",format!("{error:#}").chars().take(2048).collect::<String>())},
        },
    }
}

fn execute(input:&Input,control:&Control)->Result<()> {
    control.check()?;let mut guard=ProjectGuard::acquire(&input.project)?;guard.check_project(&input.project)?;
    let metadata=std::fs::metadata(&input.project)?;ensure!((metadata.dev(),metadata.ino())==input.identity,"integration project changed");
    let mut db=migration::open_active_unchecked(&input.project)?;
    let (_,rows)=db.operation_rows(&input.operation,None)?;let (operation,delivery)=rows.context("integration job missing")?;
    ensure!(operation.kind=="integration.run"&&delivery.revision==input.revision,"integration job is stale");
    let payload:Payload=serde_json::from_value(operation.payload.clone())?;
    let scratch=input.project.join(".integrate-scratch").join(operation.id.as_str());
    let fence=Resource::new("scratch",scratch.display().to_string())?;
    if input.mode==Mode::Observe {
        // A lost reply: classify the operation under the key before any new effect.
        ensure!(delivery.state==DeliveryState::Ambiguous,"integration observation is stale");
        let outcome=match integration::observe_job(&mut db,&payload.repository,operation.id.as_str())? {
            Some(outcome)=>classify(outcome),
            None=>{let _fence=guard.fence(&fence)?;verification::clear_scratch(&scratch)?;Outcome::Retryable{no_effect_evidence:"no integration operation under the job key; scratch removed; redeliver with the same key".into()}},
        };
        db.observe_operation(&operation.id,input.revision,OWNER,outcome,now())?;return Ok(());
    }
    if let Err(error)=verification::isolation_available() {
        // Candidate checks never run unsandboxed: stay pending with a visible reason.
        db.note_verification_paused(&operation.id,&format!("{error:#}"))?;return Err(error.context("integration paused"));
    }
    let claim=db.claim_operation(&input.operation,input.revision,OWNER,now(),LEASE_MS)?;
    let unrecorded=|error:String|anyhow::anyhow!("integration outcome unrecorded ({error}); retain claim and observe after expiry");
    let outcome=if control.check().is_err() {
        Outcome::Retryable{no_effect_evidence:"adapter authorization withdrawn before deliver was called".into()}
    } else {
        db.validate_claim(&claim,now()).map_err(|error|unrecorded(error.to_string()))?;
        let request=IntegrateRequest{result_id:payload.result_id.clone(),idempotency_key:operation.id.as_str().into(),repository:payload.repository.clone(),work_dir:scratch.clone(),fault:Default::default()};
        let mut ownership=Ownership{slot:Slot::Project(guard),scratch:fence,claim:&claim,control};
        let result=prepare_scratch(&scratch).and_then(|()|integration::integrate_job(&mut db,&request,&payload.submission_id,&mut ownership));
        // Never record or clean up without project ownership: the claim expires
        // and observation by key removes the scratch.
        let Slot::Project(held)=ownership.slot else {return Err(unrecorded(result.err().map_or_else(||"project ownership was not regained".into(),|e|format!("{e:#}"))))};
        guard=held;
        let cleanup=verification::clear_scratch(&scratch);
        outcome_of(result.and_then(|outcome|{cleanup.context("integration recorded but scratch cleanup failed")?;Ok(outcome)}))
    };
    db.finish_operation(&claim,outcome,now()).map_err(|error|unrecorded(error.to_string()))?;
    drop(guard);Ok(())
}
/// Entered from the result lane's runner for `JOB` commands.
pub fn run(command:&Cmd)->Result<Output> {
    let entered=Instant::now();ensure!(!command.timeout.is_zero()&&command.timeout<=BUDGET,"invalid integration budget");
    let text=command.stdin.as_deref().context("integration input missing")?;ensure!(text.len()<=64*1024,"integration input exceeds bounds");
    let control=Control{deadline:command.deadline.context("integration deadline missing")?.min(entered+command.timeout),cancellation:command.cancellation.clone().context("integration cancellation missing")?};
    execute(&serde_json::from_str(text)?,&control)?;Ok(Output{code:Some(0),elapsed:entered.elapsed(),..Output::default()})
}
