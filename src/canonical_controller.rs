//! One bounded canonical controller pass. The ticker owns leadership; adapters
//! retain the root execution lease through claim, external effect and receipt.
use std::path::Path;
use anyhow::{Context,Result,ensure};
use crate::paths::Ctx;
use herdr_projects::{migration,runtime,operations::{DeliveryState,dispatch::DispatchResult},reconcile::ResourceState};

pub struct PollResult {pub reachable:bool,pub operation_error:Option<String>}
struct ProbeBudget<'a> {runner:&'a dyn crate::runner::Runner,deadline:std::time::Instant}
impl crate::runner::Runner for ProbeBudget<'_> {
    fn run(&self,cmd:&crate::runner::Cmd)->Result<crate::runner::Output> {
        let remaining=self.deadline.checked_duration_since(std::time::Instant::now()).context("automatic observation probe budget exhausted")?;
        let mut cmd=cmd.clone();cmd.timeout=cmd.timeout.min(remaining);self.runner.run(&cmd)
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:std::time::Duration)->Result<String> {
        let remaining=self.deadline.checked_duration_since(std::time::Instant::now()).context("automatic observation probe budget exhausted")?;
        self.runner.socket_request(socket,line,timeout.min(remaining))
    }
}
pub fn poll(ctx:&Ctx,path:&Path,turn:u64)->Result<PollResult> {
    let path=path.canonicalize()?;
    let snapshot=runtime::snapshot(&path)?;
    ensure!(snapshot.schema_version>=9,"upgrade-store is required before canonical controller polling");
    // Collect without a SQLite transaction. Commit and expiry are serialized with
    // all supported lifecycle/effect adapters, without reacquiring ticker leadership.
    let budget=ProbeBudget{runner:ctx.runner,deadline:std::time::Instant::now()+std::time::Duration::from_secs(15)};
    let probe_ctx=Ctx{env:ctx.env,root:ctx.root.clone(),config_dir:ctx.config_dir.clone(),runner:&budget,detached_ticker:ctx.detached_ticker};
    let batch=crate::reconcile_live::collect(&probe_ctx,&path)?;
    ensure!(std::time::Instant::now()<budget.deadline,"automatic observation budget exhausted; use explicit reconciliation to investigate");
    let reachable=batch.observations.iter().any(|o|o.pane==ResourceState::Present||o.worktree==ResourceState::Present);
    {
        let _lease=crate::cleanup::lease(path.parent().context("project has no root")?)?;
        runtime::record_observations_held(&path,&batch)?;
        migration::open_active(&path)?.expire_claims(jiff::Timestamp::now().as_millisecond())?;
    }
    let result=process_next(ctx,&path,turn);
    Ok(match result {Ok(progress)=>PollResult{reachable:reachable||progress,operation_error:None},Err(error)=>PollResult{reachable,operation_error:Some(format!("{error:#}"))}})
}
fn process_next(ctx:&Ctx,path:&Path,turn:u64)->Result<bool> {
    let snapshot=runtime::snapshot(path)?;
    // Lifecycle/policy validation stays in each adapter. No imported obligation,
    // new launch or terminal input is inferred from legacy files or inbox content.
    let now=jiff::Timestamp::now().as_millisecond();
    let mut candidates=Vec::new();
    for delivery in &snapshot.deliveries {
        let Some(operation)=snapshot.operations.iter().find(|o|o.id==delivery.operation) else{continue;};
        if (delivery.state==DeliveryState::Pending&&delivery.next_due_ms<=now&&matches!(operation.kind.as_str(),"runtime.notification"|"runtime.finalization"))
            || (delivery.state==DeliveryState::Ambiguous&&operation.kind=="runtime.finalization") {
            candidates.push((operation,delivery));
        }
    }
    if candidates.is_empty(){return Ok(false);}
    candidates.sort_by(|(a,_),(b,_)|a.id.cmp(&b.id));
    let(operation,delivery)=candidates[(turn%candidates.len() as u64) as usize];
    if delivery.state==DeliveryState::Ambiguous {
        crate::finalization_delivery::observe(ctx,&path,&operation.id,delivery.revision,snapshot.head)?;
        return Ok(true);
    }
    let result=match operation.kind.as_str() {
        "runtime.notification"=>crate::notification_delivery::deliver(ctx,&path,&operation.id,delivery.revision)?,
        "runtime.finalization"=>crate::finalization_delivery::deliver(ctx,&path,&operation.id,delivery.revision)?,
        _=>unreachable!("candidate kinds checked above"),
    };
    match result {
        DispatchResult::Recorded(_)=>Ok(true),
        DispatchResult::Unrecorded{..}=>anyhow::bail!("external operation outcome was not committed; retain claim until expiry and inspect receipts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runner::fake::ok,notification_delivery,finalization_delivery};
    use herdr_projects::{domain::ProjectState,operations::DeliveryState};
    #[test]
    fn ticker_delivers_accepted_notification_under_leadership_and_restart_does_not_replay() {
        let(world,path,task)=notification_delivery::tests::fixture();world.runner.on("--version",ok("herdr 0.9.1"));world.runner.on("notification show",ok(r#"{"result":{"shown":true}}"#));let leader=std::fs::File::create(world.root.join(".ticker.lock")).unwrap();leader.try_lock().unwrap();
        let before=runtime::snapshot(&path).unwrap();let config=world.ctx().config_dir.join("config.toml");let paused=runtime::set_state(&path,before.head,before.control.unwrap().revision,ProjectState::Paused,&config).unwrap();runtime::set_state(&path,paused.head,paused.control.revision,ProjectState::Active,&config).unwrap();let head=runtime::snapshot(&path).unwrap().head;runtime::add_task(&path,herdr_projects::domain::TaskId::new("second").unwrap(),"new task".into(),head).unwrap();assert!(migration::upgrade_active(&path).is_err());let op=notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();let mut memory=crate::steps::Memory::new(&world.ctx());assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));assert_eq!(world.runner.count("notification show"),1);assert_eq!(runtime::snapshot(&path).unwrap().deliveries.iter().find(|d|d.operation==op.id).unwrap().state,DeliveryState::Confirmed);
        drop(leader);let mut restarted=crate::steps::Memory::new(&world.ctx());crate::ticker::tick_for_test(&world.ctx(),&mut restarted);assert_eq!(world.runner.count("notification show"),1);assert_eq!(world.runner.count("agent prompt"),0);
    }
    #[test]
    fn controller_expires_claims_without_replaying_ambiguous_notifications() {
        let(world,path,task)=notification_delivery::tests::fixture();let op=notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();let mut db=migration::open_active(&path).unwrap();let now=jiff::Timestamp::now().as_millisecond();db.claim_operation(&op.id,1,"previous-controller",now,1).unwrap();std::thread::sleep(std::time::Duration::from_millis(3));
        poll(&world.ctx(),&path,0).unwrap();assert_eq!(runtime::snapshot(&path).unwrap().deliveries[0].state,DeliveryState::Ambiguous);poll(&world.ctx(),&path,0).unwrap();assert_eq!(world.runner.count("notification show"),0);
    }
    #[test]
    fn paused_control_blocks_notification_and_execution_lease_blocks_the_pass() {
        let(world,path,task)=notification_delivery::tests::fixture();let op=notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();let before=runtime::snapshot(&path).unwrap();let lease=crate::cleanup::lease(&world.root).unwrap();assert!(poll(&world.ctx(),&path,0).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);drop(lease);
        runtime::set_state(&path,before.head,before.control.unwrap().revision,ProjectState::Paused,&world.ctx().config_dir.join("config.toml")).unwrap();assert!(poll(&world.ctx(),&path,0).unwrap().operation_error.is_some());let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries.iter().find(|d|d.operation==op.id).unwrap().state,DeliveryState::Pending);assert_eq!(world.runner.count("notification show"),0);
    }
    #[test]
    fn blocked_operation_preserves_live_reachability_and_capacity() {
        use herdr_projects::domain::{Operation,OperationId,Commit,Mutation};
        let(world,path,_socket)=crate::runtime_ownership::tests::fixture();let before=runtime::snapshot(&path).unwrap();crate::runtime_ownership::adopt(&world.ctx(),&path,"thread:t-0001",2,before.head).unwrap();let before=runtime::snapshot(&path).unwrap();let task=before.tasks.iter().find(|t|t.active_attempt.is_some()).unwrap();
        let invalid=Operation{id:OperationId::new("malformed-notification").unwrap(),task:task.id.clone(),kind:"runtime.notification".into(),target:"coordinator".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:task.revision,due_unix_ms:0,idempotency_key:"malformed".into()};migration::open_active(&path).unwrap().commit(Commit{expected_head:before.head,mutations:vec![Mutation::Enqueue(invalid)]}).unwrap();
        let result=poll(&world.ctx(),&path,0).unwrap();assert!(result.reachable);assert!(result.operation_error.is_some());let mut memory=crate::steps::Memory::new(&world.ctx());assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));assert!(runtime::snapshot(&path).unwrap().attempts[0].retains_capacity());assert_eq!(world.runner.count("notification show"),0);
    }
    #[test]
    fn observation_budget_caps_native_probe_and_prevents_followup_spawns() {
        use crate::runner::{Runner,RealRunner,Cmd};use std::time::{Duration,Instant};
        let started=Instant::now();let budget=ProbeBudget{runner:&RealRunner,deadline:started+Duration::from_millis(40)};let cmd=Cmd::new("sh",Duration::from_secs(10)).args(["-c","sleep 5"]);let output=budget.run(&cmd).unwrap();assert!(!output.success());assert!(started.elapsed()<Duration::from_secs(2));assert!(budget.run(&Cmd::new("true",Duration::from_secs(10))).is_err());
    }
    #[test]
    fn controller_recovers_finalization_receipt_after_failed_commit_and_source_loss() {
        let(world,path,op)=finalization_delivery::tests::fixture();let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute_batch("CREATE TRIGGER reject_confirmation BEFORE UPDATE ON operation_delivery WHEN NEW.state='confirmed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();assert!(poll(&world.ctx(),&path,0).unwrap().operation_error.is_some());let snapshot=runtime::snapshot(&path).unwrap();assert_eq!(snapshot.deliveries[0].state,DeliveryState::Claimed);raw.execute_batch("DROP TRIGGER reject_confirmation").unwrap();
        let source=snapshot.runtime_bindings.iter().find(|b|b.task.as_ref()==Some(&op.task)).unwrap().identity.thread_dir.clone();std::fs::remove_dir_all(source).unwrap();migration::open_active(&path).unwrap().expire_claims(jiff::Timestamp::now().as_millisecond()+300_001).unwrap();poll(&world.ctx(),&path,0).unwrap();let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Confirmed);assert_eq!(after.tasks.iter().find(|t|t.id==op.task).unwrap().state,herdr_projects::domain::TaskState::AwaitingReview);let revision=after.tasks.iter().find(|t|t.id==op.task).unwrap().revision;poll(&world.ctx(),&path,0).unwrap();assert_eq!(runtime::snapshot(&path).unwrap().tasks.iter().find(|t|t.id==op.task).unwrap().revision,revision);
    }
}
