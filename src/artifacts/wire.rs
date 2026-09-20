//! Version 1: magic, u32 JSON length, source manifest, concatenated file bytes.
//! The sender's successful exit certifies a second source scan matched.
use super::*;
use crate::{paths::Ctx, remote};

const MAGIC: &[u8; 8] = b"HPAR\x01\0\0\0";
pub(super) const STREAM_LIMIT: usize = BYTE_LIMIT as usize + MANIFEST_LIMIT + 12;

#[derive(Serialize, Deserialize)]
struct Source {
    schema: u32,
    source: String,
    entries: Vec<Entry>,
}

pub fn probe() {
    println!("{}", serde_json::json!({"schema": 1, "byte_limit": BYTE_LIMIT, "entry_limit": ENTRY_LIMIT, "live_versions": [1]}));
}

pub fn export(path: &Path, writer: &mut impl Write) -> Result<()> {
    let root=crate::source_tree::Directory::open(path)?;
    let source = Source { schema: 1, source: fs::canonicalize(path)?.to_str().context("source path is not UTF-8")?.into(), entries: scan_open(&root, None)? };
    validate(&source)?;
    let json = serde_json::to_vec(&source)?;
    ensure!(json.len() <= MANIFEST_LIMIT, "artifact manifest exceeds limit");
    writer.write_all(MAGIC)?;
    writer.write_all(&(json.len() as u32).to_be_bytes())?;
    writer.write_all(&json)?;
    let mut budget=crate::source_tree::Budget::new();
    for entry in &source.entries {
        if entry.directory {continue;}
        let mut file=root.file(Path::new(&entry.path))?;budget.size(&file)?;let before=file.metadata()?;
        let mut copied=0u64;let mut hash=Sha256::new();let mut buffer=[0;64*1024];
        loop {let n=budget.read(&mut file,&mut buffer)?;if n==0{break;}copied+=n as u64;ensure!(copied<=entry.bytes,"source file grew while streaming");hash.update(&buffer[..n]);writer.write_all(&buffer[..n])?;}
        crate::source_tree::unchanged(&file,&before)?;
        ensure!(copied==entry.bytes&&format!("{:x}",hash.finalize())==entry.sha256,"source file changed while streaming");
    }
    root.matches_path(path)?;
    ensure!(scan_open(&root,None)?==source.entries,"artifact source changed while streaming");
    root.matches_path(path)?;
    writer.flush()?;
    Ok(())
}

fn validate(source: &Source) -> Result<()> {
    ensure!(source.schema == 1 && Path::new(&source.source).is_absolute(), "unsupported artifact source schema or path");
    ensure!(source.entries.len() <= ENTRY_LIMIT, "too many artifact entries");
    let mut found = std::collections::BTreeSet::new();
    let mut directories = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for e in &source.entries {
        let parts: Vec<_> = e.path.split('/').collect();
        ensure!(parts.len() <= 65 && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains('\0')), "unsafe artifact path");
        ensure!(e.path == "report.md" || e.path == "library" || e.path.starts_with("library/"), "artifact path outside report/library");
        ensure!(found.insert(&e.path), "duplicate artifact path");
        if let Some((parent, _)) = e.path.rsplit_once('/') { ensure!(directories.contains(parent), "artifact parent missing or not a directory"); }
        if e.directory {
            ensure!(e.path != "report.md" && e.bytes == 0 && e.sha256.is_empty(), "invalid artifact directory");
            directories.insert(e.path.as_str());
        } else {
            ensure!(e.path != "library" && e.sha256.len() == 64 && e.sha256.bytes().all(|b| b.is_ascii_hexdigit()), "invalid artifact file");
            total = total.checked_add(e.bytes).context("artifact byte count overflow")?;
            ensure!(total <= BYTE_LIMIT, "artifact stream exceeds 50 MiB");
        }
    }
    Ok(())
}

fn receive(project: &Project, record: &Thread, staging: Staging, archive: &Path) -> Result<Snapshot> {
    receive_into(project,record,staging,archive,&Control::default(),true,||Ok(()),||Ok(()))
}

