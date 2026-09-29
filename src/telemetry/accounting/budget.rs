//! TM2.4 budget bridge in **shadow mode** (docs/telemetry/contracts-accounting.md
//! §14) and M04: what a budget policy would have decided, from canonical
//! policy, attempts and inputs read strictly read-only and the valued ledger.
//! Nothing here is enforced or written: admission and scheduling never read
//! it, and `state.db` is opened through `telemetry::read_only` only.
use super::charges;
use super::cost::{self, Dec, Stored};
use crate::domain::{BudgetPolicy, UnknownUsagePolicy};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "lost"];
const CONSUMPTION: &str = "published_rate_estimate of the telemetry ledger (fixture-only rate cards); never canonically accepted usage";

/// A what-if policy file (`--policy`): synthetic limits, reservation
/// estimates and a new request evaluated beside the canonical policy. It is
/// never installed and grants nothing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WhatIf {
    synthetic: bool,
    policy_id: String,
    version: i64,
    source: String,
    #[serde(default)]
    unknown_usage: Option<UnknownUsagePolicy>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    project: Option<Limits>,
    #[serde(default)]
    tasks: BTreeMap<String, Limits>,
    /// In-flight reservation estimates of open attempts.
    #[serde(default)]
    reservations: BTreeMap<String, Estimate>,
    /// The next admission being evaluated.
    #[serde(default)]
    request: Option<Request>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_amount: Option<String>,
    max_provider_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Estimate {
    amount: Option<String>,
    tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Request {
    task: Option<String>,
    amount: Option<String>,
    tokens: Option<u64>,
}

/// A policy unit: provider tokens, or money in one currency.
#[derive(Clone, Copy)]
enum Unit<'a> {
    Tokens,
    Money(&'a str),
}

impl Unit<'_> {
    fn name(&self) -> &'static str { match self { Unit::Tokens => "provider_tokens", Unit::Money(_) => "amount" } }
    fn show(&self, d: Dec) -> Value {
        match self { Unit::Tokens => d.to_string().parse::<i64>().map_or_else(|_| json!(d.to_string()), |n| json!(n)), Unit::Money(_) => json!(d.to_string()) }
    }
}

/// A canonical attempt as the shadow sees it.
struct Attempt {
    id: String,
    task: String,
    state: String,
    kind: Option<String>,
    pinned: Option<i64>,
    decided: Option<i64>,
    never_running: bool,
}

impl Attempt {
    fn open(&self) -> bool { !TERMINAL.contains(&self.state.as_str()) }
}

/// Canonical rows, read-only: attempts (with the budget revision their inputs
/// pinned and whether the lifecycle log shows they never ran) and the budget
/// policy history (validated as the store validates it).
/// The budget policy history, `None` without the table, `Err` when a record fails validation.
type Policies = Option<Result<Vec<BudgetPolicy>, &'static str>>;

