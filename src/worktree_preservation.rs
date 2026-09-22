//! Durable repository evidence for stopped workers. Capture does not authorize
//! restoration, task-result acceptance, or source deletion.
use std::{collections::BTreeMap,ffi::OsStr,io::Write,os::unix::fs::MetadataExt,path::{Path,PathBuf},time::Instant};
use anyhow::{Context,Result,ensure};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use crate::{domain::*,runner::Cancellation,source_tree::{self,Budget,Control,Directory}};
mod git;
mod outputs;
mod output_reader;
pub use output_reader::{load_outputs,load_binding_outputs,VerifiedOutputs};
pub use outputs::OutputManifest;
pub(crate) use outputs::capture_outputs_held;
pub use git::Archive as GitArchive;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path:String,pub directory:bool,pub executable:bool,pub bytes:u64,pub sha256:String,
    #[serde(default,skip_serializing_if="is_false")]
    pub symlink:bool,
}
fn is_false(value:&bool)->bool {!*value}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version:u32,pub scope:String,pub attempt:AttemptId,pub worktree:WorktreeReceipt,pub entries:Vec<Entry>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub git:Option<GitArchive>,
}
#[derive(Debug,Clone,Serialize)]
pub struct Snapshot {pub digest:String,pub directory:PathBuf,pub manifest:Manifest}
fn hash(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}

fn scan(directory:&Directory,relative:&Path,budget:&mut Budget,entries:&mut Vec<Entry>,blobs:&mut BTreeMap<String,Vec<u8>>,depth:usize,skip_git:bool)->Result<()> {
    let before=directory.metadata()?;
    for name in directory.names(budget)? {
        if skip_git && relative.as_os_str().is_empty() && name==".git" {continue;}
        budget.entry(depth)?;
        let path=relative.join(&name);
        let text=path.to_str().context("checkout snapshot paths must be UTF-8")?.to_owned();
        ensure!(text.len()<=4096,"checkout snapshot path exceeds bounds");
        if skip_git && directory.kind(&name)?==Some(source_tree::NodeKind::Link) {
            let bytes=directory.link_bytes(&name,budget)?;let digest=hash(&bytes);
            entries.push(Entry{path:text,directory:false,executable:false,bytes:bytes.len() as u64,sha256:digest.clone(),symlink:true});
            blobs.entry(digest).or_insert(bytes);continue;
        }
        let file=directory.child(&name)?;
        let metadata=file.metadata()?;
        if metadata.is_dir() {
            entries.push(Entry{symlink:false,path:text,directory:true,executable:false,bytes:0,sha256:String::new()});
            scan(&Directory::from_file(file)?,&path,budget,entries,blobs,depth+1,skip_git)?;
        } else {
            budget.size(&file)?;
            let mut file=file;let mut bytes=Vec::new();let mut buffer=[0u8;65536];
            loop {let n=budget.read(&mut file,&mut buffer)?;if n==0 {break;}bytes.extend_from_slice(&buffer[..n]);}
            source_tree::unchanged(&file,&metadata)?;
            let digest=hash(&bytes);
            entries.push(Entry{symlink:false,path:text,directory:false,executable:metadata.mode()&0o111!=0,bytes:bytes.len() as u64,sha256:digest.clone()});
            blobs.entry(digest).or_insert(bytes);
        }
    }
    directory.unchanged(&before)?;
    budget.check()
}

