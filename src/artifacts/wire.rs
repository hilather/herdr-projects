//! Version 1: magic, u32 JSON length, source manifest, concatenated file bytes.
//! The sender's successful exit certifies a second source scan matched.
use super::*;
use crate::{paths::Ctx, remote};

const MAGIC: &[u8; 8] = b"HPAR\x01\0\0\0";
const STREAM_LIMIT: usize = BYTE_LIMIT as usize + MANIFEST_LIMIT + 12;

#[derive(Serialize, Deserialize)]
struct Source {
    schema: u32,
    source: String,
    entries: Vec<Entry>,
}

pub fn probe() {
    println!("{}", serde_json::json!({"schema": 1, "byte_limit": BYTE_LIMIT, "entry_limit": ENTRY_LIMIT}));
}

pub fn export(path: &Path, writer: &mut impl Write) -> Result<()> {
    real_dir(path)?;
    let source = Source { schema: 1, source: fs::canonicalize(path)?.to_str().context("source path is not UTF-8")?.into(), entries: scan(path, None)? };
    validate(&source)?;
    let json = serde_json::to_vec(&source)?;
    ensure!(json.len() <= MANIFEST_LIMIT, "artifact manifest exceeds limit");
    writer.write_all(MAGIC)?;
    writer.write_all(&(json.len() as u32).to_be_bytes())?;
    writer.write_all(&json)?;
    for entry in &source.entries {
        if entry.directory { continue; }
        let mut file = regular(&path.join(&entry.path))?.take(entry.bytes);
        ensure!(std::io::copy(&mut file, writer)? == entry.bytes, "source file disappeared or shrank");
    }
    ensure!(fs::canonicalize(path)? == Path::new(&source.source) && scan(path, None)? == source.entries, "artifact source changed while streaming");
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
    let mut stream = File::open(archive)?;
    let mut magic = [0; 8];
    stream.read_exact(&mut magic)?;
    ensure!(&magic == MAGIC, "invalid artifact stream magic");
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    ensure!(size <= MANIFEST_LIMIT, "artifact manifest exceeds limit");
    let mut json = vec![0; size];
    stream.read_exact(&mut json)?;
    let source: Source = serde_json::from_slice(&json)?;
    validate(&source)?;
    for entry in &source.entries {
        let path = staging.0.join(&entry.path);
        if entry.directory { fs::create_dir(path)?; continue; }
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
        ensure!(std::io::copy(&mut (&mut stream).take(entry.bytes), &mut file)? == entry.bytes, "truncated artifact payload");
        file.sync_all()?;
    }
    ensure!(stream.read(&mut [0])? == 0, "trailing artifact data");
    for entry in source.entries.iter().rev().filter(|e| e.directory) { File::open(staging.0.join(&entry.path))?.sync_all()?; }
    fs::remove_file(archive)?;
    let manifest = Manifest { schema: 1, thread: record.id.clone(), generation: record.lifecycle_generation, source: source.source, machine: record.machine.clone(), entries: source.entries };
    publish(project, record, staging, manifest)
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
