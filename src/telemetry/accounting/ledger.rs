//! The usage ledger (docs/telemetry/contracts-accounting.md §1–§2): entries
//! normalized from the Codex sidecar tables, read by SQL only, with one
//! disposition per entry and rollout that observed it.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Codex counter convention: input includes cache reads, output includes reasoning.
pub const NORMALIZATION: &str = "codex-v1";
const MAX_SAFE: i64 = 1 << 53;
const NATIVE: [&str; 6] = ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"];
const NORMALIZED: [&str; 7] = ["input_tokens", "cache_read_tokens", "new_input_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens", "total_tokens"];

pub struct Entry {
    pub id: String,
    pub session: String,
    pub basis: &'static str,
    scope: &'static str,
    precedence: i64,
    pub position: i64,
    response_id: Option<String>,
    pub model: Option<String>,
    native: [Option<i64>; 6],
    /// `NORMALIZED` order; `None` when the native fields do not satisfy `codex-v1`.
    pub normalized: Option<[i64; 7]>,
    /// `(path_digest, disposition, reason)`, first observation first.
    pub provenance: Vec<(String, &'static str, Option<String>)>,
}

impl Entry {
    /// Counted in usage totals: the primary (delta) basis with an accepted observation.
    pub fn counted(&self) -> bool {
        self.basis == "delta" && self.provenance.iter().any(|p| p.1 == "accepted")
    }
}

/// `codex-v1`: all six native counters present, non-negative and ≤ 2^53, with
/// cached ⊆ input, reasoning ⊆ output and total = input + output. Cache writes
/// are kept as reported; their overlap with input is not certified.
fn normalize(native: [Option<i64>; 6]) -> Option<[i64; 7]> {
    let mut c = [0; 6];
    for (slot, value) in c.iter_mut().zip(native) { *slot = value.filter(|v| (0..=MAX_SAFE).contains(v))?; }
    let [input, cached, write, output, reasoning, total] = c;
    (cached <= input && reasoning <= output && total == input + output).then_some([input, cached, input - cached, write, output, reasoning, total])
}

/// Every entry derivable from the Codex tables, in `(session, basis, position)` order.
pub fn derive(db: &Connection) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut others = db.prepare("SELECT path_digest FROM rollout_sources WHERE session_id=?1 AND path_digest<>?2 AND records>=?3 ORDER BY path_digest")?;
    let mut stmt = db.prepare("SELECT u.session_id,u.ordinal,u.path_digest,u.response_id,u.model,u.accepted,u.reason,u.input_tokens,u.cached_input_tokens,
        u.cache_write_input_tokens,u.output_tokens,u.reasoning_output_tokens,u.total_tokens,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=u.session_id AND q.ordinal=u.ordinal) FROM codex_usage u ORDER BY u.session_id,u.ordinal")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let (session, ordinal, first): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (accepted, reason, quarantined): (bool, Option<String>, bool) = (r.get(5)?, r.get(6)?, r.get(13)?);
        let native = [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?];
        let normalized = normalize(native);
        let (disposition, reason) = match (quarantined, accepted, normalized) {
            (true, ..) => ("conflict", Some("payload_digest_mismatch".to_owned())),
            (false, true, Some(_)) => ("accepted", None),
            (false, true, None) => ("unresolved", Some("normalization_refused".to_owned())),
            (false, false, _) => ("unresolved", reason),
        };
        let mut provenance = vec![(first.clone(), disposition, reason.clone())];
        for other in others.query_map(params![session, first, ordinal], |r| r.get::<_, String>(0))? {
            let (disposition, reason) = if quarantined { (disposition, reason.clone()) } else { ("duplicate", None) };
            provenance.push((other?, disposition, reason));
        }
        entries.push(Entry { id: format!("codex:{session}:{ordinal}"), session, basis: "delta", scope: "request", precedence: 1, position: ordinal,
            response_id: r.get(3)?, model: r.get(4)?, native, normalized, provenance });
    }
    // Cumulative thread totals (secondary basis, reconciliation only): per
    // session by position, a total above the high-water mark is accepted, an
    // equal one duplicates it, a lower one is a regression without reset evidence.
    let mut stmt = db.prepare("SELECT session_id,path_digest,records,thread_usage FROM rollout_sources WHERE thread_usage IS NOT NULL ORDER BY session_id,records,path_digest")?;
    let mut rows = stmt.query([])?;
    let mut high: Option<(String, i64)> = None;
    while let Some(r) = rows.next()? {
        let (session, path, position, usage): (String, String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        let usage: Value = serde_json::from_str(&usage)?;
        let native = NATIVE.map(|k| usage[k].as_i64());
        let normalized = normalize(native);
        let mark = high.as_ref().filter(|h| h.0 == session).map(|h| h.1);
        let (disposition, reason) = match (normalized.map(|n| n[6]), mark) {
            (None, _) => ("unresolved", Some("invariant_violation")),
            (Some(total), Some(mark)) if total == mark => ("duplicate", None),
            (Some(total), Some(mark)) if total < mark => ("unresolved", Some("regression_without_reset")),
            (Some(total), _) => { high = Some((session.clone(), total)); ("accepted", None) }
        };
        entries.push(Entry { id: format!("codex:{session}:thread:{path}"), session, basis: "cumulative", scope: "thread", precedence: 2, position,
            response_id: None, model: None, native, normalized, provenance: vec![(path, disposition, reason.map(str::to_owned))] });
    }
    Ok(entries)
}

