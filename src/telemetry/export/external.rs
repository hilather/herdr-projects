//! The optional external metric export (contracts-export.md §6): a
//! deployment setting in `<config_dir>/telemetry-export.toml`, disabled by
//! default (no file, or `enabled = false`, refuses). Destinations are local
//! only: a directory (for a separate shipper to pick up) or stdout (a pipe).
//! There is no network client.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "telemetry-export.toml";
pub const CONFIG_SCHEMA: &str = "telemetry-export-config.v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config { schema: String, external: Option<External> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct External { enabled: bool, destination: String, directory: Option<PathBuf> }

pub enum Destination { Directory(PathBuf), Stdout }

impl Destination {
    pub fn kind(&self) -> &'static str { match self { Destination::Directory(_) => "directory", Destination::Stdout => "stdout" } }
}

fn euid() -> u32 { unsafe { libc::geteuid() } }

/// The enabled destination, or the refusal `(code, detail)`.
pub fn load(config_dir: &Path) -> Result<std::result::Result<Destination, (&'static str, String)>> {
    load_setting(config_dir, &Setting { file: CONFIG_FILE, schema: CONFIG_SCHEMA, what: "export", disabled: "external_export_disabled" })
}

/// One external-destination deployment setting: its file under the config
/// directory, schema, what it sends, and the refusal code while
/// disabled. Shared by exports and TM4.5 alert notices.
#[derive(Clone, Copy)]
pub struct Setting { pub file: &'static str, pub schema: &'static str, pub what: &'static str, pub disabled: &'static str }

/// The enabled destination of `setting`, or the refusal `(code, detail)`.
pub fn load_setting(config_dir: &Path, setting: &Setting) -> Result<std::result::Result<Destination, (&'static str, String)>> {
    let Setting { file, schema, what, disabled } = *setting;
    let path = config_dir.join(file);
    let m = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Err((disabled, format!("no {file}: external {what} is off by default")))),
        Err(e) => return Err(e).with_context(|| format!("read the external {what} configuration")),
    };
    ensure!(m.file_type().is_file() && m.uid() == euid() && m.mode() & 0o022 == 0 && m.len() <= 16 * 1024,
        "{file} must be a regular file owned by this user, not group/world writable, at most 16 KiB");
    let config: Config = toml::from_str(&fs::read_to_string(&path)?).map_err(|e| anyhow::anyhow!("invalid {file}: {e}"))?;
    ensure!(config.schema == schema, "{file} schema must be {schema}");
    let Some(external) = config.external.filter(|e| e.enabled) else {
        return Ok(Err((disabled, format!("[external] enabled is not true in {file}"))));
    };
    Ok(Ok(match external.destination.as_str() {
        "stdout" => { ensure!(external.directory.is_none(), "`directory` applies to destination = \"directory\" only"); Destination::Stdout }
        "directory" => {
            let dir = external.directory.context("destination = \"directory\" needs `directory`")?;
            ensure!(dir.is_absolute(), "the external {what} directory must be an absolute path");
            let m = fs::symlink_metadata(&dir).with_context(|| format!("the external {what} directory is unavailable"))?;
            ensure!(m.file_type().is_dir() && m.uid() == euid() && m.mode() & 0o022 == 0,
                "the external {what} directory must be a real directory owned by this user, not group/world writable");
            Destination::Directory(dir)
        }
        other => anyhow::bail!("unknown external {what} destination `{other}` (directory or stdout)"),
    }))
}

/// Write `bytes` to a new file `path` (never replacing one): a hidden owner-only
/// partial file is written and synced, then linked into place, so a reader
/// never sees an incomplete export under the final name.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().context("the output path has no file name")?.to_string_lossy().into_owned();
    ensure!(fs::symlink_metadata(path).is_err(), "{} exists; an export is written to a new file", path.display());
    let partial = dir.join(format!(".{name}.partial-{}-{}", std::process::id(), jiff::Timestamp::now().as_nanosecond()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&partial)
            .with_context(|| format!("create {}", partial.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::hard_link(&partial, path).with_context(|| format!("{} exists or cannot be created; an export is written to a new file", path.display()))
    })();
    let _ = fs::remove_file(&partial);
    result
}