/// Caller must establish successful supervised sender completion before ingress.
/// This verifies and retains exact preservation bytes, never resolves a thread.
#[allow(dead_code)]
pub fn receive_controlled(project:&Project,record:&Thread,archive:&Path,control:&Control,authorize:impl FnOnce()->Result<()>)->Result<Snapshot> {
    control.check()?;
    let stage=staging(project,record)?;
    receive_into(project,record,stage,archive,control,false,authorize,||Ok(()))
}
fn receive_into(project:&Project,record:&Thread,staging:Staging,archive:&Path,control:&Control,remove_archive:bool,authorize:impl FnOnce()->Result<()>,mut after_file:impl FnMut()->Result<()>)->Result<Snapshot> {
    control.check()?;let mut stream=regular(archive)?;let before=stream.metadata()?;
    ensure!(before.len()<=STREAM_LIMIT as u64,"artifact stream exceeds bounds");
    let mut magic = [0; 8];stream.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "invalid artifact stream magic");
    let mut size = [0; 4];stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= MANIFEST_LIMIT, "artifact manifest exceeds limit");
    let mut json = vec![0; size];stream.read_exact(&mut json)?;
    let source: Source = serde_json::from_slice(&json)?;validate(&source)?;
    control.check()?;let mut budget=control.budget();
    for entry in &source.entries {
        budget.entry(entry.path.matches('/').count())?;
        let path = staging.0.join(&entry.path);
        if entry.directory { fs::create_dir(path)?; continue; }
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        let mut remaining=entry.bytes;let mut hash=Sha256::new();let mut buffer=[0;64*1024];
        while remaining>0 {
            let size=(remaining as usize).min(buffer.len());let n=budget.read(&mut stream,&mut buffer[..size])?;
            ensure!(n>0,"truncated artifact payload");file.write_all(&buffer[..n])?;hash.update(&buffer[..n]);remaining-=n as u64;
        }
        ensure!(format!("{:x}",hash.finalize())==entry.sha256,"artifact payload digest mismatch");file.sync_all()?;after_file()?;budget.check()?;
    }
    ensure!(stream.read(&mut [0])? == 0, "trailing artifact data");crate::source_tree::unchanged(&stream,&before)?;
    for entry in source.entries.iter().rev().filter(|e| e.directory) {budget.check()?;File::open(staging.0.join(&entry.path))?.sync_all()?;}
    if remove_archive {fs::remove_file(archive)?;}
    let manifest = Manifest { schema: 1, thread: record.id.clone(), generation: record.lifecycle_generation, source: source.source, machine: record.machine.clone(), entries: source.entries };
    budget.check()?;
    publish_mode_controlled(project,record,staging,manifest,false,control,authorize)
}

