//! Bounded, content-addressed local preservation snapshots. Live copies are
//! projections; these retained snapshots are the evidence used by cleanup.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt,DirBuilderExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{project::Project, thread::{self, Thread},source_tree::Control};

const BYTE_LIMIT: u64 = 50 * 1024 * 1024;
const ENTRY_LIMIT: usize = 10_000;
const MANIFEST_LIMIT: usize = 4 * 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Entry {
    path: String,
    directory: bool,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    schema: u32,
    thread: String,
    generation: u64,
    source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    machine: String,
    entries: Vec<Entry>,
}

impl Manifest {
    #[cfg(feature="state-store")]
    pub fn matches_canonical_execution(&self,record:&Thread)->bool {
        self.schema==1&&self.thread==record.id&&self.generation==record.lifecycle_generation&&self.machine.is_empty()&&self.source==record.thread_dir
    }
    pub fn report_hash(&self) -> Option<&str> {
        self.entries.iter().find(|e| e.path == "report.md" && !e.directory).map(|e| e.sha256.as_str())
    }
}

pub struct Snapshot {
    pub id: String,
    pub manifest: Manifest,
}

fn real_dir(path: &Path) -> Result<()> {
    ensure!(fs::symlink_metadata(path)?.is_dir(), "{} is not a real directory", path.display());
    Ok(())
}

fn make_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
        Err(e) => return Err(e.into()),
    }
    real_dir(path)
}

fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.nlink() == 1, "{} is not a regular file with one link (symlinks and hard links are not preserved)", path.display());
    Ok(file)
}

fn digest(mut source:File, budget:&mut crate::source_tree::Budget, copy:Option<&Path>)->Result<(u64,String)> {
    budget.size(&source)?;let before=source.metadata()?;
    let mut target=copy.map(|p|OpenOptions::new().write(true).create_new(true).mode(0o600).open(p)).transpose()?;
    let mut bytes=0u64;let mut hash=Sha256::new();let mut buffer=[0u8;64*1024];
    loop {
        let n=match budget.read(&mut source,&mut buffer) {Ok(n)=>n,Err(error)=>{crate::source_tree::unchanged(&source,&before)?;return Err(error);}};if n==0{break;}
        bytes+=n as u64;hash.update(&buffer[..n]);
        if let Some(target)=&mut target{target.write_all(&buffer[..n])?;}
    }
    crate::source_tree::unchanged(&source,&before)?;
    if let Some(target)=target{target.sync_all()?;}budget.check()?;
    Ok((bytes,hash.finalize().iter().map(|b|format!("{b:02x}")).collect()))
}

fn visit(source:File,relative:&Path,destination:Option<&Path>,entries:&mut Vec<Entry>,budget:&mut crate::source_tree::Budget,depth:usize)->Result<()> {
    budget.entry(depth)?;
    let path=relative.to_str().context("artifact filenames must be UTF-8")?.to_string();
    if source.metadata()?.is_dir() {
        let directory=crate::source_tree::Directory::from_file(source)?;
        if let Some(destination)=destination{fs::create_dir(destination.join(relative))?;}
        entries.push(Entry{path,directory:true,bytes:0,sha256:String::new()});
        for name in directory.names(budget)? {visit(directory.child(&name)?,&relative.join(name),destination,entries,budget,depth+1)?;}
        if let Some(destination)=destination{File::open(destination.join(relative))?.sync_all()?;}
        budget.check()?;
    }else{
        let target=destination.map(|d|d.join(relative));
        let(bytes,sha256)=digest(source,budget,target.as_deref())?;
        entries.push(Entry{path,directory:false,bytes,sha256});
    }
    Ok(())
}
fn scan_open(root:&crate::source_tree::Directory,destination:Option<&Path>)->Result<Vec<Entry>> {
    scan_open_controlled(root,destination,&Control::default())
}
fn scan_open_controlled(root:&crate::source_tree::Directory,destination:Option<&Path>,control:&Control)->Result<Vec<Entry>> {
    control.check()?;let mut budget=control.budget();let mut entries=Vec::new();
    for name in ["report.md","library"] {
        if let Some(source)=root.optional(std::ffi::OsStr::new(name))?{
            visit(source,Path::new(name),destination,&mut entries,&mut budget,0)?;
        }
    }
    control.check()?;Ok(entries)
}
fn scan(root:&Path,destination:Option<&Path>)->Result<Vec<Entry>> {
    scan_controlled(root,destination,&Control::default())
}
fn scan_controlled(root:&Path,destination:Option<&Path>,control:&Control)->Result<Vec<Entry>> {
    control.check()?;let opened=crate::source_tree::Directory::open(root)?;
    let entries=scan_open_controlled(&opened,destination,control)?;opened.matches_path(root)?;control.check()?;Ok(entries)
}

struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

pub fn capture_local(project: &Project, record: &Thread) -> Result<Snapshot> {
    capture(project, record, || Ok(()))
}

fn capture(project: &Project, record: &Thread, before_verify: impl FnOnce() -> Result<()>) -> Result<Snapshot> {
    capture_mode(project,record,before_verify,false)
}

fn capture_mode(project:&Project,record:&Thread,before_verify:impl FnOnce()->Result<()>,canonical:bool)->Result<Snapshot> {
    ensure!(!record.is_remote() && !record.thread_dir.is_empty(), "local artifact source is required");
    artifact_id(&record.id,canonical)?;
    let source = Path::new(&record.thread_dir);
    real_dir(source)?;
    let opened=crate::source_tree::Directory::open(source)?;
    let identity = fs::canonicalize(source)?;
    let staging = staging_mode(project, record,canonical)?;
    let manifest = Manifest {
        schema: 1, thread: record.id.clone(), generation: record.lifecycle_generation,
        machine: String::new(),
        source: identity.to_str().context("artifact source path must be UTF-8")?.into(),
        entries: scan_open(&opened, Some(&staging.0))?,
    };
    before_verify()?;
    opened.matches_path(source)?;
    ensure!(manifest.entries == scan(&staging.0, None)?, "staged artifact verification failed");
    verify_source(record, &manifest)?;
    opened.matches_path(source)?;
    publish_mode(project, record, staging, manifest,canonical,||opened.matches_path(source))
}

