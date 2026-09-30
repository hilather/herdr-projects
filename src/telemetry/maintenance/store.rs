//! `<project>/.state/telemetry-ops.db`: the operations store beside the
//! sidecar (docs/telemetry/operations-runbook.md). It holds what must outlive
//! a sidecar rebuild or restore: salted deletion tombstones, holds, the
//! maintenance run log, the backup inventory and restore reports. It is not
//! derived from `telemetry.db` and is backed up with it. Tombstones, runs and
//! restores are append-only; a hold changes once, when it is released.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const FILE: &str = "telemetry-ops.db";
pub const PRINCIPAL: &str = "operator:cli";
/// The class name under which hold scopes are digested.
pub const HOLD_SCOPE: &str = "hold.scope";
const VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ops_salts (
    salt_id INTEGER PRIMARY KEY,
    salt TEXT NOT NULL UNIQUE CHECK (length(salt) = 64),
    created_unix_ms INTEGER NOT NULL
) STRICT;
-- One deletion: its retention class, an opaque key (sha256 over the salt, the
-- class and the key; never the key itself) and/or an age cutoff (rows older
-- than it). No deleted content, label or unsalted hash is stored.
CREATE TABLE IF NOT EXISTS tombstones (
    seq INTEGER PRIMARY KEY,
    class TEXT NOT NULL CHECK (length(class) BETWEEN 1 AND 64),
    salt_id INTEGER NOT NULL REFERENCES ops_salts(salt_id),
    key_digest TEXT CHECK (key_digest IS NULL OR (length(key_digest) = 71 AND substr(key_digest, 1, 7) = 'sha256:')),
    before_unix_ms INTEGER,
    reason TEXT NOT NULL CHECK (reason IN ('retention_expired', 'operator_deletion')),
    run_id TEXT NOT NULL,
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    recorded_unix_ms INTEGER NOT NULL,
    CHECK (key_digest IS NOT NULL OR before_unix_ms IS NOT NULL)
) STRICT;
CREATE INDEX IF NOT EXISTS tombstones_key ON tombstones(class, key_digest);
CREATE TRIGGER IF NOT EXISTS tombstones_no_update BEFORE UPDATE ON tombstones BEGIN SELECT RAISE(ABORT, 'tombstones are append-only'); END;
CREATE TRIGGER IF NOT EXISTS tombstones_no_delete BEFORE DELETE ON tombstones BEGIN SELECT RAISE(ABORT, 'tombstones are append-only'); END;
CREATE TABLE IF NOT EXISTS holds (
    hold_id TEXT PRIMARY KEY,
    class TEXT NOT NULL CHECK (length(class) BETWEEN 1 AND 64),
    scope TEXT CHECK (scope IS NULL OR length(scope) BETWEEN 1 AND 160),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 160),
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    placed_unix_ms INTEGER NOT NULL,
    released_unix_ms INTEGER,
    release_reason TEXT CHECK (release_reason IS NULL OR length(release_reason) BETWEEN 1 AND 160),
    CHECK ((released_unix_ms IS NULL) = (release_reason IS NULL))
) STRICT;
CREATE TRIGGER IF NOT EXISTS holds_release_once BEFORE UPDATE ON holds
WHEN OLD.released_unix_ms IS NOT NULL OR NEW.hold_id IS NOT OLD.hold_id OR NEW.class IS NOT OLD.class OR NEW.scope IS NOT OLD.scope
  OR NEW.reason IS NOT OLD.reason OR NEW.principal IS NOT OLD.principal OR NEW.placed_unix_ms IS NOT OLD.placed_unix_ms
