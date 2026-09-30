//! `<project>/.state/telemetry.db`: analytics only, never read to grant launch.
//! Versioned per stream in `telemetry_streams`: `codex` (the migrations under
//! `migrations/telemetry/`, whose history is also `user_version`) and one
//! stream per lane (`super::LANES`, `migrations/telemetry/<stream>/`). No
//! cross-database transaction with the canonical store (contracts §0).
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const STREAMS_TABLE: &str = "CREATE TABLE IF NOT EXISTS telemetry_streams (stream TEXT PRIMARY KEY, version INTEGER NOT NULL CHECK (version >= 0)) STRICT;";
const CODEX: &[&str] = &[include_str!("../../migrations/telemetry/0001_codex_usage.sql"), include_str!("../../migrations/telemetry/0002_reevaluation.sql"),
    include_str!("../../migrations/telemetry/0003_read_indexes.sql")];

/// Every stream and its migrations; index + 1 is the stream version.
fn streams() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    std::iter::once(("codex", CODEX)).chain(super::LANES.iter().map(|lane| (lane.stream, lane.migrations)))
}

/// Stored stream versions. A sidecar from before streams has only `codex` = `user_version`.
fn versions(db: &Connection) -> Result<BTreeMap<String, usize>> {
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='telemetry_streams')", [], |r| r.get(0))? {
        return Ok(BTreeMap::from([("codex".to_owned(), db.query_row("PRAGMA user_version", [], |r| r.get(0))?)]));
    }
    Ok(db.prepare("SELECT stream,version FROM telemetry_streams")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Refuse only a stream stored newer than this binary knows (an unknown stream knows 0).
fn check(versions: &BTreeMap<String, usize>) -> Result<()> {
    for (stream, &version) in versions {
        let known = streams().find(|s| s.0 == stream).map_or(0, |s| s.1.len());
        if version > known { bail!("telemetry sidecar stream {stream} version {version} is newer than this binary (knows {known})"); }
    }
    Ok(())
}

/// Stored stream versions, refused when any is newer than this binary knows
/// (TM5.3 backup and restore check a copy with it before exposing it).
pub(crate) fn stream_versions(db: &Connection) -> Result<BTreeMap<String, usize>> {
    let versions = versions(db)?;
    check(&versions)?;
    Ok(versions)
}

/// Whether a sidecar read without migrating has the 0002 `reevaluation` column.
fn reevaluation_column(db: &Connection) -> rusqlite::Result<&'static str> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('rollout_sources') WHERE name='reevaluation')", [], |r| r.get(0))?;
    Ok(if present { "s.reevaluation" } else { "NULL" })
}

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
    migrate(&mut db)?;
    Ok(Some(db))
}

/// Upgrade a writable sidecar, including a private backup copy before retention.
pub(crate) fn migrate(db: &mut Connection) -> Result<()> {
    // One transaction: a pre-streams sidecar gains `telemetry_streams` with
    // `codex` = `user_version`, then each stream migrates from its version.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let versions = versions(&tx)?;
    check(&versions)?;
    tx.execute_batch(STREAMS_TABLE)?;
    let mut upgraded = false;
    for (stream, migrations) in streams() {
        let from = versions.get(stream).copied().unwrap_or(0);
        if from > 0 { tx.execute("INSERT OR IGNORE INTO telemetry_streams(stream,version) VALUES(?1,?2)", rusqlite::params![stream, from])?; }
        for (index, migration) in migrations.iter().enumerate().skip(from) {
            upgraded = true;
            tx.execute_batch(migration)?;
            tx.execute("INSERT INTO telemetry_streams(stream,version) VALUES(?1,?2) ON CONFLICT(stream) DO UPDATE SET version=excluded.version",
                rusqlite::params![stream, index + 1])?;
        }
    }
    if upgraded { super::accounting::ledger::invalidate(&tx, "schema_upgrade")?; }
    tx.commit()?;
    Ok(())
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
    let versions = versions(&db)?;
    check(&versions)?;
    if versions.get("codex").is_none_or(|&version| version == 0) {
        bail!("telemetry sidecar schema 0 is not readable by this binary");
    }
    Ok(Some(db))
}

/// `telemetry <slug> <lane> status`: the stream's stored version. Read-only.
pub fn status(project: &Path, stream: &str) -> Result<Value> {
    let version = match read(project)? {
        None => unavailable("collection_not_run"),
        Some(db) => json!(versions(&db)?.get(stream).copied().unwrap_or(0)),
    };
    Ok(json!({"stream": stream, "version": version}))
}

/// Per-attempt usage (contracts §4 `usage`) and per-rollout metadata. Read-only.
pub fn report(project: &Path) -> Result<Value> {
    let attempts = super::codex::canonical_attempts(project)?;
    let Some(db) = read(project)? else {
        let attempts = attempts.iter().map(|a| json!({"attempt_id": a.id, "after_termination": if a.terminated_unix_ms().is_some() { unavailable("collection_not_run") } else { Value::Null }, "usage": unavailable(if a.codex() { "collection_not_run" } else { "adapter_absent" })}));
        return Ok(json!({"attempts": attempts.collect::<Vec<_>>(), "sessions": []}));
    };
    let mut sessions = Vec::new();
    let mut stmt = db.prepare(&format!("SELECT s.session_id,s.binding,s.attempt_id,s.cli_version,s.records,s.cwd,s.originator,s.source,
        (SELECT count(*) FROM codex_usage u WHERE u.session_id=s.session_id AND u.accepted=1),
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),{}
        FROM rollout_sources s ORDER BY s.session_id,s.path_digest", reevaluation_column(&db)?))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?,
        r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, i64>(8)?, r.get::<_, bool>(9)?,
        r.get::<_, Option<String>>(10)?)))?;
    for row in rows {
        let (session, binding, attempt, version, records, cwd, originator, source, accepted, quarantined, reevaluation) = row?;
        sessions.push(json!({"session_id": session, "binding": binding, "attempt_id": attempt, "cli_version": version,
            "certified": super::codex::certified(&version), "records": records, "accepted": accepted, "quarantined": quarantined,
            "cwd": cwd, "originator": originator, "source": source, "reevaluation": reevaluation}));
    }
    let mut out = Vec::new();
    for attempt in &attempts {
        let usage = if attempt.codex() { attempt_usage(&db, &attempt.id)? } else { unavailable("adapter_absent") };
        let after_termination = after_termination(&db, &attempt.id, attempt.terminated_unix_ms())?;
        out.push(json!({"attempt_id": attempt.id, "usage": usage, "after_termination": after_termination}));
    }
    Ok(json!({"attempts": out, "sessions": sessions}))
}

