//! Additive per-file publication with an exact durable recovery stage.
use super::*;
use crate::{project,thread};
use herdr_projects::{execution_guard::ProjectGuard,live_copy_intent::LiveCopyIntent};

fn eligible(project:&Project,guard:&ProjectGuard,t:&Thread)->Result<()> {
    thread::validate_id(&t.id)?;
    guard.check_project(&project.dir())?;project::ensure_legacy(&project.dir())?;
    ensure!(project.try_status()?==project::Status::Active,"live projection requires an active project");
    ensure!(t.status==thread::Status::Open&&t.removal.is_none(),"thread is not eligible for live projection");
    thread::copy_delivery::validate(t)?;Ok(())
}
pub(super) fn verify(path:&Path,source:&Source,control:&Control)->Result<()> {
    control.check()?;validate(source)?;let root=Directory::open(path)?;
    let mut report=control.budget();let mut library=None;
    for entry in entries(source) {
        let budget=if entry.path=="report.md" {&mut report}else{library.get_or_insert_with(||control.budget())};
        budget.entry(entry.path.matches('/').count())?;
        if entry.directory {root.directory(Path::new(&entry.path))?;continue;}
        let (bytes,hash)=digest(root.file(Path::new(&entry.path))?,budget,None)?;
        ensure!(bytes==entry.bytes&&hash==entry.sha256,"retained live stage is corrupt");
    }
    root.matches_path(path)?;control.check()?;Ok(())
}
fn load_stage(project:&Project,id:&str,intent:&LiveCopyIntent,control:&Control)->Result<(PathBuf,Source)> {
    control.check()?;thread::validate_id(id)?;intent.validate()?;
    let path=project.state_dir().join("live-copies").join(intent.stage_name(id));
    let root=Directory::open(&path)?;let mut bytes=Vec::new();
    root.file(Path::new("manifest.json"))?.take(MANIFEST_LIMIT as u64+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=MANIFEST_LIMIT&&thread::sha256_hex(&bytes)==intent.stage_digest,"retained live manifest is missing or corrupt");
    let source:Source=serde_json::from_slice(&bytes)?;
    ensure!(source.report.as_ref().map(|e|e.sha256.as_str())==Some(intent.report_hash.as_str()),"live stage report mismatch");
    verify(&path,&source,control)?;Ok((path,source))
}

