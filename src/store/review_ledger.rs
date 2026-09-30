//! Review lifecycle in the shared ledger (migration 0059,
//! docs/telemetry/contracts-review.md §9). A review session's start and its
//! completion each take the next `seq` of the one ordering shared with the
//! finding, fix, protocol and seed history, so [`Lifecycle`] replays review
//! status to the same watermark as triage. The owner's corrections of the
//! protocol registry (retracting a pass binding or a unit exclusion) are rows
//! of the same log. Also the seed links (§8) that make a claim an evaluation
//! artefact at a watermark, outside discovery and validation credit. Since
//! 0063 an opportunity's opening and its assignment are ledger rows too
//! (`review_opportunity_log`), so replay lists an opportunity only from its
//! opening and its assignment only from its assignment.
use super::*;
use super::finding_triage::{self, FindingEvent};
use std::collections::{BTreeMap, BTreeSet};

/// Authority of the lifecycle rows review capture records (§1: the controller's record).
pub(super) const CAPTURE_AUTHORITY: &str = "review_capture.v1";

fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)", [name], |r| r.get(0))?)
}

/// Whether migration 0059 has run.
pub(super) fn present(db: &Connection) -> Result<bool> { table(db, "review_log") }

/// Append a session's `started` or `completed` row at the next `seq`. A no-op
/// on a store before 0059, whose sessions carry no sequence.
pub(super) fn record_session_event(tx: &Connection, session: &str, event: &str, principal: &str, now: i64) -> Result<Option<i64>> {
    if !present(tx)? { return Ok(None); }
    let seq = finding_triage::head(tx)? + 1;
    tx.execute("INSERT INTO review_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,NULL,?5)", params![seq, event, principal, CAPTURE_AUTHORITY, now])?;
    tx.execute("INSERT INTO review_session_events(seq,session_id,event,backfilled) VALUES(?1,?2,?3,0)", params![seq, session, event])?;
    Ok(Some(seq))
}

/// Whether migration 0063 has run.
pub(super) fn opportunities_sequenced(db: &Connection) -> Result<bool> { table(db, "review_opportunity_log") }

/// Append an opportunity's `opened` or `assigned` row at the next `seq`, in
/// the opening's or assignment's transaction. A no-op before 0063.
pub(super) fn record_opportunity_event(tx: &Connection, opportunity: &str, event: &str, principal: &str, now: i64) -> Result<Option<i64>> {
    if !opportunities_sequenced(tx)? { return Ok(None); }
    let seq = finding_triage::head(tx)? + 1;
    tx.execute("INSERT INTO review_opportunity_log(seq,opportunity_id,event,principal,authority,recorded_unix_ms,backfilled) VALUES(?1,?2,?3,?4,?5,?6,0)",
        params![seq, opportunity, event, principal, CAPTURE_AUTHORITY, now])?;
    Ok(Some(seq))
}

/// Opportunities opened and assigned at `at`: opportunity -> ledger seq
/// (`None` when backfilled, visible at every watermark). `None` before 0063,
/// when every stored opportunity and assignment is visible.
pub(super) type OpportunityEvents = Option<(BTreeMap<String, Option<i64>>, BTreeMap<String, Option<i64>>)>;
pub(super) fn opportunity_events(db: &Connection, at: i64) -> Result<OpportunityEvents> {
    if !opportunities_sequenced(db)? { return Ok(None); }
    let (mut opened, mut assigned) = (BTreeMap::new(), BTreeMap::new());
    for row in db.prepare("SELECT opportunity_id,event,CASE WHEN backfilled=1 THEN NULL ELSE seq END FROM review_opportunity_log WHERE seq<=?1 OR backfilled=1")?
        .query_map([at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)))? {
        let (opportunity, event, seq) = row?;
        if event == "opened" { opened.insert(opportunity, seq); } else { assigned.insert(opportunity, seq); }
    }
    Ok(Some((opened, assigned)))
}

