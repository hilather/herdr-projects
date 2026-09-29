//! Lane D (docs/telemetry/phase2-lanes.md): review capture, contracts in
//! docs/telemetry/contracts-review.md. Sidecar stream `review` (no tables yet).
//! Hooks registered centrally in `super::LANES`; this lane adds subcommands,
//! metrics, tick work and `migrations/telemetry/review/NNNN_*.sql` here only.
//! Writes go through the canonical store (`SqliteStore`, one `state.db`
//! transaction each); `show`, `present`, `report` and the metrics hook read
//! `state.db` strictly read-only.
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::store::{ReviewAssignmentChoice, ReviewOpportunitySpec, SqliteStore};

pub const STREAM: &str = "review";
/// `include_str!` of `migrations/telemetry/review/`, in order; index + 1 is the stream version.
pub const MIGRATIONS: &[&str] = &[];

/// Principal recorded for rows written on this CLI.
const OPERATOR: &str = "operator:cli";
/// Why acceptance and every accepted-quality metric are inactive.
const INACTIVE: &str = "no_reviewer_authority_producer";
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;

/// `herdr-projects telemetry <slug> review ...`
#[derive(clap::Subcommand)]
pub enum Command {
    /// Stream version of this lane's sidecar tables. Read-only.
    Status,
    /// Open a review opportunity on one submission's exact candidate.
    Open {
        submission: String,
        /// `candidate_diff`, `candidate_tree` or `contract_scope`.
        #[arg(long, default_value = "candidate_diff")]
        scope: String,
        /// Review method: `code`, `skeptical`, `security`, `test`, `architecture`.
        #[arg(long, default_value = "code")]
        kind: String,
        /// `gate`, `evaluation` or `advisory`.
        #[arg(long, default_value = "evaluation")]
        role: String,
        /// Versioned protocol identifier, e.g. `review-protocol.v1`.
        #[arg(long)]
        protocol: String,
        /// Finding already known before this review (`finding:<ref>`), repeatable.
        #[arg(long = "prior-finding")]
        prior_findings: Vec<String>,
        /// Time budget in milliseconds.
        #[arg(long)]
        budget_ms: Option<u64>,
    },
    /// Assign the opportunity's reviewer, once: `--reviewer` (operator) or
    /// `--blind` over `--candidate` profiles (blind_cross_provider.v1).
    Assign {
        opportunity: String,
        /// Retained native profile chosen by the operator.
        #[arg(long, conflicts_with_all = ["blind", "candidates"])]
        reviewer: Option<String>,
        /// Deterministic blind cross-provider policy over `--candidate`.
        #[arg(long, requires = "candidates")]
        blind: bool,
        /// Retained native profile weighed by the blind policy, repeatable.
        #[arg(long = "candidate")]
        candidates: Vec<String>,
    },
    /// Record that an attempt started a session of an assigned opportunity.
    Start {
        opportunity: String,
        #[arg(long)]
        attempt: String,
    },
    /// Record a session's end from the reviewer's receipt (`review_receipt.v1`)
    /// as a proposal with declared coverage.
    Complete {
        #[arg(long)]
        input_file: PathBuf,
    },
    /// Privileged acceptance of a session's completion. Inactive: refuses
    /// until a reviewer-authority producer exists.
    Accept { session: String },
    /// What a blind reviewer may see of an opportunity: the exact candidate,
    /// scope and protocol, never the author attempt or configuration. Read-only.
    Present { opportunity: String },
    /// Opportunities with assignment, sessions and completions, as JSON. Read-only.
    Show {
        /// Window start (Unix ms), by the opportunity's creation.
        #[arg(long)]
        since: Option<i64>,
    },
    /// Lane metrics (M20; M21-M24 inactive) as JSON. Read-only.
    Report {
        /// Window start (Unix ms), by the assignment (unassigned: by creation).
        #[arg(long)]
        since: Option<i64>,
    },
}