fn canonical(db: &Connection) -> Result<(Vec<Attempt>, Policies)> {
    let table = |name: &str| cost::table(db, name);
    let (inputs, pinned, kind) = if table("attempt_inputs")? {
        ("LEFT JOIN attempt_inputs i ON i.attempt_id=a.id", "json_extract(i.payload,'$.inputs.budget.revision')", "json_extract(i.payload,'$.inputs.effective_profile.kind')")
    } else { ("", "NULL", "NULL") };
    let decided = if table("dispatch_decisions")? { "(SELECT decided_unix_ms FROM dispatch_decisions d WHERE d.attempt_id=a.id)" } else { "NULL" };
    let never = if table("attempt_lifecycle")? {
        "EXISTS(SELECT 1 FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state='reserved') AND NOT EXISTS(SELECT 1 FROM attempt_lifecycle l WHERE l.attempt_id=a.id AND l.state='running')"
    } else { "0" };
    let attempts = db.prepare(&format!("SELECT a.id,a.task_id,a.state,{kind},{pinned},{decided},{never} FROM attempts a {inputs} ORDER BY a.rowid"))?
        .query_map([], |r| Ok(Attempt { id: r.get(0)?, task: r.get(1)?, state: r.get(2)?, kind: r.get(3)?, pinned: r.get(4)?, decided: r.get(5)?, never_running: r.get(6)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let policies = if table("budget_policies")? {
        let rows: Vec<(i64, String, String)> = db.prepare("SELECT revision,payload,payload_hash FROM budget_policies ORDER BY revision")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        let valid = rows.iter().enumerate().all(|(n, (revision, payload, hash))| *revision == n as i64 + 1 && format!("{:x}", Sha256::digest(payload.as_bytes())) == *hash);
        Some(if !valid { Err("policy_record_invalid") } else {
            rows.iter().map(|(_, payload, _)| serde_json::from_str::<BudgetPolicy>(payload).map_err(|_| "policy_record_invalid")).collect()
        })
    } else { None };
    Ok((attempts, policies))
}

/// One attempt's known consumption in `unit` and the reasons part of it is unknown.
fn consumption(unit: Unit, attempt: &Attempt, rows: Option<&[&Stored]>) -> Result<(Dec, BTreeMap<String, usize>)> {
    let (mut known, mut unknown) = (Dec::ZERO, BTreeMap::<String, usize>::new());
    let Some(rows) = rows else {
        *unknown.entry("not_priced".to_owned()).or_default() += 1;
        return Ok((known, unknown));
    };
    for s in rows {
        let reason = match unit {
            Unit::Tokens => match s.tokens() { Some(t) => { known = known.add(Dec::parse(&t.to_string())?)?; continue } None => "usage_not_counted".to_owned() },
            Unit::Money(currency) => match (s.status.as_str(), s.currency.as_deref(), s.amount.as_deref()) {
                ("priced", Some(c), Some(amount)) if c == currency => { known = known.add(Dec::parse(amount)?)?; continue }
                ("priced", ..) => "currency_differs".to_owned(),
                _ => s.reason.clone().unwrap_or_default(),
            },
        };
        *unknown.entry(reason).or_default() += 1;
    }
    if rows.is_empty() && !attempt.open() && !attempt.never_running {
        let reason = if attempt.kind.as_deref().is_some_and(|k| k != "codex") { "adapter_absent" } else { "no_usage_observed" };
        *unknown.entry(reason.to_owned()).or_default() += 1;
    }
    Ok((known, unknown))
}

/// Admission's question in shadow: accepted consumption + remaining reserved
/// exposure + the new request against the limit. Known exposure over the
/// limit blocks whatever is unknown; otherwise unknown usage follows the
/// policy (`refuse` blocks, `allow_incomplete` warns); never 0.
#[allow(clippy::too_many_arguments)]
fn evaluate(unit: Unit, limit: Dec, attempts: &[&Attempt], rows: &BTreeMap<&str, Vec<&Stored>>, priced: bool,
    reservations: &BTreeMap<String, Estimate>, request: Option<Option<Dec>>, policy: UnknownUsagePolicy) -> Result<Value> {
    let (mut accepted, mut remaining) = (Dec::ZERO, Dec::ZERO);
    let mut unknown = Vec::new();
    let mut per_attempt = Vec::new();
    for a in attempts {
        let (known, reasons) = consumption(unit, a, priced.then(|| rows.get(a.id.as_str()).map(Vec::as_slice).unwrap_or(&[])))?;
        accepted = accepted.add(known)?;
        for (reason, entries) in &reasons { unknown.push(json!({"attempt_id": a.id, "reason": reason, "entries": entries})); }
        let mut reserved = Value::Null;
        if a.open() {
            let estimate = reservations.get(&a.id).and_then(|e| match unit {
                Unit::Tokens => e.tokens.map(|t| Dec::parse(&t.to_string())),
                Unit::Money(_) => e.amount.as_deref().map(Dec::rate),
            }).transpose()?;
            match estimate {
                Some(r) if !r.sub(known)?.is_negative() => {
                    remaining = remaining.add(r.sub(known)?)?;
                    reserved = json!({"reservation": unit.show(r), "remaining": unit.show(r.sub(known)?)});
                }
                Some(r) => {
                    unknown.push(json!({"attempt_id": a.id, "reason": "reservation_overrun", "entries": 0}));
                    reserved = json!({"reservation": unit.show(r), "remaining": super::unavailable("reservation_overrun")});
                }
                None => {
                    unknown.push(json!({"attempt_id": a.id, "reason": "in_flight_usage_unknown", "entries": 0}));
                    reserved = json!({"reservation": null, "remaining": super::unavailable("in_flight_usage_unknown")});
                }
            }
        }
        per_attempt.push(json!({"attempt_id": a.id, "task_id": a.task, "state": a.state, "accepted": unit.show(known), "in_flight": reserved}));
    }
    let exposure = accepted.add(remaining)?;
    let request_value = match request {
        None => Value::Null,
        Some(Some(r)) => unit.show(r),
        Some(None) => { unknown.push(json!({"attempt_id": null, "reason": "new_request_usage_unknown", "entries": 0})); super::unavailable("new_request_usage_unknown") }
    };
    let known_request = request.flatten();
    let projected = known_request.map_or(Ok(exposure), |r| exposure.add(r))?;
    let over = |d: Dec| -> Result<bool> { Ok(limit.sub(d)?.is_negative()) };
    let (decision, reason) = if over(exposure)? {
        ("would_block", "limit_exceeded")
    } else if known_request.is_some() && over(projected)? {
        ("would_block", "projected_exposure_exceeds_limit")
    } else if request.is_some_and(|r| r.is_none()) && !exposure.sub(limit)?.is_negative() {
        ("would_block", "no_headroom")
    } else if !unknown.is_empty() {
        match policy { UnknownUsagePolicy::Refuse => ("would_block", "provider_usage_unavailable"), UnknownUsagePolicy::AllowIncomplete => ("would_warn", "usage_incomplete") }
    } else {
        ("allow", "within_limit")
    };
    let mut out = json!({"dimension": unit.name(), "limit": unit.show(limit), "accepted": unit.show(accepted), "remaining_reserved": unit.show(remaining),
        "exposure": unit.show(exposure), "new_request": request_value, "projected": unit.show(projected), "unknown": unknown,
        "unknown_usage": match policy { UnknownUsagePolicy::Refuse => "refuse", UnknownUsagePolicy::AllowIncomplete => "allow_incomplete" },
        "decision": decision, "reason": reason, "attempts": per_attempt});
    if let Unit::Money(currency) = unit { out["currency"] = json!(currency); }
    Ok(out)
}

fn with(mut v: Value, extra: Value) -> Value {
    if let (Value::Object(v), Value::Object(e)) = (&mut v, extra) { v.extend(e); }
    v
}

/// `accounting budget-shadow`: the canonical budget policy (and an optional
/// what-if policy file) evaluated in shadow over the valued ledger. Strictly
/// read-only; `mode: shadow`, never enforced.
pub fn shadow(project: &Path, policy_file: Option<&Path>, as_of: Option<i64>) -> Result<Value> {
    let what_if: Option<(WhatIf, String)> = policy_file.map(|file| -> Result<_> {
        let text = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
        let w: WhatIf = charges::read_file(file)?;
        charges::synthetic(w.synthetic, &w.source).with_context(|| format!("what-if policy {}", file.display()))?;
        ensure!(!w.policy_id.is_empty() && w.version > 0, "policy_id must not be empty and version must be positive");
        if let Some(c) = &w.currency { charges::currency(c)?; }
        let money = w.project.iter().chain(w.tasks.values()).any(|l| l.max_amount.is_some());
        ensure!(!money || w.currency.is_some(), "a what-if policy with max_amount needs a currency");
        Ok((w, cost::digest(&text)))
    }).transpose()?;
    let state = project.join(".state/state.db");
    if !state.exists() { return Ok(super::unavailable("no_state_store")); }
    let db = crate::telemetry::read_only(&state)?;
    let (attempts, policies) = canonical(&db)?;
    let sidecar = crate::telemetry::sidecar::read(project)?;
    let valued = match &sidecar { Some(s) => charges::valuations(s, as_of)?, None => None };
    let mut rows = BTreeMap::<&str, Vec<&Stored>>::new();
    if let Some((_, stored)) = &valued {
        for s in stored.values() { if let Some(a) = &s.attempt_id { rows.entry(a.as_str()).or_default().push(s); } }
    }
    let priced = valued.is_some();
    let all: Vec<&Attempt> = attempts.iter().collect();
    let none = BTreeMap::new();
    let (reservations, request) = match &what_if { Some((w, _)) => (&w.reservations, w.request.as_ref()), None => (&none, None) };

    // The canonical policy in force (latest revision), as admission reads it today.
    let mut evaluations = Vec::new();
    let (canonical_json, today) = match &policies {
        None => (json!({"policy": null, "reason": "no_budget_policy"}), None),
        Some(Err(reason)) => (json!({"policy": super::unavailable(reason)}), None),
        Some(Ok(history)) if history.is_empty() => (json!({"policy": null, "reason": "no_budget_policy"}), None),
        Some(Ok(history)) => {
            let policy = history.last().context("policy")?;
            let reference = policy.reference().map_err(anyhow::Error::msg)?;
            let limits = &policy.limits;
            let (mut blockers, mut incomplete) = (Vec::new(), false);
            if limits.max_attempts.is_some_and(|cap| attempts.len() as u64 >= cap) { blockers.push("attempt_budget_exhausted"); }
            match (limits.max_provider_tokens, limits.unknown_usage) {
                (Some(0), _) => blockers.push("provider_token_budget_exhausted"),
                (Some(_), UnknownUsagePolicy::Refuse) => blockers.push("provider_usage_unavailable"),
                (Some(_), UnknownUsagePolicy::AllowIncomplete) => incomplete = true,
                (None, _) => {}
            }
            let source = json!({"policy_source": "canonical", "policy_revision": policy.revision, "scope": "project"});
            if let Some(cap) = limits.max_attempts {
                let blocked = attempts.len() as u64 >= cap;
                evaluations.push(with(source.clone(), json!({"dimension": "attempts", "limit": cap, "admitted": attempts.len(),
                    "decision": if blocked { "would_block" } else { "allow" }, "reason": if blocked { "attempt_budget_exhausted" } else { "within_limit" }})));
            }
            if let Some(cap) = limits.max_provider_tokens {
                let req = Some(request.and_then(|r| r.tokens).map(|t| Dec::parse(&t.to_string())).transpose()?);
                let e = evaluate(Unit::Tokens, Dec::parse(&cap.to_string())?, &all, &rows, priced, reservations, req, limits.unknown_usage)?;
                evaluations.push(with(source, e));
            }
            let today = if !blockers.is_empty() { "would_block" } else if incomplete { "would_warn" } else { "allow" };
            (json!({"policy": {"revision": policy.revision, "digest": reference.digest, "limits": limits},
                "decision_today": {"blockers": blockers, "incomplete": incomplete, "provider_tokens": "unknown"},
                "task_budgets": super::unavailable("no_canonical_task_budget")}), Some(today))
        }
    };
    let canonical_unknown = match &policies { Some(Ok(h)) => h.last().map(|p| p.limits.unknown_usage), _ => None };
    let shadow_canonical = evaluations.iter().map(|e| e["decision"].as_str().unwrap_or_default()).fold("allow", worst).to_owned();

    // The what-if policy: synthetic limits per project and task.
    if let Some((w, _)) = &what_if {
        let policy = w.unknown_usage.or(canonical_unknown).unwrap_or(UnknownUsagePolicy::Refuse);
        let source = json!({"policy_source": "what_if", "policy_id": w.policy_id, "policy_version": w.version});
        let mut scopes: Vec<(String, &Limits, Vec<&Attempt>, bool)> = Vec::new();
        if let Some(limits) = &w.project { scopes.push(("project".to_owned(), limits, all.clone(), true)); }
        for (task, limits) in &w.tasks {
            let of: Vec<&Attempt> = attempts.iter().filter(|a| &a.task == task).collect();
            scopes.push((format!("task:{task}"), limits, of, request.is_some_and(|r| r.task.as_deref() == Some(task.as_str()))));
        }
        for (scope, limits, of, requested) in scopes {
            let source = with(source.clone(), json!({"scope": scope}));
            if let (Some(max), Some(currency)) = (&limits.max_amount, &w.currency) {
                let req = if requested { Some(request.and_then(|r| r.amount.as_deref()).map(Dec::rate).transpose()?) } else { None };
                evaluations.push(with(source.clone(), evaluate(Unit::Money(currency), Dec::rate(max)?, &of, &rows, priced, reservations, req, policy)?));
            }
            if let Some(max) = limits.max_provider_tokens {
                let req = if requested { Some(request.and_then(|r| r.tokens).map(|t| Dec::parse(&t.to_string())).transpose()?) } else { None };
                evaluations.push(with(source, evaluate(Unit::Tokens, Dec::parse(&max.to_string())?, &of, &rows, priced, reservations, req, policy)?));
            }
        }
    }
    let decisions: BTreeSet<&str> = evaluations.iter().filter_map(|e| e["decision"].as_str()).collect();
    let reasons: BTreeSet<&str> = evaluations.iter().filter(|e| e["decision"] != "allow").filter_map(|e| e["reason"].as_str()).collect();
    Ok(json!({"mode": "shadow", "enforcement": "none", "canonical_writes": "none",
        "note": "what the budget policy would have decided; admission and scheduling never read this",
        "as_of_unix_ms": as_of, "consumption_basis": CONSUMPTION,
        "canonical": canonical_json, "evaluations": evaluations,
        "decision": {"would_block": decisions.contains("would_block"), "would_warn": decisions.contains("would_warn"), "reasons": reasons},
        "differs_from_canonical": today.map(|t| t != shadow_canonical),
        "attempts": attempts.iter().map(|a| json!({"attempt_id": a.id, "task_id": a.task, "state": a.state, "open": a.open(),
            "pinned_policy_revision": a.pinned})).collect::<Vec<_>>(),
        "provenance": {"state_db": "read_only", "valuation_revision": valued.as_ref().map(|(h, _)| h.0),
            "valuation_computed_unix_ms": valued.as_ref().map(|(h, _)| h.4), "ledger_synced_unix_ms": valued.as_ref().map(|(h, _)| h.3),
            "sidecar": if sidecar.is_some() { "present" } else { "collection_not_run" },
            "what_if_policy": what_if.as_ref().map(|(w, digest)| json!({"policy_id": w.policy_id, "version": w.version, "digest": digest,
                "synthetic": true, "source": w.source}))}}))
}

fn worst<'a>(a: &'a str, b: &'a str) -> &'a str {
    let rank = |d: &str| match d { "would_block" => 2, "would_warn" => 1, _ => 0 };
    if rank(b) > rank(a) { b } else { a }
}

/// M04 `cost_per_accepted_task` (doc 07, contracts §6 `T`/`A`): the full
/// lifecycle estimate of every attempt of the terminal tasks (failed and
/// cancelled attempts and child sessions included) / count(A). A value only
/// when every such attempt is completely priced in one currency; otherwise
/// `unavailable` with the priced subtotal labeled partial, never 0.
pub fn metrics(project: &Path, since: Option<i64>) -> Result<BTreeMap<String, Value>> {
    let body = |extra: Value| BTreeMap::from([("M04".to_owned(), with(json!({"definition": "M04.cost-v1", "name": "cost_per_accepted_task",
        "basis": cost::BASIS, "rate_cards": "fixture_only", "never_added_to": "M11",
        "caveat": "published-rate estimates from fixture-only rate cards: only as real as the cards imported"}), extra))]);
    let state = project.join(".state/state.db");
    if !state.exists() { return Ok(body(json!({"value": super::unavailable("no_state_store")}))); }
    let db = crate::telemetry::read_only(&state)?;
    let (attempts, _) = canonical(&db)?;
    let tasks = crate::telemetry::metrics::task_evidence(&db)?;
    let in_window = |a: &Attempt| since.is_none_or(|since| a.decided.is_some_and(|at| at >= since));
    let (mut terminal, mut accepted, mut open) = (BTreeSet::new(), 0usize, 0usize);
    for (task, state, evidence) in &tasks {
        if since.is_some() && !attempts.iter().any(|a| &a.task == task && in_window(a)) { continue; }
        if *evidence || ["succeeded", "failed", "cancelled"].contains(&state.as_str()) {
            terminal.insert(task.as_str());
            accepted += usize::from(*evidence);
        } else { open += 1; }
    }
    let Some(sidecar) = crate::telemetry::sidecar::read(project)? else { return Ok(body(json!({"value": super::unavailable("collection_not_run")}))); };
    let Some(((revision, basis, ..), stored)) = charges::valuations(&sidecar, None)? else { return Ok(body(json!({"value": super::unavailable("not_priced")}))); };
    let cohort: Vec<&Attempt> = attempts.iter().filter(|a| terminal.contains(a.task.as_str())).collect();
    let mut values = Vec::new();
    let mut unobserved = BTreeMap::<&str, usize>::new();
    for a in &cohort {
        let rows: Vec<&Stored> = stored.values().filter(|s| s.attempt_id.as_deref() == Some(a.id.as_str())).collect();
        if rows.is_empty() && !a.never_running {
            *unobserved.entry(if a.kind.as_deref().is_some_and(|k| k != "codex") { "adapter_absent" } else { "no_usage_observed" }).or_default() += 1;
        }
        for s in rows { values.push(json!({"valuation": cost::valuation(s, &basis)?})); }
    }
    let (estimate, coverage) = cost::summarize(&values.iter().collect::<Vec<_>>())?;
    let coverage = with(coverage, json!({"attempts": cohort.len(), "attempts_without_usage": unobserved}));
    let common = json!({"revision": revision, "numerator": estimate, "denominator": accepted, "coverage": coverage,
        "tasks": {"terminal": terminal.len(), "accepted": accepted, "open_excluded": open}});
    let value = if accepted == 0 {
        json!({"value": null, "reason": "empty_denominator"})
    } else if !unobserved.is_empty() || estimate["status"] != "complete" {
        json!({"value": super::unavailable("lifecycle_cost_incomplete")})
    } else {
        json!({"value": format!("{}/{accepted}", estimate["amount"].as_str().unwrap_or_default()), "currency": estimate["currency"]})
    };
    Ok(body(with(common, value)))
}
