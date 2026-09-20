//! Durable brief delivery claims. A lost response never authorizes replay.
use super::*;
use herdr_projects::execution_guard::ProjectGuard;
use crate::source_tree::Control;
pub use herdr_projects::prompt_claim::{Claim,Phase};
pub fn validate(t:&Thread)->Result<()> {
    anyhow::ensure!(t.prompt_sequence<=i64::MAX as u64,"brief sequence exhausted");
    if let Some(claim)=&t.prompt_claim {claim.validate(t.prompt_sequence)?;if claim.phase==Phase::Pending {anyhow::ensure!(t.pending_live_copy.is_none()&&t.pending_final_copy.is_none(),"pending brief conflicts with copy projection");}}
    Ok(())
}
pub fn ready(t:&Thread)->Result<()> {
    validate(t)?;
    anyhow::ensure!(t.status==Status::Open&&t.prompt_pending&&t.removal.is_none()&&t.pending_live_copy.is_none()&&t.pending_final_copy.is_none(),"thread is not eligible for a brief prompt");
    if let Some(claim)=&t.prompt_claim {
        anyhow::ensure!(claim.phase!=Phase::Pending&&claim.notified&&claim.execution!=execution_fingerprint(t),"brief delivery already claimed; reconcile before retrying");
    }
    Ok(())
}
#[allow(dead_code)] // Worker admission must use this before the external prompt.
pub fn claim(project:&Project,guard:&ProjectGuard,expected:&Thread,prompt:&str,control:&Control)->Result<Claim> {
    control.check()?;guard.check_project(&project.dir())?;project::ensure_legacy(&project.dir())?;ready(expected)?;
    let claim=Claim{sequence:expected.prompt_sequence.checked_add(1).context("brief sequence exhausted")?,execution:execution_fingerprint(expected),prompt:prompt.into(),phase:Phase::Pending,error:String::new(),notified:false};claim.validate(claim.sequence)?;
    update_checked(project,&expected.id,|current|{
        control.check()?;anyhow::ensure!(project.try_status()?==project::Status::Active,"project is not active");ready(current)?;
        anyhow::ensure!(execution_fingerprint(current)==claim.execution&&current.prompt_sequence==expected.prompt_sequence&&current.prompt_claim==expected.prompt_claim,"thread changed before brief claim");
        current.prompt_sequence=claim.sequence;current.prompt_claim=Some(claim.clone());Ok(())
    })?;
    std::fs::File::open(project.dir().join("threads"))?.sync_all()?;Ok(claim)
}
#[allow(dead_code)] // Only the concrete worker may supply confirmed delivery.
pub fn confirm(project:&Project,guard:&ProjectGuard,id:&str,claim:&Claim,control:&Control)->Result<()> {
    control.check()?;guard.check_project(&project.dir())?;anyhow::ensure!(claim.phase==Phase::Pending,"brief claim already completed");
    update_checked(project,id,|current|{
        control.check()?;validate(current)?;
        anyhow::ensure!(current.prompt_claim.as_ref()==Some(claim)&&execution_fingerprint(current)==claim.execution&&current.status==Status::Open&&current.prompt_pending&&current.pending_live_copy.is_none()&&current.pending_final_copy.is_none(),"thread changed during brief delivery");
        let mut confirmed=claim.clone();confirmed.phase=Phase::Confirmed;confirmed.notified=true;current.prompt_claim=Some(confirmed);current.prompt_pending=false;Ok(())
    })?;
    std::fs::File::open(project.dir().join("threads"))?.sync_all()?;Ok(())
}
/// Run only while the caller excludes active project workers. A surviving
/// supervisor retains those locks until the possibly-issued prompt has exited.
pub fn recover(project:&Project,guard:&ProjectGuard)->Result<()> {
    guard.check_project(&project.dir())?;
    let (records,diagnostics)=list_with_diagnostics(project);anyhow::ensure!(diagnostics.is_empty(),"invalid thread inventory; brief recovery refused");
    for record in records {
        validate(&record)?;let Some(mut claim)=record.prompt_claim else {continue;};
        if claim.phase==Phase::Pending {
            let pending=claim.clone();claim.phase=Phase::Uncertain;claim.error="Brief delivery was interrupted; the prompt may already have been submitted. Inspect the agent before explicitly restarting this thread.".into();
            update_checked(project,&record.id,|current|{
                anyhow::ensure!(current.prompt_claim.as_ref()==Some(&pending),"brief claim changed during recovery");
                if execution_fingerprint(current)==claim.execution&&current.status==Status::Open {current.status=Status::Failed;current.error=claim.error.clone();}
                current.prompt_claim=Some(claim.clone());Ok(())
            })?;
            std::fs::File::open(project.dir().join("threads"))?.sync_all()?;
        }
        if claim.phase==Phase::Uncertain&&!claim.notified {
            let id=format!("brief-{}-{}-{}",record.id,claim.execution,claim.sequence);
            crate::inbox::write_once(project,&id,"thread-state",&record.id,&format!("{} brief delivery needs reconciliation",record.id),&claim.error)?;
            std::fs::File::open(project.dir().join("inbox"))?.sync_all()?;
            update_checked(project,&record.id,|current|{anyhow::ensure!(current.prompt_claim.as_ref()==Some(&claim),"brief claim changed before notice acknowledgement");current.prompt_claim.as_mut().unwrap().notified=true;Ok(())})?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,Project,Thread) {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let t=allocate(&project,|t|{t.status=Status::Open;t.prompt_pending=true;}).unwrap();(root,project,t)
    }
    #[test]
    fn durable_claim_blocks_replay_and_confirmation_clears_only_matching_brief() {
        let(_root,project,t)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();let control=Control::default();
        let pending=claim(&project,&guard,&t,"read the brief",&control).unwrap();let current=load(&project,&t.id).unwrap();
        assert_eq!(current.prompt_claim.as_ref(),Some(&pending));assert!(current.prompt_pending);assert!(ready(&current).is_err());assert!(copy_delivery::ready(&current).is_err());
        assert!(claim(&project,&guard,&current,"read the brief",&control).is_err());
        confirm(&project,&guard,&t.id,&pending,&control).unwrap();let confirmed=load(&project,&t.id).unwrap();assert!(!confirmed.prompt_pending);assert_eq!(confirmed.prompt_claim.as_ref().unwrap().phase,Phase::Confirmed);
        assert!(confirm(&project,&guard,&t.id,&pending,&control).is_err());recover(&project,&guard).unwrap();assert_eq!(load(&project,&t.id).unwrap(),confirmed);
    }
    #[test]
    fn lost_claim_becomes_uncertain_and_handled_notice_does_not_duplicate() {
        let(_root,project,t)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();let pending=claim(&project,&guard,&t,"read the brief",&Control::default()).unwrap();
        let id=format!("brief-{}-{}-{}",t.id,pending.execution,pending.sequence);
        // Replay of a notice already delivered and handled before acknowledgement.
        recover(&project,&guard).unwrap();crate::inbox::done(&project,&[id.clone()],false).unwrap();
        update(&project,&t.id,|t|t.prompt_claim.as_mut().unwrap().notified=false).unwrap();recover(&project,&guard).unwrap();
        let current=load(&project,&t.id).unwrap();assert_eq!(current.status,Status::Failed);assert!(current.prompt_pending);assert!(current.prompt_claim.as_ref().unwrap().notified);assert!(ready(&current).is_err());
        assert!(!project.dir().join("inbox").join(format!("{id}.md")).exists());assert!(project.dir().join("inbox/done").join(format!("{id}.md")).exists());
        assert!(current.error.contains("may already have been submitted"));
    }
    #[test]
    fn historical_uncertainty_does_not_fail_replacement_and_restart_can_claim_again() {
        let(_root,project,t)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();let pending=claim(&project,&guard,&t,"old brief",&Control::default()).unwrap();
        let replacement=update(&project,&t.id,|t|{t.lifecycle_generation+=1;t.pane_id="replacement".into();}).unwrap();
        assert!(confirm(&project,&guard,&t.id,&pending,&Control::default()).is_err());recover(&project,&guard).unwrap();let current=load(&project,&t.id).unwrap();
        assert_eq!(current.status,Status::Open);assert_eq!(current.pane_id,replacement.pane_id);assert!(current.prompt_pending);assert!(current.error.is_empty());assert!(ready(&current).is_ok());
        let next=claim(&project,&guard,&current,"new brief",&Control::default()).unwrap();assert_eq!(next.sequence,2);assert_ne!(next.execution,pending.execution);
    }
    #[test]
    fn stale_cancelled_and_copy_owned_threads_cannot_claim_or_acknowledge() {
        let(_root,project,t)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();let control=Control::default();control.cancellation.cancel();
        assert!(claim(&project,&guard,&t,"brief",&control).is_err());assert_eq!(load(&project,&t.id).unwrap(),t);
        update(&project,&t.id,|t|t.lifecycle_generation+=1).unwrap();assert!(claim(&project,&guard,&t,"brief",&Control::default()).is_err());
        let mut current=load(&project,&t.id).unwrap();current.pending_live_copy=Some(herdr_projects::live_copy_intent::LiveCopyIntent{sequence:1,execution:"a".repeat(64),authority:"a".repeat(64),previous_hash:String::new(),previous_receipt:None,report_hash:"a".repeat(64),stage_digest:"a".repeat(64)});assert!(ready(&current).is_err());
        let current=load(&project,&t.id).unwrap();let pending=claim(&project,&guard,&current,"brief",&Control::default()).unwrap();assert!(confirm(&project,&guard,&t.id,&pending,&control).is_err());assert_eq!(load(&project,&t.id).unwrap().prompt_claim.as_ref(),Some(&pending));
    }
    #[test]
    fn ticker_recovers_lost_brief_before_an_unreachable_session() {
        let(root,project,t)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();claim(&project,&guard,&t,"brief",&Control::default()).unwrap();drop(guard);
        let env=crate::paths::Env::for_test(root.path(),&[]);let runner=crate::runner::RealRunner;let ctx=crate::paths::Ctx{env:&env,root:root.path().into(),config_dir:root.path().join("cfg"),runner:&runner,detached_ticker:false};
        let pool=std::sync::Arc::new(crate::executor::Executor::new(crate::executor::Limits::default(),std::sync::Arc::new(crate::runner::RealRunner)).unwrap());
        let mut memory=crate::steps::Memory::new(&ctx);memory.copy_jobs=Some(crate::copy_jobs::Queue::new(pool.clone()));
        crate::ticker::tick_for_test(&ctx,&mut memory);let current=load(&project,&t.id).unwrap();assert_eq!(current.status,Status::Failed);assert!(current.prompt_claim.as_ref().unwrap().notified);assert!(pool.stop(std::time::Duration::from_secs(1)));
    }
}
