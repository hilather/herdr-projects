//! Reclaim unreferenced staging only after exclusive managed project ownership.
use super::*;
use herdr_projects::execution_guard::ProjectGuard;

fn decimal(text:&str)->bool {text.parse::<u64>().is_ok_and(|n|n.to_string()==text)}
fn owned_name(name:&str)->bool {
    if let Some(rest)=name.strip_prefix(".download-").or_else(||name.strip_prefix(".stage-")) {
        return rest.split_once('-').is_some_and(|(pid,sequence)|decimal(pid)&&pid!="0"&&decimal(sequence));
    }
    let name=name.strip_prefix("final-").unwrap_or(name);
    let Some((prefix,digest))=name.rsplit_once('-') else{return false;};
    let Some((id,sequence))=prefix.rsplit_once('-') else{return false;};
    thread::validate_id(id).is_ok()&&decimal(sequence)&&sequence!="0"&&digest.len()==64&&digest.bytes().all(|b|b.is_ascii_hexdigit())
}
fn references(project:&Project,control:&Control)->Result<BTreeSet<String>> {
    let directory=Directory::open(&project.dir().join("threads"))?;let before=directory.metadata()?;
    let mut budget=control.budget();let mut referenced=BTreeSet::new();
    for name in directory.names(&budget)? {
        budget.entry(0)?;
        let Some(name_text)=name.to_str() else {anyhow::bail!("non-UTF-8 thread inventory; staging retained");};
        let Some(id)=name_text.strip_suffix(".toml") else {continue;};
        thread::validate_id(id)?;
        ensure!(directory.kind(&name)?==Some(NodeKind::File),"invalid thread record; staging retained");
        let mut file=directory.child(&name)?;let metadata=file.metadata()?;
        ensure!(metadata.len()<=16*1024*1024,"thread record exceeds bounds; staging retained");
        let mut bytes=Vec::new();let mut buffer=[0u8;8192];
        loop {let n=budget.read(&mut file,&mut buffer)?;if n==0{break;}bytes.extend_from_slice(&buffer[..n]);ensure!(bytes.len()<=16*1024*1024,"thread record exceeds bounds; staging retained");}
        crate::source_tree::unchanged(&file,&metadata)?;
        let text=std::str::from_utf8(&bytes)?;let record:Thread=toml::from_str(text)?;
        let input:toml::Value=toml::from_str(text)?;let known=toml::Value::try_from(&record)?;
        ensure!(input.as_table().context("invalid thread record")?.keys().all(|key|known.get(key).is_some()),"unknown thread fields; staging retained");
        ensure!(record.id==id,"thread filename identity mismatch; staging retained");thread::copy_delivery::validate(&record)?;
        if let Some(intent)=record.pending_live_copy {referenced.insert(intent.stage_name(id));}
        if let Some(intent)=record.pending_final_copy {referenced.insert(intent.stage_name(id));}
    }
    directory.unchanged(&before)?;directory.matches_path(&project.dir().join("threads"))?;Ok(referenced)
}
/// Called at worker ingress before reserving a new spool. Recovery bypasses this
/// path entirely, including when inventory or unrelated records are corrupt.
pub fn make_room(project:&Project,guard:&ProjectGuard,control:&Control)->Result<usize> {
    control.check()?;guard.check_project(&project.dir())?;crate::project::ensure_legacy(&project.dir())?;
    let _lock=project.lock()?;ensure!(project.try_status()?==crate::project::Status::Active,"project is not active");
    let state=Directory::open(&project.state_dir())?;
    match state.kind(OsStr::new("live-copies"))? {None=>return Ok(0),Some(NodeKind::Directory)=>{},_=>anyhow::bail!("invalid live staging directory")}
    let directory=state.directory(Path::new("live-copies"))?;let budget=control.budget();let names=directory.names(&budget)?;
    // Spool admission reserves one additional slot for extracted bytes.
    if names.len()<STAGE_LIMIT-1 {return Ok(0);}
    let referenced=references(project,control)?;let mut removed=0;
    for name in names {
        control.check()?;let Some(text)=name.to_str() else {continue;};
        if !owned_name(text)||referenced.contains(text)||directory.kind(&name)?!=Some(NodeKind::Directory){continue;}
        guard.check_project(&project.dir())?;directory.matches_path(&project.state_dir().join("live-copies"))?;
        directory.remove_owned_tree(&name,&mut control.staging_cleanup_budget())?;removed+=1;
    }
    control.check()?;Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,Project) {
        let root=tempfile::tempdir().unwrap();let project=crate::project::create(root.path(),"demo","",vec![]).unwrap();
        fs::create_dir_all(project.state_dir().join("live-copies")).unwrap();(root,project)
    }
    fn fill(project:&Project,count:usize) {
        for n in 0..count {let dir=project.state_dir().join("live-copies").join(format!(".download-123-{n}"));fs::create_dir(&dir).unwrap();fs::write(dir.join("stream"),b"abandoned").unwrap();}
    }
    #[test]
    fn full_inventory_reclaims_only_unreferenced_owned_stages() {
        let(_root,project)=fixture();let hash="a".repeat(64);
        let live=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.live_copy_sequence=1;t.pending_live_copy=Some(herdr_projects::live_copy_intent::LiveCopyIntent{sequence:1,execution:hash.clone(),authority:hash.clone(),previous_hash:String::new(),previous_receipt:None,report_hash:hash.clone(),stage_digest:hash.clone()});}).unwrap();
        let final_copy=thread::allocate(&project,|t|{t.status=thread::Status::Open;t.final_copy_sequence=1;t.pending_final_copy=Some(herdr_projects::final_copy_intent::FinalCopyIntent{sequence:1,execution:hash.clone(),authority:hash.clone(),previous_hash:String::new(),previous_receipt:None,report_hash:None,stage_digest:hash.clone(),snapshot:None,operation:"merged-fixture".into(),purpose:herdr_projects::final_copy_intent::Purpose::Merged{pr:"https://github.com/example/repo/pull/1".into()}});}).unwrap();
        let names=[live.pending_live_copy.as_ref().unwrap().stage_name(&live.id),final_copy.pending_final_copy.as_ref().unwrap().stage_name(&final_copy.id),"operator-data".into()];
        for name in &names {let dir=project.state_dir().join("live-copies").join(name);fs::create_dir(&dir).unwrap();fs::write(dir.join("marker"),b"retain").unwrap();}
        let unreferenced=project.state_dir().join("live-copies").join(format!("t-9999-1-{hash}"));fs::create_dir(&unreferenced).unwrap();fs::write(unreferenced.join("manifest.json"),b"orphan").unwrap();
        fill(&project,11);let guard=ProjectGuard::acquire(&project.dir()).unwrap();
        assert_eq!(make_room(&project,&guard,&Control::default()).unwrap(),12);
        for name in &names {assert_eq!(fs::read(project.state_dir().join("live-copies").join(name).join("marker")).unwrap(),b"retain");}
        assert_eq!(thread::load(&project,&live.id).unwrap(),live);assert_eq!(thread::load(&project,&final_copy.id).unwrap(),final_copy);
        assert!(!unreferenced.exists());assert!(Spool::reserve(&project,&Control::default()).is_ok());
    }
    #[test]
    fn corrupt_thread_records_prevent_any_reclamation() {
        for variant in ["invalid","mismatch","link","future"] {
            let(root,project)=fixture();fill(&project,15);let path=project.dir().join("threads/t-0001.toml");
            match variant {
                "invalid"=>fs::write(&path,b"not valid [toml").unwrap(),
                "mismatch"=>fs::write(&path,b"id = 't-0002'\n").unwrap(),
                "future"=>fs::write(&path,b"id = 't-0001'\nfuture_copy_intent = 'retained'\n").unwrap(),
                "link"=>{let outside=root.path().join("record");fs::write(&outside,b"id = 't-0001'\n").unwrap();std::os::unix::fs::symlink(outside,&path).unwrap();},
                _=>unreachable!(),
            }
            let guard=ProjectGuard::acquire(&project.dir()).unwrap();assert!(make_room(&project,&guard,&Control::default()).is_err(),"{variant}");
            assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),15);
        }
    }
    #[test]
    fn cancellation_wrong_project_and_unknown_names_preserve_inventory() {
        let(root,project)=fixture();fill(&project,15);let other=crate::project::create(root.path(),"other","",vec![]).unwrap();
        let wrong=ProjectGuard::acquire(&other.dir()).unwrap();assert!(make_room(&project,&wrong,&Control::default()).is_err());drop(wrong);
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();let control=Control::default();control.cancellation.cancel();assert!(make_room(&project,&guard,&control).is_err());
        assert_eq!(fs::read_dir(project.state_dir().join("live-copies")).unwrap().count(),15);
        for name in [".download-0-1",".download-123-01",".stage-1-x","final-t-0001-0-bad","t-0001-1-bad","operator-data"] {assert!(!owned_name(name));}
    }
    #[test]
    fn special_entries_are_not_followed_and_leave_recoverable_remainders() {
        let(root,project)=fixture();fill(&project,15);let outside=root.path().join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("keep"),b"untouched").unwrap();
        let orphan=project.state_dir().join("live-copies/.download-123-0");std::os::unix::fs::symlink(&outside,orphan.join("link")).unwrap();
        let guard=ProjectGuard::acquire(&project.dir()).unwrap();assert!(make_room(&project,&guard,&Control::default()).is_err());
        assert_eq!(fs::read(outside.join("keep")).unwrap(),b"untouched");assert!(orphan.exists());
    }
}
