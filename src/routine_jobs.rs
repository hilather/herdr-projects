//! Routine jobs share the bounded command pool. The marker is never an OS
//! executable, and pool output is never accepted as durable cleanup authority.
use std::{path::Path,sync::Arc,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use serde::{Deserialize,Serialize};
use crate::{executor::{Identity,Lane,Request},runner::{Cmd,Output,Runner}};
use herdr_projects::domain::OperationId;

const JOB:&str="\0herdr-projects-routine-execution";
const BUDGET:Duration=Duration::from_secs(95);
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {project:String,operation:OperationId,revision:u64}

pub struct JobRunner {pub inner:Arc<dyn Runner+Send+Sync>}
impl Runner for JobRunner {
    fn run(&self,cmd:&Cmd)->Result<Output> {
        if cmd.program!=JOB {return self.inner.run(cmd);}
        let entered=Instant::now();
        ensure!(!cmd.timeout.is_zero()&&cmd.timeout<=BUDGET,"invalid routine queue budget");
        let deadline=cmd.deadline.context("routine absolute queue deadline missing")?.min(entered+cmd.timeout);
        let input=cmd.stdin.as_deref().context("routine job input missing")?;
        ensure!(input.len()<=8192,"routine job input exceeds bound");
        let input:Input=serde_json::from_str(input)?;
        ensure!(input.project.len()<=4096&&Path::new(&input.project).is_absolute()&&input.revision>0,"invalid routine job identity");
        let cancellation=cmd.cancellation.clone().context("routine job cancellation missing")?;
        // This trusted service uses concrete RealRunner and commits the claim,
        // output and sealed cleanup receipt itself. An injected observation
        // Runner cannot mint a successful routine receipt.
        let receipt=herdr_projects::routines::execute_queued(Path::new(&input.project),&input.operation,input.revision,cancellation,deadline)?;
        let stdout=serde_json::to_string(&receipt)?;
        ensure!(stdout.len()<=crate::runner::CAPTURE_LIMIT,"routine result exceeds executor capture bound");
        Ok(Output{code:Some(0),stdout_bytes:stdout.as_bytes().to_vec(),stdout_total_bytes:stdout.len() as u64,stdout,elapsed:entered.elapsed(),..Output::default()})
    }
    fn socket_request(&self,socket:&Path,line:&str,timeout:Duration)->Result<String> {self.inner.socket_request(socket,line,timeout)}
}

pub fn request(project:&Path,operation:&OperationId,revision:u64)->Result<Request> {
    let project=project.canonicalize()?;
    let root=project.parent().context("routine project has no root")?;
    let identity=Identity{operation:format!("routine-execute:{}",operation.as_str()),revision,
        project:project.to_str().context("routine project is not UTF-8")?.into(),
        // Retained root execution ownership permits only one routine per root.
        // Project-scoped ownership is required before relaxing this constraint.
        machine:format!("routine-root:{}",root.display()),terminal:None};
    let input=Input{project:identity.project.clone(),operation:operation.clone(),revision};
    let deadline=Instant::now()+BUDGET;
    let mut command=Cmd::new(JOB,BUDGET).stdin(serde_json::to_string(&input)?);command.deadline=Some(deadline);
    Ok(Request{identity,lane:Lane::Transfer,deadline,command})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{Executor,Limits};
    use herdr_projects::{runtime,routines,domain::TaskId,operations::DeliveryState};
    fn fixture(script:&[u8],deadline:u64)->(crate::scenarios::World,std::path::PathBuf,OperationId) {
        let(world,path)=crate::canonical_controller::tests::routine_fixture(&[("check",script,deadline)]);
        let occurrence=routines::schedule(&path,"check",runtime::snapshot(&path).unwrap().head).unwrap().unwrap();
        (world,path,occurrence.operation.unwrap())
    }
    #[cfg(target_os="linux")]
    #[test]
    fn queued_ingress_rechecks_revision_budget_and_authority_without_pinning_event_head() {
        let(_world,path,id)=fixture(b"printf once >> marker",1000);let before=runtime::snapshot(&path).unwrap();
        for (revision,budget) in [(2,95),(1,1)] {
            let runner=JobRunner{inner:Arc::new(Forbidden)};let mut cmd=request(&path,&id,revision).unwrap().command;
            cmd.timeout=Duration::from_secs(budget);cmd.cancellation=Some(crate::runner::Cancellation::default());
            assert!(runner.run(&cmd).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        }
        let runner=JobRunner{inner:Arc::new(Forbidden)};let mut expired=request(&path,&id,1).unwrap().command;
        expired.deadline=Some(Instant::now());expired.cancellation=Some(crate::runner::Cancellation::default());
        assert!(runner.run(&expired).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        std::fs::write(path.join("check.sh"),b"touch changed").unwrap();
        let runner=JobRunner{inner:Arc::new(Forbidden)};let mut cmd=request(&path,&id,1).unwrap().command;cmd.cancellation=Some(crate::runner::Cancellation::default());
        assert!(runner.run(&cmd).is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        std::fs::write(path.join("check.sh"),b"printf once >> marker").unwrap();
        let work=request(&path,&id,1).unwrap();
        runtime::add_task(&path,TaskId::new("unrelated").unwrap(),"unrelated event".into(),before.head).unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        let completion=pool.submit(work).unwrap().recv_timeout(Duration::from_secs(3)).unwrap();assert!(completion.result.unwrap().success());
        let after=runtime::snapshot(&path).unwrap();assert!(after.routine_receipts[0].cleanup_verified);
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert_eq!(runtime::snapshot(&path).unwrap(),after);assert_eq!(std::fs::read(path.join("marker")).unwrap(),b"once");
        assert!(pool.stop(Duration::from_secs(2)));
    }
    #[cfg(target_os="linux")]
    #[test]
    fn running_job_preserves_control_lane_and_cancellation_drains_without_certifying_cleanup() {
        let(_world,path,id)=fixture(b"touch started; setsid /bin/sh -c 'sleep 0.8; touch escaped' >/dev/null 2>&1 & sleep 10",10_000);
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(crate::runner::RealRunner)})).unwrap();
        let ticket=pool.submit(request(&path,&id,1).unwrap()).unwrap();let deadline=Instant::now()+Duration::from_secs(3);
        while !path.join("started").exists() {assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));}
        let before=runtime::snapshot(&path).unwrap();assert_eq!(before.deliveries[0].state,DeliveryState::Claimed);
        assert!(runtime::add_task(&path,TaskId::new("blocked").unwrap(),"mutation remains fenced".into(),before.head).is_err());
        let control=Request{identity:Identity{operation:"independent-read".into(),revision:1,project:"other-project".into(),machine:"local".into(),terminal:None},lane:Lane::Control,deadline:Instant::now()+Duration::from_secs(1),command:Cmd::new("/bin/true",Duration::from_secs(1))};
        assert!(pool.submit(control).unwrap().recv_timeout(Duration::from_secs(1)).unwrap().result.unwrap().success());
        assert!(pool.stop(Duration::from_secs(2)));assert!(ticket.recv_timeout(Duration::from_secs(1)).unwrap().result.is_ok());
        let after=runtime::snapshot(&path).unwrap();assert_eq!(after.deliveries[0].state,DeliveryState::Ambiguous);assert!(!after.routine_receipts[0].cleanup_verified);
        std::thread::sleep(Duration::from_millis(900));assert!(!path.join("escaped").exists());
    }
    #[cfg(target_os="linux")]
    #[test]
    fn receipt_commit_failure_survives_pool_restart_without_replaying_effect() {
        let(_world,path,id)=fixture(b"printf once >> marker",1000);
        let raw=rusqlite::Connection::open(path.join(".state/state.db")).unwrap();raw.execute_batch("CREATE TRIGGER fail_routine_receipt BEFORE INSERT ON events WHEN NEW.kind='routine.completed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert!(pool.stop(Duration::from_secs(2)));drop(pool);raw.execute_batch("DROP TRIGGER fail_routine_receipt;").unwrap();
        let before=runtime::snapshot(&path).unwrap();assert_eq!(before.deliveries[0].state,DeliveryState::Claimed);assert!(before.routine_receipts.is_empty());
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        assert!(pool.submit(request(&path,&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(3)).unwrap().result.is_err());
        assert_eq!(runtime::snapshot(&path).unwrap(),before);assert_eq!(std::fs::read(path.join("marker")).unwrap(),b"once");assert!(pool.stop(Duration::from_secs(2)));
    }
    struct Forbidden;
    impl Runner for Forbidden {
        fn run(&self,_:&Cmd)->Result<Output> {panic!("routine must not use the injectable observation runner")}
        fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
    }
    #[cfg(target_os="linux")]
    #[test]
    fn script_withdrawn_while_waiting_in_pool_is_never_claimed() {
        use std::sync::{mpsc,Mutex};
        struct Gate {started:mpsc::Sender<()>,release:Mutex<mpsc::Receiver<()>>}
        impl Runner for Gate {
            fn run(&self,cmd:&Cmd)->Result<Output> {assert_eq!(cmd.program,"fixture-gate");self.started.send(()).unwrap();self.release.lock().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();Ok(Output{code:Some(0),..Output::default()})}
            fn socket_request(&self,_:&Path,_:&str,_:Duration)->Result<String>{unreachable!()}
        }
        let(_world,path,id)=fixture(b"touch marker",1000);let before=runtime::snapshot(&path).unwrap();
        let(started,ready)=mpsc::channel();let(release,gate)=mpsc::channel();let mut limits=Limits::default();limits.workers=[1,1];
        let pool=Executor::new(limits,Arc::new(JobRunner{inner:Arc::new(Gate{started,release:Mutex::new(gate)})})).unwrap();
        let blocker=Request{identity:Identity{operation:"gate".into(),revision:1,project:"other-project".into(),machine:"fixture".into(),terminal:None},lane:Lane::Transfer,deadline:Instant::now()+Duration::from_secs(3),command:Cmd::new("fixture-gate",Duration::from_secs(3))};
        let blocker=pool.submit(blocker).unwrap();ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let queued=pool.submit(request(&path,&id,1).unwrap()).unwrap();assert_eq!(pool.metrics().queued[1],1);
        std::fs::write(path.join("check.sh"),b"touch unauthorized").unwrap();release.send(()).unwrap();
        assert!(blocker.recv_timeout(Duration::from_secs(1)).unwrap().result.is_ok());
        assert!(queued.recv_timeout(Duration::from_secs(2)).unwrap().result.is_err());assert_eq!(runtime::snapshot(&path).unwrap(),before);
        assert!(!path.join("marker").exists()&&!path.join("unauthorized").exists());assert!(pool.stop(Duration::from_secs(1)));
    }
    #[test]
    fn invalid_or_cancelled_job_cannot_enter_an_external_runner() {
        let dir=tempfile::tempdir().unwrap();
        let pool=Executor::new(Limits::default(),Arc::new(JobRunner{inner:Arc::new(Forbidden)})).unwrap();
        let id=OperationId::new("missing-operation").unwrap();
        let mut work=request(dir.path(),&id,1).unwrap();let cancel=crate::runner::Cancellation::default();cancel.cancel();work.command.cancellation=Some(cancel);
        let completion=pool.submit(work).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!completion.runner_entered&&completion.result.unwrap().cancelled);
        let completion=pool.submit(request(dir.path(),&id,1).unwrap()).unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.runner_entered&&completion.result.is_err());
        assert!(!dir.path().join(".state/state.db").exists());assert!(pool.stop(Duration::from_secs(2)));
    }
}
