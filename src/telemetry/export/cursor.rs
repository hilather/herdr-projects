//! Authenticated, scoped, expiring page cursors (docs/telemetry/contracts-export.md §4),
//! shared by the query service's drill-down pages and exports.
//!
//! A cursor is `c2.<hex payload>.<hex mac>`: the payload is compact JSON
//! `{v, kind, kid, project, iat, exp, ...position}` and the MAC is
//! HMAC-SHA256 of the payload bytes under a per-user key kept at
//! `<config_dir>/telemetry-cursor.key` (owner-only 0600, 64 hex digits,
//! generated on first use, never exported). `project` is a digest of the
//! canonical project path (never the path itself), `kid` the key's
//! fingerprint. Opening checks, in order: well-formed encoding, key present,
//! MAC over raw bytes, JSON and matching `kid`, expiry (`cursor_expired`), project scope
//! (`cursor_foreign_project`) and kind.
//! A missing or rotated key yields `cursor_revoked`; a failed MAC otherwise
//! yields `invalid_cursor`. On MAC failure, size-capped JSON is parsed only
//! to diagnose a rotated key.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const KEY_FILE: &str = "telemetry-cursor.key";
pub const PREFIX: &str = "c2";
/// Cursor lifetime from issue.
pub const TTL_MS: i64 = 30 * 60 * 1000;
/// Most payload bytes a cursor may carry (bounded parse).
const MAX_PAYLOAD: usize = 4096;

