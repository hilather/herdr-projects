//! `telemetry <slug> review authority import|revoke|show` and `review accept`
//! (contracts-review.md §10): delegated `code_review` authority and the
//! reviewer's acceptance decisions, plus M24 review discovery efficiency.
//! Signatures are verified by `crate::authority` against the pinned owner
//! policy (grants, revocations) and the grant subject's key (decisions);
//! `show` and M24 read `state.db` and the telemetry sidecar strictly read-only.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::store::{FindingState, review_authority_state};

/// Recorded on every decision under a grant.
pub(super) const AUTHORITY: &str = "delegated_code_review.v1";
/// Why acceptance is unavailable on a store without the authority tables.
pub(super) const ABSENT: &str = "review_authority_absent";

/// `herdr-projects telemetry <slug> review authority ...`
#[derive(clap::Subcommand)]
pub enum AuthorityCommand {
    /// Install an owner-signed `code_review` grant (`code_review_authority.v1`,
    /// namespace `code-review-authority@herdr-projects`).
    Import { document: PathBuf, signature: PathBuf },
    /// Record an owner-signed revocation (`code_review_revocation.v1`,
    /// namespace `code-review-revocation@herdr-projects`); stops later decisions.
    Revoke { document: PathBuf, signature: PathBuf },
    /// Grants with their status and decisions, as JSON. Read-only.
    Show,
}

pub(super) fn authority(project: &Path, command: AuthorityCommand) -> Result<Value> {
    Ok(match command {
        AuthorityCommand::Import { document, signature } => json!({"grant": crate::authority::import_review_authority(project, &document, &signature)?}),
        AuthorityCommand::Revoke { document, signature } => json!({"revocation": crate::authority::revoke_review_authority(project, &document, &signature)?}),
        AuthorityCommand::Show => {
            let db = super::read(project)?.context("review authority needs the reviewer-authority store schema")?;
            review_authority_state(&db, jiff::Timestamp::now().as_millisecond())?.context("review authority needs the reviewer-authority store schema")?
        }
    })
}

pub(super) fn accept(project: &Path, session: &str, document: &Path, signature: &Path) -> Result<Value> {
    Ok(json!({"acceptance": crate::authority::accept_review(project, session, document, signature)?}))
}

/// Whether the store has the authority tables.
pub(super) fn present(db: &rusqlite::Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_authority_grants')", [], |r| r.get(0))?)
}

/// Each decided session's decision record, or `None` before the authority tables.
pub(super) fn decisions(db: &rusqlite::Connection) -> Result<Option<BTreeMap<String, Value>>> {
    if !present(db)? { return Ok(None); }
    let rows = db.prepare("SELECT session_id,decision,reason,authority_principal,authority_ref,authority,decided_unix_ms FROM review_acceptances")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, json!({"decision": r.get::<_, String>(1)?, "reason": r.get::<_, Option<String>>(2)?,
            "authority_principal": r.get::<_, String>(3)?, "grant_id": r.get::<_, String>(4)?, "authority": r.get::<_, String>(5)?,
            "decided_unix_ms": r.get::<_, i64>(6)?}))))?.collect::<rusqlite::Result<_>>()?;
    Ok(Some(rows))
}

/// `{accepted, rejected, undecided}` counts of `sessions` by their decision.
pub(super) fn acceptance_counts<'a>(decided: &BTreeMap<String, Value>, sessions: impl IntoIterator<Item = &'a str>) -> Value {
    let (mut accepted, mut rejected, mut undecided) = (0usize, 0usize, 0usize);
    for s in sessions {
        match decided.get(s).and_then(|d| d["decision"].as_str()) {
            Some("accepted") => accepted += 1,
            Some(_) => rejected += 1,
            None => undecided += 1,
        }
    }
    json!({"accepted": accepted, "rejected": rejected, "undecided": undecided, "authority": AUTHORITY})
}

fn gcd(a: u128, b: u128) -> u128 { if b == 0 { a } else { gcd(b, a % b) } }

/// An exact decimal string as `(numerator, denominator)` (a power of ten).
fn decimal(text: &str) -> Option<(u128, u128)> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) || fraction.len() > 18 { return None; }
    let den = 10u128.checked_pow(fraction.len() as u32)?;
    let num = format!("{whole}{fraction}").parse::<u128>().ok()?;
    Some((num, den))
}

