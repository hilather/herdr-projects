//! Stock Herdr fallback for a supervised first pane. Stock `workspace.create`
//! always starts the default shell, so the controller types one line that
//! `exec`s this fixed launcher. Only shell-safe absolute paths are typed; the
//! literal argv travels in a private single-use file named by its digest.
use super::*;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

/// Hidden `herdr-farm` subcommand that consumes one launch spec.
pub const SUBCOMMAND: &str = "launch-exec";
const DIRECTORY: &str = "launch-specs";
const LIMIT: u64 = 128 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    version: u32,
    operation: String,
    cwd: String,
    command_digest: String,
    argv: Vec<String>,
}

/// One planned exec-into-shell launch. `line` needs no quoting in any shell and
/// has no newline: stock Herdr runs typed text only on an explicit Enter key.
pub(crate) struct Launch {
    pub(crate) line: String,
    pub(crate) spec: PathBuf,
}

pub(crate) fn digest(argv: &[String]) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(argv)?)))
}

fn safe(path: &Path) -> Result<&str> {
    let text = path.to_str().context("launch path is not UTF-8")?;
    ensure!(
        path.is_absolute()
            && text.len() <= 1024
            && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            && !text.split('/').any(|c| c == "." || c == ".."),
        "launch path {text:?} is outside the shell-safe character set [A-Za-z0-9/._-]"
    );
    Ok(text)
}

fn private_directory(dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "launch spec directory is not private to this user"
    );
    Ok(())
}

/// Validate every typed path before any effect. `state` is a controller-owned
/// directory; the spec name binds the file to the creation intent's digest.
pub(crate) fn plan(state: &Path, command_digest: &str) -> Result<Launch> {
    ensure!(
        command_digest.len() == 64 && command_digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid launch command digest"
    );
    let launcher = std::env::current_exe()?.canonicalize()?;
    ensure!(launcher.is_file(), "launcher executable missing");
    let spec = path(&state.canonicalize()?, command_digest);
    let line = format!("exec {} {SUBCOMMAND} {}", safe(&launcher)?, safe(&spec)?);
    Ok(Launch { line, spec })
}

pub(crate) fn path(state: &Path, command_digest: &str) -> PathBuf {
    state.join(DIRECTORY).join(format!("{command_digest}.spec"))
}

/// Write the single-use spec (0600, never replacing an existing file).
pub(crate) fn write(launch: &Launch, operation: &str, cwd: &str, argv: &[String]) -> Result<()> {
    let dir = launch.spec.parent().context("launch spec directory missing")?;
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    private_directory(dir)?;
    let bytes = serde_json::to_vec(&Spec {
        version: 1,
        operation: operation.into(),
        cwd: cwd.into(),
        command_digest: digest(argv)?,
        argv: argv.to_vec(),
    })?;
    ensure!(bytes.len() as u64 <= LIMIT, "launch spec exceeds limit");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&launch.spec)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Remove an unconsumed spec so a late shell exec can never start the worker.
pub(crate) fn remove(spec: &Path) -> Result<()> {
    match fs::remove_file(spec) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Launcher: validate and consume one spec, then replace this process with its
/// canonical supervisor argv. Returns only on refusal or exec failure.
pub fn exec(spec: &Path) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    safe(spec)?;
    let dir = spec.parent().context("launch spec directory missing")?;
    ensure!(
        dir.file_name().and_then(|n| n.to_str()) == Some(DIRECTORY),
        "launch spec outside a launch-specs directory"
    );
    private_directory(dir)?;
    let name = spec
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".spec"))
        .context("invalid launch spec name")?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(spec)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1
            && metadata.len() <= LIMIT,
        "launch spec is not a private single-link file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    let parsed: Spec = serde_json::from_slice(&bytes).context("invalid launch spec")?;
    ensure!(
        parsed.version == 1
            && parsed.command_digest == name
            && digest(&parsed.argv)? == name
            && Path::new(&parsed.cwd).is_absolute(),
        "launch spec does not match its bound command digest"
    );
    let argv = &parsed.argv;
    ensure!(argv.len() >= 15, "launch spec is not a supervisor command");
    let wall = argv[13]
        .strip_suffix('s')
        .context("invalid supervisor wall deadline")?
        .parse()?;
    ensure!(
        crate::worker_supervision::command(Path::new(&argv[14]), &argv[15..], wall)? == *argv,
        "launch spec is not the canonical supervisor vector"
    );
    // Single use: consumed before exec, so no retry can observe it again.
    fs::remove_file(spec)?;
    std::env::set_current_dir(&parsed.cwd)?;
    let error = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    Err(anyhow::Error::from(error).context("launcher exec failed"))
}
