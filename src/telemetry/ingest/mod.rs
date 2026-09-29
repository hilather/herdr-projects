//! Ingest ledger (TM1.2, contracts-collection.md A2): sanitized source
//! envelopes (doc 03 §1 subset), their quarantine, per-source cursors and
//! coverage gaps, all in sidecar stream `ingest`. Written by an adapter inside
//! its own sidecar transaction, so envelopes and adapter rows commit together.
use super::sanitize;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Wire envelope version (independent of the sidecar schema).
const SCHEMA_VERSION: i64 = 1;
/// Normalized envelope ceiling, framing included (doc 03 §1).
const MAX_ENVELOPE: usize = 64 << 10;

/// `measurement.normalization_version` of a Codex kind's envelope: raised when
/// its allowlist grows (A4: `session_meta` and `token_count`), so an envelope
/// written under the smaller allowlist is superseded, not a digest conflict.
fn normalization_version(kind: &str) -> i64 {
    match kind {
        "session_meta" | "token_count" => 2,
        _ => 1,
    }
}

/// Canonical JSON (contracts §0): sorted keys, compact, integers only after sanitizing.
fn canonical(value: &Value) -> String {
    value.to_string()
}

fn digest(text: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

/// One source's envelope writer for a single pass.
pub struct Ledger {
    source: String,
    start: u64,
    /// No cursor existed before this pass.
    pub fresh: bool,
}

/// The source record a position holds, as far as the adapter knows it.
pub struct Record<'a> {
    pub kind: &'a str,
    pub payload: &'a Value,
    pub occurred_unix_ms: Option<i64>,
    pub session: &'a str,
    pub adapter_version: &'a str,
}

impl Ledger {
    /// Start a pass over `source` (a path digest) at byte `start`. A source read
    /// before this stream existed has no envelopes for `[0, start)`: that range
    /// is a `predates_ingest` gap.
    pub fn begin(db: &Connection, source: &str, start: u64, now: i64) -> Result<Self> {
        let fresh = db.query_row("SELECT 1 FROM source_cursors WHERE source=?1", [source], |_| Ok(())).optional()?.is_none();
        if fresh && start > 0 {
            gap(db, source, 0, start, "predates_ingest", now)?;
        }
        Ok(Self { source: source.to_owned(), start, fresh })
    }

