//! Project-local fallback while another project owns the shared root barrier.
//! No prompts, metadata writes, copies, launches or notification commands.
use super::*;
use herdr_projects::status_notice::StatusNotice;

pub(super) fn deliver(project:&Project)->Result<()> {
    let (threads,diagnostics)=thread::list_with_diagnostics(project);
    anyhow::ensure!(diagnostics.is_empty(),"unreadable thread records: {}",diagnostics.join("; "));
    for t in threads {
        let Some(notice)=t.pending_status_notice else {continue;};
        notice.validate(&t.id,t.status_notice_sequence)?;
        // A committed historical notice survives execution replacement. It
        // never changes the replacement's status or execution identity.
        crate::inbox::write_once(project,&notice.id,&notice.kind,&notice.subject,&notice.summary,&notice.body)?;
        thread::update_checked(project,&t.id,|current| {
            anyhow::ensure!(current.pending_status_notice.as_ref()==Some(&notice),"status notice changed during delivery");
            current.pending_status_notice=None;Ok(())
        })?;
    }
    Ok(())
}

fn record(project:&Project,t:&thread::Thread,state:&str,group:thread::Group,note:&str)->Result<()> {
    if state==t.last_state && group.token()==t.last_group {return Ok(());}
    let execution=thread::execution_fingerprint(t);
    thread::update_checked(project,&t.id,|current| {
        anyhow::ensure!(thread::execution_fingerprint(current)==execution && current.last_state==t.last_state && current.last_group==t.last_group,"thread changed during observation");
        anyhow::ensure!(current.pending_status_notice.is_none(),"prior status notice still pending");
        if !t.last_group.is_empty() && group.token()!=t.last_group && matches!(group,thread::Group::WaitingOnYou|thread::Group::Landing|thread::Group::Idle) {
            let sequence=current.status_notice_sequence.checked_add(1).context("status notice sequence exhausted")?;
            // TOML integers are signed; refuse before any observation is saved.
            anyhow::ensure!(sequence<=i64::MAX as u64,"status notice sequence exhausted");
            let mut summary=format!("{} \"{}\" is now {} ({note})",t.id,t.title,group.label());
            if group==thread::Group::WaitingOnYou && !t.pane_id.is_empty() {summary.push_str(&format!("; it needs the user in pane {}",t.pane_id));}
            let notice=StatusNotice {
                id:format!("status-{}-{execution}-{sequence}",t.id),kind:"thread-state".into(),subject:t.id.clone(),summary,
                body:format!("Observed execution `{execution}`: {} -> {}.",t.last_group,group.token()),
                execution:execution.clone(),sequence,previous_group:t.last_group.clone(),next_group:group.token().into(),
            };
            notice.validate(&t.id,sequence)?;
            current.status_notice_sequence=sequence;
            current.pending_status_notice=Some(notice);
        }
        if current.last_state!=state {current.last_state=state.into();current.last_state_change=project::now();}
        current.last_group=group.token().into();Ok(())
    })?;
    Ok(())
}

pub(super) fn observe(project:&Project,agents:&[Agent],panes:&[Pane])->Result<()> {
    let now=jiff::Timestamp::now();
    for t in open_threads(project,false) {
        // Starting-timeout decisions and pending prompts belong to the effect pass.
        if t.status!=thread::Status::Open {continue;}
        let mut live=thread::live_state(&t,agents,panes,now);
        let state=live.agent_state.clone().unwrap_or_default();
        if state!=t.last_state {live.state_secs=0;}
        // Use only the previously copied report receipt, never a fresh local hash.
        let group=thread::group(&t,&live,now);
        let note=if !live.pane_exists {"pane closed"} else if state.is_empty() {"no agent"} else {&state};
        record(project,&t,&state,group,note)?;
    }
    deliver(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn committed_notice_retries_across_delivery_crashes_and_execution_replacement() {
        let root=tempfile::tempdir().unwrap();let project=project::create(root.path(),"demo","",vec![]).unwrap();
        let t=thread::allocate(&project,|t| {t.status=thread::Status::Open;t.last_group="working".into();t.last_state="working".into();t.pane_id="old-pane".into();}).unwrap();
        let blocked=project.dir().join("inbox");std::fs::remove_dir_all(&blocked).unwrap();std::fs::write(&blocked,"blocked").unwrap();
        record(&project,&t,"idle",thread::Group::Idle,"idle").unwrap();
        assert!(deliver(&project).is_err());
        let pending=thread::load(&project,&t.id).unwrap();assert_eq!(pending.last_group,"idle");
        let notice=pending.pending_status_notice.clone().unwrap();assert_eq!(pending.status_notice_sequence,1);
        // A failed replay must not allocate a second identity or overwrite intent.
        assert!(record(&project,&pending,"blocked",thread::Group::WaitingOnYou,"blocked").is_err());
        assert_eq!(thread::load(&project,&t.id).unwrap(),pending);
        std::fs::remove_file(&blocked).unwrap();std::fs::create_dir_all(blocked.join("done")).unwrap();
        crate::inbox::write_once(&project,&notice.id,&notice.kind,&notice.subject,&notice.summary,&notice.body).unwrap();
        crate::inbox::done(&project,&[notice.id.clone()],false).unwrap();
        // Simulate crash after delivery, followed by a user replacing execution.
        thread::update(&project,&t.id,|t| {t.lifecycle_generation+=1;t.pane_id="new-pane".into();t.last_group="working".into();}).unwrap();
        deliver(&project).unwrap();deliver(&project).unwrap();
        let current=thread::load(&project,&t.id).unwrap();assert!(current.pending_status_notice.is_none());assert_eq!(current.last_group,"working");assert_eq!(current.pane_id,"new-pane");assert_eq!(current.status_notice_sequence,1);
        assert!(blocked.join("done").join(format!("{}.md",notice.id)).exists());assert!(!blocked.join(format!("{}.md",notice.id)).exists());
        // Stale observations may never mutate the replacement execution.
        assert!(record(&project,&t,"idle",thread::Group::Idle,"idle").is_err());
    }
}