/// Append the owner's correction row (`pass_retracted`, `exclusion_retracted`)
/// after the authority and expected-head checks, and record what it reverses.
pub(super) fn record_retraction(tx: &Connection, kind: &str, reverses: i64, principal: &str, expected: Option<i64>, now: i64) -> Result<FindingEvent> {
    finding_triage::triage_authority(tx, principal)?;
    if !present(tx)? { return Err(StoreError::UnsupportedSchema(tx.query_row("PRAGMA user_version", [], |r| r.get(0))?)); }
    let head = finding_triage::head(tx)?;
    if let Some(expected) = expected && head != expected { return Err(StoreError::Invalid(format!("finding history moved: head is {head}, expected {expected}"))); }
    let seq = head + 1;
    tx.execute("INSERT INTO review_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![seq, kind, principal, finding_triage::TRIAGE_AUTHORITY, expected, now])?;
    tx.execute("INSERT INTO protocol_retractions(seq,reverses) VALUES(?1,?2)", params![seq, reverses])?;
    Ok(tx.query_row("SELECT kind,principal,authority,expected_seq,recorded_unix_ms FROM review_log WHERE seq=?1", [seq],
        |r| Ok(FindingEvent { seq, kind: r.get(0)?, principal: r.get(1)?, authority: r.get(2)?, expected_seq: r.get(3)?, recorded_unix_ms: r.get(4)?,
            subject: serde_json::json!({"reverses": reverses}) }))?)
}

/// Protocol-log rows reversed by a retraction recorded by `at`: reversed seq -> retraction seq.
pub(super) fn protocol_retractions(db: &Connection, at: i64) -> Result<BTreeMap<i64, i64>> {
    if !table(db, "protocol_retractions")? { return Ok(BTreeMap::new()); }
    Ok(db.prepare("SELECT reverses,seq FROM protocol_retractions WHERE seq<=?1")?.query_map([at], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Review sessions started and completed at a watermark. Before 0059 (no
/// sequence) every stored session and completion is visible; backfilled rows
/// (recorded before 0059) are visible at every watermark. Likewise
/// opportunities and assignments with 0063.
pub(super) struct Lifecycle { visible: Option<(BTreeSet<String>, BTreeSet<String>)>, opportunities: OpportunityEvents }

impl Lifecycle {
    pub(super) fn at(db: &Connection, at: Option<i64>) -> Result<Self> {
        let opportunities = opportunity_events(db, at.unwrap_or(i64::MAX))?;
        if !present(db)? { return Ok(Self { visible: None, opportunities }); }
        let (mut started, mut completed) = (BTreeSet::new(), BTreeSet::new());
        for row in db.prepare("SELECT session_id,event FROM review_session_events WHERE seq<=?1 OR backfilled=1")?
            .query_map([at.unwrap_or(i64::MAX)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (session, event) = row?;
            if event == "started" { started.insert(session); } else { completed.insert(session); }
        }
        Ok(Self { visible: Some((started, completed)), opportunities })
    }
    /// Whether the opportunity is opened at the watermark.
    pub(super) fn opened(&self, opportunity: &str) -> bool { self.opportunities.as_ref().is_none_or(|o| o.0.contains_key(opportunity)) }
    /// Whether its assignment is recorded at the watermark (it must also be stored).
    pub(super) fn assignment_visible(&self, opportunity: &str) -> bool { self.opportunities.as_ref().is_none_or(|o| o.1.contains_key(opportunity)) }
    fn started(&self, session: &str) -> bool { self.visible.as_ref().is_none_or(|v| v.0.contains(session)) }
    fn completed(&self, session: &str) -> bool { self.visible.as_ref().is_none_or(|v| v.1.contains(session)) }
}

/// An opportunity's review status (§4) at the lifecycle's watermark, and its
/// completed session. The assignment replays with its ledger row (0063).
pub(super) fn opportunity_status(db: &Connection, opportunity: &str, lifecycle: &Lifecycle) -> Result<(&'static str, Option<String>)> {
    let assigned: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM review_assignments WHERE opportunity_id=?1)", [opportunity], |r| r.get(0))?
        && lifecycle.assignment_visible(opportunity);
    if !assigned { return Ok(("unassigned", None)); }
    let rows: Vec<(String, Option<String>)> = db.prepare("SELECT r.session_id,c.outcome FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=?1 ORDER BY r.ordinal")?
        .query_map([opportunity], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let sessions: Vec<(String, Option<String>)> = rows.into_iter().filter(|s| lifecycle.started(&s.0))
        .map(|(session, outcome)| { let outcome = outcome.filter(|_| lifecycle.completed(&session)); (session, outcome) }).collect();
    if sessions.is_empty() { return Ok(("no_session", None)); }
    if let Some(done) = sessions.iter().find(|s| s.1.as_deref() == Some("completed")) { return Ok(("completed", Some(done.0.clone()))); }
    Ok((if sessions.iter().any(|s| s.1.is_none()) { "in_progress" } else { "ended_without_completion" }, None))
}

/// Claims linked to a seed at `at`: a detection (§8) recorded by then and not
/// retracted by then. They are evaluation artefacts, never discovery or
/// validation credit (M21, M22, M23).
pub(super) fn seed_linked_claims(db: &Connection, at: i64) -> Result<BTreeSet<i64>> {
    if !table(db, "seed_detections")? { return Ok(BTreeSet::new()); }
    Ok(db.prepare("SELECT d.claim_id FROM seed_detections d WHERE d.seq<=?1 AND NOT EXISTS(SELECT 1 FROM seed_retractions r WHERE r.reverses=d.seq AND r.seq<=?1)")?
        .query_map([at], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}
