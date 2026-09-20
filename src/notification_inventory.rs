//! Bounded, strict inbox identity capture for notification delivery authority.
use std::{collections::BTreeSet,fs,time::{Duration,Instant}};
use anyhow::{Result,Context,ensure};
use crate::{project::Project,paths,source_tree::Control};
const MAX_IDS:usize=1024;
const MAX_ID_BYTES:usize=48*1024;
const MAX_SCAN_BYTES:usize=50*1024*1024;
pub use herdr_projects::notification_claim::Batch;
use herdr_projects::notification_claim::valid_id;
pub fn capture(project:&Project,control:&Control)->Result<Batch> {
    let control=Control{deadline:control.deadline.min(Instant::now()+Duration::from_secs(10)),cancellation:control.cancellation.clone()};control.check()?;
    let seen=paths::read_control_text(&project.state_dir().join("inbox-seen.json"),1024*1024)?;
    let seen:BTreeSet<String>=seen.map(|text|serde_json::from_str(&text).context("invalid inbox seen state")).transpose()?.unwrap_or_default();
    ensure!(seen.len()<=MAX_IDS&&seen.iter().all(|id|valid_id(id))&&seen.iter().map(String::len).sum::<usize>()<=MAX_ID_BYTES,"inbox seen state exceeds identity bounds");
    let dir=project.dir().join("inbox");ensure!(fs::symlink_metadata(&dir)?.is_dir(),"notification inbox must be a real directory");
    let mut ids=BTreeSet::new();let mut bytes=0usize;let mut id_bytes=0usize;
    for (n,entry) in fs::read_dir(&dir)?.enumerate() {
        control.check()?;ensure!(n<2048,"notification inbox exceeds entry budget");let entry=entry?;let path=entry.path();
        if path.extension().is_none_or(|s|s!="md"){continue;}
        let name=path.file_stem().and_then(|s|s.to_str()).filter(|s|valid_id(s)).context("invalid notification inbox filename")?;
        let limit=(MAX_SCAN_BYTES-bytes).min(16*1024*1024);let text=paths::read_control_text(&path,limit)?.context("notification inbox entry disappeared")?;
        bytes=bytes.checked_add(text.len()).context("notification inbox byte overflow")?;ensure!(bytes<=MAX_SCAN_BYTES,"notification inbox exceeds byte budget");
        let item=crate::inbox::parse(&text).context("invalid notification inbox item")?;ensure!(item.id==name,"notification inbox identity mismatch");
        if !seen.contains(name) {
            ensure!(ids.insert(name.to_string()),"duplicate notification inbox identity");id_bytes=id_bytes.checked_add(name.len()).context("notification identity overflow")?;
            ensure!(ids.len()<=MAX_IDS&&id_bytes<=MAX_ID_BYTES,"notification inbox exceeds identity budget");
        }
    }
    control.check()?;let ids:Vec<String>=ids.into_iter().collect();Batch::new(ids)
}
/// Consumption requires a context receipt or a valid handled item; disappearance
/// alone does not authorize another automatic delivery after uncertainty.
pub fn consumed(project:&Project,ids:&[String],control:&Control)->Result<bool> {
    Batch::new(ids.to_vec())?;control.check()?;
    let text=paths::read_control_text(&project.state_dir().join("inbox-seen.json"),1024*1024)?;
    let seen:BTreeSet<String>=text.map(|text|serde_json::from_str(&text).context("invalid inbox seen state")).transpose()?.unwrap_or_default();
    ensure!(seen.len()<=MAX_IDS&&seen.iter().all(|id|valid_id(id))&&seen.iter().map(String::len).sum::<usize>()<=MAX_ID_BYTES,"inbox seen state exceeds identity bounds");
    let done=project.dir().join("inbox/done");let mut bytes=0usize;
    for id in ids {
        control.check()?;if seen.contains(id){continue;}
        let m=match fs::symlink_metadata(&done){Ok(m)=>m,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(false),Err(e)=>return Err(e.into())};ensure!(m.is_dir(),"handled inbox must be a real directory");
        let Some(text)=paths::read_control_text(&done.join(format!("{id}.md")),(MAX_SCAN_BYTES-bytes).min(16*1024*1024))? else{return Ok(false);};
        bytes=bytes.checked_add(text.len()).context("handled inbox byte overflow")?;ensure!(bytes<=MAX_SCAN_BYTES,"handled inbox exceeds byte budget");
        let item=crate::inbox::parse(&text).context("invalid handled inbox item")?;ensure!(item.id==*id,"handled inbox identity mismatch");
    }control.check()?;Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,Project){let t=tempfile::tempdir().unwrap();let p=crate::project::create(t.path(),"demo","",vec![]).unwrap();crate::inbox::write_once(&p,"item-a","test","fixture","A","").unwrap();(t,p)}
    #[test]
    fn strict_inventory_captures_sorted_unseen_ids_and_verifies_consumption() {
        let(_t,p)=fixture();crate::inbox::write_once(&p,"item-b","test","fixture","B","").unwrap();let c=Control::default();assert_eq!(capture(&p,&c).unwrap().ids,["item-a","item-b"]);
        assert!(!consumed(&p,&["item-a".into()],&c).unwrap());crate::inbox::mark_seen(&p,&["item-a".into()]).unwrap();assert_eq!(capture(&p,&c).unwrap().ids,["item-b"]);assert!(consumed(&p,&["item-a".into()],&c).unwrap());
        crate::inbox::done(&p,&["item-b".into()],false).unwrap();assert!(consumed(&p,&["item-b".into()],&c).unwrap());assert!(capture(&p,&c).unwrap().ids.is_empty());
    }
    #[test]
    fn malformed_aliased_fifo_oversized_inbox_and_seen_files_refuse_promptly() {
        for target in ["inbox/item-a.md",".state/inbox-seen.json"] {for mode in ["malformed","alias","fifo","oversized"] {
            let(_t,p)=fixture();let path=p.dir().join(target);let _=fs::remove_file(&path);
            match mode {
                "malformed"=>fs::write(&path,"malformed {").unwrap(),
                "alias"=>{let source=p.dir().join("source");fs::write(&source,"[]").unwrap();std::os::unix::fs::symlink(source,&path).unwrap();},
                "fifo"=>{let name=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);},
                _=>{let f=fs::File::create(&path).unwrap();f.set_len(16*1024*1024+1).unwrap();},
            }
            let start=Instant::now();assert!(capture(&p,&Control::default()).is_err(),"{target} {mode}");assert!(start.elapsed()<Duration::from_secs(2));
        }}
    }
    #[test]
    fn inventory_refuses_cancelled_and_oversized_identity_sets() {
        let(_t,p)=fixture();let c=Control::default();c.cancellation.cancel();assert!(capture(&p,&c).is_err());
        let ids=(0..1025).map(|n|format!("item-{n:04}")).collect::<Vec<_>>();assert!(Batch::new(ids.clone()).is_err());fs::write(p.state_dir().join("inbox-seen.json"),serde_json::to_vec(&ids).unwrap()).unwrap();assert!(capture(&p,&Control::default()).is_err());
    }
}
