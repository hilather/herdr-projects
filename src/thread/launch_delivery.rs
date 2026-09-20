//! One durable launch claim per execution; missing acknowledgement never retries.
use super::*;
use herdr_projects::execution_guard::ProjectGuard;
use crate::source_tree::Control;
pub use herdr_projects::launch_claim::{Claim,Phase};
pub fn validate(t:&Thread)->Result<()> {
    anyhow::ensure!(t.launch_sequence<=i64::MAX as u64,"launch sequence exhausted");
    if let Some(claim)=&t.launch_claim {claim.validate(t.launch_sequence)?;if claim.phase==Phase::Pending {anyhow::ensure!(t.pending_live_copy.is_none()&&t.pending_final_copy.is_none()&&t.prompt_claim.as_ref().is_none_or(|c|c.phase!=Phase::Pending),"pending launch conflicts with another effect");}}
    Ok(())
}
pub const RECONCILIATION_NOTE:&str="start acknowledged; no agent observed, inspect the pane before explicitly restarting";
pub fn needs_reconciliation(t:&Thread,live:&Live)->bool {
    t.status==Status::Open&&t.prompt_pending&&live.agent_state.is_none()&&t.launch_claim.as_ref().is_some_and(|c|c.phase==Phase::Confirmed&&c.generation==t.lifecycle_generation)
}
pub fn ready(t:&Thread)->Result<()> {
    super::prompt_delivery::ready(t)?;
    anyhow::ensure!(t.launch_attempts<MAX_LAUNCH_ATTEMPTS,"launch attempt limit reached");
    if let Some(claim)=&t.launch_claim {anyhow::ensure!(claim.phase!=Phase::Pending&&claim.notified&&claim.generation!=t.lifecycle_generation,"launch already claimed; inspect the agent before explicitly restarting");}Ok(())
}
pub fn claim(project:&Project,guard:&ProjectGuard,expected:&Thread,arguments:&[String],route_digest:&str,terminal:&str,control:&Control)->Result<Claim> {
    control.check()?;guard.check_project(&project.dir())?;project::ensure_legacy(&project.dir())?;ready(expected)?;
    let claim=Claim{generation:expected.lifecycle_generation,sequence:expected.launch_sequence.checked_add(1).context("launch sequence exhausted")?,execution:execution_fingerprint(expected),arguments_digest:sha256_hex(&serde_json::to_vec(arguments)?),route_digest:route_digest.into(),terminal:terminal.into(),phase:Phase::Pending,error:String::new(),notified:false};claim.validate(claim.sequence)?;
    update_checked(project,&expected.id,|current|{control.check()?;anyhow::ensure!(project.try_status()?==project::Status::Active,"project is not active");ready(current)?;
        anyhow::ensure!(execution_fingerprint(current)==claim.execution&&current.launch_sequence==expected.launch_sequence&&current.launch_claim==expected.launch_claim&&current.launch_attempts==expected.launch_attempts,"thread changed before launch claim");
        current.launch_attempts=current.launch_attempts.checked_add(1).context("launch attempts exhausted")?;current.launch_sequence=claim.sequence;current.launch_claim=Some(claim.clone());Ok(())})?;
    std::fs::File::open(project.dir().join("threads"))?.sync_all()?;Ok(claim)
}
pub fn confirm(project:&Project,guard:&ProjectGuard,id:&str,claim:&Claim,control:&Control)->Result<()> {
    control.check()?;guard.check_project(&project.dir())?;anyhow::ensure!(claim.phase==Phase::Pending,"launch claim already completed");
    update_checked(project,id,|current|{control.check()?;validate(current)?;
        anyhow::ensure!(current.launch_claim.as_ref()==Some(claim)&&execution_fingerprint(current)==claim.execution&&current.status==Status::Open&&current.prompt_pending&&current.removal.is_none()&&current.pending_live_copy.is_none()&&current.pending_final_copy.is_none(),"thread changed during launch");
        let mut confirmed=claim.clone();confirmed.phase=Phase::Confirmed;confirmed.notified=true;current.launch_claim=Some(confirmed);Ok(())})?;
    std::fs::File::open(project.dir().join("threads"))?.sync_all()?;Ok(())
}
pub fn recover(project:&Project,guard:&ProjectGuard)->Result<()> {
    guard.check_project(&project.dir())?;let (records,diagnostics)=list_with_diagnostics(project);anyhow::ensure!(diagnostics.is_empty(),"invalid thread inventory; launch recovery refused");
    for record in records {
        validate(&record)?;let Some(mut claim)=record.launch_claim else{continue;};
        if claim.phase==Phase::Pending {
            let pending=claim.clone();claim.phase=Phase::Uncertain;claim.error="Agent start was interrupted; the agent may already be running. Inspect its pane before explicitly restarting this thread.".into();
            update_checked(project,&record.id,|current|{anyhow::ensure!(current.launch_claim.as_ref()==Some(&pending),"launch claim changed during recovery");
                if current.lifecycle_generation==claim.generation&&current.status==Status::Open {current.status=Status::Failed;current.error=claim.error.clone();}current.launch_claim=Some(claim.clone());Ok(())})?;
            std::fs::File::open(project.dir().join("threads"))?.sync_all()?;
        }
        if claim.phase==Phase::Uncertain&&!claim.notified {
            let id=format!("launch-{}-{}-{}",record.id,claim.execution,claim.sequence);
            crate::inbox::write_once(project,&id,"thread-state",&record.id,&format!("{} agent start needs reconciliation",record.id),&claim.error)?;std::fs::File::open(project.dir().join("inbox"))?.sync_all()?;
            update_checked(project,&record.id,|current|{anyhow::ensure!(current.launch_claim.as_ref()==Some(&claim),"launch claim changed before notice acknowledgement");current.launch_claim.as_mut().unwrap().notified=true;Ok(())})?;
        }
    }Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,Project,Thread){let root=tempfile::tempdir().unwrap();let p=project::create(root.path(),"demo","",vec![]).unwrap();let t=allocate(&p,|t|{t.status=Status::Open;t.prompt_pending=true;}).unwrap();(root,p,t)}
    #[test]
    fn claim_fences_other_effects_and_confirmation_permits_brief_but_not_another_start(){
        let(_root,p,t)=fixture();let g=ProjectGuard::acquire(&p.dir()).unwrap();let c=Control::default();let claim=claim(&p,&g,&t,&["private-argument".into()],&"a".repeat(64),"terminal",&c).unwrap();
        let pending=load(&p,&t.id).unwrap();assert!(ready(&pending).is_err());assert!(prompt_delivery::ready(&pending).is_err());assert!(copy_delivery::ready(&pending).is_err());assert_eq!(pending.launch_attempts,1);
        assert!(!std::fs::read_to_string(record_path(&p,&t.id)).unwrap().contains("private-argument"));confirm(&p,&g,&t.id,&claim,&c).unwrap();
        let confirmed=load(&p,&t.id).unwrap();assert!(confirmed.prompt_pending);assert_eq!(group(&confirmed,&Live{pane_exists:true,..Default::default()},jiff::Timestamp::now()),Group::WaitingOnYou);assert!(prompt_delivery::ready(&confirmed).is_ok());assert!(ready(&confirmed).is_err());let mut edited=confirmed.clone();edited.pr="https://example.invalid/pr/1".into();assert!(ready(&edited).is_err(),"PR changes must not authorize another start");assert!(confirm(&p,&g,&t.id,&claim,&c).is_err());
    }
    #[test]
    fn lost_start_recovers_once_and_requires_new_execution(){
        let(_root,p,t)=fixture();let g=ProjectGuard::acquire(&p.dir()).unwrap();claim(&p,&g,&t,&[],&"b".repeat(64),"terminal",&Control::default()).unwrap();recover(&p,&g).unwrap();recover(&p,&g).unwrap();
        let failed=load(&p,&t.id).unwrap();assert_eq!(failed.status,Status::Failed);assert_eq!(failed.launch_claim.as_ref().unwrap().phase,Phase::Uncertain);assert!(ready(&failed).is_err());assert_eq!(crate::inbox::unhandled(&p).len(),1);
        update(&p,&t.id,|t|{t.status=Status::Open;t.lifecycle_generation+=1;}).unwrap();assert!(ready(&load(&p,&t.id).unwrap()).is_ok());
    }
    #[test]
    fn historical_uncertainty_does_not_fail_replacement_or_duplicate_notice(){
        let(_root,p,t)=fixture();let g=ProjectGuard::acquire(&p.dir()).unwrap();claim(&p,&g,&t,&[],&"b".repeat(64),"terminal",&Control::default()).unwrap();update(&p,&t.id,|t|t.lifecycle_generation+=1).unwrap();recover(&p,&g).unwrap();
        assert_eq!(load(&p,&t.id).unwrap().status,Status::Open);assert_eq!(crate::inbox::unhandled(&p).len(),1);recover(&p,&g).unwrap();assert_eq!(crate::inbox::unhandled(&p).len(),1);
    }
    #[test]
    fn stale_cancelled_and_invalid_claims_preserve_the_record(){
        let(_root,p,t)=fixture();let g=ProjectGuard::acquire(&p.dir()).unwrap();let control=Control::default();control.cancellation.cancel();assert!(claim(&p,&g,&t,&[],&"b".repeat(64),"terminal",&control).is_err());
        assert!(claim(&p,&g,&t,&[],"invalid","terminal",&Control::default()).is_err());assert_eq!(load(&p,&t.id).unwrap(),t);
        update(&p,&t.id,|t|t.lifecycle_generation+=1).unwrap();assert!(claim(&p,&g,&t,&[],&"b".repeat(64),"terminal",&Control::default()).is_err());assert!(load(&p,&t.id).unwrap().launch_claim.is_none());
    }
}