fn exact_file(directory:&Directory,name:&str,bytes:&[u8],control:&Control)->Result<bool> {
    control.check()?;
    let Some(mut file)=directory.optional(OsStr::new(name))? else {return Ok(false);};
    let before=file.metadata()?;let mut budget=control.budget();budget.size(&file)?;
    ensure!(before.len()==bytes.len() as u64,"retained checkout snapshot size differs");
    let mut read=Vec::new();let mut buffer=[0u8;65536];
    loop {let n=budget.read(&mut file,&mut buffer)?;if n==0 {break;}read.extend_from_slice(&buffer[..n]);}
    source_tree::unchanged(&file,&before)?;
    ensure!(read==bytes,"retained checkout snapshot bytes differ");
    Ok(true)
}
fn retain(directory:&Directory,name:&str,bytes:&[u8],control:&Control)->Result<()> {
    if !exact_file(directory,name,bytes,control)? {
        directory.write_atomic(OsStr::new(name),|file|{control.check()?;file.write_all(bytes)?;control.check()})?;
    }
    ensure!(exact_file(directory,name,bytes,control)?,"checkout snapshot publication missing");
    Ok(())
}

/// Explicit capture after exact termination. No task transition, resource
/// release, Git mutation, source removal or verified-result receipt is produced.
pub fn capture_stopped_files(project:&Path,attempt:&AttemptId,expected_head:u64,deadline:Instant,cancellation:Cancellation)->Result<Vec<Snapshot>> {
    capture(project,attempt,expected_head,deadline,cancellation,false)
}
/// Preserve working files, reachable approved/current history, and indexed
/// objects plus exact index bytes. No restore or deletion authority is granted.
pub fn capture_stopped_repository(project:&Path,attempt:&AttemptId,expected_head:u64,deadline:Instant,cancellation:Cancellation)->Result<Vec<Snapshot>> {
    capture(project,attempt,expected_head,deadline,cancellation,true)
}
fn capture(project:&Path,attempt:&AttemptId,expected_head:u64,deadline:Instant,cancellation:Cancellation,include_git:bool)->Result<Vec<Snapshot>> {
    let control=Control{deadline,cancellation};control.check()?;
    let project=project.canonicalize()?;
    let guard=crate::execution_guard::RootGuard::exclusive(project.parent().context("project root missing")?)?;
    control.check()?;
    let mut db=crate::migration::open_active_controlled(&project,crate::store::controlled::ReadControl::new(deadline,control.cancellation.clone()))?;
    let state=db.read_snapshot(Some(expected_head))?;
    ensure!(state.control.as_ref().context("project control missing")?.state!=ProjectState::Archived,"archived project cannot capture new checkout snapshots");
    let current=state.attempts.iter().find(|a|&a.id==attempt).context("attempt missing")?;
    ensure!(current.termination_observed && !current.retains_capacity(),"checkout capture requires exact attempt termination");
    let record=state.attempt_inputs.iter().find(|r|&r.attempt==attempt).context("retained launch inputs missing")?;
    capture_held(&project,&state,record,&guard,&control,include_git)
}

