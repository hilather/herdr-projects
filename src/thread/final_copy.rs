//! Atomic final-copy acknowledgement. Publication callers own project effects.
use super::*;
use herdr_projects::{execution_guard::ProjectGuard,final_copy_intent::{FinalCopyIntent,Notice,Purpose}};
use crate::source_tree::Control;

pub fn check(t:&Thread,intent:&FinalCopyIntent)->Result<()> {
    copy_delivery::validate(t)?;
    anyhow::ensure!(t.status==Status::Open&&t.removal.is_none()&&t.pending_live_copy.is_none()
        &&t.pending_final_copy.as_ref()==Some(intent)&&execution_fingerprint(t)==intent.execution
        &&t.report_hash==intent.previous_hash&&t.copy_receipt==intent.previous_receipt
        &&t.pending_copy_notice.is_none()&&t.pending_review_notice.is_none()&&t.pending_final_notice.is_none(),"final-copy execution or delivery state changed");
    Ok(())
}
pub fn resolution_eligible(project:&Project,t:&Thread,purpose:&Purpose)->Result<bool> {
    purpose.validate()?;
    Ok(match purpose {
        Purpose::Idle{days,started,last_state_change,last_report_change}=>{
            let text=crate::paths::read_control_text(&project.project_md(),1024*1024)?.context("project settings missing")?;
            let settings=project::parse_project_md(&text)?.0;let now=jiff::Timestamp::now().as_second();
            settings.auto_resolve_days==*days&&t.last_group==Group::Idle.token()&&t.last_state_change==*last_state_change&&t.last_report_change==*last_report_change
                &&[started,last_state_change,last_report_change].iter().filter(|s|!s.is_empty()).all(|s|s.parse::<jiff::Timestamp>().is_ok_and(|stamp|now.saturating_sub(stamp.as_second())>=i64::from(*days)*86_400))
        },
        Purpose::Merged{pr}=>t.pr==*pr&&t.pr_state=="MERGED"&&t.suppressed_merged_pr!=*pr
            &&crate::paths::read_control_text(&home_report_path(project,&t.id),50*1024*1024).ok().flatten()
                .and_then(|text|crate::pr::pr_line(&text).ok().flatten()).as_ref()==Some(pr),
    })
}
/// Narrow commit path: ordinary mutation cannot change lifecycle while a final
/// projection is pending. A withdrawn eligibility check completes the copy but
/// leaves the thread open, avoiding a permanently stranded recovery stage.
pub fn commit(project:&Project,guard:&ProjectGuard,id:&str,intent:&FinalCopyIntent,notes:Vec<String>,resolve:bool,control:&Control)->Result<()> {
    let _lock=project.lock()?;control.check()?;guard.check_project(&project.dir())?;
    anyhow::ensure!(project.try_status()?==project::Status::Active,"final-copy project is not active");
    let mut t=load(project,id)?;check(&t,intent)?;
    let changed_idle_report=matches!(intent.purpose,Purpose::Idle{..})&&intent.report_hash.as_ref().is_some_and(|hash|hash!=&intent.previous_hash);
    let resolve=resolve&&!changed_idle_report&&resolution_eligible(project,&t,&intent.purpose)?;
    let summary=if resolve {format!("{} was resolved automatically ({})",t.id,intent.purpose.reason())}
        else {format!("{} final copy completed, but changed eligibility prevented automatic resolution",t.id)};
    let body=notes.join("; ");
    let notice=Notice{id:format!("final-{}-{}-{}",t.id,intent.execution,intent.sequence),sequence:intent.sequence,execution:intent.execution.clone(),summary,body};notice.validate(&t.id,t.final_copy_sequence)?;
    if let Some(hash)=&intent.report_hash {
        let receipt=match &t.copy_receipt {
            Some(receipt) if receipt.execution==intent.execution&&receipt.report_hash==*hash&&receipt.notes==notes=>receipt.clone(),
            _=>herdr_projects::copy_receipt::CopyReceipt{sequence:t.copy_receipt.as_ref().map_or(0,|r|r.sequence).checked_add(1).context("copy sequence exhausted")?,execution:intent.execution.clone(),report_hash:hash.clone(),notes},
        };
        receipt.validate()?;t.copy_receipt=Some(receipt);
        if t.report_hash!=*hash {t.last_report_change=project::now();}
        t.report_hash=hash.clone();
    }
    if resolve {
        t.status=Status::Resolved;t.resolved_reason=intent.purpose.reason().into();t.prompt_pending=false;t.last_finalization=intent.operation.clone();
        t.artifact_snapshot=intent.snapshot.clone().unwrap_or_default();
    }
    t.pending_final_notice=Some(notice);t.pending_final_copy=None;t.updated=project::now();
    control.check()?;write_record(project,&t)?;std::fs::File::open(project.dir().join("threads"))?.sync_all()?;Ok(())
}
pub fn deliver(project:&Project,t:&Thread)->Result<()> {
    let Some(notice)=&t.pending_final_notice else {return Ok(());};notice.validate(&t.id,t.final_copy_sequence)?;
    crate::inbox::write_once(project,&notice.id,"thread-state",&t.id,&notice.summary,&notice.body)?;
    std::fs::File::open(project.dir().join("inbox"))?.sync_all()?;
    update_checked(project,&t.id,|current|{
        anyhow::ensure!(current.pending_final_notice.as_ref()==Some(notice),"final-copy notice changed");current.pending_final_notice=None;Ok(())
    })?;Ok(())
}
