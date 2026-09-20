//! Recoverable report-optional projection followed by one atomic finalization.
use super::*;
use herdr_projects::{execution_guard::ProjectGuard,final_copy_intent::{FinalCopyIntent,Purpose}};

fn eligible(project:&Project,guard:&ProjectGuard,t:&Thread)->Result<()> {
    guard.check_project(&project.dir())?;crate::project::ensure_legacy(&project.dir())?;
    ensure!(project.try_status()?==crate::project::Status::Active&&t.status==thread::Status::Open&&t.removal.is_none(),"thread is not eligible for final-copy publication");
    thread::copy_delivery::validate(t)
}
fn load_stage(project:&Project,t:&Thread,intent:&FinalCopyIntent,control:&Control)->Result<(PathBuf,Source)> {
    control.check()?;intent.validate()?;thread::validate_id(&t.id)?;
    let path=project.state_dir().join("live-copies").join(intent.stage_name(&t.id));
    let root=Directory::open(&path)?;let mut bytes=Vec::new();root.file(Path::new("manifest.json"))?.take(MANIFEST_LIMIT as u64+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=MANIFEST_LIMIT&&thread::sha256_hex(&bytes)==intent.stage_digest,"retained final-copy manifest is missing or corrupt");
    let source:Source=serde_json::from_slice(&bytes)?;projection::verify(&path,&source,control)?;
    ensure!(source.report.as_ref().map(|e|e.sha256.as_str())==intent.report_hash.as_deref(),"final-copy report does not match intent");
    match &intent.snapshot {
        Some(id)=>{
            ensure!(source.omissions.is_empty(),"partial final copy cannot have preservation evidence");
            let manifest=load_mode_controlled(project,t,id,false,control)?;
            ensure!(manifest.generation==t.lifecycle_generation&&manifest.machine==t.machine&&manifest.source==t.thread_dir
                &&manifest.entries==entries(&source).cloned().collect::<Vec<_>>(),"final-copy preservation identity or bytes changed");
        },
        None=>ensure!(!source.omissions.is_empty(),"complete final copy requires preservation evidence"),
    }
    root.matches_path(&path)?;control.check()?;Ok((path,source))
}
fn check(project:&Project,guard:&ProjectGuard,id:&str,intent:&FinalCopyIntent,authority:&str)->Result<Thread> {
    let t=thread::load(project,id)?;eligible(project,guard,&t)?;thread::final_copy::check(&t,intent)?;
    ensure!(intent.authority==authority,"final-copy authority changed");Ok(t)
}
impl LiveCopy {
    /// `authorize` errors withdraw effect permission; false means resolution is
    /// no longer eligible. Before intent creation either outcome refuses work.
    pub fn begin_final_controlled(self,project:&Project,guard:&ProjectGuard,expected:&Thread,authority:&str,operation:&str,purpose:Purpose,control:&Control,mut authorize:impl FnMut()->Result<bool>)->Result<()> {
        control.check()?;eligible(project,guard,expected)?;thread::copy_delivery::ready(expected)?;purpose.validate()?;
        ensure!(authorize()?&&thread::final_copy::resolution_eligible(project,expected,&purpose)?,"automatic finalization is no longer eligible");
        ensure!(self.staging.0.parent()==Some(project.state_dir().join("live-copies").as_path()),"final stage belongs to another project");
        projection::verify(&self.staging.0,&self.source,control)?;
        let snapshot=self.preserve_controlled(project,expected,control,||{ensure!(authorize()?&&thread::final_copy::resolution_eligible(project,expected,&purpose)?,"automatic finalization is no longer eligible");Ok(())})?.map(|s|s.id);
        ensure!(authorize()?&&thread::final_copy::resolution_eligible(project,expected,&purpose)?,"automatic finalization is no longer eligible");
        let bytes=serde_json::to_vec(&self.source)?;ensure!(bytes.len()<=MANIFEST_LIMIT,"final-copy manifest exceeds bounds");
        let intent=FinalCopyIntent{sequence:expected.final_copy_sequence.checked_add(1).context("final-copy sequence exhausted")?,execution:thread::execution_fingerprint(expected),authority:authority.into(),
            previous_hash:expected.report_hash.clone(),previous_receipt:expected.copy_receipt.clone(),report_hash:self.report_hash().map(str::to_owned),stage_digest:thread::sha256_hex(&bytes),snapshot,operation:operation.into(),purpose};intent.validate()?;
        let mut manifest=OpenOptions::new().write(true).create_new(true).mode(0o600).open(self.staging.0.join("manifest.json"))?;
        manifest.write_all(&bytes)?;manifest.sync_all()?;File::open(&self.staging.0)?.sync_all()?;control.check()?;
        let parent=self.staging.0.parent().unwrap();let retained=parent.join(intent.stage_name(&expected.id));
        if retained.try_exists()? {load_stage(project,expected,&intent,control)?;}else {fs::rename(&self.staging.0,&retained)?;}
        File::open(parent)?.sync_all()?;File::open(project.state_dir())?.sync_all()?;
        thread::update_checked(project,&expected.id,|current|{
            control.check()?;eligible(project,guard,current)?;thread::copy_delivery::ready(current)?;
            ensure!(thread::final_copy::resolution_eligible(project,current,&intent.purpose)?,"automatic finalization eligibility changed before intent");
            ensure!(thread::execution_fingerprint(current)==intent.execution&&current.report_hash==intent.previous_hash
                &&current.copy_receipt==intent.previous_receipt&&current.final_copy_sequence==expected.final_copy_sequence,"thread changed before final-copy intent");
            current.final_copy_sequence=intent.sequence;current.pending_final_copy=Some(intent.clone());Ok(())
        })?;
        File::open(project.dir().join("threads"))?.sync_all()?;Ok(())
    }
}
pub fn resume_controlled(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,control:&Control,mut authorize:impl FnMut()->Result<bool>)->Result<()> {
    resume_with(project,guard,id,authority,control,&mut authorize,||Ok(()))
}
fn resume_with(project:&Project,guard:&ProjectGuard,id:&str,authority:&str,control:&Control,authorize:&mut impl FnMut()->Result<bool>,mut after_file:impl FnMut()->Result<()>)->Result<()> {
    control.check()?;let current=thread::load(project,id)?;let intent=current.pending_final_copy.clone().context("no pending final copy")?;
    let current=check(project,guard,id,&intent,authority)?;authorize()?;
    let (path,source)=load_stage(project,&current,&intent,control)?;let stage=Directory::open(&path)?;
    check(project,guard,id,&intent,authority)?;authorize()?;
    let root=Directory::open(&project.dir())?;let mut library=control.budget();
    for entry in &source.library {
        library.check()?;let relative=Path::new(&entry.path).strip_prefix("library")?;
        let base=root.create_dir(OsStr::new("library"))?.create_dir(OsStr::new(id))?;
        if entry.directory {projection::parent(&base,relative,control)?;continue;}
        let destination=projection::parent(&base,relative.parent().context("missing final-copy parent")?,control)?;
        projection::copy_entry(&stage,&destination,relative.file_name().unwrap(),entry,&mut library)?;after_file()?;
    }
    if let Some(report)=&source.report {
        let reports=root.create_dir(OsStr::new("threads"))?;
        projection::copy_entry(&stage,&reports,OsStr::new(&format!("{id}.md")),report,&mut control.budget())?;after_file()?;
    }
    projection::verify_home(&root,id,&source,control)?;root.matches_path(&project.dir())?;
    let resolve=authorize()?;check(project,guard,id,&intent,authority)?;
    thread::final_copy::commit(project,guard,id,&intent,render_notes(&source),resolve,control)?;
    fs::remove_dir_all(&path)?;File::open(path.parent().unwrap())?.sync_all()?;Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const AUTHORITY:&str="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn fixture(mode:&str)->(tempfile::TempDir,Project,Thread,LiveCopy,Purpose) {
        let(root,project,record)=super::super::super::tests::fixture();
        if mode=="missing" {fs::remove_file(Path::new(&record.thread_dir).join("report.md")).unwrap();}
        if mode=="partial" {std::os::unix::fs::symlink("artifact",Path::new(&record.thread_dir).join("library/link")).unwrap();}
        if mode=="omitted-report" {fs::remove_file(Path::new(&record.thread_dir).join("report.md")).unwrap();std::os::unix::fs::symlink("library/artifact",Path::new(&record.thread_dir).join("report.md")).unwrap();}
        let prior=if mode=="missing"||mode=="omitted-report"||mode=="changed-report" {b"old".to_vec()}else{b"report\0\xff".to_vec()};
        let t=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.thread_dir=record.thread_dir;t.report_hash=thread::sha256_hex(&prior);t.last_group=thread::Group::Idle.token().into();t.last_state_change="2020-01-01T00:00:00Z".into();t.last_report_change=t.last_state_change.clone();}).unwrap();
        fs::write(thread::home_report_path(&project,&t.id),&prior).unwrap();
        let purpose=Purpose::Idle{days:7,started:"2020-01-01T00:00:00Z".into(),last_state_change:t.last_state_change.clone(),last_report_change:t.last_report_change.clone()};
        let mut bytes=Vec::new();export(Path::new(&t.thread_dir),&mut bytes).unwrap();let archive=root.path().join("wire");fs::write(&archive,bytes).unwrap();let stage=receive(&project,&archive).unwrap();
        (root,project,t,stage,purpose)
    }
    fn begin(project:&Project,guard:&ProjectGuard,t:&Thread,stage:LiveCopy,purpose:Purpose) {
        stage.begin_final_controlled(project,guard,t,AUTHORITY,"fixture-finalization",purpose,&Control::default(),||Ok(true)).unwrap();
    }
    #[test]
    fn interrupted_projection_recovers_without_source_and_atomically_resolves() {
        let(_root,project,t,stage,purpose)=fixture("complete");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
        let pending=thread::load(&project,&t.id).unwrap();let intent=pending.pending_final_copy.clone().unwrap();assert!(intent.snapshot.is_some());
        fs::remove_dir_all(&t.thread_dir).unwrap();let control=Control::default();
        assert!(resume_with(&project,&guard,&t.id,AUTHORITY,&control,&mut ||Ok(true),||{control.cancellation.cancel();Ok(())}).is_err());
        let current=thread::load(&project,&t.id).unwrap();assert_eq!(current,pending);assert!(thread::update(&project,&t.id,|t|t.status=thread::Status::Resolved).is_err());assert!(thread::copy_delivery::ready(&current).is_err());
        resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).unwrap();let resolved=thread::load(&project,&t.id).unwrap();
        assert_eq!(resolved.status,thread::Status::Resolved);assert_eq!(resolved.resolved_reason,"auto");assert_eq!(resolved.last_finalization,"fixture-finalization");assert_eq!(resolved.artifact_snapshot,intent.snapshot.unwrap());assert!(resolved.pending_final_copy.is_none());assert!(resolved.pending_final_notice.is_some());
        assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");assert!(resolved.copy_receipt.is_some());
        assert!(resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).is_err());
    }
    #[test]
    fn missing_reports_and_partial_stages_keep_distinct_preservation_semantics() {
        for mode in ["missing","partial","omitted-report"] {
            let(_root,project,t,stage,purpose)=fixture(mode);let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
            resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).unwrap();let result=thread::load(&project,&t.id).unwrap();assert_eq!(result.status,thread::Status::Resolved);
            if mode=="missing" {let snapshot=super::super::super::load(&project,&result,&result.artifact_snapshot).unwrap();assert!(snapshot.report_hash().is_none());assert_eq!(result.report_hash,t.report_hash);assert!(result.copy_receipt.is_none());assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"old");}
            else {assert!(result.artifact_snapshot.is_empty());assert!(!result.pending_final_notice.as_ref().unwrap().body.is_empty());}
            if mode=="omitted-report" {assert_eq!(result.report_hash,t.report_hash);assert!(result.copy_receipt.is_none());}
        }
    }
    #[test]
    fn changed_eligibility_completes_copy_without_stranding_intent_or_resolving() {
        let(_root,project,t,stage,purpose)=fixture("complete");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
        resume_with(&project,&guard,&t.id,AUTHORITY,&Control::default(),&mut ||Ok(true),||{thread::update(&project,&t.id,|t|t.last_group=thread::Group::Working.token().into())?;Ok(())}).unwrap();
        let result=thread::load(&project,&t.id).unwrap();assert_eq!(result.status,thread::Status::Open);assert!(result.pending_final_copy.is_none());assert!(result.last_finalization.is_empty());assert!(result.artifact_snapshot.is_empty());assert!(result.pending_final_notice.as_ref().unwrap().summary.contains("prevented"));assert!(result.copy_receipt.is_some());
    }
    #[test]
    fn report_changed_during_idle_final_copy_resets_clock_and_prevents_resolution() {
        let(_root,project,t,stage,purpose)=fixture("changed-report");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
        resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).unwrap();
        let current=thread::load(&project,&t.id).unwrap();assert_eq!(current.status,thread::Status::Open);assert_ne!(current.report_hash,t.report_hash);assert_ne!(current.last_report_change,t.last_report_change);assert!(current.pending_final_copy.is_none());assert!(current.last_finalization.is_empty());
    }
    #[test]
    fn withdrawn_authority_missing_stage_and_corrupt_preservation_never_resolve() {
        for fault in ["authority","stage","snapshot","cancelled"] {
            let(_root,project,t,stage,purpose)=fixture("complete");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
            let pending=thread::load(&project,&t.id).unwrap();let intent=pending.pending_final_copy.as_ref().unwrap();let control=Control::default();
            match fault {
                "stage"=>fs::remove_dir_all(project.state_dir().join("live-copies").join(intent.stage_name(&t.id))).unwrap(),
                "snapshot"=>fs::write(project.state_dir().join("artifacts").join(&t.id).join(intent.snapshot.as_ref().unwrap()).join("report.md"),b"corrupt").unwrap(),
                "cancelled"=>control.cancellation.cancel(),_=>{},
            }
            assert!(resume_controlled(&project,&guard,&t.id,AUTHORITY,&control,||{ensure!(fault!="authority","revoked");Ok(true)}).is_err());assert_eq!(thread::load(&project,&t.id).unwrap(),pending);assert_eq!(fs::read(thread::home_report_path(&project,&t.id)).unwrap(),b"report\0\xff");
        }
    }
    #[test]
    fn merged_finalization_requires_the_published_report_to_keep_its_pr() {
        for mode in ["same-pr","changed-pr","missing-report","stale-observation"] {
            let(root,project,t,old_stage,_)=fixture("complete");drop(old_stage);
            let url="https://github.com/owner/repo/pull/1";let old=format!("PR: {url}\nold\n");
            let t=thread::update(&project,&t.id,|t|{t.pr=url.into();t.pr_state="MERGED".into();t.report_hash=thread::sha256_hex(old.as_bytes());}).unwrap();fs::write(thread::home_report_path(&project,&t.id),old).unwrap();
            let report=Path::new(&t.thread_dir).join("report.md");
            if mode=="missing-report" {fs::remove_file(&report).unwrap();}else {fs::write(&report,format!("PR: {}\nnew\n",if mode=="changed-pr"{"https://github.com/owner/repo/pull/2"}else{url})).unwrap();}
            let mut bytes=Vec::new();export(Path::new(&t.thread_dir),&mut bytes).unwrap();let archive=root.path().join("merged");fs::write(&archive,bytes).unwrap();let stage=receive(&project,&archive).unwrap();
            let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,Purpose::Merged{pr:url.into()});
            if mode=="stale-observation" {thread::update(&project,&t.id,|t|t.pr_state="OPEN".into()).unwrap();}
            resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).unwrap();let current=thread::load(&project,&t.id).unwrap();
            assert_eq!(current.status,if mode=="changed-pr"||mode=="stale-observation"{thread::Status::Open}else{thread::Status::Resolved},"{mode}");assert!(current.pending_final_copy.is_none());
        }
    }
    #[test]
    fn pending_final_copy_excludes_legacy_lifecycle_and_live_copy_paths() {
        let(root,project,t,stage,purpose)=fixture("complete");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);drop(guard);
        let current=thread::load(&project,&t.id).unwrap();let env=crate::paths::Env::for_test(root.path(),&[]);let runner=crate::runner::fake::FakeRunner::new();
        let ctx=crate::paths::Ctx{root:root.path().into(),config_dir:root.path().join("cfg"),env:&env,runner:&runner,detached_ticker:false};
        assert!(crate::threads::resolve(&ctx,&project.slug,&t.id,&crate::threads::ResolveArgs{skip_copy:true,..Default::default()}).unwrap_err().to_string().contains("recover"));
        assert!(crate::lifecycle::delete(&ctx,&project.slug,true).unwrap_err().to_string().contains("recover"));
        assert!(crate::cleanup::remove(&ctx,&project,&current,true).unwrap_err().to_string().contains("recover"));
        assert!(matches!(thread::copy_home_local(&project,&current,true,&runner).outcome,thread::CopyOutcome::Failed(_)));assert_eq!(thread::load(&project,&t.id).unwrap(),current);
    }
    #[test]
    fn completed_notice_replays_once_even_after_handling_and_execution_replacement() {
        let(_root,project,t,stage,purpose)=fixture("partial");let guard=ProjectGuard::acquire(&project.dir()).unwrap();begin(&project,&guard,&t,stage,purpose);
        resume_controlled(&project,&guard,&t.id,AUTHORITY,&Control::default(),||Ok(true)).unwrap();let saved=thread::load(&project,&t.id).unwrap();let notice=saved.pending_final_notice.clone().unwrap();
        crate::inbox::write_once(&project,&notice.id,"thread-state",&t.id,&notice.summary,&notice.body).unwrap();fs::create_dir_all(project.dir().join("inbox/done")).unwrap();fs::rename(project.dir().join("inbox").join(format!("{}.md",notice.id)),project.dir().join("inbox/done").join(format!("{}.md",notice.id))).unwrap();
        thread::update(&project,&t.id,|t|{t.lifecycle_generation+=1;t.status=thread::Status::Open;}).unwrap();thread::copy_delivery::deliver(&project).unwrap();thread::copy_delivery::deliver(&project).unwrap();
        let current=thread::load(&project,&t.id).unwrap();assert_eq!(current.lifecycle_generation,saved.lifecycle_generation+1);assert_eq!(current.status,thread::Status::Open);assert!(current.pending_final_notice.is_none());assert!(crate::inbox::unhandled(&project).is_empty());
    }
}