/// Exact sum of decimal strings, as a trimmed decimal string.
fn add_decimals(values: &[String]) -> Option<String> {
    let places = values.iter().map(|v| v.split_once('.').map_or(0, |(_, f)| f.len())).max().unwrap_or(0);
    let scale = 10u128.checked_pow(places as u32)?;
    let mut total: u128 = 0;
    for v in values {
        let (n, d) = decimal(v)?;
        total = total.checked_add(n.checked_mul(scale / d)?)?;
    }
    let whole = total / scale;
    let fraction = format!("{:0width$}", total % scale, width = places);
    let fraction = fraction.trim_end_matches('0');
    Some(if fraction.is_empty() { whole.to_string() } else { format!("{whole}.{fraction}") })
}

/// One review session of an opportunity, as M24 weighs it.
pub(super) struct ReviewSessionCost<'a> { pub attempt_id: &'a str, pub same_attempt_as_author: bool }

/// One assigned opportunity in the window, as M24 weighs it.
pub(super) struct ReviewOpportunityCost<'a> {
    pub status: &'a str,
    /// The completed session, when there is one.
    pub completed_session: Option<&'a str>,
    /// Latest completion time among its sessions.
    pub ended_unix_ms: Option<i64>,
    pub sessions: Vec<ReviewSessionCost<'a>>,
}

