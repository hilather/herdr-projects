//! Capture the dedicated output directory after native quiescence. Partial
//! reports are evidence candidates; no report or task success is required.
use super::*;
use crate::source_tree::NodeKind;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputManifest {
    pub version:u32,pub attempt:AttemptId,pub source:String,pub entries:Vec<Entry>,
}

pub(crate) fn capture_outputs_held(project:&Path,record:&AttemptInputRecord,control:&Control)->Result<AttemptOutputReference> {
    control.check()?;
    let path=worker_output_path(&record.inputs,&record.attempt).map_err(anyhow::Error::msg)?;
    let state=Directory::open(&project.join(".state"))?;
    let absent=||AttemptOutputReference{source:path.clone(),digest:None};
    match state.kind(OsStr::new("worker-output"))? {
        None=>{state.matches_path(&project.join(".state"))?;control.check()?;return Ok(absent());},
        Some(NodeKind::Directory)=>{},_=>anyhow::bail!("worker output parent is not a real directory"),
    }
    let output_parent=state.directory(Path::new("worker-output"))?;
    match output_parent.kind(OsStr::new(record.attempt.as_str()))? {
        None=>{
            output_parent.matches_path(&project.join(".state/worker-output"))?;
            state.matches_path(&project.join(".state"))?;control.check()?;return Ok(absent());
        },
        Some(NodeKind::Directory)=>{},_=>anyhow::bail!("worker output source is not a real directory"),
    }
    let source=output_parent.directory(Path::new(record.attempt.as_str()))?;
    let mut entries=Vec::new();let mut blobs=BTreeMap::new();
    scan(&source,Path::new(""),&mut control.budget(),&mut entries,&mut blobs,0,false)?;
    let mut repeated=Vec::new();
    scan(&source,Path::new(""),&mut control.budget(),&mut repeated,&mut BTreeMap::new(),0,false)?;
    ensure!(entries==repeated,"worker outputs changed during preservation");
    source.matches_path(Path::new(&path))?;
    let manifest=OutputManifest{version:1,attempt:record.attempt.clone(),source:path.clone(),entries};
    let bytes=serde_json::to_vec(&manifest)?;ensure!(bytes.len()<=4*1024*1024,"worker output manifest exceeds bounds");
    let digest=hash(&bytes);
    let root=state.create_dir(OsStr::new("worker-output-snapshots"))?;
    let parent=root.create_dir(OsStr::new(record.attempt.as_str()))?;
    let inventory=parent.names(&control.budget())?;
    ensure!(inventory.len()<=1024 && (inventory.len()<1024 || parent.kind(OsStr::new(&digest))?.is_some()),"worker output snapshot inventory is full");
    let directory=parent.create_dir(OsStr::new(&digest))?;
    let names=directory.names(&control.staging_cleanup_budget())?;
    ensure!(names.iter().filter(|n|n.as_encoded_bytes().starts_with(b".live-")).count()<16,"worker output temporary inventory is full");
    for (id,body) in blobs {retain(&directory,&id,&body,control)?;}
    source.matches_path(Path::new(&path))?;control.check()?;
    let published=project.join(".state/worker-output-snapshots").join(record.attempt.as_str()).join(&digest);
    state.matches_path(&project.join(".state"))?;
    root.matches_path(&project.join(".state/worker-output-snapshots"))?;
    parent.matches_path(published.parent().unwrap())?;directory.matches_path(&published)?;
    retain(&directory,"manifest.json",&bytes,control)?;
    directory.matches_path(&published)?;control.check()?;
    Ok(AttemptOutputReference{source:path,digest:Some(digest)})
}
