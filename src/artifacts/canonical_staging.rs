//! Canonical private staging is bounded and never confused with published evidence.
use super::*;
use crate::source_tree::{Directory,NodeKind};
use std::ffi::{OsStr,OsString};

const TEMP_LIMIT:usize=16;

// Supported publishers hold root/project execution ownership around this check
// and creation. Published snapshots count toward traversal bounds, never deletion.
pub(super) fn room(project:&Project,control:&Control)->Result<()> {
    control.check()?;let state=Directory::open(&project.state_dir())?;
    match state.kind(OsStr::new("canonical-artifacts"))? {
        None=>return Ok(()),Some(NodeKind::Directory)=>{},_=>anyhow::bail!("invalid canonical artifact directory"),
    }
    let root=state.directory(Path::new("canonical-artifacts"))?;let mut budget=control.budget();let mut temporary=0;
    for name in root.names(&budget)? {
        budget.entry(0)?;
        artifact_id(name.to_str().context("invalid canonical artifact key")?,true)?;
        let directory=root.directory(Path::new(&name))?;
        for entry in directory.names(&budget)? {
            budget.entry(0)?;
            // Include malformed/foreign stage names in accounting; never remove them.
            if entry.as_encoded_bytes().starts_with(b".stage-") {temporary+=1;}
            ensure!(temporary<TEMP_LIMIT,"canonical staging inventory is full; inspect abandoned stages before capturing more artifacts");
        }
    }
    control.check()
}

#[cfg(feature="state-store")]
pub(crate) fn receipt_room(parent:&Path,control:&Control)->Result<()> {
    let directory=Directory::open(parent)?;let mut budget=control.budget();let mut temporary=0;
    for name in directory.names(&budget)? {
        budget.entry(0)?;
        if name.as_encoded_bytes().ends_with(b".next") {temporary+=1;}
        ensure!(temporary<TEMP_LIMIT,"temporary finalization receipt inventory is full; inspect retained files before publishing more receipts");
    }
    control.check()
}

pub(super) struct Cleanup {parent:Directory,name:OsString,identity:(u64,u64),control:Control}
impl Cleanup {
    pub(super) fn new(path:&Path,control:&Control)->Result<Self> {
        let parent=Directory::open(path.parent().context("stage parent missing")?)?;
        let name=path.file_name().context("stage name missing")?.to_owned();let metadata=parent.child(&name)?.metadata()?;
        ensure!(metadata.is_dir(),"stage is not a directory");
        Ok(Self{parent,name,identity:(metadata.dev(),metadata.ino()),control:Control{deadline:control.deadline,cancellation:control.cancellation.clone()}})
    }
    pub(super) fn remove(&self)->Result<()> {
        self.control.check()?;
        // Publication renames the stage away. Never clean a replacement inode.
        if self.parent.kind(&self.name)?.is_none(){return Ok(());}
        let metadata=self.parent.child(&self.name)?.metadata()?;
        ensure!((metadata.dev(),metadata.ino())==self.identity&&metadata.is_dir(),"stage identity changed; cleanup refused");
        self.parent.remove_owned_tree(&self.name,&mut self.control.staging_cleanup_budget())
    }
}
