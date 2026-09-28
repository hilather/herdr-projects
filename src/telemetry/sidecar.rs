//! `<project>/.state/telemetry.db`: analytics only, never read to grant launch.
//! Own migration sequence under `migrations/telemetry/`; no cross-database
//! transaction with the canonical store (contracts §0).
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MIGRATIONS: &[&str] = &[include_str!("../../migrations/telemetry/0001_codex_usage.sql")];

pub fn path(project: &Path) -> PathBuf {
    project.join(".state").join("telemetry.db")
}

/// Open and migrate the sidecar. Absent and `create` false → `None`. Created mode 0600.
pub fn open(project: &Path, create: bool) -> Result<Option<Connection>> {
    let path = path(project);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() => bail!("telemetry sidecar is not a regular file"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(None);
            }
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(&path).context("create telemetry sidecar")?;
        }
        Err(error) => return Err(error.into()),
    }
    let mut db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    let version: usize = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > MIGRATIONS.len() {
        bail!("telemetry sidecar schema {version} is newer than this binary");
    }
    for migration in &MIGRATIONS[version..] {
        let tx = db.transaction()?;
        tx.execute_batch(migration)?;
        tx.commit()?;
    }
    Ok(Some(db))
}

/// The sidecar opened strictly read-only (`super::read_only`); absent → `None`.
pub(crate) fn read(project: &Path) -> Result<Option<super::ReadOnly>> {
    let path = path(project);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(meta) if !meta.is_file() => bail!("telemetry sidecar is not a regular file"),
        Ok(_) => {}
    }
    let db = super::read_only(&path)?;
    let version: usize = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 || version > MIGRATIONS.len() {
        bail!("telemetry sidecar schema {version} is not readable by this binary");
    }
    Ok(Some(db))
}

/// Per-attempt usage (contracts §4 `usage`) and per-rollout metadata. Read-only.
pub fn report(project: &Path) -> Result<Value> {
    let attempts = super::codex::canonical_attempts(project)?;
    let Some(db) = read(project)? else {
        let attempts = attempts.iter().map(|a| json!({"attempt_id": a.id, "usage": unavailable(if a.codex() { "collection_not_run" } else { "adapter_absent" })}));
        return Ok(json!({"attempts": attempts.collect::<Vec<_>>(), "sessions": []}));
    };
    let mut sessions = Vec::new();
    let mut stmt = db.prepare("SELECT s.session_id,s.binding,s.attempt_id,s.cli_version,s.records,s.cwd,s.originator,s.source,
        (SELECT count(*) FROM codex_usage u WHERE u.session_id=s.session_id AND u.accepted=1),
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id)
        FROM rollout_sources s ORDER BY s.session_id,s.path_digest")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?,
        r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, i64>(8)?, r.get::<_, bool>(9)?)))?;
    for row in rows {
        let (session, binding, attempt, version, records, cwd, originator, source, accepted, quarantined) = row?;
        sessions.push(json!({"session_id": session, "binding": binding, "attempt_id": attempt, "cli_version": version,
            "certified": super::codex::certified(&version), "records": records, "accepted": accepted, "quarantined": quarantined,
            "cwd": cwd, "originator": originator, "source": source}));
    }
    let mut out = Vec::new();
    for attempt in &attempts {
        let usage = if attempt.codex() { attempt_usage(&db, &attempt.id)? } else { unavailable("adapter_absent") };
        out.push(json!({"attempt_id": attempt.id, "usage": usage}));
    }
    Ok(json!({"attempts": out, "sessions": sessions}))
}

pub(super) fn attempt_usage(db: &Connection, attempt: &str) -> Result<Value> {
    // Records collected before their version was certified keep NULL counters,
    // so the session stays uncertified rather than summing to 0.
    let bound: Vec<(String, String, bool)> = db.prepare("SELECT DISTINCT session_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.session_id=s.session_id AND u.reason='cli_version_uncertified') THEN '' ELSE cli_version END,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id)
        FROM rollout_sources s WHERE binding='bound' AND attempt_id=?1")?
        .query_map([attempt], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    if bound.is_empty() {
        return Ok(unavailable("not_bound"));
    }
    if bound.iter().any(|s| s.2) {
        return Ok(unavailable("quarantined"));
    }
    if bound.iter().any(|s| !super::codex::certified(&s.1)) {
        return Ok(unavailable("cli_version_uncertified"));
    }
    let mut sums = [0i64; 6];
    let mut records = 0;
    for (session, ..) in &bound {
        let row: Option<[i64; 7]> = db.query_row("SELECT count(*),sum(input_tokens),sum(cached_input_tokens),sum(cache_write_input_tokens),
            sum(output_tokens),sum(reasoning_output_tokens),sum(total_tokens) FROM codex_usage WHERE session_id=?1 AND accepted=1", [session],
            |r| Ok([r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, Option<i64>>(2)?.unwrap_or(0), r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0), r.get::<_, Option<i64>>(5)?.unwrap_or(0), r.get::<_, Option<i64>>(6)?.unwrap_or(0)])).optional()?;
        if let Some(row) = row {
            records += row[0];
            for (sum, value) in sums.iter_mut().zip(&row[1..]) { *sum += value; }
        }
    }
    Ok(json!({"input_tokens": sums[0], "cached_input_tokens": sums[1], "cache_write_input_tokens": sums[2],
        "output_tokens": sums[3], "reasoning_output_tokens": sums[4], "total_tokens": sums[5], "records": records}))
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}
