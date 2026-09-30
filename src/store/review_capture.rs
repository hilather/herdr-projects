//! Review capture (migration 0054, docs/telemetry/contracts-review.md, plan
//! TM3.1). An opportunity binds one exact candidate submission; its
//! assignment, the sessions that execute it and each session's completion are
//! separate append-only rows. A completion is the reviewer's receipt, kept as a
//! proposal with declared coverage. Acceptance is a separate decision under a
//! delegated `code_review` grant (`review_authority.rs`, the reviewer-authority migration).
//! Nothing here launches, verifies, integrates or stores
//! review content; references are IDs and digests only.
use super::*;
use crate::domain::agent_configuration;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const OPPORTUNITY_SCHEMA: &str = "review_opportunity.v1";
pub const SESSION_SCHEMA: &str = "review_session.v1";
pub const RECEIPT_SCHEMA: &str = "review_receipt.v1";
/// Deterministic blind cross-provider assignment policy (contracts-review.md §3).
pub const BLIND_POLICY: &str = "blind_cross_provider.v1";
pub const SCOPES: [&str; 3] = ["candidate_diff", "candidate_tree", "contract_scope"];
pub const KINDS: [&str; 5] = ["code", "skeptical", "security", "test", "architecture"];
pub const ROLES: [&str; 3] = ["gate", "evaluation", "advisory"];
pub const OUTCOMES: [&str; 5] = ["completed", "incomplete", "failed", "timed_out", "interrupted"];
/// Reason codes of a session that did not complete; a completed session has none.
pub const END_REASONS: [&str; 5] = ["budget_exhausted", "reviewer_error", "scope_unavailable", "operator_stopped", "unspecified"];
const MAX_RECEIPT_BYTES: usize = 64 * 1024;
const MAX_FINDINGS: usize = 256;
const MAX_REFS: usize = 64;

/// What an operator opens: a review of one submission's exact candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOpportunitySpec {
    pub submission_id: String,
    pub scope: String,
    pub kind: String,
    pub role: String,
    pub protocol: String,
    /// Finding references known before the review (`finding:<ref>`).
    pub prior_findings: Vec<String>,
    pub budget_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewOpportunity {
    pub opportunity_id: String,
    pub submission_id: String,
    pub task_id: String,
    pub contract_revision: i64,
    pub candidate_oid: String,
    pub scope: String,
    pub kind: String,
    pub role: String,
    pub protocol: String,
    pub prior_findings: Vec<String>,
    pub budget_ms: Option<u64>,
    pub creator_principal: String,
    pub created_unix_ms: i64,
}

