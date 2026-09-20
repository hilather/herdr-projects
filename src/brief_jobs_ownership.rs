//! Conservative root-scoped terminal reference inventory under the shared root
//! barrier. Legacy rebinding and canonical adoption require the exclusive side.
//! Unverified canonical route edits can still occur; they grant no send authority.
use std::{path::{Path,PathBuf},fs};
use anyhow::{Result,Context,ensure};
use crate::{project::{Project,Coordinator},thread::{self,Thread},source_tree::Control,paths};
fn exists(path:&Path)->Result<bool>{match fs::symlink_metadata(path){Ok(_)=>Ok(true),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(false),Err(e)=>Err(e.into())}}
fn location(value:&str)->Result<PathBuf> {
    let path=Path::new(value);ensure!(path.is_absolute(),"terminal reference lacks absolute session");
    match path.canonicalize(){Ok(p)=>Ok(p),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{ensure!(path.components().all(|c|matches!(c,std::path::Component::RootDir|std::path::Component::Normal(_))),"unresolved session alias");Ok(path.into())},Err(e)=>Err(e.into())}
}
struct Inventory<'a>{control:&'a Control,bytes:usize,records:usize,socket:PathBuf,pane:&'a str,remote:bool}
impl Inventory<'_> {
    fn read(&mut self,path:&Path)->Result<Option<String>> {
        self.control.check()?;let text=paths::read_control_text(path,16*1024*1024)?;
        self.bytes=self.bytes.checked_add(text.as_ref().map_or(0,String::len)).context("terminal inventory overflow")?;ensure!(self.bytes<=50*1024*1024,"terminal inventory exceeds byte budget");self.control.check()?;Ok(text)
    }
    fn check(&mut self,machine:&str,socket:&str,pane:&str)->Result<()> {
        self.control.check()?;self.records+=1;ensure!(self.records<=1024,"terminal inventory exceeds 1024 references");
        if !pane.is_empty()&&pane==self.pane {
            // SSH aliases (including loopback) cannot prove distinct terminal
            // servers. A matching pane anywhere is therefore a conflict when
            // either side is remote, even across different profile IDs.
            ensure!(!self.remote&&machine.is_empty(),"brief pane has another reference with unverified remote identity");
            ensure!(location(socket)?!=self.socket,"brief terminal is referenced by another record");
        }Ok(())
    }
}
pub fn check(project:&Project,t:&Thread,socket:&Path,control:&Control)->Result<()> {
    let bounded=Control{deadline:control.deadline.min(std::time::Instant::now()+std::time::Duration::from_secs(10)),cancellation:control.cancellation.clone()};let control=&bounded;
    let mut scan=Inventory{control,bytes:0,records:0,socket:socket.canonicalize()?,pane:&t.pane_id,remote:t.is_remote()};
    let current=project.dir().canonicalize()?;
    for (n,entry) in fs::read_dir(&project.root)?.enumerate() {
        control.check()?;ensure!(n<1024,"terminal root inventory exceeds 1024 entries");let entry=entry?;let kind=entry.file_type()?;if !kind.is_dir()&&!kind.is_symlink(){continue;}
        let name=entry.file_name();let name=name.to_str().context("invalid terminal root entry")?;if name.starts_with('.') {continue;}
        let dir=entry.path();let marker=exists(&dir.join("PROJECT.md"))?;
        let canonical=exists(&dir.join(".state/format.json"))?||exists(&dir.join(".state/migration"))?;
        let recognized=canonical||exists(&dir.join(".state/project.json"))?||exists(&dir.join(".state/coordinator.json"))?;
        ensure!(marker||!recognized,"recognizable neighbor lacks PROJECT.md");if !marker{continue;}
        crate::project::validate_slug(name)?;ensure!(kind.is_dir()&&fs::symlink_metadata(dir.join("PROJECT.md"))?.is_file()&&fs::symlink_metadata(dir.join(".state"))?.is_dir(),"terminal neighbor contains aliases");
        let dir=dir.canonicalize()?;
        if canonical {
            #[cfg(not(feature="state-store"))]
            anyhow::bail!("canonical neighbor requires a state-store build for terminal identity inventory");
            #[cfg(feature="state-store")]
            {
                let mut budget=herdr_projects::store::identity_inventory::Budget::new(50*1024*1024-scan.bytes,1024-scan.records,control.deadline,control.cancellation.clone())?;
                let bindings=herdr_projects::migration::read_identity_inventory(&dir,&mut budget)?;
                scan.bytes+=budget.used();
                for binding in bindings {scan.check(&binding.identity.machine,&binding.identity.socket,&binding.identity.pane_id)?;}
                continue;
            }
        }
        let coordinator=scan.read(&dir.join(".state/coordinator.json"))?.map(|text|serde_json::from_str::<Coordinator>(&text)).transpose()?;
        let socket=coordinator.as_ref().map(|c|c.socket.as_str()).unwrap_or("");
        if let Some(c)=&coordinator {scan.check("",socket,&c.pane_id)?;}
        ensure!(fs::symlink_metadata(dir.join("threads"))?.is_dir(),"terminal thread inventory is aliased");
        for (n,entry) in fs::read_dir(dir.join("threads"))?.enumerate() {
            control.check()?;ensure!(n<256,"terminal thread inventory exceeds 256 entries");let path=entry?.path();if path.extension().is_none_or(|s|s!="toml"){continue;}
            let text=scan.read(&path)?.context("terminal reference disappeared")?;let record:Thread=toml::from_str(&text)?;thread::validate_id(&record.id)?;
            ensure!(path.file_stem().and_then(|s|s.to_str())==Some(&record.id),"terminal reference filename mismatch");
            if dir==current&&record.id==t.id {continue;}
            scan.check(&record.machine,socket,&record.pane_id)?;
        }
    }
    control.check()
}
