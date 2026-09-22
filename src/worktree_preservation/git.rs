//! Portable object pack plus exact index bytes. No refs or objects are written
//! into the source repository. Restore is separately authorized work.
use super::*;
use std::collections::BTreeSet;
use crate::worktree_preparation::Git;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEvidence {pub sha256:String,pub bytes:u64}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {pub path:String,pub mode:String,pub object:String,pub stage:u8}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedIndex {pub name:String,pub file:FileEvidence}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub object_format:String,pub head:String,pub base:String,pub entries:Vec<IndexEntry>,
    pub index:Option<FileEvidence>,pub shared_index:Option<SharedIndex>,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {pub version:u32,pub state:State,pub pack:FileEvidence}
fn oid(value:&str,len:usize)->bool {value.len()==len && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))}
fn run(git:&Git,receipt:&WorktreeReceipt,args:&[&str],input:Option<String>,limit:usize)->Result<Vec<u8>> {
    let directory=format!("--git-dir={}",receipt.git_directory);
    let worktree=format!("--work-tree={}",receipt.plan.path);
    let mut argv=vec![directory.as_str(),worktree.as_str()];argv.extend_from_slice(args);
    git.capture(Path::new(&receipt.plan.path),&argv,input,limit)
}
fn text(git:&Git,receipt:&WorktreeReceipt,args:&[&str])->Result<String> {
    Ok(std::str::from_utf8(&run(git,receipt,args,None,8192)?)?.trim_end_matches('\n').into())
}
fn file(directory:&Directory,name:&str,control:&Control,remaining:&mut usize,blobs:&mut BTreeMap<String,Vec<u8>>)->Result<Option<FileEvidence>> {
    let Some(mut file)=directory.optional(OsStr::new(name))? else {return Ok(None);};
    let before=file.metadata()?;
    ensure!(before.len()<=4*1024*1024 && before.len()<=*remaining as u64,"Git index exceeds preservation bounds");
    let mut budget=control.budget();budget.size(&file)?;
    let mut bytes=Vec::new();let mut buffer=[0u8;65536];
    loop {let n=budget.read(&mut file,&mut buffer)?;if n==0 {break;}bytes.extend_from_slice(&buffer[..n]);}
    source_tree::unchanged(&file,&before)?;
    *remaining=remaining.checked_sub(bytes.len()).context("Git preservation byte budget exhausted")?;
    let evidence=FileEvidence{sha256:hash(&bytes),bytes:bytes.len() as u64};
    blobs.entry(evidence.sha256.clone()).or_insert(bytes);Ok(Some(evidence))
}
fn observe(git:&Git,receipt:&WorktreeReceipt,control:&Control,remaining:&mut usize,blobs:&mut BTreeMap<String,Vec<u8>>)->Result<State> {
    control.check()?;
    let object_format=text(git,receipt,&["rev-parse","--show-object-format"])?;
    let length=match object_format.as_str(){"sha1"=>40,"sha256"=>64,_=>anyhow::bail!("unsupported Git object format")};
    let head=text(git,receipt,&["rev-parse","--verify","HEAD^{commit}"])?;
    ensure!(oid(&head,length) && oid(&receipt.plan.source.commit,length),"invalid preserved Git commit");
    let raw=run(git,receipt,&["ls-files","--stage","--sparse","-z"],None,4*1024*1024)?;
    ensure!(raw.is_empty() || raw.last()==Some(&0),"incomplete Git index inventory");
    let mut entries=Vec::new();let mut seen=BTreeSet::new();
    for record in raw.split(|b|*b==0).filter(|s|!s.is_empty()) {
        ensure!(entries.len()<10_000,"Git index inventory exceeds bounds");
        let (header,path)=std::str::from_utf8(record)?.split_once('\t').context("invalid Git index record")?;
        let fields=header.split(' ').collect::<Vec<_>>();
        ensure!(fields.len()==3 && matches!(fields[0],"100644"|"100755"|"120000"|"040000") && oid(fields[1],length),"unsupported or invalid Git index object");
        let stage=fields[2].parse::<u8>()?;
        ensure!(stage<=3 && !path.is_empty() && path.len()<=4096 && Path::new(path).components().all(|p|matches!(p,std::path::Component::Normal(_))) && seen.insert((path.to_owned(),stage)),"invalid Git index path or stage");
        entries.push(IndexEntry{path:path.into(),mode:fields[0].into(),object:fields[1].into(),stage});
    }
    let directory=Directory::open(Path::new(&receipt.git_directory))?;
    let index=file(&directory,"index",control,remaining,blobs)?;
    let shared=text(git,receipt,&["rev-parse","--shared-index-path"])?;
    let shared_index=if shared.is_empty(){None}else{
        let path=Path::new(&shared);
        let path=if path.is_absolute(){path.to_owned()}else{Path::new(&receipt.plan.path).join(path)};
        ensure!(path.canonicalize()?==path,"noncanonical shared Git index");
        let parent=path.parent().context("shared index parent missing")?;
        ensure!(parent==Path::new(&receipt.git_directory) || parent==Path::new(&receipt.common_directory),"shared index is outside retained repository metadata");
        let name=path.file_name().and_then(|s|s.to_str()).context("shared index name missing")?;
        ensure!(name.strip_prefix("sharedindex.").is_some_and(|s|oid(s,length)),"invalid shared index name");
        let file=file(&Directory::open(parent)?,name,control,remaining,blobs)?.context("shared index disappeared")?;
        Some(SharedIndex{name:name.into(),file})
    };
    Ok(State{object_format,head,base:receipt.plan.source.commit.clone(),entries,index,shared_index})
}
pub(super) fn capture(git:&Git,receipt:&WorktreeReceipt,control:&Control,remaining:&mut usize,blobs:&mut BTreeMap<String,Vec<u8>>)->Result<Archive> {
    let state=observe(git,receipt,control,remaining,blobs)?;
    let mut roots=BTreeSet::from([state.head.clone(),state.base.clone()]);
    roots.extend(state.entries.iter().map(|entry|entry.object.clone()));
    ensure!(*remaining>0,"Git pack preservation budget exhausted");
    let pack=run(git,receipt,&["-c","pack.threads=1","pack-objects","--revs","--stdout","--window=0","--compression=0","--no-reuse-object","--no-reuse-delta"],Some(format!("{}\n",roots.into_iter().collect::<Vec<_>>().join("\n"))),*remaining)?;
    ensure!(pack.starts_with(b"PACK"),"Git archive is not a pack");
    *remaining=remaining.checked_sub(pack.len()).context("Git pack preservation budget exhausted")?;
    let evidence=FileEvidence{sha256:hash(&pack),bytes:pack.len() as u64};
    blobs.entry(evidence.sha256.clone()).or_insert(pack);
    let archive=Archive{version:1,state,pack:evidence};verify(git,receipt,control,&archive)?;Ok(archive)
}
pub(super) fn verify(git:&Git,receipt:&WorktreeReceipt,control:&Control,archive:&Archive)->Result<()> {
    let state=observe(git,receipt,control,&mut (source_tree::BYTE_LIMIT as usize),&mut BTreeMap::new())?;
    ensure!(state==archive.state,"Git refs or index changed during preservation");Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs,process::{Command,Stdio},time::Duration};
    fn command(path:&Path,args:&[&str],input:Option<&[u8]>)->Vec<u8> {
        let mut child=Command::new("/usr/bin/git").current_dir(path).env_clear()
            .env("PATH","/usr/bin:/bin").env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null")
            .args(["-c","core.hooksPath=/dev/null","-c","user.name=Fixture","-c","user.email=f@example.invalid","-c","commit.gpgsign=false"])
            .args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        if let Some(bytes)=input {child.stdin.take().unwrap().write_all(bytes).unwrap();}else{drop(child.stdin.take());}
        let output=child.wait_with_output().unwrap();assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));output.stdout
    }
    fn string(path:&Path,args:&[&str])->String {String::from_utf8(command(path,args,None)).unwrap().trim().into()}
    #[test]
    fn packed_history_and_index_restore_without_the_source_repository() {
        for mode in ["ordinary","split","conflict","sha256","sha256_split","intent","flags"] {
            let split=matches!(mode,"split"|"sha256_split");
            let root=tempfile::tempdir().unwrap();let source=root.path().join("source");fs::create_dir(&source).unwrap();
            let format=if mode.starts_with("sha256"){"--object-format=sha256"}else{"--object-format=sha1"};
            command(&source,&["init","--quiet",format],None);
            fs::write(source.join("tracked"),b"approved base").unwrap();command(&source,&["add","."],None);command(&source,&["commit","--quiet","-m","base"],None);
            let base=string(&source,&["rev-parse","HEAD"]);let tree=string(&source,&["rev-parse","HEAD^{tree}"]);
            fs::write(source.join("tracked"),b"committed result").unwrap();command(&source,&["commit","--quiet","-am","result"],None);
            let head=string(&source,&["rev-parse","HEAD"]);
            let staged=[0,255,10,42];fs::write(source.join("tracked"),staged).unwrap();command(&source,&["add","tracked"],None);
            if mode=="conflict" {
                let object=String::from_utf8(command(&source,&["hash-object","-w","--stdin"],Some(b"conflict-only object"))).unwrap();
                let records=(1..=3).map(|stage|format!("100644 {} {stage}\tconflict\n",object.trim())).collect::<String>();
                command(&source,&["update-index","--index-info"],Some(records.as_bytes()));
            }
            if split {command(&source,&["update-index","--split-index"],None);}
            if mode=="intent" {fs::write(source.join("intent"),b"not staged yet").unwrap();command(&source,&["add","-N","intent"],None);}
            if mode=="flags" {command(&source,&["update-index","--assume-unchanged","tracked"],None);}
            fs::write(source.join("tracked"),b"unstaged bytes remain separate").unwrap();
            let identity=ResourceIdentity{device:1,inode:1,born_secs:1,born_nanos:0};
            let receipt=WorktreeReceipt{plan:WorktreePlan{source:RepositoryInput{repository:source.display().to_string(),commit:base.clone(),tree},path:source.display().to_string(),branch:"unused-by-object-reader".into()},directory:identity.clone(),git_directory:source.join(".git").display().to_string(),git_identity:identity.clone(),common_directory:source.join(".git").display().to_string(),common_identity:identity};
            let control=Control{deadline:Instant::now()+Duration::from_secs(15),cancellation:Default::default()};
            let guard=crate::execution_guard::RootGuard::exclusive(root.path()).unwrap();
            let git=Git{deadline:control.deadline,cancellation:control.cancellation.clone(),locks:guard.inherit().unwrap()};
            let mut blobs=BTreeMap::new();let archive=capture(&git,&receipt,&control,&mut (source_tree::BYTE_LIMIT as usize),&mut blobs).unwrap();
            assert_eq!(archive.state.head,head);assert_eq!(archive.state.base,base);assert_eq!(archive.state.shared_index.is_some(),split);
            let again=capture(&git,&receipt,&control,&mut (source_tree::BYTE_LIMIT as usize),&mut BTreeMap::new()).unwrap();assert_eq!(archive,again);
            assert!(capture(&git,&receipt,&control,&mut 1,&mut BTreeMap::new()).is_err());
            command(&source,&["update-index","--chmod=+x","tracked"],None);
            assert!(verify(&git,&receipt,&control,&archive).is_err());
            let restored=root.path().join("restored");fs::create_dir(&restored).unwrap();
            command(&restored,&["init","--quiet",format],None);
            command(&restored,&["index-pack","--stdin","--strict"],Some(&blobs[&archive.pack.sha256]));
            fs::write(restored.join(".git/index"),&blobs[&archive.state.index.as_ref().unwrap().sha256]).unwrap();
            if let Some(shared)=&archive.state.shared_index {fs::write(restored.join(".git").join(&shared.name),&blobs[&shared.file.sha256]).unwrap();}
            // Remove source objects before checking restoration independence.
            fs::remove_dir_all(source.join(".git")).unwrap();
            command(&restored,&["cat-file","-e",&format!("{base}^{{commit}}")],None);
            assert_eq!(command(&restored,&["show",&format!("{head}:tracked")],None),b"committed result");
            assert_eq!(command(&restored,&["show",":tracked"],None),staged);
            if mode=="flags" {assert!(string(&restored,&["ls-files","-v","tracked"]).starts_with("h "));}
            if mode=="intent" {assert!(string(&restored,&["ls-files","--debug","intent"]).contains("flags: 20004000"));}
            if mode=="conflict" {
                for stage in 1..=3 {assert_eq!(command(&restored,&["show",&format!(":{stage}:conflict")],None),b"conflict-only object");}
            }else{command(&restored,&["write-tree"],None);}
        }
    }
}