/// M24 review discovery efficiency (doc 07): validated unique findings
/// discovered by the closed opportunities `Q` / lifecycle review cost of `Q`.
/// Closed: a completed review with a delegated decision, or every session
/// ended without completing. Cost: every session of `Q` (failed, timed-out,
/// rejected and zero-find reviews included), each its attempt's primary
/// published-rate estimate from the accounting sidecar (read-only).
#[allow(clippy::too_many_arguments)]
pub(super) fn m24(project: &Path, cohort: &[ReviewOpportunityCost], all_attempts: &BTreeMap<String, usize>, authors: &BTreeSet<String>,
    decided: Option<&BTreeMap<String, Value>>, triage: Option<&FindingState>, horizon_ms: i64, now: i64) -> Result<Value> {
    let mut m = json!({"definition": "M24.v1", "name": "review_discovery_efficiency", "acceptance_authority": AUTHORITY, "triage_trust": "operator_owner.v1",
        "basis": "published_rate_estimate", "rate_cards": "fixture_only", "observational": true,
        "caveat": "rate cards are fixture-only by owner decision: the cost is only as real as the cards imported, never a provider charge",
        "not_included": ["owner_triage_time", "unlinked_child_sessions"]});
    let Some(decided) = decided else { m["value"] = super::unavailable(ABSENT); return Ok(m) };
    let Some(triage) = triage else { m["value"] = super::unavailable("finding_triage_absent"); return Ok(m) };
    // Claims → (session, seed-linked); submissions' pending state per session.
    let mut claim_session = BTreeMap::<i64, (&str, bool)>::new();
    let mut pending_sessions = BTreeSet::<&str>::new();
    for s in &triage.submissions {
        for c in &s.claims { claim_session.insert(c.claim_id, (s.session_id.as_str(), c.seed_linked)); }
        if s.outcome == "pending" { pending_sessions.insert(s.session_id.as_str()); }
    }
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut q = Vec::new();
    let mut accepted_sessions = BTreeSet::<&str>::new();
    let mut rejected_sessions = BTreeSet::<&str>::new();
    let mut triage_partial = 0usize;
    for o in cohort {
        let (class, closed_at) = match (o.status, o.completed_session) {
            ("completed", Some(s)) => match decided.get(s) {
                Some(d) if d["decision"] == "accepted" => ("accepted", d["decided_unix_ms"].as_i64()),
                Some(d) => ("rejected", d["decided_unix_ms"].as_i64()),
                None => ("awaiting_acceptance", None),
            },
            ("ended_without_completion", _) => ("unsuccessful", o.ended_unix_ms),
            _ => ("open", None),
        };
        if closed_at.is_none() { *counts.entry(class).or_default() += 1; continue; }
        if class == "accepted" && o.completed_session.is_some_and(|s| pending_sessions.contains(s)) {
            // Adjudication horizon: pending triage within it is not closed yet.
            if closed_at.unwrap_or(now).saturating_add(horizon_ms) > now { *counts.entry("awaiting_adjudication").or_default() += 1; continue; }
            triage_partial += 1;
        }
        *counts.entry(class).or_default() += 1;
        if let Some(s) = o.completed_session {
            if class == "accepted" { accepted_sessions.insert(s); } else if class == "rejected" { rejected_sessions.insert(s); }
        }
        q.push(o);
    }
    let (mut found, mut excluded_rejected, mut seeded) = (0u128, 0usize, 0usize);
    for f in triage.findings.iter().filter(|f| f.status == "validated") {
        let Some((session, seed_linked)) = f.discovery_claim.and_then(|c| claim_session.get(&c)) else { continue };
        if accepted_sessions.contains(session) { if *seed_linked { seeded += 1 } else { found += 1 } }
        else if rejected_sessions.contains(session) { excluded_rejected += 1 }
    }
    m["opportunities"] = json!({"closed": q.len(), "accepted": counts.get("accepted").copied().unwrap_or(0), "rejected": counts.get("rejected").copied().unwrap_or(0),
        "unsuccessful": counts.get("unsuccessful").copied().unwrap_or(0), "awaiting_acceptance": counts.get("awaiting_acceptance").copied().unwrap_or(0),
        "awaiting_adjudication": counts.get("awaiting_adjudication").copied().unwrap_or(0), "open": counts.get("open").copied().unwrap_or(0)});
    m["numerator"] = json!(found as u64);
    m["excluded_rejected_review"] = json!(excluded_rejected);
    m["seeded_evaluation"] = json!(seeded);
    m["as_of_seq"] = json!(triage.as_of_seq);
    if q.is_empty() { m["value"] = Value::Null; m["reason"] = json!("empty_denominator"); return Ok(m); }
    // Cost of every session of Q, from the latest valuation revision.
    let Some(sidecar) = crate::telemetry::sidecar::read(project)? else { m["value"] = super::unavailable("collection_not_run"); return Ok(m) };
    let cost = crate::telemetry::accounting::cost::cost(&sidecar, None, None)?;
    if cost["status"] == "unavailable" { m["value"] = super::unavailable(cost["reason"].as_str().unwrap_or("not_priced")); return Ok(m); }
    m["revision"] = cost["revision"].clone();
    let estimates: BTreeMap<&str, &Value> = cost["attempts"].as_array().map(|a| a.iter().filter_map(|x| Some((x["attempt_id"].as_str()?, &x["estimate"]))).collect()).unwrap_or_default();
    let (mut amounts, mut currencies, mut unavailable) = (Vec::new(), BTreeSet::new(), BTreeMap::<String, usize>::new());
    let (mut sessions, mut priced, mut partial) = (0usize, 0usize, 0usize);
    for o in &q {
        for s in &o.sessions {
            sessions += 1;
            // An attempt that reviewed more than once or also authored cannot give one session its whole cost.
            if s.same_attempt_as_author || authors.contains(s.attempt_id) || all_attempts.get(s.attempt_id).copied().unwrap_or(0) > 1 {
                *unavailable.entry("shared_attempt_unallocated".into()).or_default() += 1;
                continue;
            }
            let Some(estimate) = estimates.get(s.attempt_id) else { *unavailable.entry("no_usage_bound".into()).or_default() += 1; continue };
            match estimate["status"].as_str() {
                Some("complete") => { priced += 1; amounts.push(estimate["amount"].as_str().unwrap_or("0").to_owned()); currencies.insert(estimate["currency"].as_str().unwrap_or_default().to_owned()); }
                Some("partial") => { partial += 1; amounts.push(estimate["priced_amount"].as_str().unwrap_or("0").to_owned()); currencies.insert(estimate["currency"].as_str().unwrap_or_default().to_owned()); }
                _ => *unavailable.entry(estimate["reason"].as_str().unwrap_or("unavailable").to_owned()).or_default() += 1,
            }
        }
    }
    m["sessions"] = json!({"total": sessions, "priced": priced, "partial": partial, "unavailable": unavailable});
    if currencies.len() > 1 { m["value"] = json!({"status": "unavailable", "reason": "mixed_currency", "currencies": currencies}); return Ok(m); }
    let Some(currency) = currencies.into_iter().next() else { m["value"] = super::unavailable("review_cost_unavailable"); return Ok(m) };
    let amount = add_decimals(&amounts).context("review cost exceeds the exact range")?;
    let complete = partial == 0 && unavailable.is_empty();
    m["cost"] = json!({"status": if complete { "complete" } else { "partial" }, "currency": currency, "amount": amount});
    m["unit"] = json!(format!("findings/{currency}"));
    let (num, den) = decimal(&amount).context("invalid review cost")?;
    let ratio = if num == 0 { None } else {
        let (n, d) = (found.checked_mul(den).context("M24 exceeds the exact range")?, num);
        let g = gcd(n, d).max(1);
        Some(if d / g == 1 { format!("{}", n / g) } else { format!("{}/{}", n / g, d / g) })
    };
    let mut reasons = Vec::new();
    if !complete { reasons.push("review_cost_partial"); }
    if triage_partial > 0 { reasons.push("triage_pending_past_horizon"); }
    m["triage_partial"] = json!(triage_partial);
    m["value"] = match ratio {
        None => json!({"status": "unavailable", "reason": "zero_cost"}),
        Some(r) if reasons.is_empty() => json!(r),
        Some(r) => json!({"status": "partial", "value": r, "reasons": reasons}),
    };
    Ok(m)
}
