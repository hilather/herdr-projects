//! Codex rollout adapter (contracts §5): bounded tail reads of
//! `<execution_home>/.codex/sessions/**/rollout-*.jsonl`, allowlisted typed
//! fields only, idempotent by `(session_id, ordinal)`, bound by cwd and time.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Number, Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use super::{ingest, sanitize};

/// Versions whose counters are accepted: each certified by a live run
/// (`0.154.0`: docs/telemetry/codex-live-0.154.0.md).
pub const CERTIFIED: &[&str] = &["0.154.0"];
/// Longest line parsed; longer lines are skipped whole without being retained.
const MAX_LINE: u64 = 16 << 20;
const MAX_DEPTH: usize = 4;
const MAX_SAFE: u64 = 1 << 53;

pub fn certified(version: &str) -> bool {
    CERTIFIED.contains(&version)
}

/// Bytes one collect may read across all rollouts; the rest waits for the next collect.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub bytes: u64,
}

impl Budget {
    pub const CLI: Self = Self { bytes: 256 << 20 };
    pub const TICK: Self = Self { bytes: 8 << 20 };
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Collected {
    pub files: u64,
    pub bytes: u64,
    pub records: u64,
    /// Records stored while their version was uncertified, accepted on re-read.
    pub reevaluated: u64,
    pub budget_exhausted: bool,
}

/// A canonical attempt with retained inputs, read by value from `state.db`.
pub struct CanonicalAttempt {
    pub id: String,
    kind: Option<String>,
    home: Option<String>,
    decided_unix_ms: Option<i64>,
    binding: Binding,
}

/// The attempt's latest canonical collector binding revision (migration 0052).
enum Binding {
    /// Launched after 0052 without a binding, or never launched.
    None,
    /// Existed before 0052 (or the store predates it): contracts §5 rules 1-4.
    Predates,
    /// `execution_home` recorded at launch.
    Active(Option<String>),
    /// `execution_home` and the revocation time.
    Revoked(Option<String>, i64),
}

impl CanonicalAttempt {
    pub fn codex(&self) -> bool {
        self.kind.as_deref() == Some("codex")
    }
}

fn canonical(project: &Path) -> Result<(Vec<CanonicalAttempt>, Vec<String>)> {
    let path = project.join(".state/state.db");
    if !path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let db = super::read_only(&path)?;
    let table = |name: &str| db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get::<_, bool>(0));
    let profile = |value: &Value| (value["kind"].as_str().map(str::to_owned), value["execution_home"].as_str().map(str::to_owned));
    let mut attempts = Vec::new();
    if table("attempt_inputs")? {
        let sql = if table("dispatch_decisions")? {
            "SELECT i.attempt_id,i.payload,d.decided_unix_ms FROM attempt_inputs i LEFT JOIN dispatch_decisions d ON d.attempt_id=i.attempt_id ORDER BY i.attempt_id"
        } else {
            "SELECT attempt_id,payload,NULL FROM attempt_inputs ORDER BY attempt_id"
        };
        let mut stmt = db.prepare(sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)))?;
        for row in rows {
            let (id, payload, decided_unix_ms) = row?;
            let inputs: Value = serde_json::from_str(&payload)?;
            let (kind, home) = profile(&inputs["inputs"]["effective_profile"]);
            attempts.push(CanonicalAttempt { id, kind, home, decided_unix_ms, binding: Binding::Predates });
        }
    }
    if table("collector_bindings")? {
        let mut stmt = db.prepare("SELECT b.attempt_id,b.state,b.execution_home,b.unix_ms FROM collector_bindings b
            WHERE b.revision=(SELECT max(revision) FROM collector_bindings c WHERE c.attempt_id=b.attempt_id)")?;
        let mut latest: std::collections::BTreeMap<String, Binding> = stmt.query_map([], |r| {
            let (state, home, at): (String, Option<String>, i64) = (r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((r.get(0)?, match state.as_str() { "active" => Binding::Active(home), "revoked" => Binding::Revoked(home, at), _ => Binding::Predates }))
        })?.collect::<rusqlite::Result<_>>()?;
        for attempt in &mut attempts {
            attempt.binding = latest.remove(&attempt.id).unwrap_or(Binding::None);
        }
    }
    let mut homes: Vec<String> = attempts.iter().filter(|a| a.codex()).filter_map(|a| a.home.clone()).collect();
    if table("native_profiles")? {
        let mut stmt = db.prepare("SELECT report FROM native_profiles")?;
        for report in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let report: Value = serde_json::from_str(&report?)?;
            if let (Some(kind), Some(home)) = profile(&report["preparation"]["profile"]) && kind == "codex" { homes.push(home); }
        }
    }
    homes.sort();
    homes.dedup();
    Ok((attempts, homes))
}