/// The command's stdout.
pub fn run(project: &Path, command: Command) -> Result<String> {
    let now = jiff::Timestamp::now().as_millisecond();
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    let value = match command {
        Command::Status => super::sidecar::status(project, STREAM)?,
        Command::Open { submission, scope, kind, role, protocol, prior_findings, budget_ms } => {
            let spec = ReviewOpportunitySpec { submission_id: submission, scope, kind, role, protocol, prior_findings, budget_ms };
            json!({"opportunity": open()?.open_review_opportunity(&spec, OPERATOR, now)?})
        }
        Command::Assign { opportunity, reviewer, blind, candidates } => {
            let choice = match reviewer {
                Some(profile) => ReviewAssignmentChoice::Operator { profile },
                None if blind => ReviewAssignmentChoice::BlindCrossProvider { candidates },
                None => anyhow::bail!("assign needs --reviewer or --blind with --candidate"),
            };
            json!({"assignment": open()?.assign_review(&opportunity, &choice, OPERATOR, now)?})
        }
        Command::Start { opportunity, attempt } => json!({"session": open()?.start_review_session(&opportunity, &attempt, OPERATOR, now)?}),
        Command::Complete { input_file } => {
            let size = std::fs::metadata(&input_file).with_context(|| format!("read {}", input_file.display()))?.len();
            anyhow::ensure!(size <= MAX_RECEIPT_BYTES, "review receipt exceeds {MAX_RECEIPT_BYTES} bytes");
            let bytes = std::fs::read(&input_file).with_context(|| format!("read {}", input_file.display()))?;
            json!({"completion": open()?.complete_review_session(&bytes, OPERATOR, now)?})
        }
        Command::Accept { session } => { open()?.accept_review(&session, OPERATOR)?; json!({}) }
        Command::Present { opportunity } => present(project, &opportunity)?,
        Command::Show { since } => show(project, since)?,
        Command::Report { since } => json!({"metrics": metrics(project, since)?, "since_unix_ms": since}),
    };
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

/// `state.db` read-only, or `None` before migration 0054.
fn read(project: &Path) -> Result<Option<super::ReadOnly>> {
    let db = super::read_only(&project.join(".state/state.db"))?;
    crate::store::check_schema(&db)?;
    let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_opportunities')", [], |r| r.get(0))?;
    Ok(present.then_some(db))
}

fn present(project: &Path, opportunity: &str) -> Result<Value> {
    let db = read(project)?.context("review capture needs store schema 54")?;
    let row = db.query_row("SELECT o.opportunity_id,o.task_id,o.contract_revision,s.repository,s.base_oid,o.candidate_oid,s.object_format,o.scope,o.kind,o.protocol,o.prior_findings,o.budget_ms
        FROM review_opportunities o JOIN result_submissions s ON s.submission_id=o.submission_id WHERE o.opportunity_id=?1", [opportunity],
        |r| Ok(json!({"opportunity_id": r.get::<_, String>(0)?, "task_id": r.get::<_, String>(1)?, "contract_revision": r.get::<_, i64>(2)?,
            "repository": r.get::<_, String>(3)?, "base_oid": r.get::<_, String>(4)?, "candidate_oid": r.get::<_, String>(5)?, "object_format": r.get::<_, String>(6)?,
            "scope": r.get::<_, String>(7)?, "kind": r.get::<_, String>(8)?, "protocol": r.get::<_, String>(9)?,
            "prior_findings": serde_json::from_str::<Value>(&r.get::<_, String>(10)?).unwrap_or(Value::Null), "budget_ms": r.get::<_, Option<i64>>(11)?}))).optional()?;
    row.map(|r| json!({"presentation": r})).with_context(|| format!("no review opportunity {opportunity}"))
}

/// One opportunity as `show` reports it, with its status for M20.
struct Opportunity { record: Value, kind: String, protocol: String, assigned: Option<i64>, created: i64, status: &'static str, findings: Option<i64> }

fn opportunities(db: &rusqlite::Connection) -> Result<Vec<Opportunity>> {
    let db = db.unchecked_transaction()?;
    let rows: Vec<(Value, String, String, i64)> = db.prepare("SELECT opportunity_id,submission_id,task_id,contract_revision,candidate_oid,scope,kind,role,protocol,prior_findings,budget_ms,creator_principal,created_unix_ms
        FROM review_opportunities ORDER BY created_unix_ms,rowid")?
        .query_map([], |r| Ok((json!({"opportunity_id": r.get::<_, String>(0)?, "submission_id": r.get::<_, String>(1)?, "task_id": r.get::<_, String>(2)?,
            "contract_revision": r.get::<_, i64>(3)?, "candidate_oid": r.get::<_, String>(4)?, "scope": r.get::<_, String>(5)?, "kind": r.get::<_, String>(6)?,
            "role": r.get::<_, String>(7)?, "protocol": r.get::<_, String>(8)?, "prior_findings": serde_json::from_str::<Value>(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
            "budget_ms": r.get::<_, Option<i64>>(10)?, "creator_principal": r.get::<_, String>(11)?, "created_unix_ms": r.get::<_, i64>(12)?}),
            r.get(6)?, r.get(8)?, r.get(12)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (mut record, kind, protocol, created) in rows {
        let id = record["opportunity_id"].as_str().unwrap_or_default().to_owned();
        let assignment = db.query_row("SELECT policy,reviewer_configuration_id,reviewer_profile_digest,reviewer_family,author_attempt_id,author_configuration_id,author_family,same_family,blind,reason,eligible,assigner_principal,assigned_unix_ms
            FROM review_assignments WHERE opportunity_id=?1", [&id],
            |r| Ok(json!({"policy": r.get::<_, String>(0)?, "reviewer_configuration_id": r.get::<_, String>(1)?, "reviewer_profile_digest": r.get::<_, String>(2)?,
                "reviewer_family": r.get::<_, Option<String>>(3)?, "author_attempt_id": r.get::<_, String>(4)?, "author_configuration_id": r.get::<_, Option<String>>(5)?,
                "author_family": r.get::<_, Option<String>>(6)?, "same_family": r.get::<_, Option<bool>>(7)?, "blind": r.get::<_, bool>(8)?, "reason": r.get::<_, String>(9)?,
                "eligible": serde_json::from_str::<Value>(&r.get::<_, String>(10)?).unwrap_or(Value::Null), "assigner_principal": r.get::<_, String>(11)?,
                "assigned_unix_ms": r.get::<_, i64>(12)?}))).optional()?;
        let sessions: Vec<Value> = db.prepare("SELECT r.session_id,r.ordinal,r.attempt_id,r.configuration_id,r.matches_assignment,r.same_attempt_as_author,r.recorder_principal,r.started_unix_ms,
                c.outcome,c.reason,c.findings_submitted,c.finding_refs,c.evidence_refs,c.coverage_basis,c.trust,c.receipt_digest,c.recorder_principal,c.completed_unix_ms
            FROM review_sessions r LEFT JOIN review_completions c ON c.session_id=r.session_id WHERE r.opportunity_id=?1 ORDER BY r.ordinal")?
            .query_map([&id], |r| {
                let completion = match r.get::<_, Option<String>>(8)? {
                    None => Value::Null,
                    Some(outcome) => json!({"outcome": outcome, "reason": r.get::<_, Option<String>>(9)?, "findings_submitted": r.get::<_, i64>(10)?,
                        "finding_refs": serde_json::from_str::<Value>(&r.get::<_, String>(11)?).unwrap_or(Value::Null),
                        "evidence_refs": serde_json::from_str::<Value>(&r.get::<_, String>(12)?).unwrap_or(Value::Null),
                        "coverage_basis": r.get::<_, String>(13)?, "trust": r.get::<_, String>(14)?, "receipt_digest": r.get::<_, String>(15)?,
                        "recorder_principal": r.get::<_, String>(16)?, "completed_unix_ms": r.get::<_, i64>(17)?,
                        "acceptance": unavailable(INACTIVE)}),
                };
                Ok(json!({"session_id": r.get::<_, String>(0)?, "ordinal": r.get::<_, i64>(1)?, "attempt_id": r.get::<_, String>(2)?,
                    "configuration_id": r.get::<_, Option<String>>(3)?, "matches_assignment": r.get::<_, Option<bool>>(4)?,
                    "same_attempt_as_author": r.get::<_, bool>(5)?, "recorder_principal": r.get::<_, String>(6)?, "started_unix_ms": r.get::<_, i64>(7)?,
                    "completion": completion}))
            })?.collect::<rusqlite::Result<_>>()?;
        let completed = sessions.iter().find(|s| s["completion"]["outcome"] == "completed");
        let status = if assignment.is_none() { "unassigned" } else if sessions.is_empty() { "no_session" } else if completed.is_some() { "completed" }
            else if sessions.iter().any(|s| s["completion"].is_null()) { "in_progress" } else { "ended_without_completion" };
        let findings = completed.and_then(|s| s["completion"]["findings_submitted"].as_i64());
        let assigned = assignment.as_ref().and_then(|a| a["assigned_unix_ms"].as_i64());
        record["status"] = json!(status);
        // Unknown is not zero: only a completed review has a findings count.
        record["findings_submitted"] = findings.map_or_else(|| unavailable(status), |n| json!(n));
        record["assignment"] = assignment.unwrap_or(Value::Null);
        record["sessions"] = Value::Array(sessions);
        out.push(Opportunity { record, kind, protocol, assigned, created, status, findings });
    }
    Ok(out)
}

fn show(project: &Path, since: Option<i64>) -> Result<Value> {
    let listed = match read(project)? { Some(db) => opportunities(&db)?, None => Vec::new() };
    let records: Vec<Value> = listed.into_iter().filter(|o| since.is_none_or(|s| o.created >= s)).map(|o| o.record).collect();
    Ok(json!({"acceptance": {"active": false, "reason": INACTIVE}, "opportunities": records, "since_unix_ms": since}))
}

fn ratio(numerator: usize, denominator: usize) -> Value {
    if denominator == 0 { Value::Null } else { json!(format!("{numerator}/{denominator}")) }
}

/// Metrics merged into `telemetry <slug> report` (`super::metrics::report`).
/// M20 review completion over assigned opportunities (declared coverage);
/// M21-M24 need accepted decisions and are inactive.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let mut m20 = json!({"definition": "M20.v1", "name": "review_completion", "basis": "declared", "trust": "proposal"});
    match read(project)? {
        None => m20["value"] = unavailable("review_capture_absent"),
        Some(db) => {
            let all = opportunities(&db)?;
            let unassigned = all.iter().filter(|o| o.assigned.is_none() && since.is_none_or(|s| o.created >= s)).count();
            let cohort: Vec<&Opportunity> = all.iter().filter(|o| o.assigned.is_some_and(|at| since.is_none_or(|s| at >= s))).collect();
            let completed = cohort.iter().filter(|o| o.status == "completed").count();
            let count = |status: &str| cohort.iter().filter(|o| o.status == status).count();
            let mut by = BTreeMap::<String, (usize, usize)>::new();
            for o in &cohort {
                let cell = by.entry(format!("{}/{}", o.kind, o.protocol)).or_default();
                cell.1 += 1;
                if o.status == "completed" { cell.0 += 1; }
            }
            m20["numerator"] = json!(completed);
            m20["denominator"] = json!(cohort.len());
            m20["value"] = ratio(completed, cohort.len());
            if cohort.is_empty() { m20["reason"] = json!("empty_denominator"); }
            m20["status"] = json!({"completed": completed, "completed_empty": cohort.iter().filter(|o| o.findings == Some(0)).count(),
                "ended_without_completion": count("ended_without_completion"), "in_progress": count("in_progress"), "no_session": count("no_session")});
            m20["unassigned"] = json!(unassigned);
            m20["by_kind_protocol"] = by.into_iter().map(|(k, (n, d))| (k, ratio(n, d))).collect();
        }
    }
    let mut out = BTreeMap::from([("M20".to_owned(), m20)]);
    for (id, name) in [("M21", "validated_unique_findings"), ("M22", "proposal_validation_rate"), ("M23", "duplicate_report_share"), ("M24", "review_discovery_efficiency")] {
        out.insert(id.to_owned(), json!({"definition": format!("{id}.v1"), "name": name, "value": unavailable(INACTIVE)}));
    }
    Ok(out)
}

/// Ticker telemetry pass, after the Codex collect, within `budget`. Writes only the sidecar.
pub fn tick(_project: &Path, _budget: super::codex::Budget) -> Result<()> { Ok(()) }
