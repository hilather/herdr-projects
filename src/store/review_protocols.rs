//! Review protocols and experiments (migration 0057,
//! docs/telemetry/contracts-review.md §7, plan TM3.4). A versioned protocol
//! (`review_protocol.v1`) fixes a second review's method, scope, reviewer
//! role, budget, prior disclosure and outcome criteria. A pass binds one
//! opportunity run under it to the ordinary reviews it follows before its
//! review starts, freezing the ledger cutoff whose validated findings count as
//! known, whether every prior examined the same exact artifact, and whether
//! the prior coverage was complete. Its incremental yield counts only new
//! validated unique findings (§5 discoveries whose group root was not known):
//! a reworded duplicate, or a finding later merged into a known one, is a
//! rediscovery. A preregistered experiment (`review_experiment.v1`) freezes
//! its design, exact eligibility, arms, assignment rule, outcome and minimum
//! units; each unit is assigned before its outcome exists, and exclusions and
//! crossover stay recorded. Nothing here opens, assigns or launches a review:
//! an arm is a label, never a routing decision.
//!
//! Every change is one append-only `protocol_log` row in the one ordering of
//! `finding_log` and `fix_log`; [`protocol_state`] replays it to any
//! sequence. Only the triage authority (the project owner at the CLI) writes.
use super::finding_triage::{self, FindingEvent, FindingState, SEVERITIES, TRIAGE_AUTHORITY};
use super::review_ledger;
use super::*;
use crate::domain::agent_configuration;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const PROTOCOL_SCHEMA: &str = "review_protocol.v1";
pub const EXPERIMENT_SCHEMA: &str = "review_experiment.v1";
/// A pass's outcome: validated unique findings not known at its cutoff.
pub const PASS_OUTCOME: &str = "new_validated_unique_findings.v1";
/// An experiment unit's outcome: validated unique findings its reviews discovered.
pub const EXPERIMENT_OUTCOME: &str = "validated_unique_findings.v1";
pub const ADJUDICATION: &str = "owner_triage.v1";
pub const STOPPING_RULES: [&str; 3] = ["budget_exhausted", "checklist_complete", "first_blocking_finding"];
pub const EXPERIMENT_STOPPING_RULES: [&str; 2] = ["fixed_horizon", "planned_units"];
pub const EXCLUSION_REASONS: [&str; 4] = ["ineligible_discovered", "artifact_withdrawn", "protocol_violation", "operator_error"];
/// Provisional minimum analyzable units per arm (plan doc 07 §6).
pub const DEFAULT_MIN_UNITS: u32 = 10;
const DAY_MS: i64 = 86_400_000;
const MAX_DEFINITION_BYTES: usize = 8 * 1024;
const MAX_TOKENS: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolDefinition {
    schema: String,
    protocol: String,
    kind: String,
    scope: String,
    role: String,
    /// Assumptions to challenge, e.g. `unsupported_claims`.
    challenges: Vec<String>,
    /// Failure classes to inspect, e.g. `concurrency`.
    failure_classes: Vec<String>,
    permitted_tools: Vec<String>,
    budget_ms: u64,
    evidence_min: u32,
    stopping_rule: String,
    prior_disclosure: String,
    #[serde(default)]
    reviewer_profile: Option<String>,
    outcome: OutcomeCriteria,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeCriteria { primary: String, adjudication: String, severity_policy: String, min_severity: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExperimentDefinition {
    schema: String,
    experiment: String,
    design: String,
    #[serde(default)]
    seed: Option<String>,
    eligibility: Eligibility,
    arms: Vec<ArmDefinition>,
    primary_outcome: String,
    adjudication: String,
    horizon_days: u32,
    #[serde(default)]
    min_units: Option<u32>,
    #[serde(default)]
    match_on: Vec<String>,
    stopping_rule: String,
    #[serde(default)]
    planned_units: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Eligibility { kind: String, scope: String, role: String, protocol: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmDefinition { arm: String, protocol: Option<String> }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewProtocol {
    pub seq: i64,
    pub protocol: String,
    pub definition_digest: String,
    pub definition: serde_json::Value,
    pub registered_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PassClaim {
    pub submission_id: i64,
    pub claim_id: i64,
    /// The claim's derived §5 outcome at the watermark.
    pub outcome: String,
    pub canonical_finding: Option<String>,
    pub severity: Option<String>,
    /// `new`, `rediscovered`, `known`, `below_severity_floor`, `rejected` or `pending`.
    pub incremental: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PassState {
    pub seq: i64,
    pub opportunity_id: String,
    pub protocol: String,
    pub submission_id: String,
    pub candidate_oid: String,
    pub bound_unix_ms: i64,
    pub cutoff_seq: i64,
    /// Each prior: `{opportunity_id, submission_id, candidate_oid, scope, artifact, status}` at binding.
    pub priors: serde_json::Value,
    /// `same_artifact`, `changed_artifact` or `different_scope`.
    pub comparability: String,
    /// `complete` when every prior was completed at binding.
    pub prior_coverage: String,
    pub prior_disclosure: String,
    /// The pass opportunity's review status (contracts-review.md §4).
    pub status: String,
    /// Group roots, at the watermark, of the findings known at the cutoff.
    pub known_findings: Vec<String>,
    pub claims: Vec<PassClaim>,
    pub new_unique_findings: Vec<String>,
    pub rediscovered: usize,
    pub eligible: bool,
    /// Why the pass is outside M28 (first failing rule), when it is.
    pub exclusion: Option<String>,
    /// The retraction that reversed this binding (recorded in error), by the watermark.
    pub retracted_seq: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnitState {
    pub seq: i64,
    pub opportunity_id: String,
    pub submission_id: String,
    pub arm: String,
    pub block: Option<String>,
    pub assigned_unix_ms: i64,
    pub cutoff_seq: i64,
    /// Pass opportunities that name this unit's opportunity as a prior.
    pub passes: Vec<String>,
    /// For an arm with a protocol: a pass under it exists; `null` for an arm without one.
    pub treatment_received: Option<bool>,
    /// A pass under another protocol than the arm's (or any pass in an arm without one).
    pub crossover: bool,
    pub exclusion: Option<serde_json::Value>,
    /// `analyzable`, `pending`, `censored` or `excluded`.
    pub status: String,
    pub outcome: Option<usize>,
    pub new_unique_findings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExperimentState {
    pub seq: i64,
    pub experiment: String,
    pub design: String,
    pub seed: Option<String>,
    pub definition_digest: String,
    pub definition: serde_json::Value,
    pub reference_arm: String,
    pub min_units: i64,
    pub horizon_ms: i64,
    pub registered_unix_ms: i64,
    pub units: Vec<UnitState>,
    pub estimate: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProtocolState {
    pub head_seq: i64,
    pub as_of_seq: i64,
    pub protocols: Vec<ReviewProtocol>,
    pub passes: Vec<PassState>,
    pub experiments: Vec<ExperimentState>,
    /// Every `protocol_log` row up to the watermark.
    pub history: Vec<FindingEvent>,
}

fn invalid(message: String) -> StoreError { StoreError::Invalid(message) }

fn schema_57(tx: &Connection) -> Result<()> {
    check_schema(tx)?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 57 { return Err(StoreError::UnsupportedSchema(version)); }
    Ok(())
}

fn token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

fn identifier(value: &str, what: &str) -> Result<()> {
    if token(value) { Ok(()) } else { Err(invalid(format!("{what} must be a lowercase identifier such as skeptical-challenge.v1"))) }
}

/// Sorted distinct tokens, `min..=16` of them.
fn tokens(values: &[String], what: &str, min: usize) -> Result<Vec<String>> {
    if values.len() < min || values.len() > MAX_TOKENS { return Err(invalid(format!("{what} has {min} to {MAX_TOKENS} entries"))); }
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.len() != values.len() { return Err(invalid(format!("duplicate {what} entry"))); }
    if let Some(bad) = sorted.iter().find(|v| !token(v)) { return Err(invalid(format!("{what} entry {bad:?} is not a lowercase identifier"))); }
    Ok(sorted)
}

fn one_of(value: &str, allowed: &[&str], what: &str) -> Result<()> {
    if allowed.contains(&value) { Ok(()) } else { Err(invalid(format!("unknown {what} {value}"))) }
}

fn digest(bytes: &str) -> String { format!("sha256:{:x}", Sha256::digest(bytes.as_bytes())) }

fn severity_rank(severity: &str) -> usize { SEVERITIES.iter().position(|s| *s == severity).unwrap_or(SEVERITIES.len()) }

/// Append the owner's protocol row after the authority and expected-head checks.
fn log(tx: &Connection, kind: &str, principal: &str, expected: Option<i64>, now: i64) -> Result<i64> {
    finding_triage::triage_authority(tx, principal)?;
    let head = finding_triage::head(tx)?;
    if let Some(expected) = expected && head != expected { return Err(invalid(format!("finding history moved: head is {head}, expected {expected}"))); }
    let seq = head + 1;
    tx.execute("INSERT INTO protocol_log(seq,kind,principal,authority,expected_seq,recorded_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![seq, kind, principal, TRIAGE_AUTHORITY, expected, now])?;
    Ok(seq)
}

fn done(tx: rusqlite::Transaction<'_>, seq: i64, subject: serde_json::Value) -> Result<FindingEvent> {
    let out = tx.query_row("SELECT kind,principal,authority,expected_seq,recorded_unix_ms FROM protocol_log WHERE seq=?1", [seq],
        |r| Ok(FindingEvent { seq, kind: r.get(0)?, principal: r.get(1)?, authority: r.get(2)?, expected_seq: r.get(3)?, recorded_unix_ms: r.get(4)?, subject }))?;
    tx.commit()?;
    Ok(out)
}

/// `(submission_id, task_id, candidate_oid, scope, kind, role, protocol, budget_ms, created_unix_ms)` of an opportunity.
type OpportunityRow = (String, String, String, String, String, String, String, Option<i64>, i64);
fn opportunity(db: &Connection, id: &str) -> Result<OpportunityRow> {
    db.query_row("SELECT submission_id,task_id,candidate_oid,scope,kind,role,protocol,budget_ms,created_unix_ms FROM review_opportunities WHERE opportunity_id=?1", [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))).optional()?
        .ok_or_else(|| invalid(format!("no review opportunity {id}")))
}

/// The arm a randomized experiment's recorded seed gives an artifact.
fn randomized_arm(seed: &str, submission: &str, arms: usize) -> usize {
    let hash = format!("{:x}", Sha256::digest(format!("{EXPERIMENT_SCHEMA}:{seed}:{submission}").as_bytes()));
    (u64::from_str_radix(&hash[..16], 16).unwrap_or(0) % arms as u64) as usize
}

impl SqliteStore {
    /// Register a versioned review protocol (`review_protocol.v1` JSON). A
    /// protocol is immutable; a changed definition needs a new identifier.
    pub fn register_review_protocol(&mut self, definition: &[u8], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if definition.len() > MAX_DEFINITION_BYTES { return Err(invalid(format!("protocol definition exceeds {MAX_DEFINITION_BYTES} bytes"))); }
        let d: ProtocolDefinition = serde_json::from_slice(definition).map_err(|e| invalid(format!("invalid review protocol: {e}")))?;
        if d.schema != PROTOCOL_SCHEMA { return Err(invalid(format!("review protocol schema must be {PROTOCOL_SCHEMA}"))); }
        identifier(&d.protocol, "protocol")?;
        one_of(&d.kind, &super::review_capture::KINDS, "review kind")?;
        one_of(&d.scope, &super::review_capture::SCOPES, "review scope")?;
        one_of(&d.role, &super::review_capture::ROLES, "review role")?;
        one_of(&d.stopping_rule, &STOPPING_RULES, "stopping rule")?;
        one_of(&d.prior_disclosure, &["withheld", "disclosed"], "prior disclosure")?;
        one_of(&d.outcome.primary, &[PASS_OUTCOME], "primary outcome")?;
        one_of(&d.outcome.adjudication, &[ADJUDICATION], "adjudication")?;
        one_of(&d.outcome.severity_policy, &[finding_triage::SEVERITY_POLICY], "severity policy")?;
        one_of(&d.outcome.min_severity, &SEVERITIES, "severity")?;
        if d.budget_ms == 0 { return Err(invalid("a protocol's budget must be positive".into())); }
        if d.evidence_min > 64 { return Err(invalid("evidence_min is 0 to 64".into())); }
        let (challenges, failure_classes, tools) = (tokens(&d.challenges, "challenges", 1)?, tokens(&d.failure_classes, "failure_classes", 1)?, tokens(&d.permitted_tools, "permitted_tools", 0)?);
        let budget = integer(d.budget_ms)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_protocols WHERE protocol=?1)", [&d.protocol], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("protocol {} is already registered: a changed protocol needs a new versioned identifier", d.protocol)));
        }
        let reviewer = match &d.reviewer_profile {
            None => None,
            Some(name) => {
                let (_, profile) = super::review_capture::retained_profile(&tx, name)?;
                let configuration = agent_configuration(&profile);
                super::dispatch_log::insert_configuration(&tx, &configuration, now)?;
                Some(configuration.id)
            }
        };
        let canonical = serde_json::json!({"budget_ms": budget, "challenges": challenges, "evidence_min": d.evidence_min, "failure_classes": failure_classes,
            "kind": d.kind, "outcome": {"adjudication": d.outcome.adjudication, "min_severity": d.outcome.min_severity, "primary": d.outcome.primary,
            "severity_policy": d.outcome.severity_policy}, "permitted_tools": tools, "prior_disclosure": d.prior_disclosure, "protocol": d.protocol,
            "reviewer_configuration_id": reviewer, "role": d.role, "schema": PROTOCOL_SCHEMA, "scope": d.scope, "stopping_rule": d.stopping_rule}).to_string();
        let definition_digest = digest(&canonical);
        let seq = log(&tx, "protocol_registered", principal, expected_seq, now)?;
        tx.execute("INSERT INTO review_protocols(seq,protocol,kind,scope,role,budget_ms,prior_disclosure,evidence_min,min_severity,reviewer_configuration_id,definition_digest,canonical_json)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![seq, d.protocol, d.kind, d.scope, d.role, budget, d.prior_disclosure, d.evidence_min, d.outcome.min_severity, reviewer, definition_digest, canonical])?;
        done(tx, seq, serde_json::json!({"protocol": d.protocol, "definition_digest": definition_digest}))
    }

    /// Bind `opportunity` as a pass of its registered protocol after the
    /// ordinary reviews `priors`, before its review starts. Freezes the known
    /// findings cutoff, each prior's artifact comparability and status.
    pub fn bind_review_pass(&mut self, opportunity_id: &str, priors: &[String], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if priors.is_empty() || priors.len() > MAX_TOKENS { return Err(invalid(format!("a pass names 1 to {MAX_TOKENS} prior opportunities"))); }
        let mut sorted = priors.to_vec();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != priors.len() { return Err(invalid("duplicate prior opportunity".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let (submission, task, candidate, scope, kind, role, protocol, budget, created) = opportunity(&tx, opportunity_id)?;
        let registered: Option<(i64, String, String, String, i64, i64)> = tx.query_row("SELECT p.seq,p.kind,p.scope,p.role,p.budget_ms,l.recorded_unix_ms FROM review_protocols p JOIN protocol_log l ON l.seq=p.seq WHERE p.protocol=?1",
            [&protocol], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
        let Some((protocol_seq, p_kind, p_scope, p_role, p_budget, registered_at)) = registered else {
            return Err(invalid(format!("protocol {protocol} is not registered: a pass runs under a registered protocol")));
        };
        for (what, have, want) in [("kind", &kind, &p_kind), ("scope", &scope, &p_scope), ("role", &role, &p_role)] {
            if have != want { return Err(invalid(format!("opportunity {what} {have} differs from protocol {protocol}'s {want}"))); }
        }
        if budget != Some(p_budget) { return Err(invalid(format!("opportunity budget {budget:?} differs from protocol {protocol}'s assigned budget {p_budget}"))); }
        if created < registered_at { return Err(invalid(format!("opportunity {opportunity_id} was opened before protocol {protocol} was registered"))); }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_sessions WHERE opportunity_id=?1)", [opportunity_id], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("opportunity {opportunity_id} already has a session: a pass is bound before its review starts")));
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM skeptical_passes WHERE opportunity_id=?1)", [opportunity_id], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("opportunity {opportunity_id} is already a bound pass")));
        }
        let mut records = Vec::with_capacity(sorted.len());
        let lifecycle = review_ledger::Lifecycle::at(&tx, None)?;
        for prior in &sorted {
            if prior == opportunity_id { return Err(invalid("a pass cannot be its own prior review".into())); }
            let (p_submission, p_task, p_candidate, p_scope, ..) = opportunity(&tx, prior)?;
            if p_task != task { return Err(invalid(format!("prior opportunity {prior} reviews another task"))); }
            let artifact = if p_submission != submission || p_candidate != candidate { "changed_artifact" } else if p_scope != scope { "different_scope" } else { "same_artifact" };
            let (status, _) = review_ledger::opportunity_status(&tx, prior, &lifecycle)?;
            records.push(serde_json::json!({"opportunity_id": prior, "submission_id": p_submission, "candidate_oid": p_candidate, "scope": p_scope, "artifact": artifact, "status": status}));
        }
        let label = |a: &str| records.iter().any(|r| r["artifact"] == a);
        let comparability = if label("changed_artifact") { "changed_artifact" } else if label("different_scope") { "different_scope" } else { "same_artifact" };
        let coverage = if records.iter().all(|r| r["status"] == "completed") { "complete" } else { "incomplete" };
        let cutoff = finding_triage::head(&tx)?;
        let seq = log(&tx, "pass_bound", principal, expected_seq, now)?;
        let priors_json = serde_json::Value::Array(records);
        tx.execute("INSERT INTO skeptical_passes(seq,opportunity_id,protocol_seq,cutoff_seq,priors,comparability,prior_coverage) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![seq, opportunity_id, protocol_seq, cutoff, priors_json.to_string(), comparability, coverage])?;
        done(tx, seq, serde_json::json!({"opportunity_id": opportunity_id, "protocol": protocol, "cutoff_seq": cutoff, "priors": priors_json,
            "comparability": comparability, "prior_coverage": coverage}))
    }

    /// Preregister an experiment (`review_experiment.v1` JSON). It is frozen:
    /// design, exact eligibility, arms (the first is the reference), the
    /// assignment rule, outcome, horizon and minimum units never change.
    pub fn register_review_experiment(&mut self, definition: &[u8], expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        if definition.len() > MAX_DEFINITION_BYTES { return Err(invalid(format!("experiment definition exceeds {MAX_DEFINITION_BYTES} bytes"))); }
        let d: ExperimentDefinition = serde_json::from_slice(definition).map_err(|e| invalid(format!("invalid review experiment: {e}")))?;
        if d.schema != EXPERIMENT_SCHEMA { return Err(invalid(format!("review experiment schema must be {EXPERIMENT_SCHEMA}"))); }
        identifier(&d.experiment, "experiment")?;
        one_of(&d.design, &["randomized", "matched"], "design")?;
        match (d.design.as_str(), &d.seed) {
            ("randomized", Some(seed)) if finding_triage::hex64(seed) => {}
            ("randomized", _) => return Err(invalid("a randomized experiment records its seed (64 lowercase hex)".into())),
            (_, Some(_)) => return Err(invalid("a matched experiment has no seed: the owner assigns each unit's arm within a block".into())),
            _ => {}
        }
        let match_on = tokens(&d.match_on, "match_on", usize::from(d.design == "matched"))?;
        let e = &d.eligibility;
        one_of(&e.kind, &super::review_capture::KINDS, "review kind")?;
        one_of(&e.scope, &super::review_capture::SCOPES, "review scope")?;
        if e.role == "gate" { return Err(invalid("a required gate review is never withheld to balance an experiment: eligibility role is evaluation or advisory".into())); }
        one_of(&e.role, &["evaluation", "advisory"], "review role")?;
        identifier(&e.protocol, "eligibility protocol")?;
        if d.arms.len() < 2 || d.arms.len() > 4 { return Err(invalid("an experiment has 2 to 4 arms".into())); }
        let (mut names, mut protocols) = (BTreeSet::new(), BTreeSet::new());
        for arm in &d.arms {
            identifier(&arm.arm, "arm")?;
            if !names.insert(&arm.arm) { return Err(invalid(format!("duplicate arm {}", arm.arm))); }
            if !protocols.insert(&arm.protocol) { return Err(invalid("two arms with the same protocol compare nothing".into())); }
        }
        one_of(&d.primary_outcome, &[EXPERIMENT_OUTCOME], "primary outcome")?;
        one_of(&d.adjudication, &[ADJUDICATION], "adjudication")?;
        one_of(&d.stopping_rule, &EXPERIMENT_STOPPING_RULES, "stopping rule")?;
        if !(1..=3650).contains(&d.horizon_days) { return Err(invalid("horizon_days is 1 to 3650".into())); }
        let min_units = d.min_units.unwrap_or(DEFAULT_MIN_UNITS);
        if min_units < 2 { return Err(invalid("min_units is at least 2".into())); }
        if d.planned_units == Some(0) { return Err(invalid("planned_units must be positive".into())); }
        if d.stopping_rule == "planned_units" && d.planned_units.is_none() { return Err(invalid("the planned_units stopping rule needs planned_units".into())); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        for protocol in d.arms.iter().filter_map(|a| a.protocol.as_ref()) {
            if !tx.query_row("SELECT EXISTS(SELECT 1 FROM review_protocols WHERE protocol=?1)", [protocol], |r| r.get::<_, bool>(0))? {
                return Err(invalid(format!("arm protocol {protocol} is not registered")));
            }
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_experiments WHERE experiment=?1)", [&d.experiment], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("experiment {} is already registered: a preregistration is frozen", d.experiment)));
        }
        let arms = serde_json::Value::Array(d.arms.iter().map(|a| serde_json::json!({"arm": a.arm, "protocol": a.protocol})).collect());
        let horizon_ms = i64::from(d.horizon_days) * DAY_MS;
        let canonical = serde_json::json!({"adjudication": d.adjudication, "arms": arms, "design": d.design, "eligibility": {"kind": e.kind, "protocol": e.protocol,
            "role": e.role, "scope": e.scope}, "experiment": d.experiment, "horizon_days": d.horizon_days, "match_on": match_on, "min_units": min_units,
            "planned_units": d.planned_units, "primary_outcome": d.primary_outcome, "schema": EXPERIMENT_SCHEMA, "seed": d.seed, "stopping_rule": d.stopping_rule}).to_string();
        let definition_digest = digest(&canonical);
        let seq = log(&tx, "experiment_registered", principal, expected_seq, now)?;
        tx.execute("INSERT INTO review_experiments(seq,experiment,design,seed,eligible_kind,eligible_scope,eligible_role,eligible_protocol,arms,min_units,horizon_ms,definition_digest,canonical_json)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![seq, d.experiment, d.design, d.seed, e.kind, e.scope, e.role, e.protocol, arms.to_string(), min_units, horizon_ms, definition_digest, canonical])?;
        done(tx, seq, serde_json::json!({"experiment": d.experiment, "design": d.design, "definition_digest": definition_digest}))
    }

    /// Assign an exactly eligible opportunity to an arm before any completion
    /// of it exists. Randomized: the arm follows from the recorded seed and the
    /// submission; matched: the owner names the block and the arm.
    #[allow(clippy::too_many_arguments)]
    pub fn assign_experiment_unit(&mut self, experiment: &str, opportunity_id: &str, block: Option<&str>, arm: Option<&str>, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        type ExperimentRow = (i64, String, Option<String>, String, String, String, String, String);
        let row: Option<ExperimentRow> = tx.query_row(
            "SELECT seq,design,seed,eligible_kind,eligible_scope,eligible_role,eligible_protocol,arms FROM review_experiments WHERE experiment=?1", [experiment],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).optional()?;
        let Some((experiment_seq, design, seed, e_kind, e_scope, e_role, e_protocol, arms)) = row else { return Err(invalid(format!("no experiment {experiment}"))) };
        // A `planned_units` experiment stops assigning at its planned number of units (the 0059 trigger repeats it).
        let (rule, planned, assigned): (Option<String>, Option<i64>, i64) = tx.query_row("SELECT json_extract(e.canonical_json,'$.stopping_rule'),json_extract(e.canonical_json,'$.planned_units'),
                (SELECT count(*) FROM experiment_units u WHERE u.experiment_seq=e.seq) FROM review_experiments e WHERE e.seq=?1", [experiment_seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if let (Some("planned_units"), Some(planned)) = (rule.as_deref(), planned) && assigned >= planned {
            return Err(invalid(format!("experiment {experiment} reached its {planned} planned units: its stopping rule ends assignment")));
        }
        let arms: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&arms).map_err(|_| StoreError::Corrupt("invalid experiment arms".into()))?
            .iter().filter_map(|a| a["arm"].as_str().map(str::to_owned)).collect();
        let (submission, _, _, scope, kind, role, protocol, ..) = opportunity(&tx, opportunity_id)?;
        for (what, have, want) in [("kind", &kind, &e_kind), ("scope", &scope, &e_scope), ("role", &role, &e_role), ("protocol", &protocol, &e_protocol)] {
            if have != want { return Err(invalid(format!("opportunity {opportunity_id} is not eligible: {what} {have}, the preregistration requires {want}"))); }
        }
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM review_sessions s JOIN review_completions c ON c.session_id=s.session_id WHERE s.opportunity_id=?1)", [opportunity_id], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("opportunity {opportunity_id} already has an outcome: assignment is frozen before outcomes")));
        }
        if let Some(existing) = tx.query_row("SELECT opportunity_id FROM experiment_units WHERE experiment_seq=?1 AND (opportunity_id=?2 OR submission_id=?3)",
            params![experiment_seq, opportunity_id, submission], |r| r.get::<_, String>(0)).optional()? {
            return Err(invalid(format!("the artifact of {opportunity_id} is already a unit of {experiment} (opportunity {existing})")));
        }
        let (arm, block) = if design == "randomized" {
            if block.is_some() || arm.is_some() { return Err(invalid("a randomized experiment assigns the arm from its recorded seed: no --block or --arm".into())); }
            let seed = seed.ok_or_else(|| StoreError::Corrupt("randomized experiment without a seed".into()))?;
            (arms[randomized_arm(&seed, &submission, arms.len())].clone(), None)
        } else {
            let (Some(block), Some(arm)) = (block, arm) else { return Err(invalid("a matched experiment needs --block and --arm".into())) };
            identifier(block, "block")?;
            if !arms.iter().any(|a| a == arm) { return Err(invalid(format!("experiment {experiment} has no arm {arm}"))); }
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM experiment_units WHERE experiment_seq=?1 AND block=?2 AND arm=?3)", params![experiment_seq, block, arm], |r| r.get::<_, bool>(0))? {
                return Err(invalid(format!("block {block} already has a unit in arm {arm}")));
            }
            (arm.to_owned(), Some(block.to_owned()))
        };
        let seq = log(&tx, "unit_assigned", principal, expected_seq, now)?;
        tx.execute("INSERT INTO experiment_units(seq,experiment_seq,opportunity_id,submission_id,arm,block) VALUES(?1,?2,?3,?4,?5,?6)",
            params![seq, experiment_seq, opportunity_id, submission, arm, block])?;
        done(tx, seq, serde_json::json!({"experiment": experiment, "opportunity_id": opportunity_id, "submission_id": submission, "arm": arm, "block": block}))
    }

    /// Exclude an assigned unit, with a reason. It stays listed under its arm.
    pub fn exclude_experiment_unit(&mut self, experiment: &str, opportunity_id: &str, reason: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        one_of(reason, &EXCLUSION_REASONS, "exclusion reason")?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let unit: Option<(i64, String)> = tx.query_row("SELECT u.seq,u.arm FROM experiment_units u JOIN review_experiments e ON e.seq=u.experiment_seq WHERE e.experiment=?1 AND u.opportunity_id=?2",
            params![experiment, opportunity_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((unit_seq, arm)) = unit else { return Err(invalid(format!("opportunity {opportunity_id} is not a unit of {experiment}"))) };
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM experiment_exclusions WHERE unit_seq=?1)", [unit_seq], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("unit {opportunity_id} is already excluded (a retracted exclusion is not recorded again)")));
        }
        let seq = log(&tx, "unit_excluded", principal, expected_seq, now)?;
        tx.execute("INSERT INTO experiment_exclusions(seq,unit_seq,reason) VALUES(?1,?2,?3)", params![seq, unit_seq, reason])?;
        done(tx, seq, serde_json::json!({"experiment": experiment, "opportunity_id": opportunity_id, "unit_seq": unit_seq, "arm": arm, "reason": reason}))
    }

    /// Reverse one pass binding (`kind` `pass_bound`) or one unit exclusion
    /// (`unit_excluded`) recorded in error at history sequence `target`. The
    /// retraction is a `review_log` row (store schema 59); nothing is deleted.
    pub fn retract_protocol_record(&mut self, target: i64, kind: &str, expected_seq: Option<i64>, principal: &str, now: i64) -> Result<FindingEvent> {
        let retraction = match kind { "pass_bound" => "pass_retracted", "unit_excluded" => "exclusion_retracted", _ => return Err(invalid(format!("only a pass binding or a unit exclusion is retracted, not {kind}"))) };
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema_57(&tx)?;
        finding_triage::triage_authority(&tx, principal)?;
        let recorded: Option<String> = tx.query_row("SELECT kind FROM protocol_log WHERE seq=?1", [target], |r| r.get(0)).optional()?;
        if recorded.as_deref() != Some(kind) {
            let what = if kind == "pass_bound" { "pass binding" } else { "unit exclusion" };
            return Err(invalid(format!("no {what} at seq {target}")));
        }
        if review_ledger::present(&tx)? && tx.query_row("SELECT EXISTS(SELECT 1 FROM protocol_retractions WHERE reverses=?1)", [target], |r| r.get::<_, bool>(0))? {
            return Err(invalid(format!("the record at seq {target} is already retracted")));
        }
        let event = review_ledger::record_retraction(&tx, retraction, target, principal, expected_seq, now)?;
        tx.commit()?;
        Ok(event)
    }
}

/// An exact fraction `num/den` (`den > 0`), reduced; an integer when `den` divides it.
pub fn fraction(num: i64, den: i64) -> String {
    fn gcd(a: i64, b: i64) -> i64 { if b == 0 { a.abs() } else { gcd(b, a % b) } }
    let g = gcd(num, den).max(1);
    let (n, d) = (num / g, den / g);
    if d == 1 { n.to_string() } else { format!("{n}/{d}") }
}

fn unavailable(reason: &str) -> serde_json::Value { serde_json::json!({"status": "unavailable", "reason": reason}) }

/// Roots at the watermark (`triage`) of the findings known at `cutoff`:
/// findings validated then (roots or merged into one), and the opportunity's declared prior findings.
fn known_at(db: &Connection, cutoff: i64, declared: &[String], triage: &FindingState, cache: &mut BTreeMap<i64, BTreeSet<String>>) -> Result<BTreeSet<String>> {
    if let std::collections::btree_map::Entry::Vacant(slot) = cache.entry(cutoff) {
        slot.insert(match finding_triage::finding_state(db, Some(cutoff))? {
            Some(then) => then.findings.iter().filter(|f| f.status != "unvalidated").map(|f| f.finding_id.clone()).collect(),
            None => BTreeSet::new(),
        });
    }
    let roots: BTreeMap<&str, &str> = triage.findings.iter().map(|f| (f.finding_id.as_str(), f.root.as_str())).collect();
    Ok(cache[&cutoff].iter().map(String::as_str).chain(declared.iter().map(String::as_str)).filter_map(|f| roots.get(f).map(|r| (*r).to_owned())).collect())
}

/// Replay the protocol, pass and experiment history (with findings) up to
/// `as_of` (default: the head) on any connection, read-only; `now` decides
/// experiment horizons. `None` before migration 0057. Review statuses replay
/// to the same watermark (sessions and completions are ledger rows since
/// 0059); a retracted pass or exclusion stops counting at its retraction.
pub fn protocol_state(db: &Connection, as_of: Option<i64>, now: i64) -> Result<Option<ProtocolState>> {
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='protocol_log')", [], |r| r.get(0))?;
    if !present { return Ok(None); }
    let Some(triage) = finding_triage::finding_state(db, as_of)? else { return Ok(None) };
    let at = triage.as_of_seq;
    let mut cache = BTreeMap::new();
    // Review status replays with the ledger (0059); retracted bindings and exclusions by the watermark.
    let lifecycle = review_ledger::Lifecycle::at(db, Some(at))?;
    let retracted = review_ledger::protocol_retractions(db, at)?;

    let protocols: Vec<ReviewProtocol> = db.prepare("SELECT p.seq,p.protocol,p.definition_digest,p.canonical_json,l.recorded_unix_ms FROM review_protocols p JOIN protocol_log l ON l.seq=p.seq WHERE p.seq<=?1 ORDER BY p.seq")?
        .query_map([at], |r| Ok(ReviewProtocol { seq: r.get(0)?, protocol: r.get(1)?, definition_digest: r.get(2)?,
            definition: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(serde_json::Value::Null), registered_unix_ms: r.get(4)? }))?.collect::<rusqlite::Result<_>>()?;

    // Passes and their incremental yield at the watermark.
    type PassRow = (i64, String, i64, String, String, String, String, i64, String, String, i64, Option<String>, String, String);
    let rows: Vec<PassRow> = db.prepare("SELECT s.seq,s.opportunity_id,s.cutoff_seq,s.priors,s.comparability,s.prior_coverage,p.protocol,p.evidence_min,p.min_severity,p.prior_disclosure,
            l.recorded_unix_ms,p.reviewer_configuration_id,o.submission_id,o.candidate_oid
        FROM skeptical_passes s JOIN review_protocols p ON p.seq=s.protocol_seq JOIN protocol_log l ON l.seq=s.seq JOIN review_opportunities o ON o.opportunity_id=s.opportunity_id
        WHERE s.seq<=?1 ORDER BY s.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut passes = Vec::with_capacity(rows.len());
    for (seq, opportunity_id, cutoff_seq, priors, comparability, prior_coverage, protocol, evidence_min, min_severity, prior_disclosure, bound, reviewer, submission_id, candidate_oid) in rows {
        let declared: Vec<String> = db.query_row("SELECT prior_findings FROM review_opportunities WHERE opportunity_id=?1", [&opportunity_id], |r| r.get::<_, String>(0))
            .map(|t| serde_json::from_str(&t).unwrap_or_default())?;
        let known = known_at(db, cutoff_seq, &declared, &triage, &mut cache)?;
        let (status, completed) = review_ledger::opportunity_status(db, &opportunity_id, &lifecycle)?;
        let retracted_seq = retracted.get(&seq).copied();
        let mut claims = Vec::new();
        for sub in triage.submissions.iter().filter(|s| completed.as_ref() == Some(&s.session_id)) {
            for c in &sub.claims {
                let incremental = match c.outcome.as_str() {
                    "validated" if c.canonical_finding.as_ref().is_some_and(|r| known.contains(r)) => "known",
                    "validated" if c.severity.as_deref().is_some_and(|s| severity_rank(s) > severity_rank(&min_severity)) => "below_severity_floor",
                    "validated" => "new",
                    "duplicate" => "rediscovered",
                    other => other,
                };
                claims.push(PassClaim { submission_id: sub.submission_id, claim_id: c.claim_id, outcome: c.outcome.clone(), canonical_finding: c.canonical_finding.clone(),
                    severity: c.severity.clone(), incremental: incremental.to_owned() });
            }
        }
        let new_unique_findings: Vec<String> = claims.iter().filter(|c| c.incremental == "new").filter_map(|c| c.canonical_finding.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let rediscovered = claims.iter().filter(|c| c.incremental == "rediscovered").count();
        let assigned: Option<String> = db.query_row("SELECT reviewer_configuration_id FROM review_assignments WHERE opportunity_id=?1", [&opportunity_id], |r| r.get(0)).optional()?;
        let evidence: i64 = match &completed {
            Some(session) => db.query_row("SELECT json_array_length(evidence_refs) FROM review_completions WHERE session_id=?1", [session], |r| r.get(0))?,
            None => 0,
        };
        let exclusion = if retracted_seq.is_some() { Some("retracted".to_owned()) }
            else if comparability != "same_artifact" { Some(comparability.clone()) }
            else if prior_coverage != "complete" { Some("incomplete_prior_coverage".to_owned()) }
            else if status != "completed" { Some("not_completed".to_owned()) }
            else if reviewer.is_some() && assigned != reviewer { Some("reviewer_mismatch".to_owned()) }
            else if evidence < evidence_min { Some("evidence_requirement_unmet".to_owned()) }
            else if claims.iter().any(|c| c.incremental == "pending") { Some("pending_triage".to_owned()) }
            else { None };
        passes.push(PassState { seq, opportunity_id, protocol, submission_id, candidate_oid, bound_unix_ms: bound, cutoff_seq,
            priors: serde_json::from_str(&priors).unwrap_or(serde_json::Value::Null), comparability, prior_coverage, prior_disclosure, status: status.to_owned(),
            known_findings: known.into_iter().collect(), claims, new_unique_findings, rediscovered, eligible: exclusion.is_none(), exclusion, retracted_seq });
    }

    // Experiments, their units and the preregistered estimate.
    type ExperimentRow = (i64, String, String, Option<String>, String, i64, i64, String, String, i64);
    let rows: Vec<ExperimentRow> = db.prepare("SELECT e.seq,e.experiment,e.design,e.seed,e.arms,e.min_units,e.horizon_ms,e.definition_digest,e.canonical_json,l.recorded_unix_ms
        FROM review_experiments e JOIN protocol_log l ON l.seq=e.seq WHERE e.seq<=?1 ORDER BY e.seq")?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut experiments = Vec::with_capacity(rows.len());
    for (seq, experiment, design, seed, arms, min_units, horizon_ms, definition_digest, canonical, registered) in rows {
        let arms: Vec<(String, Option<String>)> = serde_json::from_str::<Vec<serde_json::Value>>(&arms).unwrap_or_default().iter()
            .map(|a| (a["arm"].as_str().unwrap_or_default().to_owned(), a["protocol"].as_str().map(str::to_owned))).collect();
        type UnitRow = (i64, String, String, String, Option<String>, i64, Option<i64>, Option<String>);
        let unit_rows: Vec<UnitRow> = db.prepare("SELECT u.seq,u.opportunity_id,u.submission_id,u.arm,u.block,l.recorded_unix_ms,x.seq,x.reason FROM experiment_units u JOIN protocol_log l ON l.seq=u.seq
                LEFT JOIN experiment_exclusions x ON x.unit_seq=u.seq AND x.seq<=?2 WHERE u.experiment_seq=?1 AND u.seq<=?2 ORDER BY u.seq")?
            .query_map(params![seq, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut units = Vec::with_capacity(unit_rows.len());
        for (unit_seq, opportunity_id, submission_id, arm, block, assigned, excluded_seq, reason) in unit_rows {
            let arm_protocol = arms.iter().find(|a| a.0 == arm).and_then(|a| a.1.clone());
            let own: Vec<&PassState> = passes.iter().filter(|p| p.retracted_seq.is_none() && p.priors.as_array().is_some_and(|ps| ps.iter().any(|x| x["opportunity_id"] == opportunity_id.as_str()))).collect();
            let treatment_received = arm_protocol.as_ref().map(|p| own.iter().any(|x| &x.protocol == p));
            let crossover = own.iter().any(|x| Some(&x.protocol) != arm_protocol.as_ref());
            let members: BTreeSet<&str> = std::iter::once(opportunity_id.as_str()).chain(own.iter().map(|p| p.opportunity_id.as_str())).collect();
            let cutoff = unit_seq - 1;
            let declared: Vec<String> = Vec::new();
            let known = known_at(db, cutoff, &declared, &triage, &mut cache)?;
            let subs: Vec<_> = triage.submissions.iter().filter(|s| members.contains(s.opportunity_id.as_str())).collect();
            let found: BTreeSet<String> = subs.iter().flat_map(|s| &s.claims).filter(|c| c.outcome == "validated")
                .filter_map(|c| c.canonical_finding.clone()).filter(|r| !known.contains(r)).collect();
            let mut settled = !subs.iter().any(|s| s.claims.iter().any(|c| c.outcome == "pending"));
            for member in &members {
                if !matches!(review_ledger::opportunity_status(db, member, &lifecycle)?.0, "completed" | "ended_without_completion") { settled = false; }
            }
            let horizon_passed = now >= assigned + horizon_ms;
            // A treatment not yet received may still come until the horizon.
            if treatment_received == Some(false) && !horizon_passed { settled = false; }
            let exclusion_retracted = excluded_seq.and_then(|x| retracted.get(&x).copied());
            let status = if excluded_seq.is_some() && exclusion_retracted.is_none() { "excluded" } else if settled { "analyzable" } else if horizon_passed { "censored" } else { "pending" };
            units.push(UnitState { seq: unit_seq, opportunity_id, submission_id, arm, block, assigned_unix_ms: assigned, cutoff_seq: cutoff,
                passes: own.iter().map(|p| p.opportunity_id.clone()).collect(), treatment_received, crossover,
                exclusion: excluded_seq.map(|s| serde_json::json!({"seq": s, "reason": reason, "retracted_seq": exclusion_retracted})), status: status.to_owned(),
                outcome: (status == "analyzable").then_some(found.len()), new_unique_findings: if status == "analyzable" { found.into_iter().collect() } else { Vec::new() } });
        }
        let estimate = estimate(&design, &arms, &units, min_units);
        experiments.push(ExperimentState { seq, experiment, design, seed, definition_digest, definition: serde_json::from_str(&canonical).unwrap_or(serde_json::Value::Null),
            reference_arm: arms.first().map(|a| a.0.clone()).unwrap_or_default(), min_units, horizon_ms, registered_unix_ms: registered, units, estimate });
    }

    let history: Vec<FindingEvent> = db.prepare("SELECT seq,kind,principal,authority,expected_seq,recorded_unix_ms FROM protocol_log WHERE seq<=?1 ORDER BY seq")?
        .query_map([at], |r| Ok(FindingEvent { seq: r.get(0)?, kind: r.get(1)?, principal: r.get(2)?, authority: r.get(3)?, expected_seq: r.get(4)?,
            recorded_unix_ms: r.get(5)?, subject: serde_json::Value::Null }))?.collect::<rusqlite::Result<_>>()?;
    let history = history.into_iter().map(|mut e| {
        e.subject = match e.kind.as_str() {
            "protocol_registered" => protocols.iter().find(|p| p.seq == e.seq).map_or(serde_json::Value::Null, |p| serde_json::json!({"protocol": p.protocol})),
            "pass_bound" => passes.iter().find(|p| p.seq == e.seq).map_or(serde_json::Value::Null, |p| serde_json::json!({"opportunity_id": p.opportunity_id, "protocol": p.protocol})),
            "experiment_registered" => experiments.iter().find(|x| x.seq == e.seq).map_or(serde_json::Value::Null, |x| serde_json::json!({"experiment": x.experiment})),
            _ => experiments.iter().find_map(|x| x.units.iter().find(|u| u.seq == e.seq || u.exclusion.as_ref().is_some_and(|v| v["seq"] == e.seq))
                .map(|u| serde_json::json!({"experiment": x.experiment, "opportunity_id": u.opportunity_id, "arm": u.arm}))).unwrap_or(serde_json::Value::Null),
        };
        e
    }).collect();
    Ok(Some(ProtocolState { head_seq: triage.head_seq, as_of_seq: at, protocols, passes, experiments, history }))
}

/// The preregistered comparison, by intention to treat (units analyzed in
/// their assigned arm; crossover counted, never moved). Each arm is compared
/// with the reference (first) arm only with at least `min_units` analyzable
/// units per arm (randomized) or complete blocks (matched); below that the
/// difference is `unavailable: insufficient_data` with its counts.
fn estimate(design: &str, arms: &[(String, Option<String>)], units: &[UnitState], min_units: i64) -> serde_json::Value {
    let mut per_arm = serde_json::Map::new();
    let mut stats: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for (arm, protocol) in arms {
        let mine: Vec<&UnitState> = units.iter().filter(|u| &u.arm == arm).collect();
        let count = |status: &str| mine.iter().filter(|u| u.status == status).count();
        let analyzable: Vec<&&UnitState> = mine.iter().filter(|u| u.status == "analyzable").collect();
        let (n, y) = (analyzable.len() as i64, analyzable.iter().map(|u| u.outcome.unwrap_or(0) as i64).sum::<i64>());
        stats.insert(arm, (n, y));
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for u in mine.iter().filter(|u| u.status == "excluded") { if let Some(x) = &u.exclusion { *reasons.entry(x["reason"].as_str().unwrap_or_default().to_owned()).or_default() += 1; } }
        per_arm.insert(arm.clone(), serde_json::json!({"protocol": protocol, "assigned": mine.len(), "analyzable": n, "pending": count("pending"), "censored": count("censored"),
            "excluded": reasons, "crossover": mine.iter().filter(|u| u.crossover).count(),
            "treatment_not_received": mine.iter().filter(|u| u.treatment_received == Some(false)).count(),
            "outcome_total": y, "mean": if n == 0 { serde_json::Value::Null } else { serde_json::json!(fraction(y, n)) }}));
    }
    let reference = arms.first().map(|a| a.0.as_str()).unwrap_or_default();
    let mut differences = serde_json::Map::new();
    let mut complete_blocks = serde_json::Value::Null;
    if design == "randomized" {
        let (n0, y0) = stats.get(reference).copied().unwrap_or_default();
        for (arm, _) in arms.iter().skip(1) {
            let (n1, y1) = stats.get(arm.as_str()).copied().unwrap_or_default();
            let value = if n0 >= min_units && n1 >= min_units { serde_json::json!(fraction(y1 * n0 - y0 * n1, n1 * n0)) } else { unavailable("insufficient_data") };
            differences.insert(arm.clone(), serde_json::json!({"value": value, "analyzable": [n0, n1]}));
        }
    } else {
        // Blocks with exactly one analyzable unit in every arm.
        let mut blocks: BTreeMap<&str, BTreeMap<&str, i64>> = BTreeMap::new();
        for u in units.iter().filter(|u| u.status == "analyzable") {
            if let Some(b) = &u.block { blocks.entry(b.as_str()).or_default().insert(u.arm.as_str(), u.outcome.unwrap_or(0) as i64); }
        }
        let complete: Vec<&BTreeMap<&str, i64>> = blocks.values().filter(|b| arms.iter().all(|a| b.contains_key(a.0.as_str()))).collect();
        let n = complete.len() as i64;
        complete_blocks = serde_json::json!(n);
        for (arm, _) in arms.iter().skip(1) {
            let sum: i64 = complete.iter().map(|b| b[arm.as_str()] - b[reference]).sum();
            let value = if n >= min_units { serde_json::json!(fraction(sum, n)) } else { unavailable("insufficient_data") };
            differences.insert(arm.clone(), serde_json::json!({"value": value, "complete_blocks": n}));
        }
    }
    serde_json::json!({"estimate": design, "analysis": "intention_to_treat", "outcome": EXPERIMENT_OUTCOME, "reference_arm": reference, "min_units": min_units,
        "arms": per_arm, "differences": differences, "complete_blocks": complete_blocks, "uncertainty": unavailable("interval_not_computed")})
}