fn staging(project: &Project, record: &Thread) -> Result<Staging> {staging_mode(project,record,false)}
fn staging_mode(project:&Project,record:&Thread,canonical:bool)->Result<Staging> {
    artifact_id(&record.id,canonical)?;
    let parent = project.state_dir().join(artifact_directory(canonical)).join(&record.id);
    {
        let _lock = artifact_lock(project,canonical)?;
        real_dir(&project.state_dir())?;
        make_dir(parent.parent().unwrap())?;
        make_dir(&parent)?;
        File::open(parent.parent().unwrap())?.sync_all()?;
        File::open(project.state_dir())?.sync_all()?;
    }
    let stage_path = parent.join(format!(".stage-{}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)));
    fs::DirBuilder::new().mode(0o700).create(&stage_path)?;
    let staging = Staging(stage_path);
    Ok(staging)
}

fn publish_mode(project: &Project, record: &Thread, staging: Staging, manifest: Manifest,canonical:bool,before_publish:impl FnOnce()->Result<()>) -> Result<Snapshot> {
    publish_mode_controlled(project,record,staging,manifest,canonical,&Control::default(),before_publish)
}
fn publish_mode_controlled(project:&Project,record:&Thread,staging:Staging,manifest:Manifest,canonical:bool,control:&Control,before_publish:impl FnOnce()->Result<()>)->Result<Snapshot> {
    control.check()?;
    ensure!(scan_controlled(&staging.0, None,control)? == manifest.entries, "snapshot verification failed");
    let parent = staging.0.parent().context("missing snapshot parent")?;
    let bytes = serde_json::to_vec(&manifest)?;
    ensure!(bytes.len() <= MANIFEST_LIMIT, "artifact manifest is too large");
    let id = thread::sha256_hex(&bytes);
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(staging.0.join("manifest.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    File::open(&staging.0)?.sync_all()?;
    let target = parent.join(&id);
    let _lock = artifact_lock(project,canonical)?;
    control.check()?;real_dir(&parent)?;
    let exists=target.try_exists()?;
    if exists {
        // Never overwrite a previous snapshot, including damaged evidence.
        let existing = load_mode_controlled(project, record, &id,canonical,control)?;
        ensure!(existing == manifest, "existing artifact snapshot differs");
    }
    before_publish()?;control.check()?;
    if !exists {
        fs::rename(&staging.0, &target)?;
        File::open(&parent)?.sync_all()?;
    }
    control.check()?;Ok(Snapshot { id, manifest })
}

pub fn verify_source(record: &Thread, manifest: &Manifest) -> Result<()> {
    ensure!(manifest.machine.is_empty() && !record.is_remote(), "remote source verification requires its helper");
    ensure!(manifest.schema == 1 && manifest.thread == record.id && manifest.generation == record.lifecycle_generation,
        "artifact snapshot belongs to a different thread execution");
    ensure!(fs::canonicalize(&record.thread_dir)? == Path::new(&manifest.source), "artifact source identity changed");
    ensure!(scan(Path::new(&record.thread_dir), None)? == manifest.entries, "artifact source changed during preservation");
    Ok(())
}

pub fn load(project:&Project,record:&Thread,id:&str)->Result<Manifest> {load_mode(project,record,id,false)}
fn load_mode(project: &Project, record: &Thread, id: &str,canonical:bool) -> Result<Manifest> {
    load_mode_controlled(project,record,id,canonical,&Control::default())
}
fn load_mode_controlled(project:&Project,record:&Thread,id:&str,canonical:bool,control:&Control)->Result<Manifest> {
    control.check()?;
    ensure!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()), "invalid artifact snapshot id");
    artifact_id(&record.id,canonical)?;
    let parent = project.state_dir().join(artifact_directory(canonical));
    for dir in [&parent, &parent.join(&record.id), &parent.join(&record.id).join(id)] { real_dir(dir)?; }
    let dir = parent.join(&record.id).join(id);
    let mut bytes = Vec::new();
    regular(&dir.join("manifest.json"))?.take(MANIFEST_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MANIFEST_LIMIT && thread::sha256_hex(&bytes) == id, "artifact manifest is corrupt");
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(manifest.schema == 1 && manifest.thread == record.id, "unsupported artifact manifest identity");
    control.check()?;ensure!(scan_controlled(&dir, None,control)? == manifest.entries, "retained artifact bytes do not match their manifest");
    Ok(manifest)
}

#[cfg(test)]
mod tests;

mod wire;
pub mod live;
pub use wire::{capture_remote, export, probe};
#[allow(unused_imports)] // Native automatic final-copy ingress follows controlled preservation.
pub use wire::receive_controlled;

fn artifact_directory(canonical:bool)->&'static str {if canonical {"canonical-artifacts"}else{"artifacts"}}
fn artifact_id(id:&str,canonical:bool)->Result<()> {
    if canonical {ensure!(id.strip_prefix("runtime-").is_some_and(|s|s.len()==64&&s.bytes().all(|b|b.is_ascii_hexdigit())),"invalid canonical artifact identity");Ok(())}
    else {thread::validate_id(id)}
}
enum ArtifactLock { Legacy{_lock:crate::project::ProjectLock}, #[cfg(feature="state-store")] Canonical{_file:File} }
fn artifact_lock(project:&Project,canonical:bool)->Result<ArtifactLock> {
    if !canonical {return Ok(ArtifactLock::Legacy{_lock:project.lock()?});}
    #[cfg(feature="state-store")]
    {
        // Separate entry point: legacy mutation guards are never disabled.
        // The caller retains the root execution lease across the complete copy.
        herdr_projects::migration::open_active(&project.dir())?;
        real_dir(&project.state_dir())?;
        let file=OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(project.state_dir().join("lock"))?;
        file.try_lock().context("project metadata is busy")?;
        Ok(ArtifactLock::Canonical{_file:file})
    }
    #[cfg(not(feature="state-store"))]
    anyhow::bail!("canonical artifact capture requires state-store")
}
#[cfg(feature="state-store")]
pub fn capture_canonical(project:&Project,record:&Thread)->Result<Snapshot> {capture_mode(project,record,||Ok(()),true)}
#[cfg(feature="state-store")]
pub fn load_canonical(project:&Project,record:&Thread,id:&str)->Result<Manifest> {load_mode(project,record,id,true)}
