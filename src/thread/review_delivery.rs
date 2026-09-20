//! Prepare review intent under the record lock; replay delivery independently.
use super::*;
use herdr_projects::review_notice::ReviewNotice;

pub fn validate(t: &Thread) -> Result<()> {
    anyhow::ensure!(t.review_notice_sequence <= i64::MAX as u64, "review sequence exhausted");
    if let Some(notice) = &t.pending_review_notice { notice.validate(&t.id,t.review_notice_sequence)?; }
    Ok(())
}
fn receipt(t: &Thread) -> Option<&herdr_projects::copy_receipt::CopyReceipt> {
    t.copy_receipt.as_ref().filter(|r|r.execution == execution_fingerprint(t) && r.report_hash == t.report_hash)
}
fn due(t: &Thread) -> bool {
    if t.status != Status::Open || t.removal.is_some() || t.report_hash.is_empty()
        || ![Group::ReadyForReview.token(),Group::Landing.token()].contains(&t.last_group.as_str()) { return false; }
    let execution=execution_fingerprint(t);
    if let Some(receipt)=receipt(t) {
        return t.last_review_execution != execution || t.last_review_copy_sequence != receipt.sequence || t.last_review_item_hash != t.report_hash;
    }
    // Compatibility for pre-receipt reports, including already acknowledged ones.
    t.last_review_item_hash != t.report_hash || (!t.last_review_execution.is_empty() && t.last_review_execution != execution)
}

pub fn prepare(project: &Project, expected: &Thread) -> Result<()> {
    copy_delivery::validate(expected)?;validate(expected)?;
    if expected.pending_review_notice.is_some() || !due(expected) { return Ok(()); }
    let execution=execution_fingerprint(expected);
    update_checked(project,&expected.id,|current| {
        copy_delivery::validate(current)?;validate(current)?;
        anyhow::ensure!(execution_fingerprint(current)==execution && current.report_hash==expected.report_hash
            && current.copy_receipt==expected.copy_receipt && current.last_group==expected.last_group
            && current.last_review_item_hash==expected.last_review_item_hash && current.last_review_execution==expected.last_review_execution
            && current.last_review_copy_sequence==expected.last_review_copy_sequence
            && current.review_notice_sequence==expected.review_notice_sequence && current.pending_review_notice.is_none() && due(current), "thread changed before review notice preparation");
        let sequence=current.review_notice_sequence.checked_add(1).context("review sequence exhausted")?;
        let mut summary=format!("{} \"{}\" has a new report: threads/{}.md",current.id,current.title,current.id);
        if let Some(notes)=copy_delivery::notes(current).filter(|n|!n.is_empty()) { summary.push_str(&format!("; not everything was copied: {}",notes.join("; "))); }
        let mut notice=ReviewNotice{id:format!("review-{}-{execution}-{sequence}",current.id),kind:"thread-state".into(),subject:current.id.clone(),summary,body:String::new(),execution,report_hash:current.report_hash.clone(),sequence,copy_receipt:receipt(current).cloned()};
        notice.body=notice.expected_body();notice.validate(&current.id,sequence)?;
        current.review_notice_sequence=sequence;current.pending_review_notice=Some(notice);Ok(())
    })?;
    Ok(())
}

