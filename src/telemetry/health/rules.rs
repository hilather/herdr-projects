//! The declared health-rule table `health-rules.v3`
//! (docs/telemetry/contracts-health.md §2). Each rule reads only through the
//! TM4.1 query service (`analytics::query`) or a lane's own read path
//! (`accounting quota|attention|entries|budget-shadow`, TM4.4 `compare`), and
//! answers `ok`, `warn`, `critical` or `unknown`. A missing or failed source is
//! `unknown` with its reason: never `ok` and never a zero value. Labels are
//! bounded (project, family, rule, service kind, role): no task, attempt,
//! session, account or configuration identity is ever a label. Everything
//! here is read-only; `store::evaluate` persists the outcome.
use super::recommend;
use crate::telemetry::analytics::{compare, query, registry};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub const VERSION: &str = "health-rules.v3";
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Most roles (task classes) the recommendation rule labels in one pass.
pub const MAX_ROLES: usize = 16;
/// Fewest samples per window before a shift rule compares two windows.
pub const MIN_SHIFT_SAMPLES: i64 = 5;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum State { Ok, Warn, Critical, Unknown }

impl State {
    pub fn as_str(self) -> &'static str {
        match self { State::Ok => "ok", State::Warn => "warn", State::Critical => "critical", State::Unknown => "unknown" }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text { "ok" => Some(State::Ok), "warn" => Some(State::Warn), "critical" => Some(State::Critical), "unknown" => Some(State::Unknown), _ => None }
    }
}

