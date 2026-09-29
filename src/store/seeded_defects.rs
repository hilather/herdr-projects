//! Seeded-defect recall evaluation (migration 0058,
//! docs/telemetry/contracts-review.md §8, plan TM3.6). The evaluation authority
//! registers a result submission as a seeded candidate (seed class and a
//! content-addressed reproducer reference per seed, never the seed itself) or
//! as a clean control, before any review of it. A seed counts as detected only
//! through the owner's link of a triaged claim of a review of that candidate
//! (a claim whose current decision is `validated` or `duplicate`); every other
//! finding on it is an ordinary finding. A candidate is revealed only after
//! every review of it has ended, and is never reviewed afterwards. A seeded
//! candidate never integrates ([`refuse_seeded_integration`], the
//! `integration_jobs` eligibility predicate and the migration's triggers), and
//! its verified result never satisfies a dependent's `verified_result` edge
//! (`satisfaction.rs` `verified_counts`). `seed_log` shares the finding and fix
//! history's one ordering, so [`seed_state`] replays detections against triage
//! to one watermark. Reviewer-facing views never read these tables.
use super::*;
use super::finding_triage::{self, FindingState};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The only evaluation principal: the project owner at the CLI.
pub const EVALUATION_PRINCIPAL: &str = "operator:cli";
/// Authority recorded on every `seed_log` row.
pub const EVALUATION_AUTHORITY: &str = "evaluation_owner.v1";
pub const REVEAL_POLICY: &str = "reveal_after_close.v1";
/// Plan doc 06 §6a seed classes.
pub const SEED_CLASSES: [&str; 6] = ["logic", "boundary", "concurrency", "security", "test_weakening", "requirement_omission"];
pub const DISPOSITIONS: [&str; 2] = ["discarded", "repaired"];
const MAX_SEEDS: usize = 16;
const MAX_EVIDENCE: usize = 64;

/// Whether the seeded-candidate registry's migration has run.
pub(super) fn registry_present(tx: &Connection) -> Result<bool> {
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seeded_candidates')", [], |r| r.get(0))?)
}

/// Refuse a new review of a revealed evaluation candidate (the migration's triggers repeat it).
pub(super) fn refuse_review_after_reveal(tx: &Connection, submission: &str) -> Result<()> {
    if registry_present(tx)? && tx.query_row("SELECT EXISTS(SELECT 1 FROM seed_reveals WHERE submission_id=?1)", [submission], |r| r.get::<_, bool>(0))? {
        return Err(StoreError::Invalid("a revealed evaluation candidate is not reviewed again".into()));
    }
    Ok(())
}

/// Whether verified result `result_id` is of a seeded candidate.
pub(super) fn seeded_result(tx: &Connection, result_id: &str) -> Result<bool> {
    if !registry_present(tx)? { return Ok(false); }
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM verified_results r JOIN seeded_candidates x ON x.submission_id=r.submission_id WHERE r.result_id=?1 AND x.arm='seeded')",
        [result_id], |r| r.get(0))?)
}

/// Whether submission `submission` is a seeded candidate.
pub(super) fn seeded_submission(tx: &Connection, submission: &str) -> Result<bool> {
    if !registry_present(tx)? { return Ok(false); }
    Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM seeded_candidates WHERE submission_id=?1 AND arm='seeded')", [submission], |r| r.get(0))?)
}

/// Refuse to begin integrating a verified result of a seeded candidate.
pub(super) fn refuse_seeded_integration(tx: &Connection, result_id: &str) -> Result<()> {
    if seeded_result(tx, result_id)? { return Err(StoreError::Invalid("a seeded candidate never integrates".into())); }
    Ok(())
}

