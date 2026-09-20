//! Live-copy protocol v1. Partial projections are never preservation receipts.
use super::*;
use crate::source_tree::{Budget,Control,Directory,Limit,NodeKind};
use std::collections::BTreeSet;
use std::ffi::OsStr;
pub mod projection;
#[allow(dead_code)] // Trusted final-copy worker admission follows recovery validation.
pub mod finalization;

const MAGIC:&[u8;8]=b"HPLV\x01\0\0\0";
pub const STREAM_LIMIT:usize=2*BYTE_LIMIT as usize+MANIFEST_LIMIT+12;
const OMISSION_LIMIT:usize=128;
const OMISSION_BYTES:usize=16*1024;
const OMISSION_PATH:usize=3000;
const STAGE_LIMIT:usize=16;
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(rename_all="kebab-case")]
enum Reason { SymbolicLink, UnsupportedEntry, LibraryLimit }
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
struct Omission { path:String,reason:Reason }
#[derive(Debug,Clone,Serialize,Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
struct Source { schema:u32,report:Option<Entry>,library:Vec<Entry>,omissions:Vec<Omission> }
#[derive(Debug)]
struct OmissionLimit;
impl std::fmt::Display for OmissionLimit {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("live-copy omissions exceed bounds")}}
impl std::error::Error for OmissionLimit {}
fn omit(notes:&mut Vec<Omission>,path:&str,reason:Reason)->Result<()> {
    if notes.len()>=OMISSION_LIMIT || path.len()>OMISSION_PATH || notes.iter().map(|n|n.path.len()).sum::<usize>()+path.len()>OMISSION_BYTES {return Err(OmissionLimit.into());}
    notes.push(Omission{path:path.into(),reason});Ok(())
}
fn visit(parent:&Directory,name:&OsStr,path:&Path,entries:&mut Vec<Entry>,notes:&mut Vec<Omission>,budget:&mut Budget,depth:usize)->Result<()> {
    budget.entry(depth)?;
    let Some(text)=path.to_str() else {return Err(OmissionLimit.into());};
    if text.len()>4096 {return Err(OmissionLimit.into());}
    let kind=parent.kind(name)?.context("live artifact disappeared during scan")?;
    match kind {
        NodeKind::Link=>omit(notes,text,Reason::SymbolicLink),
        NodeKind::Other=>omit(notes,text,Reason::UnsupportedEntry),
        NodeKind::Directory=>{
            let directory=Directory::from_file(parent.child(name)?)?;
            let before=directory.metadata()?;
            entries.push(Entry{path:text.into(),directory:true,bytes:0,sha256:String::new()});
            let scanned=(|| {for name in directory.names(budget)? {visit(&directory,&name,&path.join(&name),entries,notes,budget,depth+1)?;}Ok(())})();
            directory.unchanged(&before)?;scanned
        },
        NodeKind::File=>{
            let (bytes,sha256)=digest(parent.child(name)?,budget,None)?;
            entries.push(Entry{path:text.into(),directory:false,bytes,sha256});Ok(())
        },
    }
}
fn scan(root:&Directory)->Result<Source> {
    let mut source=Source{schema:1,report:None,library:Vec::new(),omissions:Vec::new()};
    if let Some(kind)=root.kind(OsStr::new("report.md"))? {
        if kind==NodeKind::File {
            let (bytes,sha256)=digest(root.child(OsStr::new("report.md"))?,&mut Budget::new(),None)?;
            source.report=Some(Entry{path:"report.md".into(),directory:false,bytes,sha256});
        }else {omit(&mut source.omissions,"report.md",if kind==NodeKind::Link {Reason::SymbolicLink}else{Reason::UnsupportedEntry})?;}
    }
    let Some(kind)=root.kind(OsStr::new("library"))? else {return Ok(source);};
    if kind!=NodeKind::Directory {
        omit(&mut source.omissions,"library",if kind==NodeKind::Link {Reason::SymbolicLink}else{Reason::UnsupportedEntry})?;return Ok(source);
    }
    let mut notes=source.omissions.clone();
    match visit(root,OsStr::new("library"),Path::new("library"),&mut source.library,&mut notes,&mut Budget::new(),0) {
        Ok(())=>source.omissions=notes,
        Err(error) if error.downcast_ref::<Limit>().is_some() || error.downcast_ref::<OmissionLimit>().is_some()=>{
            source.library.clear();omit(&mut source.omissions,"library",Reason::LibraryLimit)?;
        },
        Err(error)=>return Err(error),
    }
    validate(&source)?;Ok(source)
}
fn entries(source:&Source)->impl Iterator<Item=&Entry> {source.report.iter().chain(source.library.iter())}
fn validate(source:&Source)->Result<()> {
    ensure!(source.schema==1,"unsupported live-copy schema");
    ensure!(source.library.len()<=ENTRY_LIMIT && source.omissions.len()<=OMISSION_LIMIT,"live-copy inventory exceeds bounds");
    ensure!(source.omissions.iter().map(|n|n.path.len()).sum::<usize>()<=OMISSION_BYTES,"live-copy omissions exceed bounds");
    let mut found=BTreeSet::new();let mut dirs=BTreeSet::new();let mut bytes=0u64;
    for entry in entries(source) {
        let parts:Vec<_>=entry.path.split('/').collect();
        ensure!(entry.path.len()<=4096 && parts.len()<=65 && parts.iter().all(|p|!p.is_empty()&&*p!="."&&*p!=".."&&!p.contains('\0')),"unsafe live-copy path");
        ensure!(found.insert(entry.path.as_str()),"duplicate live-copy entry");
        if entry.path=="report.md" {
            ensure!(!entry.directory && source.report.as_ref()==Some(entry) && entry.bytes<=BYTE_LIMIT,"invalid live report");
        }else{
            ensure!(entry.path=="library"||entry.path.starts_with("library/"),"live-copy path outside library");
            if let Some((parent,_))=entry.path.rsplit_once('/') {ensure!(dirs.contains(parent),"missing live-copy parent");}
            if entry.path=="library" {ensure!(entry.directory,"live library is not a directory");}
            bytes=bytes.checked_add(entry.bytes).context("live-copy byte overflow")?;ensure!(bytes<=BYTE_LIMIT,"live library exceeds 50 MiB");
        }
        if entry.directory {ensure!(entry.bytes==0&&entry.sha256.is_empty(),"invalid live directory");dirs.insert(entry.path.as_str());}
        else {ensure!(entry.sha256.len()==64&&entry.sha256.bytes().all(|b|b.is_ascii_hexdigit()),"invalid live-copy digest");}
    }
    if let Some(report)=&source.report {ensure!(report.path=="report.md","invalid live report path");}
    let mut omissions=BTreeSet::new();
    for note in &source.omissions {
        let parts:Vec<_>=note.path.split('/').collect();
        ensure!(note.path.len()<=OMISSION_PATH && parts.len()<=65 && parts.iter().all(|p|!p.is_empty()&&*p!="."&&*p!=".."&&!p.contains('\0')),"unsafe omission path");
        ensure!(note.path=="report.md"||note.path=="library"||note.path.starts_with("library/"),"omission outside live artifacts");
        ensure!(!found.contains(note.path.as_str())&&omissions.insert(&note.path),"conflicting live-copy omission");
        if note.path=="library" {ensure!(source.library.is_empty(),"omitted library contains entries");}
        if matches!(note.reason,Reason::LibraryLimit) {ensure!(note.path=="library","library limit outside library");}
    }
    Ok(())
}

