//! Persist copy receipts before delivering independently retryable warnings.
use super::*;
use herdr_projects::copy_receipt::{CopyNotice, CopyReceipt};

pub fn validate(t: &Thread) -> Result<()> {
    super::prompt_delivery::validate(t)?;
    anyhow::ensure!(t.final_copy_sequence<=i64::MAX as u64,"final-copy sequence exhausted");
    if let Some(intent)=&t.pending_final_copy {intent.validate()?;anyhow::ensure!(intent.sequence==t.final_copy_sequence&&t.pending_live_copy.is_none(),"invalid pending final copy");}
    if let Some(notice)=&t.pending_final_notice {notice.validate(&t.id,t.final_copy_sequence)?;}
    anyhow::ensure!(t.live_copy_sequence<=i64::MAX as u64,"invalid live-copy sequence");
    if let Some(intent)=&t.pending_live_copy {intent.validate()?;anyhow::ensure!(intent.sequence==t.live_copy_sequence,"live-copy intent sequence mismatch");}
    if let Some(receipt) = &t.copy_receipt { receipt.validate()?; }
    if let Some(notice) = &t.pending_copy_notice {
        notice.validate(&t.id, t.copy_receipt.as_ref().context("copy notice has no receipt")?)?;
    }
    Ok(())
}

pub fn ready(t: &Thread) -> Result<()> {
    ready_for(t,None)
}
fn ready_for(t:&Thread,intent:Option<&herdr_projects::live_copy_intent::LiveCopyIntent>)->Result<()> {
    validate(t)?;
    anyhow::ensure!(t.launch_claim.as_ref().is_none_or(|c|c.phase!=super::launch_delivery::Phase::Pending),"recover pending agent start first");
    anyhow::ensure!(t.prompt_claim.as_ref().is_none_or(|claim|claim.phase!=super::prompt_delivery::Phase::Pending),"recover pending brief delivery first");
    super::review_delivery::validate(t)?;
    anyhow::ensure!(t.pending_copy_notice.is_none(), "prior copy warning still pending");
    anyhow::ensure!(t.pending_review_notice.is_none(), "prior review notice still pending");
    anyhow::ensure!(t.pending_final_copy.is_none()&&t.pending_final_notice.is_none(),"recover or deliver the pending final copy first");
    anyhow::ensure!(t.pending_live_copy.as_ref()==intent,"recover the pending live projection first");
    Ok(())
}

pub fn record(project: &Project, expected: &Thread, copied: &Copied) -> Result<()> {
    record_inner(project,expected,copied,None,None)
}
pub(crate) fn record_projection(project:&Project,expected:&Thread,copied:&Copied,intent:&herdr_projects::live_copy_intent::LiveCopyIntent,control:&crate::source_tree::Control)->Result<()> {
    record_inner(project,expected,copied,Some(intent),Some(control))
}
fn record_inner(project:&Project,expected:&Thread,copied:&Copied,intent:Option<&herdr_projects::live_copy_intent::LiveCopyIntent>,control:Option<&crate::source_tree::Control>)->Result<()> {
    ready_for(expected,intent)?;
    anyhow::ensure!(expected.status == Status::Open && expected.removal.is_none(), "thread is not eligible for a live copy receipt");
    let notes = match &copied.outcome {
        CopyOutcome::Complete => Vec::new(),
        CopyOutcome::Partial(notes) => notes.clone(),
        CopyOutcome::Failed(error) => bail!("copy failed: {error}"),
    };
    let hash = copied.report_hash.as_ref().context("observed report was not copied; retaining its previous hash")?;
    let execution = execution_fingerprint(expected);
    if let Some(intent)=intent {
        anyhow::ensure!(intent.execution==execution&&intent.previous_hash==expected.report_hash
            &&intent.previous_receipt==expected.copy_receipt&&&intent.report_hash==hash,"live projection receipt does not match its intent");
    }
    update_checked(project, &expected.id, |current| {
        if let Some(control)=control {control.check()?;}
        ready_for(current,intent)?;
        anyhow::ensure!(current.status == Status::Open && current.removal.is_none()
            && execution_fingerprint(current) == execution && current.report_hash == expected.report_hash
            && current.copy_receipt == expected.copy_receipt, "thread changed during copy");
        if &current.report_hash == hash && current.copy_receipt.as_ref().is_some_and(|r| r.execution == execution && &r.report_hash == hash && r.notes == notes) {
            current.pending_live_copy=None;
            return Ok(());
        }
        let sequence = current.copy_receipt.as_ref().map_or(0, |r| r.sequence).checked_add(1).context("copy sequence exhausted")?;
        let receipt = CopyReceipt { sequence, execution, report_hash: hash.clone(), notes };
        receipt.validate()?;
        current.pending_copy_notice = if receipt.notes.is_empty() { None } else { Some(CopyNotice::new(&current.id, receipt.clone())?) };
        current.copy_receipt = Some(receipt);
        current.report_hash = hash.clone();
        current.last_report_change = project::now();
        current.pending_live_copy=None;
        Ok(())
    })?;
    Ok(())
}

pub fn deliver(project: &Project) -> Result<()> {
    let (threads, diagnostics) = list_with_diagnostics(project);
    anyhow::ensure!(diagnostics.is_empty(), "unreadable thread records: {}", diagnostics.join("; "));
    for t in threads {
        validate(&t)?;
        final_copy::deliver(project,&t)?;
        let Some(notice) = t.pending_copy_notice else { continue; };
        crate::inbox::write_once(project, &notice.id, &notice.kind, &notice.subject, &notice.summary, &notice.body)?;
        update_checked(project, &t.id, |current| {
            anyhow::ensure!(current.pending_copy_notice.as_ref() == Some(&notice), "copy notice changed during delivery");
            current.pending_copy_notice = None;
            Ok(())
        })?;
    }
    Ok(())
}

