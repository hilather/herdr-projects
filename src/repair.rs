//! Explicit repair handoff. Inspection never rewrites damaged authoritative files.
use std::{fs::{self, File, OpenOptions}, io::{Read, Write}, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Component, Path}};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use crate::{inbox, project::{self, Project}, steps, thread};
const LIMIT: u64 = 16 * 1024 * 1024;
#[derive(Serialize)]
pub struct Diagnostic {
    path: String,
    code: &'static str,
    sha256: Option<String>,
    message: String,
}
fn read(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file() && meta.nlink() == 1, "expected a regular file with one link");
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "record exceeds inspection limit");
    Ok(bytes)
}
fn validate(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.to_str().context("non-UTF-8 record path")?;
    ensure!(path.components().all(|c| matches!(c, Component::Normal(_))), "record path must stay within the project");
    let parts: Vec<_> = name.split('/').collect();
    match parts.as_slice() {
        ["threads", file] if file.ends_with(".toml") => {
            let id = file.trim_end_matches(".toml");
            thread::validate_id(id)?;
            let record: thread::Thread = toml::from_str(std::str::from_utf8(bytes)?)?;
            ensure!(record.id == id, "thread identity does not match its filename");
        }
        [".state", "project.json"] => { serde_json::from_slice::<project::ProjectState>(bytes)?; }
        [".state", "ticker.json"] => { serde_json::from_slice::<steps::State>(bytes)?; }
        ["inbox", file] | ["inbox", "done", file] if file.ends_with(".md") => {
            let item = inbox::parse(std::str::from_utf8(bytes)?).context("invalid inbox frontmatter")?;
            ensure!(!item.id.is_empty() && item.id == file.trim_end_matches(".md"), "inbox identity does not match its filename");
            ensure!(!item.kind.is_empty(), "inbox kind is missing");
        }
        _ => anyhow::bail!("repair supports thread TOML, project/ticker JSON and inbox Markdown records only"),
    }
    Ok(())
}
fn parents(project: &Project, relative: &Path) -> Result<()> {
    ensure!(relative.components().all(|c| matches!(c, Component::Normal(_))), "unsafe record path");
    let mut path = project.dir();
    for component in relative.parent().context("missing record parent")?.components() {
        path.push(component);
        ensure!(fs::symlink_metadata(&path)?.is_dir(), "record parent is not a real directory");
    }
    Ok(())
}
pub fn inspect(project: &Project) -> Result<Vec<Diagnostic>> {
    let mut result = Vec::new();
    let mut paths = vec![std::path::PathBuf::from(".state/ticker.json"), std::path::PathBuf::from(".state/project.json")];
    for dir in ["threads", "inbox", "inbox/done"] {
        let relative = Path::new(dir).join("placeholder");
        if !project.dir().join(dir).exists() { continue; }
        parents(project, &relative)?;
        for entry in fs::read_dir(project.dir().join(dir))? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|e| e == if dir == "threads" { "toml" } else { "md" }) {
                paths.push(Path::new(dir).join(entry.file_name()));
            }
        }
    }
    paths.sort();
    for path in paths {
        if fs::symlink_metadata(project.dir().join(&path)).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) { continue; }
        let read = parents(project, &path).and_then(|()| read(&project.dir().join(&path)));
        let (hash, error) = match read {
            Ok(bytes) => (Some(thread::sha256_hex(&bytes)), validate(&path, &bytes).err()),
            Err(error) => (None, Some(error)),
        };
        if let Some(error) = error { result.push(Diagnostic { path: path.to_string_lossy().into_owned(), code: "invalid-record", sha256: hash, message: error.to_string() }); }
    }
    Ok(result)
}
pub fn restore(root: &Path, project: &Project, path: &Path, from: &Path, expected: &str) -> Result<String> {
    let replacement = read(from).context("cannot read replacement")?;
    validate(path, &replacement).context("replacement did not validate")?;
    // Keep the ticker lock until the repair commits; a probe alone races startup.
    let ticker = OpenOptions::new().write(true).create(true).truncate(false).custom_flags(libc::O_NOFOLLOW).open(root.join(".ticker.lock"))?;
    ticker.try_lock().context("stop the ticker before restoring authoritative records")?;
    let _lease = crate::cleanup::lease(root)?;
    let _lock = project.lock()?;
    parents(project, path)?;
    let target = project.dir().join(path);
    let original = read(&target)?;
    ensure!(thread::sha256_hex(&original) == expected, "record changed since inspection; inspect again before restoring");
    let backup = project.state_dir().join("repair-backups");
    match fs::create_dir(&backup) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}, Err(e) => return Err(e.into()) }
    ensure!(fs::symlink_metadata(&backup)?.is_dir(), "backup directory must not be a symlink");
    let saved = backup.join(format!("{expected}.original"));
    match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&saved) {
        Ok(mut file) => { file.write_all(&original)?; file.sync_all()?; }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { ensure!(read(&saved)? == original, "existing repair backup does not match"); }
        Err(e) => return Err(e.into()),
    }
    File::open(&backup)?.sync_all()?;
    File::open(project.state_dir())?.sync_all()?;
    project::write_atomic(&target, &replacement)?;
    File::open(target.parent().unwrap())?.sync_all()?;
    Ok(saved.display().to_string())
}
