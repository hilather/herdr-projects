//! Read immutable output evidence without consulting the former worker directory.
use super::*;
use std::path::Component;

/// Constructed only after verifying the complete manifest and every referenced blob.
pub struct VerifiedOutputs {manifest:OutputManifest,blobs:BTreeMap<String,Vec<u8>>}
impl VerifiedOutputs {
    pub fn manifest(&self)->&OutputManifest {&self.manifest}
    pub fn bytes(&self,entry:&Entry)->Option<&[u8]> {self.blobs.get(&entry.sha256).map(Vec::as_slice)}
    pub fn report_hash(&self)->Option<&str> {self.manifest.entries.iter().find(|e|e.path=="report.md"&&!e.directory).map(|e|e.sha256.as_str())}
}
fn valid_hash(value:&str)->bool {value.len()==64&&value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn read(directory:&Directory,name:&str,limit:u64,budget:&mut Budget)->Result<Vec<u8>> {
    let mut file=directory.child(OsStr::new(name))?;let before=file.metadata()?;
    budget.size(&file)?;ensure!(before.len()<=limit,"retained output file exceeds bounds");
    let mut bytes=Vec::new();let mut buffer=[0;65536];
    loop {let n=budget.read(&mut file,&mut buffer)?;if n==0 {break;}ensure!(bytes.len() as u64+n as u64<=limit,"retained output file exceeds bounds");bytes.extend_from_slice(&buffer[..n]);}
    source_tree::unchanged(&file,&before)?;Ok(bytes)
}

/// The caller supplies receipt-bound identity, not a filesystem-selected snapshot.
/// This read does not authorize task acceptance or deleting any source.
pub fn load_outputs(project:&Path,attempt:&AttemptId,reference:&AttemptOutputReference,control:&Control)->Result<VerifiedOutputs> {
    control.check()?;
    ensure!(attempt.as_str().strip_prefix("attempt-").is_some_and(valid_hash),"invalid canonical output attempt");
    let digest=reference.digest.as_deref().context("termination recorded no output directory")?;
    ensure!(valid_hash(digest),"invalid retained output digest");
    let path=project.join(".state/worker-output-snapshots").join(attempt.as_str()).join(digest);
    let directory=Directory::open(&path)?;
    let bytes=read(&directory,"manifest.json",4*1024*1024,&mut control.budget())?;
    ensure!(hash(&bytes)==digest,"retained output manifest digest mismatch");
    let manifest:OutputManifest=serde_json::from_slice(&bytes)?;
    ensure!(manifest.version==1&&&manifest.attempt==attempt&&manifest.source==reference.source,"retained output identity mismatch");
    let mut budget=control.budget();let mut paths=BTreeMap::new();let mut blobs=BTreeMap::new();
    for entry in &manifest.entries {
        ensure!(!entry.symlink,"output snapshot contains an unsupported link");
        let relative=Path::new(&entry.path);
        ensure!(!entry.path.is_empty()&&entry.path.len()<=4096&&relative.components().all(|p|matches!(p,Component::Normal(_)))
            &&relative.components().collect::<PathBuf>().as_os_str()==OsStr::new(&entry.path),"invalid retained output entry path");
        budget.entry(relative.components().count()-1)?;
        ensure!(!paths.contains_key(&entry.path),"duplicate retained output entry");
        if let Some(parent)=relative.parent().filter(|p|!p.as_os_str().is_empty()) {
            ensure!(paths.get(parent.to_str().unwrap())==Some(&true),"retained output parent is missing or not a directory");
        }
        paths.insert(entry.path.clone(),entry.directory);
        if entry.directory {ensure!(entry.bytes==0&&entry.sha256.is_empty()&&!entry.executable,"invalid retained output directory");}
        else {
            ensure!(valid_hash(&entry.sha256),"invalid retained output blob digest");
            let bytes=read(&directory,&entry.sha256,entry.bytes,&mut budget)?;
            ensure!(bytes.len() as u64==entry.bytes&&hash(&bytes)==entry.sha256,"retained output blob mismatch");
            blobs.insert(entry.sha256.clone(),bytes);
        }
    }
    directory.matches_path(&path)?;control.check()?;
    Ok(VerifiedOutputs{manifest,blobs})
}

/// Resolve only canonical termination evidence for this exact runtime binding.
/// Historical bindings without output receipts continue to use their local source.
pub fn load_binding_outputs(project:&Path,state:&crate::domain::Snapshot,binding:&RuntimeBinding,control:&Control)->Result<Option<VerifiedOutputs>> {
    control.check()?;
    let mut selected=None;
    for event in state.events.iter().filter(|e|e.kind=="runtime.worker_terminated") {
        let receipt:WorkerTerminationReceipt=serde_json::from_value(event.payload.clone())?;
        if receipt.binding!=binding.id {continue;}
        ensure!(event.payload_version==1&&event.entity==receipt.attempt.as_str(),"output event identity mismatch");
        ensure!(selected.is_none(),"duplicate output termination evidence");
        selected=Some(receipt);
    }
    let Some(receipt)=selected else{return Ok(None);};
    let Some(reference)=receipt.output_snapshot.as_ref() else{return Ok(None);};
    let attempt=state.attempts.iter().find(|a|a.id==receipt.attempt).context("output attempt missing")?;
    ensure!(receipt.version==1&&receipt.binding_revision==binding.revision&&attempt.termination_observed&&!attempt.retains_capacity()
        &&binding.task.as_ref()==Some(&attempt.task),"output termination binding changed");
    let record=state.attempt_inputs.iter().find(|r|r.attempt==attempt.id&&r.operation==receipt.launch).context("output launch inputs missing")?;
    let source=worker_output_path(&record.inputs,&attempt.id).map_err(anyhow::Error::msg)?;
    ensure!(record.inputs.binding==binding.id&&record.inputs.task==attempt.task&&reference.source==source&&binding.identity.thread_dir==source&&binding.identity.machine.is_empty()
        &&Path::new(&record.inputs.project_store)==project.join(".state/state.db"),"output source identity mismatch");
    Ok(Some(load_outputs(project,&attempt.id,reference,control)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn fixture()->(tempfile::TempDir,AttemptId,AttemptOutputReference,OutputManifest) {
        let root=tempfile::tempdir().unwrap();let attempt=AttemptId::new(format!("attempt-{}","a".repeat(64))).unwrap();
        let source=root.path().join(".state/worker-output").join(attempt.as_str()).to_str().unwrap().to_owned();
        let manifest=OutputManifest{version:1,attempt:attempt.clone(),source:source.clone(),entries:vec![
            Entry{symlink:false,path:"report.md".into(),directory:false,executable:false,bytes:6,sha256:hash(b"report")},
            Entry{symlink:false,path:"library".into(),directory:true,executable:false,bytes:0,sha256:String::new()},
            Entry{symlink:false,path:"library/binary".into(),directory:false,executable:true,bytes:3,sha256:hash(&[0,255,1])},
        ]};
        (root,attempt,AttemptOutputReference{source,digest:None},manifest)
    }
    fn publish(root:&Path,attempt:&AttemptId,reference:&mut AttemptOutputReference,manifest:&OutputManifest)->PathBuf {
        let bytes=serde_json::to_vec(manifest).unwrap();let digest=hash(&bytes);reference.digest=Some(digest.clone());
        let path=root.join(".state/worker-output-snapshots").join(attempt.as_str()).join(digest);fs::create_dir_all(&path).unwrap();
        fs::write(path.join("manifest.json"),bytes).unwrap();fs::write(path.join(hash(b"report")),b"report").unwrap();fs::write(path.join(hash(&[0,255,1])),[0,255,1]).unwrap();path
    }
    #[test]
    fn reads_without_source_and_refuses_corruption_or_unsafe_storage() {
        for change in ["none","manifest","blob","missing","link","hardlink","directory","cancelled","deadline","identity","absent"] {
            let(root,attempt,mut reference,manifest)=fixture();let path=publish(root.path(),&attempt,&mut reference,&manifest);let mut control=Control::default();
            let blob=path.join(hash(b"report"));
            match change {
                "manifest"=>fs::write(path.join("manifest.json"),b"{}").unwrap(),
                "blob"=>fs::write(&blob,b"REPORT").unwrap(),
                "missing"=>fs::remove_file(&blob).unwrap(),
                "link"|"hardlink"=>{let outside=root.path().join("outside");fs::write(&outside,b"report").unwrap();fs::remove_file(&blob).unwrap();if change=="link"{std::os::unix::fs::symlink(outside,&blob).unwrap();}else{fs::hard_link(outside,&blob).unwrap();}},
                "directory"=>{let moved=root.path().join("moved");fs::rename(&path,&moved).unwrap();std::os::unix::fs::symlink(moved,&path).unwrap();},
                "cancelled"=>control.cancellation.cancel(),"deadline"=>control.deadline=Instant::now(),
                "identity"=>reference.source.push_str("-other"),"absent"=>reference.digest=None,_=>{},
            }
            let result=load_outputs(root.path(),&attempt,&reference,&control);
            if change=="none" {let outputs=result.unwrap();assert_eq!(outputs.report_hash(),Some(hash(b"report").as_str()));assert_eq!(outputs.bytes(&outputs.manifest().entries[2]).unwrap(),[0,255,1]);assert!(!Path::new(&reference.source).exists());}else{assert!(result.is_err(),"{change}");}
        }
    }
    #[test]
    fn digest_valid_manifests_still_require_safe_bounded_structure() {
        for change in ["traversal","absolute","dot","double","trailing","duplicate","missing-parent","file-parent","directory-data","size","hash","depth","entries","version","attempt","symlink"] {
            let(root,attempt,mut reference,mut manifest)=fixture();
            match change {
                "traversal"=>manifest.entries[0].path="../escape".into(),"absolute"=>manifest.entries[0].path="/escape".into(),
                "dot"=>manifest.entries[0].path="./report.md".into(),"double"=>manifest.entries[2].path="library//binary".into(),
                "trailing"=>manifest.entries[0].path="report.md/".into(),
                "duplicate"=>manifest.entries.push(manifest.entries[0].clone()),"missing-parent"=>{manifest.entries.remove(1);},
                "file-parent"=>manifest.entries[2].path="report.md/child".into(),"directory-data"=>manifest.entries[1].bytes=1,
                "size"=>manifest.entries[0].bytes=7,"hash"=>manifest.entries[0].sha256="../escape".into(),
                "depth"=>{manifest.entries.clear();for n in 1..=66{manifest.entries.push(Entry{symlink:false,path:vec!["a";n].join("/"),directory:true,executable:false,bytes:0,sha256:String::new()});}},
                "entries"=>{manifest.entries=(0..=source_tree::ENTRY_LIMIT).map(|n|Entry{symlink:false,path:format!("dir-{n}"),directory:true,executable:false,bytes:0,sha256:String::new()}).collect();},
                "symlink"=>manifest.entries[0].symlink=true,
                "version"=>manifest.version=2,"attempt"=>manifest.attempt=AttemptId::new(format!("attempt-{}","b".repeat(64))).unwrap(),_=>unreachable!(),
            }
            publish(root.path(),&attempt,&mut reference,&manifest);
            assert!(load_outputs(root.path(),&attempt,&reference,&Control::default()).is_err(),"{change}");
        }
    }
}