/// One seed of a seeded candidate: its class and the `sha256:` reference of
/// the minimal reproducer held elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedSpec { pub seed_class: String, pub reproducer_ref: String }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvaluationArm { Seeded(Vec<SeedSpec>), CleanControl }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedDetection {
    pub seq: i64,
    pub claim_id: i64,
    pub opportunity_id: String,
    pub evidence_refs: Vec<String>,
    pub retracted_seq: Option<i64>,
    /// Unretracted and the claim's current decision is `validated` or `duplicate` at the watermark.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedRecord {
    pub seed_id: i64,
    pub ordinal: i64,
    pub seed_class: String,
    pub reproducer_ref: String,
    pub detections: Vec<SeedDetection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvaluationCandidate {
    pub seq: i64,
    pub submission_id: String,
    pub candidate_oid: String,
    /// `seeded` or `clean_control`.
    pub arm: String,
    pub reveal_policy: String,
    pub registered_unix_ms: i64,
    pub revealed_seq: Option<i64>,
    pub disposal: Option<String>,
    pub disposed_seq: Option<i64>,
    pub seeds: Vec<SeedRecord>,
}

/// One review opportunity of an evaluation candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvaluationOpportunity {
    pub opportunity_id: String,
    pub submission_id: String,
    pub arm: String,
    pub kind: String,
    pub protocol: String,
    /// The assigned reviewer configuration (null when unassigned).
    pub configuration_id: Option<String>,
    pub assigned_unix_ms: Option<i64>,
    /// Some session completed.
    pub completed: bool,
    /// Finding submissions of its sessions at the watermark, by outcome.
    pub submissions: BTreeMap<String, usize>,
}

/// M43 trial: one seed on one completed opportunity of its candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedTrial {
    pub seed_id: i64,
    pub seed_class: String,
    pub opportunity_id: String,
    /// `detected`, `missed`, or `pending` (not detected while a submission is untriaged).
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeedState {
    pub head_seq: i64,
    pub as_of_seq: i64,
    pub candidates: Vec<EvaluationCandidate>,
    pub opportunities: Vec<EvaluationOpportunity>,
    pub trials: Vec<SeedTrial>,
    /// Every `seed_log` row up to the watermark.
    pub history: Vec<finding_triage::FindingEvent>,
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_seeded(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    if !registry_present(tx)? { return Err(StoreError::UnsupportedSchema(tx.query_row("PRAGMA user_version", [], |r| r.get(0))?)); }
    Ok(())
}

/// Refuse every principal but the evaluation authority: a reviewer, the
/// implementing worker, any other attempt or an import never sees or sets seeds.
fn evaluation_authority(tx: &Connection, principal: &str) -> Result<()> {
    if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid principal".into())); }
    let bare = principal.strip_prefix("worker:").unwrap_or(principal);
    if principal.starts_with("worker:") || tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [bare], |r| r.get::<_, bool>(0))? {
        return Err(invalid("a worker cannot register, detect or reveal seeds: only the evaluation authority can".into()));
    }
    if principal.starts_with("import:") { return Err(invalid("an import cannot register, detect or reveal seeds".into())); }
    if principal != EVALUATION_PRINCIPAL {
        return Err(invalid(format!("no evaluation authority for {principal}: only {EVALUATION_PRINCIPAL} (the project owner) is the evaluation authority")));
    }
    Ok(())
}

fn hex64(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }
fn evidence_ref(value: &str) -> bool { value.strip_prefix("sha256:").or_else(|| value.strip_prefix("verification_run:")).is_some_and(hex64) }

fn evidence(values: &[String]) -> Result<Vec<String>> {
    if values.is_empty() { return Err(invalid("a detection needs at least one evidence reference".into())); }
    if values.len() > MAX_EVIDENCE { return Err(invalid(format!("at most {MAX_EVIDENCE} evidence references"))); }
    let sorted: BTreeSet<&String> = values.iter().collect();
    if sorted.len() != values.len() { return Err(invalid("duplicate evidence reference".into())); }
    if let Some(bad) = sorted.iter().position(|v| !evidence_ref(v)) { return Err(invalid(format!("evidence reference {} is not an allowed reference", bad + 1))); }
    Ok(sorted.into_iter().cloned().collect())
}

/// Append the evaluation authority's row after the authority and expected-head checks.
fn log(tx: &Connection, kind: &str, principal: &str, expected: Option<i64>, now: i64) -> Result<i64> {
    evaluation_authority(tx, principal)?;
    let head = finding_triage::head(tx)?;
    if let Some(expected) = expected && head != expected { return Err(invalid(format!("finding history moved: head is {head}, expected {expected}"))); }
    let seq = head + 1;
    tx.execute("INSERT INTO seed_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
        params![seq, kind, principal, EVALUATION_AUTHORITY, expected, now])?;
    Ok(seq)
}