/// Rebuild the ledger, session graph and model segments (§3) from the Codex
/// tables in one sidecar transaction; returns counts.
pub fn sync(db: &mut Connection) -> Result<Value> {
    let tx = db.transaction()?;
    let entries = derive(&tx)?;
    tx.execute_batch("DELETE FROM usage_dispositions; DELETE FROM usage_entries;")?;
    let mut counts = BTreeMap::<&str, usize>::new();
    for e in &entries {
        let n = e.normalized.map(|n| n.map(Some)).unwrap_or([None; 7]);
        let native = Value::Object(NATIVE.iter().zip(e.native).map(|(k, v)| ((*k).to_owned(), json!(v))).collect());
        tx.execute("INSERT INTO usage_entries(entry_id,source,session_id,basis,scope,normalization_version,precedence,position,response_id,model,native,
            input_tokens,cache_read_tokens,new_input_tokens,cache_write_tokens,output_tokens,reasoning_tokens,total_tokens)
            VALUES(?1,'codex',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![e.id, e.session, e.basis, e.scope, NORMALIZATION, e.precedence, e.position, e.response_id, e.model, native.to_string(),
                n[0], n[1], n[2], n[3], n[4], n[5], n[6]])?;
        for (path, disposition, reason) in &e.provenance {
            tx.execute("INSERT INTO usage_dispositions(entry_id,path_digest,disposition,reason) VALUES(?1,?2,?3,?4)", params![e.id, path, disposition, reason])?;
            *counts.entry(disposition).or_default() += 1;
        }
    }
    let (sessions, segments) = super::graph::store(&tx, &entries)?;
    tx.execute("INSERT INTO usage_ledger(singleton,normalization_version,synced_unix_ms) VALUES(1,?1,?2)
        ON CONFLICT(singleton) DO UPDATE SET normalization_version=excluded.normalization_version,synced_unix_ms=excluded.synced_unix_ms",
        params![NORMALIZATION, jiff::Timestamp::now().as_millisecond()])?;
    tx.commit()?;
    Ok(json!({"entries": entries.len(), "dispositions": counts, "sessions": sessions, "model_segments": segments}))
}

/// The synced ledger as JSON, read-only; `ledger_not_synced` before the first sync.
pub fn read(db: &Connection) -> Result<Value> {
    let synced = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_ledger')", [], |r| r.get::<_, bool>(0))?
        && db.query_row("SELECT 1 FROM usage_ledger", [], |_| Ok(())).optional()?.is_some();
    if !synced { return Ok(super::unavailable("ledger_not_synced")); }
    let mut provenance = BTreeMap::<String, Vec<Value>>::new();
    let mut stmt = db.prepare("SELECT entry_id,path_digest,disposition,reason FROM usage_dispositions ORDER BY entry_id,path_digest")?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))? {
        let (entry, path, disposition, reason) = row?;
        provenance.entry(entry).or_default().push(json!({"path_digest": path, "disposition": disposition, "reason": reason}));
    }
    let mut stmt = db.prepare("SELECT entry_id,session_id,basis,scope,normalization_version,precedence,position,response_id,model,native,
        input_tokens,cache_read_tokens,new_input_tokens,cache_write_tokens,output_tokens,reasoning_tokens,total_tokens FROM usage_entries ORDER BY entry_id")?;
    let mut entries = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let counters: Vec<Option<i64>> = (10..17).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?;
        let normalized = if counters.iter().all(Option::is_some) { Value::Object(NORMALIZED.iter().zip(&counters).map(|(k, v)| ((*k).to_owned(), json!(v))).collect()) }
            else { super::unavailable("not_normalized") };
        entries.push(json!({"entry_id": id, "session_id": r.get::<_, String>(1)?, "basis": r.get::<_, String>(2)?, "scope": r.get::<_, String>(3)?,
            "normalization_version": r.get::<_, String>(4)?, "precedence": r.get::<_, i64>(5)?, "position": r.get::<_, i64>(6)?,
            "response_id": r.get::<_, Option<String>>(7)?, "model": r.get::<_, Option<String>>(8)?,
            "native": serde_json::from_str::<Value>(&r.get::<_, String>(9)?)?, "normalized": normalized,
            "provenance": provenance.remove(&id).unwrap_or_default()}));
    }
    Ok(json!({"entries": entries}))
}
