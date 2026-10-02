//! Short, private directories for Unix control sockets.
//!
//! `sun_path` holds 108 bytes including the NUL, so a socket cannot live in a
//! deep state or temporary directory. Sockets go in a small owner-only
//! directory under a short private base (the user's private
//! `XDG_RUNTIME_DIR`, else `/tmp/hp-sock-UID`); logs and configuration stay in
//! their long-lived directories.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Longest socket path `bind` accepts (`sun_path` minus its NUL).
pub const MAX_SOCKET_PATH: usize = 107;

fn private_dir(path: &Path) -> bool {
    let uid = unsafe { libc::geteuid() };
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0)
}

fn base() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && runtime.is_absolute()
        && private_dir(&runtime)
    {
        return Ok(runtime);
    }
    let base = PathBuf::from(format!("/tmp/hp-sock-{}", unsafe { libc::geteuid() }));
    match fs::DirBuilder::new().mode(0o700).create(&base) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("cannot create {}", base.display())),
    }
    ensure!(private_dir(&base), "{} must be a directory owned by you with mode 0700", base.display());
    Ok(base)
}

/// Refuse a socket path `bind` could not accept, naming the path and length.
pub fn check_length(socket: &Path) -> Result<()> {
    let length = socket.as_os_str().len();
    if length > MAX_SOCKET_PATH {
        bail!(
            "Unix socket path {} is {length} bytes; the limit is {MAX_SOCKET_PATH}. Set XDG_RUNTIME_DIR to a shorter private directory",
            socket.display()
        );
    }
    Ok(())
}

fn create(name: &str, reuse: bool) -> Result<PathBuf> {
    let dir = base()?.join(name);
    check_length(&dir.join("s"))?;
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if reuse && error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(private_dir(&dir), "{} must be a directory owned by you with mode 0700", dir.display());
        }
        Err(error) => return Err(error).with_context(|| format!("cannot create {}", dir.display())),
    }
    Ok(dir)
}

/// A fresh, random, never-reused socket directory; the caller removes it.
pub fn fresh() -> Result<PathBuf> {
    let mut bytes = [0u8; 8];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    create(&format!("l{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()), false)
}

/// The socket directory for one long-lived server, stable for `seed` so a
/// rerun finds the server it started.
pub fn stable(seed: &Path) -> Result<PathBuf> {
    let digest = Sha256::digest(seed.as_os_str().as_encoded_bytes());
    create(&format!("r{}", digest.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>()), true)
}