pub fn canonical_attempts(project: &Path) -> Result<Vec<CanonicalAttempt>> {
    Ok(canonical(project)?.0)
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Scan every Codex execution home, ingest complete new lines, recompute bindings.
/// `create` false: a project without Codex homes gets no sidecar.
pub fn collect(project: &Path, budget: Budget, create: bool) -> Result<Option<Collected>> {
    let (attempts, homes) = canonical(project)?;
    let Some(mut db) = super::sidecar::open(project, create || !homes.is_empty())? else { return Ok(None) };
    let project = std::fs::canonicalize(project)?;
    let worktrees = format!("{}/.state/worktrees/", project.display());
    let mut done = Collected::default();
    let mut remaining = budget.bytes;
    let reread = reread(&db)?;
    // Sources read before A4 (no `rollout_metadata`) are read again from byte 0
    // too, so their rows gain the A4 metadata; stored keys dedupe.
    let from_start: std::collections::BTreeSet<String> = reread.union(&backfill(&db)?).cloned().collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut unwritable = false;
    for home in &homes {
        let mut files = Vec::new();
        walk(&Path::new(home).join(".codex/sessions"), 0, &mut files);
        files.sort();
        seen.extend(files.iter().map(|file| digest(file.as_os_str().as_encoded_bytes())));
        for file in files {
            if remaining == 0 || unwritable {
                done.budget_exhausted |= remaining == 0;
                break;
            }
            let mut span = (0, 0);
            let read = match tail(&mut db, &file, &digest(home.as_bytes()), &worktrees, remaining, &from_start, &mut done, &mut span) {
                Ok(read) => read,
                // The pass rolled back: its range is a coverage gap and later
                // sources wait for the next collect (contracts-collection.md A2).
                Err(error) if ingest::unwritable(&error) => {
                    let key = digest(file.as_os_str().as_encoded_bytes());
                    if ingest::gap(&db, &key, span.0, span.1, "sidecar_write_failed", jiff::Timestamp::now().as_millisecond()).is_err() { return Err(error); }
                    unwritable = true;
                    0
                }
                Err(error) => return Err(error),
            };
            remaining -= read.min(remaining);
        }
    }
    done.budget_exhausted |= remaining == 0;
    for key in reread.difference(&seen) {
        db.execute("UPDATE rollout_sources SET reevaluation='rollout_unavailable' WHERE path_digest=?1", [key])?;
    }
    bind(&db, &attempts)?;
    Ok(Some(done))
}

/// Sources whose version is now certified but that hold rows stored while it
/// was not, at or before their offset: re-read from the start (contracts §5).
/// Rows past the offset are reached by the ordinary tail, so a re-read that
/// runs out of budget continues instead of starting over.
fn reread(db: &Connection) -> Result<std::collections::BTreeSet<String>> {
    let sources: Vec<(String, String)> = db.prepare("SELECT s.path_digest,s.cli_version FROM rollout_sources s JOIN collect_offsets o ON o.path_digest=s.path_digest
        WHERE EXISTS(SELECT 1 FROM codex_usage u WHERE u.path_digest=s.path_digest AND u.reason='cli_version_uncertified' AND u.ordinal<=o.records)")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(sources.into_iter().filter(|(_, version)| certified(version)).map(|(key, _)| key).collect())
}

/// Sources with a session read before the A4 metadata existed (ingest 0004):
/// no `rollout_metadata` row although their `session_meta` was stored.
fn backfill(db: &Connection) -> Result<std::collections::BTreeSet<String>> {
    Ok(db.prepare("SELECT s.path_digest FROM rollout_sources s JOIN collect_offsets o ON o.path_digest=s.path_digest
        WHERE NOT EXISTS(SELECT 1 FROM rollout_metadata m WHERE m.path_digest=s.path_digest)")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Regular `rollout-*.jsonl` files only; symlinks are never followed.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() && depth < MAX_DEPTH {
            walk(&entry.path(), depth + 1, out);
        } else if kind.is_file() && name.starts_with("rollout-") && name.ends_with(".jsonl") {
            out.push(entry.path());
        }
    }
}

/// Every line must be a JSON object with a string `type`; anything else is
/// quarantined `line_malformed` (contracts-collection.md A3).
#[derive(Deserialize)]
struct LineTag {
    #[serde(rename = "type")]
    kind: String,
}
/// Top-level kinds the adapter reads (contracts §5); others are ignored unread.
const KINDS: [&str; 4] = ["session_meta", "turn_context", "token_usage_record", "event_msg"];
#[derive(Deserialize)]
struct Tag {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<PayloadTag>,
}
#[derive(Deserialize)]
struct PayloadTag {
    #[serde(rename = "type")]
    kind: Option<String>,
}
#[derive(Deserialize)]
struct Envelope<T> {
    payload: T,
}
#[derive(Deserialize)]
struct SessionMeta {
    id: String,
    timestamp: Option<String>,
    cwd: String,
    cli_version: String,
    originator: Option<String>,
    source: Option<Value>,
}
#[derive(Deserialize)]
struct TurnContext {
    model: Option<String>,
    effort: Option<String>,
}
#[derive(Deserialize, Default)]
struct Usage {
    input_tokens: Option<Number>,
    cached_input_tokens: Option<Number>,
    cache_write_input_tokens: Option<Number>,
    output_tokens: Option<Number>,
    reasoning_output_tokens: Option<Number>,
    total_tokens: Option<Number>,
}
#[derive(Deserialize)]
struct UsageRecord {
    turn_id: Option<String>,
    response_id: Option<String>,
    #[serde(default)]
    usage: Usage,
    thread_token_usage: Option<Usage>,
}
#[derive(Deserialize)]
struct TokenCount {
    info: Option<TokenInfo>,
    rate_limits: Option<RateLimits>,
}
#[derive(Deserialize)]
struct TokenInfo {
    total_token_usage: Option<Usage>,
}
#[derive(Deserialize)]
struct RateLimits {
    limit_id: Option<String>,
    primary: Option<Window>,
    plan_type: Option<String>,
}
#[derive(Deserialize)]
struct Window {
    used_percent: Option<Number>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}
#[derive(Deserialize)]
struct TaskComplete {
    turn_id: Option<String>,
    duration_ms: Option<i64>,
    time_to_first_token_ms: Option<i64>,
}

impl Usage {
    fn fields(&self) -> [(&'static str, &Option<Number>); 6] {
        [("cache_write_input_tokens", &self.cache_write_input_tokens), ("cached_input_tokens", &self.cached_input_tokens),
            ("input_tokens", &self.input_tokens), ("output_tokens", &self.output_tokens),
            ("reasoning_output_tokens", &self.reasoning_output_tokens), ("total_tokens", &self.total_tokens)]
    }
    fn json(&self) -> Value {
        Value::Object(self.fields().into_iter().map(|(k, v)| (k.to_owned(), v.clone().map_or(Value::Null, Value::Number))).collect())
    }
    /// `[cache_write, cached, input, output, reasoning, total]` when all are integers in `0..=2^53`.
    fn counters(&self) -> Option<[i64; 6]> {
        let mut out = [0; 6];
        for (slot, (_, value)) in out.iter_mut().zip(self.fields()) {
            *slot = value.as_ref()?.as_u64().filter(|v| *v <= MAX_SAFE)? as i64;
        }
        Some(out)
    }
}

fn ms(text: Option<&str>) -> Option<i64> {
    text?.parse::<jiff::Timestamp>().ok().map(|t| t.as_millisecond())
}

/// The attempt whose worktree contains `cwd`, compared textually; `..` and `.` refuse.
fn cwd_attempt(cwd: &str, worktrees: &str) -> Option<String> {
    if cwd.split('/').any(|part| part == ".." || part == ".") { return None; }
    let rest = cwd.strip_prefix(worktrees)?;
    let id = rest.split('/').next().filter(|id| !id.is_empty())?;
    Some(id.to_owned())
}

struct Cursor {
    offset: u64,
    records: i64,
    rate_limits: i64,
    model: Option<String>,
    effort: Option<String>,
    session: Option<(String, String, Option<i64>)>,
}

/// Ingest complete lines of one rollout after its stored offset, with their
/// envelopes, in one sidecar transaction. `span`: the byte range this pass covers.
#[allow(clippy::too_many_arguments)]
fn tail(db: &mut Connection, file: &Path, home: &str, worktrees: &str, allowance: u64, reread: &std::collections::BTreeSet<String>, done: &mut Collected,
    span: &mut (u64, u64)) -> Result<u64> {
    let key = digest(file.as_os_str().as_encoded_bytes());
    let Ok(mut handle) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(file) else { return Ok(0) };
    let meta = handle.metadata()?;
    let tx = db.transaction()?;
    let stored = tx.query_row("SELECT device,inode,byte_offset,records,rate_limits,model,effort FROM collect_offsets WHERE path_digest=?1", [&key],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).optional()?;
    let mut cursor = match stored {
        Some((dev, ino, offset, records, rate_limits, model, effort)) if !reread.contains(&key) && dev as u64 == meta.dev() && ino as u64 == meta.ino() && offset as u64 <= meta.len() =>
            Cursor { offset: offset as u64, records, rate_limits, model, effort, session: None },
        // New, replaced or re-read file: read from the start; existing keys
        // dedupe, re-evaluate or quarantine.
        _ => {
            tx.execute("DELETE FROM rollout_sources WHERE path_digest=?1", [&key])?;
            tx.execute("DELETE FROM rollout_metadata WHERE path_digest=?1", [&key])?;
            Cursor { offset: 0, records: 0, rate_limits: 0, model: None, effort: None, session: None }
        }
    };
    let now = jiff::Timestamp::now().as_millisecond();
    *span = (cursor.offset, meta.len());
    let ledger = ingest::Ledger::begin(&tx, &key, cursor.offset, now)?;
    if cursor.offset == meta.len() {
        if ledger.fresh && cursor.offset > 0 {
            ledger.finish(&tx, None, cursor.offset, now)?;
            tx.commit()?;
        }
        return Ok(0);
    }
    cursor.session = tx.query_row("SELECT session_id,cli_version,session_unix_ms FROM rollout_sources WHERE path_digest=?1", [&key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    handle.seek(SeekFrom::Start(cursor.offset))?;
    let mut reader = BufReader::new(handle.take(allowance));
    let (mut line, mut read) = (Vec::new(), 0u64);
    loop {
        line.clear();
        let n = reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut line)? as u64;
        if n == 0 { break; }
        if line.last() != Some(&b'\n') {
            if n < MAX_LINE { break; } // Partial last line: wait for the writer.
            // Oversized: skip to its end without retaining it.
            let mut skipped = n;
            loop {
                line.clear();
                let more = reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut line)? as u64;
                skipped += more;
                if more == 0 || line.last() == Some(&b'\n') { break; }
            }
            if line.last() != Some(&b'\n') { break; }
            ledger.oversized_line(&tx, cursor.offset, skipped, now)?;
            read += skipped;
            cursor.offset += skipped;
            continue;
        }
        read += n;
        let at = cursor.offset;
        cursor.offset += n;
        // Not an object with a string `type` (corrupt, truncated mid-file, blank): quarantined.
        let object = line.trim_ascii_start().starts_with(b"{");
        let Some(LineTag { kind }) = serde_json::from_slice::<LineTag>(&line).ok().filter(|_| object) else {
            ledger.malformed(&tx, at, "line_malformed", n, now)?;
            continue;
        };
        // Unknown kinds are skipped by type tag without reading further.
        if !KINDS.contains(&kind.as_str()) { continue; }
        let first_meta = cursor.session.is_none();
        // A read kind whose typed fields do not parse yields no rows and no envelope.
        let Ok(tag) = serde_json::from_slice::<Tag>(&line) else {
            ledger.malformed(&tx, at, "record_malformed", n, now)?;
            continue;
        };
        if !record(&tx, &tag, &line, &key, home, worktrees, &mut cursor, now, done)? {
            ledger.malformed(&tx, at, "record_malformed", n, now)?;
            continue;
        }
        let kind = match (tag.kind.as_deref(), tag.payload.as_ref().and_then(|p| p.kind.as_deref())) {
            (Some("event_msg"), inner) => inner,
            (Some("session_meta"), _) if !first_meta => None,
            (kind, _) => kind,
        };
        if let (Some(kind), Some((session, version, _))) = (kind, &cursor.session)
            && let Ok(Envelope { payload }) = serde_json::from_slice::<Envelope<Value>>(&line) {
            ledger.observe(&tx, at, ingest::Record { kind, payload: &payload, occurred_unix_ms: ms(tag.timestamp.as_deref()), session, adapter_version: version }, now)?;
        }
    }
    // Bytes pulled from disk, including a partial last line, count against the budget.
    let pulled = allowance - reader.into_inner().limit();
    if read > 0 { done.files += 1; }
    done.bytes += pulled;
    tx.execute("INSERT INTO collect_offsets(path_digest,device,inode,byte_offset,records,rate_limits,model,effort,updated_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
        ON CONFLICT(path_digest) DO UPDATE SET device=excluded.device,inode=excluded.inode,byte_offset=excluded.byte_offset,records=excluded.records,
        rate_limits=excluded.rate_limits,model=excluded.model,effort=excluded.effort,updated_unix_ms=excluded.updated_unix_ms",
        params![key, meta.dev() as i64, meta.ino() as i64, cursor.offset as i64, cursor.records, cursor.rate_limits, cursor.model, cursor.effort, now])?;
    tx.execute("UPDATE rollout_sources SET records=?2 WHERE path_digest=?1", params![key, cursor.records])?;
    ledger.finish(&tx, cursor.session.as_ref().map(|s| s.0.as_str()), cursor.offset, now)?;
    if let Some((session, version, _)) = &cursor.session && certified(version) {
        reconcile(&tx, session, now)?;
    }
    tx.commit()?;
    Ok(pulled)
}

/// Store one read record. `false`: its kind is read but its typed fields do not
/// parse (the caller quarantines it `record_malformed`); a malformed
/// `turn_context` also makes the model and effort of later records unknown.
#[allow(clippy::too_many_arguments)]
fn record(tx: &Transaction, tag: &Tag, line: &[u8], key: &str, home: &str, worktrees: &str, cursor: &mut Cursor, now: i64, done: &mut Collected) -> Result<bool> {
    let inner = tag.payload.as_ref().and_then(|p| p.kind.as_deref());
    match (tag.kind.as_deref(), inner) {
        (Some("session_meta"), _) if cursor.session.is_none() => {
            let Ok(Envelope { payload: meta }) = serde_json::from_slice::<Envelope<SessionMeta>>(line) else { return Ok(false) };
            let source = match meta.source {
                Some(Value::String(s)) => Some(s),
                Some(Value::Object(map)) => map.keys().next().cloned(),
                _ => None,
            };
            let at = ms(meta.timestamp.as_deref());
            let Ok(Envelope { payload: raw }) = serde_json::from_slice::<Envelope<Value>>(line) else { return Ok(false) };
            tx.execute("INSERT INTO rollout_sources(path_digest,home_digest,session_id,session_unix_ms,cwd,cwd_attempt,cli_version,originator,source,records,binding,observed_unix_ms)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0,'unbound',?10)",
                params![key, home, meta.id, at, sanitize::home_prefix(&meta.cwd), cwd_attempt(&meta.cwd, worktrees), meta.cli_version, meta.originator, source, now])?;
            // A4 metadata (contracts-collection.md), read leniently: a field of
            // another type is NULL and never makes the session record malformed.
            let text = |path: &str, class| sanitize::field(&raw, path, class).as_str().map(str::to_owned);
            tx.execute("INSERT OR REPLACE INTO rollout_metadata(path_digest,model_provider,forked_from_id,subagent_kind,subagent_parent_thread_id,subagent_depth)
                VALUES(?1,?2,?3,?4,?5,?6)", params![key, text("model_provider", sanitize::Class::Text), text("forked_from_id", sanitize::Class::Id),
                    text("subagent_kind", sanitize::Class::Tag), text("subagent_parent_thread_id", sanitize::Class::Id),
                    sanitize::field(&raw, "subagent_depth", sanitize::Class::Number).as_i64()])?;
            cursor.session = Some((meta.id, meta.cli_version, at));
        }
        (Some("turn_context"), _) => {
            let Ok(Envelope { payload }) = serde_json::from_slice::<Envelope<TurnContext>>(line) else {
                (cursor.model, cursor.effort) = (None, None);
                return Ok(false);
            };
            (cursor.model, cursor.effort) = (payload.model, payload.effort);
        }
        (Some("token_usage_record"), _) => {
            let Ok(Envelope { payload: record }) = serde_json::from_slice::<Envelope<UsageRecord>>(line) else { return Ok(false) };
            cursor.records += 1;
            let Some((session, version, _)) = &cursor.session else { return Ok(true) };
            let payload = json!({"response_id": record.response_id, "turn_id": record.turn_id, "model": cursor.model, "effort": cursor.effort, "usage": record.usage.json()});
            let payload_digest = digest(payload.to_string().as_bytes());
            let first: Option<(String, Option<String>)> = tx.query_row("SELECT payload_digest,reason FROM codex_usage WHERE session_id=?1 AND ordinal=?2",
                params![session, cursor.records], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            // A4: the line time, outside the payload digest. The first stays; a
            // row stored before A4 gains it when its line is read again.
            if first.as_ref().is_none_or(|(first, _)| *first == payload_digest) {
                tx.execute("INSERT OR IGNORE INTO codex_usage_times(session_id,ordinal,record_unix_ms) VALUES(?1,?2,?3)",
                    params![session, cursor.records, ms(tag.timestamp.as_deref())])?;
            }
            match first {
                // Stored while uncertified, now certified: the same record, evaluated again.
                Some((first, Some(reason))) if first == payload_digest && reason == "cli_version_uncertified" && certified(version) => {
                    let (reason, c) = evaluate(&record.usage, version);
                    tx.execute("UPDATE codex_usage SET cache_write_input_tokens=?3,cached_input_tokens=?4,input_tokens=?5,output_tokens=?6,
                        reasoning_output_tokens=?7,total_tokens=?8,accepted=?9,reason=?10 WHERE session_id=?1 AND ordinal=?2",
                        params![session, cursor.records, c[0], c[1], c[2], c[3], c[4], c[5], reason.is_none(), reason])?;
                    done.reevaluated += 1;
                }
                Some((first, _)) if first == payload_digest => {}
                Some((first, _)) => {
                    tx.execute("INSERT OR IGNORE INTO codex_quarantine(session_id,ordinal,first_digest,new_digest,observed_unix_ms) VALUES(?1,?2,?3,?4,?5)",
                        params![session, cursor.records, first, payload_digest, now])?;
                }
                None => {
                    let (reason, c) = evaluate(&record.usage, version);
                    tx.execute("INSERT INTO codex_usage(session_id,ordinal,path_digest,response_id,turn_id,model,effort,payload_digest,cache_write_input_tokens,cached_input_tokens,
                        input_tokens,output_tokens,reasoning_output_tokens,total_tokens,accepted,reason,observed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                        params![session, cursor.records, key, record.response_id, record.turn_id, cursor.model, cursor.effort, payload_digest,
                            c[0], c[1], c[2], c[3], c[4], c[5], reason.is_none(), reason, now])?;
                    done.records += 1;
                }
            }
            if let Some(thread) = record.thread_token_usage.filter(|_| certified(version)) {
                tx.execute("UPDATE rollout_sources SET thread_usage=?2 WHERE path_digest=?1", params![key, thread.json().to_string()])?;
            }
        }
        (Some("event_msg"), Some("token_count")) => {
            let Ok(Envelope { payload }) = serde_json::from_slice::<Envelope<TokenCount>>(line) else { return Ok(false) };
            let Ok(Envelope { payload: raw }) = serde_json::from_slice::<Envelope<Value>>(line) else { return Ok(false) };
            let Some((session, version, at)) = &cursor.session else { return Ok(true) };
            if let Some(total) = payload.info.and_then(|i| i.total_token_usage).filter(|_| certified(version)) {
                tx.execute("UPDATE rollout_sources SET token_count_usage=?2 WHERE path_digest=?1", params![key, total.json().to_string()])?;
            }
            if let (Some(limits), Some(observed)) = (payload.rate_limits, ms(tag.timestamp.as_deref()).or(*at)) {
                cursor.rate_limits += 1;
                let window = limits.primary;
                tx.execute("INSERT OR IGNORE INTO codex_rate_limits(session_id,ordinal,limit_id,used_percent,window_minutes,resets_at,plan_type,observed_ts) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![session, cursor.rate_limits, limits.limit_id, window.as_ref().and_then(|w| w.used_percent.as_ref()).map(Number::to_string),
                        window.as_ref().and_then(|w| w.window_minutes), window.as_ref().and_then(|w| w.resets_at), limits.plan_type, observed])?;
                // A4: the secondary window and the reached type, read leniently
                // (another type is NULL, never a malformed record); the first stays.
                let number = |path: &str| sanitize::field(&raw, path, sanitize::Class::Number);
                let percent = match number("rate_limits.secondary.used_percent") { Value::Number(n) => Some(n.to_string()), Value::String(s) => Some(s), _ => None };
                tx.execute("INSERT OR IGNORE INTO codex_rate_limit_windows(session_id,ordinal,secondary_used_percent,secondary_window_minutes,secondary_resets_at,
                    rate_limit_reached_type) VALUES(?1,?2,?3,?4,?5,?6)", params![session, cursor.rate_limits, percent, number("rate_limits.secondary.window_minutes").as_i64(),
                    number("rate_limits.secondary.resets_at").as_i64(),
                    sanitize::field(&raw, "rate_limits.rate_limit_reached_type", sanitize::Class::Text).as_str()])?;
            }
        }
        (Some("event_msg"), Some("task_complete")) => {
            let Ok(Envelope { payload }) = serde_json::from_slice::<Envelope<TaskComplete>>(line) else { return Ok(false) };
            if let (Some((session, ..)), Some(turn)) = (&cursor.session, payload.turn_id) {
                tx.execute("INSERT OR IGNORE INTO codex_turns(session_id,turn_id,model,effort,duration_ms,time_to_first_token_ms) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![session, turn, cursor.model, cursor.effort, payload.duration_ms, payload.time_to_first_token_ms])?;
            }
        }
        _ => {}
    }
    Ok(true)
}

/// Contracts §5 validation: `invariant_violation` before
/// `cli_version_uncertified`; a record not accepted keeps no counters.
fn evaluate(usage: &Usage, version: &str) -> (Option<&'static str>, [Option<i64>; 6]) {
    let valid = usage.counters().filter(|[_, cached, input, output, reasoning, total]| *total == input + output && cached <= input && reasoning <= output);
    let reason = if valid.is_none() { Some("invariant_violation") } else if !certified(version) { Some("cli_version_uncertified") } else { None };
    (reason, valid.filter(|_| reason.is_none()).map(|c| c.map(Some)).unwrap_or([None; 6]))
}

/// Σ accepted usage vs the last reported `thread_token_usage` and `token_count` totals.
fn reconcile(tx: &Transaction, session: &str, now: i64) -> Result<()> {
    const FIELDS: [&str; 6] = ["cache_write_input_tokens", "cached_input_tokens", "input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"];
    let sums: Vec<i64> = tx.query_row(&format!("SELECT {} FROM codex_usage WHERE session_id=?1 AND accepted=1",
        FIELDS.map(|f| format!("coalesce(sum({f}),0)")).join(",")), [session], |r| (0..6).map(|i| r.get(i)).collect())?;
    let reported: Vec<(Option<String>, Option<String>)> = tx.prepare("SELECT thread_usage,token_count_usage FROM rollout_sources WHERE session_id=?1")?
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (kind, value) in [("thread_total", reported.iter().rev().find_map(|r| r.0.clone())), ("token_count_total", reported.iter().rev().find_map(|r| r.1.clone()))] {
        let Some(value) = value else { continue };
        let value: Value = serde_json::from_str(&value)?;
        let differing: Vec<&str> = FIELDS.iter().zip(&sums).filter(|(f, sum)| value[**f].as_i64() != Some(**sum)).map(|(f, _)| *f).collect();
        if differing.is_empty() {
            tx.execute("DELETE FROM codex_discrepancy WHERE session_id=?1 AND kind=?2", params![session, kind])?;
        } else {
            tx.execute("INSERT INTO codex_discrepancy(session_id,kind,summed_total,reported_total,fields,observed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)
                ON CONFLICT(session_id,kind) DO UPDATE SET summed_total=excluded.summed_total,reported_total=excluded.reported_total,fields=excluded.fields,observed_unix_ms=excluded.observed_unix_ms",
                params![session, kind, sums[5], value["total_tokens"].as_i64().unwrap_or(-1), json!(differing).to_string(), now])?;
        }
    }
    Ok(())
}

/// Contracts §5 binding rule through the canonical collector binding
/// (contracts-collection.md), recomputed on every collect. Rules 2 and 3 select
/// candidates; rule 1 uses the binding's `execution_home`. A revoked binding
/// keeps rollouts that started before the revocation (their accepted usage is
/// never erased) and binds none that start at or after it. Attempts that
/// predate 0052 fall back to rules 1-4 on their retained inputs.
fn bind(db: &Connection, attempts: &[CanonicalAttempt]) -> Result<()> {
    let sources: Vec<(String, String, Option<i64>, Option<String>)> = db.prepare("SELECT path_digest,home_digest,session_unix_ms,cwd_attempt FROM rollout_sources")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    for (key, home, at, cwd_attempt) in sources {
        let rule1 = |h: &Option<String>| h.as_ref().is_some_and(|h| digest(h.as_bytes()) == home);
        let (mut matches, mut refused) = (Vec::new(), None);
        for a in attempts.iter().filter(|a| a.codex() && cwd_attempt.as_deref() == Some(a.id.as_str())) {
            let Some(at) = at.filter(|at| a.decided_unix_ms.is_some_and(|decided| *at >= decided)) else { continue };
            match &a.binding {
                Binding::Predates if rule1(&a.home) => matches.push((a, "predates_binding")),
                Binding::Active(h) if rule1(h) => matches.push((a, "collector_binding")),
                Binding::Revoked(h, revoked) if rule1(h) && at < *revoked => matches.push((a, "collector_binding")),
                Binding::Revoked(h, _) if rule1(h) => refused = refused.or(Some("binding_revoked")),
                Binding::None => refused = refused.or(Some("no_binding")),
                _ => {}
            }
        }
        let (binding, attempt, basis) = match matches.as_slice() {
            [] => ("unbound", None, refused.unwrap_or("no_match")),
            [(one, basis)] => ("bound", Some(one.id.as_str()), *basis),
            _ => ("ambiguous", None, "ambiguous"),
        };
        db.execute("UPDATE rollout_sources SET binding=?2,attempt_id=?3 WHERE path_digest=?1", params![key, binding, attempt])?;
        db.execute("INSERT INTO source_bindings(path_digest,basis) VALUES(?1,?2) ON CONFLICT(path_digest) DO UPDATE SET basis=excluded.basis", params![key, basis])?;
    }
    Ok(())
}
