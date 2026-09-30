//! TM5.3 sidecar backup and restore (plan doc 09 "Backup and restore
//! protocol"; docs/telemetry/operations-runbook.md). `backup create` copies
//! `telemetry.db` and the operations store with SQLite's online backup API
//! (a consistent snapshot, committed WAL frames included), checks each copy,
//! and writes a digest manifest; `--encrypt-to` encrypts every file with the
//! operator's `age` (no plaintext left in the backup directory; refused when
//! `age` is not installed). `backup restore` is offline (the project's
//! maintenance barrier: the ticker is stopped), verifies every digest, checks
//! integrity and stream versions, refuses to replace a newer sidecar without
//! `--force`, merges and reapplies every tombstone before the copy is exposed,
//! and writes only the sidecar and the operations store: canonical state is
//! read, never written, so a restore cannot re-accept budget or canonical
//! mutations. Canonical `state.db` backups are the canonical store's own
//! procedure (runbook "Canonical store").
use super::store;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `(hold_id, class, scope digest, reason, placed, released, release reason)`.
type HoldRow = (String, String, Option<String>, String, i64, Option<i64>, Option<String>);

pub const MANIFEST: &str = "manifest.json";
pub const SCHEMA: &str = "telemetry-backup.v1";
const SIDECAR: &str = "telemetry.db";

/// `herdr-projects telemetry <slug> backup ...`
#[derive(clap::Subcommand, Clone, Debug)]
pub enum Command {
    /// Back up telemetry.db and the operations store into a new directory OUT, with a digest manifest.
    Create {
        #[arg(long)]
        out: PathBuf,
        /// Encrypt every file to this age recipient with the installed `age` (refused when absent).
        #[arg(long = "encrypt-to")]
        encrypt_to: Option<String>,
    },
    /// Restore a backup into this project (offline: stop the ticker first). Sidecar and operations store only.
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// Replace a sidecar holding data newer than the backup.
        #[arg(long)]
        force: bool,
        /// The age identity file that decrypts an encrypted backup.
        #[arg(long)]
        identity: Option<PathBuf>,
    },
    /// Verify a backup directory's manifest and file digests without restoring. Read-only.
    Verify { #[arg(long)] from: PathBuf },
    /// The backup inventory of this project. Read-only.
    List,
}