/// How a rule reads its source.
#[derive(Clone, Copy, Debug)]
pub enum Eval {
    /// Passive verification flips with canonical run evidence.
    VerificationFlaky,
    /// Query-service source watermark: time since the last collect.
    Collector,
    /// A ratio metric that should be complete (`n/d` below thresholds).
    Coverage(&'static str),
    /// Lane B ledger dispositions: `unresolved` warns, `conflict` is critical.
    Conflicts,
    /// Bound usage records observed after an attempt termination receipt.
    AfterTermination,
    /// Lane B shadow budget bridge over the canonical policy.
    Exposure,
    /// A count (the metric's numerator) in the look-back window.
    Count(&'static str),
    /// A metric's current window against the window before it.
    Shift(&'static str),
    /// Throttled-time share (M38).
    Throttled,
    /// Lane B quota windows: the lowest remaining headroom of a current window.
    Quota,
    /// Lane B attention intervals: the longest open waiting-on-you interval.
    Waiting,
    /// TM4.4 comparison rankings: a recommendation whose configuration changed.
    Recommendation,
}

/// Threshold direction: a value at or above (`Above`) or below (`Below`) a
/// threshold enters that state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction { Above, Below }

pub struct Rule {
    pub name: &'static str,
    pub family: &'static str,
    pub service: Option<&'static str>,
    /// The read path (query-service metric or lane command).
    pub source: &'static str,
    pub eval: Eval,
    pub direction: Direction,
    /// Thresholds in `unit`.
    pub warn: i64,
    pub critical: i64,
    pub unit: &'static str,
    /// Look-back or comparison window, ms.
    pub window_ms: Option<i64>,
    /// No new alert for the same labels within this long after the last one resolved.
    pub cooldown_ms: i64,
    pub detail: &'static str,
}

pub const RULES: &[Rule] = &[
    Rule { name: "verification_flaky", family: "proxy", service: None, source: "lane:quality flaky", eval: Eval::VerificationFlaky,
        direction: Direction::Above, warn: 1, critical: i64::MAX, unit: "flipped_pairs", window_ms: Some(30 * DAY), cooldown_ms: HOUR,
        detail: "any accepted/checks_failed verdict flip for the same tree and policy in the last 30 days warns" },
    Rule { name: "usage_after_termination", family: "consumption", service: Some("codex"), source: "lane:usage after_termination", eval: Eval::AfterTermination,
        direction: Direction::Above, warn: 1, critical: i64::MAX, unit: "records", window_ms: None, cooldown_ms: HOUR,
        detail: "bound usage after termination warns; records remain counted in M08" },
    Rule { name: "collector_stale", family: "collection", service: Some("codex"), source: "query:source_watermarks.sidecar.last_collect_unix_ms",
        eval: Eval::Collector, direction: Direction::Above, warn: 15 * MINUTE, critical: HOUR, unit: "ms", window_ms: None, cooldown_ms: HOUR,
        detail: "time since the last Codex collect; no sidecar or no collect recorded is unknown" },
    Rule { name: "usage_coverage", family: "consumption", service: Some("codex"), source: "query:M13", eval: Eval::Coverage("M13"),
        direction: Direction::Below, warn: 1000, critical: 500, unit: "permille", window_ms: None, cooldown_ms: HOUR,
        detail: "terminated Codex attempts with complete bound usage; below 100% warns, below 50% is critical" },
    Rule { name: "cost_coverage", family: "cost", service: Some("codex"), source: "query:M14", eval: Eval::Coverage("M14"),
        direction: Direction::Below, warn: 1000, critical: 500, unit: "permille", window_ms: None, cooldown_ms: HOUR,
        detail: "valued usage entries that are priced; below 100% warns, below 50% is critical" },
    Rule { name: "accounting_conflict", family: "consumption", service: Some("codex"), source: "lane:accounting entries", eval: Eval::Conflicts,
        direction: Direction::Above, warn: 1, critical: 1, unit: "dispositions", window_ms: None, cooldown_ms: HOUR,
        detail: "usage dispositions left open: `unresolved` warns, `conflict` (payload mismatch, quarantine) is critical" },
    Rule { name: "budget_exposure", family: "cost", service: None, source: "lane:accounting budget-shadow", eval: Eval::Exposure,
        direction: Direction::Above, warn: 1, critical: 2, unit: "decision", window_ms: None, cooldown_ms: HOUR,
        detail: "shadow budget bridge under the canonical policy: would_warn warns, would_block is critical; enforcement stays canonical" },
    Rule { name: "fix_reopened", family: "review_quality", service: None, source: "query:M27", eval: Eval::Count("M27"),
        direction: Direction::Above, warn: 1, critical: 3, unit: "reopened_integrations", window_ms: Some(30 * DAY), cooldown_ms: 6 * HOUR,
        detail: "integrated fixes reopened within their horizon, integrations in the last 30 days" },
    Rule { name: "integration_reverted", family: "proxy", service: None, source: "query:M48", eval: Eval::Count("M48"),
        direction: Direction::Above, warn: 1, critical: 3, unit: "reverted_integrations", window_ms: Some(30 * DAY), cooldown_ms: 6 * HOUR,
        detail: "integrations reverted within the proxy horizon (a proxy, never a validated regression), last 30 days" },
    Rule { name: "latency_shift", family: "lifecycle", service: None, source: "query:M06", eval: Eval::Shift("M06"),
        direction: Direction::Above, warn: 2000, critical: 4000, unit: "permille_of_prior", window_ms: Some(7 * DAY), cooldown_ms: 6 * HOUR,
        detail: "lead-time p95 of the last 7 days against the 7 days before; at least 5 samples in each" },
    Rule { name: "attempt_cost_shift", family: "lifecycle", service: None, source: "query:M07", eval: Eval::Shift("M07"),
        direction: Direction::Above, warn: 1500, critical: 2000, unit: "permille_of_prior", window_ms: Some(7 * DAY), cooldown_ms: 6 * HOUR,
        detail: "attempts per accepted task of the last 7 days against the 7 days before; at least 5 accepted tasks in each" },
    Rule { name: "service_throttled", family: "services", service: Some("codex"), source: "query:M38", eval: Eval::Throttled,
        direction: Direction::Above, warn: 1, critical: 100, unit: "permille", window_ms: None, cooldown_ms: HOUR,
        detail: "throttled-time share; unknown while throttling is not certified for the service" },
    Rule { name: "quota_headroom", family: "services", service: Some("codex"), source: "lane:accounting quota", eval: Eval::Quota,
        direction: Direction::Below, warn: 20_000, critical: 5_000, unit: "millipercent_remaining", window_ms: None, cooldown_ms: HOUR,
        detail: "lowest remaining percent of a current (not yet reset) trusted quota window; 0 is an exhausted window" },
    Rule { name: "waiting_on_you", family: "attention", service: None, source: "lane:accounting attention", eval: Eval::Waiting,
        direction: Direction::Above, warn: 5 * MINUTE, critical: 30 * MINUTE, unit: "ms", window_ms: None, cooldown_ms: 15 * MINUTE,
        detail: "longest open waiting-on-you interval of an open attempt, first blocked sample to its latest sample" },
    Rule { name: "recommendation_stale", family: "recommendation", service: None, source: "compare:M02 + M50", eval: Eval::Recommendation,
        direction: Direction::Below, warn: 500, critical: 0, unit: "permille_freshness", window_ms: None, cooldown_ms: 6 * HOUR,
        detail: "a role's advisory recommendation whose configuration's lineage now dispatches another configuration (M50 below 1/2)" },
];

pub fn find(name: &str) -> Option<&'static Rule> { RULES.iter().find(|r| r.name == name) }

/// One rule outcome (one per role for the recommendation rule).
pub struct Outcome {
    pub rule: &'static Rule,
    pub role: Option<String>,
    pub state: State,
    pub reasons: Vec<Value>,
    pub metric: Value,
    pub window: Value,
    pub evidence: Value,
}

impl Outcome {
    /// Bounded labels: project, family, rule, service kind and (recommendations) role.
    pub fn labels(&self, project: &str) -> Value {
        let mut labels = json!({"project": project, "family": self.rule.family, "rule": self.rule.name});
        if let Some(service) = self.rule.service { labels["service"] = json!(service); }
        if let Some(role) = &self.role { labels["role"] = json!(role); }
        labels
    }
    /// The dedup key: the labels' canonical JSON (sorted keys).
    pub fn key(&self, project: &str) -> String { serde_json::to_string(&self.labels(project)).unwrap_or_default() }
    pub fn json(&self, project: &str) -> Value {
        json!({"rule": self.rule.name, "labels": self.labels(project), "state": self.state.as_str(), "reasons": self.reasons, "metric": self.metric,
            "evidence_window": self.window, "evidence": self.evidence, "thresholds": thresholds(self.rule), "rules_version": VERSION})
    }
}

pub fn thresholds(rule: &Rule) -> Value {
    json!({"direction": match rule.direction { Direction::Above => "at_or_above", Direction::Below => "below" }, "warn": rule.warn, "critical": rule.critical,
        "unit": rule.unit, "window_ms": rule.window_ms, "cooldown_ms": rule.cooldown_ms})
}

pub fn table() -> Value {
    json!({"rules_version": VERSION, "labels": ["project", "family", "rule", "service", "role"], "rules": RULES.iter().map(|r| json!({"rule": r.name, "family": r.family,
        "service": r.service, "source": r.source, "thresholds": thresholds(r), "detail": r.detail})).collect::<Vec<_>>()})
}

fn code(code: &str) -> Value { json!({"code": code}) }

/// Query-service results by window, fetched once per pass.
pub struct Ctx<'a> {
    pub project: &'a Path,
    pub now: i64,
    results: BTreeMap<(Option<i64>, Option<i64>), BTreeMap<String, Value>>,
    /// One set of sources for every window of the pass: canonical rows load once.
    sources: Option<query::Sources<'a>>,
}

impl<'a> Ctx<'a> {
    pub fn new(project: &'a Path, now: i64) -> Self { Ctx { project, now, results: BTreeMap::new(), sources: None } }

    /// One query-service result (`analytics-query.v1`) for `metric` over `[from, to)`,
    /// every metric of that window requested in one read.
    fn result(&mut self, metric: &str, from: Option<i64>, to: Option<i64>) -> Result<Value> {
        if !self.results.contains_key(&(from, to)) {
            let metrics: Vec<String> = RULES.iter().filter_map(|r| match r.eval {
                Eval::Coverage(m) | Eval::Count(m) | Eval::Shift(m) => Some(m),
                Eval::Collector => Some("M08"),
                Eval::Throttled => Some("M38"),
                _ => None,
            }).filter(|m| window_of(m, self.now).contains(&(from, to))).map(str::to_owned).collect();
            let args = query::Args { metrics, cohort: None, from, to, as_of: None, as_of_seq: None, by: None, horizon_ms: None, drill: None,
                page_size: query::DEFAULT_PAGE, cursor: None, json: true };
            if self.sources.is_none() { self.sources = Some(query::Sources::new(self.project)?); }
            let out = query::run_lean(self.sources.as_mut().expect("sources"), &query::request(&args)?)?;
            let map = out["results"].as_array().into_iter().flatten().map(|r| (r["metric_id"].as_str().unwrap_or_default().to_owned(), r.clone())).collect();
            self.results.insert((from, to), map);
        }
        Ok(self.results[&(from, to)].get(metric).cloned().unwrap_or(Value::Null))
    }
}

/// The query windows a metric is read over by the rule table.
fn window_of(metric: &str, now: i64) -> Vec<(Option<i64>, Option<i64>)> {
    RULES.iter().filter_map(|r| match r.eval {
        Eval::Collector if metric == "M08" => Some(vec![(None, None)]),
        Eval::Throttled if metric == "M38" => Some(vec![(None, None)]),
        Eval::Coverage(m) if m == metric => Some(vec![(None, None)]),
        Eval::Count(m) if m == metric => Some(vec![(r.window_ms.map(|w| now - w), None)]),
        Eval::Shift(m) if m == metric => { let w = r.window_ms.unwrap_or(7 * DAY); Some(vec![(Some(now - w), Some(now)), (Some(now - 2 * w), Some(now - w))]) }
        _ => None,
    }).flatten().collect()
}

fn registry_metric(result: &Value) -> Value {
    json!({"metric_id": result["metric_id"], "definition": result["definition"], "registry": result["registry"],
        "certification": result["certification"]["status"], "read": query::CONTRACT})
}

fn window_json(result: &Value) -> Value {
    let mut w = result["window"].clone();
    if w.is_null() { w = json!({"from_unix_ms": null, "to_unix_ms": null}); }
    w["cohort"] = result["cohort"].clone();
    w["time_basis"] = result["time_basis"].clone();
    w
}

/// `"n/d"` → `(n, d)`.
pub fn ratio(value: &Value) -> Option<(i128, i128)> {
    let (n, d) = value.as_str()?.split_once('/')?;
    Some((n.trim().parse().ok()?, d.trim().parse().ok()?))
}

/// An exact non-negative decimal string in thousandths (`"12.5"` → 12500).
fn milli(text: &str) -> Option<i64> {
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > 3 { return None; }
    let frac = format!("{frac:0<3}");
    int.parse::<i64>().ok()?.checked_mul(1000)?.checked_add(frac.parse::<i64>().ok()?)
}

/// The rule's state: `above(t)` / `below(t)` say whether the value is at or above / below threshold `t` (in the rule's unit).
fn grade(rule: &Rule, above: impl Fn(i64) -> bool, below: impl Fn(i64) -> bool) -> State {
    match rule.direction {
        Direction::Above if above(rule.critical) => State::Critical,
        Direction::Above if above(rule.warn) => State::Warn,
        Direction::Below if below(rule.critical) => State::Critical,
        Direction::Below if below(rule.warn) => State::Warn,
        _ => State::Ok,
    }
}

fn outcome(rule: &'static Rule, state: State, reasons: Vec<Value>, metric: Value, window: Value, evidence: Value) -> Outcome {
    Outcome { rule, role: None, state, reasons, metric, window, evidence }
}

fn unknown(rule: &'static Rule, reason: &str, metric: Value, window: Value) -> Outcome {
    outcome(rule, State::Unknown, vec![code(reason)], metric, window, json!({"value": {"status": "unavailable", "reason": reason}}))
}

fn lane_metric(source: &str) -> Value { json!({"read": source, "registry": registry::VERSION}) }

fn unbounded(now: i64) -> Value { json!({"from_unix_ms": null, "to_unix_ms": null, "observed_unix_ms": now}) }

/// Evaluate every rule, read-only. A rule whose source fails is `unknown`
/// (`source_error`), never `ok`.
pub fn evaluate(project: &Path, now: i64) -> Vec<Outcome> {
    let mut ctx = Ctx::new(project, now);
    let mut out = Vec::new();
    for rule in RULES {
        match evaluate_rule(&mut ctx, rule) {
            Ok(outcomes) => out.extend(outcomes),
            Err(_) => out.push(unknown(rule, "source_error", lane_metric(rule.source), unbounded(now))),
        }
    }
    out
}

fn evaluate_rule(ctx: &mut Ctx, rule: &'static Rule) -> Result<Vec<Outcome>> {
    let now = ctx.now;
    Ok(vec![match rule.eval {
        Eval::VerificationFlaky => {
            let from = rule.window_ms.map(|w| now - w);
            let evidence = crate::telemetry::quality::flakes::report(ctx.project, from, Some(now))?;
            let metric = lane_metric(rule.source);
            let window = json!({"from_unix_ms": from, "to_unix_ms": now, "semantics": "half_open", "time_basis": "verification_completed"});
            if evidence["status"] == "unavailable" { return Ok(vec![unknown(rule, evidence["reason"].as_str().unwrap_or("unavailable"), metric, window)]); }
            let n = evidence["numerator"].as_i64().unwrap_or(0);
            outcome(rule, if n > 0 { State::Warn } else { State::Ok }, vec![code(if n > 0 { "verification_verdict_flip" } else { "none_observed" })], metric, window, evidence)
        }
        Eval::Collector => {
            let r = ctx.result("M08", None, None)?;
            let metric = json!({"read": query::CONTRACT, "field": "source_watermarks.sidecar.last_collect_unix_ms", "registry": registry::VERSION});
            let sidecar = &r["source_watermarks"]["sidecar"];
            if sidecar.is_null() { return Ok(vec![unknown(rule, "collection_not_run", metric, unbounded(now))]); }
            let Some(at) = sidecar["last_collect_unix_ms"].as_i64() else { return Ok(vec![unknown(rule, "no_collect_recorded", metric, unbounded(now))]) };
            let age = now - at;
            let state = grade(rule, |t| age >= t, |_| false);
            outcome(rule, state, vec![json!({"code": if state == State::Ok { "collector_current" } else { "collector_stale" }, "age_ms": age})], metric,
                json!({"from_unix_ms": at, "to_unix_ms": now, "semantics": "observation_lag"}), json!({"last_collect_unix_ms": at, "age_ms": age}))
        }
        Eval::Coverage(id) => {
            let r = ctx.result(id, None, None)?;
            let (metric, window) = (registry_metric(&r), window_json(&r));
            if r["status"] == "unavailable" { return Ok(vec![unknown(rule, r["reason"].as_str().unwrap_or("unavailable"), metric, window)]); }
            let evidence = json!({"value": r["value"], "numerator": r["numerator"], "denominator": r["denominator"], "incomplete": r["detail"]["incomplete"]});
            if r["value"].is_null() {
                return Ok(vec![outcome(rule, State::Ok, vec![json!({"code": "empty_population", "reason": r["reason"]})], metric, window, evidence)]);
            }
            let Some((n, d)) = ratio(&r["value"]) else { return Ok(vec![unknown(rule, "value_not_ratio", metric, window)]) };
            let state = grade(rule, |_| false, |t| n * 1000 < i128::from(t) * d);
            outcome(rule, state, vec![json!({"code": if state == State::Ok { "coverage_complete" } else { "coverage_loss" }, "value": r["value"]})], metric, window, evidence)
        }
        Eval::AfterTermination => {
            let metric = lane_metric(rule.source);
            let v = crate::telemetry::sidecar::after_termination_summary(ctx.project)?;
            let records = v["records"].as_i64().unwrap_or(0);
            if records == 0 && let Some(reason) = v["missing_reason"].as_str() {
                return Ok(vec![unknown(rule, reason, metric, unbounded(now))]);
            }
            outcome(rule, if records > 0 { State::Warn } else { State::Ok }, vec![code(if records > 0 { "usage_after_termination" } else { "none_observed" })],
                metric, unbounded(now), json!({"records": records, "attempts": v["attempts"], "accounting": "still counted in M08", "partial_observation": !v["missing_reason"].is_null()}))
        }
        Eval::Conflicts => {
            let metric = lane_metric(rule.source);
            let Some(db) = crate::telemetry::sidecar::read(ctx.project)? else { return Ok(vec![unknown(rule, "collection_not_run", metric, unbounded(now))]) };
            // Counted in SQL: the whole ledger as JSON took gigabytes at a million events.
            let Some(open) = crate::telemetry::accounting::ledger::open_dispositions(&db)? else {
                return Ok(vec![unknown(rule, "ledger_not_synced", metric, unbounded(now))]);
            };
            let mut counts = BTreeMap::<String, BTreeMap<String, i64>>::new();
            for (disposition, reason, n) in open {
                *counts.entry(disposition).or_default().entry(reason.unwrap_or_else(|| "unspecified".to_owned())).or_default() += n;
            }
            let total = |d: &str| counts.get(d).map_or(0, |m| m.values().sum::<i64>());
            let (conflicts, unresolved) = (total("conflict"), total("unresolved"));
            let state = if conflicts >= rule.critical { State::Critical } else if unresolved >= rule.warn { State::Warn } else { State::Ok };
            let reasons = if state == State::Ok { vec![code("no_open_dispositions")] } else {
                counts.keys().map(|d| json!({"code": format!("{d}_dispositions"), "count": total(d)})).collect()
            };
            outcome(rule, state, reasons, metric, unbounded(now), json!({"conflict": conflicts, "unresolved": unresolved, "by_reason": counts}))
        }
        Eval::Exposure => {
            let metric = lane_metric(rule.source);
            let v = crate::telemetry::accounting::budget::shadow(ctx.project, None, None)?;
            if v["status"] == "unavailable" { return Ok(vec![unknown(rule, v["reason"].as_str().unwrap_or("unavailable"), metric, unbounded(now))]); }
            if v["canonical"]["policy"].is_null() || v["canonical"]["policy"]["status"] == "unavailable" {
                let reason = v["canonical"]["reason"].as_str().or(v["canonical"]["policy"]["reason"].as_str()).unwrap_or("no_budget_policy");
                return Ok(vec![unknown(rule, reason, metric, unbounded(now))]);
            }
            const KEEP: [&str; 15] = ["dimension", "policy_source", "policy_revision", "scope", "unit", "currency", "limit", "admitted", "accepted",
                "remaining_reserved", "exposure", "projected", "unknown_usage", "decision", "reason"];
            let evaluations: Vec<Value> = v["evaluations"].as_array().into_iter().flatten()
                .map(|e| Value::Object(KEEP.iter().filter_map(|k| e.get(*k).map(|x| ((*k).to_owned(), x.clone()))).collect())).collect();
            let state = if v["decision"]["would_block"] == true { State::Critical } else if v["decision"]["would_warn"] == true { State::Warn } else { State::Ok };
            let reasons = if state == State::Ok { vec![code("within_limit")] } else {
                v["decision"]["reasons"].as_array().into_iter().flatten().map(|r| json!({"code": r})).collect()
            };
            outcome(rule, state, reasons, metric, unbounded(now), json!({"mode": "shadow", "enforcement": "canonical budget path only", "evaluations": evaluations}))
        }
        Eval::Count(id) => {
            let from = rule.window_ms.map(|w| now - w);
            let r = ctx.result(id, from, None)?;
            let (metric, window) = (registry_metric(&r), window_json(&r));
            if r["status"] == "unavailable" { return Ok(vec![unknown(rule, r["reason"].as_str().unwrap_or("unavailable"), metric, window)]); }
            let Some(n) = r["numerator"].as_i64() else { return Ok(vec![unknown(rule, "numerator_unavailable", metric, window)]) };
            let state = grade(rule, |t| n >= t, |_| false);
            outcome(rule, state, vec![json!({"code": if n > 0 { format!("{}_observed", rule.unit) } else { "none_observed".to_owned() }, "count": n})], metric, window,
                json!({"numerator": n, "denominator": r["denominator"], "value": r["value"], "censored": r["detail"]["censored"]}))
        }
        Eval::Shift(id) => {
            let w = rule.window_ms.unwrap_or(7 * DAY);
            let current = ctx.result(id, Some(now - w), Some(now))?;
            let prior = ctx.result(id, Some(now - 2 * w), Some(now - w))?;
            let metric = registry_metric(&current);
            let window = json!({"current": window_json(&current), "prior": window_json(&prior)});
            let sample = |r: &Value| if id == "M06" { r["numerator"].as_i64() } else { r["denominator"].as_i64() };
            let part = |r: &Value| -> Option<(i128, i128)> {
                if id == "M06" { r["value"].as_i64().map(|v| (i128::from(v), 1)) } else { ratio(&r["value"]) }
            };
            for r in [&current, &prior] {
                if r["status"] == "unavailable" { return Ok(vec![unknown(rule, r["reason"].as_str().unwrap_or("unavailable"), metric, window)]); }
            }
            let evidence = json!({"current": {"value": current["value"], "samples": sample(&current)}, "prior": {"value": prior["value"], "samples": sample(&prior)},
                "min_samples": MIN_SHIFT_SAMPLES});
            if [&current, &prior].iter().any(|r| sample(r).is_none_or(|n| n < MIN_SHIFT_SAMPLES)) {
                return Ok(vec![outcome(rule, State::Unknown, vec![code("insufficient_data")], metric, window, evidence)]);
            }
            let (Some((a, b)), Some((c, d))) = (part(&current), part(&prior)) else { return Ok(vec![outcome(rule, State::Unknown, vec![code("value_unavailable")], metric, window, evidence)]) };
            // current / prior = (a/b) / (c/d) = a·d / (b·c); compared in per-mille of the prior.
            let (num, den) = (a * d, b * c);
            if den == 0 { return Ok(vec![outcome(rule, State::Unknown, vec![code("prior_zero")], metric, window, evidence)]); }
            let state = grade(rule, |t| num * 1000 >= i128::from(t) * den, |_| false);
            outcome(rule, state, vec![json!({"code": if state == State::Ok { "no_shift" } else { "shift_up" }, "ratio_to_prior": format!("{num}/{den}")})], metric, window, evidence)
        }
        Eval::Throttled => {
            let r = ctx.result("M38", None, None)?;
            let (metric, window) = (registry_metric(&r), window_json(&r));
            if r["status"] == "unavailable" { return Ok(vec![unknown(rule, r["reason"].as_str().unwrap_or("unavailable"), metric, window)]); }
            let Some((n, d)) = ratio(&r["value"]) else { return Ok(vec![unknown(rule, "value_not_ratio", metric, window)]) };
            let state = grade(rule, |t| n * 1000 >= i128::from(t) * d, |_| false);
            outcome(rule, state, vec![json!({"code": if state == State::Ok { "not_throttled" } else { "throttled" }, "value": r["value"]})], metric, window,
                json!({"value": r["value"], "numerator": r["numerator"], "denominator": r["denominator"]}))
        }
        Eval::Quota => {
            let metric = lane_metric(rule.source);
            let Some(db) = crate::telemetry::sidecar::read(ctx.project)? else { return Ok(vec![unknown(rule, "collection_not_run", metric, unbounded(now))]) };
            if !crate::telemetry::accounting::quota::synced(&db)? {
                return Ok(vec![unknown(rule, "ledger_not_synced", metric, unbounded(now))]);
            }
            let (windows, latest_reset): (i64, Option<i64>) = db.query_row("SELECT count(*),max(resets_unix_ms) FROM quota_windows", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if windows == 0 { return Ok(vec![unknown(rule, "no_quota_observation", metric, unbounded(now))]); }
            // Only the current windows, streamed in the quota read's original
            // order. No dispatch-history M40 JSON or historical windows.
            // Keep milli's exact validation; SQLite REAL casts would accept
            // extra precision and malformed decimals that the rule excludes.
            let mut stmt = db.prepare("SELECT remaining,used,unit,window_kind,window_minutes,window_start_unix_ms,resets_unix_ms,last_observed_unix_ms
                FROM quota_windows WHERE resets_unix_ms>?1 ORDER BY account,limit_id,window_kind,resets_unix_ms")?;
            let mut rows = stmt.query([now])?;
            let (mut current, mut below_warn, mut lowest) = (0usize, 0usize, None::<(Value, i64, i64)>);
            while let Some(row) = rows.next()? {
                let remaining_text: String = row.get(0)?;
                let Some(remaining) = milli(&remaining_text) else { continue };
                let resets: i64 = row.get(6)?;
                current += 1;
                below_warn += usize::from(remaining < rule.warn);
                if lowest.as_ref().is_none_or(|(_, m, r)| (remaining, resets) < (*m, *r)) {
                    lowest = Some((json!({"remaining": remaining_text, "used": row.get::<_, String>(1)?, "unit": row.get::<_, String>(2)?,
                        "window_kind": row.get::<_, String>(3)?, "window_minutes": row.get::<_, i64>(4)?, "window_start_unix_ms": row.get::<_, i64>(5)?,
                        "resets_unix_ms": resets, "last_observed_unix_ms": row.get::<_, i64>(7)?}), remaining, resets));
                }
            }
            let Some((lowest, remaining, _)) = lowest else {
                return Ok(vec![outcome(rule, State::Unknown, vec![code("no_current_window")], metric, unbounded(now),
                    json!({"windows": windows, "latest_reset_unix_ms": latest_reset, "detail": "every observed window has reset: the new window's headroom is unknown"}))]);
            };
            let state = grade(rule, |_| false, |t| remaining < t);
            let reason = if remaining == 0 { "window_exhausted" } else if state == State::Ok { "headroom_ok" } else { "headroom_low" };
            let last = lowest["last_observed_unix_ms"].as_i64().unwrap_or(now);
            outcome(rule, state, vec![json!({"code": reason, "remaining": lowest["remaining"]})], metric,
                json!({"from_unix_ms": lowest["window_start_unix_ms"], "to_unix_ms": lowest["resets_unix_ms"], "semantics": "quota_window", "observed_unix_ms": last}),
                json!({"current_windows": current, "below_warn": below_warn, "account_basis": crate::telemetry::accounting::quota::ACCOUNT_BASIS,
                    "lowest": {"remaining": lowest["remaining"], "used": lowest["used"], "unit": lowest["unit"], "window_kind": lowest["window_kind"],
                        "window_minutes": lowest["window_minutes"], "resets_unix_ms": lowest["resets_unix_ms"], "last_observed_unix_ms": last, "age_ms": now - last},
                    "semantics": "not_certified"}))
        }
        Eval::Waiting => {
            let metric = lane_metric(rule.source);
            let Some(db) = crate::telemetry::sidecar::read(ctx.project)? else { return Ok(vec![unknown(rule, "collection_not_run", metric, unbounded(now))]) };
            let Some((open, not_observed, waits, longest)) = crate::telemetry::accounting::attention::open_wait_summary(ctx.project, &db)? else {
                return Ok(vec![unknown(rule, "attention_not_collected", metric, unbounded(now))]);
            };
            let evidence = |longest: Option<(i64, i64)>| json!({"open_attempts": open, "waiting_attempts": waits, "not_observed": not_observed,
                "longest_wait_ms": longest.map(|(o, l)| l - o), "scope": "human_routed_waits", "signal": crate::telemetry::accounting::attention::signal()["certified"]});
            if open == 0 { return Ok(vec![outcome(rule, State::Ok, vec![code("no_open_attempts")], metric, unbounded(now), evidence(None))]); }
            if longest.is_none() && not_observed == open {
                return Ok(vec![outcome(rule, State::Unknown, vec![code("not_observed")], metric, unbounded(now), evidence(None))]);
            }
            let ms = longest.map_or(0, |(o, l)| l - o);
            let state = grade(rule, |t| longest.is_some() && ms >= t, |_| false);
            let mut reasons = vec![json!({"code": if longest.is_some() { "waiting_on_you" } else { "no_open_wait" }, "longest_wait_ms": longest.map(|_| ms)})];
            if not_observed > 0 { reasons.push(json!({"code": "partial_observation", "not_observed": not_observed})); }
            let window = match longest { Some((o, l)) => json!({"from_unix_ms": o, "to_unix_ms": l, "semantics": "open_wait_observed"}), None => unbounded(now) };
            outcome(rule, state, reasons, metric, window, evidence(longest))
        }
        Eval::Recommendation => return recommendations(ctx, rule),
    }])
}

/// One outcome per role (task class) with a comparison cell, at most `MAX_ROLES`.
fn recommendations(ctx: &mut Ctx, rule: &'static Rule) -> Result<Vec<Outcome>> {
    let args = compare::Args { metrics: vec!["M02".into()], by: "configuration".into(), cohort: None, from: None, to: None, horizon_ms: None, task_class: None,
        seed: None, json: true };
    let report = compare::run(ctx.project, &args)?;
    let log = recommend::DispatchLog::load(ctx.project)?;
    let roles: Vec<String> = report["results"][0]["cells"].as_array().into_iter().flatten().filter_map(|c| c["task_class"].as_str().map(str::to_owned)).take(MAX_ROLES).collect();
    let metric = json!({"metric_id": "M50", "definition": registry::FRESHNESS.definition, "compared": "M02.cohort-v1", "comparison": registry::COMPARISON.version,
        "registry": registry::VERSION});
    let mut out = Vec::new();
    for role in roles {
        let rec = recommend::recommendation(&report, &role, &log);
        let (state, reasons) = match rec["status"].as_str() {
            Some("stale") => (State::Warn, rec["reasons"].as_array().cloned().unwrap_or_default()),
            Some("recommended") => (State::Ok, vec![json!({"code": "recommendation_fresh"})]),
            _ => (State::Ok, vec![json!({"code": "no_recommendation"})]),
        };
        let evidence = json!({"status": rec["status"], "recommended": rec["recommendation"]["label"], "current": rec["freshness"]["current_label"],
            "freshness": {"value": rec["freshness"]["value"], "state": rec["freshness"]["state"], "stale_below": rec["freshness"]["stale_below"]}});
        out.push(Outcome { rule, role: Some(role), state, reasons, metric: metric.clone(), window: rec["evidence_window"].clone(), evidence });
    }
    if out.is_empty() {
        out.push(outcome(rule, State::Ok, vec![code("no_comparison_cells")], metric, unbounded(ctx.now), json!({"roles": 0})));
    }
    Ok(out)
}