// Trusted termination calls this only after proving native quiescence, before
// changing canonical disposition. Public capture still requires terminal state.
pub(crate) fn capture_held(project:&Path,state:&crate::domain::Snapshot,record:&AttemptInputRecord,guard:&crate::execution_guard::RootGuard,control:&Control,include_git:bool)->Result<Vec<Snapshot>> {
    control.check()?;
    let deadline=control.deadline;
    let proof=crate::worktree_preparation::pin_started_held(project,state,record,deadline,control.cancellation.clone())?;
    if record.inputs.repositories.is_empty() {return Ok(vec![]);}
    let event=state.events.iter().find(|e|e.kind=="runtime.worktrees_ready" && e.entity==record.operation.as_str()).context("ready worktree evidence missing")?;
    let receipts:Vec<WorktreeReceipt>=serde_json::from_value(event.payload.clone())?;
    capture_receipts(project,record,proof,receipts,guard,control,include_git)
}
fn capture_receipts(project:&Path,record:&AttemptInputRecord,proof:crate::worktree_preparation::WorktreeProof,receipts:Vec<WorktreeReceipt>,guard:&crate::execution_guard::RootGuard,control:&Control,include_git:bool)->Result<Vec<Snapshot>> {
    let attempt=&record.attempt;let deadline=control.deadline;
    let git=crate::worktree_preparation::Git{deadline,cancellation:control.cancellation.clone(),locks:guard.inherit()?};
    let mut remaining=source_tree::BYTE_LIMIT as usize;
    let mut budget=control.budget();let mut captured=Vec::new();
    for receipt in receipts {
        let source=Directory::open(Path::new(&receipt.plan.path))?;
        let mut entries=Vec::new();let mut blobs=BTreeMap::new();
        scan(&source,Path::new(""),&mut budget,&mut entries,&mut blobs,0,true)?;
        source.matches_path(Path::new(&receipt.plan.path))?;
        let archive=if include_git {Some(git::capture(&git,&receipt,&control,&mut remaining,&mut blobs)?)}else{None};
        captured.push((source,Manifest{version:if entries.iter().any(|e|e.symlink){3}else if include_git{2}else{1},scope:if include_git{"repository_state"}else{"working_files"}.into(),attempt:attempt.clone(),worktree:receipt,entries,git:archive},blobs));
    }
    // Verify again using bytes, not Git status or a size/mtime shortcut.
    let mut budget=control.budget();
    for (source,manifest,_) in &captured {
        let mut entries=Vec::new();let mut blobs=BTreeMap::new();
        scan(source,Path::new(""),&mut budget,&mut entries,&mut blobs,0,true)?;
        ensure!(entries==manifest.entries,"checkout changed during capture");
        source.matches_path(Path::new(&manifest.worktree.plan.path))?;
        if let Some(archive)=&manifest.git {git::verify(&git,&manifest.worktree,&control,archive)?;}
    }
    proof.check()?;control.check()?;
    let state_dir=Directory::open(&project.join(".state"))?;
    let root=state_dir.create_dir(OsStr::new("worktree-file-snapshots"))?;
    let parent=root.create_dir(OsStr::new(attempt.as_str()))?;
    // Bound cumulative retained snapshots. Never reclaim accepted or incomplete
    // evidence implicitly to make space for another capture.
    let inventory=parent.names(&control.budget())?;
    ensure!(inventory.len()<=1024,"checkout snapshot inventory is full");
    let mut results=Vec::new();
    for (_,manifest,blobs) in captured {
        let bytes=serde_json::to_vec(&manifest)?;ensure!(bytes.len()<=4*1024*1024,"checkout manifest exceeds bounds");
        let digest=hash(&bytes);
        if parent.kind(OsStr::new(&digest))?.is_none() {ensure!(inventory.len()+results.len()<1024,"checkout snapshot inventory is full");}
        let directory=parent.create_dir(OsStr::new(&digest))?;
        let names=directory.names(&control.staging_cleanup_budget())?;
        ensure!(names.iter().filter(|name|name.as_encoded_bytes().starts_with(b".live-")).count()<16,"checkout snapshot temporary inventory is full");
        for (id,body) in blobs {retain(&directory,&id,&body,&control)?;}
        // Manifest is the completion marker and is published only after every
        // content-addressed file has been persisted and read back successfully.
        if let Some(archive)=&manifest.git {git::verify(&git,&manifest.worktree,&control,archive)?;}
        proof.check()?;control.check()?;
        state_dir.matches_path(&project.join(".state"))?;
        root.matches_path(&project.join(".state/worktree-file-snapshots"))?;
        parent.matches_path(&project.join(".state/worktree-file-snapshots").join(attempt.as_str()))?;
        let published=project.join(".state/worktree-file-snapshots").join(attempt.as_str()).join(&digest);
        directory.matches_path(&published)?;
        retain(&directory,"manifest.json",&bytes,&control)?;
        directory.matches_path(&published)?;
        results.push(Snapshot{digest,directory:published,manifest});
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn entries(path:&Path,control:&Control)->Result<Vec<Entry>> {
        let mut result=Vec::new();let mut blobs=BTreeMap::new();
        scan(&Directory::open(path)?,Path::new(""),&mut control.budget(),&mut result,&mut blobs,0,true)?;
        Ok(result)
    }
    #[test]
    fn file_scan_detects_same_size_same_mtime_edits_and_honors_control() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("result");
        fs::write(&path,b"first").unwrap();let before=fs::metadata(&path).unwrap().modified().unwrap();
        let first=entries(root.path(),&Control::default()).unwrap();
        fs::write(&path,b"other").unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_times(fs::FileTimes::new().set_modified(before)).unwrap();
        assert_ne!(first,entries(root.path(),&Control::default()).unwrap());
        let cancelled=Control::default();cancelled.cancellation.cancel();
        assert!(entries(root.path(),&cancelled).is_err());
        assert!(entries(root.path(),&Control{deadline:Instant::now(),cancellation:Default::default()}).is_err());
    }
    #[test]
    fn special_sources_are_rejected_without_following_or_waiting() {
        let root=tempfile::tempdir().unwrap();let outside=tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private"),b"unchanged").unwrap();
        let path=root.path().join("entry");
        std::os::unix::fs::symlink(outside.path(),&path).unwrap();
        let captured=entries(root.path(),&Control::default()).unwrap();assert_eq!(captured.len(),1);
        assert!(captured[0].symlink);assert_eq!(captured[0].sha256,hash(outside.path().as_os_str().as_encoded_bytes()));
        assert_eq!(fs::read(outside.path().join("private")).unwrap(),b"unchanged");fs::remove_file(&path).unwrap();
        let name=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: live NUL-terminated pathname and valid mode.
        assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);
        assert!(entries(root.path(),&Control::default()).is_err());fs::remove_file(&path).unwrap();
        fs::hard_link(outside.path().join("private"),&path).unwrap();
        assert!(entries(root.path(),&Control::default()).is_err());
        assert_eq!(fs::read(outside.path().join("private")).unwrap(),b"unchanged");
    }
    #[test]
    fn interrupted_file_publication_resumes_without_replacing_conflicting_bytes() {
        let root=tempfile::tempdir().unwrap();let directory=Directory::open(root.path()).unwrap();
        let bytes=b"partial worker result";let id=hash(bytes);
        retain(&directory,&id,bytes,&Control::default()).unwrap();
        assert!(!root.path().join("manifest.json").exists());
        retain(&directory,&id,bytes,&Control::default()).unwrap();
        fs::write(root.path().join(&id),b"corrupt").unwrap();
        assert!(retain(&directory,&id,bytes,&Control::default()).is_err());
        assert_eq!(fs::read(root.path().join(&id)).unwrap(),b"corrupt");
    }
}


pub(crate) fn capture_preparation_held(project:&Path,state:&crate::domain::Snapshot,record:&AttemptInputRecord,intent:&WorktreeCreation,guard:&crate::execution_guard::RootGuard,control:&Control)->Result<(Vec<PreparationSnapshotReference>,AttemptOutputReference)> {
    control.check()?;
    let git=crate::worktree_preparation::Git{deadline:control.deadline,cancellation:control.cancellation.clone(),locks:guard.inherit()?};
    let(proof,receipts,missing)=crate::worktree_preparation::pin_preparation_held(&git,state,intent)?;
    let captured=capture_receipts(project,record,proof,receipts,guard,control,true)?;
    let output=capture_outputs_held(project,record,control)?;
    for plan in &missing {crate::worktree_preparation::verify_uncreated(&git,plan)?;}
    let references=intent.plans.iter().map(|plan|->Result<PreparationSnapshotReference>{
        let digest=if missing.contains(plan){None}else{Some(captured.iter().find(|s|&s.manifest.worktree.plan==plan).context("preparation snapshot coverage missing")?.digest.clone())};
        Ok(PreparationSnapshotReference{plan:plan.clone(),digest})
    }).collect::<Result<Vec<_>>>()?;
    control.check()?;Ok((references,output))
}