/// A refusal: `(code, detail)`, mapped by the caller onto its rejection type.
pub type Refusal = (&'static str, Value);

/// The per-user cursor key, loaded (or on first issue created) lazily.
pub struct Keyring {
    dir: Option<PathBuf>,
    key: OnceCell<([u8; 32], String)>,
}

fn euid() -> u32 { unsafe { libc::geteuid() } }

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { return None; }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// HMAC-SHA256 (RFC 2104) over `msg` with a 32-byte key.
pub fn hmac(key: &[u8; 32], msg: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    let ipad: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new().chain_update(&ipad).chain_update(msg).finalize();
    Sha256::new().chain_update(&opad).chain_update(inner).finalize().into()
}

fn equal(a: &[u8], b: &[u8]) -> bool { a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 }

/// The authorization scope of a project: a digest of its canonical path.
pub fn project_scope(project: &Path) -> Result<String> {
    let path = fs::canonicalize(project).with_context(|| "the project directory is unavailable")?;
    Ok(format!("sha256:{:x}", Sha256::digest(format!("herdr-projects/project:{}", path.display()).as_bytes())))
}

impl Keyring {
    pub fn new(config_dir: &Path) -> Self { Keyring { dir: Some(config_dir.to_path_buf()), key: OnceCell::new() } }

    /// `$HOME/.config/herdr-farm`, the product config directory (`paths::Env::config_dir`).
    pub fn from_home() -> Self {
        let dir = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| crate::product_environment::config_dir_for_home(&PathBuf::from(h)));
        Keyring { dir, key: OnceCell::new() }
    }

    fn path(&self) -> Option<PathBuf> { self.dir.as_ref().map(|d| d.join(KEY_FILE)) }

    /// Read the key: `None` when absent. A present key must be a regular,
    /// single-link, owner-only (0600) file of this user.
    fn read(path: &Path) -> Result<Option<[u8; 32]>> {
        let m = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("cursor key is unreadable"),
        };
        ensure!(m.file_type().is_file(), "cursor key {} must be a regular file, not a symlink", path.display());
        ensure!(m.uid() == euid() && m.nlink() == 1, "cursor key {} must be owned by this user with a single link", path.display());
        ensure!(m.mode() & 0o777 == 0o600, "cursor key {} must have mode 600 (has {:o})", path.display(), m.mode() & 0o777);
        let mut text = String::new();
        fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?.take(65).read_to_string(&mut text)?;
        let bytes = unhex(text.trim_end()).filter(|b| b.len() == 32).with_context(|| format!("cursor key {} is not 64 hex digits", path.display()))?;
        Ok(Some(bytes.try_into().expect("32 bytes")))
    }

    fn create(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("cursor key has no directory")?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).context("create the config directory for the cursor key")?;
        let m = fs::symlink_metadata(dir)?;
        ensure!(m.file_type().is_dir() && m.uid() == euid() && m.mode() & 0o022 == 0,
            "config directory {} must be a real directory owned by this user, not group/world writable", dir.display());
        let mut bytes = [0u8; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(mut file) => { file.write_all(hex(&bytes).as_bytes())?; file.sync_all()?; Ok(()) }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()), // a racing first use won; read theirs
            Err(e) => Err(e).context("create the cursor key"),
        }
    }

    /// The key and its id; `create` generates it on first use.
    fn key(&self, create: bool) -> Result<Option<&([u8; 32], String)>> {
        if let Some(k) = self.key.get() { return Ok(Some(k)); }
        let Some(path) = self.path() else {
            if create { bail!("cursor key unavailable: HOME is not set"); }
            return Ok(None);
        };
        let key = match Self::read(&path)? {
            Some(key) => key,
            None if create => { self.create(&path)?; Self::read(&path)?.context("cursor key vanished after creation")? }
            None => return Ok(None),
        };
        let kid = format!("{:x}", Sha256::digest([b"herdr-projects/cursor-key:".as_slice(), &key].concat()))[..16].to_owned();
        Ok(Some(self.key.get_or_init(|| (key, kid))))
    }

    /// Issue a cursor of `kind` for `project` carrying `position`; `(token, expires_unix_ms)`.
    pub fn seal(&self, kind: &str, project: &str, now: i64, position: Value) -> Result<(String, i64)> {
        let (key, kid) = self.key(true)?.context("cursor key unavailable")?;
        let exp = now + TTL_MS;
        let mut payload = json!({"v": 2, "kind": kind, "kid": kid, "project": project, "iat": now, "exp": exp});
        if let (Value::Object(p), Value::Object(pos)) = (&mut payload, position) { for (k, v) in pos { p.entry(k).or_insert(v); } }
        let text = serde_json::to_string(&payload)?;
        Ok((format!("{PREFIX}.{}.{}", hex(text.as_bytes()), hex(&hmac(key, text.as_bytes()))), exp))
    }

    /// Open a cursor of `kind` for `project`: its payload, or a refusal.
    pub fn open(&self, kind: &str, project: &str, now: i64, token: &str) -> std::result::Result<Value, Refusal> {
        let invalid = || ("invalid_cursor", json!({}));
        let parts: Vec<&str> = token.split('.').collect();
        let &[prefix, body, mac] = &parts[..] else { return Err(invalid()) };
        if prefix != PREFIX || body.len() > 2 * MAX_PAYLOAD || mac.len() != 64 { return Err(invalid()); }
        let bytes = unhex(body).ok_or_else(invalid)?;
        let mac = unhex(mac).ok_or_else(invalid)?;
        let revoked = || ("cursor_revoked", json!({"detail": "the cursor key was rotated or removed; start again without --cursor"}));
        let (key, kid) = match self.key(false) { Ok(Some(k)) => k, Ok(None) => return Err(revoked()), Err(e) => return Err(("cursor_key_unusable", json!({"detail": format!("{e:#}")}))) };
        if !equal(&hmac(key, &bytes), &mac) {
            // The size-capped, unauthenticated payload is used only to diagnose
            // revocation; no cursor claims are consumed before authentication.
            if serde_json::from_slice::<Value>(&bytes).ok().is_some_and(|p| p["kid"].as_str().is_some_and(|id| id != kid)) {
                return Err(revoked());
            }
            return Err(invalid());
        }
        let payload: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if payload["kid"] != kid.as_str() { return Err(revoked()); }
        let exp = payload["exp"].as_i64().ok_or_else(invalid)?;
        if now >= exp { return Err(("cursor_expired", json!({"expired_unix_ms": exp, "detail": "start again without --cursor"}))); }
        if payload["project"] != project { return Err(("cursor_foreign_project", json!({"detail": "a cursor is valid only for the project that issued it"}))); }
        if payload["v"] != 2 || payload["kind"] != kind { return Err(invalid()); }
        Ok(payload)
    }
}