pub fn capture_remote(ctx: &Ctx, project: &Project, record: &Thread, target: &str) -> Result<Snapshot> {
    let binary = ctx.env.var("HERDR_PROJECTS_REMOTE_BIN").unwrap_or("herdr-projects");
    let command = format!("{} artifact-stream", remote::quote(binary));
    let capability = remote::ssh(ctx.runner, target, &format!("{command} --probe"), None, remote::SSH_TIMEOUT)?;
    if capability.code == Some(255) || capability.timed_out || capability.cancelled {
        anyhow::bail!("artifact helper connection failed: {}", capability.error_text());
    }
    let version = serde_json::from_slice::<serde_json::Value>(&capability.stdout_bytes).ok();
    if !capability.success() || version.as_ref().and_then(|v| v["schema"].as_u64()) != Some(1) {
        anyhow::bail!("[transport-unsupported] install a compatible herdr-projects artifact-stream helper on {target} (or set HERDR_PROJECTS_REMOTE_BIN), then run `thread resolve {} {}` to retry", project.slug, record.id);
    }
    let staging = staging(project, record)?;
    let archive = staging.0.join(".wire");
    let mut cmd = remote::ssh_command(target, &format!("{command} --path {}", remote::quote(&record.thread_dir)), remote::COPY_TIMEOUT)?;
    cmd.stdout_file = Some((archive.clone(), STREAM_LIMIT));
    let output = ctx.runner.run(&cmd)?;
    ensure!(output.success(), "artifact stream failed: {}", output.error_text());
    receive(project, record, staging, &archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(root:&Path,record:&Thread)->PathBuf {
        let mut bytes=Vec::new();export(Path::new(&record.thread_dir),&mut bytes).unwrap();
        let archive=root.join("stream");fs::write(&archive,bytes).unwrap();archive
    }
    fn stages(project:&Project)->Vec<String> {
        fs::read_dir(project.state_dir().join("artifacts/t-0001")).into_iter().flatten().map(|e|e.unwrap().file_name().into_string().unwrap()).collect()
    }
    #[test]
    fn controlled_receive_retains_verified_bytes_and_leaves_input_owned_by_caller() {
        let (root,project,record)=super::super::tests::fixture();let archive=archive(root.path(),&record);
        let saved=receive_controlled(&project,&record,&archive,&Control::default(),||Ok(())).unwrap();
        assert!(archive.is_file());assert_eq!(load(&project,&record,&saved.id).unwrap(),saved.manifest);
        assert_eq!(fs::read(project.state_dir().join("artifacts/t-0001").join(&saved.id).join("report.md")).unwrap(),b"report\0\xff");
        assert!(!project.dir().join("threads/t-0001.toml").exists(),"preservation alone cannot resolve or certify a thread");
        assert_eq!(receive_controlled(&project,&record,&archive,&Control::default(),||Ok(())).unwrap().id,saved.id);
        assert_eq!(stages(&project),vec![saved.id]);
    }
    #[test]
    fn cancellation_and_authority_withdrawal_never_publish_a_new_snapshot() {
        for fault in ["cancelled","expired","during-extraction","before-publish","authority"] {
            let(root,project,record)=super::super::tests::fixture();let archive=archive(root.path(),&record);let mut control=Control::default();
            match fault {"cancelled"=>control.cancellation.cancel(),"expired"=>control.deadline=std::time::Instant::now(),_=>{}}
            let result=if fault=="during-extraction" {
                let stage=staging(&project,&record).unwrap();
                receive_into(&project,&record,stage,&archive,&control,false,||Ok(()),||{control.cancellation.cancel();Ok(())})
            }else {
                receive_controlled(&project,&record,&archive,&control,||{
                    if fault=="before-publish" {control.cancellation.cancel();}
                    ensure!(fault!="authority","authority withdrawn");Ok(())
                })
            };
            assert!(result.is_err(),"{fault}");assert!(stages(&project).is_empty(),"{fault}");assert!(archive.is_file());
            assert!(!project.dir().join("threads/t-0001.toml").exists());
        }
    }
    #[test]
    fn controlled_receive_refuses_special_archives_and_preserves_existing_evidence() {
        use std::os::unix::{ffi::OsStrExt,fs::symlink};
        let(root,project,record)=super::super::tests::fixture();let archive=archive(root.path(),&record);
        let saved=receive_controlled(&project,&record,&archive,&Control::default(),||Ok(())).unwrap();
        for kind in ["fifo","symlink","oversized","corrupt"] {
            let bad=root.path().join(kind);
            match kind {
                "fifo"=>{let c=std::ffi::CString::new(bad.as_os_str().as_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(c.as_ptr(),0o600)},0);},
                "symlink"=>symlink(&archive,&bad).unwrap(),
                "oversized"=>File::create(&bad).unwrap().set_len(STREAM_LIMIT as u64+1).unwrap(),
                _=>{let mut bytes=fs::read(&archive).unwrap();*bytes.last_mut().unwrap()^=255;fs::write(&bad,bytes).unwrap();},
            }
            let before=std::time::Instant::now();assert!(receive_controlled(&project,&record,&bad,&Control::default(),||Ok(())).is_err());assert!(before.elapsed()<std::time::Duration::from_secs(2));
            assert_eq!(stages(&project),vec![saved.id.clone()]);assert_eq!(load(&project,&record,&saved.id).unwrap(),saved.manifest);
        }
        let control=Control::default();assert!(receive_controlled(&project,&record,&archive,&control,||{control.cancellation.cancel();Ok(())}).is_err());
        assert_eq!(stages(&project),vec![saved.id.clone()]);assert_eq!(load(&project,&record,&saved.id).unwrap(),saved.manifest);
    }

    #[test]
    fn roundtrip_preserves_binary_empty_directories_and_hostile_names() {
        let (_root, project, mut record) = super::super::tests::fixture();
        fs::write(Path::new(&record.thread_dir).join("library/a 'λ$\nfile"), [0, 255, 254]).unwrap();
        let mut bytes = Vec::new();
        export(Path::new(&record.thread_dir), &mut bytes).unwrap();
        record.machine = "remote".into();
        let staging = staging(&project, &record).unwrap();
        let archive = staging.0.join(".wire");
        fs::write(&archive, bytes).unwrap();
        let snapshot = receive(&project, &record, staging, &archive).unwrap();
        load(&project, &record, &snapshot.id).unwrap();
        assert_eq!(snapshot.manifest.machine, "remote");
    }

    #[test]
    fn remote_handshake_blocks_old_helpers_and_streams_verified_bytes() {
        use crate::{paths::Env, runner::fake::{FakeRunner, ok, fail}};
        let (root, project, mut record) = super::super::tests::fixture();
        record.machine = "fixture".into();
        let env = Env::for_test(root.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("--probe", fail(127, "helper missing"));
        let ctx = Ctx { root: root.path().into(), config_dir: root.path().join("cfg"), env: &env, runner: &runner, detached_ticker: false };
        assert!(capture_remote(&ctx, &project, &record, "fixture").err().unwrap().to_string().contains("[transport-unsupported]"));
        assert!(!project.state_dir().join("artifacts").exists());
        let runner = FakeRunner::new();
        runner.on("--probe", ok(r#"{"schema":1}"#));
        let mut output = ok("");
        export(Path::new(&record.thread_dir), &mut output.stdout_bytes).unwrap();
        runner.on("--path", output);
        let ctx = Ctx { runner: &runner, ..ctx };
        let result = capture_remote(&ctx, &project, &record, "fixture").unwrap();
        assert_eq!(result.manifest.machine, "fixture");
        load(&project, &record, &result.id).unwrap();
    }

    #[test]
    fn sender_rechecks_source_after_streaming() {
        struct Racing { path: PathBuf, bytes: Vec<u8> }
        impl Write for Racing {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.bytes.is_empty() { fs::write(&self.path, b"changed during stream")?; }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let (_root, _project, record) = super::super::tests::fixture();
        let mut writer = Racing { path: Path::new(&record.thread_dir).join("report.md"), bytes: Vec::new() };
        assert!(export(Path::new(&record.thread_dir), &mut writer).is_err());
    }

    #[test]
    fn traversal_duplicates_truncated_and_corrupt_payloads_are_rejected() {
        let (_root, project, record) = super::super::tests::fixture();
        let mut good = Vec::new();
        export(Path::new(&record.thread_dir), &mut good).unwrap();
        for fault in ["truncated", "corrupt", "traversal", "duplicate", "oversize"] {
            let mut data = good.clone();
            if fault == "truncated" { data.pop(); }
            else if fault == "corrupt" { *data.last_mut().unwrap() ^= 255; }
            else {
                let size = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
                let mut source: Source = serde_json::from_slice(&data[12..12 + size]).unwrap();
                match fault {
                    "traversal" => source.entries[0].path = "../escape".into(),
                    "duplicate" => source.entries.push(source.entries[0].clone()),
                    _ => source.entries.iter_mut().find(|entry| !entry.directory).unwrap().bytes = BYTE_LIMIT + 1,
                }
                let json = serde_json::to_vec(&source).unwrap();
                data = [MAGIC.as_slice(), &(json.len() as u32).to_be_bytes(), &json].concat();
            }
            let staging = staging(&project, &record).unwrap();
            let archive = staging.0.join(".wire");
            fs::write(&archive, data).unwrap();
            assert!(receive(&project, &record, staging, &archive).is_err(), "{fault}");
        }
    }
}
