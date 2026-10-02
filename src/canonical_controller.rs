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
    let now=jiff::Timestamp::now().as_millisecond();
    match herdr_projects::store::HOT_PATH_READ {
        herdr_projects::store::HotPathRead::Targeted=>{
            let control=herdr_projects::store::controlled::ReadControl::new(
                std::time::Instant::now()+std::time::Duration::from_secs(2),Default::default());
            let db=herdr_projects::migration::open_active_scoped(&path,control)?;
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
            let mut db=herdr_projects::migration::open_active(&path)?;
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
    let control=herdr_projects::store::controlled::ReadControl::new(budget.deadline,Default::default());
    let probe_ctx=Ctx{env:ctx.env,root:ctx.root.clone(),config_dir:ctx.config_dir.clone(),runner:&budget,detached_ticker:ctx.detached_ticker};
    let batch=crate::reconcile_live::collect_controlled(&probe_ctx,&path,&control)?;
    ensure!(std::time::Instant::now()<budget.deadline,"automatic observation budget exhausted; use explicit reconciliation to investigate");
    let reachable=batch.observations.iter().any(|o|o.pane==ResourceState::Present||o.worktree==ResourceState::Present);
    runtime::record_controller_observations_controlled(&path,&batch,&ownership,&control)?;
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
    let mut errors=observation_error.into_iter().collect::<Vec<_>>();
    // Each service below takes project ownership, which shares the root
    // barrier. While a root-exclusive effect (launch, brief, termination) is
    // admitted it owns the root until the next pass drains it: it takes the
    // exclusive root at start and again between its stages, and a service
    // sharing the root at that moment makes it fail and back off. Under load
    // the next pass starts as soon as the effect is admitted, so every retry
    // lost that race. Defer the services to the pass after the drain; they
    // report pending work so the ticker stays awake.
    let root_owned=effects.as_ref().is_some_and(|queue|queue.pending_exclusive_root());
    let mut service=|name:&str,run:&dyn Fn(&Path)->anyhow::Result<bool>|->bool {
        if root_owned {return true;}
        match run(path) {Ok(pending)=>pending,Err(error)=>{errors.push(format!("{name}: {error:#}"));true}}
    };
    let stop_work=service("barrier stop service",&|path|Ok(herdr_projects::store::service_project_barrier_stops(path)?.pending));
    let (admission_log,result)=process_next_with_launches(ctx,&path,turn,effects,launch_dispatch_enabled());
    let wait_work=service("wait service",&|path|Ok(herdr_projects::store::service_project_waits(path)?.pending));
    let replan_work=service("replan request service",&|path|Ok(herdr_projects::store::service_project_replans(path)?.pending));
    let verification_work=service("verification job service",&|path|Ok(herdr_projects::store::service_project_verification_jobs(path)?.pending));
    let integration_work=service("integration job service",&|path|Ok(herdr_projects::store::service_project_integration_jobs(path)?.pending));
    let completion_work=if root_owned {true} else {
        match herdr_projects::store::service_project_result_completions(path) {
            Ok((pending,diagnostic))=>{if let Some(reason)=diagnostic {errors.push(format!("result completion service: {reason}"));}pending},
            Err(error)=>{errors.push(format!("result completion service: {error:#}"));true},
        }
    };
    let routine_work=match scheduled {Ok(report)=>{if let Some(error)=report.diagnostic {errors.push(format!("routine scheduling: {error}"));}report.active},Err(error)=>{errors.push(format!("routine scheduling: {error:#}"));false}};
    // An admission failure is diagnostic only. Already-prepared dispatch still runs.
    let (progress,unknown_effects)=match result {
        Ok((progress,admission))=>{if let Some(error)=admission {errors.push(format!("admission: {error}"));}(progress,false)}
        Err(error)=>{errors.push(format!("{error:#}"));(false,queued)}
    };
    Ok(PollResult{reachable:reachable||progress,scheduled_work:routine_work||wait_work||stop_work||replan_work||verification_work||integration_work||completion_work,unknown_effects,operation_error:(!errors.is_empty()).then(||errors.join("; ")),admission_log})
}
fn process_next(ctx:&Ctx,path:&Path,turn:u64,effects:Option<&mut crate::copy_jobs::Queue>)->Result<bool> {
    Ok(process_next_with_launches(ctx,path,turn,effects,launch_dispatch_enabled()).1?.0)
}
#[cfg(target_os="linux")]
fn linux_admission(path:&Path)->(Option<String>,Option<String>) {
    if herdr_projects::watchdog::is_paused(path) {
        let reason=herdr_projects::watchdog::pause_reason(path).unwrap_or("admission_paused");
        let line=herdr_projects::watchdog::admission_log_line(reason,None,0);
        return (Some(format!("admission_paused: {reason}")),Some(line));
    }
    if !herdr_projects::admission::wake_enabled(path) {return (None,None);}
    // Ok(Some) is backpressure, not success. Prepared dispatch still runs after this note.
    let observation=herdr_projects::admission::admit_decision_observed(path);
    match observation.result {
        Ok(decision)=>{
            let line=herdr_projects::watchdog::admission_log_line_observed(decision.reason,decision.task_id.as_deref(),u128::from(observation.duration_ms),Some(observation.sql));
            let diagnostic=decision.block.map(|block| format!("{}: {}", block.blocker, block.reason));
            (diagnostic,Some(line))
        }
        Err(error)=>{
            let reason=error.downcast_ref::<herdr_projects::store::StoreError>().and_then(herdr_projects::watchdog::cause).unwrap_or("error");
            if let Some(store)=error.downcast_ref::<herdr_projects::store::StoreError>() {
                let _=herdr_projects::watchdog::note(path,store);
            }
            let line=herdr_projects::watchdog::admission_log_line_observed(reason,None,u128::from(observation.duration_ms),Some(observation.sql));
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
    if effect_held(path,&operation.kind) {return Ok(false);}
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
    // Verification runs only in the supervised verifier lane of the background ticker.
    if operation.kind=="verification.run"||operation.kind=="integration.run" {return Ok(false);}
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

/// Queued effect jobs are held while an integrity failure stands.
fn effect_held(path:&Path,kind:&str)->bool {
    matches!(kind,"verification.run"|"integration.run"|"runtime.finalization"|"runtime.notification")&&herdr_projects::watchdog::effects_paused(path).is_some()
}
// A hint selects work only. Concrete workers retain full validation before
// claiming or acting; a route hint does not certify provenance or authority.
fn offer_next(ctx:&Ctx,path:&Path,turn:u64,effects:&mut crate::copy_jobs::Queue,include_launches:bool)->Result<bool> {
    use herdr_projects::store::{identity_inventory::Budget,controller_hint::EffectMode};
    let mut budget=Budget::new(2*1024*1024,1024,std::time::Instant::now()+std::time::Duration::from_millis(100),Default::default())?;
    let Some(hint)=herdr_projects::migration::read_controller_dispatch_hint(path,&mut budget,turn,jiff::Timestamp::now().as_millisecond(),include_launches)? else{return Ok(false);};
    if effect_held(path,&hint.operation.kind) {return Ok(false);}
    match hint.operation.kind.as_str() {
        "runtime.notification"=>effects.offer_canonical_notification(ctx,path,&hint.operation,hint.delivery_revision,hint.notification_socket.as_deref().context("notification route hint missing")?)?,
        "runtime.finalization"=>effects.offer_canonical_finalization(ctx,path,&hint.operation,hint.delivery_revision,match hint.mode {EffectMode::Deliver=>crate::canonical_finalization_jobs::Mode::Deliver,EffectMode::Observe=>crate::canonical_finalization_jobs::Mode::Observe})?,
        "runtime.launch" if hint.mode==EffectMode::Deliver=>effects.offer_canonical_launch(path,&hint.operation,hint.delivery_revision)?,
        "runtime.worker_brief"|"runtime.worker_termination"|"runtime.worker_brief_prepare"|"runtime.launch"=>effects.offer_canonical_brief(path,&hint.operation,hint.delivery_revision)?,
        #[cfg(target_os="linux")]
        "verification.run"=>effects.offer_canonical_verification(path,&hint.operation,hint.delivery_revision,match hint.mode {EffectMode::Deliver=>crate::canonical_verification_jobs::Mode::Deliver,EffectMode::Observe=>crate::canonical_verification_jobs::Mode::Observe})?,
        #[cfg(target_os="linux")]
        "integration.run"=>effects.offer_canonical_integration(path,&hint.operation,hint.delivery_revision,match hint.mode {EffectMode::Deliver=>crate::canonical_verification_jobs::Mode::Deliver,EffectMode::Observe=>crate::canonical_verification_jobs::Mode::Observe})?,
        _=>anyhow::bail!("unsupported controller effect hint"),
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{notification_delivery,finalization_delivery};
    use herdr_projects::operations::DeliveryState;
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
    fn controller_expires_claims_without_replaying_ambiguous_notifications() {
        let(world,path,task)=notification_delivery::tests::fixture();let op=notification_delivery::enqueue(&world.ctx(),&path,&task,runtime::snapshot(&path).unwrap().head).unwrap();let mut db=migration::open_active(&path).unwrap();let now=jiff::Timestamp::now().as_millisecond();db.claim_operation(&op.id,1,"previous-controller",now,1).unwrap();std::thread::sleep(std::time::Duration::from_millis(3));
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();
        raw.execute("INSERT INTO tasks VALUES('retired/invalid',1,'succeeded','cold history',NULL)",[]).unwrap();
        assert!(runtime::snapshot(&path).is_err());
        poll(&world.ctx(),&path,0).unwrap();
        assert_eq!(raw.query_row("SELECT state FROM operation_delivery WHERE operation_id=?1",[op.id.as_str()],|r|r.get::<_,String>(0)).unwrap(),"ambiguous");
        poll(&world.ctx(),&path,0).unwrap();assert_eq!(world.runner.count("notification show"),0);
        assert!(runtime::snapshot(&path).is_err());
        raw.execute("DELETE FROM tasks WHERE id='retired/invalid'",[]).unwrap();
        assert_eq!(runtime::snapshot(&path).unwrap().deliveries[0].state,DeliveryState::Ambiguous);
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

}

#[cfg(all(test,target_os="linux"))]
#[path="canonical_controller_launch_tests.rs"]
mod launch_tests;