/// `report` for the terminal: one line per attempt, then one per rollout.
pub fn text(report: &Value) -> String {
    let word = |v: &Value| v.as_str().map_or_else(|| if v.is_null() { "-".to_owned() } else { v.to_string() }, str::to_owned);
    let mut out = String::new();
    for attempt in report["attempts"].as_array().into_iter().flatten() {
        let usage = &attempt["usage"];
        let shown = if usage.get("total_tokens").is_some() {
            format!("in={} out={} total={} records={}", usage["input_tokens"], usage["output_tokens"], usage["total_tokens"], usage["records"])
        } else {
            format!("{}:{}", word(&usage["status"]), word(&usage["reason"]))
        };
        out += &format!("{} usage={shown} after_termination={}\n", word(&attempt["attempt_id"]), attempt["after_termination"]);
    }
    for s in report["sessions"].as_array().into_iter().flatten() {
        out += &format!("session {} binding={} attempt={} cli={} certified={} records={} accepted={} quarantined={}", word(&s["session_id"]), word(&s["binding"]),
            word(&s["attempt_id"]), word(&s["cli_version"]), s["certified"], s["records"], s["accepted"], s["quarantined"]);
        if !s["reevaluation"].is_null() { out += &format!(" reevaluation={}", word(&s["reevaluation"])); }
        out += "\n";
    }
    out
}

/// Diagnostic only: every usage record on a bound rollout remains in M08.
pub(super) fn after_termination(db: &Connection, attempt: &str, terminated: Option<i64>) -> Result<Value> {
    let Some(at) = terminated else { return Ok(Value::Null) };
    let timed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_usage_times')", [], |r| r.get(0))?;
    if !timed { return Ok(unavailable("predates_collection")); }
    let (records, first): (i64, Option<i64>) = db.query_row("SELECT count(*),min(t.record_unix_ms) FROM codex_usage u JOIN codex_usage_times t USING(session_id,ordinal)
        WHERE t.record_unix_ms>?2 AND EXISTS(SELECT 1 FROM rollout_sources s WHERE s.path_digest=u.path_digest AND s.binding='bound' AND s.attempt_id=?1)",
        rusqlite::params![attempt, at], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(if records == 0 { Value::Null } else { json!({"records": records, "first_unix_ms": first, "terminated_unix_ms": at}) })
}

pub(super) fn attempt_usage(db: &Connection, attempt: &str) -> Result<Value> {
    // Records collected before their version was certified keep NULL counters
    // until a collect re-reads their rollout, so the session stays uncertified
    // rather than summing to 0; `detail` says when that rollout is gone.
    let bound: Vec<(String, String, bool, Option<String>)> = db.prepare_cached(&format!("SELECT DISTINCT session_id,
        CASE WHEN EXISTS(SELECT 1 FROM codex_usage u WHERE u.session_id=s.session_id AND u.reason='cli_version_uncertified') THEN '' ELSE cli_version END,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=s.session_id),{}
        FROM rollout_sources s WHERE binding='bound' AND attempt_id=?1", reevaluation_column(db)?))?
        .query_map([attempt], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    if bound.is_empty() {
        return Ok(unavailable("not_bound"));
    }
    if bound.iter().any(|s| s.2) {
        return Ok(unavailable("quarantined"));
    }
    if bound.iter().any(|s| !super::codex::certified(&s.1)) {
        let mut usage = unavailable("cli_version_uncertified");
        if let Some(detail) = bound.iter().filter(|s| !super::codex::certified(&s.1)).find_map(|s| s.3.clone()) { usage["detail"] = json!(detail); }
        return Ok(usage);
    }
    // A record that failed validation keeps no counters: summing the rest
    // would present an unknown total as known (contracts §0).
    let rejected: bool = db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM codex_usage WHERE accepted=0 AND session_id IN ({}))",
        vec!["?"; bound.len()].join(",")), rusqlite::params_from_iter(bound.iter().map(|s| &s.0)), |r| r.get(0))?;
    if rejected {
        return Ok(unavailable("records_not_accepted"));
    }
    let mut sums = [0i64; 6];
    let mut records = 0;
    for (session, ..) in &bound {
        let row: Option<[i64; 7]> = db.prepare_cached("SELECT count(*),sum(input_tokens),sum(cached_input_tokens),sum(cache_write_input_tokens),
            sum(output_tokens),sum(reasoning_output_tokens),sum(total_tokens) FROM codex_usage WHERE session_id=?1 AND accepted=1 AND NOT EXISTS(SELECT 1 FROM codex_usage e WHERE e.session_id=codex_usage.session_id AND e.accepted=1 AND e.response_id IS NOT NULL AND e.response_id=codex_usage.response_id AND e.payload_digest=codex_usage.payload_digest AND e.ordinal<codex_usage.ordinal)")?.query_row([session],
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