/// Who reviews: one named retained profile (`operator`), or the blind
/// cross-provider policy over candidate profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAssignmentChoice { Operator { profile: String }, BlindCrossProvider { candidates: Vec<String> } }

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewAssignment {
    pub opportunity_id: String,
    pub policy: String,
    pub reviewer_configuration_id: String,
    pub reviewer_profile_digest: String,
    pub reviewer_family: Option<String>,
    pub author_attempt_id: String,
    pub author_configuration_id: Option<String>,
    pub author_family: Option<String>,
    /// Covariate: `Some(true)` same provider family, `None` unknown.
    pub same_family: Option<bool>,
    pub blind: bool,
    pub reason: String,
    pub eligible: serde_json::Value,
    pub assigner_principal: String,
    pub assigned_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewSession {
    pub session_id: String,
    pub opportunity_id: String,
    pub ordinal: i64,
    pub attempt_id: String,
    pub configuration_id: Option<String>,
    pub matches_assignment: Option<bool>,
    pub same_attempt_as_author: bool,
    pub recorder_principal: String,
    pub started_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewCompletion {
    pub session_id: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub submission_id: String,
    pub candidate_oid: String,
    pub findings_submitted: usize,
    pub finding_refs: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub coverage_basis: String,
    pub trust: String,
    pub receipt_digest: String,
    pub recorder_principal: String,
    pub completed_unix_ms: i64,
    /// True when an identical receipt was already recorded.
    pub replayed: bool,
    /// The finding submission of each reference (store schema 55 and later).
    pub finding_submissions: Vec<i64>,
}

/// The reviewer's receipt. Unknown fields (e.g. a self-declared `accepted`)
/// refuse the receipt: a worker cannot carry acceptance in it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    session_id: String,
    submission_id: String,
    candidate_oid: String,
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
    /// `finding:<ref>`, or `{"ref": "finding:<ref>", "title": "..."}` (the
    /// title is kept only as a contracts §7 excerpt). Nothing else: a finding
    /// entry that carries a decision or severity is refused.
    #[serde(default)]
    findings: Vec<serde_json::Value>,
    #[serde(default)]
    evidence: Vec<String>,
}

/// Finding references of a receipt, and the excerpted title of each titled one.
fn receipt_findings(entries: &[serde_json::Value]) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let mut refs = Vec::with_capacity(entries.len());
    let mut titles = BTreeMap::new();
    for entry in entries {
        match entry {
            serde_json::Value::String(r) => refs.push(r.clone()),
            serde_json::Value::Object(map) => {
                if let Some(field) = map.keys().find(|k| !["ref", "title"].contains(&k.as_str())) { return Err(invalid(format!("invalid review receipt: unknown field `{field}` in a finding"))); }
                let Some(r) = map.get("ref").and_then(|v| v.as_str()) else { return Err(invalid("invalid review receipt: a finding object needs a string `ref`".into())) };
                match map.get("title") {
                    None | Some(serde_json::Value::Null) => {}
                    Some(serde_json::Value::String(t)) => { if let Some(t) = super::finding_triage::title_excerpt(t) { titles.insert(r.to_owned(), t); } }
                    Some(_) => return Err(invalid("invalid review receipt: a finding title is a string".into())),
                }
                refs.push(r.to_owned());
            }
            _ => return Err(invalid("invalid review receipt: a finding is a reference or {ref, title}".into())),
        }
    }
    Ok((refs, titles))
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_54(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 54 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

fn principal_ok(principal: &str) -> Result<()> {
    if principal.is_empty() || principal.len() > 128 { return Err(invalid("invalid principal".into())); }
    Ok(())
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
fn hex64(value: &str) -> bool { value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) }

/// `finding:<token>`: a reference, never a title or text.
fn finding_ref(value: &str) -> bool { value.strip_prefix("finding:").is_some_and(|v| token(v, 64)) }
/// `sha256:<hex64>` (content-addressed evidence held elsewhere) or `verification_run:<hex64>`.
fn evidence_ref(value: &str) -> bool {
    value.strip_prefix("sha256:").or_else(|| value.strip_prefix("verification_run:")).is_some_and(hex64)
}

/// Sorted, distinct references that each satisfy `ok`.
fn refs(values: &[String], ok: fn(&str) -> bool, max: usize, what: &str) -> Result<Vec<String>> {
    if values.len() > max { return Err(invalid(format!("at most {max} {what} references"))); }
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != values.len() { return Err(invalid(format!("duplicate {what} reference"))); }
    if let Some(bad) = sorted.iter().position(|v| !ok(v)) { return Err(invalid(format!("{what} reference {} is not an allowed reference", bad + 1))); }
    Ok(sorted)
}

fn digest(bytes: &str) -> String { format!("sha256:{:x}", Sha256::digest(bytes.as_bytes())) }

/// Provider family of a configuration's product kind (`product_family.v1`).
/// Model providers are not collected (contracts-collection.md A3), so the
/// family is the product's vendor; any other kind is unknown.
fn family(tx: &Connection, configuration: &str) -> Result<Option<String>> {
    let json: Option<String> = tx.query_row("SELECT canonical_json FROM agent_configurations WHERE configuration_id=?1", [configuration], |r| r.get(0)).optional()?;
    let kind = json.and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok()).and_then(|v| v["kind"].as_str().map(str::to_owned));
    Ok(kind.as_deref().and_then(kind_family).map(str::to_owned))
}
fn kind_family(kind: &str) -> Option<&'static str> {
    match kind { "codex" => Some("openai"), "claude" => Some("anthropic"), _ => None }
}