pub fn deliver(project: &Project) -> Result<()> {
    let (threads,diagnostics)=list_with_diagnostics(project);
    anyhow::ensure!(diagnostics.is_empty(),"unreadable thread records: {}",diagnostics.join("; "));
    for t in threads {
        validate(&t)?;copy_delivery::validate(&t)?;
        let Some(notice)=t.pending_review_notice else { continue; };
        crate::inbox::write_once(project,&notice.id,&notice.kind,&notice.subject,&notice.summary,&notice.body)?;
        update_checked(project,&t.id,|current| {
            anyhow::ensure!(current.pending_review_notice.as_ref()==Some(&notice),"review notice changed during delivery");
            if execution_fingerprint(current)==notice.execution && current.report_hash==notice.report_hash
                && receipt(current)==notice.copy_receipt.as_ref() {
                current.last_review_item_hash=notice.report_hash.clone();
                current.last_review_execution=notice.execution.clone();
                current.last_review_copy_sequence=notice.copy_receipt.as_ref().map_or(0,|r|r.sequence);
            }
            current.pending_review_notice=None;Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir,Project,Thread) {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let t=allocate(&project,|t|{t.status=Status::Open;t.last_group=Group::ReadyForReview.token().into();}).unwrap();
        (root,project,t)
    }
    fn copy(project:&Project,t:&Thread,bytes:&[u8]) -> Thread {
        copy_delivery::record(project,t,&Copied{artifact_snapshot:None,outcome:CopyOutcome::Complete,report_hash:Some(sha256_hex(bytes))}).unwrap();
        load(project,&t.id).unwrap()
    }
    #[test]
    fn delivery_crash_replays_done_notice_and_acknowledges_despite_readiness_change() {
        let (_root,project,t)=fixture();let t=copy(&project,&t,b"A");prepare(&project,&t).unwrap();
        let pending=load(&project,&t.id).unwrap();let notice=pending.pending_review_notice.clone().unwrap();
        assert!(copy_delivery::ready(&pending).is_err());
        assert!(copy_delivery::record(&project,&pending,&Copied{artifact_snapshot:None,outcome:CopyOutcome::Complete,report_hash:Some(sha256_hex(b"B"))}).is_err());
        let inbox=project.dir().join("inbox");std::fs::remove_dir_all(&inbox).unwrap();std::fs::write(&inbox,b"blocked").unwrap();
        assert!(deliver(&project).is_err());assert_eq!(load(&project,&t.id).unwrap(),pending);
        std::fs::remove_file(&inbox).unwrap();std::fs::create_dir_all(inbox.join("done")).unwrap();
        crate::inbox::write_once(&project,&notice.id,&notice.kind,&notice.subject,&notice.summary,&notice.body).unwrap();
        crate::inbox::done(&project,&[notice.id.clone()],false).unwrap();
        update(&project,&t.id,|t|t.last_group="working".into()).unwrap();
        deliver(&project).unwrap();deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();assert!(current.pending_review_notice.is_none());
        assert_eq!(current.last_review_item_hash,sha256_hex(b"A"));assert_eq!(current.last_review_copy_sequence,1);
        assert_eq!(current.last_group,"working");assert_eq!(current.review_notice_sequence,1);
        let current=update(&project,&t.id,|t|t.last_group=Group::ReadyForReview.token().into()).unwrap();prepare(&project,&current).unwrap();
        assert!(load(&project,&t.id).unwrap().pending_review_notice.is_none());
        assert!(crate::inbox::unhandled(&project).is_empty());assert!(inbox.join("done").join(format!("{}.md",notice.id)).exists());
    }
    #[test]
    fn historical_delivery_never_acknowledges_replacement_with_identical_report() {
        let (_root,project,t)=fixture();let t=copy(&project,&t,b"A");prepare(&project,&t).unwrap();
        let notice=load(&project,&t.id).unwrap().pending_review_notice.unwrap();
        update(&project,&t.id,|t|{t.lifecycle_generation+=1;t.pane_id="replacement".into();}).unwrap();
        deliver(&project).unwrap();let replacement=load(&project,&t.id).unwrap();
        assert!(replacement.last_review_item_hash.is_empty());assert!(replacement.last_review_execution.is_empty());
        assert_eq!(replacement.last_review_copy_sequence,0);assert_eq!(replacement.pane_id,"replacement");
        let replacement=copy(&project,&replacement,b"A");prepare(&project,&replacement).unwrap();
        let new=load(&project,&t.id).unwrap().pending_review_notice.unwrap();
        assert_ne!(new.execution,notice.execution);assert_ne!(new.id,notice.id);assert_eq!(new.sequence,2);
        deliver(&project).unwrap();assert_eq!(crate::inbox::unhandled(&project).len(),2);
    }
    #[test]
    fn repeated_hashes_and_changed_copy_notes_get_distinct_review_notices() {
        let (_root,project,mut t)=fixture();let mut ids=std::collections::BTreeSet::new();
        for bytes in [b"A",b"B",b"A"] {
            t=copy(&project,&t,bytes);prepare(&project,&t).unwrap();
            let notice=load(&project,&t.id).unwrap().pending_review_notice.unwrap();assert!(ids.insert(notice.id));
            deliver(&project).unwrap();t=load(&project,&t.id).unwrap();
        }
        copy_delivery::record(&project,&t,&Copied{artifact_snapshot:None,outcome:CopyOutcome::Partial(vec!["library skipped".into()]),report_hash:Some(sha256_hex(b"A"))}).unwrap();
        copy_delivery::deliver(&project).unwrap();t=load(&project,&t.id).unwrap();prepare(&project,&t).unwrap();
        let notice=load(&project,&t.id).unwrap().pending_review_notice.unwrap();
        assert_eq!(notice.sequence,4);assert_eq!(notice.copy_receipt.as_ref().unwrap().sequence,4);
        assert!(notice.summary.contains("library skipped"));assert!(ids.insert(notice.id));
    }
    #[test]
    fn legacy_acknowledgement_is_preserved_and_stale_preparation_refuses() {
        let (_root,project,t)=fixture();let t=update(&project,&t.id,|t|{t.report_hash="legacy-hash".into();t.last_review_item_hash="legacy-hash".into();}).unwrap();
        prepare(&project,&t).unwrap();assert!(load(&project,&t.id).unwrap().pending_review_notice.is_none());
        let old=update(&project,&t.id,|t|t.report_hash="new-legacy-hash".into()).unwrap();
        update(&project,&t.id,|t|t.lifecycle_generation+=1).unwrap();
        assert!(prepare(&project,&old).is_err());
        let current=load(&project,&t.id).unwrap();prepare(&project,&current).unwrap();deliver(&project).unwrap();
        let current=load(&project,&t.id).unwrap();assert_eq!(current.last_review_item_hash,"new-legacy-hash");assert_eq!(current.last_review_copy_sequence,0);
    }
}
