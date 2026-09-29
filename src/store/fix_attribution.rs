//! Fix attribution, regressions and role credit (migration 0056,
//! docs/telemetry/contracts-review.md §6, plan TM3.3). A validated canonical
//! finding gets repair opportunities whose initial assignment group is frozen
//! when they open; attempts are bound to an opportunity before they have any
//! result; a fix is one bound attempt's result submission, verified only by an
//! accepted native verification of that exact candidate and integrated only by
//! the integration of that verified candidate. Reopenings end the current
//! resolution and keep its history; introduction needs a causal decision with
//! evidence, else it stays `unattributed`. Discovery, validation,
//! implementation, verification, integration and introduction are separate
//! roles whose shares of one finding sum to at most 1.
//!
//! Every change is one append-only `fix_log` row in the same ordering as
//! `finding_log` ([`fix_state`] replays both to any sequence). Only the triage
//! authority (the project owner at the CLI) records attribution.
use super::finding_triage::{self, FindingEvent, FindingState, TRIAGE_AUTHORITY};
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const REPAIR_POLICY: &str = "repair_assignment.v1";
pub const DEFAULT_REPAIR_HORIZON_MS: i64 = 14 * 86_400_000;
pub const CLOSE_OUTCOMES: [&str; 3] = ["fixed", "no_fix", "cancelled"];
pub const ASSURANCES: [&str; 2] = ["regression_reproduced", "approved_alternative"];
pub const REOPEN_REASONS: [&str; 2] = ["regression", "reverted"];
pub const INTRODUCTION_METHODS: [&str; 3] = ["controlled_reproducer", "reliable_bisect", "minimized_patch"];
/// Inference that is never causal evidence of introduction.
const INFERENCES: [&str; 4] = ["blame", "last_editor", "temporal_proximity", "fixer"];
/// Credit is exact: units of 1/720720 (lcm of 1..16), the largest share denominator.
pub const CREDIT_UNIT: u64 = 720_720;
const MAX_SHARES: usize = 32;
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];

/// A repair opportunity's initial assignment group, frozen when it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAssignment { Profile(String), Unassigned }

/// One contributor's exact share `num/den` (`den` 1..16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditShareSpec { pub attempt_id: String, pub num: u32, pub den: u32 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntroductionRequest {
    /// A causal decision: the introducing commit, the method that established
    /// it and the attempts that authored that exact commit, with shares.
    Attributed { introducing_oid: String, method: String, contributors: Vec<CreditShareSpec> },
    /// The evidence shows the introduction cannot be attributed.
    Unattributable,
}