impl LiveCopy {
    /// Caller has verified supervised sender success and supplies the frozen
    /// authority digest plus a current configuration/routing validation closure.
    #[allow(dead_code)]
    pub fn publish(self,project:&Project,guard:&ProjectGuard,expected:&Thread,authority:&str,authorize:impl FnMut()->Result<()>)->Result<()> {
        self.publish_controlled(project,guard,expected,authority,&Control::default(),authorize)
    }
    #[allow(dead_code)]
    pub fn publish_controlled(self,project:&Project,guard:&ProjectGuard,expected:&Thread,authority:&str,control:&Control,mut authorize:impl FnMut()->Result<()>)->Result<()> {
        self.begin_controlled(project,guard,expected,authority,control,&mut authorize)?;
        resume_controlled(project,guard,&expected.id,authority,control,authorize)
    }
    #[cfg(test)]
    fn begin(self,project:&Project,guard:&ProjectGuard,expected:&Thread,authority:&str,authorize:&mut impl FnMut()->Result<()>)->Result<()> {
        self.begin_controlled(project,guard,expected,authority,&Control::default(),authorize)
    }
    fn begin_controlled(self,project:&Project,guard:&ProjectGuard,expected:&Thread,authority:&str,control:&Control,authorize:&mut impl FnMut()->Result<()>)->Result<()> {
        control.check()?;
        eligible(project,guard,expected)?;thread::copy_delivery::ready(expected)?;authorize()?;
        ensure!(self.staging.0.parent()==Some(project.state_dir().join("live-copies").as_path()),"live stage belongs to a different project");
        verify(&self.staging.0,&self.source,control)?;authorize()?;control.check()?;
        let bytes=serde_json::to_vec(&self.source)?;let digest=thread::sha256_hex(&bytes);
        let sequence=expected.live_copy_sequence.checked_add(1).context("live-copy sequence exhausted")?;
        let intent=LiveCopyIntent{sequence,execution:thread::execution_fingerprint(expected),authority:authority.into(),previous_hash:expected.report_hash.clone(),previous_receipt:expected.copy_receipt.clone(),report_hash:self.report_hash().context("live stage has no copied report")?.into(),stage_digest:digest};intent.validate()?;
        let mut manifest=OpenOptions::new().write(true).create_new(true).mode(0o600).open(self.staging.0.join("manifest.json"))?;
        manifest.write_all(&bytes)?;manifest.sync_all()?;File::open(&self.staging.0)?.sync_all()?;
        let parent=self.staging.0.parent().unwrap();let retained=parent.join(intent.stage_name(&expected.id));
        if retained.try_exists()? {load_stage(project,&expected.id,&intent,control)?;}else {fs::rename(&self.staging.0,&retained)?;}
        File::open(parent)?.sync_all()?;
        File::open(project.state_dir())?.sync_all()?;
        // Drop only removes the old temporary path. The retained stage is now
        // owned by the durable intent (or harmless orphan if this commit fails).
        thread::update_checked(project,&expected.id,|current| {
            control.check()?;thread::copy_delivery::ready(current)?;eligible(project,guard,current)?;
            ensure!(thread::execution_fingerprint(current)==intent.execution&&current.report_hash==intent.previous_hash
                &&current.copy_receipt==intent.previous_receipt&&current.live_copy_sequence==expected.live_copy_sequence,"thread changed before live projection intent");
            current.live_copy_sequence=sequence;current.pending_live_copy=Some(intent.clone());Ok(())
        })?;
        File::open(project.dir().join("threads"))?.sync_all()?;Ok(())
    }
}
fn check(project:&Project,guard:&ProjectGuard,id:&str,intent:&LiveCopyIntent,authority:&str)->Result<Thread> {
    let t=thread::load(project,id)?;eligible(project,guard,&t)?;
    ensure!(t.pending_live_copy.as_ref()==Some(intent)&&thread::execution_fingerprint(&t)==intent.execution
        &&t.report_hash==intent.previous_hash&&t.copy_receipt==intent.previous_receipt&&authority==intent.authority,"live projection authority or execution changed; recovery refused");
    ensure!(t.pending_copy_notice.is_none()&&t.pending_review_notice.is_none(),"pending notice blocks live projection");Ok(t)
}
pub(super) fn parent(root:&Directory,path:&Path,control:&Control)->Result<Directory> {
    let mut current=root.directory(Path::new("."))?;
    for part in path.components() {
        let std::path::Component::Normal(name)=part else {anyhow::bail!("invalid projection path");};
        control.check()?;current=current.create_dir(name)?;
    }
    Ok(current)
}
#[allow(dead_code)]
pub fn resume(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,authorize:impl FnMut()->Result<()>)->Result<()> {
    resume_controlled(project,guard,id,authority,&Control::default(),authorize)
}
#[allow(dead_code)]
pub fn resume_controlled(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,control:&Control,mut authorize:impl FnMut()->Result<()>)->Result<()> {
    resume_with_control(project,guard,id,authority,control,&mut authorize,||Ok(()))
}
#[cfg(test)]
fn resume_with(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,authorize:&mut impl FnMut()->Result<()>,after_file:impl FnMut()->Result<()>)->Result<()> {
    resume_with_control(project,guard,id,authority,&Control::default(),authorize,after_file)
}
fn resume_with_control(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,control:&Control,authorize:&mut impl FnMut()->Result<()>,mut after_file:impl FnMut()->Result<()>)->Result<()> {
    control.check()?;
    let t=thread::load(project,id)?;let intent=t.pending_live_copy.clone().context("no pending live projection")?;
    check(project,guard,id,&intent,authority)?;authorize()?;
    let (stage_path,source)=load_stage(project,id,&intent,control)?;let stage=Directory::open(&stage_path)?;
    authorize()?;check(project,guard,id,&intent,authority)?;
    let project_root=Directory::open(&project.dir())?;
    // Library first, report last. A crash may expose a partial additive library,
    // but the durable intent remains until every included byte is published.
    let mut library_budget=control.budget();
    for entry in &source.library {
        library_budget.check()?;
        let relative=Path::new(&entry.path).strip_prefix("library")?;
        let base=project_root.create_dir(OsStr::new("library"))?.create_dir(OsStr::new(id))?;
        if entry.directory {parent(&base,relative,control)?;continue;}
        let destination=parent(&base,relative.parent().context("missing projection parent")?,control)?;
        copy_entry(&stage,&destination,relative.file_name().unwrap(),entry,&mut library_budget)?;after_file()?;
    }
    let report=source.report.as_ref().context("live stage has no report")?;
    let reports=project_root.create_dir(OsStr::new("threads"))?;
    copy_entry(&stage,&reports,OsStr::new(&format!("{id}.md")),report,&mut control.budget())?;after_file()?;
    verify_home(&project_root,id,&source,control)?;
    project_root.matches_path(&project.dir())?;authorize()?;
    let current=check(project,guard,id,&intent,authority)?;
    let notes=render_notes(&source);
    let copied=thread::Copied{artifact_snapshot:None,outcome:if notes.is_empty(){thread::CopyOutcome::Complete}else{thread::CopyOutcome::Partial(notes)},report_hash:Some(intent.report_hash.clone())};
    control.check()?;thread::copy_delivery::record_projection(project,&current,&copied,&intent,control)?;
    File::open(project.dir().join("threads"))?.sync_all()?;
    fs::remove_dir_all(&stage_path)?;File::open(stage_path.parent().unwrap())?.sync_all()?;Ok(())
}
pub(super) fn verify_home(root:&Directory,id:&str,source:&Source,control:&Control)->Result<()> {
    let mut report=control.budget();let mut library=None;
    for entry in entries(source) {
        let (path,budget)=if entry.path=="report.md" {(PathBuf::from(format!("threads/{id}.md")),&mut report)}
            else {(Path::new("library").join(id).join(Path::new(&entry.path).strip_prefix("library")?),library.get_or_insert_with(||control.budget()))};
        budget.check()?;
        if entry.directory {root.directory(&path)?;continue;}
        let (bytes,hash)=digest(root.file(&path)?,budget,None)?;
        ensure!(bytes==entry.bytes&&hash==entry.sha256,"published live copy changed before receipt commit");
    }
    Ok(())
}
pub(super) fn copy_entry(stage:&Directory,destination:&Directory,name:&OsStr,entry:&Entry,budget:&mut Budget)->Result<()> {
    let mut source=stage.file(Path::new(&entry.path))?;budget.size(&source)?;let before=source.metadata()?;
    destination.write_atomic(name,|target| {
        let mut hash=Sha256::new();let mut bytes=0u64;let mut buffer=[0;64*1024];
        loop {let n=budget.read(&mut source,&mut buffer)?;if n==0{break;}bytes+=n as u64;hash.update(&buffer[..n]);target.write_all(&buffer[..n])?;}
        crate::source_tree::unchanged(&source,&before)?;
        ensure!(bytes==entry.bytes&&format!("{:x}",hash.finalize())==entry.sha256,"live stage changed during publication");budget.check()?;Ok(())
    })?;budget.check()?;Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const AUTHORITY:&str="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn fixture()->(tempfile::TempDir,Project,Thread,LiveCopy) {
        let (root,project,record)=super::super::super::tests::fixture();
        let t=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.thread_dir=record.thread_dir;t.report_hash=thread::sha256_hex(b"old");t.last_group=thread::Group::ReadyForReview.token().into();}).unwrap();
        fs::write(thread::home_report_path(&project,&t.id),b"old").unwrap();
        let mut bytes=Vec::new();export(Path::new(&t.thread_dir),&mut bytes).unwrap();let archive=root.path().join("archive");fs::write(&archive,bytes).unwrap();let staged=receive(&project,&archive).unwrap();
        (root,project,t,staged)
    }
    #[test]
    fn cancelled_or_expired_copy_cannot_create_intent_or_receive() {
        for expired in [false,true] {
            let (root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
            let mut control=Control::default();
            if expired {control.deadline=std::time::Instant::now();}else{control.cancellation.cancel();}
            assert!(receive_controlled(&project,&root.path().join("archive"),&control).is_err());
            assert!(staged.publish_controlled(&project,&guard,&t,AUTHORITY,&control,||Ok(())).is_err());
            assert_eq!(thread::load(&project,&t.id).unwrap(),t);
            assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        }
    }
    #[test]
    fn cancellation_during_publication_preserves_exact_recovery_stage() {
        let (_root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
        staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();
        let control=Control::default();
        assert!(resume_with_control(&project,&guard,&t.id,AUTHORITY,&control,&mut ||Ok(()),||{control.cancellation.cancel();Ok(())}).is_err());
        let pending=thread::load(&project,&t.id).unwrap();assert!(pending.pending_live_copy.is_some());assert_eq!(pending.report_hash,t.report_hash);
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        fs::remove_dir_all(&t.thread_dir).unwrap();
        resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).unwrap();
        assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());
    }
    #[test]
    fn cancellation_at_final_authorization_prevents_receipt_commit() {
        let (_root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
        staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();
        let control=Control::default();let mut calls=0;
        assert!(resume_controlled(&project,&guard,&t.id,AUTHORITY,&control,||{calls+=1;if calls==3 {control.cancellation.cancel();}Ok(())}).is_err());
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        let pending=thread::load(&project,&t.id).unwrap();assert!(pending.pending_live_copy.is_some());assert_eq!(pending.report_hash,t.report_hash);
        resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).unwrap();
        assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());
    }
    #[test]
    fn additive_projection_is_durable_and_never_becomes_cleanup_evidence() {
        let (_root,project,t,staged)=fixture();let home=project.dir().join("library").join(&t.id);fs::create_dir_all(&home).unwrap();fs::write(home.join("retained"),b"old content").unwrap();
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();staged.publish(&project,&guard,&t,AUTHORITY,||Ok(())).unwrap();
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        assert_eq!(fs::read(home.join("artifact")).unwrap(),b"version A");assert_eq!(fs::read(home.join("retained")).unwrap(),b"old content");
        let current=thread::load(&project,&t.id).unwrap();assert!(current.pending_live_copy.is_none());assert_eq!(current.copy_receipt.unwrap().report_hash,current.report_hash);
        assert!(current.artifact_snapshot.is_empty());assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),0);
    }
    #[test]
    fn interruption_recovers_exact_stage_after_source_loss_and_blocks_lifecycle() {
        let (_root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
        staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();
        assert!(resume_with(&project,&guard,&t.id,AUTHORITY,&mut ||Ok(()),||anyhow::bail!("interrupted after library write")).is_err());
        let pending=thread::load(&project,&t.id).unwrap();assert!(pending.pending_live_copy.is_some());assert_eq!(pending.report_hash,t.report_hash);
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        assert!(thread::update(&project,&t.id,|t|t.lifecycle_generation+=1).is_err());
        assert!(thread::copy_delivery::ready(&pending).is_err());thread::review_delivery::prepare(&project,&pending).unwrap();
        assert!(thread::load(&project,&t.id).unwrap().pending_review_notice.is_none());
        fs::remove_dir_all(&t.thread_dir).unwrap();
        assert!(resume(&project,&guard,&t.id,&"b".repeat(64),||Ok(())).is_err());
        resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).unwrap();
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());
    }
    #[test]
    fn missing_or_corrupt_stage_refuses_without_replacing_home_report() {
        for missing in [false,true] {
            let (_root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
            staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();let pending=thread::load(&project,&t.id).unwrap();
            let path=project.state_dir().join("live-copies").join(pending.pending_live_copy.as_ref().unwrap().stage_name(&t.id));
            if missing {fs::remove_dir_all(path).unwrap();}else{fs::write(path.join("report.md"),b"corrupt").unwrap();}
            assert!(resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).is_err());assert_eq!(thread::load(&project,&t.id).unwrap(),pending);
            assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        }
    }
    #[test]
    fn wrong_guard_and_destination_links_cannot_publish_outside_project() {
        let (root,project,t,staged)=fixture();let other=project::create(root.path(),"other","",vec![]).unwrap();let wrong=ProjectGuard::acquire(&other.dir()).unwrap();
        assert!(staged.publish(&project,&wrong,&t,AUTHORITY,||Ok(())).is_err());assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());drop(wrong);
        let (root,project,t,staged)=fixture();let outside=root.path().join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("artifact"),b"outside").unwrap();
        std::os::unix::fs::symlink(&outside,project.dir().join("library").join(&t.id)).unwrap();
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();assert!(staged.publish(&project,&guard,&t,AUTHORITY,||Ok(())).is_err());
        assert_eq!(fs::read(outside.join("artifact")).unwrap(),b"outside");assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");
        assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_some());
    }
    #[test]
    fn failure_after_report_publish_retains_intent_until_receipt_commit() {
        let (_root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();
        let mut files=0;
        assert!(resume_with(&project,&guard,&t.id,AUTHORITY,&mut ||Ok(()),||{files+=1;if files==2 {anyhow::bail!("receipt not committed");}Ok(())}).is_err());
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        let pending=thread::load(&project,&t.id).unwrap();assert_eq!(pending.report_hash,t.report_hash);assert!(pending.pending_live_copy.is_some());
        fs::write(Path::new(&t.thread_dir).join("report.md"),b"old").unwrap();
        resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).unwrap();
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        assert_eq!(thread::load(&project,&t.id).unwrap().report_hash,thread::sha256_hex(b"report\0\xff"));
    }
    #[test]
    fn full_stage_inventory_blocks_new_receive_but_allows_exact_recovery() {
        let (root,project,t,staged)=fixture();let guard=ProjectGuard::acquire(&project.dir()).unwrap();
        staged.begin(&project,&guard,&t,AUTHORITY,&mut ||Ok(())).unwrap();
        let parent=project.state_dir().join("live-copies");
        for n in 1..STAGE_LIMIT {fs::create_dir(parent.join(format!("orphan-{n}"))).unwrap();}
        assert!(receive(&project,&root.path().join("archive")).is_err());
        assert_eq!(fs::read_dir(&parent).unwrap().count(),STAGE_LIMIT);
        resume(&project,&guard,&t.id,AUTHORITY,||Ok(())).unwrap();
        assert!(thread::load(&project,&t.id).unwrap().pending_live_copy.is_none());
        assert_eq!(fs::read_dir(parent).unwrap().count(),STAGE_LIMIT-1);
    }
}