pub fn notes(t: &Thread) -> Option<&[String]> {
    t.copy_receipt.as_ref().filter(|r| r.execution == execution_fingerprint(t) && r.report_hash == t.report_hash)
        .map(|r| r.notes.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Project, Thread) {
        let root=tempfile::tempdir().unwrap();
        let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let thread=allocate(&project,|t| {t.status=Status::Open;t.last_group="working".into();}).unwrap();
        (root,project,thread)
    }
    fn partial(bytes:&[u8]) -> Copied {
        Copied{artifact_snapshot:None,outcome:CopyOutcome::Partial(vec!["library link skipped".into()]),report_hash:Some(sha256_hex(bytes))}
    }
    #[test]
    fn warning_survives_restart_before_readiness_and_delivery_before_ack() {
        let (_root,project,t)=fixture();record(&project,&t,&partial(b"A")).unwrap();
        let pending=load(&project,&t.id).unwrap();let notice=pending.pending_copy_notice.clone().unwrap();
        assert_eq!(notes(&pending).unwrap(), &["library link skipped"]);
        assert!(record(&project,&pending,&partial(b"B")).is_err());
        let inbox=project.dir().join("inbox");std::fs::remove_dir_all(&inbox).unwrap();std::fs::write(&inbox,b"blocked").unwrap();
        assert!(deliver(&project).is_err());assert_eq!(load(&project,&t.id).unwrap(),pending);
        std::fs::remove_file(&inbox).unwrap();std::fs::create_dir_all(inbox.join("done")).unwrap();
        crate::inbox::write_once(&project,&notice.id,&notice.kind,&notice.subject,&notice.summary,&notice.body).unwrap();
        crate::inbox::done(&project,&[notice.id.clone()],false).unwrap();
        // A killed ticker can restart after inbox delivery but before clearing intent.
        deliver(&project).unwrap();deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();assert!(current.pending_copy_notice.is_none());
        assert_eq!(current.last_group,"working");assert_eq!(notes(&current).unwrap(), &["library link skipped"]);
        assert!(inbox.join("done").join(format!("{}.md",notice.id)).exists());
        assert!(!inbox.join(format!("{}.md",notice.id)).exists());
        let mut state=crate::steps::State::default();
        crate::steps::write_thread_items(&project,&mut state,&[],false).unwrap();
        assert!(crate::inbox::unhandled(&project).is_empty());
        update(&project,&t.id,|t|t.last_group=Group::ReadyForReview.token().into()).unwrap();
        crate::steps::write_thread_items(&project,&mut state,&[],false).unwrap();
        let items=crate::inbox::unhandled(&project);
        assert_eq!(items.len(),1);assert!(items[0].summary.contains("library link skipped"));
    }
    #[test]
    fn historical_warning_does_not_mutate_replacement_and_stale_copy_refuses() {
        let (_root,project,t)=fixture();record(&project,&t,&partial(b"A")).unwrap();
        let old=load(&project,&t.id).unwrap();
        let replacement=update(&project,&t.id,|t|{t.lifecycle_generation+=1;t.pane_id="replacement".into();t.report_hash=sha256_hex(b"replacement");}).unwrap();
        assert!(notes(&replacement).is_none());deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();assert_eq!(current.report_hash,replacement.report_hash);assert_eq!(current.pane_id,"replacement");
        let mut stale=old.clone();stale.pending_copy_notice=None;
        assert!(record(&project,&stale,&partial(b"B")).is_err());
        assert_eq!(load(&project,&t.id).unwrap(),current);
    }
    #[test]
    fn distinct_copies_get_sequences_but_identical_receipts_reuse_them() {
        let (_root,project,t)=fixture();record(&project,&t,&partial(b"A")).unwrap();deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();record(&project,&current,&partial(b"A")).unwrap();
        assert_eq!(load(&project,&t.id).unwrap(),current);
        for (bytes,sequence) in [(b"B",2),(b"A",3)] {
            let current=load(&project,&t.id).unwrap();record(&project,&current,&partial(bytes)).unwrap();
            assert_eq!(load(&project,&t.id).unwrap().copy_receipt.unwrap().sequence,sequence);deliver(&project).unwrap();
        }
        let current=load(&project,&t.id).unwrap();let mut changed=partial(b"A");changed.outcome=CopyOutcome::Partial(vec!["different omission".into()]);
        record(&project,&current,&changed).unwrap();assert_eq!(load(&project,&t.id).unwrap().copy_receipt.unwrap().sequence,4);
    }
    #[test]
    fn failed_library_after_report_publication_never_advances_receipt() {
        let (_root,project,t)=fixture();record(&project,&t,&partial(b"A")).unwrap();deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();
        let failed=Copied{artifact_snapshot:None,outcome:CopyOutcome::Failed("library failed".into()),report_hash:Some(sha256_hex(b"B"))};
        assert!(record(&project,&current,&failed).is_err());assert_eq!(load(&project,&t.id).unwrap(),current);
        record(&project,&current,&partial(b"B")).unwrap();assert_eq!(load(&project,&t.id).unwrap().report_hash,sha256_hex(b"B"));
    }
}