pub fn run(project: &Path, command: Command) -> Result<String> {
    let value = match command {
        Command::Create { out, encrypt_to } => create(project, &out, encrypt_to.as_deref())?,
        Command::Restore { from, force, identity } => restore(project, &from, force, identity.as_deref())?,
        Command::Verify { from } => { let (manifest, _) = verify(project, &from)?; json!({"verified": true, "backup_id": manifest["backup_id"], "files": manifest["files"]}) }
        Command::List => {
            let rows = match store::read(project)? {
                Some(db) => db.prepare("SELECT backup_id,location,created_unix_ms,encrypted,deleted_unix_ms FROM backups ORDER BY created_unix_ms,backup_id")?
                    .query_map([], |r| Ok(json!({"backup_id": r.get::<_, String>(0)?, "location": r.get::<_, String>(1)?, "created_unix_ms": r.get::<_, i64>(2)?,
                        "encrypted": r.get::<_, bool>(3)?, "deleted_unix_ms": r.get::<_, Option<i64>>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?,
                None => Vec::new(),
            };
            json!({"backups": rows})
        }
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

/// A plain file name from a manifest (no directory, no hidden file).
pub fn safe_name(name: &str) -> Result<&str> {
    ensure!(!name.is_empty() && !name.starts_with('.') && !name.contains('/') && name.len() <= 64, "invalid file name in backup manifest");
    Ok(name)
}

fn file_digest(path: &Path) -> Result<(u64, String)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok((bytes.len() as u64, store::sha256(&bytes)))
}

/// A new owner-only directory for scratch copies under the project's `.state`.
fn scratch(project: &Path, what: &str) -> Result<PathBuf> {
    let dir = project.join(".state").join(format!(".{what}-{}-{}", std::process::id(), jiff::Timestamp::now().as_nanosecond()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

struct Scratch(PathBuf);
impl Drop for Scratch { fn drop(&mut self) { let _ = super::remove_tree(&self.0); } }

/// Copy `src` into a new file `dest` (mode 0600) with the online backup API,
/// as a self-contained rollback-journal database, and check its integrity.
fn snapshot(src: &Connection, dest: &Path) -> Result<()> {
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(dest)
        .with_context(|| format!("create {}", dest.display()))?;
    let mut db = Connection::open_with_flags(dest, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    rusqlite::backup::Backup::new(src, &mut db)?.run_to_completion(256, std::time::Duration::ZERO, None)?;
    db.pragma_update(None, "journal_mode", "DELETE")?;
    let check: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(check == "ok", "the copy of {} failed its integrity check", dest.display());
    Ok(())
}

/// The latest time any sidecar row records (every `*_unix_ms` column): what "newer" compares.
fn watermark(db: &Connection) -> Result<i64> {
    let columns: Vec<(String, String)> = db.prepare("SELECT m.name,p.name FROM sqlite_master m, pragma_table_info(m.name) p WHERE m.type='table' AND p.name LIKE '%\\_unix\\_ms' ESCAPE '\\'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = 0i64;
    for (table, column) in columns {
        let max: Option<i64> = db.query_row(&format!("SELECT max(\"{column}\") FROM \"{table}\" WHERE typeof(\"{column}\")='integer'"), [], |r| r.get(0))?;
        out = out.max(max.unwrap_or(0));
    }
    Ok(out)
}

/// Row counts of the tables behind each retention class and the non-derivable tables.
fn counts(db: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut out = BTreeMap::new();
    for table in ["rollout_sources", "codex_usage", "source_observations", "attention_samples", "health_evaluations", "analytics_revisions", "analytics_workspace_metrics", "analytics_workspace_comparisons", "rate_cards",
        "provider_charges", "fx_tables", "valuation_revisions", "tombstones", "holds"] {
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?;
        if exists { out.insert(table.to_owned(), db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?); }
    }
    Ok(out)
}

/// The executable `age` on PATH, if installed.
fn age() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("age")).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
    }))
}

fn run_age(tool: &Path, args: &[&std::ffi::OsStr]) -> Result<()> {
    use crate::execution_guard::GatedSpawn;
    let out = std::process::Command::new(tool).args(args).env_remove("AGE_PASSPHRASE").output_gated().context("run age")?;
    // Its stderr may name files; never echo it.
    ensure!(out.status.success(), "age failed ({}); nothing was kept", out.status);
    Ok(())
}

pub fn create(project: &Path, out: &Path, recipient: Option<&str>) -> Result<Value> {
    super::super::review::refuse_owner_cli_in_worker_context(project, "`backup create`")?;
    let tool = match recipient {
        Some(r) => {
            ensure!(!r.is_empty() && r.len() <= 512 && !r.contains(char::is_whitespace), "invalid age recipient");
            Some(age().context("`age` is not installed: nothing was written. Omit --encrypt-to and keep backups on encrypted storage (runbook \"Encryption\")")?)
        }
        None => None,
    };
    ensure!(super::super::sidecar::path(project).is_file(), "no telemetry.db to back up (run `telemetry <slug> collect` first)");
    match std::fs::symlink_metadata(out) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new().mode(0o700).create(out).with_context(|| format!("create {}", out.display()))?,
        Err(error) => return Err(error.into()),
        Ok(meta) => ensure!(meta.is_dir() && std::fs::read_dir(out)?.next().is_none() && meta.uid() == unsafe { libc::geteuid() },
            "{} exists and is not an empty directory of this user", out.display()),
    }
    let out = std::fs::canonicalize(out)?;
    // Shared, as a collect: no maintenance apply or restore runs mid-copy, so
    // the sidecar and the tombstones are one consistent state.
    let _lock = super::lock(project, false)?;
    let work = Scratch(scratch(project, "backup")?);
    let plain = |name: &str| if tool.is_some() { work.0.join(name) } else { out.join(name) };
    let sidecar = super::super::sidecar::read(project)?.context("no telemetry.db to back up")?;
    snapshot(&sidecar, &plain(SIDECAR))?;
    drop(sidecar);
    let mut names = vec![SIDECAR];
    if let Some(ops) = store::read(project)? { snapshot(&ops, &plain(store::FILE))?; names.push(store::FILE); }
    let copy = Connection::open_with_flags(plain(SIDECAR), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let streams = super::super::sidecar::stream_versions(&copy)?;
    let (mark, mut rows) = (watermark(&copy)?, counts(&copy)?);
    drop(copy);
    if names.contains(&store::FILE) {
        let ops = Connection::open_with_flags(plain(store::FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        rows.extend(counts(&ops)?);
    }
    let mut files = Vec::new();
    for name in &names {
        let (bytes, digest) = file_digest(&plain(name))?;
        match &tool {
            None => files.push(json!({"name": name, "bytes": bytes, "sha256": digest})),
            Some(tool) => {
                let sealed = format!("{name}.age");
                run_age(tool, &["-r".as_ref(), recipient.unwrap_or_default().as_ref(), "-o".as_ref(), out.join(&sealed).as_os_str(), plain(name).as_os_str()])?;
                let (sealed_bytes, sealed_digest) = file_digest(&out.join(&sealed))?;
                files.push(json!({"name": sealed, "bytes": sealed_bytes, "sha256": sealed_digest, "plaintext": {"name": name, "bytes": bytes, "sha256": digest}}));
            }
        }
    }
    let created = store::now();
    let mut manifest = json!({"schema": SCHEMA, "project": super::super::health::store::slug(project), "created_unix_ms": created, "streams": streams,
        "watermark_unix_ms": mark, "rows": rows, "files": files,
        "encryption": recipient.map(|r| json!({"tool": "age", "recipient": r})),
        "excluded": {"state.db": "canonical store: its own backup procedure (runbook \"Canonical store\")", "telemetry-cursor.key": "secret: stored separately, never in a telemetry backup",
            "codex rollouts": "native sources owned by Codex", "spool, git quarantine, replay repositories": "canonical workflow artefacts, not telemetry"}});
    let id = store::sha256(manifest.to_string().as_bytes());
    manifest["backup_id"] = json!(id);
    let text = serde_json::to_string_pretty(&manifest)? + "\n";
    super::super::export::external::write_new(&out.join(MANIFEST), text.as_bytes())?;
    let digest = store::sha256(text.as_bytes());
    store::open(project)?.execute("INSERT INTO backups(backup_id,location,manifest_digest,created_unix_ms,encrypted) VALUES(?1,?2,?3,?4,?5)",
        params![id, out.display().to_string(), digest, created, tool.is_some()])?;
    Ok(json!({"backup_id": id, "location": out, "manifest_digest": digest, "files": manifest["files"], "encrypted": tool.is_some(), "watermark_unix_ms": mark,
        "streams": manifest["streams"], "canonical_store": "not included: back up state.db with the canonical procedure"}))
}

/// Read and check a backup directory: its manifest names this project and every file matches its digest.
fn verify(project: &Path, from: &Path) -> Result<(Value, String)> {
    let bytes = crate::migration::read_plan_file(&from.join(MANIFEST)).context("read the backup manifest")?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid backup manifest"))?;
    ensure!(manifest["schema"] == SCHEMA, "unsupported backup manifest (want {SCHEMA})");
    let slug = super::super::health::store::slug(project);
    ensure!(manifest["project"] == slug, "the backup is of project {}, not {slug}", manifest["project"]);
    let mut without = manifest.clone();
    without.as_object_mut().context("invalid backup manifest")?.remove("backup_id");
    ensure!(manifest["backup_id"].as_str() == Some(&store::sha256(without.to_string().as_bytes())), "the backup manifest does not match its backup_id");
    for file in manifest["files"].as_array().context("invalid backup manifest")? {
        let name = safe_name(file["name"].as_str().context("invalid backup manifest")?)?;
        let (bytes, digest) = file_digest(&from.join(name))?;
        ensure!(Some(bytes) == file["bytes"].as_u64() && Some(digest.as_str()) == file["sha256"].as_str(), "{name} does not match the backup manifest");
    }
    Ok((manifest, store::sha256(&bytes)))
}

pub fn restore(project: &Path, from: &Path, force: bool, identity: Option<&Path>) -> Result<Value> {
    super::super::review::refuse_owner_cli_in_worker_context(project, "`backup restore`")?;
    // Offline: the maintenance barrier refuses while the ticker or any effect holds the project.
    let _barrier = crate::migration::maintenance(project).context("restore is offline: stop the ticker and retry")?;
    let _lock = super::lock(project, true)?;
    let (manifest, _) = verify(project, from)?;
    let work = Scratch(scratch(project, "restore")?);
    let mut plain = BTreeMap::new();
    for file in manifest["files"].as_array().into_iter().flatten() {
        let name = safe_name(file["name"].as_str().unwrap_or_default())?;
        match file.get("plaintext") {
            None => { std::fs::copy(from.join(name), work.0.join(name))?; plain.insert(name.to_owned(), work.0.join(name)); }
            Some(inner) => {
                let tool = age().context("the backup is encrypted and `age` is not installed")?;
                let identity = identity.context("the backup is encrypted: pass --identity <age identity file>")?;
                let target = safe_name(inner["name"].as_str().unwrap_or_default())?;
                run_age(&tool, &["-d".as_ref(), "-i".as_ref(), identity.as_os_str(), "-o".as_ref(), work.0.join(target).as_os_str(), from.join(name).as_os_str()])?;
                let (bytes, digest) = file_digest(&work.0.join(target))?;
                ensure!(Some(bytes) == inner["bytes"].as_u64() && Some(digest.as_str()) == inner["sha256"].as_str(), "decrypted {target} does not match the backup manifest");
                plain.insert(target.to_owned(), work.0.join(target));
            }
        }
    }
    let copy_path = plain.get(SIDECAR).context("the backup holds no telemetry.db")?.clone();
    let mut copy = Connection::open_with_flags(&copy_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    let check: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(check == "ok", "the backup's telemetry.db failed its integrity check");
    super::super::sidecar::migrate(&mut copy)?;
    let streams = super::super::sidecar::stream_versions(&copy)?;
    let backup_mark = watermark(&copy)?;
    // Newer: the live sidecar records something later than anything in the backup.
    let live = super::super::sidecar::path(project);
    let mut replaced = Value::Null;
    if live.is_file() {
        let current = super::super::sidecar::read(project)?.context("the live sidecar is unreadable")?;
        let current_mark = watermark(&current)?;
        if current_mark > backup_mark {
            let later = |table: &str, column: &str| -> Result<i64> {
                let exists: bool = current.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [table], |r| r.get(0))?;
                Ok(if exists { current.query_row(&format!("SELECT count(*) FROM {table} WHERE {column}>?1"), [backup_mark], |r| r.get(0))? } else { 0 })
            };
            let lost = json!({"attention_samples": later("attention_samples", "observed_unix_ms")?, "rate_cards": later("rate_cards", "imported_unix_ms")?,
                "provider_charges": later("provider_charges", "imported_unix_ms")?, "fx_tables": later("fx_tables", "imported_unix_ms")?,
                "valuation_revisions": later("valuation_revisions", "computed_unix_ms")?});
            if !force {
                bail!("the live sidecar is newer than the backup (watermark {current_mark} > {backup_mark}); not derivable from sources and lost on restore: {lost}. Pass --force to replace it");
            }
            replaced = json!({"live_watermark_unix_ms": current_mark, "not_recoverable": lost});
        }
    }
    // Tombstones: the live store's and the backup's, before anything is exposed.
    let ops = store::open(project)?;
    let mut merged = 0;
    if let Some(path) = plain.get(store::FILE) {
        let backup_ops = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        merged = store::merge_from(&ops, &backup_ops)?;
        let holds: Vec<HoldRow> = backup_ops.prepare(
            "SELECT hold_id,class,scope,reason,placed_unix_ms,released_unix_ms,release_reason FROM holds")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
        for (id, class, scope, reason, placed, released, release_reason) in holds {
            ops.execute("INSERT OR IGNORE INTO holds(hold_id,class,scope,reason,principal,placed_unix_ms,released_unix_ms,release_reason) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id, class, scope, reason, store::PRINCIPAL, placed, released, release_reason])?;
        }
    }
    let tombstones = super::Tombstones::load(&ops)?;
    let reapplied = super::enforce(&mut copy, &tombstones)?;
    super::super::accounting::ledger::invalidate(&copy, "sidecar_restore")?;
    let rows = counts(&copy)?;
    let orphans = orphans(project, &copy)?;
    drop(copy);
    // Expose: the online backup API writes the checked copy into the live
    // database in one step (WAL and readers handled by SQLite).
    if !live.is_file() {
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&live).context("create telemetry.db")?;
    }
    {
        let source = Connection::open_with_flags(&copy_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut target = Connection::open_with_flags(&live, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        target.busy_timeout(std::time::Duration::from_secs(5))?;
        rusqlite::backup::Backup::new(&source, &mut target)?.run_to_completion(256, std::time::Duration::ZERO, None)?;
        target.pragma_update(None, "journal_mode", "WAL")?;
    }
    if let Some(db) = super::super::sidecar::open(project, false)? {
        // Backups predating the frontier acquire it during this upgrade too.
        super::super::accounting::ledger::invalidate(&db, "sidecar_restore")?;
    }
    let restored = store::now();
    let backup_id = manifest["backup_id"].as_str().unwrap_or_default().to_owned();
    let restore_id = store::sha256(format!("{backup_id}:{restored}").as_bytes());
    let incarnation = store::sha256(format!("incarnation:{restore_id}").as_bytes());
    let report = json!({"restore_id": restore_id, "backup_id": backup_id, "incarnation": incarnation, "restored_unix_ms": restored,
        "backup_created_unix_ms": manifest["created_unix_ms"], "watermark_unix_ms": backup_mark, "streams": streams, "verified_files": manifest["files"].as_array().map_or(0, Vec::len),
        "tombstones": {"merged_from_backup": merged, "total": tombstones.count, "reapplied": reapplied}, "rows": rows, "orphans": orphans,
        "forced": force, "replaced_newer": replaced, "canonical_written": false,
        "budget": "restore is analytics-only: no canonical budget, usage or quality acceptance is written or re-fed from it",
        "next": ["telemetry <slug> collect (refreshes bindings from canonical state; restored native identities dedupe)", "telemetry <slug> accounting sync",
            "telemetry <slug> analytics refresh"]});
    ops.execute("INSERT INTO restores(restore_id,backup_id,incarnation,restored_unix_ms,forced,report) VALUES(?1,?2,?3,?4,?5,?6)",
        params![restore_id, backup_id, incarnation, restored, force, report.to_string()])?;
    Ok(report)
}

/// Bound attempts of the restored copy that the canonical store does not know.
fn orphans(project: &Path, copy: &Connection) -> Result<i64> {
    let attempts: Vec<String> = copy.prepare("SELECT DISTINCT attempt_id FROM rollout_sources WHERE attempt_id IS NOT NULL")?.query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let path = project.join(".state/state.db");
    if attempts.is_empty() || !path.is_file() { return Ok(0); }
    let db = super::super::read_only(&path)?;
    let mut n = 0;
    for attempt in attempts {
        let known: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [&attempt], |r| r.get(0))?;
        if !known { n += 1; }
    }
    Ok(n)
}