/// A role credit in exact units, formatted as a reduced fraction (`"1"`, `"2/3"`).
pub fn credit_text(units: u64) -> String {
    fn gcd(a: u64, b: u64) -> u64 { if b == 0 { a } else { gcd(b, a % b) } }
    let g = gcd(units, CREDIT_UNIT).max(1);
    let (n, d) = (units / g, CREDIT_UNIT / g);
    if d == 1 { n.to_string() } else { format!("{n}/{d}") }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreditShare {
    /// `attempt:<id>`, `principal:<name>` or `service:<name>`.
    pub contributor: String,
    pub attempt_id: Option<String>,
    /// The attempt's dispatch configuration (null when it predates the dispatch log).
    pub configuration_id: Option<String>,
    pub share: String,
    #[serde(skip)]
    pub units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleCredit {
    /// The allocation rule, e.g. `earliest_validated.v1` or `owner_allocation.v1`.
    pub policy: String,
    /// The history row the credit comes from.
    pub source_seq: Option<i64>,
    pub shares: Vec<CreditShare>,
    pub allocated: String,
    /// Credit nobody holds: unknown, mixed or unattributed. Never given away.
    pub unallocated: String,
    pub unallocated_reason: Option<String>,
    #[serde(skip)]
    pub allocated_units: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Introduction {
    /// `unattributed` (no decision), `attributed` or `unattributable`.
    pub status: String,
    pub method: Option<String>,
    pub introducing_oid: Option<String>,
    /// The exact candidate the discovering review examined.
    pub detection_oid: Option<String>,
    pub evidence_refs: Vec<String>,
    pub credit: RoleCredit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolutionInterval {
    pub integration_seq: i64,
    pub proposal_seq: i64,
    pub repair_seq: i64,
    pub integrated_id: String,
    pub commit_oid: String,
    pub integrated_unix_ms: i64,
    /// Null while the resolution is current.
    pub ended_seq: Option<i64>,
    /// `reopened` or `superseded` (a later integrated fix of the same finding).
    pub ended_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reopening {
    pub seq: i64,
    pub reason: String,
    pub observed_oid: String,
    pub integration_seq: i64,
    pub evidence_refs: Vec<String>,
    pub recorded_unix_ms: i64,
    /// The retraction that reversed it, if any (it then ends nothing).
    pub retracted_seq: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingFixState {
    pub finding_id: String,
    /// The finding's triage status at the watermark (`validated`, `merged`, `unvalidated`).
    pub status: String,
    pub root: String,
    /// `resolved`, `fix_verified`, `fix_proposed`, `reopened`, `repair_open` or `unrepaired`.
    pub remediation: String,
    pub verified: bool,
    pub integrated: bool,
    pub currently_resolved: bool,
    pub resolutions: Vec<ResolutionInterval>,
    pub reopenings: Vec<Reopening>,
    pub repairs: Vec<i64>,
    /// Role credit of a validated finding; null for a merged or unvalidated one.
    pub discovery: Option<RoleCredit>,
    pub validation: Option<RoleCredit>,
    /// Of the fix that resolves the finding now, else its latest verified fix; null without one.
    pub implementation: Option<RoleCredit>,
    pub verification: Option<RoleCredit>,
    pub integration: Option<RoleCredit>,
    pub introduction: Option<Introduction>,
    /// Arrival of the discovery submission (windowing).
    #[serde(skip)]
    pub discovered_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairAttemptState {
    pub seq: i64,
    pub attempt_id: String,
    pub ordinal: i64,
    pub configuration_id: Option<String>,
    /// A later attempt than the initial one: provenance, never a new cohort.
    pub reassignment: bool,
    pub bound_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixVerification {
    pub seq: i64,
    pub run_id: String,
    pub result_id: String,
    pub commit_oid: String,
    pub assurance: String,
    pub evidence_refs: Vec<String>,
    pub recorded_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixIntegration {
    pub seq: i64,
    pub integrated_id: String,
    pub commit_oid: String,
    pub integrated_unix_ms: i64,
    pub recorded_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProposalState {
    pub seq: i64,
    pub submission_id: String,
    pub attempt_id: String,
    pub candidate_oid: String,
    pub verification: Option<FixVerification>,
    pub integration: Option<FixIntegration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairState {
    pub repair_seq: i64,
    pub finding_id: String,
    /// `configuration` or `unassigned`.
    pub assignment: String,
    pub configuration_id: Option<String>,
    pub profile_digest: Option<String>,
    pub policy: String,
    pub horizon_ms: i64,
    pub opened_unix_ms: i64,
    pub attempts: Vec<RepairAttemptState>,
    pub proposals: Vec<ProposalState>,
    /// `fixed`, `no_fix` or `cancelled`; null while open.
    pub closure: Option<String>,
    pub closed_unix_ms: Option<i64>,
    /// `currently_resolved`, `integrated`, `verified`, `proposed` or `no_candidate`.
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixState {
    pub head_seq: i64,
    pub as_of_seq: i64,
    pub findings: Vec<FindingFixState>,
    pub repairs: Vec<RepairState>,
    /// Every `fix_log` row up to the watermark.
    pub history: Vec<FindingEvent>,
    /// The finding triage state replayed to the same watermark.
    #[serde(skip)]
    pub triage: FindingState,
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_56(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 56 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

fn oid(value: &str) -> bool { (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

/// Append the owner's attribution row after the authority and expected-head checks.
fn log(tx: &Connection, kind: &str, principal: &str, expected: Option<i64>, now: i64) -> Result<i64> {
    finding_triage::triage_authority(tx, principal)?;
    let head = finding_triage::head(tx)?;
    if let Some(expected) = expected && head != expected { return Err(invalid(format!("finding history moved: head is {head}, expected {expected}"))); }
    let seq = head + 1;
    tx.execute("INSERT INTO fix_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![seq, kind, principal, TRIAGE_AUTHORITY, expected, now])?;
    Ok(seq)
}

fn done(tx: rusqlite::Transaction<'_>, seq: i64, subject: serde_json::Value) -> Result<FindingEvent> {
    let out = tx.query_row("SELECT kind,principal,authority,expected_seq,recorded_unix_ms FROM fix_log WHERE seq=?1", [seq],
        |r| Ok(FindingEvent { seq, kind: r.get(0)?, principal: r.get(1)?, authority: r.get(2)?, expected_seq: r.get(3)?, recorded_unix_ms: r.get(4)?, subject }))?;
    tx.commit()?;
    Ok(out)
}

fn configuration(db: &Connection, attempt: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [attempt], |r| r.get(0)).optional()?)
}

/// The current fix state at the head, for write-time checks.
fn current(tx: &Connection) -> Result<FixState> { fix_state(tx, None)?.ok_or(StoreError::UnsupportedSchema(55)) }

fn validated_root<'a>(state: &'a FixState, finding: &str) -> Result<&'a FindingFixState> {
    let f = state.findings.iter().find(|f| f.finding_id == finding).ok_or_else(|| invalid(format!("no canonical finding {finding}")))?;
    if f.status != "validated" { return Err(invalid(format!("finding {finding} is {}: attribution applies to a validated unique finding (its group root)", f.status))); }
    Ok(f)
}

fn shares(specs: &[CreditShareSpec]) -> Result<u64> {
    if specs.len() > MAX_SHARES { return Err(invalid(format!("at most {MAX_SHARES} shares"))); }
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for s in specs {
        if !seen.insert(&s.attempt_id) { return Err(invalid(format!("attempt {} has two shares", s.attempt_id))); }
        if !(1..=16).contains(&s.den) || s.num == 0 || s.num > s.den { return Err(invalid(format!("share {}/{} must be a fraction in (0, 1] with a denominator of 1 to 16", s.num, s.den))); }
        total += u64::from(s.num) * (CREDIT_UNIT / u64::from(s.den));
    }
    if total > CREDIT_UNIT { return Err(invalid("the shares of one role sum to more than 1: mixed contributions cannot each receive full credit".into())); }
    Ok(total)
}

fn insert_shares(tx: &Connection, seq: i64, specs: &[CreditShareSpec]) -> Result<()> {
    for s in specs { tx.execute("INSERT INTO credit_shares(seq,attempt_id,share_num,share_den) VALUES(?1,?2,?3,?4)", params![seq, s.attempt_id, s.num, s.den])?; }
    Ok(())
}

fn share_json(specs: &[CreditShareSpec]) -> serde_json::Value {
    serde_json::Value::Array(specs.iter().map(|s| serde_json::json!({"attempt_id": s.attempt_id, "share": credit_text(u64::from(s.num) * (CREDIT_UNIT / u64::from(s.den)))})).collect())
}

impl SqliteStore {
    /// Open a repair opportunity for a validated finding, freezing its initial
    /// assignment group (a retained profile's configuration, or explicitly
    /// unassigned) and horizon before any repair runs. One open opportunity per
    /// finding; a currently resolved finding needs a reopening first.
    pub fn open_repair(&mut self, finding: &str, assignment: &RepairAssignment, horizon_ms: i64, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if horizon_ms <= 0 { return Err(invalid("the repair horizon must be positive".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let state = current(&tx)?;
        let f = validated_root(&state, finding)?;
        if f.currently_resolved { return Err(invalid(format!("finding {finding} is currently resolved; record a reopening before a new repair"))); }
        if let Some(open) = state.repairs.iter().find(|r| r.finding_id == finding && r.closure.is_none()) {
            return Err(invalid(format!("finding {finding} already has open repair opportunity {}", open.repair_seq)));
        }
        let (configuration, profile_digest) = match assignment {
            RepairAssignment::Unassigned => (None, None),
            RepairAssignment::Profile(name) => {
                let (digest, profile) = super::review_capture::retained_profile(&tx, name)?;
                let configuration = crate::domain::agent_configuration(&profile);
                super::dispatch_log::insert_configuration(&tx, &configuration, now)?;
                (Some(configuration.id), Some(digest))
            }
        };
        let kind = if configuration.is_some() { "configuration" } else { "unassigned" };
        let seq = log(&tx, "repair_opened", principal, expected_seq, now)?;
        tx.execute("INSERT INTO repair_opportunities(seq,finding_id,assignment,configuration_id,profile_digest,policy,horizon_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![seq, finding, kind, configuration, profile_digest, REPAIR_POLICY, horizon_ms])?;
        done(tx, seq, serde_json::json!({"repair_seq": seq, "finding_id": finding, "assignment": kind, "configuration_id": configuration,
            "profile_digest": profile_digest, "policy": REPAIR_POLICY, "horizon_ms": horizon_ms}))
    }

    /// Bind an attempt to an open repair opportunity before its outcome is
    /// known: it must not be terminal and must have no result submission.
    /// Later attempts are reassignments; the opportunity keeps its initial group.
    pub fn bind_repair_attempt(&mut self, repair: i64, attempt: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_opportunities WHERE seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("no repair opportunity {repair}"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_closures WHERE repair_seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("repair opportunity {repair} is closed"))); }
        let state: String = tx.query_row("SELECT state FROM attempts WHERE id=?1", [attempt], |r| r.get(0)).optional()?.ok_or_else(|| invalid(format!("no attempt {attempt}")))?;
        if TERMINAL.contains(&state.as_str()) { return Err(invalid(format!("attempt {attempt} is already {state}: an attempt is bound before its outcome is known"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM result_submissions WHERE attempt_id=?1)", [attempt], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("attempt {attempt} already has a result: an attempt is bound before its outcome is known")));
        }
        if let Some(other) = tx.query_row("SELECT repair_seq FROM repair_attempts WHERE attempt_id=?1", [attempt], |r| r.get::<_, i64>(0)).optional()? {
            return Err(invalid(format!("attempt {attempt} is already bound to repair opportunity {other}")));
        }
        let ordinal: i64 = tx.query_row("SELECT count(*)+1 FROM repair_attempts WHERE repair_seq=?1", [repair], |r| r.get(0))?;
        let configuration = configuration(&tx, attempt)?;
        let seq = log(&tx, "attempt_bound", principal, expected_seq, now)?;
        tx.execute("INSERT INTO repair_attempts(seq,repair_seq,attempt_id,ordinal,configuration_id) VALUES(?1,?2,?3,?4,?5)", params![seq, repair, attempt, ordinal, configuration])?;
        done(tx, seq, serde_json::json!({"repair_seq": repair, "attempt_id": attempt, "ordinal": ordinal, "configuration_id": configuration}))
    }

    /// Record a bound attempt's result submission as a fix proposal at its exact candidate.
    pub fn propose_fix(&mut self, repair: i64, submission: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_opportunities WHERE seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("no repair opportunity {repair}"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_closures WHERE repair_seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("repair opportunity {repair} is closed"))); }
        let (attempt, candidate): (String, String) = tx.query_row("SELECT attempt_id,candidate_oid FROM result_submissions WHERE submission_id=?1", [submission], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?.ok_or_else(|| invalid(format!("no result submission {submission}")))?;
        let bound: Option<i64> = tx.query_row("SELECT repair_seq FROM repair_attempts WHERE attempt_id=?1", [&attempt], |r| r.get(0)).optional()?;
        if bound != Some(repair) { return Err(invalid(format!("submission {submission} is by attempt {attempt}, which is not bound to repair opportunity {repair}"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_proposals WHERE submission_id=?1)", [submission], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("submission {submission} is already a fix proposal"))); }
        let seq = log(&tx, "proposed", principal, expected_seq, now)?;
        tx.execute("INSERT INTO fix_proposals(seq,repair_seq,submission_id,attempt_id,candidate_oid) VALUES(?1,?2,?3,?4,?5)", params![seq, repair, submission, attempt, candidate])?;
        done(tx, seq, serde_json::json!({"repair_seq": repair, "submission_id": submission, "attempt_id": attempt, "candidate_oid": candidate}))
    }

    /// The owner's decision that an accepted native verification run of the
    /// proposal's exact candidate (same submission, same commit, with its
    /// verified result) repairs the finding. A run on any other commit refuses.
    #[allow(clippy::too_many_arguments)]
    pub fn verify_fix(&mut self, proposal: i64, run: &str, assurance: &str, evidence: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if !ASSURANCES.contains(&assurance) { return Err(invalid(format!("unknown assurance {assurance}"))); }
        let evidence = finding_triage::evidence(evidence)?;
        if evidence.is_empty() { return Err(invalid("a verified fix needs at least one evidence reference linking it to the finding".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let (repair, submission, candidate): (i64, String, String) = tx.query_row("SELECT repair_seq,submission_id,candidate_oid FROM fix_proposals WHERE seq=?1", [proposal],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or_else(|| invalid(format!("no fix proposal {proposal}")))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_closures WHERE repair_seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("repair opportunity {repair} is closed"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_verifications WHERE proposal_seq=?1)", [proposal], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("fix proposal {proposal} is already verified"))); }
        let (run_submission, commit, run_state): (String, String, String) = tx.query_row("SELECT submission_id,commit_oid,state FROM verification_runs WHERE run_id=?1", [run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.ok_or_else(|| invalid(format!("no verification run {run}")))?;
        if run_submission != submission || commit != candidate {
            return Err(invalid(format!("verification run {run} checked commit {commit} of submission {run_submission}, not the fix's exact candidate {candidate}: passing checks on one commit cannot verify another")));
        }
        if run_state != "accepted" { return Err(invalid(format!("verification run {run} was {run_state}"))); }
        let result: String = tx.query_row("SELECT result_id FROM verified_results WHERE run_id=?1 AND submission_id=?2 AND commit_oid=?3", params![run, submission, candidate], |r| r.get(0))
            .optional()?.ok_or_else(|| invalid(format!("verification run {run} has no verified result")))?;
        let seq = log(&tx, "verified", principal, expected_seq, now)?;
        tx.execute("INSERT INTO fix_verifications(seq,proposal_seq,run_id,result_id,commit_oid,assurance,evidence_refs) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![seq, proposal, run, result, candidate, assurance, serde_json::json!(evidence).to_string()])?;
        done(tx, seq, serde_json::json!({"proposal_seq": proposal, "run_id": run, "result_id": result, "commit_oid": candidate, "assurance": assurance, "evidence_refs": evidence}))
    }

    /// Link the factory's integration of a verified fix's exact candidate. A
    /// fix verified only on its branch is not integrated.
    pub fn integrate_fix(&mut self, proposal: i64, integrated: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let (submission, candidate): (String, String) = tx.query_row("SELECT submission_id,candidate_oid FROM fix_proposals WHERE seq=?1", [proposal], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?.ok_or_else(|| invalid(format!("no fix proposal {proposal}")))?;
        let verification: i64 = tx.query_row("SELECT seq FROM fix_verifications WHERE proposal_seq=?1", [proposal], |r| r.get(0)).optional()?
            .ok_or_else(|| invalid(format!("fix proposal {proposal} is not verified: only a verified fix can be integrated")))?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_integrations WHERE proposal_seq=?1)", [proposal], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("fix proposal {proposal} is already integrated"))); }
        type Row = (String, i64, Option<String>, Option<String>, Option<String>);
        let (commit, at, verified_submission, verified_commit, parent): Row = tx.query_row("SELECT ic.commit_oid,ic.created_unix_ms,v.submission_id,v.commit_oid,c.parent_verified FROM integrated_commits ic
            LEFT JOIN integration_operations o ON o.operation_id=ic.operation_id LEFT JOIN verified_results v ON v.result_id=o.verified_result_id
            LEFT JOIN integration_candidates c ON c.candidate_id=ic.candidate_id WHERE ic.integrated_id=?1", [integrated],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?.ok_or_else(|| invalid(format!("no integrated commit {integrated}")))?;
        if verified_submission.as_deref() != Some(submission.as_str()) || verified_commit.as_deref() != Some(candidate.as_str()) || parent.as_deref() != Some(candidate.as_str()) {
            return Err(invalid(format!("integration {integrated} did not integrate the fix's exact verified candidate {candidate}")));
        }
        let seq = log(&tx, "integrated", principal, expected_seq, now)?;
        tx.execute("INSERT INTO fix_integrations(seq,proposal_seq,verification_seq,integrated_id,commit_oid,integrated_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![seq, proposal, verification, integrated, commit, at])?;
        done(tx, seq, serde_json::json!({"proposal_seq": proposal, "verification_seq": verification, "integrated_id": integrated, "commit_oid": commit, "integrated_unix_ms": at}))
    }

    /// Close a repair opportunity: `fixed` (it has a verified fix), `no_fix` or
    /// `cancelled` (it has none). A closed opportunity stays in its cohort.
    pub fn close_repair(&mut self, repair: i64, outcome: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if !CLOSE_OUTCOMES.contains(&outcome) { return Err(invalid(format!("unknown repair outcome {outcome}"))); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        if !tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_opportunities WHERE seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("no repair opportunity {repair}"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_closures WHERE repair_seq=?1)", [repair], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("repair opportunity {repair} is already closed"))); }
        let verified: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_proposals p JOIN fix_verifications v ON v.proposal_seq=p.seq WHERE p.repair_seq=?1)", [repair], |r| r.get(0))?;
        if verified != (outcome == "fixed") {
            let (has, close) = if verified { ("has a", "fixed") } else { ("has no", "no_fix or cancelled") };
            return Err(invalid(format!("repair opportunity {repair} {has} verified fix: close it {close}")));
        }
        let seq = log(&tx, "repair_closed", principal, expected_seq, now)?;
        tx.execute("INSERT INTO repair_closures(seq,repair_seq,outcome) VALUES(?1,?2,?3)", params![seq, repair, outcome])?;
        done(tx, seq, serde_json::json!({"repair_seq": repair, "outcome": outcome}))
    }

    /// Record an accepted occurrence showing a currently resolved finding's
    /// defect at a later exact revision (`regression`) or its fix reverted
    /// (`reverted`). It ends the current resolution; the fix's receipts stay.
    #[allow(clippy::too_many_arguments)]
    pub fn reopen_finding(&mut self, finding: &str, reason: &str, observed_oid: &str, evidence: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if !REOPEN_REASONS.contains(&reason) { return Err(invalid(format!("unknown reopen reason {reason}"))); }
        if !oid(observed_oid) { return Err(invalid("the observed revision must be an exact commit id".into())); }
        let evidence = finding_triage::evidence(evidence)?;
        if evidence.is_empty() { return Err(invalid("a reopening needs at least one evidence reference for the occurrence".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let state = current(&tx)?;
        let f = validated_root(&state, finding)?;
        let integration = f.resolutions.iter().find(|r| r.ended_seq.is_none()).map(|r| r.integration_seq)
            .ok_or_else(|| invalid(format!("finding {finding} is not currently resolved: only an integrated fix can be reopened")))?;
        let seq = log(&tx, "reopened", principal, expected_seq, now)?;
        tx.execute("INSERT INTO fix_reopenings(seq,finding_id,integration_seq,reason,observed_oid,evidence_refs) VALUES(?1,?2,?3,?4,?5,?6)",
            params![seq, finding, integration, reason, observed_oid, serde_json::json!(evidence).to_string()])?;
        done(tx, seq, serde_json::json!({"finding_id": finding, "integration_seq": integration, "reason": reason, "observed_oid": observed_oid, "evidence_refs": evidence}))
    }

    /// A causal introduction decision. Blame, the last editor, temporal
    /// proximity or being the fixer are not evidence; an attributed contributor
    /// must be an attempt whose exact candidate is the introducing commit, and
    /// never through the fix it made for this finding.
    pub fn attribute_introduction(&mut self, finding: &str, request: &IntroductionRequest, evidence: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let evidence = finding_triage::evidence(evidence)?;
        if evidence.is_empty() { return Err(invalid("an introduction decision needs at least one evidence reference".into())); }
        let (status, method, introducing, contributors) = match request {
            IntroductionRequest::Unattributable => ("unattributable", None, None, &[][..]),
            IntroductionRequest::Attributed { introducing_oid, method, contributors } => {
                if INFERENCES.contains(&method.as_str()) { return Err(invalid(format!("{method} is inference, not causal evidence: use a controlled reproducer, a reliable bisect or a minimized patch"))); }
                if !INTRODUCTION_METHODS.contains(&method.as_str()) { return Err(invalid(format!("unknown introduction method {method}"))); }
                if !oid(introducing_oid) { return Err(invalid("the introducing revision must be an exact commit id".into())); }
                ("attributed", Some(method.as_str()), Some(introducing_oid.as_str()), contributors.as_slice())
            }
        };
        shares(contributors)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let state = current(&tx)?;
        validated_root(&state, finding)?;
        for c in contributors {
            let authored: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM result_submissions WHERE attempt_id=?1 AND candidate_oid=?2)", params![c.attempt_id, introducing], |r| r.get(0))?;
            if !authored { return Err(invalid(format!("attempt {} has no candidate at the introducing commit: introduction is not inferred from other work", c.attempt_id))); }
            let own_fix: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_proposals p JOIN repair_opportunities o ON o.seq=p.repair_seq WHERE p.attempt_id=?1 AND p.candidate_oid=?2 AND o.finding_id=?3)",
                params![c.attempt_id, introducing, finding], |r| r.get(0))?;
            if own_fix { return Err(invalid(format!("attempt {}'s candidate is a fix for {finding}: the fixer is not charged with introducing what it repaired", c.attempt_id))); }
        }
        let seq = log(&tx, "introduced", principal, expected_seq, now)?;
        tx.execute("INSERT INTO introduction_decisions(seq,finding_id,status,method,introducing_oid,evidence_refs) VALUES(?1,?2,?3,?4,?5,?6)",
            params![seq, finding, status, method, introducing, serde_json::json!(evidence).to_string()])?;
        insert_shares(&tx, seq, contributors)?;
        done(tx, seq, serde_json::json!({"finding_id": finding, "status": status, "method": method, "introducing_oid": introducing, "contributors": share_json(contributors), "evidence_refs": evidence}))
    }

    /// The owner's fractional allocation of a role's credit, with evidence:
    /// `discovery` among the reporters of the finding's validated or duplicate
    /// claims, `implementation` of one verified fix among its repair's
    /// attempts. Shares sum to at most 1; the remainder stays unallocated.
    #[allow(clippy::too_many_arguments)]
    pub fn allocate_credit(&mut self, finding: &str, role: &str, proposal: Option<i64>, specs: &[CreditShareSpec], evidence: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let evidence = finding_triage::evidence(evidence)?;
        if evidence.is_empty() { return Err(invalid("a credit allocation needs at least one evidence reference: timing alone does not prove a contribution".into())); }
        if specs.is_empty() { return Err(invalid("a credit allocation names at least one share".into())); }
        shares(specs)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let state = current(&tx)?;
        validated_root(&state, finding)?;
        let eligible: BTreeSet<String> = match (role, proposal) {
            ("discovery", None) => state.triage.submissions.iter().filter(|s| s.claims.iter().any(|c| c.canonical_finding.as_deref() == Some(finding) && matches!(c.decided.as_str(), "validated" | "duplicate")))
                .map(|s| s.reporter_attempt_id.clone()).collect(),
            ("implementation", Some(proposal)) => {
                let repair = state.repairs.iter().find(|r| r.finding_id == finding && r.proposals.iter().any(|p| p.seq == proposal && p.verification.is_some()))
                    .ok_or_else(|| invalid(format!("fix proposal {proposal} is not a verified fix of {finding}")))?;
                repair.attempts.iter().map(|a| a.attempt_id.clone()).collect()
            }
            ("discovery", Some(_)) => return Err(invalid("discovery credit is per finding, not per fix proposal".into())),
            ("implementation", None) => return Err(invalid("implementation credit names the verified fix proposal".into())),
            _ => return Err(invalid(format!("unknown credit role {role}: allocate discovery or implementation"))),
        };
        if let Some(bad) = specs.iter().find(|s| !eligible.contains(&s.attempt_id)) {
            return Err(invalid(format!("attempt {} did not contribute to the {role} of {finding}", bad.attempt_id)));
        }
        let seq = log(&tx, "credited", principal, expected_seq, now)?;
        tx.execute("INSERT INTO credit_allocations(seq,finding_id,role,proposal_seq,policy,evidence_refs) VALUES(?1,?2,?3,?4,'owner_allocation.v1',?5)",
            params![seq, finding, role, proposal, serde_json::json!(evidence).to_string()])?;
        insert_shares(&tx, seq, specs)?;
        done(tx, seq, serde_json::json!({"finding_id": finding, "role": role, "proposal_seq": proposal, "shares": share_json(specs), "evidence_refs": evidence}))
    }

    /// Reverse one credit allocation, reopening or introduction decision
    /// recorded in error. It stays in the history; the as-of views before this
    /// sequence still show it.
    pub fn retract_attribution(&mut self, target: i64, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_56(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let kind: String = tx.query_row("SELECT kind FROM fix_log WHERE seq=?1", [target], |r| r.get(0)).optional()?.ok_or_else(|| invalid(format!("no fix history row {target}")))?;
        if !["credited", "reopened", "introduced"].contains(&kind.as_str()) { return Err(invalid(format!("history row {target} is {kind}: only a credit allocation, reopening or introduction can be retracted"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_retractions WHERE reverses=?1)", [target], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("history row {target} is already retracted"))); }
        if kind == "reopened" {
            // A resolution restored by the retraction must not overlap a later one.
            let later: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM fix_integrations i JOIN fix_proposals p ON p.seq=i.proposal_seq JOIN repair_opportunities o ON o.seq=p.repair_seq
                WHERE o.finding_id=(SELECT finding_id FROM fix_reopenings WHERE seq=?1) AND i.seq>?1)", [target], |r| r.get(0))?;
            if later { return Err(invalid(format!("a later fix of the finding was integrated after reopening {target}; retracting it would rewrite that repair"))); }
        }
        let seq = log(&tx, "retracted", principal, expected_seq, now)?;
        tx.execute("INSERT INTO fix_retractions(seq,reverses) VALUES(?1,?2)", params![seq, target])?;
        done(tx, seq, serde_json::json!({"reverses": target, "kind": kind}))
    }
}

fn json_refs(text: &str) -> Vec<String> { serde_json::from_str(text).unwrap_or_default() }

/// Shares of `seq` as credit, with each attempt's configuration.
fn load_shares(db: &Connection, seq: i64) -> Result<Vec<CreditShare>> {
    let rows: Vec<(String, u64, u64)> = db.prepare("SELECT attempt_id,share_num,share_den FROM credit_shares WHERE seq=?1 ORDER BY attempt_id")?
        .query_map([seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    rows.into_iter().map(|(attempt, n, d)| {
        let units = n * (CREDIT_UNIT / d.max(1));
        Ok(CreditShare { contributor: format!("attempt:{attempt}"), configuration_id: configuration(db, &attempt)?, attempt_id: Some(attempt), share: credit_text(units), units })
    }).collect()
}

fn role(policy: &str, source: Option<i64>, shares: Vec<CreditShare>, reason: Option<&str>) -> RoleCredit {
    let allocated: u64 = shares.iter().map(|s| s.units).sum();
    let unallocated = CREDIT_UNIT.saturating_sub(allocated);
    RoleCredit { policy: policy.into(), source_seq: source, allocated: credit_text(allocated), unallocated: credit_text(unallocated),
        unallocated_reason: if unallocated > 0 { Some(reason.unwrap_or("unallocated").into()) } else { None }, shares, allocated_units: allocated }
}

fn whole(db: &Connection, contributor: String, attempt: Option<String>) -> Result<CreditShare> {
    let configuration = match &attempt { Some(a) => configuration(db, a)?, None => None };
    Ok(CreditShare { contributor, attempt_id: attempt, configuration_id: configuration, share: "1".into(), units: CREDIT_UNIT })
}

/// Replay the finding and fix history up to `as_of` (default: the head) on any
/// connection, read-only. `None` before migration 0056.
pub fn fix_state(db: &Connection, as_of: Option<i64>) -> Result<Option<FixState>> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='fix_log')", [], |r| r.get(0))?;
    if !present { return Ok(None); }
    let Some(triage) = finding_triage::finding_state(db, as_of)? else { return Ok(None) };
    let at = triage.as_of_seq;
    let retracted: BTreeMap<i64, i64> = db.prepare("SELECT reverses,seq FROM fix_retractions WHERE seq<=?1")?.query_map([at], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;

    // Repairs, their attempts, proposals, verifications, integrations and closures.
    type RepairRow = (i64, String, String, Option<String>, Option<String>, String, i64, i64);
    let rows: Vec<RepairRow> = db.prepare("SELECT o.seq,o.finding_id,o.assignment,o.configuration_id,o.profile_digest,o.policy,o.horizon_ms,l.recorded_unix_ms
        FROM repair_opportunities o JOIN fix_log l ON l.seq=o.seq WHERE o.seq<=?1 ORDER BY o.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut repairs = Vec::with_capacity(rows.len());
    for (repair_seq, finding_id, assignment, configuration_id, profile_digest, policy, horizon_ms, opened_unix_ms) in rows {
        let attempts: Vec<RepairAttemptState> = db.prepare("SELECT a.seq,a.attempt_id,a.ordinal,a.configuration_id,l.recorded_unix_ms FROM repair_attempts a JOIN fix_log l ON l.seq=a.seq
            WHERE a.repair_seq=?1 AND a.seq<=?2 ORDER BY a.ordinal")?
            .query_map(params![repair_seq, at], |r| Ok(RepairAttemptState { seq: r.get(0)?, attempt_id: r.get(1)?, ordinal: r.get(2)?, configuration_id: r.get(3)?,
                reassignment: r.get::<_, i64>(2)? > 1, bound_unix_ms: r.get(4)? }))?.collect::<rusqlite::Result<_>>()?;
        let proposal_rows: Vec<(i64, String, String, String)> = db.prepare("SELECT seq,submission_id,attempt_id,candidate_oid FROM fix_proposals WHERE repair_seq=?1 AND seq<=?2 ORDER BY seq")?
            .query_map(params![repair_seq, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut proposals = Vec::with_capacity(proposal_rows.len());
        for (seq, submission_id, attempt_id, candidate_oid) in proposal_rows {
            let verification = db.query_row("SELECT v.seq,v.run_id,v.result_id,v.commit_oid,v.assurance,v.evidence_refs,l.recorded_unix_ms FROM fix_verifications v JOIN fix_log l ON l.seq=v.seq
                WHERE v.proposal_seq=?1 AND v.seq<=?2", params![seq, at],
                |r| Ok(FixVerification { seq: r.get(0)?, run_id: r.get(1)?, result_id: r.get(2)?, commit_oid: r.get(3)?, assurance: r.get(4)?,
                    evidence_refs: json_refs(&r.get::<_, String>(5)?), recorded_unix_ms: r.get(6)? })).optional()?;
            let integration = db.query_row("SELECT i.seq,i.integrated_id,i.commit_oid,i.integrated_unix_ms,l.recorded_unix_ms FROM fix_integrations i JOIN fix_log l ON l.seq=i.seq
                WHERE i.proposal_seq=?1 AND i.seq<=?2", params![seq, at],
                |r| Ok(FixIntegration { seq: r.get(0)?, integrated_id: r.get(1)?, commit_oid: r.get(2)?, integrated_unix_ms: r.get(3)?, recorded_unix_ms: r.get(4)? })).optional()?;
            proposals.push(ProposalState { seq, submission_id, attempt_id, candidate_oid, verification, integration });
        }
        let closure: Option<(String, i64)> = db.query_row("SELECT c.outcome,l.recorded_unix_ms FROM repair_closures c JOIN fix_log l ON l.seq=c.seq WHERE c.repair_seq=?1 AND c.seq<=?2",
            params![repair_seq, at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        repairs.push(RepairState { repair_seq, finding_id, assignment, configuration_id, profile_digest, policy, horizon_ms, opened_unix_ms, attempts, proposals,
            closed_unix_ms: closure.as_ref().map(|c| c.1), closure: closure.map(|c| c.0), outcome: String::new() });
    }

    // Reopenings (with their retractions) per finding.
    let reopen_rows: Vec<(i64, String, i64, String, String, String, i64)> = db.prepare("SELECT r.seq,r.finding_id,r.integration_seq,r.reason,r.observed_oid,r.evidence_refs,l.recorded_unix_ms
        FROM fix_reopenings r JOIN fix_log l ON l.seq=r.seq WHERE r.seq<=?1 ORDER BY r.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut reopenings: BTreeMap<String, Vec<Reopening>> = BTreeMap::new();
    for (seq, finding, integration_seq, reason, observed_oid, evidence, recorded) in reopen_rows {
        reopenings.entry(finding).or_default().push(Reopening { seq, reason, observed_oid, integration_seq, evidence_refs: json_refs(&evidence), recorded_unix_ms: recorded,
            retracted_seq: retracted.get(&seq).copied() });
    }

    // Resolution intervals per finding, in integration order.
    let mut resolutions: BTreeMap<String, Vec<ResolutionInterval>> = BTreeMap::new();
    let mut integrations: Vec<(i64, &RepairState, &ProposalState, &FixIntegration)> = repairs.iter()
        .flat_map(|r| r.proposals.iter().filter_map(move |p| p.integration.as_ref().map(|i| (i.seq, r, p, i)))).collect();
    integrations.sort_by_key(|i| i.0);
    for (_, repair, proposal, integration) in &integrations {
        let list = resolutions.entry(repair.finding_id.clone()).or_default();
        if let Some(previous) = list.iter_mut().find(|r| r.ended_seq.is_none()) {
            previous.ended_seq = Some(integration.seq);
            previous.ended_by = Some("superseded".into());
        }
        let ended = reopenings.get(&repair.finding_id).and_then(|all| all.iter().find(|o| o.integration_seq == integration.seq && o.retracted_seq.is_none())).map(|o| o.seq);
        list.push(ResolutionInterval { integration_seq: integration.seq, proposal_seq: proposal.seq, repair_seq: repair.repair_seq, integrated_id: integration.integrated_id.clone(),
            commit_oid: integration.commit_oid.clone(), integrated_unix_ms: integration.integrated_unix_ms, ended_seq: ended, ended_by: ended.map(|_| "reopened".into()) });
    }
    let current_integration: BTreeSet<i64> = resolutions.values().flatten().filter(|r| r.ended_seq.is_none()).map(|r| r.integration_seq).collect();
    for repair in &mut repairs {
        let has = |f: &dyn Fn(&ProposalState) -> bool| repair.proposals.iter().any(f);
        repair.outcome = if has(&|p| p.integration.as_ref().is_some_and(|i| current_integration.contains(&i.seq))) { "currently_resolved" }
            else if has(&|p| p.integration.is_some()) { "integrated" } else if has(&|p| p.verification.is_some()) { "verified" }
            else if !repair.proposals.is_empty() { "proposed" } else { "no_candidate" }.into();
    }

    // Latest unretracted owner allocations: (finding, role, proposal) -> seq.
    let mut allocations: BTreeMap<(String, String, Option<i64>), i64> = BTreeMap::new();
    for row in db.prepare("SELECT seq,finding_id,role,proposal_seq FROM credit_allocations WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<i64>>(3)?)))? {
        let (seq, finding, role, proposal) = row?;
        if !retracted.contains_key(&seq) { allocations.insert((finding, role, proposal), seq); }
    }
    /// (seq, status, method, introducing commit, evidence) of the latest unretracted decision.
    type Decision = (i64, String, Option<String>, Option<String>, String);
    let mut introductions: BTreeMap<String, Decision> = BTreeMap::new();
    for row in db.prepare("SELECT seq,finding_id,status,method,introducing_oid,evidence_refs FROM introduction_decisions WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))? {
        let (seq, finding, status, method, introducing, evidence) = row?;
        if !retracted.contains_key(&seq) { introductions.insert(finding, (seq, status, method, introducing, evidence)); }
    }

    let mut findings = Vec::with_capacity(triage.findings.len());
    for group in &triage.findings {
        let id = &group.finding_id;
        let own: Vec<&RepairState> = repairs.iter().filter(|r| &r.finding_id == id).collect();
        let proposals = || own.iter().flat_map(|r| r.proposals.iter().map(move |p| (*r, p)));
        let intervals = resolutions.remove(id).unwrap_or_default();
        let reopen = reopenings.remove(id).unwrap_or_default();
        let currently_resolved = intervals.iter().any(|r| r.ended_seq.is_none());
        let last_reopen = reopen.iter().filter(|o| o.retracted_seq.is_none()).map(|o| o.seq).max().unwrap_or(0);
        let after = |seq: i64| seq > last_reopen;
        let remediation = if currently_resolved { "resolved" }
            else if proposals().any(|(_, p)| after(p.seq) && p.verification.is_some()) { "fix_verified" }
            else if proposals().any(|(_, p)| after(p.seq)) { "fix_proposed" }
            else if last_reopen > 0 { "reopened" }
            else if own.iter().any(|r| r.closure.is_none()) { "repair_open" } else { "unrepaired" };
        let validated = group.status == "validated";
        // Discovery: the D2 discovery claim's reporter (earliest validated), unless the owner allocated shares.
        let discovery_claim = group.discovery_claim.and_then(|claim| triage.submissions.iter().find(|s| s.claims.iter().any(|c| c.claim_id == claim)).map(|s| (claim, s)));
        let (discovery, validation, detection_oid, discovered) = if let (true, Some((claim, submission))) = (validated, discovery_claim) {
            let discovery = match allocations.get(&(id.clone(), "discovery".into(), None)) {
                Some(&seq) => role("owner_allocation.v1", Some(seq), load_shares(db, seq)?, Some("shared_discovery_unallocated")),
                None => role("earliest_validated.v1", Some(submission.seq), vec![whole(db, format!("attempt:{}", submission.reporter_attempt_id), Some(submission.reporter_attempt_id.clone()))?], None),
            };
            let decision = submission.claims.iter().find(|c| c.claim_id == claim).and_then(|c| c.decision_seq);
            let principal = triage.history.iter().find(|e| Some(e.seq) == decision).map_or_else(|| finding_triage::TRIAGE_PRINCIPAL.to_owned(), |e| e.principal.clone());
            let validation = role("triage_decision.v1", decision, vec![whole(db, format!("principal:{principal}"), None)?], None);
            let detection: Option<String> = db.query_row("SELECT candidate_oid FROM review_opportunities WHERE opportunity_id=?1", [&submission.opportunity_id], |r| r.get(0)).optional()?;
            (Some(discovery), Some(validation), detection, Some(submission.recorded_unix_ms))
        } else { (None, None, None, None) };
        // The fix credited now: the current resolution's, else the latest verified one.
        let credited = intervals.iter().find(|r| r.ended_seq.is_none()).map(|r| r.proposal_seq)
            .or_else(|| proposals().filter(|(_, p)| p.verification.is_some()).map(|(_, p)| p.seq).max());
        let (mut implementation, mut verification, mut integration) = (None, None, None);
        if let (true, Some(seq)) = (validated, credited) && let Some((repair, proposal)) = proposals().find(|(_, p)| p.seq == seq) {
            implementation = Some(match allocations.get(&(id.clone(), "implementation".into(), Some(seq))) {
                Some(&allocation) => role("owner_allocation.v1", Some(allocation), load_shares(db, allocation)?, Some("mixed_contribution_unallocated")),
                None => {
                    let bound: Vec<&RepairAttemptState> = repair.attempts.iter().filter(|a| a.seq < seq).collect();
                    if bound.len() == 1 && bound[0].attempt_id == proposal.attempt_id {
                        role("sole_attempt.v1", Some(seq), vec![whole(db, format!("attempt:{}", proposal.attempt_id), Some(proposal.attempt_id.clone()))?], None)
                    } else { role("sole_attempt.v1", Some(seq), Vec::new(), Some("mixed_contribution_unallocated")) }
                }
            });
            if let Some(v) = &proposal.verification { verification = Some(role("native_verifier.v1", Some(v.seq), vec![whole(db, "service:native_verifier".into(), None)?], None)); }
            if let Some(i) = &proposal.integration { integration = Some(role("integrator.v1", Some(i.seq), vec![whole(db, "service:integrator".into(), None)?], None)); }
        }
        let introduction = validated.then(|| -> Result<Introduction> {
            Ok(match introductions.get(id) {
                None => Introduction { status: "unattributed".into(), method: None, introducing_oid: None, detection_oid: detection_oid.clone(), evidence_refs: Vec::new(),
                    credit: role("introduction_decision.v1", None, Vec::new(), Some("unattributed")) },
                Some((seq, status, method, introducing, evidence)) => Introduction { status: status.clone(), method: method.clone(), introducing_oid: introducing.clone(),
                    detection_oid: detection_oid.clone(), evidence_refs: json_refs(evidence),
                    credit: role("introduction_decision.v1", Some(*seq), load_shares(db, *seq)?, Some(if status == "unattributable" { "unattributable" } else { "unattributed" })) },
            })
        }).transpose()?;
        findings.push(FindingFixState { finding_id: id.clone(), status: group.status.clone(), root: group.root.clone(), remediation: remediation.into(),
            verified: proposals().any(|(_, p)| p.verification.is_some()), integrated: !intervals.is_empty(), currently_resolved, resolutions: intervals, reopenings: reopen,
            repairs: own.iter().map(|r| r.repair_seq).collect(), discovery, validation, implementation, verification, integration, introduction, discovered_unix_ms: discovered });
    }

    let history = history(db, at)?;
    Ok(Some(FixState { head_seq: triage.head_seq, as_of_seq: at, findings, repairs, history, triage }))
}

fn history(db: &Connection, at: i64) -> Result<Vec<FindingEvent>> {
    let rows: Vec<(i64, String, String, String, Option<i64>, i64)> = db.prepare("SELECT seq,kind,principal,authority,expected_seq,recorded_unix_ms FROM fix_log WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (seq, kind, principal, authority, expected_seq, recorded_unix_ms) in rows {
        let one = |sql: &str| -> Result<serde_json::Value> {
            Ok(db.query_row(sql, [seq], |r| {
                let mut map = serde_json::Map::new();
                for i in 0..r.as_ref().column_count() {
                    let name = r.as_ref().column_name(i)?.to_owned();
                    let value = match r.get_ref(i)? {
                        rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                        rusqlite::types::ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
                        _ => serde_json::Value::Null,
                    };
                    map.insert(name, value);
                }
                Ok(serde_json::Value::Object(map))
            })?)
        };
        let subject = match kind.as_str() {
            "repair_opened" => one("SELECT finding_id,assignment,configuration_id,horizon_ms FROM repair_opportunities WHERE seq=?1")?,
            "attempt_bound" => one("SELECT repair_seq,attempt_id,ordinal,configuration_id FROM repair_attempts WHERE seq=?1")?,
            "proposed" => one("SELECT repair_seq,submission_id,attempt_id,candidate_oid FROM fix_proposals WHERE seq=?1")?,
            "verified" => one("SELECT proposal_seq,run_id,commit_oid,assurance FROM fix_verifications WHERE seq=?1")?,
            "integrated" => one("SELECT proposal_seq,integrated_id,commit_oid FROM fix_integrations WHERE seq=?1")?,
            "repair_closed" => one("SELECT repair_seq,outcome FROM repair_closures WHERE seq=?1")?,
            "reopened" => one("SELECT finding_id,integration_seq,reason,observed_oid FROM fix_reopenings WHERE seq=?1")?,
            "introduced" => one("SELECT finding_id,status,method,introducing_oid FROM introduction_decisions WHERE seq=?1")?,
            "credited" => one("SELECT finding_id,role,proposal_seq FROM credit_allocations WHERE seq=?1")?,
            _ => one("SELECT reverses FROM fix_retractions WHERE seq=?1")?,
        };
        out.push(FindingEvent { seq, kind, principal, authority, expected_seq, recorded_unix_ms, subject });
    }
    Ok(out)
}
