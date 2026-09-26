//! One bounded canonical controller pass. Observations retain project ownership;
//! terminal-effect adapters still retain the exclusive root execution lease.
#[path="canonical_observations.rs"]
pub mod observations;
use std::path::Path;
use anyhow::{Context,Result,ensure};
use crate::paths::Ctx;
use herdr_projects::{runtime,operations::dispatch::DispatchResult,reconcile::ResourceState};
#[cfg(test)]
use herdr_projects::migration;

// Verified prepared launches participate in ordinary controller polling. Native
// capabilities, current inputs, signed approval and capacity remain enforced at
// ingress; enabling dispatch does not certify optional worker protocols.
const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true;
pub(crate) fn launch_dispatch_enabled()->bool { PREPARED_LAUNCH_DISPATCH_ENABLED }


pub struct PollResult {pub reachable:bool,pub scheduled_work:bool,pub unknown_effects:bool,pub operation_error:Option<String>,pub admission_log:Option<String>}
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
    let ownership=herdr_projects::execution_guard::ProjectGuard::acquire(&path)?;
    let mut db=herdr_projects::migration::open_active(&path)?;
    let now=jiff::Timestamp::now().as_millisecond();
    match herdr_projects::store::HOT_PATH_READ {
        herdr_projects::store::HotPathRead::Targeted=>{
            let schema=match db.read_targeted_hot_path(now,launch_dispatch_enabled()) {
                Ok(schema)=>schema,
                Err(error)=>{
                    let _=herdr_projects::watchdog::note(&path,&error);
                    if matches!(error,herdr_projects::store::StoreError::Cancelled|herdr_projects::store::StoreError::Deadline) {
                        anyhow::bail!("controller read aborted: {error}");
                    }
                    return Err(error.into());
                }
            };
            ensure!(schema>=9,"upgrade-store is required before canonical controller polling");
        }
        herdr_projects::store::HotPathRead::Snapshot=>{
            let snapshot=db.read_snapshot(None)?;
            if let Err(error)=db.shadow_against_snapshot(&snapshot,now,launch_dispatch_enabled()) {
                if matches!(error,herdr_projects::store::StoreError::Cancelled|herdr_projects::store::StoreError::Deadline) {
                    anyhow::bail!("controller read aborted: {error}");
                }
            }
            ensure!(snapshot.schema_version>=9,"upgrade-store is required before canonical controller polling");
        }
    }
    // Collect without a SQLite transaction. Commit and expiry are serialized with
    // all supported lifecycle/effect adapters, without reacquiring ticker leadership.
    let budget=ProbeBudget{runner:ctx.runner,deadline:std::time::Instant::now()+std::time::Duration::from_secs(15)};
    let probe_ctx=Ctx{env:ctx.env,root:ctx.root.clone(),config_dir:ctx.config_dir.clone(),runner:&budget,detached_ticker:ctx.detached_ticker};
    let batch=crate::reconcile_live::collect(&probe_ctx,&path)?;
    ensure!(std::time::Instant::now()<budget.deadline,"automatic observation budget exhausted; use explicit reconciliation to investigate");
    let reachable=batch.observations.iter().any(|o|o.pane==ResourceState::Present||o.worktree==ResourceState::Present);
    runtime::record_controller_observations_guarded(&path,&batch,&ownership)?;
    drop(ownership);
    finish_poll(ctx,&path,turn,reachable,None,None,None)
}
/// Background maintenance commits planning/observations itself. Replies carry
/// liveness only; effects get their main-pass opportunity before replenishment.
#[cfg(test)]
pub fn poll_queued(ctx:&Ctx,path:&Path,turn:u64,reads:&mut observations::Reads)->Result<PollResult> {
    poll_queued_effects(ctx,path,turn,reads,None)
}
pub fn poll_queued_effects(ctx:&Ctx,path:&Path,turn:u64,reads:&mut observations::Reads,effects:Option<&mut crate::copy_jobs::Queue>)->Result<PollResult> {
    let (reachable,scheduled,error)=match reads.poll(ctx,path) {
        Ok(observations::Poll::Ready(sample))=>(sample.reachable.unwrap_or(false),sample.scheduled_work.unwrap_or(false),sample.diagnostic),
        Ok(observations::Poll::Pending)=>(false,false,None),
        Ok(observations::Poll::Failed(error))=>(false,false,Some(error)),
        Err(error)=>(false,false,Some(format!("canonical maintenance: {error:#}"))),
    };
    finish_poll(ctx,path,turn,reachable,error,effects,Some(scheduled))
}
fn finish_poll(ctx:&Ctx,path:&Path,turn:u64,reachable:bool,observation_error:Option<String>,effects:Option<&mut crate::copy_jobs::Queue>,background_plan:Option<bool>)->Result<PollResult> {
    // Background maintenance owns planning. Foreground callers retain their
    // explicit synchronous service; either path still offers independent effects.
    let scheduled=match background_plan {Some(active)=>Ok(herdr_projects::routines::ScheduleTurn{active,diagnostic:None}),None=>herdr_projects::routines::schedule_turn(path,turn)};
    let queued=effects.is_some();
    let (admission_log,result)=process_next_with_launches(ctx,&path,turn,effects,launch_dispatch_enabled());
    let mut errors=observation_error.into_iter().collect::<Vec<_>>();
    let routine_work=match scheduled {Ok(report)=>{if let Some(error)=report.diagnostic {errors.push(format!("routine scheduling: {error}"));}report.active},Err(error)=>{errors.push(format!("routine scheduling: {error:#}"));false}};
    // An admission failure is diagnostic only. Already-prepared dispatch still runs.
    let (progress,unknown_effects)=match result {
        Ok((progress,admission))=>{if let Some(error)=admission {errors.push(format!("admission: {error}"));}(progress,false)}
        Err(error)=>{errors.push(format!("{error:#}"));(false,queued)}
    };
    Ok(PollResult{reachable:reachable||progress,scheduled_work:routine_work,unknown_effects,operation_error:(!errors.is_empty()).then(||errors.join("; ")),admission_log})
}
fn process_next(ctx:&Ctx,path:&Path,turn:u64,effects:Option<&mut crate::copy_jobs::Queue>)->Result<bool> {
    Ok(process_next_with_launches(ctx,path,turn,effects,launch_dispatch_enabled()).1?.0)
}
#[cfg(target_os="linux")]
fn linux_admission(path:&Path)->(Option<String>,Option<String>) {
    let started=std::time::Instant::now();
    if herdr_projects::watchdog::is_paused(path) {
        let reason=herdr_projects::watchdog::pause_reason(path).unwrap_or("admission_paused");
        let line=herdr_projects::watchdog::admission_log_line(reason,None,0);
        return (Some(format!("admission_paused: {reason}")),Some(line));
    }
    if !herdr_projects::admission::wake_enabled(path) {return (None,None);}
    // Ok(Some) is backpressure, not success. Prepared dispatch still runs after this note.
    match herdr_projects::admission::admit_decision(path) {
        Ok(decision)=>{
            let line=herdr_projects::watchdog::admission_log_line(decision.reason,decision.task_id.as_deref(),started.elapsed().as_millis());
            let diagnostic=decision.block.map(|block| format!("{}: {}", block.blocker, block.reason));
            (diagnostic,Some(line))
        }
        Err(error)=>{
            let reason=error.downcast_ref::<herdr_projects::store::StoreError>().and_then(herdr_projects::watchdog::cause).unwrap_or("error");
            if let Some(store)=error.downcast_ref::<herdr_projects::store::StoreError>() {
                let _=herdr_projects::watchdog::note(path,store);
            }
            let line=herdr_projects::watchdog::admission_log_line(reason,None,started.elapsed().as_millis());
            (Some(format!("{error:#}")),Some(line))
        }
    }
}
#[cfg(not(target_os="linux"))]
fn linux_admission(_path:&Path)->(Option<String>,Option<String>) {(None,None)}
fn process_next_with_launches(ctx:&Ctx,path:&Path,turn:u64,effects:Option<&mut crate::copy_jobs::Queue>,include_launches:bool)->(Option<String>,Result<(bool,Option<String>)>) {
    let (admission,admission_log)=linux_admission(path);
    let result=match dispatch_prepared(ctx,path,turn,effects,include_launches) {
        Ok(progress)=>Ok((progress,admission)),
        Err(error)=>Err(match &admission {Some(note)=>error.context(format!("admission: {note}")),None=>error}),
    };
    (admission_log,result)
}
fn dispatch_prepared(ctx:&Ctx,path:&Path,turn:u64,effects:Option<&mut crate::copy_jobs::Queue>,include_launches:bool)->Result<bool> {
    if let Some(effects)=effects{return offer_next(ctx,path,turn,effects,include_launches);}
    use herdr_projects::store::{identity_inventory::Budget,controller_hint::EffectMode};
    let mut budget=Budget::new(2*1024*1024,1024,std::time::Instant::now()+std::time::Duration::from_millis(100),Default::default())?;
    let Some(hint)=herdr_projects::migration::read_controller_dispatch_hint(path,&mut budget,turn,jiff::Timestamp::now().as_millisecond(),include_launches)? else{return Ok(false);};
    let operation=&hint.operation;
    if operation.kind=="runtime.launch" {
        #[cfg(target_os="linux")]
        return match hint.mode {
            EffectMode::Deliver=>Ok(herdr_projects::canonical_worker::advance_launch(path,&operation.id,hint.delivery_revision,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default())?.is_some()),
            EffectMode::Observe=>Ok(herdr_projects::canonical_worker::reconcile_launch(path,&operation.id,hint.delivery_revision,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default())?),
        };
        #[cfg(not(target_os="linux"))]
        anyhow::bail!("canonical resource recovery requires Linux pidfs");
    }
    if operation.kind=="runtime.worker_brief_prepare" {
        herdr_projects::canonical_worker::prepare_brief(path,&herdr_projects::domain::AttemptId::new(operation.target.clone()).map_err(anyhow::Error::msg)?,hint.delivery_revision,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default())?;
        return Ok(true);
    }
    if operation.kind=="runtime.worker_termination" {
        #[cfg(target_os="linux")]
        return Ok(herdr_projects::canonical_worker::reconcile_termination(path,&herdr_projects::domain::AttemptId::new(operation.target.clone()).map_err(anyhow::Error::msg)?,hint.delivery_revision,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default())?.is_some());
        #[cfg(not(target_os="linux"))]
        anyhow::bail!("canonical termination requires Linux pidfs");
    }
    if hint.mode==EffectMode::Observe {
        ensure!(operation.kind=="runtime.finalization","unsupported observation hint");
        crate::finalization_delivery::observe(ctx,path,&operation.id,hint.delivery_revision,hint.head)?;
        return Ok(true);
    }
    let result=match operation.kind.as_str() {
        "runtime.notification"=>crate::notification_delivery::deliver(ctx,path,&operation.id,hint.delivery_revision)?,
        "runtime.finalization"=>crate::finalization_delivery::deliver(ctx,path,&operation.id,hint.delivery_revision)?,
        "runtime.worker_brief"=>DispatchResult::Recorded(herdr_projects::canonical_worker::deliver_brief(path,&operation.id,hint.delivery_revision,std::time::Instant::now()+std::time::Duration::from_secs(45),Default::default())?),
        _=>anyhow::bail!("unsupported controller effect hint"),
    };
    match result {
        DispatchResult::Recorded(_)=>Ok(true),
        DispatchResult::Unrecorded{..}=>anyhow::bail!("external operation outcome was not committed; retain claim until expiry and inspect receipts"),
    }
}