pub fn export(path:&Path,writer:&mut impl Write)->Result<()> {
    let root=Directory::open(path)?;let source=scan(&root)?;validate(&source)?;
    let json=serde_json::to_vec(&source)?;ensure!(json.len()<=MANIFEST_LIMIT,"live manifest exceeds bounds");
    writer.write_all(MAGIC)?;writer.write_all(&(json.len() as u32).to_be_bytes())?;writer.write_all(&json)?;
    let mut report_budget=None;let mut library_budget=None;
    for entry in entries(&source).filter(|e|!e.directory) {
        let budget=if entry.path=="report.md" {&mut report_budget}else{&mut library_budget}.get_or_insert_with(Budget::new);
        let mut file=root.file(Path::new(&entry.path))?;budget.size(&file)?;let before=file.metadata()?;
        let mut count=0u64;let mut hash=Sha256::new();let mut buffer=[0;64*1024];
        loop {let n=budget.read(&mut file,&mut buffer)?;if n==0{break;}count+=n as u64;ensure!(count<=entry.bytes,"live source grew while streaming");hash.update(&buffer[..n]);writer.write_all(&buffer[..n])?;}
        crate::source_tree::unchanged(&file,&before)?;
        ensure!(count==entry.bytes&&format!("{:x}",hash.finalize())==entry.sha256,"live source changed while streaming");
    }
    ensure!(scan(&root)?==source,"live artifact source changed while streaming");root.matches_path(path)?;writer.flush()?;Ok(())
}

