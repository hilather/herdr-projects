//! `telemetry <slug> review fixes ...` (contracts-review.md §6, plan TM3.3):
//! repair opportunities, fix proposals, exact-candidate verification and
//! integration links, reopenings, introduction decisions and role credit,
//! plus the fix metrics (M21, M25, M26, M27, M29; M24 unavailable). Every
//! write is the project owner's (`operator:cli`) through `SqliteStore`; `show`
//! and the metrics read `state.db` strictly read-only.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::store::{CREDIT_UNIT, CreditShareSpec, DEFAULT_REPAIR_HORIZON_MS, FixState, IntroductionRequest, RepairAssignment, SqliteStore, credit_text, fix_state};

const OPERATOR: &str = "operator:cli";
const TRIAGE_AUTHORITY: &str = "operator_owner.v1";
const DAY_MS: i64 = 86_400_000;
/// M24 needs the lifecycle cost of review opportunities, which nothing allocates.
const NO_REVIEW_COST: &str = "review_cost_unallocated";

/// `herdr-projects telemetry <slug> review fixes ...`
#[derive(clap::Subcommand)]
pub enum FixesCommand {
    /// Findings' remediation, resolution intervals, reopenings and role
    /// credit, repair opportunities and the fix history, replayed to a
    /// history sequence, as JSON. Read-only.
    Show {
        /// Replay the finding and fix history only up to this sequence (default: its head).
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// Open a repair opportunity for a validated finding, freezing its
    /// initial assignment group (`--assign PROFILE` or `--unassigned`).
    Open {
        finding: String,
        /// Retained native profile the repair is initially assigned to.
        #[arg(long, conflicts_with = "unassigned", required_unless_present = "unassigned")]
        assign: Option<String>,
        #[arg(long)]
        unassigned: bool,
        /// Horizon the opportunity is followed to (days, default 14).
        #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(u32).range(1..=3650))]
        horizon_days: u32,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Bind an attempt to an open repair opportunity before it has any result.
    Bind {
        repair: i64,
        #[arg(long)]
        attempt: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Record a bound attempt's result submission as a fix proposal.
    Propose {
        repair: i64,
        #[arg(long)]
        submission: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Decide that an accepted verification run of the proposal's exact candidate repairs the finding.
    Verify {
        proposal: i64,
        #[arg(long)]
        run: String,
        /// `regression_reproduced` or `approved_alternative`.
        #[arg(long)]
        assurance: String,
        /// `sha256:<hex64>` or `verification_run:<hex64>`, repeatable (at least one).
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Link the integration of a verified fix's exact candidate (`integrated_commits` id).
    Integrate {
        proposal: i64,
        #[arg(long)]
        integrated: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Close a repair opportunity: `fixed`, `no_fix` or `cancelled`.
    Close {
        repair: i64,
        #[arg(long)]
        outcome: String,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Reopen a currently resolved finding: `regression` (defect at a later
    /// exact revision) or `reverted` (the fix was reverted).
    Reopen {
        finding: String,
        #[arg(long)]
        reason: String,
        /// The exact commit where the defect was observed (or the revert).
        #[arg(long)]
        observed: String,
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Record a causal introduction decision (`--commit` and `--method`, with
    /// `--contributor ATTEMPT=N/D` shares) or `--unattributable`.
    Introduce {
        finding: String,
        #[arg(long, conflicts_with = "unattributable", required_unless_present = "unattributable", requires = "method")]
        commit: Option<String>,
        /// `controlled_reproducer`, `reliable_bisect` or `minimized_patch`.
        #[arg(long)]
        method: Option<String>,
        #[arg(long = "contributor")]
        contributors: Vec<String>,
        #[arg(long)]
        unattributable: bool,
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Allocate a role's credit (`discovery`, or `implementation` of `--proposal`) as `--share ATTEMPT=N/D`.
    Credit {
        finding: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        proposal: Option<i64>,
        #[arg(long = "share", required = true)]
        shares: Vec<String>,
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
    /// Retract a credit allocation, reopening or introduction decision recorded in error.
    Retract {
        seq: i64,
        #[arg(long)]
        expect_seq: Option<i64>,
    },
}

fn share(text: &str) -> Result<CreditShareSpec> {
    let (attempt, fraction) = text.rsplit_once('=').with_context(|| format!("share {text} is not ATTEMPT=N/D"))?;
    let (num, den) = fraction.split_once('/').unwrap_or((fraction, "1"));
    Ok(CreditShareSpec { attempt_id: attempt.to_owned(), num: num.parse().with_context(|| format!("share {text}"))?, den: den.parse().with_context(|| format!("share {text}"))? })
}

pub(super) fn run(project: &Path, command: FixesCommand, now: i64) -> Result<Value> {
    let open = || SqliteStore::open(&project.join(".state/state.db"));
    let event = match command {
        FixesCommand::Show { as_of } => {
            let db = super::read(project)?.context("fix attribution needs store schema 56")?;
            let state = fix_state(&db, as_of)?.context("fix attribution needs store schema 56")?;
            return Ok(json!({"fixes": state}));
        }
        FixesCommand::Open { finding, assign, unassigned: _, horizon_days, expect_seq } => {
            let assignment = assign.map_or(RepairAssignment::Unassigned, RepairAssignment::Profile);
            open()?.open_repair(&finding, &assignment, i64::from(horizon_days) * DAY_MS, expect_seq, OPERATOR, now)?
        }
        FixesCommand::Bind { repair, attempt, expect_seq } => open()?.bind_repair_attempt(repair, &attempt, expect_seq, OPERATOR, now)?,
        FixesCommand::Propose { repair, submission, expect_seq } => open()?.propose_fix(repair, &submission, expect_seq, OPERATOR, now)?,
        FixesCommand::Verify { proposal, run, assurance, evidence, expect_seq } => open()?.verify_fix(proposal, &run, &assurance, &evidence, expect_seq, OPERATOR, now)?,
        FixesCommand::Integrate { proposal, integrated, expect_seq } => open()?.integrate_fix(proposal, &integrated, expect_seq, OPERATOR, now)?,
        FixesCommand::Close { repair, outcome, expect_seq } => open()?.close_repair(repair, &outcome, expect_seq, OPERATOR, now)?,
        FixesCommand::Reopen { finding, reason, observed, evidence, expect_seq } => open()?.reopen_finding(&finding, &reason, &observed, &evidence, expect_seq, OPERATOR, now)?,
        FixesCommand::Introduce { finding, commit, method, contributors, unattributable: _, evidence, expect_seq } => {
            let request = match commit {
                Some(introducing_oid) => IntroductionRequest::Attributed { introducing_oid, method: method.unwrap_or_default(),
                    contributors: contributors.iter().map(|c| share(c)).collect::<Result<_>>()? },
                None => { anyhow::ensure!(contributors.is_empty() && method.is_none(), "an unattributable introduction names no method or contributors"); IntroductionRequest::Unattributable }
            };
            open()?.attribute_introduction(&finding, &request, &evidence, expect_seq, OPERATOR, now)?
        }
        FixesCommand::Credit { finding, role, proposal, shares, evidence, expect_seq } => {
            let specs = shares.iter().map(|s| share(s)).collect::<Result<Vec<_>>>()?;
            open()?.allocate_credit(&finding, &role, proposal, &specs, &evidence, expect_seq, OPERATOR, now)?
        }
        FixesCommand::Retract { seq, expect_seq } => open()?.retract_attribution(seq, expect_seq, OPERATOR, now)?,
    };
    Ok(json!({"event": event}))
}

fn unavailable(reason: &str) -> Value { json!({"status": "unavailable", "reason": reason}) }

fn ratio(numerator: usize, denominator: usize) -> Value { if denominator == 0 { Value::Null } else { json!(format!("{numerator}/{denominator}")) } }

/// An exact ratio of credit units as a reduced `"n/d"`.
fn credit_ratio(allocated: u64, eligible: u64) -> Value {
    fn gcd(a: u64, b: u64) -> u64 { if b == 0 { a } else { gcd(b, a % b) } }
    if eligible == 0 { return Value::Null; }
    let g = gcd(allocated, eligible).max(1);
    json!(format!("{}/{}", allocated / g, eligible / g))
}

/// Fix metrics at `now` over the head state: M21 discovery credit, M25/M26
/// finding outcomes and initial-assignment cohorts, M27 reopen rate, M29
/// attribution coverage; M24 unavailable.
pub(super) fn metrics(state: Option<&FixState>, now: i64, since: Option<i64>, horizon_ms: i64) -> BTreeMap<String, Value> {
    let head = |id: &str, name: &str| json!({"definition": format!("{id}.v1"), "name": name, "basis": "owner_attribution", "trust": TRIAGE_AUTHORITY});
    let mut m21 = head("M21", "validated_unique_findings");
    let mut m25 = head("M25", "verified_fix_rate");
    let mut m26 = head("M26", "currently_resolved_rate");
    let mut m27 = head("M27", "reopen_rate");
    let mut m29 = head("M29", "quality_attribution_coverage");
    let m24 = json!({"definition": "M24.v1", "name": "review_discovery_efficiency", "value": unavailable(NO_REVIEW_COST)});
    let Some(state) = state else {
        for m in [&mut m21, &mut m25, &mut m26, &mut m27, &mut m29] { m["value"] = unavailable("fix_attribution_absent"); }
        return BTreeMap::from([("M21".into(), m21), ("M24".into(), m24), ("M25".into(), m25), ("M26".into(), m26), ("M27".into(), m27), ("M29".into(), m29)]);
    };
    let unit = CREDIT_UNIT;
    // F: validated unique findings, windowed by their discovery's arrival.
    let cohort: Vec<_> = state.findings.iter().filter(|f| f.status == "validated" && since.is_none_or(|s| f.discovered_unix_ms.is_some_and(|at| at >= s))).collect();

    // M21: discovery credit, never a count of every contributor.
    let mut by_configuration: BTreeMap<String, u64> = BTreeMap::new();
    let (mut allocated, mut participation) = (0u64, 0usize);
    for f in &cohort {
        let Some(d) = &f.discovery else { continue };
        allocated += d.allocated_units;
        for s in &d.shares {
            participation += 1;
            *by_configuration.entry(s.configuration_id.clone().unwrap_or_else(|| "unknown".into())).or_default() += s.units;
        }
    }
    m21["value"] = json!(credit_text(allocated));
    m21["unallocated"] = json!(credit_text(cohort.len() as u64 * unit - allocated));
    m21["by_configuration"] = by_configuration.into_iter().map(|(k, v)| (k, json!(credit_text(v)))).collect();
    m21["participation"] = json!(participation);
    m21["drilldown"] = json!({"validated_unique_findings": cohort.len()});
    m21["observational"] = json!(true);

    // M25/M26 finding outcomes over F.
    let verified = cohort.iter().filter(|f| f.verified).count();
    let resolved = cohort.iter().filter(|f| f.currently_resolved).count();
    for (m, n) in [(&mut m25, verified), (&mut m26, resolved)] {
        m["value"] = ratio(n, cohort.len());
        m["findings"] = json!({"label": "finding_outcomes", "numerator": n, "denominator": cohort.len()});
        if cohort.is_empty() { m["reason"] = json!("empty_denominator"); }
    }
    // Assignment cohorts R_g: repair opportunities by initial group, followed to their horizon.
    let current: BTreeSet<i64> = state.findings.iter().flat_map(|f| &f.resolutions).filter(|r| r.ended_seq.is_none()).map(|r| r.integration_seq).collect();
    #[derive(Default)]
    struct Cell { eligible: usize, verified: usize, resolved: usize, censored: usize, reassigned: usize }
    let mut groups: BTreeMap<String, Cell> = BTreeMap::new();
    let mut effective: BTreeSet<String> = BTreeSet::new();
    for r in state.repairs.iter().filter(|r| since.is_none_or(|s| r.opened_unix_ms >= s)) {
        let cell = groups.entry(r.configuration_id.clone().unwrap_or_else(|| "unassigned".into())).or_default();
        effective.extend(r.attempts.iter().filter_map(|a| a.configuration_id.clone()));
        let end = r.opened_unix_ms + r.horizon_ms;
        if r.closure.is_none() && now < end { cell.censored += 1; continue; }
        cell.eligible += 1;
        if r.attempts.len() > 1 { cell.reassigned += 1; }
        if r.proposals.iter().any(|p| p.verification.as_ref().is_some_and(|v| v.recorded_unix_ms <= end)) { cell.verified += 1; }
        if r.proposals.iter().any(|p| p.integration.as_ref().is_some_and(|i| current.contains(&i.seq) && i.recorded_unix_ms <= end)) { cell.resolved += 1; }
    }
    for configuration in effective { groups.entry(configuration).or_default(); }
    let censored: usize = groups.values().map(|c| c.censored).sum();
    for (m, pick) in [(&mut m25, (|c: &Cell| c.verified) as fn(&Cell) -> usize), (&mut m26, |c: &Cell| c.resolved)] {
        m["by_assignment"] = groups.iter().map(|(g, c)| {
            let mut cell = json!({"numerator": pick(c), "denominator": c.eligible, "value": ratio(pick(c), c.eligible), "not_achieved": c.eligible - pick(c),
                "reassigned": c.reassigned, "censored": c.censored});
            if c.eligible == 0 { cell["reason"] = json!(if c.censored > 0 { "empty_denominator" } else { "no_assigned_opportunities" }); }
            (g.clone(), cell)
        }).collect();
        m["assignment_policy"] = json!("repair_assignment.v1");
        m["censored"] = json!(censored);
        m["as_of_seq"] = json!(state.as_of_seq);
    }

    // M27: integrated fixes reopened within the horizon / integrated fixes observed that long.
    let reopened: BTreeMap<i64, i64> = state.findings.iter().flat_map(|f| &f.reopenings).filter(|o| o.retracted_seq.is_none())
        .fold(BTreeMap::new(), |mut m, o| { m.entry(o.integration_seq).or_insert(o.recorded_unix_ms); m });
    let (mut observed, mut reopened_within, mut not_observed) = (0, 0, 0);
    for i in state.repairs.iter().flat_map(|r| &r.proposals).filter_map(|p| p.integration.as_ref()).filter(|i| since.is_none_or(|s| i.integrated_unix_ms >= s)) {
        let end = i.integrated_unix_ms + horizon_ms;
        let within = reopened.get(&i.seq).is_some_and(|&at| at <= end);
        if within { reopened_within += 1; observed += 1; } else if now >= end { observed += 1; } else { not_observed += 1; }
    }
    m27["numerator"] = json!(reopened_within);
    m27["denominator"] = json!(observed);
    m27["value"] = ratio(reopened_within, observed);
    if observed == 0 { m27["reason"] = json!("empty_denominator"); }
    m27["censored"] = json!(not_observed);
    m27["horizon_days"] = json!(horizon_ms / DAY_MS);

    // M29: allocated role credit / eligible role credit, per role and overall.
    let mut roles = serde_json::Map::new();
    let (mut all_allocated, mut all_eligible) = (0u64, 0u64);
    for role in ["discovery", "validation", "implementation", "verification", "integration", "introduction"] {
        let credits: Vec<u64> = cohort.iter().filter_map(|f| match role {
            "discovery" => f.discovery.as_ref(), "validation" => f.validation.as_ref(), "implementation" => f.implementation.as_ref(),
            "verification" => f.verification.as_ref(), "integration" => f.integration.as_ref(), _ => f.introduction.as_ref().map(|i| &i.credit),
        }.map(|c| c.allocated_units)).collect();
        let (a, e) = (credits.iter().sum::<u64>(), credits.len() as u64 * unit);
        all_allocated += a;
        all_eligible += e;
        roles.insert(role.into(), json!({"allocated": credit_text(a), "eligible": credits.len(), "value": credit_ratio(a, e)}));
    }
    m29["value"] = credit_ratio(all_allocated, all_eligible);
    if all_eligible == 0 { m29["reason"] = json!("empty_denominator"); }
    m29["by_role"] = Value::Object(roles);
    BTreeMap::from([("M21".into(), m21), ("M24".into(), m24), ("M25".into(), m25), ("M26".into(), m26), ("M27".into(), m27), ("M29".into(), m29)])
}

/// Default M27 horizon (days), as C2's integration outcomes.
pub(super) const DEFAULT_HORIZON_DAYS: i64 = 14;
const _: () = assert!(DEFAULT_REPAIR_HORIZON_MS == DEFAULT_HORIZON_DAYS * DAY_MS);