// A hint selects work only. Concrete workers retain full validation before
// claiming or acting; a route hint does not certify provenance or authority.
fn offer_next(ctx:&Ctx,path:&Path,turn:u64,effects:&mut crate::copy_jobs::Queue,include_launches:bool)->Result<bool> {
    use herdr_projects::store::{identity_inventory::Budget,controller_hint::EffectMode};
    let mut budget=Budget::new(2*1024*1024,1024,std::time::Instant::now()+std::time::Duration::from_millis(100),Default::default())?;
    let Some(hint)=herdr_projects::migration::read_controller_dispatch_hint(path,&mut budget,turn,jiff::Timestamp::now().as_millisecond(),include_launches)? else{return Ok(false);};
    match hint.operation.kind.as_str() {
        "runtime.notification"=>effects.offer_canonical_notification(ctx,path,&hint.operation,hint.delivery_revision,hint.notification_socket.as_deref().context("notification route hint missing")?)?,
        "runtime.finalization"=>effects.offer_canonical_finalization(ctx,path,&hint.operation,hint.delivery_revision,match hint.mode {EffectMode::Deliver=>crate::canonical_finalization_jobs::Mode::Deliver,EffectMode::Observe=>crate::canonical_finalization_jobs::Mode::Observe})?,
        "runtime.launch" if hint.mode==EffectMode::Deliver=>effects.offer_canonical_launch(path,&hint.operation,hint.delivery_revision)?,
        "runtime.worker_brief"|"runtime.worker_termination"|"runtime.worker_brief_prepare"|"runtime.launch"=>effects.offer_canonical_brief(path,&hint.operation,hint.delivery_revision)?,
        _=>anyhow::bail!("unsupported controller effect hint"),
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{runner::fake::ok,notification_delivery,finalization_delivery};
    use herdr_projects::{domain::ProjectState,operations::DeliveryState};
    pub(crate) fn routine_fixture(scripts:&[(&str,&[u8],u64)])->(crate::scenarios::World,std::path::PathBuf) {
        use crate::{scenarios::World,project,runner::{RealRunner,Runner,Cmd}};
        use herdr_projects::{authority,domain::*};
        use std::{fs,time::Duration};use sha2::{Digest,Sha256};
        let world=World::new();let project=project::create(&world.root,"routines","",vec![]).unwrap();project.set_status(project::Status::Paused).unwrap();
        crate::inbox::write(&project,"test","fixture","notification survives routine error","").unwrap();
        let path=project.dir().canonicalize().unwrap();let config=world.ctx().config_dir.join("config.toml");fs::create_dir_all(config.parent().unwrap()).unwrap();
        let key=world.home.path().join("routine-owner");
        assert!(RealRunner.run(&Cmd::new("/usr/bin/ssh-keygen",Duration::from_secs(5)).args(["-q","-t","ed25519","-N","","-f"]).arg(key.to_str().unwrap())).unwrap().success());
        let public=fs::read_to_string(key.with_extension("pub")).unwrap().split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        fs::write(&config,format!("[authority]\nversion=1\nrevision=1\napproval_public_key={public:?}\n[safety.{:?}]\nroutine_commands=true\n",path.display().to_string())).unwrap();
        let plan=migration::inspect_with_config(&path,&config).unwrap();migration::apply(&path,&plan,true).unwrap();
        let s=runtime::snapshot(&path).unwrap();runtime::set_state(&path,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
        for &(name,bytes,deadline_ms) in scripts {
            let script=path.join(format!("{name}.sh"));fs::write(&script,bytes).unwrap();
            let d=RoutineDefinition{version:1,name:name.into(),revision:1,project_store:path.join(".state/state.db").display().to_string(),authority:authority::policy_reference(&path).unwrap(),config:migration::config_reference(&config).unwrap(),enabled:true,
                schedule:"every 1h".into(),timezone:"UTC".into(),start_unix_ms:jiff::Timestamp::now().as_millisecond()-1000,missed:MissedRunPolicy::CoalesceLatest,overlap:OverlapPolicy::Skip,
                script:script.display().to_string(),script_sha256:format!("{:x}",Sha256::digest(bytes)),cwd:path.display().to_string(),deadline_ms,output_cap_bytes:4000};
            let document=world.home.path().join(format!("{name}.json"));let signature=document.with_extension("sig");let bytes=serde_json::to_vec(&d).unwrap();fs::write(&document,&bytes).unwrap();
            let signed=RealRunner.run(&Cmd::new("/usr/bin/ssh-keygen",Duration::from_secs(5)).args(["-Y","sign","-f"]).arg(key.to_str().unwrap()).args(["-n",authority::ROUTINE_SIGNATURE_NAMESPACE]).stdin(std::str::from_utf8(&bytes).unwrap())).unwrap();assert!(signed.success());fs::write(&signature,signed.stdout_bytes).unwrap();
            authority::import_routine(&path,&document,&signature,runtime::snapshot(&path).unwrap().head).unwrap();
        }
        (world,path)
    }
    #[test]
    fn ticker_rotates_signed_routines_records_once_and_keeps_future_work_alive() {
        use std::fs;use herdr_projects::domain::*;
        let(world,path)=routine_fixture(&[("a-broken",b"touch MUST_NOT_EXECUTE\n",1000),("b-healthy",b"touch MUST_NOT_EXECUTE\n",1000)]);
        let config=world.ctx().config_dir.join("config.toml");
        fs::write(path.join("a-broken.sh"),"edited after approval").unwrap();
        let leader=fs::File::create(world.root.join(".ticker.lock")).unwrap();leader.try_lock().unwrap();
        let mut memory=crate::steps::Memory::new(&world.ctx());
        assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));
        assert!(runtime::snapshot(&path).unwrap().routine_occurrences.is_empty());
        assert!(crate::ticker::tick_for_test(&world.ctx(),&mut memory));
        let s=runtime::snapshot(&path).unwrap();assert_eq!(s.routine_occurrences.len(),1);assert_eq!(s.routine_occurrences[0].routine.id,"routine-b-healthy");
        assert_eq!(s.deliveries[0].state,DeliveryState::Pending);assert_eq!(s.deliveries[0].attempts,0);assert!(!path.join("MUST_NOT_EXECUTE").exists());
        // A fresh controller must not duplicate the due instant, and a routine
        // with only future work keeps the controller alive without a session.
        let mut restarted=crate::steps::Memory::new(&world.ctx());crate::ticker::tick_for_test(&world.ctx(),&mut restarted);
        assert!(crate::ticker::tick_for_test(&world.ctx(),&mut restarted));assert_eq!(runtime::snapshot(&path).unwrap().routine_occurrences.len(),1);
        let s=runtime::snapshot(&path).unwrap();runtime::set_state(&path,s.head,s.control.unwrap().revision,ProjectState::Paused,&config).unwrap();
        assert!(!crate::ticker::tick_for_test(&world.ctx(),&mut restarted));assert_eq!(runtime::snapshot(&path).unwrap().routine_occurrences.len(),1);
        let s=runtime::snapshot(&path).unwrap();let pending=&s.deliveries[0];
        runtime::retire_operation(&path,&pending.operation,pending.revision,s.head,"fixture: retire the unclaimed occurrence before resuming").unwrap();
        let task=TaskId::new("notify").unwrap();let head=runtime::snapshot(&path).unwrap().head;
        let head=runtime::add_task(&path,task.clone(),"notification".into(),head).unwrap();
        runtime::create_binding(&path,None,None,head,&RuntimeRoute{socket:"/explicit/routine-notification.sock".into(),..Default::default()}).unwrap();
        crate::reconcile_live::run(&world.ctx(),&path,true).unwrap();let s=runtime::snapshot(&path).unwrap();
        runtime::set_state(&path,s.head,s.control.unwrap().revision,ProjectState::Active,&config).unwrap();
        world.runner.on("--version",ok("herdr 0.9.1")).on("notification show",ok(r#"{"result":{"shown":true}}"#));
        let op=notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();
        let result=poll(&world.ctx(),&path,0).unwrap();assert!(result.operation_error.unwrap().contains("a-broken"));
        assert_eq!(world.runner.count("notification show"),1);
        assert_eq!(runtime::snapshot(&path).unwrap().deliveries.iter().find(|d|d.operation==op.id).unwrap().state,DeliveryState::Confirmed);
    }

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
        let invalid=Operation{id:OperationId::new("malformed-notification").unwrap(),task:Some(task.id.clone()),kind:"runtime.notification".into(),target:"coordinator".into(),payload_version:1,payload:serde_json::json!({}),expected_revision:task.revision,due_unix_ms:0,idempotency_key:"malformed".into()};migration::open_active(&path).unwrap().commit(Commit{expected_head:before.head,mutations:vec![Mutation::Enqueue(invalid)]}).unwrap();
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
        let source=snapshot.runtime_bindings.iter().find(|b|b.task==op.task).unwrap().identity.thread_dir.clone();std::fs::remove_dir_all(source).unwrap();migration::open_active(&path).unwrap().expire_claims(jiff::Timestamp::now().as_millisecond()+300_001).unwrap();poll(&world.ctx(),&path,0).unwrap();let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Confirmed);assert_eq!(after.tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().state,herdr_projects::domain::TaskState::AwaitingReview);let revision=after.tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().revision;poll(&world.ctx(),&path,0).unwrap();assert_eq!(runtime::snapshot(&path).unwrap().tasks.iter().find(|t|Some(&t.id)==op.task.as_ref()).unwrap().revision,revision);
    }
    #[test]
    fn effect_hint_does_not_bypass_worker_provenance_checks() {
        use std::{sync::Arc,time::{Duration,Instant}};
        let(world,path,_op)=finalization_delivery::tests::fixture();let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
        raw.execute("UPDATE runtime_bindings SET payload_hash=?1",["0".repeat(64)]).unwrap();assert!(runtime::snapshot(&path).is_err());
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(crate::canonical_finalization_jobs::JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());
        assert!(!process_next(&world.ctx(),&path,0,Some(&mut queue)).unwrap());assert!(queue.offered());assert!(queue.admit().is_empty());let end=Instant::now()+Duration::from_secs(3);let mut errors=Vec::new();
        while queue.pending(){errors.extend(queue.drain());assert!(Instant::now()<end);std::thread::sleep(Duration::from_millis(5));}
        assert_eq!(errors.len(),1);let state:(String,u64)=raw.query_row("SELECT state,attempts FROM operation_delivery WHERE operation_id LIKE 'finalize-%'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(state,("pending".into(),0));assert!(!path.join(".state/canonical-artifacts").exists());assert!(pool.stop(Duration::from_secs(2)));
    }
    #[test]
    fn unreadable_effect_hints_veto_idle_exit_instead_of_reporting_no_work() {
        use std::sync::Arc;
        let(world,path,_op)=finalization_delivery::tests::fixture();let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute("UPDATE operations SET payload_hash=?1",["0".repeat(64)]).unwrap();
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(crate::runner::RealRunner)).unwrap());let mut queue=crate::copy_jobs::Queue::new(pool.clone());
        let result=finish_poll(&world.ctx(),&path,0,false,None,Some(&mut queue),Some(false)).unwrap();assert!(!result.reachable);assert!(!result.scheduled_work);assert!(result.unknown_effects);assert!(result.operation_error.unwrap().contains("hash mismatch"));assert!(!queue.offered());assert!(pool.stop(std::time::Duration::from_secs(2)));
    }

    #[cfg(target_os="linux")]
    #[test]
    fn poll_reserves_one_dependent_only_when_factory_admission_is_already_on() {
        use herdr_projects::{domain::*,reconcile::RuntimeObservation,store::SqliteStore};
        use sha2::{Digest,Sha256};
        use std::{os::unix::fs::MetadataExt,sync::Arc};
        let root=tempfile::tempdir().unwrap();
        let project=root.path().join("project");
        std::fs::create_dir_all(project.join(".state")).unwrap();
        let db_path=project.join(".state/state.db");
        let mut db=SqliteStore::create(&db_path).unwrap();
        let now=jiff::Timestamp::now().as_millisecond();
        db.commit(Commit{expected_head:0,mutations:["pred","c"].into_iter().map(|id|Mutation::Task{expected:None,next:Task{id:TaskId::new(id).unwrap(),revision:1,state:TaskState::Draft,title:id.into(),active_attempt:None}}).collect()}).unwrap();
        let head=db.read_snapshot(None).unwrap().head;
        db.commit(Commit{expected_head:head,mutations:vec![Mutation::Attempt{expected:None,next:Attempt{id:AttemptId::new("attempt-pred").unwrap(),task:TaskId::new("pred").unwrap(),revision:1,state:AttemptState::Completed,snapshot:None,reservation:"slot-pred".into(),termination_observed:true}}]}).unwrap();
        let digest="d".repeat(64);let oid="a".repeat(40);let result_id="e".repeat(64);
        let installed:i64=rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT COALESCE(MAX(sequence),1) FROM events",[],|row|row.get(0)).unwrap();
        let raw=rusqlite::Connection::open(&db_path).unwrap();
        raw.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        raw.execute("INSERT INTO task_contracts(task_id,contract_revision,plan_revision,project_store,expected_head,repository,base_oid,object_format,memory_snapshot_id,route,raw_bytes,raw_digest,installed_seq) VALUES('pred',1,NULL,'/tmp/project',0,'/tmp/repo',?1,'sha1',NULL,'verify_only',?2,?3,?4)",rusqlite::params![oid,vec![b'x'],digest,installed]).unwrap();
        raw.execute("INSERT INTO acceptance_policies(task_id,contract_revision,policy_id,body) VALUES('pred',1,'policy','accept')",[]).unwrap();
        raw.execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,'/tmp/project','key-pred',?1,'{}','pred',1,?1,'attempt-pred','/tmp/repo',?2,?2,'sha1',NULL,'[]','[]',1)",rusqlite::params![digest,oid]).unwrap();
        raw.execute("INSERT INTO verification_runs(run_id,project_store,idempotency_key,payload_digest,submission_id,task_id,contract_revision,contract_digest,attempt_id,policy_id,policy_digest,commit_oid,tree_oid,object_format,memory_fence,isolation,argv,library_manifest,state,reason,exit_status,receipt_digest,store_device,store_inode,created_unix_ms) VALUES(?1,'/tmp/project',?1,?2,?2,'pred',1,?2,'attempt-pred','policy',?2,?3,?3,'sha1',0,'linux-unshare-user-pid-mount-v1','[]','[]','accepted',NULL,0,?2,0,0,1)",rusqlite::params![result_id,digest,oid]).unwrap();
        raw.execute("INSERT INTO verified_results(result_id,run_id,submission_id,commit_oid,tree_oid,object_format,policy_digest,receipt_digest,isolation,memory_fence,created_unix_ms) VALUES(?1,?1,?2,?3,?3,'sha1',?2,?2,'linux-unshare-user-pid-mount-v1',0,1)",rusqlite::params![result_id,digest,oid]).unwrap();
        drop(raw);
        let child=TaskId::new("c").unwrap();
        let snapshot=db.read_snapshot(None).unwrap();
        db.create_runtime(Some(&child),Some(snapshot.tasks.iter().find(|task|task.id==child).unwrap().revision),snapshot.head,&RuntimeRoute::default()).unwrap();
        let snapshot=db.read_snapshot(None).unwrap();
        let revision=snapshot.tasks.iter().find(|task|task.id==child).unwrap().revision;
        db.queue_task(&child,revision,snapshot.head,&QueueRequest{priority:0,dependencies:vec![Dependency{predecessor:TaskId::new("pred").unwrap(),requirement:DependencyRequirement::VerifiedResult}]},now).unwrap();
        let snapshot=db.read_snapshot(None).unwrap();
        db.set_scheduler_policy(snapshot.head,snapshot.scheduler.as_ref().unwrap().policy.revision,1,3).unwrap();
        let snapshot=db.read_snapshot(None).unwrap();
        let binding=snapshot.runtime_bindings.iter().find(|binding|binding.task.as_ref()==Some(&child)).unwrap();
        let task_revision=snapshot.tasks.iter().find(|task|task.id==child).unwrap().revision;
        db.record_observations(snapshot.head,&[RuntimeObservation{binding:binding.id.clone(),binding_revision:binding.revision,task_revision:Some(task_revision),observed_unix_ms:now,collector:"herdr-git-v1".into(),..RuntimeObservation::default()}]).unwrap();
        let snapshot=db.read_snapshot(None).unwrap();
        db.set_project_state(snapshot.head,snapshot.control.as_ref().unwrap().revision,ProjectState::Active,now,None).unwrap();
        let evidence=VersionedReference{id:"test-only-evidence".into(),revision:1,digest:"a".repeat(64)};
        let supported=CapabilityEvidence::Supported{evidence:evidence.clone()};
        let profile=FrozenProfile{version:1,name:"fixture".into(),kind:"claude".into(),definition_digest:"b".repeat(64),config:herdr_projects::migration::ConfigReference{path:"/no/such/admission-config.toml".into(),digest:None},arguments_digest:"c".repeat(64),environment_names:vec![],execution_home:None,permission_policy:evidence.clone(),adapter:evidence,agent:ExecutableIdentity{path:"/usr/bin/git".into(),digest:"d".repeat(64),version:"1.0.0".into()},herdr:ExecutableIdentity{path:"/usr/bin/git".into(),digest:"e".repeat(64),version:"0.9.1".into()},capabilities:ProfileCapabilities{launch:supported.clone(),readiness_observation:supported.clone(),prompt_submission:supported.clone(),stop:supported,checkpoint_acknowledgment:CapabilityEvidence::Unknown,structured_usage:CapabilityEvidence::Unknown,resume:CapabilityEvidence::Unknown},workflow_certificate:None};
        let reference=profile.reference().unwrap();
        let canonical=std::fs::canonicalize(&db_path).unwrap();
        let metadata=std::fs::metadata(&canonical).unwrap();
        let report=serde_json::json!({"preparation":{"profile":profile,"reference":reference,"launchable":true,"protocol_capable":false,"certified":false},"source_store":[canonical,metadata.dev(),metadata.ino()]});
        let text=serde_json::to_string(&report).unwrap();
        let report_digest=format!("{:x}",Sha256::digest(text.as_bytes()));
        let sequence:i64=rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT MAX(sequence) FROM events",[],|row|row.get(0)).unwrap();
        rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO native_profiles(profile_digest,report,report_digest,sequence) VALUES(?1,?2,?3,?4)",rusqlite::params![reference.digest,text,report_digest,sequence]).unwrap();
        let inputs=herdr_projects::admission::prepared_admission_inputs(&project).unwrap().expect("ready dependent");
        let grant=ApprovalGrant{version:1,scope:ApprovalScope::for_launch(&inputs).unwrap(),policy:inputs.effective_profile.as_ref().unwrap().permission_policy.clone(),issued_unix_ms:0,expires_unix_ms:now+3_600_000};
        let approval=grant.reference().unwrap();
        let payload=serde_json::to_vec(&grant).unwrap();
        rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO approval_grants(id,payload,payload_hash) VALUES(?1,?2,?3)",rusqlite::params![approval.id,String::from_utf8(payload).unwrap(),approval.digest]).unwrap();
        let attempts=|task:&str| rusqlite::Connection::open(&db_path).unwrap().query_row("SELECT count(*) FROM attempts WHERE task_id=?1",[task],|row|row.get::<_,i64>(0)).unwrap();
        let counts=|| -> Vec<(String,i64)> {
            let conn=rusqlite::Connection::open(&db_path).unwrap();
            let mut stmt=conn.prepare("SELECT task_id, count(*) FROM attempts GROUP BY task_id ORDER BY task_id").unwrap();
            stmt.query_map([],|row| Ok((row.get(0)?,row.get(1)?))).unwrap().map(|row| row.unwrap()).collect()
        };
        assert_eq!(attempts("c"),0);
        let home=tempfile::tempdir().unwrap();
        let env=crate::paths::Env::for_test(home.path(),&[]);
        let runner=crate::runner::RealRunner;
        let ctx=crate::paths::Ctx{env:&env,root:home.path().into(),config_dir:home.path().join("config"),runner:&runner,detached_ticker:false};
        let pool=Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),Arc::new(crate::runner::RealRunner)).unwrap());
        let mut reads=observations::Reads::new(pool.clone());
        poll_queued_effects(&ctx,&project,0,&mut reads,None).unwrap();
        assert_eq!(attempts("c"),0,"flag off must not reserve");
        let column=["factory","_admission"].concat();
        rusqlite::Connection::open(&db_path).unwrap().execute(&format!("UPDATE project_control SET {column}=?1 WHERE singleton=1"),["on"]).unwrap();
        let queued:Vec<String>={
            let conn=rusqlite::Connection::open(&db_path).unwrap();
            let mut stmt=conn.prepare("SELECT task_id FROM task_queue ORDER BY task_id").unwrap();
            stmt.query_map([],|row| row.get(0)).unwrap().map(|row| row.unwrap()).collect()
        };
        assert_eq!(queued,vec!["c".to_string()],"C must be the only queued dependent");
        let before=counts();
        assert_eq!(attempts("c"),0);
        let mut reads=observations::Reads::new(pool.clone());
        poll_queued_effects(&ctx,&project,1,&mut reads,None).unwrap();
        assert_eq!(attempts("c"),1,"one poll_queued_effects wake reserves C");
        let mut expected=before;
        expected.push(("c".into(),1));
        expected.sort();
        assert_eq!(counts(),expected,"no other task gains an attempt");
        let old=jiff::Timestamp::now().as_millisecond()-16*60*1000;
        rusqlite::Connection::open(&db_path).unwrap().execute("INSERT INTO result_submissions(submission_id,project_store,idempotency_key,payload_digest,payload,task_id,contract_revision,contract_digest,attempt_id,repository,base_oid,candidate_oid,object_format,memory_snapshot_id,artifact_manifest,claimed_checks,created_unix_ms) VALUES(?1,'/tmp/project','key-old',?2,'{}','pred',1,?2,'attempt-pred','/tmp/repo',?3,?3,'sha1',NULL,'[]','[]',?4)",rusqlite::params!["f".repeat(64),digest,oid,old]).unwrap();
        let mut reads=observations::Reads::new(pool.clone());
        let again=poll_queued_effects(&ctx,&project,2,&mut reads,None).unwrap();
        let note=again.operation_error.unwrap_or_default();
        assert!(note.contains("capacity_full: verification_backlog"),"{note}");
        assert_eq!(attempts("c"),1,"backlog is diagnostic and must not reserve another attempt");
        assert_eq!(counts(),expected,"backlog does not add an attempt");
        assert!(pool.stop(std::time::Duration::from_secs(2)));
    }

}

#[cfg(all(test,target_os="linux"))]
#[path="canonical_controller_launch_tests.rs"]
mod launch_tests;