// The transfer adapter will consume this private staging type. It deliberately
// cannot be converted to Snapshot or passed to destructive-cleanup verification.
#[allow(dead_code)]
pub struct LiveCopy { staging:Staging,source:Source }
#[allow(dead_code)]
impl LiveCopy {
    pub fn report_hash(&self)->Option<&str> {self.source.report.as_ref().map(|e|e.sha256.as_str())}
    pub fn notes(&self)->Vec<String> {
        render_notes(&self.source)
    }
    /// Preserve complete received bytes without fetching the mutable source a
    /// second time. Partial projections deliberately produce no preservation
    /// evidence. The trusted caller must have established sender success and
    /// bound the source/execution before invoking this method.
    pub fn preserve_controlled(&self,project:&Project,record:&Thread,control:&Control,authorize:impl FnOnce()->Result<()>)->Result<Option<Snapshot>> {
        control.check()?;validate(&self.source)?;
        ensure!(self.staging.0.parent()==Some(project.state_dir().join("live-copies").as_path()),"live stage belongs to a different project");
        artifact_id(&record.id,false)?;
        ensure!(!record.thread_dir.is_empty()&&Path::new(&record.thread_dir).is_absolute(),"preservation source must be an absolute recorded path");
        if !self.source.omissions.is_empty() {return Ok(None);}
        let expected=entries(&self.source).cloned().collect::<Vec<_>>();
        // Preservation has a combined 50 MiB limit, unlike live copying's
        // independent report/library limits. Never relabel a larger projection.
        let total=expected.iter().try_fold(0u64,|total,entry|total.checked_add(entry.bytes).context("preservation size overflow"))?;
        ensure!(total<=BYTE_LIMIT&&expected.len()<=ENTRY_LIMIT,"complete live stage exceeds preservation limits");
        let opened=Directory::open(&self.staging.0)?;
        let target=staging(project,record)?;
        let copied=scan_open_controlled(&opened,Some(&target.0),control)?;
        ensure!(copied==expected,"received live bytes changed before preservation");
        opened.matches_path(&self.staging.0)?;
        let manifest=Manifest{schema:1,thread:record.id.clone(),generation:record.lifecycle_generation,
            machine:record.machine.clone(),source:record.thread_dir.clone(),entries:copied};
        let snapshot=publish_mode_controlled(project,record,target,manifest,false,control,||{
            opened.matches_path(&self.staging.0)?;authorize()
        })?;
        Ok(Some(snapshot))
    }
}
fn render_notes(source:&Source)->Vec<String> {
        source.omissions.iter().map(|n| {
            let reason=match n.reason {Reason::SymbolicLink=>"symbolic link",Reason::UnsupportedEntry=>"unsupported entry",Reason::LibraryLimit=>"library limit or unsupported path"};
            format!("{} was omitted ({reason}); existing home content, if any, is retained",n.path)
        }).collect()
}
fn reserve(project:&Project,kind:&str,headroom:usize,control:&Control)->Result<Staging> {
    let parent=project.state_dir().join("live-copies");
    let _lock=project.lock()?;control.check()?;real_dir(&project.state_dir())?;make_dir(&parent)?;
    let inventory=fs::read_dir(&parent)?.take(STAGE_LIMIT).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(inventory.len()<STAGE_LIMIT-headroom,"live staging inventory is full; recover retained stages before receiving another copy");
    for _ in 0..128 {
        let path=parent.join(format!(".{kind}-{}-{}",std::process::id(),SEQUENCE.fetch_add(1,Ordering::Relaxed)));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(())=>return Ok(Staging(path)),
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>continue,
            Err(error)=>return Err(error.into()),
        }
    }
    anyhow::bail!("no free live-copy staging name")
}
/// Temporary transport ownership is separate from durable projection intent.
/// Abandoned spools count against the same inventory as retained stages.
#[allow(dead_code)]
pub struct Spool(Staging);
#[allow(dead_code)]
impl Spool {
    pub fn reserve(project:&Project,control:&Control)->Result<Self> {
        control.check()?;Ok(Self(reserve(project,"download",1,control)?))
    }
    pub fn path(&self)->PathBuf {self.0.0.join("stream")}
}
#[allow(dead_code)]
/// The trusted adapter must establish successful supervised sender completion
/// before calling this. Stream integrity alone is not execution authority.
pub fn receive(project:&Project,archive:&Path)->Result<LiveCopy> {
    receive_controlled(project,archive,&Control::default())
}
#[allow(dead_code)]
pub fn receive_controlled(project:&Project,archive:&Path,control:&Control)->Result<LiveCopy> {
    control.check()?;let header_budget=control.budget();
    let mut stream=regular(archive)?;ensure!(stream.metadata()?.len()<=STREAM_LIMIT as u64,"live-copy stream exceeds bounds");
    let before=stream.metadata()?;let mut report_budget=None;let mut library_budget=None;
    let mut magic=[0;8];stream.read_exact(&mut magic)?;ensure!(&magic==MAGIC,"invalid live-copy stream magic");
    let mut size=[0;4];stream.read_exact(&mut size)?;let size=u32::from_be_bytes(size) as usize;
    ensure!(size<=MANIFEST_LIMIT,"live-copy manifest exceeds bounds");let mut json=vec![0;size];stream.read_exact(&mut json)?;
    let source:Source=serde_json::from_slice(&json)?;validate(&source)?;
    header_budget.check()?;
    let staging=reserve(project,"stage",0,control)?;
    for entry in entries(&source) {
        let budget=if entry.path=="report.md" {&mut report_budget}else{&mut library_budget}.get_or_insert_with(||control.budget());budget.check()?;
        let path=staging.0.join(&entry.path);
        if entry.directory {fs::create_dir(&path)?;continue;}
        let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        let mut remaining=entry.bytes;let mut hash=Sha256::new();let mut buffer=[0;64*1024];
        while remaining>0 {let limit=(remaining as usize).min(buffer.len());let n=budget.read(&mut stream,&mut buffer[..limit])?;ensure!(n>0,"truncated live-copy payload");hash.update(&buffer[..n]);file.write_all(&buffer[..n])?;remaining-=n as u64;}
        ensure!(format!("{:x}",hash.finalize())==entry.sha256,"live-copy payload digest mismatch");file.sync_all()?;budget.check()?;
    }
    ensure!(stream.read(&mut [0])?==0,"trailing live-copy data");
    crate::source_tree::unchanged(&stream,&before)?;
    for entry in source.library.iter().rev().filter(|e|e.directory) {
        File::open(staging.0.join(&entry.path))?.sync_all()?;
        if let Some(budget)=&library_budget {budget.check()?;}
    }
    File::open(&staging.0)?.sync_all()?;
    if let Some(budget)=library_budget.or(report_budget) {budget.check()?;}
    control.check()?;Ok(LiveCopy{staging,source})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roundtrip(project:&Project,path:&Path,archive:&Path)->LiveCopy {
        let mut bytes=Vec::new();export(path,&mut bytes).unwrap();fs::write(archive,bytes).unwrap();receive(project,archive).unwrap()
    }
    #[test]
    fn complete_live_stage_preserves_exact_received_bytes_after_source_loss() {
        let(root,project,mut record)=super::super::tests::fixture();
        let received=roundtrip(&project,Path::new(&record.thread_dir),&root.path().join("wire"));
        fs::remove_dir_all(&record.thread_dir).unwrap();record.machine="remote-fixture".into();record.lifecycle_generation=7;
        let saved=received.preserve_controlled(&project,&record,&Control::default(),||Ok(())).unwrap().unwrap();
        assert_eq!(saved.manifest.machine,record.machine);assert_eq!(saved.manifest.source,record.thread_dir);assert_eq!(saved.manifest.generation,7);
        assert_eq!(super::super::load(&project,&record,&saved.id).unwrap(),saved.manifest);
        let path=project.state_dir().join("artifacts/t-0001").join(&saved.id);
        assert_eq!(fs::read(path.join("report.md")).unwrap(),b"report\0\xff");assert!(path.join("library/empty").is_dir());
        assert_eq!(received.preserve_controlled(&project,&record,&Control::default(),||Ok(())).unwrap().unwrap().id,saved.id);
        assert!(received.staging.0.is_dir(),"preservation must leave projection ownership intact");
        assert!(!project.dir().join("threads/t-0001.toml").exists(),"retention never certifies finalization");
    }
    #[test]
    fn omitted_entries_never_become_preservation_receipts_but_empty_reports_can_be_retained() {
        let(root,project,record)=super::super::tests::fixture();
        std::os::unix::fs::symlink("artifact",Path::new(&record.thread_dir).join("library/link")).unwrap();
        let partial=roundtrip(&project,Path::new(&record.thread_dir),&root.path().join("partial"));
        assert!(!partial.notes().is_empty());assert!(partial.preserve_controlled(&project,&record,&Control::default(),||Ok(())).unwrap().is_none());
        assert!(!project.state_dir().join("artifacts").exists());
        fs::remove_file(Path::new(&record.thread_dir).join("library/link")).unwrap();fs::remove_file(Path::new(&record.thread_dir).join("report.md")).unwrap();
        let complete=roundtrip(&project,Path::new(&record.thread_dir),&root.path().join("empty-report"));
        let saved=complete.preserve_controlled(&project,&record,&Control::default(),||Ok(())).unwrap().unwrap();assert!(saved.manifest.report_hash().is_none());
        assert!(super::super::load(&project,&record,&saved.id).is_ok());
    }
    #[test]
    fn preservation_refuses_changed_staged_bytes_wrong_project_and_lost_authority() {
        for fault in ["changed","project","cancelled","expired","authority","late-cancel"] {
            let(root,project,record)=super::super::tests::fixture();let received=roundtrip(&project,Path::new(&record.thread_dir),&root.path().join("wire"));
            let other=crate::project::create(root.path(),"other","",vec![]).unwrap();let target=if fault=="project"{&other}else{&project};
            let mut control=Control::default();
            match fault {"changed"=>fs::write(received.staging.0.join("library/artifact"),b"tampered").unwrap(),"cancelled"=>control.cancellation.cancel(),"expired"=>control.deadline=std::time::Instant::now(),_=>{}}
            assert!(received.preserve_controlled(target,&record,&control,||{if fault=="late-cancel"{control.cancellation.cancel();}ensure!(fault!="authority","authority revoked");Ok(())}).is_err(),"{fault}");
            let parent=target.state_dir().join("artifacts/t-0001");assert_eq!(fs::read_dir(&parent).into_iter().flatten().count(),0,"{fault}");assert!(received.staging.0.is_dir());
        }
    }
    #[test]
    fn live_section_limits_do_not_expand_the_combined_preservation_limit() {
        let(root,project,record)=super::super::tests::fixture();
        File::options().write(true).open(Path::new(&record.thread_dir).join("report.md")).unwrap().set_len(25*1024*1024).unwrap();
        File::options().write(true).open(Path::new(&record.thread_dir).join("library/artifact")).unwrap().set_len(26*1024*1024).unwrap();
        let received=roundtrip(&project,Path::new(&record.thread_dir),&root.path().join("wire"));assert!(received.notes().is_empty());
        let error=received.preserve_controlled(&project,&record,&Control::default(),||Ok(())).err().unwrap();assert!(error.to_string().contains("preservation limits"));
        assert!(!project.state_dir().join("artifacts").exists());assert!(received.staging.0.is_dir());
    }

    #[test]
    fn download_reservation_counts_orphans_and_leaves_receive_capacity() {
        let (root,project,record)=super::super::tests::fixture();let control=Control::default();
        let parent=project.state_dir().join("live-copies");fs::create_dir(&parent).unwrap();
        for n in 0..STAGE_LIMIT-2 {fs::create_dir(parent.join(format!("orphan-{n}"))).unwrap();}
        let spool=Spool::reserve(&project,&control).unwrap();
        assert!(Spool::reserve(&project,&control).is_err());
        let mut bytes=Vec::new();export(Path::new(&record.thread_dir),&mut bytes).unwrap();fs::write(spool.path(),bytes).unwrap();
        let staged=receive_controlled(&project,&spool.path(),&control).unwrap();
        assert_eq!(fs::read_dir(&parent).unwrap().count(),STAGE_LIMIT);
        let path=spool.path();drop(spool);assert!(!path.exists());
        assert!(staged.staging.0.exists());drop(staged);
        assert_eq!(fs::read_dir(&parent).unwrap().count(),STAGE_LIMIT-2);
        control.cancellation.cancel();assert!(Spool::reserve(&project,&control).is_err());
        assert!(root.path().exists());
    }
    #[test]
    fn live_partial_stream_preserves_binary_empty_directories_and_skips_links() {
        let (root,project,record)=super::super::tests::fixture();let source=Path::new(&record.thread_dir);
        fs::write(source.join("library/a 'λ$\nfile"),[0,255,254]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd",source.join("library/link")).unwrap();
        let staged=roundtrip(&project,source,&root.path().join("archive"));
        assert_eq!(fs::read(staged.staging.0.join("report.md")).unwrap(),b"report\0\xff");
        assert!(staged.staging.0.join("library/empty").is_dir());
        assert_eq!(fs::read(staged.staging.0.join("library/a 'λ$\nfile")).unwrap(),[0,255,254]);
        assert!(!staged.staging.0.join("library/link").exists());assert_eq!(staged.notes().len(),1);
        assert!(staged.notes()[0].contains("existing home content"));
        assert_eq!(staged.report_hash(),Some(thread::sha256_hex(b"report\0\xff").as_str()));
        assert!(capture_local(&project,&record).is_err(),"partial live copy cannot become preservation");
        let path=staged.staging.0.clone();drop(staged);assert!(!path.exists());
    }
    #[test]
    fn library_limits_omit_entire_library_without_losing_report() {
        for kind in ["bytes","entries","depth","omissions"] {
            let (root,project,record)=super::super::tests::fixture();let source=Path::new(&record.thread_dir);
            match kind {
                "bytes"=>File::create(source.join("library/large")).unwrap().set_len(BYTE_LIMIT+1).unwrap(),
                "entries"=>{for n in 0..ENTRY_LIMIT {File::create(source.join(format!("library/{n}"))).unwrap();}},
                "depth"=>{let mut p=source.join("library");for _ in 0..65 {p=p.join("d");fs::create_dir(&p).unwrap();}},
                "omissions"=>{for n in 0..=OMISSION_LIMIT {std::os::unix::fs::symlink("missing",source.join(format!("library/link{n}"))).unwrap();}},
                _=>unreachable!(),
            }
            let staged=roundtrip(&project,source,&root.path().join("archive"));
            assert!(staged.report_hash().is_some(),"{kind}");assert!(!staged.staging.0.join("library").exists(),"{kind}");
            assert_eq!(staged.source.omissions,vec![Omission{path:"library".into(),reason:Reason::LibraryLimit}],"{kind}");
        }
    }
    #[test]
    fn report_and_library_each_have_their_own_byte_budget() {
        let (root,project,record)=super::super::tests::fixture();let source=Path::new(&record.thread_dir);
        File::create(source.join("report.md")).unwrap().set_len(26*1024*1024).unwrap();
        File::create(source.join("library/artifact")).unwrap().set_len(26*1024*1024).unwrap();
        let staged=roundtrip(&project,source,&root.path().join("archive"));
        assert!(staged.notes().is_empty());assert_eq!(fs::metadata(staged.staging.0.join("library/artifact")).unwrap().len(),26*1024*1024);
        File::create(source.join("report.md")).unwrap().set_len(BYTE_LIMIT+1).unwrap();
        assert!(export(source,&mut Vec::new()).is_err());
    }
    #[test]
    fn corruption_truncation_trailing_bytes_and_wrong_protocol_never_leave_staging() {
        let (root,project,record)=super::super::tests::fixture();let mut bytes=Vec::new();export(Path::new(&record.thread_dir),&mut bytes).unwrap();
        let mut corrupt=bytes.clone();*corrupt.last_mut().unwrap()^=1;
        let mut trailing=bytes.clone();trailing.push(0);
        let mut old=Vec::new();super::super::export(Path::new(&record.thread_dir),&mut old).unwrap();
        for payload in [corrupt,trailing,bytes[..bytes.len()-1].to_vec(),old] {
            let archive=root.path().join("archive");fs::write(&archive,payload).unwrap();assert!(receive(&project,&archive).is_err());
            let parent=project.state_dir().join("live-copies");if parent.exists(){assert_eq!(fs::read_dir(parent).unwrap().count(),0);}
        }
    }
    #[test]
    fn changing_source_during_stream_is_not_certified() {
        struct Racing { path:PathBuf,bytes:Vec<u8> }
        impl Write for Racing {
            fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
                if self.bytes.is_empty(){fs::write(&self.path,b"changed")?;}
                self.bytes.extend_from_slice(bytes);Ok(bytes.len())
            }
            fn flush(&mut self)->std::io::Result<()> {Ok(())}
        }
        let (_root,_project,record)=super::super::tests::fixture();
        let mut writer=Racing{path:Path::new(&record.thread_dir).join("library/artifact"),bytes:Vec::new()};
        assert!(export(Path::new(&record.thread_dir),&mut writer).is_err());
    }
    #[test]
    fn unsafe_duplicate_oversize_and_conflicting_manifests_refuse_before_staging() {
        let (root,project,record)=super::super::tests::fixture();let valid=scan(&Directory::open(Path::new(&record.thread_dir)).unwrap()).unwrap();
        for kind in ["traversal","duplicate","oversize","conflict","wrong-report","schema"] {
            let mut source=valid.clone();
            match kind {
                "traversal"=>source.library[0].path="../escape".into(),
                "duplicate"=>source.library.push(source.library[0].clone()),
                "oversize"=>source.report.as_mut().unwrap().bytes=BYTE_LIMIT+1,
                "conflict"=>source.omissions.push(Omission{path:"report.md".into(),reason:Reason::UnsupportedEntry}),
                "wrong-report"=>source.report.as_mut().unwrap().path="library/wrong".into(),
                "schema"=>source.schema=2,
                _=>unreachable!(),
            }
            let json=serde_json::to_vec(&source).unwrap();let mut bytes=MAGIC.to_vec();bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());bytes.extend_from_slice(&json);
            let archive=root.path().join("archive");fs::write(&archive,bytes).unwrap();assert!(receive(&project,&archive).is_err(),"{kind}");
            assert!(!project.state_dir().join("live-copies").exists(),"{kind}");assert!(!root.path().join("escape").exists());
        }
    }
}