BEGIN SELECT RAISE(ABORT, 'a hold changes only once, when it is released'); END;
CREATE TRIGGER IF NOT EXISTS holds_no_delete BEFORE DELETE ON holds BEGIN SELECT RAISE(ABORT, 'holds are never deleted'); END;
CREATE TABLE IF NOT EXISTS maintenance_runs (
    run_id TEXT PRIMARY KEY,
    plan_digest TEXT NOT NULL,
    policy TEXT NOT NULL,
    principal TEXT NOT NULL CHECK (principal = 'operator:cli'),
    started_unix_ms INTEGER NOT NULL,
    completed_unix_ms INTEGER,
    summary TEXT CHECK (summary IS NULL OR json_valid(summary))
) STRICT;
CREATE TABLE IF NOT EXISTS backups (
    backup_id TEXT PRIMARY KEY,
    location TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    encrypted INTEGER NOT NULL CHECK (encrypted IN (0, 1)),
    deleted_unix_ms INTEGER
) STRICT;
CREATE TABLE IF NOT EXISTS restores (
    restore_id TEXT PRIMARY KEY,
    backup_id TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    restored_unix_ms INTEGER NOT NULL,
    forced INTEGER NOT NULL CHECK (forced IN (0, 1)),
    report TEXT NOT NULL CHECK (json_valid(report))
) STRICT;
CREATE TRIGGER IF NOT EXISTS restores_no_change BEFORE UPDATE ON restores BEGIN SELECT RAISE(ABORT, 'restore reports are append-only'); END;
PRAGMA user_version = 1;
";

pub fn path(project: &Path) -> PathBuf { project.join(".state").join(FILE) }
pub fn now() -> i64 { jiff::Timestamp::now().as_millisecond() }
pub fn sha256(bytes: &[u8]) -> String { format!("sha256:{:x}", Sha256::digest(bytes)) }

/// Open (creating mode 0600) and migrate the operations store.
pub fn open(project: &Path) -> Result<Connection> {
    open_at(&path(project))
}

pub fn open_at(path: &Path) -> Result<Connection> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => ensure!(meta.is_file(), "{} is not a regular file", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)
                .with_context(|| format!("create {}", path.display()))?;
        }
        Err(error) => return Err(error.into()),
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > VERSION { bail!("{} version {version} is newer than this binary (knows {VERSION})", path.display()); }
    if version < VERSION { db.execute_batch(&format!("BEGIN IMMEDIATE; {SCHEMA} COMMIT;"))?; }
    Ok(db)
}

/// The operations store read-only, or `None` when it does not exist yet.
pub(crate) fn read(project: &Path) -> Result<Option<super::super::ReadOnly>> {
    let path = path(project);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(meta) if !meta.is_file() => bail!("{} is not a regular file", path.display()),
        Ok(_) => {
            let db = super::super::read_only(&path)?;
            let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(version <= VERSION, "{} version {version} is newer than this binary (knows {VERSION})", path.display());
            Ok((version > 0).then_some(db))
        }
    }
}