/// The latest retained native profile named `name`, as `(profile_digest, profile)`.
pub(super) fn retained_profile(tx: &Connection, name: &str) -> Result<(String, FrozenProfile)> {
    let mut stmt = tx.prepare("SELECT profile_digest,report,report_digest FROM native_profiles ORDER BY sequence DESC LIMIT 256")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (digest, report, report_digest): (String, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
        if format!("{:x}", Sha256::digest(report.as_bytes())) != report_digest { return Err(StoreError::Corrupt("native profile report digest mismatch".into())); }
        let value: serde_json::Value = serde_json::from_str(&report).map_err(|_| StoreError::Corrupt("invalid native profile report".into()))?;
        if value["preparation"]["profile"]["name"].as_str() != Some(name) { continue; }
        let profile: FrozenProfile = serde_json::from_value(value["preparation"]["profile"].clone()).map_err(|_| StoreError::Corrupt("invalid retained profile".into()))?;
        return Ok((digest, profile));
    }
    Err(invalid(format!("no retained native profile named {name}")))
}

/// `(submission_id, candidate_oid, author attempt)` of an opportunity.
fn opportunity_binding(tx: &Connection, opportunity: &str) -> Result<(String, String, String)> {
    tx.query_row("SELECT o.submission_id,o.candidate_oid,s.attempt_id FROM review_opportunities o JOIN result_submissions s ON s.submission_id=o.submission_id WHERE o.opportunity_id=?1",
        [opportunity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
        .ok_or_else(|| invalid(format!("no review opportunity {opportunity}")))
}

/// Refuse a session of `opportunity` by `attempt` unless one may start now:
/// assigned, not revealed, every earlier session ended without completing
/// it, and `attempt` has none yet. Returns the assigned configuration and
/// the author attempt. Writes nothing (a launch draft checks it too).
pub(super) fn session_admissible(tx: &Connection, opportunity: &str, attempt: Option<&str>) -> Result<(String, String, i64)> {
    let (submission, _, author) = opportunity_binding(tx, opportunity)?;
    super::seeded_defects::refuse_review_after_reveal(tx, &submission)?;
    let assigned: Option<String> = tx.query_row("SELECT reviewer_configuration_id FROM review_assignments WHERE opportunity_id=?1", [opportunity], |r| r.get(0)).optional()?;
    let Some(assigned) = assigned else { return Err(invalid(format!("review opportunity {opportunity} is not assigned"))) };
    let sessions: Vec<(String, String, Option<String>)> = tx.prepare("SELECT r.session_id,r.attempt_id,c.outcome FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=?1 ORDER BY r.ordinal")?
        .query_map([opportunity], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    if let Some(open) = sessions.iter().find(|s| s.2.is_none()) { return Err(invalid(format!("review session {} has no completion yet", open.0))); }
    if sessions.iter().any(|s| s.2.as_deref() == Some("completed")) { return Err(invalid(format!("review opportunity {opportunity} is already completed"))); }
    if let Some(attempt) = attempt && sessions.iter().any(|s| s.1 == attempt) { return Err(invalid(format!("attempt {attempt} already has a session for this opportunity"))); }
    Ok((assigned, author, sessions.len() as i64 + 1))
}

/// Record a session start in the caller's transaction (the CLI's own, or the
/// reservation that launches the reviewing attempt).
pub(super) fn start_session_in(tx: &Connection, opportunity: &str, attempt: &str, principal: &str, now: i64) -> Result<ReviewSession> {
    principal_ok(principal)?;
    let (assigned, author, ordinal) = session_admissible(tx, opportunity, Some(attempt))?;
    if !tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1)", [attempt], |r| r.get::<_, bool>(0))? { return Err(invalid(format!("no attempt {attempt}"))); }
    let configuration: Option<String> = tx.query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [attempt], |r| r.get(0)).optional()?;
    let matches_assignment = configuration.as_ref().map(|c| *c == assigned);
    let same_attempt_as_author = attempt == author;
    let session_id = digest(&serde_json::json!({"attempt_id": attempt, "opportunity_id": opportunity, "ordinal": ordinal, "schema": SESSION_SCHEMA, "started_unix_ms": now}).to_string());
    tx.execute("INSERT INTO review_sessions(session_id,opportunity_id,ordinal,attempt_id,configuration_id,matches_assignment,same_attempt_as_author,recorder_principal,started_unix_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![session_id, opportunity, ordinal, attempt, configuration, matches_assignment, same_attempt_as_author, principal, now])?;
    // The start takes the next seq of the shared ledger (store schema 59 and later).
    super::review_ledger::record_session_event(tx, &session_id, "started", principal, now)?;
    Ok(ReviewSession { session_id, opportunity_id: opportunity.to_owned(), ordinal, attempt_id: attempt.to_owned(), configuration_id: configuration,
        matches_assignment, same_attempt_as_author, recorder_principal: principal.to_owned(), started_unix_ms: now })
}

impl SqliteStore {
    /// Open a review opportunity on `spec.submission_id`'s exact candidate,
    /// task and contract revision. Opening assigns and runs nothing.
    pub fn open_review_opportunity(&mut self, spec: &ReviewOpportunitySpec, principal: &str, now: i64) -> Result<ReviewOpportunity> {
        principal_ok(principal)?;
        for (value, allowed, what) in [(&spec.scope, &SCOPES[..], "scope"), (&spec.kind, &KINDS[..], "kind"), (&spec.role, &ROLES[..], "role")] {
            if !allowed.contains(&value.as_str()) { return Err(invalid(format!("unknown review {what} {value}"))); }
        }
        if !token(&spec.protocol, 64) { return Err(invalid("protocol must be a lowercase identifier such as review-protocol.v1".into())); }
        if spec.budget_ms == Some(0) { return Err(invalid("budget must be positive".into())); }
        let budget = spec.budget_ms.map(integer).transpose()?;
        let prior = refs(&spec.prior_findings, finding_ref, MAX_REFS, "prior finding")?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_54(&tx)?;
        let (task, revision, candidate): (String, i64, String) = tx.query_row("SELECT task_id,contract_revision,candidate_oid FROM result_submissions WHERE submission_id=?1",
            [&spec.submission_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?
            .ok_or_else(|| invalid(format!("no result submission {}", spec.submission_id)))?;
        super::seeded_defects::refuse_review_after_reveal(&tx, &spec.submission_id)?;
        let canonical_json = serde_json::json!({"budget_ms": budget, "candidate_oid": candidate, "contract_revision": revision, "created_unix_ms": now,
            "kind": spec.kind, "prior_findings": prior, "protocol": spec.protocol, "role": spec.role, "schema": OPPORTUNITY_SCHEMA,
            "scope": spec.scope, "submission_id": spec.submission_id, "task_id": task}).to_string();
        let opportunity_id = digest(&canonical_json);
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_opportunities WHERE opportunity_id=?1)", [&opportunity_id], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("review opportunity {opportunity_id} already exists")));
        }
        tx.execute("INSERT INTO review_opportunities(opportunity_id,submission_id,task_id,contract_revision,candidate_oid,scope,kind,role,protocol,prior_findings,budget_ms,creator_principal,canonical_json,created_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![opportunity_id, spec.submission_id, task, revision, candidate, spec.scope, spec.kind, spec.role, spec.protocol, serde_json::json!(prior).to_string(), budget, principal, canonical_json, now])?;
        // The opening takes the next seq of the shared ledger (store schema 63 and later).
        super::review_ledger::record_opportunity_event(&tx, &opportunity_id, "opened", principal, now)?;
        tx.commit()?;
        Ok(ReviewOpportunity { opportunity_id, submission_id: spec.submission_id.clone(), task_id: task, contract_revision: revision, candidate_oid: candidate,
            scope: spec.scope.clone(), kind: spec.kind.clone(), role: spec.role.clone(), protocol: spec.protocol.clone(), prior_findings: prior,
            budget_ms: spec.budget_ms, creator_principal: principal.to_owned(), created_unix_ms: now })
    }

    /// Assign the opportunity's reviewer once, before any session. The blind
    /// policy picks deterministically, needs no model call, and records
    /// same-family review as a covariate (contracts-review.md §3).
    pub fn assign_review(&mut self, opportunity: &str, choice: &ReviewAssignmentChoice, principal: &str, now: i64) -> Result<ReviewAssignment> {
        principal_ok(principal)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_54(&tx)?;
        let (_, _, author) = opportunity_binding(&tx, opportunity)?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_assignments WHERE opportunity_id=?1)", [opportunity], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("review opportunity {opportunity} is already assigned")));
        }
        let author_configuration: Option<String> = tx.query_row("SELECT chosen_configuration_id FROM dispatch_decisions WHERE attempt_id=?1", [&author], |r| r.get(0)).optional()?;
        let author_family = match &author_configuration { Some(c) => family(&tx, c)?, None => None };
        let (policy, names) = match choice {
            ReviewAssignmentChoice::Operator { profile } => ("operator", std::slice::from_ref(profile)),
            ReviewAssignmentChoice::BlindCrossProvider { candidates } => (BLIND_POLICY, candidates.as_slice()),
        };
        if names.is_empty() || names.len() > 16 { return Err(invalid("the blind policy weighs 1 to 16 candidate profiles".into())); }
        // (configuration_id, profile_digest, family)
        let mut candidates: Vec<(String, String, Option<String>)> = Vec::with_capacity(names.len());
        for name in names {
            let (profile_digest, profile) = retained_profile(&tx, name)?;
            let configuration = agent_configuration(&profile);
            if candidates.iter().any(|c| c.0 == configuration.id) { return Err(invalid(format!("candidate {name} repeats an earlier configuration"))); }
            super::dispatch_log::insert_configuration(&tx, &configuration, now)?;
            candidates.push((configuration.id, profile_digest, kind_family(&profile.kind).map(str::to_owned)));
        }
        let cross = |c: &(String, String, Option<String>)| author_family.is_some() && c.2.is_some() && c.2 != author_family;
        let (chosen, reason) = if policy == "operator" { (0, "operator_selected") } else {
            let key = |c: &(String, String, Option<String>)| format!("{:x}", Sha256::digest(format!("review_assignment.v1:{opportunity}:{}", c.0).as_bytes()));
            let pool: Vec<usize> = if candidates.iter().any(cross) { (0..candidates.len()).filter(|&i| cross(&candidates[i])).collect() } else { (0..candidates.len()).collect() };
            let chosen = pool.into_iter().min_by_key(|&i| key(&candidates[i])).unwrap_or(0);
            let reason = if cross(&candidates[chosen]) { "cross_provider" } else if author_family.is_none() { "author_family_unknown" } else { "no_cross_provider_eligible" };
            (chosen, reason)
        };
        let status = |i: usize, c: &(String, String, Option<String>)| if i == chosen { "chosen" } else if c.2.is_none() || author_family.is_none() { "family_unknown" }
            else if cross(c) { "eligible" } else { "same_family" };
        let eligible = serde_json::Value::Array(candidates.iter().enumerate().map(|(i, c)| serde_json::json!({"configuration_id": c.0, "profile_digest": c.1,
            "family": c.2, "status": status(i, c)})).collect());
        let (configuration, profile_digest, reviewer_family) = candidates.swap_remove(chosen);
        let same_family = match (&author_family, &reviewer_family) { (Some(a), Some(r)) => Some(a == r), _ => None };
        let blind = policy == BLIND_POLICY;
        tx.execute("INSERT INTO review_assignments(opportunity_id,policy,reviewer_configuration_id,reviewer_profile_digest,reviewer_family,author_attempt_id,author_configuration_id,author_family,same_family,blind,reason,eligible,assigner_principal,assigned_unix_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![opportunity, policy, configuration, profile_digest, reviewer_family, author, author_configuration, author_family, same_family, blind, reason, eligible.to_string(), principal, now])?;
        // So does the assignment (store schema 63 and later).
        super::review_ledger::record_opportunity_event(&tx, opportunity, "assigned", principal, now)?;
        tx.commit()?;
        Ok(ReviewAssignment { opportunity_id: opportunity.to_owned(), policy: policy.to_owned(), reviewer_configuration_id: configuration, reviewer_profile_digest: profile_digest,
            reviewer_family, author_attempt_id: author, author_configuration_id: author_configuration, author_family, same_family, blind, reason: reason.to_owned(),
            eligible, assigner_principal: principal.to_owned(), assigned_unix_ms: now })
    }

    /// Record that `attempt` started reviewing an assigned opportunity. The
    /// controller records session identity; a restart is a new session of the
    /// same opportunity, allowed only once every earlier session has ended
    /// without completing it.
    pub fn start_review_session(&mut self, opportunity: &str, attempt: &str, principal: &str, now: i64) -> Result<ReviewSession> {
        principal_ok(principal)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_54(&tx)?;
        let session = start_session_in(&tx, opportunity, attempt, principal, now)?;
        tx.commit()?;
        Ok(session)
    }

    /// Record a session's end from the reviewer's receipt (`review_receipt.v1`),
    /// as a proposal with declared coverage. The receipt must name the
    /// opportunity's exact submission and candidate; a completed review with no
    /// findings is valid. An identical receipt replays; a different one refuses.
    pub fn complete_review_session(&mut self, receipt: &[u8], principal: &str, now: i64) -> Result<ReviewCompletion> {
        principal_ok(principal)?;
        self.complete_as(receipt, Some(principal), now)
    }

    /// The reviewing worker's channel (contracts-review.md §11): the receipt of
    /// a session the controller recorded at launch, while its attempt has not
    /// ended, recorded as `worker:<attempt>`. Like any completion it is a
    /// proposal with declared coverage; nothing a worker sends accepts,
    /// rejects or triages a review.
    pub fn submit_review_receipt(&mut self, receipt: &[u8], now: i64) -> Result<ReviewCompletion> {
        self.complete_as(receipt, None, now)
    }

    /// `principal` `None`: the launched session's own worker.
    fn complete_as(&mut self, receipt: &[u8], principal: Option<&str>, now: i64) -> Result<ReviewCompletion> {
        if receipt.len() > MAX_RECEIPT_BYTES { return Err(invalid(format!("review receipt exceeds {MAX_RECEIPT_BYTES} bytes"))); }
        let r: Receipt = serde_json::from_slice(receipt).map_err(|e| invalid(format!("invalid review receipt: {e}")))?;
        if r.schema != RECEIPT_SCHEMA { return Err(invalid(format!("review receipt schema must be {RECEIPT_SCHEMA}"))); }
        if !OUTCOMES.contains(&r.outcome.as_str()) { return Err(invalid(format!("unknown review outcome {}", r.outcome))); }
        let reason = match (r.outcome.as_str(), r.reason) {
            ("completed", None) => None,
            ("completed", Some(_)) => return Err(invalid("a completed review has no end reason".into())),
            (_, None) => Some("unspecified".to_owned()),
            (_, Some(reason)) if END_REASONS.contains(&reason.as_str()) => Some(reason),
            (_, Some(reason)) => return Err(invalid(format!("unknown review end reason {reason}"))),
        };
        let (findings, titles) = receipt_findings(&r.findings)?;
        let findings = refs(&findings, finding_ref, MAX_FINDINGS, "finding")?;
        let evidence = refs(&r.evidence, evidence_ref, MAX_REFS, "evidence")?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_54(&tx)?;
        let (opportunity, attempt): (String, String) = tx.query_row("SELECT opportunity_id,attempt_id FROM review_sessions WHERE session_id=?1", [&r.session_id], |row| Ok((row.get(0)?, row.get(1)?))).optional()?
            .ok_or_else(|| invalid(format!("no review session {}", r.session_id)))?;
        let worker = format!("worker:{attempt}");
        let principal = match principal {
            Some(principal) => principal,
            None => {
                super::review_launch::worker_channel_open(&tx, &r.session_id, &attempt)?;
                principal_ok(&worker)?;
                worker.as_str()
            }
        };
        let (submission, candidate, _) = opportunity_binding(&tx, &opportunity)?;
        if r.submission_id != submission || r.candidate_oid != candidate {
            return Err(invalid(format!("review receipt names another candidate: session {} reviews submission {submission} at {candidate}", r.session_id)));
        }
        let mut normalized = serde_json::json!({"candidate_oid": candidate, "evidence": evidence, "findings": findings, "outcome": r.outcome,
            "reason": reason, "schema": RECEIPT_SCHEMA, "session_id": r.session_id, "submission_id": submission});
        // Titles enter the digest only when present, so untitled receipts keep their digest.
        if !titles.is_empty() { normalized["titles"] = serde_json::json!(titles); }
        let receipt_digest = digest(&normalized.to_string());
        let existing: Option<(String, String, i64)> = tx.query_row("SELECT receipt_digest,recorder_principal,completed_unix_ms FROM review_completions WHERE session_id=?1", [&r.session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
        let (recorder, completed_at, replayed, submissions) = match existing {
            Some((stored, _, _)) if stored != receipt_digest => return Err(invalid(format!("review session {} already has a different completion", r.session_id))),
            Some((_, recorder, at)) => (recorder, at, true, super::finding_triage::session_submissions(&tx, &r.session_id)?),
            None => {
                tx.execute("INSERT INTO review_completions(session_id,outcome,reason,submission_id,candidate_oid,findings_submitted,finding_refs,evidence_refs,coverage_basis,trust,receipt_digest,recorder_principal,completed_unix_ms)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'declared','proposal',?9,?10,?11)",
                    params![r.session_id, r.outcome, reason, submission, candidate, findings.len() as i64, serde_json::json!(findings).to_string(), serde_json::json!(evidence).to_string(), receipt_digest, principal, now])?;
                // The completion takes the next seq, before its finding submissions (store schema 59 and later).
                super::review_ledger::record_session_event(&tx, &r.session_id, "completed", principal, now)?;
                // Each finding reference becomes a submission (a proposal) once the store has finding triage.
                let version: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                let submissions = if version >= 55 { super::finding_triage::record_submissions(&tx, &r.session_id, &findings, &titles, principal, now)? } else { Vec::new() };
                tx.commit()?;
                (principal.to_owned(), now, false, submissions)
            }
        };
        Ok(ReviewCompletion { session_id: r.session_id, outcome: r.outcome, reason, submission_id: submission, candidate_oid: candidate, findings_submitted: findings.len(),
            finding_refs: findings, evidence_refs: evidence, coverage_basis: "declared".into(), trust: "proposal".into(), receipt_digest,
            recorder_principal: recorder, completed_unix_ms: completed_at, replayed, finding_submissions: submissions })
    }
}
