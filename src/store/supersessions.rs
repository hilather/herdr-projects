//! Accepted supersession reasons (migration 0060,
//! docs/telemetry/contracts-accounting.md §10; plan doc 07 M37). The project
//! owner records, once per ended attempt, that it was superseded or abandoned
//! and why: a sibling attempt changed the same area, it duplicated a sibling's
//! effort, or another reason. Evidence is a list of typed references, never
//! content. Append-only; analytics only, never read to launch, verify,
//! integrate or complete. A worker's (or an import's) claim is refused.
use super::*;
use serde::Serialize;

pub const SUPERSESSION_SCHEMA: &str = "attempt_supersession.v1";
pub const SUPERSESSION_OUTCOMES: [&str; 2] = ["superseded", "abandoned"];
/// `sibling_changed_same_area` is overlap waste (M37); the others are explained, not overlap.
pub const SUPERSESSION_REASONS: [&str; 3] = ["sibling_changed_same_area", "duplicate_effort", "other"];
/// The only principal that records a reason: the project owner on the CLI.
const OWNER: &str = "operator:cli";
const AUTHORITY: &str = "operator_owner.v1";
/// Kinds of evidence reference: canonical ids or a commit, never text.
const EVIDENCE_KINDS: [&str; 7] = ["attempt", "task", "submission", "verified_result", "integration_operation", "commit", "candidate_group"];

/// What the owner asserts about one ended attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersessionRequest { pub attempt: String, pub outcome: String, pub reason: String, pub sibling: Option<String>, pub evidence: Vec<String> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SupersessionRecord {
    pub attempt_id: String,
    pub task_id: String,
    pub outcome: String,
    pub reason: String,
    pub sibling_attempt_id: Option<String>,
    pub evidence: Vec<String>,
    pub principal: String,
    pub authority: String,
    pub recorded_unix_ms: i64,
    /// False when the same record already existed (an idempotent repeat).
    pub recorded: bool,
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

/// `kind:value`, `kind` from [`EVIDENCE_KINDS`], `value` 1-128 of `[A-Za-z0-9._:/@-]`.
fn evidence_ref(text: &str) -> bool {
    let Some((kind, value)) = text.split_once(':') else { return false };
    EVIDENCE_KINDS.contains(&kind) && (1..=128).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'-'))
}

/// Refuse every principal but the owner: a worker (`worker:*` or any
/// attempt's id) and an import (`import:*`) can only propose.
fn owner_authority(tx: &Connection, principal: &str) -> Result<()> {
    if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid principal".into())); }
    let bare = principal.strip_prefix("worker:").unwrap_or(principal);
    if principal.starts_with("worker:") || tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [bare], |r| r.get::<_, bool>(0))? {
        return Err(invalid("a worker cannot record a supersession reason: a worker's account is a proposal".into()));
    }
    if principal.starts_with("import:") { return Err(invalid("an untrusted import cannot record a supersession reason".into())); }
    if principal != OWNER { return Err(invalid(format!("no supersession authority for {principal}: only {OWNER} (the project owner) records reasons"))); }
    Ok(())
}

fn record(r: &rusqlite::Row) -> rusqlite::Result<SupersessionRecord> {
    let evidence: String = r.get(5)?;
    Ok(SupersessionRecord { attempt_id: r.get(0)?, task_id: r.get(1)?, outcome: r.get(2)?, reason: r.get(3)?, sibling_attempt_id: r.get(4)?,
        evidence: serde_json::from_str(&evidence).unwrap_or_default(), principal: r.get(6)?, authority: r.get(7)?, recorded_unix_ms: r.get(8)?, recorded: false })
}

const COLUMNS: &str = "attempt_id,task_id,outcome,reason,sibling_attempt_id,evidence,principal,authority,recorded_unix_ms";

impl SqliteStore {
    /// Record why an ended attempt was superseded or abandoned. The same
    /// assertion again is a no-op (`recorded` false); a different one for the
    /// same attempt is refused: a record is never changed.
    pub fn record_attempt_supersession(&mut self, request: &SupersessionRequest, principal: &str, now: i64) -> Result<SupersessionRecord> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&tx)?;
        let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 60 { return Err(StoreError::UnsupportedSchema(version)); }
        owner_authority(&tx, principal)?;
        let r = request;
        if !SUPERSESSION_OUTCOMES.contains(&r.outcome.as_str()) { return Err(invalid(format!("unknown supersession outcome {}", r.outcome))); }
        if !SUPERSESSION_REASONS.contains(&r.reason.as_str()) { return Err(invalid(format!("unknown supersession reason {}", r.reason))); }
        if r.evidence.is_empty() || r.evidence.len() > 16 { return Err(invalid("a supersession reason cites 1 to 16 evidence references".into())); }
        if let Some(bad) = r.evidence.iter().find(|e| !evidence_ref(e)) {
            return Err(invalid(format!("evidence {bad:?} is not a reference `<kind>:<id>` with kind one of {}", EVIDENCE_KINDS.join(", "))));
        }
        let mut evidence = r.evidence.clone();
        evidence.sort();
        evidence.dedup();
        let (task, state): (String, String) = tx.query_row("SELECT task_id,state FROM attempts WHERE id=?1", [&r.attempt], |row| Ok((row.get(0)?, row.get(1)?))).optional()?
            .ok_or_else(|| invalid(format!("no attempt {}", r.attempt)))?;
        if !["completed", "failed", "cancelled", "lost"].contains(&state.as_str()) {
            return Err(invalid(format!("attempt {} is {state}: only an ended attempt is superseded or abandoned", r.attempt)));
        }
        match (&r.sibling, r.reason.as_str()) {
            (None, "other") => {}
            (None, reason) => return Err(invalid(format!("reason {reason} names the sibling attempt (--sibling)"))),
            (Some(sibling), _) if sibling == &r.attempt => return Err(invalid("an attempt is not its own sibling".into())),
            (Some(sibling), _) => {
                if !tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [sibling], |row| row.get::<_, bool>(0))? {
                    return Err(invalid(format!("no sibling attempt {sibling}")));
                }
            }
        }
        if let Some(existing) = tx.query_row(&format!("SELECT {COLUMNS} FROM attempt_supersessions WHERE attempt_id=?1"), [&r.attempt], record).optional()? {
            if (existing.outcome.as_str(), existing.reason.as_str(), &existing.sibling_attempt_id, &existing.evidence) == (r.outcome.as_str(), r.reason.as_str(), &r.sibling, &evidence) {
                return Ok(existing);
            }
            return Err(invalid(format!("attempt {} already has a supersession reason; records are append-only", r.attempt)));
        }
        let canonical_json = serde_json::json!({"attempt_id": r.attempt, "evidence": evidence, "outcome": r.outcome, "reason": r.reason,
            "recorded_unix_ms": now, "schema": SUPERSESSION_SCHEMA, "sibling_attempt_id": r.sibling, "task_id": task}).to_string();
        tx.execute(&format!("INSERT INTO attempt_supersessions({COLUMNS},canonical_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
            params![r.attempt, task, r.outcome, r.reason, r.sibling, serde_json::to_string(&evidence).map_err(|e| invalid(e.to_string()))?, OWNER, AUTHORITY, now, canonical_json])?;
        let mut out = tx.query_row(&format!("SELECT {COLUMNS} FROM attempt_supersessions WHERE attempt_id=?1"), [&r.attempt], record)?;
        tx.commit()?;
        out.recorded = true;
        Ok(out)
    }
}