    /// The envelope of the Codex record at byte `sequence`, if its kind is
    /// allowlisted. Same identity and digest: no-op; another digest or an
    /// envelope over 64 KiB: quarantined, nothing stored.
    pub fn observe(&self, db: &Connection, sequence: u64, record: Record, now: i64) -> Result<()> {
        let Some(fields) = sanitize::codex_allowlist(record.kind) else { return Ok(()) };
        let payload = sanitize::payload(&fields, record.payload);
        let payload_text = canonical(&payload);
        let payload_digest = digest(&payload_text);
        let event_id = format!("codex:{}:{sequence}", self.source);
        let producer = format!("codex:{}", record.session);
        let kind = format!("codex.{}.v1", record.kind);
        let identity = canonical(&json!({"session_id": record.session}));
        let provenance = canonical(&json!({"adapter": "codex", "adapter_version": record.adapter_version, "interface": "rollout_jsonl", "source_trust": "collector_observed"}));
        let version = normalization_version(record.kind);
        let measurement = canonical(&json!({"measurement_basis": "reported", "coverage": "complete", "normalization_version": version}));
        let envelope = json!({"schema_version": SCHEMA_VERSION, "event_id": event_id, "producer_id": producer, "producer_epoch": self.source,
            "producer_sequence": sequence, "idempotency_key": event_id, "event_kind": kind, "occurred_unix_ms": record.occurred_unix_ms,
            "observed_unix_ms": now, "identity": identity, "provenance": provenance, "measurement": measurement, "payload": payload, "payload_digest": payload_digest});
        let bytes = canonical(&envelope).len();
        if bytes > MAX_ENVELOPE {
            return quarantine(db, &self.source, sequence, "envelope_oversized", bytes as u64, Some(&event_id), None, Some(&payload_digest), now);
        }
        let first: Option<(String, i64)> = db.query_row("SELECT payload_digest,coalesce(json_extract(measurement,'$.normalization_version'),1) FROM source_observations WHERE event_id=?1",
            [&event_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        match first {
            Some((first, _)) if first == payload_digest => Ok(()),
            // Written under an older allowlist of its kind: superseded in place.
            Some((_, older)) if older < version => {
                db.execute("UPDATE source_observations SET event_kind=?2,occurred_unix_ms=?3,observed_unix_ms=?4,identity=?5,provenance=?6,measurement=?7,payload=?8,
                    payload_digest=?9,envelope_bytes=?10 WHERE event_id=?1",
                    params![event_id, kind, record.occurred_unix_ms, now, identity, provenance, measurement, payload_text, payload_digest, bytes as i64])?;
                Ok(())
            }
            Some((first, _)) => quarantine(db, &self.source, sequence, "digest_conflict", bytes as u64, Some(&event_id), Some(&first), Some(&payload_digest), now),
            None => {
                db.execute("INSERT INTO source_observations(event_id,schema_version,producer_id,producer_epoch,producer_sequence,event_kind,occurred_unix_ms,
                    observed_unix_ms,identity,provenance,measurement,payload,payload_digest,envelope_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![event_id, SCHEMA_VERSION, producer, self.source, sequence as i64, kind, record.occurred_unix_ms, now, identity, provenance,
                        measurement, payload_text, payload_digest, bytes as i64])?;
                Ok(())
            }
        }
    }

    /// A line skipped whole for exceeding the adapter's parse limit.
    pub fn oversized_line(&self, db: &Connection, sequence: u64, bytes: u64, now: i64) -> Result<()> {
        quarantine(db, &self.source, sequence, "line_oversized", bytes, None, None, None, now)
    }

    /// A complete line that yields no observation because it does not parse:
    /// `line_malformed` (not a JSON object with a string `type`) or
    /// `record_malformed` (a read kind whose typed fields do not parse).
    /// Nothing of the line is kept.
    pub fn malformed(&self, db: &Connection, sequence: u64, reason: &'static str, bytes: u64, now: i64) -> Result<()> {
        quarantine(db, &self.source, sequence, reason, bytes, None, None, None, now)
    }

    /// Advance the cursor to `offset`; gaps inside the range read this pass are recovered.
    pub fn finish(&self, db: &Connection, producer: Option<&str>, offset: u64, now: i64) -> Result<()> {
        db.execute("INSERT INTO source_cursors(source,producer_id,byte_offset,observations,updated_unix_ms)
            VALUES(?1,?2,?3,(SELECT count(*) FROM source_observations WHERE producer_epoch=?1),?4)
            ON CONFLICT(source) DO UPDATE SET producer_id=excluded.producer_id,byte_offset=excluded.byte_offset,observations=excluded.observations,updated_unix_ms=excluded.updated_unix_ms",
            params![self.source, producer.map(|p| format!("codex:{p}")), offset as i64, now])?;
        db.execute("UPDATE coverage_gaps SET recovery='recovered',observed_unix_ms=?4 WHERE source=?1 AND recovery='pending' AND start_offset>=?2 AND end_offset<=?3",
            params![self.source, self.start as i64, offset as i64, now])?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn quarantine(db: &Connection, source: &str, sequence: u64, reason: &str, bytes: u64, event: Option<&str>, first: Option<&str>, new: Option<&str>, now: i64) -> Result<()> {
    db.execute("INSERT OR IGNORE INTO ingest_quarantine(source,sequence,reason,bytes,event_id,first_digest,new_digest,observed_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![source, sequence as i64, reason, bytes as i64, event, first, new, now])?;
    Ok(())
}

/// Record `[start, end)` of `source` as not ingested (`pending` until a pass reads it).
pub fn gap(db: &Connection, source: &str, start: u64, end: u64, reason: &str, now: i64) -> Result<()> {
    db.execute("INSERT INTO coverage_gaps(source,start_offset,end_offset,reason,recovery,observed_unix_ms) VALUES(?1,?2,?3,?4,'pending',?5)
        ON CONFLICT(source,start_offset,reason) DO UPDATE SET end_offset=max(end_offset,excluded.end_offset),recovery='pending',observed_unix_ms=excluded.observed_unix_ms",
        params![source, start as i64, end as i64, reason, now])?;
    Ok(())
}

/// Whether a collect error means the sidecar cannot take writes (full disk,
/// I/O failure, read-only file): the pass becomes a coverage gap, not a failure.
pub fn unwritable(error: &anyhow::Error) -> bool {
    use rusqlite::ErrorCode::{DiskFull, ReadOnly, SystemIoFailure};
    error.downcast_ref::<rusqlite::Error>().and_then(rusqlite::Error::sqlite_error_code).is_some_and(|code| matches!(code, DiskFull | ReadOnly | SystemIoFailure))
}