fn done(tx: rusqlite::Transaction<'_>, seq: i64, subject: serde_json::Value) -> Result<finding_triage::FindingEvent> {
    let out = tx.query_row("SELECT kind,principal,authority,expected_seq,recorded_unix_ms FROM seed_log WHERE seq=?1", [seq],
        |r| Ok(finding_triage::FindingEvent { seq, kind: r.get(0)?, principal: r.get(1)?, authority: r.get(2)?, expected_seq: r.get(3)?, recorded_unix_ms: r.get(4)?, subject }))?;
    tx.commit()?;
    Ok(out)
}

/// `(arm, revealed)` of a registered submission.
fn registration(tx: &Connection, submission: &str) -> Result<(String, bool)> {
    tx.query_row("SELECT c.arm,EXISTS(SELECT 1 FROM seed_reveals v WHERE v.submission_id=c.submission_id) FROM seeded_candidates c WHERE c.submission_id=?1",
        [submission], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
        .ok_or_else(|| invalid(format!("submission {submission} is not registered for evaluation")))
}

impl SqliteStore {
    /// Register `submission`'s exact candidate as a seeded candidate (with 1–16
    /// seeds) or a clean control. Only before any review opportunity on it and
    /// before any integration job or operation names it.
    pub fn register_evaluation_candidate(&mut self, submission: &str, arm: &EvaluationArm, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<finding_triage::FindingEvent> {
        let seeds: &[SeedSpec] = match arm { EvaluationArm::Seeded(seeds) => seeds, EvaluationArm::CleanControl => &[] };
        if matches!(arm, EvaluationArm::Seeded(_)) && (seeds.is_empty() || seeds.len() > MAX_SEEDS) { return Err(invalid(format!("a seeded candidate has 1 to {MAX_SEEDS} seeds"))); }
        for seed in seeds {
            if !SEED_CLASSES.contains(&seed.seed_class.as_str()) { return Err(invalid(format!("unknown seed class {}", seed.seed_class))); }
            if !seed.reproducer_ref.strip_prefix("sha256:").is_some_and(hex64) {
                return Err(invalid("a reproducer is a sha256:<hex64> reference to content held elsewhere, never the seed itself".into()));
            }
        }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_seeded(&tx)?;
        evaluation_authority(&tx, principal)?;
        let candidate: String = tx.query_row("SELECT candidate_oid FROM result_submissions WHERE submission_id=?1", [submission], |r| r.get(0)).optional()?
            .ok_or_else(|| invalid(format!("no result submission {submission}")))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM seeded_candidates WHERE submission_id=?1)", [submission], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("submission {submission} is already registered for evaluation")));
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_opportunities WHERE submission_id=?1)", [submission], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("submission {submission} already has a review opportunity: an evaluation arm is registered before any review")));
        }
        let integrating: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations o WHERE o.kind='integration.run' AND json_extract(o.payload,'$.submission_id')=?1)
            OR EXISTS(SELECT 1 FROM verified_results r JOIN integration_operations i ON i.verified_result_id=r.result_id WHERE r.submission_id=?1)", [submission], |r| r.get(0))?;
        if integrating { return Err(invalid(format!("submission {submission} already has an integration job or operation"))); }
        let arm_name = if seeds.is_empty() { "clean_control" } else { "seeded" };
        let seq = log(&tx, "registered", principal, expected_seq, now)?;
        tx.execute("INSERT INTO seeded_candidates(seq,submission_id,candidate_oid,arm,reveal_policy) VALUES(?1,?2,?3,?4,?5)", params![seq, submission, candidate, arm_name, REVEAL_POLICY])?;
        let mut ids = Vec::with_capacity(seeds.len());
        for (i, seed) in seeds.iter().enumerate() {
            tx.execute("INSERT INTO seeded_defects(seq,ordinal,seed_class,reproducer_ref) VALUES(?1,?2,?3,?4)", params![seq, i as i64 + 1, seed.seed_class, seed.reproducer_ref])?;
            ids.push(tx.last_insert_rowid());
        }
        // The pending projection may hold it; the producer's recheck drops it.
        done(tx, seq, serde_json::json!({"submission_id": submission, "candidate_oid": candidate, "arm": arm_name, "reveal_policy": REVEAL_POLICY, "seeds": ids}))
    }

    /// Link one triaged claim of a review of the seed's own candidate to the
    /// seed: the only way a seed counts as detected. The claim's current
    /// decision must be `validated` or `duplicate` (an accepted finding).
    pub fn record_seed_detection(&mut self, seed: i64, claim: i64, evidence_refs: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<finding_triage::FindingEvent> {
        let evidence = evidence(evidence_refs)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_seeded(&tx)?;
        evaluation_authority(&tx, principal)?;
        let submission: String = tx.query_row("SELECT c.submission_id FROM seeded_defects d JOIN seeded_candidates c ON c.seq=d.seq WHERE d.seed_id=?1", [seed], |r| r.get(0)).optional()?
            .ok_or_else(|| invalid(format!("no seed {seed}")))?;
        let state = finding_triage::finding_state(&tx, None)?.ok_or(StoreError::UnsupportedSchema(54))?;
        let (sub, current) = state.submissions.iter().find_map(|s| s.claims.iter().find(|c| c.claim_id == claim).map(|c| (s, c)))
            .ok_or_else(|| invalid(format!("claim {claim} is not in a current claim set")))?;
        let reviewed: String = tx.query_row("SELECT submission_id FROM review_opportunities WHERE opportunity_id=?1", [&sub.opportunity_id], |r| r.get(0))?;
        if reviewed != submission { return Err(invalid(format!("claim {claim} is from a review of another candidate: a seed is detected only on its own candidate"))); }
        if !["validated", "duplicate"].contains(&current.decided.as_str()) {
            return Err(invalid(format!("claim {claim} is {}: detection needs an accepted triage decision (validated or duplicate)", current.decided)));
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM seed_detections d WHERE d.claim_id=?1 AND NOT EXISTS(SELECT 1 FROM seed_retractions r WHERE r.reverses=d.seq))", [claim], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("claim {claim} already detects a seed; retract that detection first")));
        }
        let seq = log(&tx, "detected", principal, expected_seq, now)?;
        tx.execute("INSERT INTO seed_detections(seq,seed_id,claim_id,evidence_refs) VALUES(?1,?2,?3,?4)", params![seq, seed, claim, serde_json::json!(evidence).to_string()])?;
        done(tx, seq, serde_json::json!({"seed_id": seed, "claim_id": claim, "opportunity_id": sub.opportunity_id, "evidence_refs": evidence}))
    }

    /// Reverse one detection recorded in error.
    pub fn retract_seed_detection(&mut self, detection: i64, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<finding_triage::FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_seeded(&tx)?;
        evaluation_authority(&tx, principal)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM seed_detections WHERE seq=?1)", [detection], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("no seed detection at seq {detection}"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM seed_retractions WHERE reverses=?1)", [detection], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("the detection at seq {detection} is already retracted"))); }
        let seq = log(&tx, "retracted", principal, expected_seq, now)?;
        tx.execute("INSERT INTO seed_retractions(seq,reverses) VALUES(?1,?2)", params![seq, detection])?;
        done(tx, seq, serde_json::json!({"reverses": detection}))
    }

    /// Reveal an evaluation candidate once every review of it has ended (each
    /// opportunity has sessions and each session a completion). Nothing reviews it afterwards.
    pub fn reveal_evaluation_candidate(&mut self, submission: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<finding_triage::FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_seeded(&tx)?;
        evaluation_authority(&tx, principal)?;
        let (arm, revealed) = registration(&tx, submission)?;
        if revealed { return Err(invalid(format!("submission {submission} is already revealed"))); }
        let (opportunities, open): (i64, i64) = tx.query_row("SELECT count(*), coalesce(sum(NOT EXISTS(SELECT 1 FROM review_sessions r JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=o.opportunity_id)
                OR EXISTS(SELECT 1 FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=o.opportunity_id AND c.session_id IS NULL)),0)
            FROM review_opportunities o WHERE o.submission_id=?1", [submission], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if opportunities == 0 || open > 0 { return Err(invalid(format!("submission {submission} has a review that has not ended: a seed is revealed only after every review of its candidate has ended"))); }
        let seq = log(&tx, "revealed", principal, expected_seq, now)?;
        tx.execute("INSERT INTO seed_reveals(seq,submission_id) VALUES(?1,?2)", params![seq, submission])?;
        done(tx, seq, serde_json::json!({"submission_id": submission, "arm": arm}))
    }

    /// Record that a revealed candidate was discarded or repaired (as another
    /// submission). Either way the seeded submission itself never integrates.
    pub fn dispose_evaluation_candidate(&mut self, submission: &str, disposition: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<finding_triage::FindingEvent> {
        if !DISPOSITIONS.contains(&disposition) { return Err(invalid(format!("unknown disposition {disposition}"))); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_seeded(&tx)?;
        evaluation_authority(&tx, principal)?;
        let (_, revealed) = registration(&tx, submission)?;
        if !revealed { return Err(invalid(format!("submission {submission} is not revealed yet"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM seed_disposals WHERE submission_id=?1)", [submission], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("submission {submission} is already disposed"))); }
        let seq = log(&tx, "disposed", principal, expected_seq, now)?;
        tx.execute("INSERT INTO seed_disposals(seq,submission_id,disposition) VALUES(?1,?2,?3)", params![seq, submission, disposition])?;
        done(tx, seq, serde_json::json!({"submission_id": submission, "disposition": disposition}))
    }
}

/// Replay the seed registry and detections, with finding triage, up to `as_of`
/// (default: the head) on any connection, read-only. `None` before migration 0058.
pub fn seed_state(db: &Connection, as_of: Option<i64>) -> Result<Option<SeedState>> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seed_log')", [], |r| r.get(0))?;
    if !present { return Ok(None); }
    let Some(triage) = finding_triage::finding_state(db, as_of)? else { return Ok(None) };
    let at = triage.as_of_seq;
    // Current claims at the watermark: claim -> (opportunity, decided).
    let claims: BTreeMap<i64, (&str, &str)> = triage.submissions.iter()
        .flat_map(|s| s.claims.iter().map(move |c| (c.claim_id, (s.opportunity_id.as_str(), c.decided.as_str())))).collect();
    let retracted: BTreeMap<i64, i64> = db.prepare("SELECT reverses,seq FROM seed_retractions WHERE seq<=?1")?.query_map([at], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;

    type CandidateRow = (i64, String, String, String, String, i64);
    let rows: Vec<CandidateRow> = db.prepare("SELECT c.seq,c.submission_id,c.candidate_oid,c.arm,c.reveal_policy,l.recorded_unix_ms FROM seeded_candidates c JOIN seed_log l ON l.seq=c.seq WHERE c.seq<=?1 ORDER BY c.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut candidates = Vec::with_capacity(rows.len());
    for (seq, submission_id, candidate_oid, arm, reveal_policy, registered_unix_ms) in rows {
        let revealed_seq: Option<i64> = db.query_row("SELECT seq FROM seed_reveals WHERE submission_id=?1 AND seq<=?2", params![submission_id, at], |r| r.get(0)).optional()?;
        let disposal: Option<(i64, String)> = db.query_row("SELECT seq,disposition FROM seed_disposals WHERE submission_id=?1 AND seq<=?2", params![submission_id, at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let seed_rows: Vec<(i64, i64, String, String)> = db.prepare("SELECT seed_id,ordinal,seed_class,reproducer_ref FROM seeded_defects WHERE seq=?1 ORDER BY ordinal")?
            .query_map([seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut seeds = Vec::with_capacity(seed_rows.len());
        for (seed_id, ordinal, seed_class, reproducer_ref) in seed_rows {
            let links: Vec<(i64, i64, String, String)> = db.prepare("SELECT d.seq,d.claim_id,o.opportunity_id,d.evidence_refs FROM seed_detections d
                JOIN finding_claims k ON k.claim_id=d.claim_id JOIN finding_submissions f ON f.submission_id=k.submission_id
                JOIN review_sessions r ON r.session_id=f.session_id JOIN review_opportunities o ON o.opportunity_id=r.opportunity_id
                WHERE d.seed_id=?1 AND d.seq<=?2 ORDER BY d.seq")?
                .query_map(params![seed_id, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            let detections = links.into_iter().map(|(dseq, claim_id, opportunity_id, evidence)| {
                let retracted_seq = retracted.get(&dseq).copied();
                let accepted = claims.get(&claim_id).is_some_and(|c| c.1 == "validated" || c.1 == "duplicate");
                SeedDetection { seq: dseq, claim_id, opportunity_id, evidence_refs: serde_json::from_str(&evidence).unwrap_or_default(), retracted_seq, active: retracted_seq.is_none() && accepted }
            }).collect();
            seeds.push(SeedRecord { seed_id, ordinal, seed_class, reproducer_ref, detections });
        }
        candidates.push(EvaluationCandidate { seq, submission_id, candidate_oid, arm, reveal_policy, registered_unix_ms, revealed_seq,
            disposed_seq: disposal.as_ref().map(|d| d.0), disposal: disposal.map(|d| d.1), seeds });
    }

    let opportunities = evaluation_opportunities(db, &candidates, &triage, &super::review_ledger::Lifecycle::at(db, Some(at))?)?;
    let mut trials = Vec::new();
    for c in candidates.iter().filter(|c| c.arm == "seeded") {
        for o in opportunities.iter().filter(|o| o.completed && o.submission_id == c.submission_id) {
            for seed in &c.seeds {
                let detected = seed.detections.iter().any(|d| d.active && d.opportunity_id == o.opportunity_id);
                let status = if detected { "detected" } else if o.submissions.get("pending").is_some_and(|n| *n > 0) { "pending" } else { "missed" };
                trials.push(SeedTrial { seed_id: seed.seed_id, seed_class: seed.seed_class.clone(), opportunity_id: o.opportunity_id.clone(), status: status.into() });
            }
        }
    }
    let history: Vec<finding_triage::FindingEvent> = db.prepare("SELECT seq,kind,principal,authority,expected_seq,recorded_unix_ms FROM seed_log WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok(finding_triage::FindingEvent { seq: r.get(0)?, kind: r.get(1)?, principal: r.get(2)?, authority: r.get(3)?, expected_seq: r.get(4)?,
            recorded_unix_ms: r.get(5)?, subject: serde_json::Value::Null }))?.collect::<rusqlite::Result<_>>()?;
    Ok(Some(SeedState { head_seq: triage.head_seq, as_of_seq: at, candidates, opportunities, trials, history }))
}

/// Review opportunities of registered candidates, with their assigned
/// reviewer configuration, completion (replayed with the review lifecycle,
/// 0059) and submission outcomes at the watermark.
fn evaluation_opportunities(db: &Connection, candidates: &[EvaluationCandidate], triage: &FindingState, lifecycle: &super::review_ledger::Lifecycle) -> Result<Vec<EvaluationOpportunity>> {
    let mut out = Vec::new();
    for c in candidates {
        type Row = (String, String, String, Option<String>, Option<i64>);
        let rows: Vec<Row> = db.prepare("SELECT o.opportunity_id,o.kind,o.protocol,a.reviewer_configuration_id,a.assigned_unix_ms
            FROM review_opportunities o LEFT JOIN review_assignments a ON a.opportunity_id=o.opportunity_id WHERE o.submission_id=?1 ORDER BY o.created_unix_ms,o.rowid")?
            .query_map([&c.submission_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        for (opportunity_id, kind, protocol, configuration_id, assigned_unix_ms) in rows {
            let completed = super::review_ledger::opportunity_status(db, &opportunity_id, lifecycle)?.0 == "completed";
            let mut submissions: BTreeMap<String, usize> = ["pending", "validated_only", "rejected_only", "duplicate_only", "mixed"].into_iter().map(|k| (k.to_owned(), 0)).collect();
            for s in triage.submissions.iter().filter(|s| s.opportunity_id == opportunity_id) { *submissions.entry(s.outcome.clone()).or_default() += 1; }
            out.push(EvaluationOpportunity { opportunity_id, submission_id: c.submission_id.clone(), arm: c.arm.clone(), kind, protocol, configuration_id, assigned_unix_ms, completed, submissions });
        }
    }
    Ok(out)
}