/// The current salt (created on first use), from `/dev/urandom`.
pub fn salt(db: &Connection) -> Result<(i64, String)> {
    if let Some(found) = db.query_row("SELECT salt_id,salt FROM ops_salts ORDER BY salt_id DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
        return Ok(found);
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let salt: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    db.execute("INSERT INTO ops_salts(salt,created_unix_ms) VALUES(?1,?2)", params![salt, now()])?;
    Ok((db.last_insert_rowid(), salt))
}

/// The opaque tombstone key of `key` in `class` under `salt`.
pub fn key_digest(salt: &str, class: &str, key: &str) -> String {
    sha256(format!("{salt}\0{class}\0{key}").as_bytes())
}

/// Every tombstone, grouped for matching: `(class, key digest) -> cutoff`
/// (`None`: every row of the key), class-wide cutoffs, and the salts.
#[derive(Default, Clone)]
pub struct Tombstones {
    pub salts: Vec<String>,
    pub keys: BTreeMap<(String, String), Option<i64>>,
    pub before: BTreeMap<String, i64>,
    pub count: usize,
}

impl Tombstones {
    pub fn load(db: &Connection) -> Result<Self> {
        let mut t = Tombstones { salts: db.prepare("SELECT salt FROM ops_salts ORDER BY salt_id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?, ..Default::default() };
        let rows: Vec<(String, Option<String>, Option<i64>)> = db.prepare("SELECT class,key_digest,before_unix_ms FROM tombstones ORDER BY seq")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        t.count = rows.len();
        for (class, key, before) in rows {
            match key {
                Some(key) => {
                    let slot = t.keys.entry((class, key)).or_insert(before);
                    // A key tombstoned without a cutoff covers every row; else the latest cutoff wins.
                    *slot = match (*slot, before) { (None, _) | (_, None) => None, (Some(a), Some(b)) => Some(a.max(b)) };
                }
                None => { let slot = t.before.entry(class).or_insert(i64::MIN); *slot = (*slot).max(before.unwrap_or(i64::MIN)); }
            }
        }
        Ok(t)
    }

    /// The operations store's tombstones, or none without a store.
    pub fn of(project: &Path) -> Result<Self> {
        match read(project)? { Some(db) => Self::load(&db), None => Ok(Self::default()) }
    }

    /// Whether `key` of `class` is tombstoned: `Some(None)` every row,
    /// `Some(Some(cutoff))` rows older than the cutoff.
    pub fn key(&self, class: &str, key: &str) -> Option<Option<i64>> {
        self.salts.iter().find_map(|salt| self.keys.get(&(class.to_owned(), key_digest(salt, class, key))).copied())
    }

    pub fn is_empty(&self) -> bool { self.count == 0 }
}

/// Append tombstones (idempotent per class, key and cutoff) under the current salt.
pub fn tombstone(db: &Connection, class: &str, keys: &[(Option<&str>, Option<i64>)], reason: &str, run: &str, at: i64) -> Result<usize> {
    let (salt_id, salt) = salt(db)?;
    let mut added = 0;
    for (key, before) in keys {
        let digest = key.map(|k| key_digest(&salt, class, k));
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE class=?1 AND salt_id=?2 AND key_digest IS ?3 AND before_unix_ms IS ?4)",
            params![class, salt_id, digest, before], |r| r.get(0))?;
        if exists { continue; }
        db.execute("INSERT INTO tombstones(class,salt_id,key_digest,before_unix_ms,reason,run_id,principal,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![class, salt_id, digest, before, reason, run, PRINCIPAL, at])?;
        added += 1;
    }
    Ok(added)
}

/// `(class, salt id, key digest, cutoff, reason, run, recorded)`.
type TombstoneRow = (String, i64, Option<String>, Option<i64>, String, String, i64);

/// Import another operations store's salts and tombstones (a restored
/// backup's), keeping each under its own salt so its keys still match.
pub fn merge_from(db: &Connection, other: &Connection) -> Result<usize> {
    let salts: Vec<(i64, String, i64)> = other.prepare("SELECT salt_id,salt,created_unix_ms FROM ops_salts")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut map = BTreeMap::new();
    for (id, salt, created) in salts {
        db.execute("INSERT OR IGNORE INTO ops_salts(salt,created_unix_ms) VALUES(?1,?2)", params![salt, created])?;
        map.insert(id, db.query_row("SELECT salt_id FROM ops_salts WHERE salt=?1", [&salt], |r| r.get::<_, i64>(0))?);
    }
    let rows: Vec<TombstoneRow> = other.prepare("SELECT class,salt_id,key_digest,before_unix_ms,reason,run_id,recorded_unix_ms FROM tombstones ORDER BY seq")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut added = 0;
    for (class, salt_id, key, before, reason, run, at) in rows {
        let salt_id = map[&salt_id];
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE class=?1 AND salt_id=?2 AND key_digest IS ?3 AND before_unix_ms IS ?4)",
            params![class, salt_id, key, before], |r| r.get(0))?;
        if exists { continue; }
        db.execute("INSERT INTO tombstones(class,salt_id,key_digest,before_unix_ms,reason,run_id,principal,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![class, salt_id, key, before, reason, run, PRINCIPAL, at])?;
        added += 1;
    }
    Ok(added)
}

/// One active or released hold.
#[derive(Clone, Debug)]
pub struct Hold { pub id: String, pub class: String, pub scope: Option<String>, pub reason: String, pub placed: i64, pub released: Option<i64> }

impl Hold {
    /// Whether this active hold covers `class` and one of `scopes` (`all`: every class; no scope: every key).
    pub fn covers(&self, class: &str, scopes: &[&str], salts: &[String]) -> bool {
        self.released.is_none() && (self.class == "all" || self.class == class)
            && self.scope.as_deref().is_none_or(|d| scopes.iter().any(|s| salts.iter().any(|salt| key_digest(salt, HOLD_SCOPE, s) == d)))
    }
    pub fn json(&self) -> Value {
        json!({"hold_id": self.id, "class": self.class, "scope_digest": self.scope, "reason": self.reason, "placed_unix_ms": self.placed, "released_unix_ms": self.released})
    }
}

pub fn holds(db: &Connection) -> Result<Vec<Hold>> {
    Ok(db.prepare("SELECT hold_id,class,scope,reason,placed_unix_ms,released_unix_ms FROM holds ORDER BY placed_unix_ms,hold_id")?
        .query_map([], |r| Ok(Hold { id: r.get(0)?, class: r.get(1)?, scope: r.get(2)?, reason: r.get(3)?, placed: r.get(4)?, released: r.get(5)? }))?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn holds_of(project: &Path) -> Result<Vec<Hold>> {
    match read(project)? { Some(db) => holds(&db), None => Ok(Vec::new()) }
}

/// Operator free text as stored: the contracts §7 excerpt (secrets masked, 160 scalars).
pub fn note(text: &str) -> Result<String> {
    let home = std::env::var("HOME").ok();
    crate::domain::excerpt(text, home.as_deref()).filter(|t| !t.is_empty()).context("the reason is empty after redaction")
}

/// A hold scope is a key (session, attempt, task or backup id), stored as
/// given so it matches; anything else, and anything shaped like a credential,
/// is refused rather than stored.
fn scope_key(scope: &str) -> Result<String> {
    ensure!(!scope.is_empty() && scope.len() <= 160 && scope.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "a hold scope is a session, attempt, task or backup id (letters, digits, '-', '_', '.', ':')");
    let lower = scope.to_ascii_lowercase();
    ensure!(!["sk-", "ghp_", "gho_", "github_pat_", "xox", "akia", "bearer"].iter().any(|p| lower.starts_with(p)), "a hold scope must not look like a credential");
    Ok(scope.to_owned())
}

pub fn add_hold(project: &Path, class: &str, scope: Option<&str>, reason: &str) -> Result<Value> {
    let mut db = open(project)?;
    let reason = note(reason)?;
    let scope = scope.map(scope_key).transpose()?;
    let placed = now();
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // The scope is kept as a salted digest, like a tombstone key: a hold on a
    // session never leaves its id behind once the session is deleted.
    let (_, salt) = salt(&tx)?;
    let digest = scope.as_deref().map(|s| key_digest(&salt, HOLD_SCOPE, s));
    let n: i64 = tx.query_row("SELECT count(*) FROM holds", [], |r| r.get(0))?;
    let id = format!("hold-{}", n + 1);
    tx.execute("INSERT INTO holds(hold_id,class,scope,reason,principal,placed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![id, class, digest, reason, PRINCIPAL, placed])?;
    tx.commit()?;
    Ok(json!({"hold_id": id, "class": class, "scope": scope, "scope_digest": digest, "reason": reason, "principal": PRINCIPAL, "placed_unix_ms": placed}))
}

pub fn release_hold(project: &Path, id: &str, reason: &str) -> Result<Value> {
    let db = open(project)?;
    let reason = note(reason)?;
    let at = now();
    let changed = db.execute("UPDATE holds SET released_unix_ms=?2,release_reason=?3 WHERE hold_id=?1 AND released_unix_ms IS NULL", params![id, at, reason])?;
    ensure!(changed == 1, "no active hold {id}");
    Ok(json!({"hold_id": id, "released_unix_ms": at, "release_reason": reason}))
}

/// Backup inventory rows not yet deleted: `(backup_id, location, manifest digest, created)`.
pub fn backups(db: &Connection) -> Result<Vec<(String, String, String, i64)>> {
    Ok(db.prepare("SELECT backup_id,location,manifest_digest,created_unix_ms FROM backups WHERE deleted_unix_ms IS NULL ORDER BY created_unix_ms,backup_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Classes named by tombstones (for the inventory), with counts.
pub fn tombstone_counts(db: &Connection) -> Result<BTreeMap<String, i64>> {
    Ok(db.prepare("SELECT class,count(*) FROM tombstones GROUP BY class")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Tombstones older than `horizon` (400 days by default): listed, never pruned in `retention.v1`.
pub fn expired_tombstones(db: &Connection, cutoff: i64) -> Result<i64> {
    Ok(db.query_row("SELECT count(*) FROM tombstones WHERE recorded_unix_ms < ?1", [cutoff], |r| r.get(0))?)
}
