//! The usage ledger (docs/telemetry/contracts-accounting.md §1–§2): entries
//! normalized from the Codex sidecar tables, read by SQL only, with one
//! disposition per entry and rollout that observed it.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Codex counter convention: input includes cache reads, output includes reasoning.
pub const NORMALIZATION: &str = "codex-v1";
const MAX_SAFE: i64 = 1 << 53;
const NATIVE: [&str; 6] = ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"];
const NORMALIZED: [&str; 7] = ["input_tokens", "cache_read_tokens", "new_input_tokens", "cache_write_tokens", "output_tokens", "reasoning_tokens", "total_tokens"];
/// Disposition reason of a delta record that repeats an earlier accepted record
/// of its session: the same native `response_id` and the same payload digest
/// (§1). It is one invocation observed twice, never counted again.
pub const REPEATED: &str = "response_repeated";

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

    /// A delta record repeating an earlier accepted record of its session (`REPEATED`).
    pub fn repeated(&self) -> bool {
        self.provenance.first().is_some_and(|p| p.2.as_deref() == Some(REPEATED))
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
    derive_scoped(db, false)
}

fn derive_scoped(db: &Connection, scoped: bool) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut others = db.prepare("SELECT path_digest FROM rollout_sources WHERE session_id=?1 AND path_digest<>?2 AND records>=?3 ORDER BY path_digest")?;
    let filter = if scoped { " WHERE u.session_id IN (SELECT session_id FROM accounting_selected)" } else { "" };
    let mut stmt = db.prepare(&format!("SELECT u.session_id,u.ordinal,u.path_digest,u.response_id,u.model,u.accepted,u.reason,u.input_tokens,u.cached_input_tokens,
        u.cache_write_input_tokens,u.output_tokens,u.reasoning_output_tokens,u.total_tokens,
        EXISTS(SELECT 1 FROM codex_quarantine q WHERE q.session_id=u.session_id AND q.ordinal=u.ordinal),u.payload_digest FROM codex_usage u{filter} ORDER BY u.session_id,u.ordinal"))?;
    let mut rows = stmt.query([])?;
    // Accepted `(session, response_id)` → payload digest, first ordinal first.
    let mut responses = BTreeMap::<(String, String), String>::new();
    while let Some(r) = rows.next()? {
        let (session, ordinal, first): (String, i64, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        let (accepted, reason, quarantined): (bool, Option<String>, bool) = (r.get(5)?, r.get(6)?, r.get(13)?);
        let native = [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?];
        let normalized = normalize(native);
        let (response, digest): (Option<String>, String) = (r.get(3)?, r.get(14)?);
        let (disposition, reason) = match (quarantined, accepted, normalized) {
            (true, ..) => ("conflict", Some("payload_digest_mismatch".to_owned())),
            (false, true, Some(_)) => match response.map(|response| responses.entry((session.clone(), response))) {
                // The same response, recorded again with the same payload: a replay, not usage.
                Some(std::collections::btree_map::Entry::Occupied(first)) if *first.get() == digest => ("duplicate", Some(REPEATED.to_owned())),
                Some(std::collections::btree_map::Entry::Vacant(slot)) => { slot.insert(digest); ("accepted", None) }
                _ => ("accepted", None),
            },
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
    let filter = if scoped { " AND session_id IN (SELECT session_id FROM accounting_selected)" } else { "" };
    let mut stmt = db.prepare(&format!("SELECT session_id,path_digest,records,thread_usage FROM rollout_sources WHERE thread_usage IS NOT NULL{filter} ORDER BY session_id,records,path_digest"))?;
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

/// Replay changed sessions and affected quota accounts in their original order.
/// Source triggers advance a durable frontier; projection and frontier commit
/// together. Invalidated bases replay the complete history. Counts describe the
/// whole projection, as before incremental sync.
pub fn sync(db: &mut Connection) -> Result<Value> {
    // Immediate: racing syncs (ticker and CLI) serialize on the write lock.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (sequence, watermark, invalidated): (i64, i64, Option<String>) = tx.query_row(
        "SELECT sequence,watermark,invalidated FROM accounting_stream WHERE singleton=1", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let dirty: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM accounting_dirty_sessions)", [], |r| r.get(0))?;
    let reason = invalidated.or_else(|| (watermark < 0 || watermark > sequence || (watermark != sequence && !dirty)).then(|| "watermark_inconsistent".to_owned()));
    let reason = reason.or(if synced(&tx)? { None } else { Some("ledger_missing".to_owned()) });
    let full = reason.is_some();
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS accounting_selected(session_id TEXT PRIMARY KEY); DELETE FROM accounting_selected;")?;
    if full {
        tx.execute_batch("INSERT INTO accounting_selected SELECT session_id FROM rollout_sources UNION SELECT session_id FROM codex_usage;")?;
    } else {
        tx.execute_batch("INSERT INTO accounting_selected SELECT session_id FROM accounting_dirty_sessions;")?;
        // A newly collected parent changes previously unlinked children too.
        tx.execute_batch("INSERT OR IGNORE INTO accounting_selected SELECT session_id FROM session_graph_nodes WHERE claimed_parent_session_id IN (SELECT session_id FROM accounting_dirty_sessions);")?;
    }
    let entries = derive_scoped(&tx, true)?;
    if full { tx.execute_batch("DELETE FROM usage_dispositions; DELETE FROM usage_entries;")?; }
    else { tx.execute_batch("DELETE FROM usage_dispositions WHERE entry_id IN (SELECT entry_id FROM usage_entries WHERE session_id IN (SELECT session_id FROM accounting_selected));
        DELETE FROM usage_entries WHERE session_id IN (SELECT session_id FROM accounting_selected);")?; }
    let mut counts = BTreeMap::<&str, usize>::new();
    for e in &entries {
        let n = e.normalized.map(|n| n.map(Some)).unwrap_or([None; 7]);
        let native = Value::Object(NATIVE.iter().zip(e.native).map(|(k, v)| ((*k).to_owned(), json!(v))).collect());
        tx.prepare_cached("INSERT INTO usage_entries(entry_id,source,session_id,basis,scope,normalization_version,precedence,position,response_id,model,native,
            input_tokens,cache_read_tokens,new_input_tokens,cache_write_tokens,output_tokens,reasoning_tokens,total_tokens)
            VALUES(?1,'codex',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)")?
                .execute(params![e.id, e.session, e.basis, e.scope, NORMALIZATION, e.precedence, e.position, e.response_id, e.model, native.to_string(),
                n[0], n[1], n[2], n[3], n[4], n[5], n[6]])?;
        for (path, disposition, reason) in &e.provenance {
            tx.prepare_cached("INSERT INTO usage_dispositions(entry_id,path_digest,disposition,reason) VALUES(?1,?2,?3,?4)")?
                .execute(params![e.id, path, disposition, reason])?;
            *counts.entry(disposition).or_default() += 1;
        }
    }
    let (sessions, segments) = super::graph::store_scoped(&tx, &entries, full)?;
    let windows = super::quota::store_scoped(&tx, full)?;
    tx.execute("INSERT INTO usage_ledger(singleton,normalization_version,synced_unix_ms) VALUES(1,?1,?2)
        ON CONFLICT(singleton) DO UPDATE SET normalization_version=excluded.normalization_version,synced_unix_ms=excluded.synced_unix_ms",
        params![NORMALIZATION, jiff::Timestamp::now().as_millisecond()])?;
    tx.execute("UPDATE accounting_stream SET watermark=?1,invalidated=NULL,last_mode=?2,last_reason=?3 WHERE singleton=1",
        params![sequence, if full { "full_rebuild" } else { "incremental" }, reason])?;
    tx.execute_batch("DELETE FROM accounting_dirty_sessions;")?;
    // Preserve the public sync counts: they describe the entire stored projection.
    let entry_count: i64 = tx.query_row("SELECT count(*) FROM usage_entries", [], |r| r.get(0))?;
    counts.clear();
    for row in tx.prepare("SELECT disposition,count(*) FROM usage_dispositions GROUP BY disposition")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?)))? {
        let (kind, count) = row?;
        let key = match kind.as_str() { "accepted" => "accepted", "duplicate" => "duplicate", "conflict" => "conflict", _ => "unresolved" };
        counts.insert(key, count);
    }
    tx.commit()?;
    Ok(json!({"entries": entry_count, "dispositions": counts, "sessions": sessions, "model_segments": segments, "quota_windows": windows}))
}

fn synced(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_ledger')", [], |r| r.get::<_, bool>(0))?
        && db.query_row("SELECT 1 FROM usage_ledger", [], |_| Ok(())).optional()?.is_some())
}

/// `(disposition, reason, count)`: see `open_dispositions`.
pub type OpenDisposition = (String, Option<String>, i64);

/// `(disposition, reason, count)` of the synced ledger's `conflict` and
/// `unresolved` dispositions, read-only; `None` before the first sync. The
/// counts `read` would give, without building every entry (TM4.5's
/// `accounting_conflict` rule; certificate-scale.md §5).
pub fn open_dispositions(db: &Connection) -> Result<Option<Vec<OpenDisposition>>> {
    if !synced(db)? { return Ok(None); }
    Ok(Some(db.prepare("SELECT p.disposition,p.reason,count(*) FROM usage_dispositions p JOIN usage_entries e ON e.entry_id=p.entry_id
        WHERE p.disposition IN ('conflict','unresolved') GROUP BY p.disposition,p.reason ORDER BY p.disposition,p.reason")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?))
}

/// The synced ledger as JSON, read-only; `ledger_not_synced` before the first sync.
pub fn read(db: &Connection) -> Result<Value> {
    if !synced(db)? { return Ok(super::unavailable("ledger_not_synced")); }
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

/// Invalidation is part of the caller's source/maintenance transaction.
pub(crate) fn invalidate(db: &Connection, reason: &str) -> Result<()> {
    if db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounting_stream')", [], |r| r.get::<_, bool>(0))? {
        db.execute("UPDATE accounting_stream SET invalidated=?1 WHERE singleton=1", [reason])?;
    }
    Ok(())
}

pub(crate) fn status(db: &Connection) -> Result<Option<Value>> {
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounting_stream')", [], |r| r.get::<_, bool>(0))? { return Ok(None); }
    Ok(db.query_row("SELECT sequence,watermark,invalidated,last_mode,last_reason FROM accounting_stream WHERE singleton=1 AND last_mode IS NOT NULL", [], |r|
        Ok(json!({"sequence": r.get::<_, i64>(0)?, "watermark": r.get::<_, i64>(1)?, "invalidated": r.get::<_, Option<String>>(2)?,
            "mode": r.get::<_, Option<String>>(3)?, "rebuild_reason": r.get::<_, Option<String>>(4)?}))).optional()?)
}
